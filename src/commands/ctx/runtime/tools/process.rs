use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{ToolError, ToolErrorCode};
use crate::commands::ctx::config::OutputFilterRule;
use crate::commands::ctx::output::{self, CapturedOutput, CompactionScope, StreamingCapture};
use crate::commands::ctx::permit::HeavyPermit;
use crate::commands::ctx::runtime::enforcement::{ProcessEffects, SandboxLaunch};
use crate::commands::ctx::state::StateDir;
use crate::commands::ctx::supervise::{self, ChildGuard};

const MAX_WAIT_MS: u64 = 60_000;
const TERMINATE_GRACE: Duration = Duration::from_secs(2);
const MAX_COMPLETED: usize = 128;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProcessStartArgs {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub shell_script: Option<String>,
    pub cwd: PathBuf,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub network: bool,
    #[serde(default)]
    pub outside_write: bool,
    #[serde(default)]
    pub git_metadata_write: bool,
    #[serde(default)]
    pub git_push_or_destructive: bool,
    #[serde(default)]
    pub interactive: bool,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    pub idempotency_key: String,
}

impl ProcessStartArgs {
    pub(super) fn effects(&self) -> ProcessEffects {
        let shell = self.shell_script.is_some();
        let (git_write, destructive) = infer_git_effects(&self.program, &self.args);
        ProcessEffects {
            repo_write: !self.read_only,
            outside_write: self.outside_write,
            network: self.network,
            git_metadata_write: shell || self.git_metadata_write || git_write,
            git_push_or_destructive: shell || self.git_push_or_destructive || destructive,
        }
    }

    pub(super) fn display_command(&self) -> Vec<String> {
        if let Some(script) = &self.shell_script {
            let mut command = vec![self.program.clone()];
            command.extend(self.args.clone());
            command.push(script.clone());
            command
        } else {
            std::iter::once(self.program.clone())
                .chain(self.args.clone())
                .collect()
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProcessHandleArgs {
    pub handle: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProcessWaitArgs {
    pub handle: String,
    #[serde(default = "default_wait_ms")]
    pub wait_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProcessWriteArgs {
    pub handle: String,
    pub input: String,
    #[serde(default)]
    pub close: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessState {
    Running,
    Exited,
    TimedOut,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessStream {
    Stdout,
    Stderr,
    Pty,
}

#[derive(Debug, Serialize)]
pub struct ProcessOutputChunk {
    pub stream: ProcessStream,
    pub text: String,
    pub byte_len: usize,
    pub inline_truncated: bool,
}

#[derive(Debug, Serialize)]
pub struct ProcessSnapshot {
    pub handle: String,
    pub state: ProcessState,
    pub exit_code: Option<i32>,
    pub output: Vec<ProcessOutputChunk>,
    pub pending_output_bytes: usize,
    pub output_id: Option<String>,
    pub summary: Option<String>,
    pub interactive: bool,
    pub elapsed_ms: u64,
}

#[derive(Clone, Debug)]
pub(super) struct ProcessLimits {
    pub max_inline_bytes: usize,
    pub max_processes: usize,
    pub max_summary_bytes: usize,
    pub max_heavy_operations: usize,
    pub heavy_patterns: Vec<String>,
    pub output_filter: Vec<OutputFilterRule>,
    pub extra_verbatim: Vec<String>,
    pub compact_search: bool,
}

#[derive(Debug)]
struct StreamChunk {
    stream: ProcessStream,
    bytes: Vec<u8>,
}

type SpawnedProcess = (ProcessChild, Receiver<StreamChunk>, Vec<JoinHandle<()>>);

enum ProcessChild {
    Standard {
        child: Child,
        stdin: Option<ChildStdin>,
        guard: ChildGuard,
    },
    Pty {
        child: Box<dyn portable_pty::Child + Send + Sync>,
        writer: Option<Box<dyn Write + Send>>,
        guard: ChildGuard,
    },
}

impl std::fmt::Debug for ProcessChild {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Standard { child, .. } => f
                .debug_struct("Standard")
                .field("pid", &child.id())
                .finish(),
            Self::Pty { child, .. } => f
                .debug_struct("Pty")
                .field("pid", &child.process_id())
                .finish(),
        }
    }
}

impl ProcessChild {
    fn try_wait(&mut self) -> Result<Option<i32>, ToolError> {
        match self {
            Self::Standard { child, guard, .. } => {
                let status = child.try_wait().map_err(ToolError::io)?;
                if status.is_some() {
                    guard.release();
                }
                Ok(status.map(|status| status.code().unwrap_or(-1)))
            }
            Self::Pty { child, guard, .. } => {
                let status = child.try_wait().map_err(ToolError::external)?;
                if status.is_some() {
                    guard.release();
                }
                Ok(status.map(|status| status.exit_code() as i32))
            }
        }
    }

    fn write_input(&mut self, input: &[u8], close: bool) -> Result<(), ToolError> {
        match self {
            Self::Standard { stdin, .. } => {
                let sink = stdin.as_mut().ok_or_else(|| {
                    ToolError::new(ToolErrorCode::ProcessClosed, "process stdin is closed")
                })?;
                sink.write_all(input).map_err(ToolError::io)?;
                sink.flush().map_err(ToolError::io)?;
                if close {
                    stdin.take();
                }
            }
            Self::Pty { writer, .. } => {
                let sink = writer.as_mut().ok_or_else(|| {
                    ToolError::new(ToolErrorCode::ProcessClosed, "PTY input is closed")
                })?;
                sink.write_all(input).map_err(ToolError::io)?;
                sink.flush().map_err(ToolError::io)?;
                if close {
                    writer.take();
                }
            }
        }
        Ok(())
    }

    fn terminate(&mut self) -> Result<(), ToolError> {
        match self {
            Self::Standard {
                child,
                stdin,
                guard,
            } => {
                stdin.take();
                supervise::terminate(child, TERMINATE_GRACE).map_err(ToolError::external)?;
                guard.release();
            }
            Self::Pty {
                child,
                writer,
                guard,
            } => {
                writer.take();
                #[cfg(not(unix))]
                if let Some(pid) = child.process_id() {
                    supervise::kill_tree(pid);
                }
                let _ = child.kill();
                child.wait().map_err(ToolError::external)?;
                guard.release();
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
struct ManagedProcess {
    handle: String,
    child: ProcessChild,
    receiver: Receiver<StreamChunk>,
    readers: Vec<JoinHandle<()>>,
    capture: Option<StreamingCapture>,
    captured: Option<CapturedOutput>,
    command: Vec<String>,
    scope: CompactionScope,
    started: Instant,
    timeout: Option<Duration>,
    state: ProcessState,
    exit_code: Option<i32>,
    interactive: bool,
    heavy_permit: Option<HeavyPermit>,
}

#[derive(Debug)]
pub(super) struct ProcessManager {
    state: StateDir,
    repo: PathBuf,
    limits: ProcessLimits,
    processes: HashMap<String, ManagedProcess>,
    idempotency: HashMap<String, String>,
}

impl ProcessManager {
    pub(super) fn new(state: StateDir, repo: PathBuf, limits: ProcessLimits) -> Self {
        Self {
            state,
            repo,
            limits,
            processes: HashMap::new(),
            idempotency: HashMap::new(),
        }
    }

    pub(super) fn existing_for_key(&self, key: &str) -> Option<&str> {
        self.idempotency.get(key).map(String::as_str)
    }

    pub(super) fn start(
        &mut self,
        launch: SandboxLaunch,
        args: &ProcessStartArgs,
    ) -> Result<ProcessSnapshot, ToolError> {
        validate_key(&args.idempotency_key)?;
        if let Some(handle) = self
            .existing_for_key(&args.idempotency_key)
            .map(str::to_string)
        {
            return self.poll(&handle);
        }
        let running = self
            .processes
            .values()
            .filter(|process| process.state == ProcessState::Running)
            .count();
        if running >= self.limits.max_processes.max(1) {
            return Err(ToolError::new(
                ToolErrorCode::ResourceBusy,
                "native process limit is exhausted",
            ));
        }
        let command = args.display_command();
        let command_line = command.join(" ");
        let heavy_permit =
            if crate::commands::ctx::permit::is_heavy(&command_line, &self.limits.heavy_patterns) {
                Some(
                    crate::commands::ctx::permit::acquire(
                        &self.state,
                        self.limits.max_heavy_operations.max(1),
                        &format!("native:{}", args.idempotency_key),
                    )
                    .ok_or_else(|| {
                        ToolError::new(
                            ToolErrorCode::ResourceBusy,
                            "heavy-operation permit pool is exhausted",
                        )
                    })?,
                )
            } else {
                None
            };
        let capture =
            StreamingCapture::start(&self.state, &self.repo).map_err(ToolError::external)?;
        let (child, receiver, readers) = match spawn(&launch, args.interactive) {
            Ok(spawned) => spawned,
            Err(error) => {
                capture.abort();
                return Err(error);
            }
        };
        let handle = uuid::Uuid::new_v4().simple().to_string();
        let scope = output::classify_compaction(
            &command_line,
            &self.limits.extra_verbatim,
            self.limits.compact_search,
        );
        self.processes.insert(
            handle.clone(),
            ManagedProcess {
                handle: handle.clone(),
                child,
                receiver,
                readers,
                capture: Some(capture),
                captured: None,
                command,
                scope,
                started: Instant::now(),
                timeout: args.timeout_ms.map(Duration::from_millis),
                state: ProcessState::Running,
                exit_code: None,
                interactive: args.interactive,
                heavy_permit,
            },
        );
        self.idempotency
            .insert(args.idempotency_key.clone(), handle.clone());
        self.prune_completed();
        self.poll(&handle)
    }

    pub(super) fn poll(&mut self, handle: &str) -> Result<ProcessSnapshot, ToolError> {
        let limits = self.limits.clone();
        let process = self
            .processes
            .get_mut(handle)
            .ok_or_else(|| unknown(handle))?;
        let (output, pending) = update_process(process, &limits)?;
        Ok(snapshot(process, output, pending))
    }

    pub(super) fn wait(
        &mut self,
        handle: &str,
        wait_ms: u64,
    ) -> Result<ProcessSnapshot, ToolError> {
        let deadline = Instant::now() + Duration::from_millis(wait_ms.min(MAX_WAIT_MS));
        loop {
            let snapshot = self.poll(handle)?;
            if snapshot.state != ProcessState::Running || Instant::now() >= deadline {
                return Ok(snapshot);
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    pub(super) fn write_input(
        &mut self,
        handle: &str,
        input: &str,
        close: bool,
    ) -> Result<ProcessSnapshot, ToolError> {
        let process = self
            .processes
            .get_mut(handle)
            .ok_or_else(|| unknown(handle))?;
        if process.state != ProcessState::Running {
            return Err(ToolError::new(
                ToolErrorCode::ProcessClosed,
                "cannot write to a completed process",
            ));
        }
        process.child.write_input(input.as_bytes(), close)?;
        self.poll(handle)
    }

    pub(super) fn terminate(&mut self, handle: &str) -> Result<ProcessSnapshot, ToolError> {
        let limits = self.limits.clone();
        let process = self
            .processes
            .get_mut(handle)
            .ok_or_else(|| unknown(handle))?;
        if process.state == ProcessState::Running {
            process.child.terminate()?;
            process.state = ProcessState::Cancelled;
            process.exit_code = None;
        }
        let (output, pending) = finish(process, &limits, limits.max_inline_bytes)?;
        Ok(snapshot(process, output, pending))
    }

    pub(super) fn output_json(snapshot: ProcessSnapshot) -> Result<Value, ToolError> {
        serde_json::to_value(snapshot).map_err(ToolError::external)
    }

    fn terminate_all(&mut self) {
        let handles: Vec<String> = self.processes.keys().cloned().collect();
        for handle in handles {
            let _ = self.terminate(&handle);
        }
    }

    fn prune_completed(&mut self) {
        let completed = self
            .processes
            .values()
            .filter(|process| process.state != ProcessState::Running)
            .count();
        if completed <= MAX_COMPLETED {
            return;
        }
        let mut oldest: Vec<(String, Duration)> = self
            .processes
            .iter()
            .filter(|(_, process)| process.state != ProcessState::Running)
            .map(|(handle, process)| (handle.clone(), process.started.elapsed()))
            .collect();
        oldest.sort_by_key(|(_, age)| std::cmp::Reverse(*age));
        for (handle, _) in oldest.into_iter().take(completed - MAX_COMPLETED) {
            self.processes.remove(&handle);
            self.idempotency.retain(|_, held| held != &handle);
        }
    }
}

impl Drop for ProcessManager {
    fn drop(&mut self) {
        self.terminate_all();
    }
}

fn spawn(launch: &SandboxLaunch, interactive: bool) -> Result<SpawnedProcess, ToolError> {
    if interactive {
        spawn_pty(launch)
    } else {
        spawn_standard(launch)
    }
}

fn spawn_standard(launch: &SandboxLaunch) -> Result<SpawnedProcess, ToolError> {
    let mut command = Command::new(&launch.program);
    command
        .args(&launch.args)
        .current_dir(&launch.cwd)
        .env_clear()
        .envs(&launch.environment)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    supervise::isolate_process_tree(&mut command);
    let mut child = command.spawn().map_err(ToolError::io)?;
    let guard = ChildGuard::adopt(Some(child.id()));
    let stdin = child.stdin.take();
    let (sender, receiver) = mpsc::channel();
    let mut readers = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        readers.push(spawn_reader(stdout, sender.clone(), ProcessStream::Stdout));
    }
    if let Some(stderr) = child.stderr.take() {
        readers.push(spawn_reader(stderr, sender, ProcessStream::Stderr));
    }
    Ok((
        ProcessChild::Standard {
            child,
            stdin,
            guard,
        },
        receiver,
        readers,
    ))
}

fn spawn_pty(launch: &SandboxLaunch) -> Result<SpawnedProcess, ToolError> {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(ToolError::external)?;
    let mut command = CommandBuilder::new(&launch.program);
    for arg in &launch.args {
        command.arg(arg);
    }
    command.cwd(&launch.cwd);
    for (key, _) in std::env::vars_os() {
        command.env_remove(key);
    }
    for (key, value) in &launch.environment {
        command.env(key, value);
    }
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(ToolError::external)?;
    let writer = pair.master.take_writer().map_err(ToolError::external)?;
    let child = pair
        .slave
        .spawn_command(command)
        .map_err(ToolError::external)?;
    let guard = ChildGuard::adopt(child.process_id());
    let (sender, receiver) = mpsc::channel();
    let readers = vec![spawn_reader(reader, sender, ProcessStream::Pty)];
    Ok((
        ProcessChild::Pty {
            child,
            writer: Some(writer),
            guard,
        },
        receiver,
        readers,
    ))
}

fn spawn_reader<R: Read + Send + 'static>(
    mut reader: R,
    sender: Sender<StreamChunk>,
    stream: ProcessStream,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buffer = [0u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => return,
                Ok(read) => {
                    if sender
                        .send(StreamChunk {
                            stream,
                            bytes: buffer[..read].to_vec(),
                        })
                        .is_err()
                    {
                        return;
                    }
                }
            }
        }
    })
}

fn update_process(
    process: &mut ManagedProcess,
    limits: &ProcessLimits,
) -> Result<(Vec<ProcessOutputChunk>, usize), ToolError> {
    if process.state != ProcessState::Running {
        return drain(process, limits.max_inline_bytes);
    }
    if process
        .timeout
        .is_some_and(|timeout| process.started.elapsed() >= timeout)
    {
        process.child.terminate()?;
        process.state = ProcessState::TimedOut;
        process.exit_code = None;
        return finish(process, limits, limits.max_inline_bytes);
    }
    let (mut output, mut pending) = drain(process, limits.max_inline_bytes)?;
    if let Some(code) = process.child.try_wait()? {
        process.state = ProcessState::Exited;
        process.exit_code = Some(code);
        let used: usize = output.iter().map(|chunk| chunk.text.len()).sum();
        let (tail, tail_pending) = finish(
            process,
            limits,
            limits.max_inline_bytes.saturating_sub(used),
        )?;
        output.extend(tail);
        pending = pending.saturating_add(tail_pending);
    }
    Ok((output, pending))
}

fn finish(
    process: &mut ManagedProcess,
    limits: &ProcessLimits,
    inline_budget: usize,
) -> Result<(Vec<ProcessOutputChunk>, usize), ToolError> {
    for reader in process.readers.drain(..) {
        let _ = reader.join();
    }
    let drained = drain(process, inline_budget)?;
    process.heavy_permit.take();
    if let Some(capture) = process.capture.take() {
        process.captured = Some(
            capture
                .finish(
                    &process.command,
                    process.exit_code,
                    limits.max_summary_bytes,
                    process.scope,
                    &limits.output_filter,
                )
                .map_err(ToolError::external)?,
        );
    }
    Ok(drained)
}

fn drain(
    process: &mut ManagedProcess,
    inline_budget: usize,
) -> Result<(Vec<ProcessOutputChunk>, usize), ToolError> {
    let mut output = Vec::new();
    let mut remaining = inline_budget;
    let mut suppressed = 0usize;
    while let Ok(chunk) = process.receiver.try_recv() {
        if let Some(capture) = process.capture.as_mut() {
            capture.append(&chunk.bytes).map_err(ToolError::external)?;
        }
        let take = remaining.min(chunk.bytes.len());
        if take > 0 {
            let bytes = &chunk.bytes[..take];
            output.push(ProcessOutputChunk {
                stream: chunk.stream,
                text: String::from_utf8_lossy(bytes).into_owned(),
                byte_len: chunk.bytes.len(),
                inline_truncated: take < chunk.bytes.len(),
            });
            remaining -= take;
        }
        suppressed = suppressed.saturating_add(chunk.bytes.len().saturating_sub(take));
    }
    Ok((output, suppressed))
}

fn snapshot(
    process: &ManagedProcess,
    output: Vec<ProcessOutputChunk>,
    pending_output_bytes: usize,
) -> ProcessSnapshot {
    ProcessSnapshot {
        handle: process.handle.clone(),
        state: process.state,
        exit_code: process.exit_code,
        output,
        pending_output_bytes,
        output_id: process.captured.as_ref().map(|capture| capture.id.clone()),
        summary: process
            .captured
            .as_ref()
            .and_then(|capture| capture.summary.clone()),
        interactive: process.interactive,
        elapsed_ms: process
            .started
            .elapsed()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64,
    }
}

fn infer_git_effects(program: &str, args: &[String]) -> (bool, bool) {
    let program = Path::new(program)
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or(program)
        .to_ascii_lowercase();
    if program != "git" {
        return (false, false);
    }
    let command = args
        .iter()
        .find(|arg| !arg.starts_with('-'))
        .map(String::as_str)
        .unwrap_or_default();
    let write = matches!(
        command,
        "add"
            | "am"
            | "branch"
            | "checkout"
            | "cherry-pick"
            | "clean"
            | "commit"
            | "merge"
            | "mv"
            | "rebase"
            | "reset"
            | "restore"
            | "revert"
            | "rm"
            | "stash"
            | "switch"
            | "tag"
            | "worktree"
    );
    let destructive = command == "push"
        || command == "clean"
        || (command == "reset" && args.iter().any(|arg| arg == "--hard"))
        || (command == "branch" && args.iter().any(|arg| arg == "-D"));
    (write, destructive)
}

fn validate_key(key: &str) -> Result<(), ToolError> {
    if key.trim().is_empty() || key.len() > 256 || key.contains('\0') {
        Err(ToolError::new(
            ToolErrorCode::InvalidArguments,
            "idempotency_key must contain 1..=256 non-NUL bytes",
        ))
    } else {
        Ok(())
    }
}

fn unknown(handle: &str) -> ToolError {
    ToolError::new(
        ToolErrorCode::UnknownProcess,
        format!("unknown native process handle {handle}"),
    )
}

fn default_wait_ms() -> u64 {
    10_000
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;

    fn limits() -> ProcessLimits {
        ProcessLimits {
            max_inline_bytes: 4096,
            max_processes: 4,
            max_summary_bytes: 4096,
            max_heavy_operations: 1,
            heavy_patterns: Vec::new(),
            output_filter: Vec::new(),
            extra_verbatim: Vec::new(),
            compact_search: true,
        }
    }

    #[cfg(unix)]
    fn shell_launch(dir: &Path, script: &str) -> SandboxLaunch {
        SandboxLaunch {
            program: PathBuf::from("sh"),
            args: vec![OsString::from("-c"), OsString::from(script)],
            cwd: dir.to_path_buf(),
            environment: BTreeMap::from([("PATH".into(), "/usr/bin:/bin".into())]),
        }
    }

    #[cfg(unix)]
    fn args(dir: &Path, key: &str) -> ProcessStartArgs {
        ProcessStartArgs {
            program: "sh".into(),
            args: Vec::new(),
            shell_script: Some("test".into()),
            cwd: dir.to_path_buf(),
            environment: BTreeMap::new(),
            read_only: false,
            network: false,
            outside_write: false,
            git_metadata_write: false,
            git_push_or_destructive: false,
            interactive: false,
            timeout_ms: None,
            idempotency_key: key.into(),
        }
    }

    #[test]
    fn git_effects_are_inferred_instead_of_trusting_provider_flags() {
        assert_eq!(infer_git_effects("git", &["status".into()]), (false, false));
        assert_eq!(
            infer_git_effects("git", &["reset".into(), "--hard".into()]),
            (true, true)
        );
        assert_eq!(
            infer_git_effects("git.exe", &["push".into()]),
            (false, true)
        );
    }

    #[cfg(unix)]
    #[test]
    fn process_output_keeps_non_utf8_bytes_and_returns_a_retrieval_handle() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().join("state"));
        let mut manager = ProcessManager::new(state.clone(), dir.path().to_path_buf(), limits());
        let start = args(dir.path(), "bytes");
        let first = manager
            .start(shell_launch(dir.path(), "printf '\\377ok\\n'"), &start)
            .expect("start");
        let final_snapshot = if first.state == ProcessState::Exited {
            first
        } else {
            manager.wait(&first.handle, 5_000).expect("wait")
        };
        assert_eq!(final_snapshot.state, ProcessState::Exited);
        let id = final_snapshot.output_id.expect("output id");
        let output_dir = std::fs::read_dir(state.outputs())
            .expect("outputs")
            .next()
            .expect("repository output directory")
            .expect("repository output entry")
            .path();
        let log = output_dir.join(format!("{id}.log"));
        assert_eq!(std::fs::read(log).expect("read"), b"\xffok\n");
    }

    #[cfg(unix)]
    #[test]
    fn wait_is_bounded_and_terminate_reaps_the_process() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().join("state"));
        let mut manager = ProcessManager::new(state, dir.path().to_path_buf(), limits());
        let start = args(dir.path(), "sleep");
        let snapshot = manager
            .start(shell_launch(dir.path(), "sleep 30"), &start)
            .expect("start");
        assert_eq!(snapshot.state, ProcessState::Running);
        let before = Instant::now();
        let waited = manager.wait(&snapshot.handle, 30).expect("short wait");
        assert_eq!(waited.state, ProcessState::Running);
        assert!(before.elapsed() < Duration::from_secs(1));
        let stopped = manager.terminate(&snapshot.handle).expect("terminate");
        assert_eq!(stopped.state, ProcessState::Cancelled);
        assert!(stopped.output_id.is_some());
    }

    #[cfg(unix)]
    #[test]
    fn duplicate_idempotency_key_returns_the_same_handle() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().join("state"));
        let mut manager = ProcessManager::new(state, dir.path().to_path_buf(), limits());
        let start = args(dir.path(), "same");
        let first = manager
            .start(shell_launch(dir.path(), "sleep 1"), &start)
            .expect("first");
        let second = manager
            .start(shell_launch(dir.path(), "printf wrong"), &start)
            .expect("second");
        assert_eq!(first.handle, second.handle);
        manager.terminate(&first.handle).expect("terminate");
    }
}
