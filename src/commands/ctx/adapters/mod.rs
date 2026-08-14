use std::path::{Path, PathBuf};
use std::process::Command;

pub mod claude;
pub mod codex;

use super::CtxResult;
use super::config::CtxConfig;
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
/// Tells a spawned session which agent it is running as. Deliberately the
/// same name as `ctx.toml`'s own `agent` config key (`ZIRV_CTX_AGENT` in
/// `config::ENV_MAP`): it states the same fact from the other direction, so a
/// nested `zirv ctx ...` invocation inside a worker's own child processes
/// defaults to that worker's own harness rather than re-resolving from
/// scratch. Read by `mail::run_send`/`mail::run_inbox` to identify the
/// calling session without requiring an explicit `--to`/`--agent` flag.
pub const AGENT_ENV: &str = "ZIRV_CTX_AGENT";

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

    /// Whether a headless launch this adapter builds resolves to the Windows
    /// `cmd.exe /c <shim>` form (an npm-installed `.cmd`), where cmd.exe
    /// reparses the whole downstream command line. `false` (the default) off
    /// Windows and for a directly executable program. When `true`, a caller
    /// delivers the headless prompt -- and any folded mail -- on the child's
    /// stdin via [`headless_cmd_stdin`](Self::headless_cmd_stdin) rather than
    /// as an argv token, so that untrusted free text never reaches cmd.exe's
    /// parser (`guard_cmd_shim_reparse` is only the fail-closed backstop).
    fn launches_through_cmd_shim(&self) -> bool {
        false
    }

    /// A headless launch that expects its prompt on **stdin** rather than as
    /// the `-p <prompt>` argv token, for the
    /// [`launches_through_cmd_shim`](Self::launches_through_cmd_shim) case.
    /// `None` (the default) means this agent has no verified stdin form, so the
    /// caller keeps argv delivery. When `Some`, the returned `Command` reads
    /// its prompt from stdin to EOF -- the same mechanism the distiller uses --
    /// and the caller must pipe the prompt in.
    fn headless_cmd_stdin(&self, session: &SessionId, extra: &[String]) -> Option<Command> {
        let _ = (session, extra);
        None
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

    /// Argv tokens that select `model` for one interactive launch (the
    /// dashboard's orchestrator pane, via `chat.model`/`ZIRV_CTX_CHAT_MODEL`).
    /// Appended after the launch prefix, alongside any other `extra` argv
    /// `interactive_cmd` receives. The default is empty, matching every other
    /// "no verified mechanism" trait default on this trait
    /// (`system_prompt_args`, `base_system_prompt`): an adapter with no
    /// verified flag ships with no model selection rather than a guess.
    ///
    /// Both current adapters override this, so nothing calls the default body
    /// through `dyn AgentAdapter` yet -- wired into the orchestrator pane's
    /// argv when `chat.rs` builds it (dashboard Task 6).
    #[allow(dead_code)]
    fn model_args(&self, model: &str) -> Vec<String> {
        let _ = model;
        Vec::new()
    }

    /// Argv tokens that resume `session_id`'s own conversation, for the
    /// dashboard's quit/restore roster (`dash::roster::restore_argv`, called
    /// through `dyn AgentAdapter` -- unlike `model_args` above, both
    /// adapters reach this default body today, since codex does not
    /// override it). `None` -- the default, and every "no verified
    /// mechanism" trait default's own answer -- means this agent's resume
    /// story is unverified: a restore falls back to a fresh launch carrying
    /// a plain one-line "resuming after a dashboard restart" prompt instead
    /// of trying to guess a flag.
    fn resume_args(&self, session_id: &str) -> Option<Vec<String>> {
        let _ = session_id;
        None
    }

    /// Argv tokens that make this agent adopt zirv's own `session` uuid as the
    /// id of the conversation it is about to start, so a later
    /// [`resume_args`](Self::resume_args) against that same uuid finds
    /// something. Empty -- the default -- means the agent mints its own
    /// conversation id and zirv's uuid is only ever a zirv-side handle.
    ///
    /// Appended **only** to a dashboard pane's launch (`chat.rs::
    /// dash_orchestrator_pane` and `dash::fulfill_spawn_request`, both of
    /// which own a freshly minted uuid), never inside `interactive_cmd`
    /// itself: `wrap`'s relaunch path deliberately lets the harness mint a
    /// fresh conversation on every restart, and a restored pane already
    /// carries `resume_args`, which would conflict with a pin.
    ///
    /// Without this, the dashboard's restore roster stored a uuid the agent
    /// had never heard of: `claude --resume <zirv-uuid>` answered "no
    /// conversation found" and the restored pane died immediately.
    fn session_pin_args(&self, session: &str) -> Vec<String> {
        let _ = session;
        Vec::new()
    }
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

/// The cmd.exe metacharacters that, appearing RAW in an argument, cmd.exe
/// re-parses out of its own `/c` command line rather than passing through to
/// the shim it invokes. portable-pty and `std::process` both append a
/// no-whitespace metachar-bearing argument to a Windows command line unquoted,
/// and an embedded `"` toggles cmd.exe out of any quoting that *was* added
/// (BatBadBut / CVE-2024-24576's quote-toggle). Newline and carriage return
/// terminate the command line outright. Any of these in a shim-form argument
/// is therefore a command-injection primitive, not a literal argument value.
#[cfg(windows)]
const CMD_REPARSE_METACHARS: &[char] =
    &['&', '|', '<', '>', '^', '(', ')', '%', '!', '"', '\n', '\r'];

/// Whether `program` + `args` is the `cmd.exe /c <shim>` launcher form that
/// [`resolve_program`] produces for a `.cmd`/`.bat` on Windows: the program's
/// file stem is `cmd` and the first argument is `/c`. Matched structurally
/// (case-insensitively) rather than by identity with a specific `COMSPEC`
/// value, so a full-path or upper-cased `CMD.EXE` is recognised too.
#[cfg(windows)]
fn is_cmd_shim_launch(program: &str, args: &[String]) -> bool {
    let program_is_cmd = Path::new(program)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(|stem| stem.eq_ignore_ascii_case("cmd"))
        .unwrap_or(false);
    program_is_cmd
        && args
            .first()
            .map(|first| first.eq_ignore_ascii_case("/c"))
            .unwrap_or(false)
}

/// FIX D (defense-in-depth): the number of leading `args` tokens that are the
/// zirv-controlled launcher prefix, when `program` + `args` is a Windows
/// launcher form whose command line is reparsed before it reaches the real
/// script -- either the `cmd.exe /c <shim>` form (a `.cmd`/`.bat`) or the
/// `powershell -NoProfile -File <script>` form (a `.ps1`), both of which
/// [`resolve_program`] produces. `None` when it is neither, so a direct
/// `.exe` or an `sh <script>` fake agent is not keyed on at all. Keyed on the
/// `/c` / `-File` structure rather than only the launcher basename, so the
/// guard covers whichever launcher the resolver actually inserted.
#[cfg(windows)]
fn reparse_launcher_prefix(program: &str, args: &[String]) -> Option<usize> {
    let stem = Path::new(program)
        .file_stem()
        .and_then(|stem| stem.to_str())?
        .to_ascii_lowercase();
    match stem.as_str() {
        "cmd" => args
            .first()
            .map(|first| first.eq_ignore_ascii_case("/c"))
            .unwrap_or(false)
            // `/c` and the shim path are both zirv-controlled.
            .then_some(2),
        "powershell" | "pwsh" => {
            // Everything through the `-File <script>` pair is the launcher
            // prefix; the script's own arguments follow it.
            let file_at = args
                .iter()
                .position(|arg| arg.eq_ignore_ascii_case("-File"))?;
            (file_at + 1 < args.len()).then_some(file_at + 2)
        }
        _ => None,
    }
}

/// FIX (command-injection defense): fail-closed guard for the one launch shape
/// where a downstream argv element becomes cmd.exe *source text* rather than a
/// literal argument. When [`resolve_program`] rewrites an npm-installed
/// `claude.cmd` to `cmd.exe /c <shim>`, cmd.exe parses the whole appended
/// command line before invoking the shim, so any argument after the shim path
/// that carries a cmd.exe metacharacter is re-interpreted as a command. Repo-
/// controlled strings (an injected system prompt, a passed-through flag) reach
/// this argv, so an unguarded metacharacter there is arbitrary code execution
/// on a victim who merely runs a supervised session in a hostile checkout.
///
/// This rejects such a launch outright rather than trying to quote around
/// cmd.exe (which the embedded-quote toggle defeats). It is deliberately a
/// pure decision function over the already-resolved `program`/`args`, called
/// at every spawn seam (`supervise::spawn_tapped` for the headless
/// `exec`/`loop` path; the `CommandBuilder` assembly in `wrap` and
/// `dash::pane` for the pty path), so there is one metacharacter policy.
///
/// A no-op off Windows, and on Windows for any launch that is not the shim
/// form: a direct `.exe`, an `sh <script>` fake agent, or a program with no
/// launcher prefix is spawned exactly as before. zirv's own flags never carry
/// these characters, so only injected content is ever rejected. The two shim-
/// prefix tokens themselves (`/c` and the shim path) are zirv-controlled and
/// skipped.
pub fn guard_cmd_shim_reparse(program: &str, args: &[String]) -> Result<(), String> {
    #[cfg(windows)]
    {
        if let Some(prefix) = reparse_launcher_prefix(program, args) {
            for arg in args.iter().skip(prefix) {
                if let Some(bad) = arg.chars().find(|c| CMD_REPARSE_METACHARS.contains(c)) {
                    return Err(format!(
                        "refusing to launch: argument '{arg}' contains the cmd.exe \
                         metacharacter {bad:?}. zirv routes this agent through a Windows \
                         launcher ('cmd.exe /c' for an npm-installed '.cmd' shim, or \
                         'powershell -File' for a '.ps1'), which would re-parse that character \
                         as a command rather than pass it through. This is a fail-closed \
                         backstop against command injection; zirv's own arguments never contain \
                         these characters, and untrusted content (the composed system prompt, a \
                         headless task prompt) is kept off this argv entirely."
                    ));
                }
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (program, args);
    }
    Ok(())
}

/// Whether spawning `program` resolves to the Windows `cmd.exe /c <shim>`
/// launcher form (an npm-installed `.cmd`), where cmd.exe reparses the whole
/// downstream command line. The adapters use it to move a headless prompt --
/// and any folded mail -- onto the child's stdin on exactly the launch shape
/// where an argv token would otherwise be reparsed. Always `false` off
/// Windows, and for a directly executable program.
pub fn launches_through_cmd_shim(program: &str) -> bool {
    #[cfg(windows)]
    {
        match resolve_program(program) {
            Ok(resolved) => is_cmd_shim_launch(&resolved.program, &resolved.prefix),
            Err(_) => false,
        }
    }
    #[cfg(not(windows))]
    {
        let _ = program;
        false
    }
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

/// An adapter constructor: the same shape `ClaudeAdapter::new` and
/// `CodexAdapter::new` already share, named so `ADAPTERS` reads as a table
/// rather than a wall of type punctuation.
pub type AdapterCtor = fn(Option<&str>) -> Box<dyn AgentAdapter>;

fn make_claude(bin: Option<&str>) -> Box<dyn AgentAdapter> {
    Box::new(claude::ClaudeAdapter::new(bin))
}

fn make_codex(bin: Option<&str>) -> Box<dyn AgentAdapter> {
    Box::new(codex::CodexAdapter::new(bin))
}

/// The single source of truth for which adapters exist: a name paired with a
/// constructor. Adding an adapter is one entry here (plus its own module) --
/// `all`, `select`'s fallback, `describe_known_adapters`, `resolve_default`
/// and `readiness_note` all walk this table rather than naming adapters by
/// hand, so none of them can drift from it.
pub const ADAPTERS: &[(&str, AdapterCtor)] = &[("claude", make_claude), ("codex", make_codex)];

pub fn all(bin: Option<&str>) -> Vec<Box<dyn AgentAdapter>> {
    ADAPTERS.iter().map(|(_, ctor)| ctor(bin)).collect()
}

/// The registry's names, each suffixed `(disabled)` when `gate` refuses it --
/// used by the unknown-name error so a mistyped `--agent` also shows which
/// known names are actually usable right now.
fn describe_known_adapters(gate: &crate::settings::AgentGate) -> String {
    ADAPTERS
        .iter()
        .map(|(name, _)| {
            if gate.is_enabled(name) {
                name.to_string()
            } else {
                format!("{name} (disabled)")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The adapters that are both gate-enabled and `ready()` right now, in
/// registry order. Used to spell out actual options in an error instead of a
/// single hardcoded name -- `wrap`'s undetected-command refusal in
/// particular, which used to say "pass --agent claude" no matter how many
/// adapters the registry actually held.
pub fn available_adapter_names(cfg: &CtxConfig) -> Vec<&'static str> {
    let bin = cfg.agent_bin.as_deref();
    ADAPTERS
        .iter()
        .filter(|(name, ctor)| cfg.agents.is_enabled(name) && ctor(bin).ready().is_ok())
        .map(|(name, _)| *name)
        .collect()
}

/// A short clause naming every adapter that is not ready yet, for `zirv ctx
/// --help`'s `about` text. Generated from each adapter's own `ready()`
/// rather than hardcoded, so a newly wired-up adapter falls out of the
/// sentence on its own once it starts returning `Ok`, and a third adapter
/// that is also not ready is named without an edit here. Empty once every
/// adapter is ready.
pub fn readiness_note() -> String {
    let not_ready: Vec<&str> = ADAPTERS
        .iter()
        .filter(|(_, ctor)| ctor(None).ready().is_err())
        .map(|(name, _)| *name)
        .collect();
    if not_ready.is_empty() {
        return String::new();
    }
    format!("Not ready yet: {} (see issue #11).", not_ready.join(", "))
}

/// Which rule picked the default adapter, for callers (`zirv ctx status`,
/// diagnostics) that want to explain the choice rather than just use it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultOrigin {
    /// `cfg.agent` named it explicitly.
    Configured,
    /// No configured agent; this was the first adapter in registry order
    /// that was both gate-enabled and `ready()`.
    FirstEnabledReady,
}

/// Resolves the adapter `select` falls back to when neither an explicit
/// `--agent` nor detection named one: `cfg.agent` if set, else the first
/// registry entry that is both gate-enabled and `ready()`. Every call site
/// of `select` already folds `cfg.agent` into the `name` it passes in, so by
/// the time `select`'s fallback arm calls this, `cfg.agent` is always `None`
/// there -- but this function stands on its own (and is tested that way),
/// since a `None` name is not the only way to reach "use the configured or
/// default agent".
///
/// When nothing qualifies, the error aggregates one line per adapter naming
/// why it was skipped, reusing the gate's own refusal text and each
/// adapter's own `ready()` text rather than inventing new wording.
pub fn resolve_default(cfg: &CtxConfig) -> CtxResult<(Box<dyn AgentAdapter>, DefaultOrigin)> {
    let bin = cfg.agent_bin.as_deref();

    if let Some(name) = cfg.agent.as_deref() {
        let adapter = ADAPTERS
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, ctor)| ctor(bin))
            .ok_or_else(|| {
                format!(
                    "unknown agent '{name}'; known adapters: {}",
                    describe_known_adapters(&cfg.agents)
                )
            })?;
        if let Some(refusal) = cfg.agents.refusal(adapter.name()) {
            return Err(refusal.into());
        }
        adapter.ready()?;
        return Ok((adapter, DefaultOrigin::Configured));
    }

    let mut reasons = Vec::new();
    for (name, ctor) in ADAPTERS {
        let adapter = ctor(bin);
        if let Some(refusal) = cfg.agents.refusal(name) {
            reasons.push(format!("{name}: {refusal}"));
            continue;
        }
        match adapter.ready() {
            Ok(()) => return Ok((adapter, DefaultOrigin::FirstEnabledReady)),
            Err(e) => reasons.push(format!("{name}: {e}")),
        }
    }
    Err(format!(
        "no agent is both enabled and ready:\n{}",
        reasons.join("\n")
    )
    .into())
}

/// Explicit `--agent` name, else detection from the wrapped argv, else
/// `resolve_default`. The `.settings.toml` gate (`cfg.agents`) is checked
/// before `ready()` in every arm: `ready()` reports implementation state,
/// the gate reports operator policy, and a disabled agent must report the
/// disable rather than (for codex) "not implemented yet".
pub fn select(
    name: Option<&str>,
    command: &[String],
    cfg: &CtxConfig,
) -> CtxResult<Box<dyn AgentAdapter>> {
    let bin = cfg.agent_bin.as_deref();
    let adapters = all(bin);

    if let Some(name) = name {
        let found = adapters.into_iter().find(|a| a.name() == name);
        let adapter = found.ok_or_else(|| {
            format!(
                "unknown agent '{name}'; known adapters: {}",
                describe_known_adapters(&cfg.agents)
            )
        })?;
        if let Some(refusal) = cfg.agents.refusal(adapter.name()) {
            return Err(refusal.into());
        }
        adapter.ready()?;
        return Ok(adapter);
    }

    if let Some(adapter) = adapters.into_iter().find(|a| a.detect(command)) {
        if let Some(refusal) = cfg.agents.refusal(adapter.name()) {
            return Err(refusal.into());
        }
        adapter.ready()?;
        return Ok(adapter);
    }

    resolve_default(cfg).map(|(adapter, _origin)| adapter)
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

    /// A permissive `CtxConfig` (every agent enabled, no `agent_bin`
    /// override) for tests that only care about selection, not gating.
    /// `CtxConfig::default()` never touches the filesystem or `HOME` (its
    /// `AgentGate` is `AgentGate::default()`, not a `load`), so this one
    /// needs no `HomeGuard`, unlike `cfg_disabling` below.
    fn permissive_cfg() -> CtxConfig {
        CtxConfig::default()
    }

    /// A `CtxConfig` whose gate disables exactly one named agent, as if an
    /// operator or repo `.settings.toml` had set `[agents.<name>] enabled =
    /// false`, but without touching any file: `AgentGate`'s fields are
    /// crate-private, so the state is built by loading a real settings file
    /// from an isolated repo dir instead. `AgentGate::load` also reads the
    /// operator (home) layer, so this isolates `HOME`/`USERPROFILE` too --
    /// otherwise a developer machine's real `~/.zirv/.settings.toml` (if any)
    /// would leak into the loaded gate.
    fn cfg_disabling(name: &str) -> CtxConfig {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/.settings.toml"),
            format!("[agents.{name}]\nenabled = false\n"),
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        CtxConfig {
            agents: crate::settings::AgentGate::load(repo.path(), &|k| empty.get(k).cloned())
                .expect("load"),
            ..CtxConfig::default()
        }
    }

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
        //
        // NEW-1: a guard, not a manual restore. The restore used to sit after
        // an `expect_err`, so a failing resolution left this process with a
        // mangled `PATH` and a `PATHEXT` of `.EXE;.CMD;.PY` -- the highest
        // blast radius of any leak in the suite, since every later test that
        // spawns anything resolves its program through both.
        let path = std::env::var("PATH").unwrap_or_default();
        let _path_guard = crate::commands::ctx::testenv::VarGuard::set(&[
            (
                "PATH",
                Some(format!("{};{}", dir.path().display(), path).as_str()),
            ),
            ("PATHEXT", Some(".EXE;.CMD;.PY")),
        ]);
        let err = resolve_program("shim-agent").expect_err("no launcher for .py");

        assert!(err.contains("shim-agent.py"), "the error names it: {err}");
        assert!(err.contains("shim-agent"), "and what was asked for: {err}");
    }

    /// FIX 2a: a `cmd.exe /c <shim>` launch whose downstream arguments carry a
    /// cmd.exe metacharacter is refused, because cmd.exe re-parses that
    /// character as a command rather than passing it through to the shim. This
    /// is the RCE-closing guard, tested as a pure decision function -- no
    /// process is spawned.
    #[cfg(windows)]
    #[test]
    fn a_shim_form_launch_with_a_metachar_arg_is_refused() {
        let args = vec![
            "/c".to_string(),
            "C:\\tools\\claude.cmd".to_string(),
            "-p".to_string(),
            "foo&calc".to_string(),
        ];
        let err = guard_cmd_shim_reparse("cmd.exe", &args)
            .expect_err("a metachar after the shim path is command injection");
        assert!(
            err.contains("foo&calc"),
            "the error names the offending arg: {err}"
        );

        // A full-path, upper-cased COMSPEC is recognised structurally too.
        assert!(
            guard_cmd_shim_reparse(
                "C:\\Windows\\System32\\CMD.EXE",
                &[
                    "/C".to_string(),
                    "claude.cmd".to_string(),
                    "\"; calc; \"".to_string(),
                ],
            )
            .is_err(),
            "an embedded quote (the BatBadBut toggle) is rejected regardless of cmd casing"
        );
    }

    /// FIX 2a: the two shim-prefix tokens (`/c` and the shim path) are
    /// zirv-controlled and never trip the guard, and a clean downstream arg --
    /// including a real Bedrock model id with `:` `/` `.` -- passes. Runs on
    /// every platform: off Windows it exercises the no-op path, on Windows the
    /// real allow decision.
    #[test]
    fn a_shim_form_launch_with_only_clean_args_is_allowed() {
        let args = vec![
            "/c".to_string(),
            "C:\\tools\\claude.cmd".to_string(),
            "-p".to_string(),
            "do the thing".to_string(),
            "--model".to_string(),
            "us.anthropic.claude-sonnet-4-v1:0".to_string(),
        ];
        assert!(guard_cmd_shim_reparse("cmd.exe", &args).is_ok());
    }

    /// FIX D (defense-in-depth): the `powershell -NoProfile -File <script>`
    /// launcher form is guarded the same way as the cmd shim -- everything
    /// through the `-File <script>` pair is zirv-controlled prefix, and a
    /// metacharacter in a token after it is refused. The two prefix tokens and
    /// the script path never trip it.
    #[cfg(windows)]
    #[test]
    fn a_powershell_file_launch_is_guarded_after_the_script_path() {
        let bad = vec![
            "-NoProfile".to_string(),
            "-File".to_string(),
            "C:\\tools\\agent.ps1".to_string(),
            "foo&calc".to_string(),
        ];
        assert!(
            guard_cmd_shim_reparse("powershell", &bad).is_err(),
            "a metachar after the script path is refused"
        );

        let clean = vec![
            "-NoProfile".to_string(),
            "-File".to_string(),
            "C:\\tools\\agent.ps1".to_string(),
            "do the thing".to_string(),
        ];
        assert!(
            guard_cmd_shim_reparse("pwsh", &clean).is_ok(),
            "clean args on the powershell form pass, and the prefix never trips"
        );
    }

    /// FIX 2a: a direct `.exe` (no cmd.exe launcher prefix) is not the shim
    /// form, so the guard is a no-op even for an argument that would be
    /// dangerous through cmd.exe -- `CreateProcess` receives it as a literal.
    /// This is also what keeps the test harness's own `sh <script>` fake agents
    /// from being rejected.
    #[test]
    fn a_non_shim_launch_is_never_guarded() {
        let args = vec!["-p".to_string(), "foo&calc".to_string()];
        assert!(guard_cmd_shim_reparse("claude.exe", &args).is_ok());
        assert!(guard_cmd_shim_reparse("/opt/homebrew/bin/claude", &args).is_ok());
        assert!(guard_cmd_shim_reparse("sh", &["/tmp/fake-agent.sh".to_string()]).is_ok());
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
        let adapter = select(Some("claude"), &[], &permissive_cfg()).expect("claude selects");
        assert_eq!(adapter.name(), "claude");
    }

    #[test]
    fn detection_reads_the_wrapped_argv() {
        let cmd = vec![
            "/opt/homebrew/bin/claude".to_string(),
            "--resume".to_string(),
        ];
        let adapter = select(None, &cmd, &permissive_cfg()).expect("detect claude");
        assert_eq!(adapter.name(), "claude");
    }

    /// The property the fallback actually promises: whatever it picks is
    /// enabled and ready. Today that adapter happens to be claude (registry
    /// order plus claude being the only one that ever passes `ready()`), so
    /// both are asserted -- the property for its own sake, the concrete name
    /// because losing it silently would be a regression worth catching too.
    #[test]
    fn empty_command_defaults_to_claude() {
        let cfg = permissive_cfg();
        let adapter = select(None, &[], &cfg).expect("default");
        assert!(
            cfg.agents.is_enabled(adapter.name()),
            "must be gate-enabled"
        );
        assert!(adapter.ready().is_ok(), "must be ready");
        assert_eq!(adapter.name(), "claude");
    }

    #[test]
    fn unknown_name_is_an_error_that_lists_the_options() {
        let err = select(Some("gemini"), &[], &permissive_cfg()).expect_err("unknown agent");
        let msg = err.to_string();
        assert!(msg.contains("gemini"), "got {msg}");
        assert!(
            msg.contains("claude"),
            "error should list known adapters: {msg}"
        );
    }

    /// Task A3: an agent named explicitly is refused, and the message names
    /// the layer that disabled it (mirrors the settings-layer wording tests
    /// in `settings.rs`; here the point is that `select` actually surfaces
    /// it, not the exact wording).
    #[test]
    fn a_disabled_agent_named_explicitly_is_refused_with_the_layer_that_disabled_it() {
        let cfg = cfg_disabling("codex");
        let err = select(Some("codex"), &[], &cfg).expect_err("codex is disabled");
        let msg = err.to_string();
        assert!(msg.contains("codex"), "got {msg}");
        assert!(msg.contains("disabled"), "got {msg}");
        assert!(
            msg.contains(".settings.toml"),
            "names the file that disabled it: {msg}"
        );
    }

    /// The detection arm must refuse, not silently fall back to claude, the
    /// same invariant `detecting_codex_argv_does_not_silently_fall_back_to_claude`
    /// pins for the unready case.
    #[test]
    fn a_disabled_agent_detected_on_the_argv_does_not_fall_back_to_the_default() {
        let cfg = cfg_disabling("codex");
        let cmd = vec!["codex".to_string(), "exec".to_string(), "go".to_string()];
        let err = select(None, &cmd, &cfg).expect_err("must not misroute to claude");
        assert!(err.to_string().contains("codex"), "got {err}");
    }

    /// `select`'s empty-command default is the first enabled-and-ready
    /// adapter; disabling claude leaves no adapter that qualifies (codex is
    /// never ready), so the aggregated error must name both, each with its
    /// own reason.
    #[test]
    fn the_default_fallback_is_refused_when_the_default_agent_is_disabled() {
        let cfg = cfg_disabling("claude");
        let err = select(None, &[], &cfg).expect_err("no adapter qualifies");
        let msg = err.to_string();
        assert!(msg.contains("claude"), "got {msg}");
        assert!(msg.contains("disabled"), "got {msg}");
        assert!(msg.contains("codex"), "got {msg}");
        assert!(msg.contains("not implemented yet"), "got {msg}");
    }

    /// The gate is checked before `ready()`: a disabled-and-unready agent
    /// (codex, always) must report the disable, not "not implemented yet".
    #[test]
    fn the_disable_is_reported_before_an_adapters_own_readiness() {
        let cfg = cfg_disabling("codex");
        let err = select(Some("codex"), &[], &cfg).expect_err("codex is disabled");
        let msg = err.to_string();
        assert!(
            !msg.contains("not implemented yet"),
            "the gate must win over ready(): {msg}"
        );
        assert!(msg.contains("disabled"), "got {msg}");
    }

    #[test]
    fn registry_exposes_both_v1_adapters() {
        let names: Vec<&str> = ADAPTERS.iter().map(|(name, _)| *name).collect();
        assert_eq!(names, vec!["claude", "codex"]);
    }

    /// The registry table is the one place a new adapter is wired in: `all`
    /// must produce exactly one instance per table entry, in table order,
    /// with matching names -- otherwise `all` and `ADAPTERS` could drift.
    #[test]
    fn adding_an_adapter_is_one_entry_in_the_constructor_table() {
        let instances = all(None);
        assert_eq!(instances.len(), ADAPTERS.len());
        for (instance, (name, _)) in instances.iter().zip(ADAPTERS.iter()) {
            assert_eq!(instance.name(), *name);
        }
    }

    #[test]
    fn the_registry_names_are_unique_and_non_empty() {
        let names: Vec<&str> = ADAPTERS.iter().map(|(name, _)| *name).collect();
        for name in &names {
            assert!(!name.is_empty(), "no adapter may have an empty name");
        }
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            names.len(),
            "duplicate adapter name in {names:?}"
        );
    }

    #[test]
    fn an_empty_command_falls_back_to_the_first_enabled_and_ready_adapter() {
        let (adapter, origin) = resolve_default(&permissive_cfg()).expect("a default exists");
        assert_eq!(adapter.name(), "claude");
        assert_eq!(origin, DefaultOrigin::FirstEnabledReady);
    }

    /// Disabling claude (enabled but the only one that is actually `ready()`)
    /// must not fall through to codex just because it is next in the table:
    /// codex is enabled by the gate but never ready, so no adapter qualifies
    /// and `resolve_default` must refuse rather than silently pick codex.
    #[test]
    fn the_fallback_skips_an_adapter_that_is_enabled_but_not_ready() {
        let cfg = cfg_disabling("claude");
        let err = resolve_default(&cfg).expect_err("codex is enabled but never ready");
        let msg = err.to_string();
        assert!(msg.contains("codex"), "got {msg}");
        assert!(msg.contains("not implemented yet"), "got {msg}");
    }

    /// Symmetric case: an adapter that would otherwise be ready (claude) is
    /// disabled by the gate, so it must be skipped even though `ready()`
    /// alone would have accepted it.
    #[test]
    fn the_fallback_skips_an_adapter_that_is_ready_but_disabled() {
        let cfg = cfg_disabling("claude");
        let err = resolve_default(&cfg).expect_err("claude is disabled");
        assert!(err.to_string().contains("claude"), "got {err}");
    }

    /// Disabling both known adapters leaves nothing to fall back to; the
    /// error must aggregate one line per adapter naming its own reason,
    /// reusing the gate's refusal text and each adapter's own `ready()` text
    /// rather than inventing new wording.
    #[test]
    fn when_no_adapter_is_both_enabled_and_ready_the_error_names_each_one_and_why() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/.settings.toml"),
            "[agents.claude]\nenabled = false\n[agents.codex]\nenabled = false\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let cfg = CtxConfig {
            agents: crate::settings::AgentGate::load(repo.path(), &|k| empty.get(k).cloned())
                .expect("load"),
            ..CtxConfig::default()
        };

        let err = resolve_default(&cfg).expect_err("both disabled");
        let msg = err.to_string();
        assert!(msg.contains("claude"), "must name claude: {msg}");
        assert!(msg.contains("codex"), "must name codex: {msg}");
        assert!(
            msg.contains("disabled"),
            "must say why claude lost out: {msg}"
        );
        assert!(
            msg.contains("not implemented yet") || msg.contains("disabled"),
            "must say why codex lost out: {msg}"
        );
    }

    /// The fallback is only reached when neither an explicit `--agent` nor
    /// detection named an adapter; either one must bypass it entirely, even
    /// when the fallback itself would have refused.
    #[test]
    fn an_explicit_or_detected_agent_still_bypasses_the_fallback_entirely() {
        let cfg = cfg_disabling("claude");

        // Explicit name: codex is still enabled by this gate, so it is
        // selected directly (and fails on its own `ready()`, not the gate).
        let err = select(Some("codex"), &[], &cfg).expect_err("codex is never ready");
        assert!(
            err.to_string().contains("not implemented yet"),
            "must reach codex's own ready() error, not the fallback aggregate: {err}"
        );

        // Detection: an argv that names claude explicitly is refused for
        // being disabled, not silently redirected into the fallback.
        let cmd = vec!["/usr/bin/claude".to_string()];
        let err = select(None, &cmd, &cfg).expect_err("claude is disabled");
        assert!(err.to_string().contains("disabled"), "got {err}");
    }

    #[test]
    fn resolve_default_reports_which_rule_chose_the_adapter() {
        let mut cfg = permissive_cfg();
        cfg.agent = Some("claude".to_string());
        let (adapter, origin) = resolve_default(&cfg).expect("claude is configured");
        assert_eq!(adapter.name(), "claude");
        assert_eq!(origin, DefaultOrigin::Configured);

        let (adapter, origin) = resolve_default(&permissive_cfg()).expect("fallback picks one");
        assert_eq!(adapter.name(), "claude");
        assert_eq!(origin, DefaultOrigin::FirstEnabledReady);
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
