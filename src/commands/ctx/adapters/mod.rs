use std::path::{Path, PathBuf};
use std::process::Command;

pub mod claude;
pub mod codex;

use super::CtxResult;
use super::event::{Capabilities, NormalizedEvent, SessionId, SessionRef, StructuralContext};

/// How an adapter arranges for turn-boundary events to reach a supervisor's
/// socket. `env` is injected into the launched agent so the hook that runs
/// inside it can find the socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnSignalSetup {
    pub env: Vec<(String, String)>,
    pub instructions: String,
}

pub const SOCKET_ENV: &str = "ZIRV_CTX_SOCKET";
pub const SESSION_ENV: &str = "ZIRV_CTX_SESSION";

/// `Debug` is a supertrait so `Box<dyn AgentAdapter>` can appear in
/// `Result::expect_err` (the registry tests assert on the unknown-adapter
/// error path); every adapter already derives it.
pub trait AgentAdapter: std::fmt::Debug {
    fn name(&self) -> &'static str;

    /// `Err` when the adapter exists but is not safe to use yet, so callers
    /// fail loudly instead of scoring garbage.
    fn ready(&self) -> CtxResult<()>;

    fn detect(&self, command: &[String]) -> bool;

    fn headless_cmd(&self, prompt: &str, session: &SessionId, extra: &[String]) -> Command;
    fn interactive_cmd(&self, initial_prompt: Option<&str>, extra: &[String]) -> Command;
    fn distiller_cmd(&self, model: &str) -> Command;

    /// Arguments that add `prompt` to this agent's system prompt for one run.
    /// Empty when the agent has no verified mechanism, which is how an
    /// unsupported agent ships without injection rather than with a guess.
    fn system_prompt_args(&self, prompt: &str) -> Vec<String>;

    /// This agent's own base system prompt: text that only makes sense for
    /// this agent, because it names that agent's tools and conventions.
    /// Composed as a base layer, after the shipped default and before every
    /// layer a human wrote, so the user, repo and command-line layers all
    /// still append after it and still take precedence.
    ///
    /// `None` (the default) means this agent contributes nothing of its own,
    /// which is what an agent whose tool vocabulary zirv has not verified
    /// must do rather than be handed another agent's instructions.
    fn base_system_prompt(&self) -> Option<&'static str> {
        None
    }

    /// The user-facing flag name `system_prompt_args` emits, when the agent has
    /// one. Lets a caller find and merge a user's own use of the flag instead
    /// of silently overriding it with a second occurrence. `None` when the
    /// agent has no such flag, which is also the default: nothing to merge.
    fn user_system_prompt_flag(&self) -> Option<&'static str> {
        None
    }

    /// The user-facing flag name that delivers the composed prompt via a
    /// file path instead of argv text, when this agent has a verified one.
    /// `None` (the default) means: use `system_prompt_args`, which puts the
    /// prompt on argv instead.
    fn system_prompt_file_flag(&self) -> Option<&'static str> {
        None
    }

    /// Whether the binary about to be spawned advertises
    /// `system_prompt_file_flag` in its own `--help`. Probed rather than
    /// assumed: an adapter can know a flag's name and still find it missing
    /// from an older install.
    ///
    /// `launch` is the argv the caller is about to spawn, and the probe must
    /// hit exactly that program: `wrap` spawns the user's own argv, which can
    /// be an entirely different install from the one `agent_bin` names, and
    /// handing the file flag to a binary that does not have it fails the
    /// launch outright. An empty `launch` means the adapter's own program.
    ///
    /// `false` -- the default, and the fallback for any probe failure -- means
    /// argv delivery via `system_prompt_args`, never a blocked launch.
    fn supports_system_prompt_file(&self, launch: &[String]) -> bool {
        let _ = launch;
        false
    }

    /// How many leading argv tokens are the program invocation itself rather
    /// than flags the operator passed. One for a bare binary; more when
    /// `agent_bin` carries arguments, since `"/usr/bin/env claude"` spends two
    /// tokens before the first real flag. A relaunch rebuilds the invocation
    /// from `headless_cmd`, so anything inside this prefix must never be
    /// carried over as if the operator had asked for it.
    fn launch_prefix_len(&self) -> usize {
        1
    }

    fn transcript_path(&self, session: &SessionRef) -> PathBuf;

    /// Must be line-local: every line's events depend on that line alone, so
    /// parsing a transcript in pieces cut at newlines and concatenating the
    /// results is the same as parsing the whole of it. The incremental scoring
    /// path in `score.rs` feeds each adapter only the bytes appended since the
    /// last pass, and that is what makes it equal to a full parse.
    fn parse_events(&self, jsonl: &str) -> Vec<NormalizedEvent>;
    fn structural_context(&self, jsonl: &str, last_n: usize) -> StructuralContext;

    fn compact_command(&self) -> Option<&'static str>;
    fn quit_sequence(&self) -> &'static str;
    fn capabilities(&self) -> Capabilities;
    fn register_turn_signal(&self, session: &SessionRef, socket: &Path) -> TurnSignalSetup;
}

/// The program invocation at the head of an argv: the binary plus the leading
/// arguments before the first flag, which is what `sh wrapper.sh --foo` and
/// `/usr/bin/env claude -p x` both need. Anything past that is the operator's
/// own flags and has no business being passed to a `--help` probe.
pub fn program_invocation(launch: &[String]) -> Option<(String, Vec<String>)> {
    let (program, rest) = launch.split_first()?;
    let args = rest
        .iter()
        .take_while(|arg| !arg.starts_with('-'))
        .cloned()
        .collect();
    Some((program.clone(), args))
}

/// A program invocation rewritten so the host OS can actually execute it.
/// `prefix` is the tokens that have to lead the original arguments, empty
/// whenever the program can be spawned directly (always, off Windows).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProgram {
    pub program: String,
    pub prefix: Vec<String>,
}

impl ResolvedProgram {
    /// The invocation exactly as written: no launcher, nothing prepended.
    pub fn direct(program: &str) -> Self {
        Self {
            program: program.to_string(),
            prefix: Vec::new(),
        }
    }
}

/// Resolves `program` the way the OS itself would, and rewrites the
/// invocation when what it resolves to cannot be handed to the process
/// creation call directly.
///
/// Off Windows this is the identity: `execvp` honors the shebang of anything
/// on `PATH`, so there is nothing to rewrite.
///
/// On Windows it matters. An npm-installed `claude` is `claude.cmd`, and the
/// two resolvers zirv uses disagreed about it: `std::process::Command` only
/// ever appends `.exe`, while portable-pty's `search_path` honors `PATHEXT`,
/// finds `claude.cmd`, and then hands it to `CreateProcessW` as
/// `lpApplicationName`, which rejects it with `ERROR_BAD_EXE_FORMAT` (193).
/// Resolving `PATH` plus `PATHEXT` here and routing a `.cmd`/`.bat` through
/// `cmd.exe` (a `.ps1` through PowerShell) is what makes the most common
/// Windows install layout launch at all.
///
/// A program that resolves to nothing is returned untouched, so a missing
/// binary still fails with the OS's own "not found" rather than a zirv error
/// about a path that does not exist. `Err` is reserved for the one case zirv
/// can name before spawning and knows will fail: a bare name that `PATHEXT`
/// resolved to a file type with no launcher. A program written with a
/// directory in it is never an error here, whatever it ends in: the caller
/// named that exact file, and a wrapper this code has never heard of is
/// theirs to be told about by the OS, exactly as before.
#[cfg(windows)]
pub fn resolve_program(program: &str) -> Result<ResolvedProgram, String> {
    let Some((resolved, from_path)) = resolve_on_path(program) else {
        return Ok(ResolvedProgram::direct(program));
    };
    let extension = resolved
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let found = resolved.display().to_string();
    match extension.as_str() {
        "cmd" | "bat" => Ok(ResolvedProgram {
            program: std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string()),
            prefix: vec!["/c".to_string(), found],
        }),
        "ps1" => Ok(ResolvedProgram {
            program: "powershell".to_string(),
            prefix: vec!["-NoProfile".to_string(), "-File".to_string(), found],
        }),
        other if from_path && !matches!(other, "exe" | "com" | "") => Err(format!(
            "cannot launch '{program}': it resolves to '{found}', which Windows cannot execute \
             directly (CreateProcess accepts only .exe and .com). zirv runs .cmd and .bat through \
             cmd.exe and .ps1 through PowerShell, but it has no launcher for '.{other}'."
        )),
        // Directly executable, or named explicitly enough that the caller
        // owns the outcome. Deliberately keeps the program spelled the way it
        // was written rather than substituting the resolved path: nothing
        // about the launch changes, so nothing about it should.
        _ => Ok(ResolvedProgram::direct(program)),
    }
}

#[cfg(not(windows))]
pub fn resolve_program(program: &str) -> Result<ResolvedProgram, String> {
    Ok(ResolvedProgram::direct(program))
}

/// `PATH` plus `PATHEXT`, the search the Windows shell performs and
/// `std::process::Command` does not. A program that already carries a
/// directory is looked for where it says, not on `PATH`; the flag reports
/// which of the two happened, because only a `PATH` hit is a name the shell
/// itself would have claimed to be executable.
#[cfg(windows)]
fn resolve_on_path(program: &str) -> Option<(PathBuf, bool)> {
    if program.is_empty() {
        return None;
    }
    let extensions: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
        .split(';')
        .filter(|ext| !ext.is_empty())
        .map(|ext| ext.to_ascii_lowercase())
        .collect();

    let named_directory = program.contains('/') || program.contains('\\');
    let bases: Vec<PathBuf> = if named_directory {
        vec![PathBuf::from(program)]
    } else {
        std::env::var_os("PATH")
            .map(|path| {
                std::env::split_paths(&path)
                    .map(|dir| dir.join(program))
                    .collect()
            })
            .unwrap_or_default()
    };

    let from_path = !named_directory;
    for base in bases {
        // An explicit extension that exists wins outright, so
        // `claude.cmd` is never resolved to `claude.cmd.exe`.
        if base.extension().is_some() && base.is_file() {
            return Some((base, from_path));
        }
        for extension in &extensions {
            let candidate = PathBuf::from(format!("{}{extension}", base.display()));
            if candidate.is_file() {
                return Some((candidate, from_path));
            }
        }
        if base.is_file() {
            return Some((base, from_path));
        }
    }
    None
}

pub fn all(bin: Option<&str>) -> Vec<Box<dyn AgentAdapter>> {
    vec![
        Box::new(claude::ClaudeAdapter::new(bin)),
        Box::new(codex::CodexAdapter::new(bin)),
    ]
}

/// Explicit `--agent` name, else detection from the wrapped argv, else claude.
pub fn select(
    name: Option<&str>,
    command: &[String],
    bin: Option<&str>,
) -> CtxResult<Box<dyn AgentAdapter>> {
    let adapters = all(bin);

    if let Some(name) = name {
        let found = adapters.into_iter().find(|a| a.name() == name);
        let adapter = found.ok_or_else(|| {
            format!(
                "unknown agent '{name}'; known adapters: {}",
                all(None)
                    .iter()
                    .map(|a| a.name())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
        adapter.ready()?;
        return Ok(adapter);
    }

    if let Some(adapter) = adapters.into_iter().find(|a| a.detect(command)) {
        adapter.ready()?;
        return Ok(adapter);
    }

    let adapter: Box<dyn AgentAdapter> = Box::new(claude::ClaudeAdapter::new(bin));
    adapter.ready()?;
    Ok(adapter)
}

/// True when the wrapped command can be trusted to actually be this adapter's
/// agent: either the operator named it explicitly (`--agent`, or the config's
/// `agent` key), or detection matched the command's own argv. Neither true
/// means `select`'s last arm defaulted here with nothing to back it up (an
/// arbitrary wrapped command that matches no adapter), and injecting this
/// adapter's own flags (e.g. `--append-system-prompt`) into whatever program
/// that turns out to be would leak them into its output instead of an agent
/// that would ever read them.
pub fn command_matches_adapter(
    adapter: &dyn AgentAdapter,
    agent_explicit: bool,
    command: &[String],
) -> bool {
    agent_explicit || adapter.detect(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// M7 probed the adapter's own program while `wrap` spawned the user's
    /// argv, so the file flag could be handed to a binary that never
    /// advertised it -- failing the launch outright, which is the one thing
    /// the probe promises never to do. The probe target now comes from the
    /// argv about to be spawned, which means finding the invocation in it.
    #[test]
    fn the_program_invocation_stops_at_the_first_flag() {
        let argv =
            |parts: &[&str]| -> Vec<String> { parts.iter().map(|s| s.to_string()).collect() };

        assert_eq!(
            program_invocation(&argv(&["claude", "-p", "task"])),
            Some(("claude".to_string(), vec![]))
        );
        assert_eq!(
            program_invocation(&argv(&["/usr/bin/env", "claude", "-p", "task"])),
            Some(("/usr/bin/env".to_string(), vec!["claude".to_string()]))
        );
        assert_eq!(
            program_invocation(&argv(&["sh", "/opt/wrap.sh", "--model", "opus"])),
            Some(("sh".to_string(), vec!["/opt/wrap.sh".to_string()]))
        );
        assert_eq!(program_invocation(&[]), None, "nothing to probe");
    }

    /// Off Windows there is nothing to rewrite, and on Windows a program that
    /// is already directly executable is spawned exactly as it was written.
    #[test]
    fn a_directly_executable_program_is_left_alone() {
        let resolved = resolve_program("claude").expect("resolvable");
        assert_eq!(resolved.program, "claude");
        assert!(
            resolved.prefix.is_empty() || cfg!(windows),
            "only Windows ever inserts a launcher"
        );

        let missing = resolve_program("definitely-not-a-program-anywhere").expect("no error");
        assert_eq!(
            missing,
            ResolvedProgram::direct("definitely-not-a-program-anywhere"),
            "a program that resolves to nothing keeps the OS's own not-found"
        );
    }

    /// The npm install layout: `claude` on `PATH` is `claude.cmd`, which
    /// `CreateProcessW` rejects outright. `PATHEXT` finds it, and `cmd.exe`
    /// is what can actually run it.
    #[cfg(windows)]
    #[test]
    fn a_cmd_shim_is_rewritten_to_run_through_cmd_exe() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shim = dir.path().join("shim-agent.cmd");
        std::fs::write(&shim, "@echo off\r\n").expect("write");

        let resolved = resolve_program(&shim.display().to_string()).expect("resolvable");
        assert!(
            resolved.program.to_lowercase().contains("cmd"),
            "got {}",
            resolved.program
        );
        assert_eq!(
            resolved.prefix,
            vec!["/c".to_string(), shim.display().to_string()]
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_powershell_script_is_rewritten_to_run_through_powershell() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("shim-agent.ps1");
        std::fs::write(&script, "exit 0\r\n").expect("write");

        let resolved = resolve_program(&script.display().to_string()).expect("resolvable");
        assert_eq!(resolved.program, "powershell");
        assert_eq!(
            resolved.prefix,
            vec![
                "-NoProfile".to_string(),
                "-File".to_string(),
                script.display().to_string()
            ]
        );
    }

    /// A bare name resolved off `PATH` is one the shell itself claimed to be
    /// executable, so a file type with no launcher is a failure zirv can name
    /// before spawning instead of letting it surface as `os error 193`. A
    /// program written with a directory in it is the caller's own choice and
    /// is never an error here, whatever it ends in.
    #[cfg(windows)]
    #[test]
    fn an_unlaunchable_program_on_path_is_named_rather_than_left_to_error_193() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("shim-agent.py");
        std::fs::write(&script, "print('x')\n").expect("write");

        assert_eq!(
            resolve_program(&script.display().to_string()),
            Ok(ResolvedProgram::direct(&script.display().to_string())),
            "an explicit path is the caller's own choice"
        );

        // Temporarily put the directory on PATH so the bare name resolves the
        // way the shell would, with `.PY` advertised on PATHEXT.
        let path = std::env::var("PATH").unwrap_or_default();
        let pathext = std::env::var("PATHEXT").unwrap_or_default();
        unsafe {
            std::env::set_var("PATH", format!("{};{}", dir.path().display(), path));
            std::env::set_var("PATHEXT", ".EXE;.CMD;.PY");
        }
        let err = resolve_program("shim-agent").expect_err("no launcher for .py");
        unsafe {
            std::env::set_var("PATH", path);
            std::env::set_var("PATHEXT", pathext);
        }

        assert!(err.contains("shim-agent.py"), "the error names it: {err}");
        assert!(err.contains("shim-agent"), "and what was asked for: {err}");
    }

    /// The trait default: an agent zirv has verified nothing about receives
    /// no base layer, rather than another agent's instructions.
    #[test]
    fn only_the_agent_a_base_layer_was_written_for_receives_it() {
        assert!(
            claude::ClaudeAdapter::new(None)
                .base_system_prompt()
                .is_some(),
            "claude has one of its own"
        );
        assert_eq!(codex::CodexAdapter::new(None).base_system_prompt(), None);
    }

    #[test]
    fn explicit_name_wins() {
        let adapter = select(Some("claude"), &[], None).expect("claude selects");
        assert_eq!(adapter.name(), "claude");
    }

    #[test]
    fn detection_reads_the_wrapped_argv() {
        let cmd = vec![
            "/opt/homebrew/bin/claude".to_string(),
            "--resume".to_string(),
        ];
        let adapter = select(None, &cmd, None).expect("detect claude");
        assert_eq!(adapter.name(), "claude");
    }

    #[test]
    fn empty_command_defaults_to_claude() {
        let adapter = select(None, &[], None).expect("default");
        assert_eq!(adapter.name(), "claude");
    }

    #[test]
    fn unknown_name_is_an_error_that_lists_the_options() {
        let err = select(Some("gemini"), &[], None).expect_err("unknown agent");
        let msg = err.to_string();
        assert!(msg.contains("gemini"), "got {msg}");
        assert!(
            msg.contains("claude"),
            "error should list known adapters: {msg}"
        );
    }

    #[test]
    fn registry_exposes_both_v1_adapters() {
        let names: Vec<&str> = all(None).iter().map(|a| a.name()).collect();
        assert_eq!(names, vec!["claude", "codex"]);
    }

    /// The gate wrap and exec use before injecting: a command that matches no
    /// adapter, with no explicit `--agent` to back it, must not be treated as
    /// a match just because `select` had to default to one.
    #[test]
    fn an_undetected_command_with_no_explicit_agent_does_not_match() {
        let adapter = claude::ClaudeAdapter::new(None);
        let command = vec!["echo".to_string(), "hello".to_string()];
        assert!(!command_matches_adapter(&adapter, false, &command));
    }

    #[test]
    fn an_explicit_agent_matches_regardless_of_the_command() {
        let adapter = claude::ClaudeAdapter::new(None);
        let command = vec!["echo".to_string(), "hello".to_string()];
        assert!(command_matches_adapter(&adapter, true, &command));
    }

    #[test]
    fn a_detected_command_matches_even_without_an_explicit_agent() {
        let adapter = claude::ClaudeAdapter::new(None);
        let command = vec!["/opt/homebrew/bin/claude".to_string()];
        assert!(command_matches_adapter(&adapter, false, &command));
    }
}
