use std::collections::HashMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::super::CtxResult;
use super::super::event::input_hash;
use super::super::event::{
    Capabilities, NormalizedEvent, SessionId, SessionRef, StructuralContext,
};
use super::{AgentAdapter, ResolvedProgram, TurnSignalSetup};

/// Claude Code's own base layer, injected on every claude session zirv starts
/// (see `AgentAdapter::base_system_prompt`). Claude-specific by construction:
/// it names the Agent tool, `.claude/agents` and the `/code-review` skill, so
/// handing it to another agent would be handing it instructions about tools
/// that agent does not have.
///
/// Deliberately model-agnostic. It says "the model in this seat" and "the
/// most capable tier" rather than naming a lineup, because a hard-coded
/// lineup ages out of correctness the moment models are renamed, and because
/// model choice stays the operator's: this text never asks for `--model`.
pub const ORCHESTRATOR_PROMPT: &str = "\
zirv orchestrator conventions (claude)

You are an orchestrator. Coordination and judgment are the job, implementation is not: the \
orchestrator model is reserved for this seat, so delegate every substantive piece of work \
(codebase exploration, implementation, testing, review) to subagents via the Agent tool and keep \
your own context lean for planning, sequencing and integration.

- Bundle before you dispatch. Every spawn has a real startup cost, so never start an agent for one \
tiny task. Group small related tasks (same file or area, or a natural sequence) into a single \
checklist brief for one agent, with a per-item output format. Split across agents only when the \
tasks are independent and each side is substantial, then dispatch them in one message and prefer \
background dispatch so a slow worker blocks nothing. For a small follow-up in an area a worker \
just handled, continue that worker instead of spawning a fresh one.
- Route each dispatch to the cheapest model that can do the job, always cheaper than the model in \
this seat: the cheapest tier for mechanical and bulk work, a middle tier for ordinary exploration, \
implementation and test writing, and the most capable tier only for hard debugging and design \
exploration. Agents defined in .claude/agents pin their own models; do not override those.
- Write self-contained briefs. Subagents share none of your context, so state the goal, the \
constraints, the relevant file paths and the exact output format expected, and nothing else. Ask \
for compact structured findings, never raw file dumps.
- Decide rather than let a worker loop. Workers execute; choices between valid designs, \
architecture changes, and anything a worker has failed at twice come back to you. Do not read \
large files or write code yourself unless the change is trivial.
- Hold implementers to this repository's standards: follow the patterns already there, look for \
reusable code before adding new code, write a failing test first, keep diffs minimal, and run the \
project's format, lint and test commands before reporting back.
- Verify in batches: one independent reviewer gate per batch of related changes, not one per \
micro-task. You own the final integration, so resolve conflicts between agent outputs and report \
outcomes, including failures, plainly.
- Finish every development task with a full-diff review by a dedicated subagent running the \
/code-review skill, with model and effort scaled to the blast radius of the diff, scaling up when \
in doubt. Run it only once the other quality gates pass, then triage its findings, fix what is \
real, and rerun until it is clean before reporting the work done.";

fn text_of(message: &Value) -> String {
    message
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// Real context size is `input_tokens` plus both cache fields; the bare
/// `input_tokens` field is near zero once prompt caching kicks in.
pub fn context_tokens_of(usage: &Value) -> u64 {
    [
        "input_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
    ]
    .iter()
    .filter_map(|key| usage.get(*key).and_then(Value::as_u64))
    .sum()
}

pub fn parse_events(jsonl: &str) -> Vec<NormalizedEvent> {
    let mut events = Vec::new();

    for line in jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if row.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }

        match row.get("type").and_then(Value::as_str) {
            Some("user") => {
                if row.get("isMeta").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                let message = row.get("message").cloned().unwrap_or(Value::Null);
                let results: Vec<&Value> = message
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|blocks| {
                        blocks
                            .iter()
                            .filter(|b| {
                                b.get("type").and_then(Value::as_str) == Some("tool_result")
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                if results.is_empty() {
                    events.push(NormalizedEvent::TurnStart);
                    continue;
                }
                for block in results {
                    events.push(NormalizedEvent::ToolResult {
                        is_error: block
                            .get("is_error")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    });
                }
            }
            Some("assistant") => {
                let message = row.get("message").cloned().unwrap_or(Value::Null);
                let input_tokens = message.get("usage").map(context_tokens_of).unwrap_or(0);
                events.push(NormalizedEvent::AssistantFinal {
                    text: text_of(&message),
                    input_tokens,
                });

                if let Some(blocks) = message.get("content").and_then(Value::as_array) {
                    for block in blocks
                        .iter()
                        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
                    {
                        let name = block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                            .to_string();
                        let raw = block.get("input").map(Value::to_string).unwrap_or_default();
                        events.push(NormalizedEvent::ToolCall {
                            name,
                            input_hash: input_hash(&raw),
                        });
                    }
                }
            }
            Some("system")
                if row.get("subtype").and_then(Value::as_str) == Some("compact_boundary") =>
            {
                events.push(NormalizedEvent::Compaction);
            }
            _ => {}
        }
    }

    events
}

const FILE_KEYS: &[&str] = &["file_path", "notebook_path", "path"];
const ERROR_SNIPPET: usize = 200;

pub fn structural_context(jsonl: &str, last_n: usize) -> StructuralContext {
    let mut out = StructuralContext::default();

    for line in jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if row.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let message = row.get("message").cloned().unwrap_or(Value::Null);

        match row.get("type").and_then(Value::as_str) {
            Some("user") => {
                if row.get("isMeta").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                let content = message.get("content");
                if let Some(text) = content.and_then(Value::as_str) {
                    out.user_messages.push(text.to_string());
                    continue;
                }
                let Some(blocks) = content.and_then(Value::as_array) else {
                    continue;
                };
                for block in blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(text) = block.get("text").and_then(Value::as_str) {
                                out.user_messages.push(text.to_string());
                            }
                        }
                        Some("tool_result")
                            if block.get("is_error").and_then(Value::as_bool) == Some(true) =>
                        {
                            let detail = block
                                .get("content")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                                .unwrap_or_else(|| {
                                    block
                                        .get("content")
                                        .map(Value::to_string)
                                        .unwrap_or_default()
                                });
                            out.tool_errors
                                .push(detail.chars().take(ERROR_SNIPPET).collect());
                        }
                        _ => {}
                    }
                }
            }
            Some("assistant") => {
                let text = text_of(&message);
                if !text.trim().is_empty() {
                    out.assistant_texts.push(text);
                }
                let Some(blocks) = message.get("content").and_then(Value::as_array) else {
                    continue;
                };
                for block in blocks
                    .iter()
                    .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
                {
                    let Some(input) = block.get("input") else {
                        continue;
                    };
                    for key in FILE_KEYS {
                        if let Some(path) = input.get(*key).and_then(Value::as_str)
                            && !out.files_touched.iter().any(|p| p == path)
                        {
                            out.files_touched.push(path.to_string());
                        }
                    }
                }
            }
            _ => {}
        }
    }

    keep_last(&mut out.user_messages, last_n);
    keep_last(&mut out.assistant_texts, last_n);
    keep_last(&mut out.tool_errors, last_n);
    // Capped with everything else rather than left to accumulate: this is a
    // deduplicated list of every path the whole session ever named, and it
    // leaves as a single argv token in a handoff. Windows caps a command line
    // at 32,767 characters, so an uncapped list is a long session that can no
    // longer relaunch at all.
    keep_last(&mut out.files_touched, last_n);
    out
}

fn keep_last(items: &mut Vec<String>, last_n: usize) {
    if items.len() > last_n {
        items.drain(..items.len() - last_n);
    }
}

#[derive(Debug, Clone)]
pub struct ClaudeAdapter {
    program: String,
    bin_args: Vec<String>,
    home: Option<PathBuf>,
    #[cfg(test)]
    forced_file_support: Option<bool>,
}

impl ClaudeAdapter {
    /// `bin` may carry arguments, so `"sh /tmp/stub.sh"` and
    /// `"/usr/bin/env claude"` both work. The first token is the program and the
    /// rest lead every command this adapter builds.
    pub fn new(bin: Option<&str>) -> Self {
        let raw = bin.unwrap_or("claude").trim();
        let mut parts = raw.split_whitespace().map(str::to_string);
        let program = parts.next().unwrap_or_else(|| "claude".to_string());
        Self {
            program,
            bin_args: parts.collect(),
            home: None,
            #[cfg(test)]
            forced_file_support: None,
        }
    }

    /// Test seam: pins the home directory the transcript path is built from.
    #[cfg(test)]
    pub fn with_home(mut self, home: PathBuf) -> Self {
        self.home = Some(home);
        self
    }

    /// Test seam: bypasses the real `--help` probe so a unit test can force
    /// file-based or argv-based delivery without depending on the machine's
    /// installed binary.
    #[cfg(test)]
    pub fn with_file_support_forced(mut self, supported: bool) -> Self {
        self.forced_file_support = Some(supported);
        self
    }

    /// Every command starts here so the program and its leading arguments are
    /// applied uniformly to headless, interactive and distiller invocations,
    /// and so the Windows launcher rewrite (an npm-installed `claude` is
    /// `claude.cmd`, which `CreateProcess` refuses) is applied in exactly one
    /// place. A program zirv cannot resolve is spawned as written, which is
    /// today's behavior; `ready()` is where an unrunnable one is reported.
    fn base(&self) -> Command {
        let resolved = super::resolve_program(&self.program)
            .unwrap_or_else(|_| ResolvedProgram::direct(&self.program));
        let mut cmd = Command::new(&resolved.program);
        cmd.args(&resolved.prefix);
        cmd.args(&self.bin_args);
        cmd
    }

    fn home_dir(&self) -> PathBuf {
        self.home
            .clone()
            .or_else(|| crate::utils::home_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."))
    }
}

/// Bounds the `--help` probe below: a hang here must never hang the whole
/// launch, which would be a worse failure mode than falling back to argv.
const HELP_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Process-wide cache of `probe_system_prompt_file_support`'s answer, keyed
/// by the exact program invocation: the binary path plus its leading
/// arguments, since `ZIRV_CTX_AGENT_BIN` can point at different binaries (or
/// different versions of the same name resolved from a different PATH) and
/// each has its own answer. A restart inside the same `wrap`/`exec` run must
/// not re-spawn `--help` on every relaunch, which is what the cache is for;
/// it must not, in exchange, let one binary's probe answer for another's.
/// A tuple key rather than a joined string: joining `program` and `bin_args`
/// with spaces makes `("sh /tmp/x", ["--help"])` and `("sh", ["/tmp/x",
/// "--help"])` collide on the same string despite being different commands.
type ProbeKey = (PathBuf, Vec<String>);
static SYSTEM_PROMPT_FILE_SUPPORT: OnceLock<Mutex<HashMap<ProbeKey, bool>>> = OnceLock::new();

/// Probes the installed binary's own `--help` text for the file-based
/// system-prompt flag, so injection can move the composed prompt off argv
/// (visible to any other user on the machine via `ps`) without hard-coding a
/// version cutoff. Any failure to run or read the probe (binary missing,
/// timeout, whatever) is read as unsupported: this is a hardening on top of
/// argv delivery, never a new way to fail a launch.
fn probe_system_prompt_file_support(program: &str, bin_args: &[String]) -> bool {
    let key = (PathBuf::from(program), bin_args.to_vec());
    let cache = SYSTEM_PROMPT_FILE_SUPPORT.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(mut map) = cache.lock() else {
        return false;
    };
    if let Some(cached) = map.get(&key) {
        return *cached;
    }
    let detected = detect_help_flag(program, bin_args);
    map.insert(key, detected);
    detected
}

/// Verified against the real CLI (`claude --help`, v2.1.220): the flag is not
/// spelled out on its own line. It only appears folded into a shorthand,
/// `--append-system-prompt[-file]`, inside the `--bare` option's own
/// description ("Explicitly provide context via: --system-prompt[-file],
/// --append-system-prompt[-file], --add-dir ..."). Stripping `[` and `]`
/// before searching turns that shorthand into the plain flag text, and is a
/// no-op if some future help output ever spells the flag out on its own.
fn normalizes_to_advertise_the_file_flag(help_text: &str) -> bool {
    help_text
        .replace(['[', ']'], "")
        .contains("--append-system-prompt-file")
}

/// Runs `program --help` and reports whether its output names
/// `--append-system-prompt-file`. Stdin is nulled so an interactive TUI that
/// does not special-case `--help` (and just starts reading a line) gets an
/// immediate EOF instead of hanging the probe; stdout is drained on a
/// separate thread so a chatty `--help` cannot deadlock against the wait
/// loop by filling the pipe buffer.
fn detect_help_flag(program: &str, bin_args: &[String]) -> bool {
    // The same resolution the launch itself uses. Without it the probe and
    // the spawn disagree on Windows: `Command::new` only ever appends `.exe`,
    // so an npm-installed `claude.cmd` failed the probe here and then failed
    // the launch there, for two different reasons.
    let resolved =
        super::resolve_program(program).unwrap_or_else(|_| ResolvedProgram::direct(program));

    // SECURITY (FINDING 1): `bin_args` carries repo-controlled tokens on the
    // interactive path (`program_invocation` forwards every positional before
    // the first flag, e.g. `zirv chat --resume`'s handoff summary). When
    // `resolve_program` routes an npm-installed `claude.cmd` through
    // `cmd.exe /c <shim>`, cmd.exe reparses this whole probe command line, so a
    // metacharacter in `bin_args` would execute *here*, before the real launch
    // ever reaches its own `guard_cmd_shim_reparse`. Run the identical
    // fail-closed guard against the exact argv about to be spawned, and on a
    // rejection report "unsupported" (the same value every probe failure
    // yields) WITHOUT spawning -- the caller keeps argv delivery and the
    // payload is never executed.
    let mut probe_args: Vec<String> =
        Vec::with_capacity(resolved.prefix.len() + bin_args.len() + 1);
    probe_args.extend(resolved.prefix.iter().cloned());
    probe_args.extend(bin_args.iter().cloned());
    probe_args.push("--help".to_string());
    if super::guard_cmd_shim_reparse(&resolved.program, &probe_args).is_err() {
        return false;
    }

    let Ok(mut child) = Command::new(&resolved.program)
        .args(&resolved.prefix)
        .args(bin_args)
        .arg("--help")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };

    let mut stdout_pipe = child.stdout.take();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(mut pipe) = stdout_pipe.take() {
            let _ = pipe.read_to_string(&mut buf);
        }
        let _ = tx.send(buf);
    });

    let deadline = Instant::now() + HELP_PROBE_TIMEOUT;
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            let text = rx.recv_timeout(Duration::from_secs(1)).unwrap_or_default();
            return normalizes_to_advertise_the_file_flag(&text);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    false
}

/// Claude stores transcripts under a slug of the cwd with every character
/// outside `[A-Za-z0-9-]` replaced by `-`.
pub fn project_slug(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

impl AgentAdapter for ClaudeAdapter {
    fn name(&self) -> &'static str {
        "claude"
    }

    /// The one thing that can make this adapter unusable before it is asked
    /// to do anything: a program that resolves to a file this OS has no way
    /// to execute. Reported here, by name, rather than left to surface as a
    /// raw `os error 193` out of the spawn. A program that resolves to
    /// nothing at all is not an error here: that is the OS's own
    /// "not found", raised at spawn time where it has always been raised.
    fn ready(&self) -> CtxResult<()> {
        super::resolve_program(&self.program)?;
        Ok(())
    }

    fn detect(&self, command: &[String]) -> bool {
        command
            .first()
            .and_then(|p| Path::new(p).file_name())
            .map(|f| f.to_string_lossy() == "claude")
            .unwrap_or(false)
    }

    fn headless_cmd(&self, prompt: &str, session: &SessionId, extra: &[String]) -> Command {
        let mut cmd = self.base();
        cmd.arg("-p")
            .arg(prompt)
            .arg("--session-id")
            .arg(session.as_str())
            .args(extra);
        cmd
    }

    fn interactive_cmd(&self, initial_prompt: Option<&str>, extra: &[String]) -> Command {
        let mut cmd = self.base();
        if let Some(prompt) = initial_prompt {
            cmd.arg(prompt);
        }
        cmd.args(extra);
        cmd
    }

    fn system_prompt_args(&self, prompt: &str) -> Vec<String> {
        if prompt.trim().is_empty() {
            return Vec::new();
        }
        vec!["--append-system-prompt".to_string(), prompt.to_string()]
    }

    fn user_system_prompt_flag(&self) -> Option<&'static str> {
        Some("--append-system-prompt")
    }

    fn system_prompt_file_flag(&self) -> Option<&'static str> {
        Some("--append-system-prompt-file")
    }

    fn base_system_prompt(&self) -> Option<&'static str> {
        Some(ORCHESTRATOR_PROMPT)
    }

    /// Counted over the argv the operator wrote, not over the argv `base()`
    /// builds: `exec` uses this to strip the program tokens off the command
    /// it was handed before carrying the rest into a restart. The Windows
    /// launcher rewrite lives entirely inside `base()` and never touches that
    /// argv, so the prefix stays the program plus its own leading arguments.
    fn launch_prefix_len(&self) -> usize {
        1 + self.bin_args.len()
    }

    fn supports_system_prompt_file(&self, launch: &[String]) -> bool {
        #[cfg(test)]
        if let Some(forced) = self.forced_file_support {
            return forced;
        }
        // The binary that is about to run, not the one this adapter would
        // have chosen: wrap spawns the user's own argv.
        let (program, args) = super::program_invocation(launch)
            .unwrap_or_else(|| (self.program.clone(), self.bin_args.clone()));
        probe_system_prompt_file_support(&program, &args)
    }

    /// True on a Windows npm install, where `claude` is a `.cmd` shim that
    /// [`super::resolve_program`] routes through `cmd.exe /c`. That is the one
    /// launch shape where a headless prompt on argv would be reparsed by
    /// cmd.exe, so on it the prompt is delivered via stdin instead
    /// (`headless_cmd_stdin`).
    fn launches_through_cmd_shim(&self) -> bool {
        super::launches_through_cmd_shim(&self.program)
    }

    /// The `-p` headless launch with **no positional prompt**: claude then
    /// reads the prompt from stdin (verified by the distiller, which does
    /// exactly this). Everything else matches `headless_cmd`, so a stdin
    /// launch and an argv launch differ only in where the prompt travels.
    fn headless_cmd_stdin(&self, session: &SessionId, extra: &[String]) -> Option<Command> {
        let mut cmd = self.base();
        cmd.arg("-p")
            .arg("--session-id")
            .arg(session.as_str())
            .args(extra);
        Some(cmd)
    }

    /// The distillation prompt is piped to stdin so a long transcript tail
    /// never hits argv length limits. This child embeds untrusted repo
    /// CLAUDE.md text in its prompt (the judgment call) and its only job is
    /// to answer with text, so it never needs a tool. Verified against the
    /// real CLI (docs/superpowers/notes/2026-08-01-system-prompt-injection-facts.md,
    /// "I6 fix round"): `Bash` must be denied alongside `Write`/`Edit`, since
    /// a shell redirect otherwise recreates a Write tool, and the value must
    /// be one `=`-bound argv token, since the two-token form was verified to
    /// swallow the next argv entry.
    fn distiller_cmd(&self, model: &str) -> Command {
        let mut cmd = self.base();
        cmd.arg("-p")
            .arg("--model")
            .arg(model)
            .arg("--output-format")
            .arg("text")
            .arg("--disallowedTools=Write,Edit,Bash,NotebookEdit");
        cmd
    }

    fn transcript_path(&self, session: &SessionRef) -> PathBuf {
        let projects = self.home_dir().join(".claude").join("projects");
        let computed = projects
            .join(project_slug(&session.cwd))
            .join(format!("{}.jsonl", session.id));
        if computed.exists() {
            return computed;
        }

        // The slug rule is verified for `/` and `.` but not every character,
        // so fall back to finding the session file wherever it landed.
        let wanted = format!("{}.jsonl", session.id);
        if let Ok(entries) = std::fs::read_dir(&projects) {
            for entry in entries.flatten() {
                let candidate = entry.path().join(&wanted);
                if candidate.exists() {
                    return candidate;
                }
            }
        }
        computed
    }

    fn parse_events(&self, jsonl: &str) -> Vec<NormalizedEvent> {
        parse_events(jsonl)
    }

    fn structural_context(&self, jsonl: &str, last_n: usize) -> StructuralContext {
        structural_context(jsonl, last_n)
    }

    fn compact_command(&self) -> Option<&'static str> {
        Some("/compact")
    }

    fn quit_sequence(&self) -> &'static str {
        "/exit\r"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            marker_signal: true,
            token_usage: true,
            turn_signal: true,
            system_prompt: true,
        }
    }

    /// Verified against the real CLI (`claude --help`, v2.1.220): `--model
    /// <MODEL>` is a real flag.
    fn model_args(&self, model: &str) -> Vec<String> {
        vec!["--model".to_string(), model.to_string()]
    }

    /// `--resume <SESSION_ID>` is already a fact this codebase relies on
    /// elsewhere -- `wrap::extra_launch_flags` strips it (among the flags
    /// that pin a launch to an existing conversation) on every restart, and
    /// `exec.rs`'s own `RESUME_FLAGS_WITH_VALUE` treats it as a two-token
    /// flag -- so this is wiring up an already-verified flag for the
    /// dashboard's own restore path, not a fresh claim.
    fn resume_args(&self, session_id: &str) -> Option<Vec<String>> {
        Some(vec!["--resume".to_string(), session_id.to_string()])
    }

    /// The same `--session-id <uuid>` flag `headless_cmd` already pins every
    /// headless run with (verified against the real CLI), offered here so an
    /// *interactive* dashboard pane can be pinned too. That is what makes the
    /// roster's stored uuid the claude conversation id, and therefore what
    /// makes `resume_args` above resolve to a real conversation after a quit.
    fn session_pin_args(&self, session: &str) -> Vec<String> {
        vec!["--session-id".to_string(), session.to_string()]
    }

    fn register_turn_signal(&self, session: &SessionRef, socket: &Path) -> TurnSignalSetup {
        TurnSignalSetup {
            env: vec![
                (super::SOCKET_ENV.to_string(), socket.display().to_string()),
                (super::SESSION_ENV.to_string(), session.id.to_string()),
            ],
            instructions: "register a Stop hook running `zirv ctx hook stop` in \
                           ~/.claude/settings.json so turn boundaries reach the supervisor"
                .to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::event::{NormalizedEvent, input_hash};

    pub(crate) fn fixture_path(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    /// SECURITY (FINDING 1): `detect_help_flag` forwards `bin_args` (repo-
    /// controlled on the interactive path) into the `--help` probe argv. When
    /// the program resolves to the Windows `cmd.exe /c <shim>` form, cmd.exe
    /// would reparse a metacharacter in `bin_args` as a command -- so the probe
    /// must run the fail-closed `guard_cmd_shim_reparse` check *before* it
    /// spawns, and report "unsupported" (`false`) on rejection without ever
    /// executing anything. Proven by a shim that writes a sentinel next to
    /// itself if it ever runs: a metachar `bin_arg` leaves the sentinel absent,
    /// while a clean one spawns and creates it.
    #[cfg(windows)]
    #[test]
    fn detect_help_flag_refuses_to_spawn_when_a_bin_arg_would_be_reparsed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shim = dir.path().join("claude.cmd");
        // `%~dp0` is the shim's own directory, so the sentinel lands there
        // regardless of the probe's cwd. If this batch file ever runs, the
        // sentinel exists.
        std::fs::write(&shim, "@echo off\r\necho ran> \"%~dp0ran.marker\"\r\n")
            .expect("write shim");
        let sentinel = dir.path().join("ran.marker");

        // A metacharacter-bearing bin_arg: the guard must refuse before spawn.
        let detected = detect_help_flag(&shim.display().to_string(), &["foo&calc".to_string()]);
        assert!(!detected, "a rejected probe reports unsupported");
        assert!(!sentinel.exists(), "the shim must never have been spawned");

        // Control: a clean bin_arg passes the guard and does spawn the shim
        // (which then creates the sentinel), confirming the guard -- not some
        // unrelated failure -- is what stopped the metachar case above.
        let _ = detect_help_flag(&shim.display().to_string(), &["--model".to_string()]);
        assert!(
            sentinel.exists(),
            "a clean bin_arg is allowed through and the shim runs"
        );
    }

    /// The needles track `scripts/record-claude-fixture.py`'s SECRET pattern.
    /// A scrub rule with no guard behind it is a rule that can silently stop
    /// working, and the cost of that is a credential in a public repository.
    #[test]
    fn recorded_fixture_carries_no_personal_data() {
        let text = std::fs::read_to_string(fixture_path("claude-real-session.jsonl"))
            .expect("fixture must be committed");
        for needle in [
            "jonathansolskov",
            "/Users/",
            "sk-ant",
            "sk-proj",
            "ghp_",
            "gho_",
            "ghu_",
            "ghs_",
            "ghr_",
            "AKIA",
            "-----BEGIN",
            "ApiKey ",
            "Bearer ",
            "eyJ",
        ] {
            assert!(
                !text.contains(needle),
                "fixture leaks '{needle}'; re-run scripts/record-claude-fixture.py"
            );
        }
        assert_eq!(
            credential_shape(&text),
            None,
            "fixture leaks a credential-shaped string; re-run scripts/record-claude-fixture.py"
        );
        assert!(
            text.contains("compact_boundary"),
            "fixture must include a compaction"
        );
        assert!(
            text.lines().count() >= 50,
            "fixture is too small to be representative"
        );
    }

    /// The two scrub rules a literal needle cannot express: the fixture
    /// legitimately contains `?key=REDACTED` and `checkout@v2`, so only the
    /// credential-shaped forms of each count as a leak.
    fn credential_shape(text: &str) -> Option<String> {
        for (index, _) in text.match_indices("key=") {
            let hex: String = text[index + 4..]
                .chars()
                .take_while(char::is_ascii_hexdigit)
                .collect();
            if hex.len() >= 8 {
                return Some(format!("key={hex}"));
            }
        }

        for (index, _) in text.match_indices('@') {
            let has_local = text[..index]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric());
            let domain: String = text[index + 1..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-')
                .collect();
            let tld = domain.rsplit('.').next().unwrap_or_default();
            let is_email = has_local
                && domain.contains('.')
                && tld.len() >= 2
                && tld.chars().all(|c| c.is_ascii_alphabetic());
            if is_email {
                return Some(format!("@{domain}"));
            }
        }
        None
    }

    #[test]
    fn the_credential_shape_check_separates_secrets_from_ordinary_text() {
        assert_eq!(
            credential_shape("http://localhost:1/?key=deadbeefcafe"),
            Some("key=deadbeefcafe".to_string())
        );
        assert_eq!(
            credential_shape("mail someone@example.com now"),
            Some("@example.com".to_string())
        );
        assert_eq!(credential_shape("?key=REDACTED"), None);
        assert_eq!(credential_shape("uses: actions/checkout@v2"), None);
        assert_eq!(credential_shape("#[cfg(test)] @testable import"), None);
    }

    #[test]
    fn context_tokens_sum_the_cache_fields() {
        // Verified against a real transcript: input_tokens alone is 2 in a
        // 110k-token session, so the cache fields carry the real size.
        let usage = serde_json::json!({
            "input_tokens": 2,
            "cache_creation_input_tokens": 457,
            "cache_read_input_tokens": 108_427,
            "output_tokens": 577
        });
        assert_eq!(context_tokens_of(&usage), 108_886);
    }

    #[test]
    fn a_real_prompt_starts_a_turn_but_a_tool_result_does_not() {
        let jsonl = concat!(
            r#"{"type":"user","message":{"content":"do the thing"}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}"#,
            "\n"
        );
        let events = parse_events(jsonl);
        assert_eq!(
            events,
            vec![
                NormalizedEvent::TurnStart,
                NormalizedEvent::ToolResult { is_error: false },
            ]
        );
    }

    #[test]
    fn missing_is_error_counts_as_success() {
        let jsonl =
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}"#;
        assert_eq!(
            parse_events(jsonl),
            vec![NormalizedEvent::ToolResult { is_error: false }]
        );
    }

    #[test]
    fn assistant_yields_text_tokens_and_tool_calls() {
        let jsonl = concat!(
            r#"{"type":"assistant","message":{"content":["#,
            r#"{"type":"thinking","thinking":"hmm"},"#,
            r#"{"type":"text","text":"[zirv] on it"},"#,
            r#"{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}"#,
            r#"],"usage":{"input_tokens":10,"cache_read_input_tokens":90}}}"#
        );
        let events = parse_events(jsonl);
        assert_eq!(
            events,
            vec![
                NormalizedEvent::AssistantFinal {
                    text: "[zirv] on it".to_string(),
                    input_tokens: 100,
                },
                NormalizedEvent::ToolCall {
                    name: "Bash".to_string(),
                    input_hash: input_hash("{\"command\":\"ls\"}"),
                },
            ]
        );
    }

    #[test]
    fn tool_only_assistant_messages_still_report_tokens() {
        let jsonl = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t","name":"Read","input":{}}],"usage":{"input_tokens":5}}}"#;
        let events = parse_events(jsonl);
        assert_eq!(
            events[0],
            NormalizedEvent::AssistantFinal {
                text: String::new(),
                input_tokens: 5
            }
        );
    }

    #[test]
    fn compact_boundary_becomes_a_compaction_event() {
        let jsonl = r#"{"type":"system","subtype":"compact_boundary","compactMetadata":{"trigger":"manual"}}"#;
        assert_eq!(parse_events(jsonl), vec![NormalizedEvent::Compaction]);
    }

    #[test]
    fn sidechain_meta_and_garbage_lines_are_skipped() {
        let jsonl = concat!(
            r#"{"type":"assistant","isSidechain":true,"message":{"content":[{"type":"text","text":"sub"}],"usage":{}}}"#,
            "\n",
            r#"{"type":"user","isMeta":true,"message":{"content":"hook noise"}}"#,
            "\n",
            "not json at all\n",
            "\n",
            r#"{"type":"pr-link","prNumber":7}"#,
            "\n",
            r#"{"type":"user","message":{"content":"real prompt"}}"#,
            "\n"
        );
        assert_eq!(parse_events(jsonl), vec![NormalizedEvent::TurnStart]);
    }

    /// The invariant the incremental scoring path rests on (see the
    /// `parse_events` contract on `AgentAdapter`): this parser is line-local,
    /// so a transcript cut at newlines and parsed piecewise yields exactly the
    /// events one parse of the whole file yields.
    #[test]
    fn parsing_a_transcript_in_pieces_yields_the_same_events() {
        let jsonl =
            std::fs::read_to_string(fixture_path("claude-real-session.jsonl")).expect("fixture");
        let whole = parse_events(&jsonl);
        let lines: Vec<&str> = jsonl.lines().collect();

        for chunk in [1, 3, 17] {
            let pieced: Vec<NormalizedEvent> = lines
                .chunks(chunk)
                .flat_map(|piece| parse_events(&format!("{}\n", piece.join("\n"))))
                .collect();
            assert_eq!(pieced, whole, "in pieces of {chunk} lines");
        }
    }

    #[test]
    fn real_fixture_matches_recorded_expectations() {
        let jsonl =
            std::fs::read_to_string(fixture_path("claude-real-session.jsonl")).expect("fixture");
        let expected: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(fixture_path("claude-real-session.expected.json"))
                .expect("expectations"),
        )
        .expect("valid json");

        let events = parse_events(&jsonl);
        let count = |pred: &dyn Fn(&NormalizedEvent) -> bool| {
            events.iter().filter(|e| pred(e)).count() as u64
        };
        let want = |key: &str| {
            expected[key]
                .as_u64()
                .unwrap_or_else(|| panic!("{key} missing"))
        };

        assert_eq!(
            count(&|e| matches!(e, NormalizedEvent::TurnStart)),
            want("turn_start")
        );
        assert_eq!(
            count(&|e| matches!(e, NormalizedEvent::AssistantFinal { .. })),
            want("assistant")
        );
        assert_eq!(
            count(&|e| matches!(e, NormalizedEvent::ToolCall { .. })),
            want("tool_call")
        );
        assert_eq!(
            count(&|e| matches!(e, NormalizedEvent::ToolResult { is_error: true })),
            want("tool_result_error")
        );
        assert_eq!(
            count(&|e| matches!(e, NormalizedEvent::ToolResult { is_error: false })),
            want("tool_result_ok")
        );
        assert_eq!(
            count(&|e| matches!(e, NormalizedEvent::Compaction)),
            want("compaction")
        );

        let last_tokens = events
            .iter()
            .rev()
            .find_map(|e| match e {
                NormalizedEvent::AssistantFinal { input_tokens, .. } => Some(*input_tokens),
                _ => None,
            })
            .expect("fixture has assistant events");
        assert_eq!(last_tokens, want("last_context_tokens"));
        assert!(
            want("tool_result_error") >= 1,
            "fixture must contain a tool error"
        );
    }

    use crate::commands::ctx::adapters::{AgentAdapter, SESSION_ENV, SOCKET_ENV};
    use crate::commands::ctx::event::{SessionId, SessionRef};

    /// The flags an adapter-built command carries, with any launcher prefix
    /// dropped. On a Windows machine where `claude` is an npm `.cmd` shim
    /// every command this adapter builds starts `cmd.exe /c <shim>`, and
    /// those tokens are not what a test about agent flags is asserting on.
    fn built_args(adapter: &ClaudeAdapter, cmd: &Command) -> Vec<String> {
        let launcher = super::super::resolve_program(&adapter.program)
            .map(|resolved| resolved.prefix.len())
            .unwrap_or(0);
        cmd.get_args()
            .skip(launcher)
            .map(|a| a.to_string_lossy().to_string())
            .collect()
    }

    #[test]
    fn project_slug_matches_on_disk_evidence() {
        assert_eq!(
            project_slug(std::path::Path::new(
                "/Users/x/Documents/Privat/zirv-fitness-tracking"
            )),
            "-Users-x-Documents-Privat-zirv-fitness-tracking"
        );
        // A dot becomes a dash, which is why worktrees show up as `--claude-worktrees`.
        assert_eq!(
            project_slug(std::path::Path::new("/Users/x/repo/.claude-worktrees/b")),
            "-Users-x-repo--claude-worktrees-b"
        );
    }

    #[test]
    fn transcript_path_is_derived_from_home_and_cwd() {
        let home = tempfile::tempdir().expect("tempdir");
        let adapter = ClaudeAdapter::new(None).with_home(home.path().to_path_buf());
        let session = SessionRef {
            id: SessionId::parse("11111111-2222-4333-8444-555555555555"),
            cwd: std::path::PathBuf::from("/work/repo"),
        };
        assert_eq!(
            adapter.transcript_path(&session),
            home.path()
                .join(".claude/projects/-work-repo/11111111-2222-4333-8444-555555555555.jsonl")
        );
    }

    #[test]
    fn transcript_path_falls_back_to_scanning_when_the_slug_misses() {
        let home = tempfile::tempdir().expect("tempdir");
        let real = home.path().join(".claude/projects/some-other-slug");
        std::fs::create_dir_all(&real).expect("mkdir");
        let actual = real.join("11111111-2222-4333-8444-555555555555.jsonl");
        std::fs::write(&actual, "").expect("write");

        let adapter = ClaudeAdapter::new(None).with_home(home.path().to_path_buf());
        let session = SessionRef {
            id: SessionId::parse("11111111-2222-4333-8444-555555555555"),
            cwd: std::path::PathBuf::from("/work/repo"),
        };
        assert_eq!(adapter.transcript_path(&session), actual);
    }

    #[test]
    fn headless_cmd_pins_the_session_id() {
        let adapter = ClaudeAdapter::new(Some("/tmp/fake-claude"));
        let cmd = adapter.headless_cmd(
            "do the work",
            &SessionId::parse("abc"),
            &["--model".to_string(), "sonnet".to_string()],
        );
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(cmd.get_program().to_string_lossy(), "/tmp/fake-claude");
        assert_eq!(
            args,
            vec![
                "-p".to_string(),
                "do the work".to_string(),
                "--session-id".to_string(),
                "abc".to_string(),
                "--model".to_string(),
                "sonnet".to_string(),
            ]
        );
    }

    /// FIX B: the stdin headless form keeps `-p` and the session pin but never
    /// puts the prompt on argv -- claude reads it from stdin instead, so a
    /// prompt bearing a cmd.exe metacharacter is never reparsed on the shim
    /// form. The extra flags (the file-based system prompt, the operator's own)
    /// still ride on argv, exactly as `headless_cmd` places them.
    #[test]
    fn headless_cmd_stdin_omits_the_prompt_and_reads_it_from_stdin() {
        let adapter = ClaudeAdapter::new(Some("/tmp/fake-claude"));
        let cmd = adapter
            .headless_cmd_stdin(
                &SessionId::parse("abc"),
                &[
                    "--append-system-prompt-file".to_string(),
                    "/s/p.md".to_string(),
                ],
            )
            .expect("claude has a verified stdin form");
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(
            args,
            vec![
                "-p".to_string(),
                "--session-id".to_string(),
                "abc".to_string(),
                "--append-system-prompt-file".to_string(),
                "/s/p.md".to_string(),
            ],
            "no positional prompt token: the prompt travels on stdin"
        );
        assert!(
            !args.iter().any(|a| a.contains("foo") || a.contains('&')),
            "the prompt text is nowhere in argv: {args:?}"
        );
    }

    #[test]
    fn interactive_cmd_passes_the_initial_prompt_positionally() {
        let adapter = ClaudeAdapter::new(None);
        let with = adapter.interactive_cmd(Some("resume this"), &[]);
        assert_eq!(built_args(&adapter, &with), vec!["resume this".to_string()]);

        let without = adapter.interactive_cmd(None, &["--continue".to_string()]);
        assert_eq!(
            built_args(&adapter, &without),
            vec!["--continue".to_string()]
        );
    }

    #[test]
    fn distiller_cmd_uses_a_cheap_model_and_reads_stdin() {
        let adapter = ClaudeAdapter::new(None);
        let cmd = adapter.distiller_cmd("haiku");
        assert_eq!(
            built_args(&adapter, &cmd),
            vec![
                "-p".to_string(),
                "--model".to_string(),
                "haiku".to_string(),
                "--output-format".to_string(),
                "text".to_string(),
                "--disallowedTools=Write,Edit,Bash,NotebookEdit".to_string(),
            ]
        );
    }

    /// I6: the judgment/distiller child embeds untrusted repo CLAUDE.md text
    /// in its prompt and otherwise runs with the operator's full tool
    /// permissions. Verified against the real CLI (see
    /// docs/superpowers/notes/2026-08-01-system-prompt-injection-facts.md):
    /// this is the one flag, in the one argv shape, that provably blocks
    /// tool use, including an adversarial attempt to route around it via
    /// Bash or Task delegation.
    #[test]
    fn the_distiller_denies_the_tools_verified_to_matter() {
        let adapter = ClaudeAdapter::new(None);
        let cmd = adapter.distiller_cmd("haiku");
        let args = built_args(&adapter, &cmd);

        let deny = args
            .iter()
            .find(|a| a.starts_with("--disallowedTools="))
            .expect("the distiller must restrict its own tools");
        assert_eq!(
            deny, "--disallowedTools=Write,Edit,Bash,NotebookEdit",
            "must be one argv token (a two-token --flag value form was \
             verified to swallow the next argv entry instead): {args:?}"
        );
        assert!(
            deny.contains("Bash"),
            "Bash alone bypasses a Write/Edit-only deny list via a shell \
             redirect, verified against the real CLI: {deny}"
        );
    }

    /// A multi-word agent bin (`ZIRV_CTX_AGENT_BIN="sh /tmp/stub.sh"`) has to work
    /// for all three invocation kinds: exec restarts build headless commands,
    /// handoff distillation builds a distiller command, and wrap restarts build an
    /// interactive one.
    #[test]
    fn a_multi_word_agent_bin_is_split_across_every_command_kind() {
        let adapter = ClaudeAdapter::new(Some("sh /tmp/stub.sh"));

        let headless = adapter.headless_cmd("go", &SessionId::parse("abc"), &[]);
        assert_eq!(headless.get_program().to_string_lossy(), "sh");
        let args: Vec<String> = headless
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(
            args,
            vec![
                "/tmp/stub.sh".to_string(),
                "-p".to_string(),
                "go".to_string(),
                "--session-id".to_string(),
                "abc".to_string(),
            ],
            "the bin arguments come before the agent flags"
        );

        let interactive = adapter.interactive_cmd(Some("resume"), &[]);
        assert_eq!(interactive.get_program().to_string_lossy(), "sh");
        let args: Vec<String> = interactive
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(args, vec!["/tmp/stub.sh".to_string(), "resume".to_string()]);

        let distiller = adapter.distiller_cmd("haiku");
        assert_eq!(distiller.get_program().to_string_lossy(), "sh");
        let args: Vec<String> = distiller
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(
            args,
            vec![
                "/tmp/stub.sh".to_string(),
                "-p".to_string(),
                "--model".to_string(),
                "haiku".to_string(),
                "--output-format".to_string(),
                "text".to_string(),
                "--disallowedTools=Write,Edit,Bash,NotebookEdit".to_string(),
            ]
        );
    }

    #[test]
    fn a_single_word_bin_and_extra_whitespace_still_work() {
        let adapter = ClaudeAdapter::new(Some("  /opt/homebrew/bin/claude  "));
        let cmd = adapter.interactive_cmd(None, &[]);
        assert_eq!(
            cmd.get_program().to_string_lossy(),
            "/opt/homebrew/bin/claude"
        );
        assert_eq!(cmd.get_args().count(), 0);
    }

    #[test]
    fn turn_signal_setup_exports_socket_and_session() {
        let adapter = ClaudeAdapter::new(None);
        let session = SessionRef {
            id: SessionId::parse("sess-1"),
            cwd: std::path::PathBuf::from("/work/repo"),
        };
        let setup = adapter.register_turn_signal(&session, std::path::Path::new("/tmp/s/ab.sock"));
        assert!(
            setup
                .env
                .contains(&(SOCKET_ENV.to_string(), "/tmp/s/ab.sock".to_string()))
        );
        assert!(
            setup
                .env
                .contains(&(SESSION_ENV.to_string(), "sess-1".to_string()))
        );
        assert!(
            setup.instructions.contains("zirv ctx hook stop"),
            "instructions should name the hook command: {}",
            setup.instructions
        );
    }

    #[test]
    fn structural_context_extracts_prompts_files_and_errors() {
        let jsonl = concat!(
            r#"{"type":"user","message":{"content":"first prompt"}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"a","name":"Read","input":{"file_path":"/work/src/lib.rs"}}],"usage":{}}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"boom: file missing","is_error":true}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"[zirv] fixed it"}],"usage":{}}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"type":"text","text":"second prompt"}]}}"#,
            "\n"
        );
        let ctx = structural_context(jsonl, 5);
        assert_eq!(ctx.user_messages, vec!["first prompt", "second prompt"]);
        assert_eq!(ctx.assistant_texts, vec!["[zirv] fixed it"]);
        assert_eq!(ctx.files_touched, vec!["/work/src/lib.rs"]);
        assert_eq!(ctx.tool_errors.len(), 1);
        assert!(ctx.tool_errors[0].contains("boom"));
    }

    #[test]
    fn structural_context_keeps_only_the_last_n_and_dedupes_files() {
        let mut jsonl = String::new();
        for i in 0..6 {
            jsonl.push_str(&format!(
                "{{\"type\":\"user\",\"message\":{{\"content\":\"p{i}\"}}}}\n"
            ));
            jsonl.push_str(
                "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"a\",\"name\":\"Read\",\"input\":{\"file_path\":\"/same.rs\"}}],\"usage\":{}}}\n",
            );
        }
        let ctx = structural_context(&jsonl, 2);
        assert_eq!(ctx.user_messages, vec!["p4", "p5"]);
        assert_eq!(ctx.files_touched, vec!["/same.rs"]);
    }

    #[test]
    fn the_system_prompt_becomes_the_verified_flag_pair() {
        // Exactly the mechanism recorded in
        // docs/superpowers/notes/2026-08-01-system-prompt-injection-facts.md.
        let adapter = ClaudeAdapter::new(None);
        assert_eq!(
            adapter.system_prompt_args("be consistent"),
            vec![
                "--append-system-prompt".to_string(),
                "be consistent".to_string()
            ]
        );
    }

    #[test]
    fn an_empty_prompt_injects_nothing() {
        let adapter = ClaudeAdapter::new(None);
        assert!(adapter.system_prompt_args("").is_empty());
        assert!(adapter.system_prompt_args("   \n").is_empty());
    }

    /// I2: this must name the exact flag `system_prompt_args` emits, so a
    /// caller can find a user's own use of it and merge rather than override.
    #[test]
    fn the_user_facing_flag_name_matches_what_system_prompt_args_emits() {
        let adapter = ClaudeAdapter::new(None);
        assert_eq!(
            adapter.user_system_prompt_flag(),
            Some("--append-system-prompt")
        );
    }

    #[test]
    fn claude_advertises_the_capability() {
        assert!(ClaudeAdapter::new(None).capabilities().system_prompt);
    }

    #[test]
    fn model_args_uses_the_verified_flag() {
        let adapter = ClaudeAdapter::new(None);
        assert_eq!(
            adapter.model_args("opus"),
            vec!["--model".to_string(), "opus".to_string()]
        );
    }

    #[test]
    fn resume_args_uses_the_verified_flag() {
        let adapter = ClaudeAdapter::new(None);
        assert_eq!(
            adapter.resume_args("sess-1"),
            Some(vec!["--resume".to_string(), "sess-1".to_string()])
        );
    }

    #[test]
    fn the_file_flag_name_is_the_documented_one() {
        assert_eq!(
            ClaudeAdapter::new(None).system_prompt_file_flag(),
            Some("--append-system-prompt-file")
        );
    }

    /// Writes a throwaway `--help` stub so the probe can be exercised without
    /// depending on the machine's actual installed binary. The heredoc keeps
    /// `help_text` free of shell-escaping concerns. Every caller spawns it via
    /// `sh`, so it is unix-only like they are.
    #[cfg(unix)]
    fn help_stub(dir: &std::path::Path, name: &str, help_text: &str) -> String {
        let script = dir.join(name);
        std::fs::write(
            &script,
            format!("#!/bin/sh\ncat <<'EOF'\n{help_text}\nEOF\n"),
        )
        .expect("write stub");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        format!("sh {}", script.display())
    }

    #[cfg(unix)]
    #[test]
    fn supports_system_prompt_file_detects_the_flag_from_help_output() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = help_stub(
            dir.path(),
            "probe-yes.sh",
            "Options:\n  --append-system-prompt-file <path>",
        );
        let adapter = ClaudeAdapter::new(Some(&bin));
        assert!(adapter.supports_system_prompt_file(&[]));
    }

    /// Verified against the real CLI (`claude --help`, v2.1.220): the flag is
    /// never spelled out on its own; it only appears folded into this exact
    /// shorthand, inside the `--bare` option's own description. A probe that
    /// only looked for the plain flag text would report "unsupported" on the
    /// very machine this was verified on.
    #[cfg(unix)]
    #[test]
    fn supports_system_prompt_file_detects_the_real_clis_bracket_shorthand() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = help_stub(
            dir.path(),
            "probe-bracket.sh",
            "Explicitly provide context via: --system-prompt[-file],\n  \
             --append-system-prompt[-file], --add-dir (CLAUDE.md dirs)",
        );
        let adapter = ClaudeAdapter::new(Some(&bin));
        assert!(
            adapter.supports_system_prompt_file(&[]),
            "must recognize the bracket-shorthand form the real CLI actually uses"
        );
    }

    #[test]
    fn normalizes_to_advertise_the_file_flag_matches_both_spellings() {
        assert!(normalizes_to_advertise_the_file_flag(
            "--append-system-prompt[-file] <path>"
        ));
        assert!(normalizes_to_advertise_the_file_flag(
            "--append-system-prompt-file <path>"
        ));
        assert!(!normalizes_to_advertise_the_file_flag(
            "--append-system-prompt <prompt>"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn supports_system_prompt_file_is_false_when_help_omits_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = help_stub(dir.path(), "probe-no.sh", "nothing relevant here");
        let adapter = ClaudeAdapter::new(Some(&bin));
        assert!(!adapter.supports_system_prompt_file(&[]));
    }

    #[test]
    fn supports_system_prompt_file_fails_open_when_the_binary_is_missing() {
        let adapter = ClaudeAdapter::new(Some("/nonexistent/definitely-not-a-binary"));
        assert!(
            !adapter.supports_system_prompt_file(&[]),
            "a probe failure must never block a launch"
        );
    }

    /// M7: "probe... once per launch (cache the result in-process across
    /// restarts)". Rewriting the stub after the first probe must not change
    /// the cached answer: a restart inside the same run must never re-spawn
    /// `--help`.
    #[cfg(unix)]
    #[test]
    fn supports_system_prompt_file_is_cached_after_the_first_probe() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = help_stub(dir.path(), "probe-cache.sh", "--append-system-prompt-file");
        let adapter = ClaudeAdapter::new(Some(&bin));
        assert!(adapter.supports_system_prompt_file(&[]));

        let script = dir.path().join("probe-cache.sh");
        std::fs::write(&script, "#!/bin/sh\nprintf 'nothing now\\n'\n").expect("rewrite stub");
        assert!(
            adapter.supports_system_prompt_file(&[]),
            "the first probe's answer is cached for the life of the process"
        );
    }

    /// Regression: the cache used to be keyed by joining `program` and
    /// `bin_args` into one string, which made two distinct commands collide
    /// on the same key (e.g. `("sh /a", ["--help"])` and `("sh", ["/a",
    /// "--help"])` both joined to `"sh /a --help"`). Same `program` ("sh"),
    /// different `bin_args` (two different scripts, one supporting the flag
    /// and one not) must be cached independently, not share one answer.
    #[cfg(unix)]
    #[test]
    fn the_cache_key_distinguishes_different_bin_args_for_the_same_program() {
        let dir = tempfile::tempdir().expect("tempdir");
        let supports = dir.path().join("supports.sh");
        std::fs::write(
            &supports,
            "#!/bin/sh\ncat <<'EOF'\n--append-system-prompt-file\nEOF\n",
        )
        .expect("write");
        let unsupported = dir.path().join("unsupported.sh");
        std::fs::write(&unsupported, "#!/bin/sh\ncat <<'EOF'\nnothing here\nEOF\n").expect("write");
        for script in [&supports, &unsupported] {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }

        assert!(
            probe_system_prompt_file_support("sh", &[supports.display().to_string()]),
            "the supporting script's own answer must not be shadowed by the other's"
        );
        assert!(
            !probe_system_prompt_file_support("sh", &[unsupported.display().to_string()]),
            "the non-supporting script must get its own answer, not the cached true from above"
        );
    }

    #[test]
    fn the_prompt_args_compose_with_the_existing_command_builders() {
        let adapter = ClaudeAdapter::new(None);
        let mut extra = adapter.system_prompt_args("be consistent");
        extra.push("--model".to_string());
        extra.push("sonnet".to_string());

        let headless = adapter.headless_cmd("go", &SessionId::parse("abc"), &extra);
        assert_eq!(
            built_args(&adapter, &headless),
            vec![
                "-p".to_string(),
                "go".to_string(),
                "--session-id".to_string(),
                "abc".to_string(),
                "--append-system-prompt".to_string(),
                "be consistent".to_string(),
                "--model".to_string(),
                "sonnet".to_string(),
            ]
        );

        let interactive = adapter.interactive_cmd(None, &extra);
        assert_eq!(
            built_args(&adapter, &interactive)[0],
            "--append-system-prompt"
        );
    }

    #[test]
    fn structural_context_survives_the_real_fixture() {
        let jsonl =
            std::fs::read_to_string(fixture_path("claude-real-session.jsonl")).expect("fixture");
        let expected: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(fixture_path("claude-real-session.expected.json"))
                .expect("expectations"),
        )
        .expect("valid json");
        let recorded = expected["files_touched_min"].as_u64().unwrap_or(0);
        assert!(
            structural_context(&jsonl, 1_000).files_touched.len() as u64 >= recorded,
            "files_touched should find at least the recorded count"
        );

        let ctx = structural_context(&jsonl, 5);
        assert!(ctx.user_messages.len() <= 5);
        assert!(
            ctx.files_touched.len() <= 5,
            "and then keep only the tail, like every other field: {}",
            ctx.files_touched.len()
        );
    }

    /// A handoff leaves as a single argv token, and Windows caps a command
    /// line at 32,767 characters. `files_touched` accumulated every unique
    /// path of the whole session while its neighbours were capped, so a long
    /// enough session could no longer relaunch at all.
    #[test]
    fn structural_context_caps_files_touched_like_every_other_field() {
        let mut jsonl = String::new();
        for index in 0..40 {
            jsonl.push_str(&format!(
                "{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"tool_use\",\"id\":\"a\",\"name\":\"Read\",\"input\":{{\"file_path\":\"/src/file-{index}.rs\"}}}}],\"usage\":{{}}}}}}\n"
            ));
        }

        let ctx = structural_context(&jsonl, 5);
        assert_eq!(
            ctx.files_touched,
            vec![
                "/src/file-35.rs",
                "/src/file-36.rs",
                "/src/file-37.rs",
                "/src/file-38.rs",
                "/src/file-39.rs"
            ],
            "the newest paths are the ones a handoff is worth carrying"
        );
    }

    #[test]
    fn claude_contributes_the_orchestrator_layer_and_it_names_claudes_own_tools() {
        let layer = ClaudeAdapter::new(None)
            .base_system_prompt()
            .expect("claude has a base layer of its own");
        assert_eq!(layer, ORCHESTRATOR_PROMPT);
        for claude_specific in ["Agent tool", ".claude/agents", "/code-review"] {
            assert!(
                layer.contains(claude_specific),
                "the layer is claude-specific by construction: '{claude_specific}'"
            );
        }
    }

    /// `exec` strips this many leading tokens off the argv the operator
    /// wrote before carrying the rest into a restart, so a rewrite that
    /// happens inside `base()` must not be counted here: the argv it applies
    /// to is the one the adapter builds, not the one it was handed.
    #[test]
    fn the_launch_prefix_length_counts_the_operators_argv_not_the_rewritten_one() {
        assert_eq!(ClaudeAdapter::new(None).launch_prefix_len(), 1);
        assert_eq!(
            ClaudeAdapter::new(Some("claude.cmd")).launch_prefix_len(),
            1,
            "a .cmd shim is still one program token in the operator's argv"
        );
        assert_eq!(
            ClaudeAdapter::new(Some("sh /tmp/stub.sh")).launch_prefix_len(),
            2
        );
    }

    /// An npm-installed `claude` on Windows is `claude.cmd`, which
    /// `CreateProcessW` rejects with `ERROR_BAD_EXE_FORMAT` (193). The
    /// adapter has to hand it to `cmd.exe` instead, and the tokens it adds
    /// have to lead the ones it was already going to pass.
    #[cfg(windows)]
    #[test]
    fn a_cmd_shim_is_launched_through_cmd_exe_with_its_arguments_intact() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shim = dir.path().join("claude.cmd");
        std::fs::write(&shim, "@echo off\r\n").expect("write shim");

        let adapter = ClaudeAdapter::new(Some(&shim.display().to_string()));
        let cmd = adapter.interactive_cmd(Some("resume this"), &["--continue".to_string()]);

        assert!(
            cmd.get_program()
                .to_string_lossy()
                .to_lowercase()
                .contains("cmd"),
            "got {:?}",
            cmd.get_program()
        );
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(
            args,
            vec![
                "/c".to_string(),
                shim.display().to_string(),
                "resume this".to_string(),
                "--continue".to_string(),
            ]
        );
        assert_eq!(
            adapter.launch_prefix_len(),
            1,
            "and the rewrite never changes what exec strips off the operator's argv"
        );
    }

    /// The rewrite is a Windows concern only: everywhere else the program is
    /// spawned exactly as written, shebang and all.
    #[cfg(not(windows))]
    #[test]
    fn a_program_is_spawned_exactly_as_written_off_windows() {
        let adapter = ClaudeAdapter::new(Some("/opt/claude.cmd"));
        let cmd = adapter.interactive_cmd(Some("resume this"), &[]);
        assert_eq!(cmd.get_program().to_string_lossy(), "/opt/claude.cmd");
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(args, vec!["resume this".to_string()]);
    }
}
