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

/// Tells a spawned **orchestrator** session which model its own seat runs on,
/// so the `zirv ctx hook pretool` guard inside it can refuse a subagent
/// dispatch that would silently inherit that seat (see `hook::pretool_
/// decision`). Prompt-level guidance was tried first and failed: a fork
/// fan-out inherited the seat model and spent roughly half a five-hour usage
/// window in one run, so the gate is deterministic rather than advisory.
///
/// Set only by the two orchestrator launch paths (`wrap::run_with` for an
/// `Orchestrator` role, and the dashboard's first pane), never by
/// `exec`/`loop`/worker panes -- and listed in `sessions::SUPERVISION_ENV` so
/// a worker spawned from inside an orchestrator session has it scrubbed
/// rather than inherited. A seat is a property of the session that owns it,
/// exactly like `SESSION_ENV`/`SOCKET_ENV`.
pub const SEAT_MODEL_ENV: &str = "ZIRV_CTX_SEAT_MODEL";

/// How one argv token spells a model-selecting flag -- `--model`/`-m`, in
/// separated (bare, value is the next token), joined-by-`=`
/// (`--model=x`/`-m=x`), or (short form only) attached (`-mx`) form. Shared
/// by `last_model_flag` below (which needs the value) and `agent::
/// flags_pin_model` (which only needs to know a token pins something at
/// all, never the value), so the two can never drift on what counts as a
/// model flag between them.
///
/// `Separated` deliberately carries no value itself: `last_model_flag` reads
/// the following token from `flags` when it wants one, and `flags_pin_model`
/// never needs to at all -- the flag's own presence is enough to say
/// "already pinned", matching the pre-existing bare `--model`/`-m` rule.
pub(crate) enum ModelFlagForm<'a> {
    Separated,
    Joined(&'a str),
}

/// Classifies `arg`, or `None` when it is not a model flag at all.
///
/// The attached short form (`-mopus`) is recognised only when `arg` is not
/// itself a `--`-prefixed long flag -- `--model-foo` starts with `-m` too,
/// once its own leading `-` is peeled, and must not match -- and carries at
/// least one character of value (`arg.len() > 2`, so a bare `-m` is
/// `Separated`, not an attached value of `""`).
pub(crate) fn classify_model_flag(arg: &str) -> Option<ModelFlagForm<'_>> {
    if arg == "--model" || arg == "-m" {
        return Some(ModelFlagForm::Separated);
    }
    if let Some(value) = arg.strip_prefix("--model=") {
        return Some(ModelFlagForm::Joined(value));
    }
    if let Some(value) = arg.strip_prefix("-m=") {
        return Some(ModelFlagForm::Joined(value));
    }
    if !arg.starts_with("--") && arg.starts_with("-m") && arg.len() > 2 {
        return Some(ModelFlagForm::Joined(&arg[2..]));
    }
    None
}

/// The last model-flag occurrence in `flags`, in any form `classify_model_
/// flag` recognises -- CLI last-wins semantics, the same rule a real argv
/// parser applies when a flag is repeated, honored across mixed spellings
/// (a later `-mhaiku` still overrides an earlier `--model opus`). `None`
/// when `flags` names no model at all, or when a trailing bare `--model`/
/// `-m` has nothing after it to be its value -- a dangling flag with no
/// value contributes nothing, it does not clear an earlier match.
///
/// Recognises codex's `-m` short alias (all three forms) as well as
/// claude's long `--model`, unlike the version of this function before FIX
/// A: this feeds `seat_model_env`, and a codex-adapter launch built with a
/// bare `-m <expensive>`/`-m=<expensive>`/`-m<expensive>` passthrough used
/// to export no seat env at all, leaving the pretool guard blind to it.
fn last_model_flag(flags: &[String]) -> Option<&str> {
    let mut found = None;
    let mut i = 0;
    while i < flags.len() {
        match classify_model_flag(&flags[i]) {
            Some(ModelFlagForm::Separated) => {
                if let Some(value) = flags.get(i + 1) {
                    found = Some(value.as_str());
                }
                i += 2;
                continue;
            }
            Some(ModelFlagForm::Joined(value)) => {
                found = Some(value);
            }
            None => {}
        }
        i += 1;
    }
    found
}

/// The model `flags` pins when it pins **nothing else** -- every token in it
/// is part of one model flag, in any form `classify_model_flag` recognises.
/// `None` when `flags` is empty, names any other flag, leaves a bare
/// `--model`/`-m` dangling with no value, or names a value that is itself
/// flag-shaped (a leading `-` is never a model name, and this value becomes an
/// argv token).
///
/// The one caller is `agent::try_join_dashboard`: a dashboard pane cannot
/// honour arbitrary trailing flags (they belong to `exec::run_with`), so a
/// request carrying any declines the pane and runs headless. A model pin is
/// the exception the harness layer now teaches orchestrators to write on every
/// delegation, and it is the one flag a pane *can* honour, since the pane
/// builds its own argv from a resolved worker model anyway -- so recognising
/// exactly that shape is what keeps "pick the cheapest model" from silently
/// costing every dashboard delegation its pane.
pub(crate) fn model_only_flags(flags: &[String]) -> Option<&str> {
    let mut found = None;
    let mut i = 0;
    while i < flags.len() {
        match classify_model_flag(&flags[i]) {
            Some(ModelFlagForm::Separated) => {
                found = Some(flags.get(i + 1)?.as_str());
                i += 2;
            }
            Some(ModelFlagForm::Joined(value)) => {
                found = Some(value);
                i += 1;
            }
            None => return None,
        }
    }
    found
        .map(str::trim)
        .filter(|model| !model.is_empty() && !model.starts_with('-'))
}

/// The `SEAT_MODEL_ENV` pair a launch exports, or nothing. Pure, so which
/// launches disclose a seat is testable without a pty.
///
/// Only an `Orchestrator` launch with a non-blank resolved model discloses
/// one: a `Worker` is not a seat that dispatches subagents, and with no
/// resolved model the harness picks its own default, which zirv cannot name
/// and therefore must not claim to.
///
/// The resolved model prefers an operator-passed `--model`/`--model=` in
/// `flags` (the last occurrence, CLI last-wins) over `cfg_model`
/// (`cfg.chat.model`): `flags` is the argv the launch actually uses, built by
/// `extra_with_model` from `cfg_model` and then the operator's own trailing
/// flags appended after it, so an operator passthrough like `zirv chat --
/// --model fable` with no `chat.model` configured must still disclose the
/// seat it actually launches on, and a configured `chat.model` that an
/// operator's own passthrough then overrides must disclose the flag's value,
/// not the configured one -- both directions the guard was blind to when
/// this only ever read `cfg.chat.model`.
pub fn seat_model_env(
    role: super::prompt::PromptRole,
    flags: &[String],
    cfg_model: Option<&str>,
) -> Vec<(String, String)> {
    if role != super::prompt::PromptRole::Orchestrator {
        return Vec::new();
    }
    let resolved = last_model_flag(flags).or(cfg_model);
    match resolved.map(str::trim).filter(|m| !m.is_empty()) {
        Some(model) => vec![(SEAT_MODEL_ENV.to_string(), model.to_string())],
        None => Vec::new(),
    }
}

/// `Debug` is a supertrait so `Box<dyn AgentAdapter>` can appear in
/// `Result::expect_err` (the registry tests assert on the unknown-adapter
/// error path); every adapter already derives it.
pub trait AgentAdapter: std::fmt::Debug {
    fn name(&self) -> &'static str;

    /// The program this adapter actually spawns -- `agent_bin`'s override, or
    /// this adapter's own default binary name, whichever `ClaudeAdapter::new`/
    /// `CodexAdapter::new` resolved to `program` at construction. Distinct
    /// from `name()`: `name` is the fixed registry key (`"claude"`,
    /// `"codex"`), while this can be any override an operator's `agent_bin`
    /// or `--agent-bin` named. Exists so a caller with only a `&dyn
    /// AgentAdapter` -- `harness_prompt_lines`'s presence check in particular
    /// -- can ask what binary `ready()` actually resolved, without the
    /// module-private `program` field each adapter otherwise keeps to itself.
    fn program(&self) -> &str;

    /// The ACCOUNT/vendor whose rate limits this agent spends, as a stable
    /// lowercase slug (`[a-z0-9-]`): `"anthropic"` for claude, `"openai"`
    /// for codex.
    ///
    /// Deliberately *not* the binary or the adapter's own `name`. Usage
    /// windows are a property of the account being billed, and two harnesses
    /// can sit on one account -- a second Anthropic-backed harness would
    /// report `"anthropic"` here and share claude's windows, which is the
    /// truth about the limit even though it is a different program. It is
    /// what `StateDir::usage_for` names a usage file after, so a change to an
    /// existing adapter's slug orphans that adapter's stored readings.
    fn provider(&self) -> &'static str;

    /// `Err` when the adapter exists but is not safe to use yet, so callers
    /// fail loudly instead of scoring garbage.
    fn ready(&self) -> CtxResult<()>;

    fn detect(&self, command: &[String]) -> bool;

    fn headless_cmd(&self, prompt: &str, session: &SessionId, extra: &[String]) -> Command;
    fn interactive_cmd(&self, initial_prompt: Option<&str>, extra: &[String]) -> Command;
    /// Builds the judgment/distiller model child's command. `model` is empty
    /// when neither the operator's own config (`handoff.model`/`optimize.
    /// model`) nor this adapter's own [`default_distiller_model`](Self::
    /// default_distiller_model) named one, which an adapter with no sane
    /// default of its own (codex) must read as "omit the model flag
    /// entirely" rather than pass an empty value to its own CLI -- see
    /// `resolve_distiller_model` in `handoff.rs`, which is what every caller
    /// uses to turn `Option<&str>` config into this parameter.
    fn distiller_cmd(&self, model: &str) -> Command;

    /// The model name to use for the judgment/distiller child when the
    /// operator has not named one explicitly (`handoff.model`/`optimize.
    /// model` both empty/unset). `None` -- the default, and codex's own
    /// answer -- means this adapter has no verified cheap-model default of
    /// its own to guess, so `resolve_distiller_model` passes an empty model
    /// through, and this adapter's own `distiller_cmd` must read that as
    /// "omit the model flag" so the agent's own configuration (e.g. codex's
    /// `~/.codex/config.toml`) picks a model instead of zirv guessing a name
    /// that may not exist on the operator's account. Claude's own default is
    /// a real, verified value ("haiku") rather than the trait default,
    /// because a hardcoded model name is specific to one agent's lineup and
    /// must never leak into another adapter's guess.
    fn default_distiller_model(&self) -> Option<&'static str> {
        None
    }

    /// This adapter's own verified model ladder, one tier below `seat` --
    /// `seat` is the orchestrator seat's own model (`cfg.chat.model`), or
    /// `None`/unrecognised when unset, which this must read as "assume the
    /// top tier" rather than guess low. Used only when the operator has not
    /// set `review.<agent>` explicitly (see `resolve_review_model` below,
    /// the one place this and the operator override are combined into the
    /// harness-roster line an Orchestrator session sees).
    ///
    /// `""` -- the default, and not meant to be a real model id -- means
    /// this adapter has no verified ladder of its own, the same "nothing
    /// verified to guess" answer `default_distiller_model`'s `None` gives.
    /// Both registered adapters (claude, codex) override this with real,
    /// verified tier names; `resolve_review_model` is the only caller, and
    /// treats a `""` result the same way it treats any other resolved
    /// string (harmless here because every reachable adapter overrides it).
    fn review_model_below(&self, seat: Option<&str>) -> &'static str {
        let _ = seat;
        ""
    }

    /// This adapter's own hard-coded model for a delegated headless worker
    /// (`zirv ctx agent`, and the dashboard's own spawn-request pane
    /// variant) when the operator has not set `worker.<name>` explicitly.
    /// Used only by `resolve_worker_model` in this module, the one place
    /// this and the operator override are combined into the argv a
    /// delegation spawn actually launches with.
    ///
    /// `None` -- the default, and codex's own answer -- means this adapter
    /// has no verified cheap-enough default of its own to guess, the same
    /// "nothing verified to guess" answer `default_distiller_model` gives:
    /// the launch omits `--model` entirely and the agent's own
    /// configuration (codex's `~/.codex/config.toml`) picks instead.
    /// Claude's own default is `"sonnet"`, a real hard-coded value specific
    /// to claude's lineup: a delegated worker used to silently inherit
    /// whatever the operator's own interactive default happened to be
    /// (often a far pricier model than the work actually needs), which is
    /// exactly the spend this default exists to stop.
    fn default_worker_model(&self) -> Option<&'static str> {
        None
    }

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

    /// This agent's own layer for a delegated **Worker** session -- the
    /// role-scoped counterpart to [`base_system_prompt`](Self::
    /// base_system_prompt), which is spliced in for an **Orchestrator**
    /// session only. Exactly one of the two ever reaches a launch, so a
    /// worker never receives the orchestrator layer's own delegate-and-review
    /// coaching: telling a session that was itself delegated to that its job
    /// is to delegate is what invites the recursion `zirv agent`'s workers
    /// must not do.
    ///
    /// `None` (the default) means this agent contributes no worker-specific
    /// layer of its own, the same "no verified mechanism" shape every other
    /// optional layer on this trait uses.
    fn worker_system_prompt(&self) -> Option<&'static str> {
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

    /// What this harness can actually deliver for one of zirv's own policy
    /// capabilities at one requested stance -- the per-adapter half of
    /// `policy::evaluate`, which is the only caller.
    ///
    /// Answer with a `CapabilityDescriptor` naming the **verified per-run
    /// mechanism** this adapter would pin on the launch, or with
    /// `CapabilityDescriptor::advisory_only()` -- the default -- when there is
    /// none. That default is the same "no verified mechanism" shape every
    /// other optional method on this trait uses, and here it carries the
    /// load-bearing honesty rule: prompt text asking a session to respect a
    /// stance is advisory context, never enforcement, so a harness with only
    /// that to offer must report `Support::Unsupported` rather than claim a
    /// guarantee zirv cannot keep.
    ///
    /// `stance` is never `Stance::Allow`: `policy::evaluate` answers that case
    /// itself (zirv is imposing nothing, so there is no mechanism to name), so
    /// an implementation may leave it to a catch-all arm.
    ///
    /// `allow(dead_code)` for the same reason `model_args` below carries one:
    /// the only caller, `policy::evaluate`, has no production caller of its own
    /// until issues #44/#46 wire it in. Both adapters override it already, and
    /// `policy.rs`'s own tests exercise every arm.
    #[allow(dead_code)]
    fn policy_support(
        &self,
        capability: super::policy::Capability,
        stance: super::policy::Stance,
    ) -> super::policy::CapabilityDescriptor {
        let _ = (capability, stance);
        super::policy::CapabilityDescriptor::advisory_only()
    }

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

/// I: the flags an adapter-built command carries, with any launcher prefix
/// dropped. On a Windows machine where the adapter's own program resolves to
/// a real npm `.cmd` shim, every command an adapter builds starts `cmd.exe /c
/// <shim>`, and those tokens are not what a test about agent flags is
/// asserting on. `program` is the adapter's own `program` field (each
/// adapter's `base()` resolves exactly this string) -- a shared helper takes
/// it as a plain `&str` rather than `&dyn AgentAdapter` because nothing else
/// about the adapter is needed, and because `program` is private to each
/// adapter's own module, so only that module's own tests can pass it in
/// anyway. Was duplicated byte-for-byte in `claude.rs` and `codex.rs`'s own
/// test modules before this; both now call this one copy.
#[cfg(test)]
pub(crate) fn built_args(program: &str, cmd: &std::process::Command) -> Vec<String> {
    let launcher = resolve_program(program)
        .map(|resolved| resolved.prefix.len())
        .unwrap_or(0);
    cmd.get_args()
        .skip(launcher)
        .map(|a| a.to_string_lossy().to_string())
        .collect()
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

/// Whether spawning the argv `launch` puts its downstream tokens through a
/// Windows launcher that reparses them -- either the `cmd.exe /c <shim>` form
/// (an npm-installed `.cmd`) or the `powershell -File <script>` form (a
/// `.ps1`). Unlike [`launches_through_cmd_shim`], which is given only a bare
/// program name to re-resolve, this handles an argv that is **already
/// resolved** to a launcher: `chat::build_launch`/`ClaudeAdapter::base` hand
/// `wrap`/`dash_orchestrator_pane` an argv whose head is literally `cmd.exe`
/// (or `powershell`), so re-resolving that head finds a plain `.exe` and would
/// wrongly report "not a shim", leaving the forced-file-form defence inert on
/// the interactive path. Recognising the resolved launcher structure directly
/// (via [`reparse_launcher_prefix`]) is what keeps that defence engaged.
///
/// Falls back to resolving the head program for an argv that has *not* been
/// resolved yet (a raw `wrap` command such as `["claude", "--resume"]`), so
/// both call shapes reach the same verdict. Always `false` off Windows.
pub fn launch_reparses_through_shim(launch: &[String]) -> bool {
    #[cfg(windows)]
    {
        let Some((program, rest)) = launch.split_first() else {
            return false;
        };
        // An already-resolved `cmd.exe /c <shim>` or `powershell -File <script>`
        // argv: the launcher reparses everything past its own prefix.
        if reparse_launcher_prefix(program, rest).is_some() {
            return true;
        }
        // Otherwise the head is an ordinary program name that `resolve_program`
        // may still route through a launcher.
        launches_through_cmd_shim(program)
    }
    #[cfg(not(windows))]
    {
        let _ = launch;
        false
    }
}

/// `std::process::Command` -> the flat `program, arg, arg, ...` form
/// [`launch_reparses_through_shim`] wants. Shared here rather than
/// duplicated per call site (`exec.rs`, `run_loop.rs`; `dash/mod.rs` keeps
/// its own private copy, established first and not worth churning): a probe
/// command built purely to answer "what launcher shape would this be" (no
/// real prompt text on it yet) is flattened the same way regardless of
/// which module is asking.
pub fn flatten_command(command: std::process::Command) -> Vec<String> {
    let mut argv = vec![command.get_program().to_string_lossy().to_string()];
    argv.extend(command.get_args().map(|a| a.to_string_lossy().to_string()));
    argv
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

/// Whether `program`'s binary genuinely exists on disk -- either at an
/// explicit path, or somewhere on `PATH` (`PATHEXT`-aware on Windows).
///
/// This is deliberately a *stronger* claim than `resolve_program`/`ready()`
/// make: `resolve_program` is fail-open by design for a name it cannot find
/// (a program that resolves to nothing is spawned exactly as written, so a
/// genuinely missing binary fails with the OS's own "not found" rather than a
/// zirv-invented error), and several call sites rely on exactly that
/// fail-open behavior (`agent_bin` naming a not-yet-real path still has to
/// fall through to whichever adapter it actually matches by name, not error
/// out early). `harness_prompt_lines` is the one caller that turns "ready"
/// into a concrete invitation (`zirv agent <name> "<prompt>"`) an orchestrator
/// may act on immediately, so it alone needs this stronger check layered on
/// top of -- never in place of -- `ready()`.
pub fn program_is_present(program: &str) -> bool {
    if program.is_empty() {
        return false;
    }
    #[cfg(windows)]
    {
        resolve_on_path(program).is_some()
    }
    #[cfg(not(windows))]
    {
        if program.contains('/') {
            return Path::new(program).is_file();
        }
        std::env::var_os("PATH")
            .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
            .unwrap_or(false)
    }
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

/// Low 5: the account a usage readout should report for `name`, without
/// needing that adapter to be enabled or ready -- adapter name -> provider
/// is a static fact through the registry (`ctor(None).provider()` never
/// touches the filesystem, a gate, or `ready()`), so it stays answerable
/// even when `adapters::select(name, ...)` itself would refuse. `zirv ctx
/// usage`'s no-subcommand branch and `zirv ctx status`'s usage-windows line
/// used to fall back to `window::LEGACY_USAGE_PROVIDER` on *any* `select`
/// refusal, which silently showed Anthropic percentages for a repo
/// configured for a disabled codex rather than "openai: no usage source" --
/// a guess dressed up as a fact. Falls back to `LEGACY_USAGE_PROVIDER` only
/// when `name` is `None` or matches no registered adapter at all (an unknown
/// or absent configuration, where there truly is nothing more specific to
/// say than the legacy default).
pub fn provider_for_agent_name(name: Option<&str>) -> &'static str {
    name.and_then(|n| ADAPTERS.iter().find(|(adapter_name, _)| *adapter_name == n))
        .map(|(_, ctor)| ctor(None).provider())
        .unwrap_or(super::window::LEGACY_USAGE_PROVIDER)
}

/// Final wave item 4: `provider_for_agent_name(cfg.agent)` alone gets an
/// *unset* `agent` wrong whenever `resolve_default` would not have landed on
/// the legacy provider -- an operator-disabled claude (home `.settings.toml`
/// or `ZIRV_AGENT_CLAUDE_ENABLED=false`, not a repo one) with codex enabled
/// and ready falls back straight to `LEGACY_USAGE_PROVIDER` ("anthropic")
/// with no `agent` name to derive anything more specific from, even though
/// `resolve_default`'s own fallback loop would correctly skip claude and
/// land on codex. Tried first here for exactly that reason: `resolve_
/// default` is the actual selection logic (gates, `ready()`, the repo-
/// narrowing guard), so when it succeeds its answer is authoritative.
/// `provider_for_agent_name` is the fallback for when it does not -- an
/// explicitly configured, repo-disabled agent (`resolve_default`'s
/// configured arm hard-refuses there) still needs a provider, and only
/// `provider_for_agent_name` can name one without requiring readiness.
pub fn provider_for_usage_readout(cfg: &CtxConfig) -> &'static str {
    resolve_default(cfg)
        .map(|(adapter, _origin)| adapter.provider())
        .unwrap_or_else(|_| provider_for_agent_name(cfg.agent.as_deref()))
}

/// `cfg.agent_bin` is one global override applied to *whichever* adapter is
/// selected (every `ctor(bin)` call in this module reuses the same value
/// regardless of the adapter name) -- there is no per-adapter binary
/// override. That is fine for a stub path, a wrapper script (`sh
/// /path/fake-codex-agent.sh`), or a differently located install of the
/// *same* agent, but a value whose own program basename names a *different*
/// registered adapter (`agent_bin = "/usr/local/bin/claude"` while `codex`
/// is what gets selected, most plausibly stale config left over from
/// switching agents) would launch that other agent's real binary dressed up
/// in the selected adapter's own argv shape -- codex's `exec <prompt>`
/// positional form handed to the real claude CLI, wrong account, wrong
/// safety model, and no error anywhere naming what happened. Checked by
/// basename only (extension stripped, case-insensitive), not full-path
/// identity: an operator who genuinely renamed a binary to something that
/// happens to collide with another adapter's own name gets the same
/// refusal, which is the conservative, name-the-problem-and-stop failure
/// mode this guard exists for.
///
/// Returns the *other* adapter's name when `bin`'s basename collides with
/// one that is not `selected`; `None` when `bin` is unset, names no
/// registered adapter at all (a stub/wrapper path, the common test and
/// wrapper-script shape), or names `selected` itself.
fn agent_bin_names_a_different_adapter(bin: Option<&str>, selected: &str) -> Option<&'static str> {
    let bin = bin?;
    let program = bin.split_whitespace().next()?;
    let stem = Path::new(program).file_stem()?.to_str()?;
    ADAPTERS.iter().find_map(|(name, _)| {
        (!name.eq_ignore_ascii_case(selected) && stem.eq_ignore_ascii_case(name)).then_some(*name)
    })
}

/// The clear, name-both-adapters refusal `agent_bin_names_a_different_
/// adapter` backs, shared by every `select`/`resolve_default` arm that is
/// about to return `selected` as the resolved adapter.
fn refuse_if_agent_bin_names_another_adapter(bin: Option<&str>, selected: &str) -> CtxResult<()> {
    if let Some(other) = agent_bin_names_a_different_adapter(bin, selected) {
        return Err(format!(
            "agent_bin '{}' names '{other}', not the selected agent '{selected}' -- refusing to \
             launch '{other}'s binary as if it were '{selected}'. Point agent_bin at a '{selected}' \
             install, or select '{other}' instead.",
            bin.unwrap_or_default()
        )
        .into());
    }
    Ok(())
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

/// One line per registered adapter, describing whether an Orchestrator
/// session may delegate to it right now via `zirv agent <name> "<prompt>"`.
/// Rendered by a caller that already has `cfg` in hand (`wrap`, `chat`'s
/// dashboard orchestrator pane path), never by a Worker call site: the result
/// is folded into the composed prompt as the harness roster
/// (`prompt::PromptSource::Harnesses`), and a worker must not learn what it
/// could delegate to.
///
/// `current_adapter` is the resolved adapter running *this* session -- its
/// line is marked "(this session's harness)" instead of the `zirv agent`
/// invitation, since `HARNESS_PROMPT` frames delegation as going to *other*
/// harnesses; a session cannot review-round itself.
///
/// Walks `ADAPTERS` in registry order, same as `readiness_note`, but reports
/// per-adapter gate state too (`readiness_note` only ever speaks about
/// installed-but-not-ready or degraded adapters, never a disabled one): a
/// disabled adapter gets its own line naming where the disable came from
/// (`AgentState::location`) rather than silently vanishing from the roster,
/// so an operator reading the prompt can tell "not offered because disabled
/// in .zirv/.settings.toml" from "not offered because not installed".
///
/// `ready()` alone is fail-open for a binary that simply is not there (see
/// [`program_is_present`]'s own doc comment), so a `ready()`-Ok adapter is
/// checked again against the real filesystem before its line may claim
/// "ready" and hand out the `zirv agent` invitation -- otherwise it reads as
/// "not installed" instead, exactly like a genuinely unready one.
///
/// `cfg.agent_bin` is one global override (see `agent_bin_names_a_different_
/// adapter`'s own doc comment), so it is never handed to an adapter whose own
/// basename it does not name: every `ctor(bin)` call below used to reuse the
/// same `bin` for every adapter in the registry, which put a real `claude`
/// binary's presence verdict onto codex's line (and vice versa) whenever an
/// operator's `agent_bin` named one specific agent -- either wrongly
/// advertising `zirv agent codex` on the strength of claude's binary, or, for
/// a not-yet-real wrapper path, wrongly marking every *other* adapter "not
/// installed" too. `agent_bin_names_a_different_adapter` returning `Some`
/// means `bin`'s basename names a *different* registered adapter than the one
/// about to be built, so that adapter is built with `None` instead and its
/// presence is judged from its own default program name, exactly as if no
/// override were configured at all.
pub fn harness_prompt_lines(cfg: &CtxConfig, current_adapter: &str) -> Vec<String> {
    let bin = cfg.agent_bin.as_deref();
    let mut lines: Vec<String> = ADAPTERS
        .iter()
        .map(|(name, ctor)| {
            let name: &str = name;
            let is_self = name == current_adapter;
            let (enabled, location) = cfg
                .agents
                .states()
                .find(|(n, _)| *n == name)
                .map(|(_, s)| (s.enabled, s.location()))
                .unwrap_or((true, "default".to_string()));
            if !enabled {
                return format!("- {name}: disabled ({location})");
            }

            let adapter = if agent_bin_names_a_different_adapter(bin, name).is_some() {
                ctor(None)
            } else {
                ctor(bin)
            };
            match adapter.ready() {
                Ok(()) => {
                    let program = adapter.program();
                    if !program_is_present(program) {
                        return format!("- {name}: not installed (no '{program}' found)");
                    }
                    let missing = missing_capability_labels(adapter.capabilities());
                    let degraded = if missing.is_empty() {
                        String::new()
                    } else {
                        format!(" (degraded: no {})", join_with_or(&missing))
                    };
                    // Repo `.settings.toml` (or the operator, or the
                    // environment) may mark a harness capacity-limited; the
                    // roster line carries that forward so an orchestrator
                    // routes only small, bounded briefs its way -- both for
                    // reviews and for `zirv agent` delegations (see
                    // `HARNESS_PROMPT`'s final paragraph).
                    let capacity_note = if cfg.agents.is_capacity_small(name) {
                        " -- small tasks only"
                    } else {
                        ""
                    };
                    if is_self {
                        format!(
                            "- {name}: enabled, ready{capacity_note} (this session's harness){degraded}"
                        )
                    } else {
                        format!(
                            "- {name}: enabled, ready{capacity_note} -- initiate with `zirv agent {name} \"<prompt>\"`{degraded}"
                        )
                    }
                }
                Err(err) => {
                    let reason = err.to_string();
                    let short = reason.lines().next().unwrap_or(&reason);
                    format!("- {name}: installed? not ready ({short})")
                }
            }
        })
        .collect();
    if let Some(review_line) = review_roster_line(cfg) {
        lines.push(review_line);
    }
    lines
}

/// The resolved review-model choice for one enabled harness: either the
/// operator's own `cfg.review.<agent>` value, or `adapter`'s own
/// `AgentAdapter::review_model_below` ladder default computed from the
/// orchestrator seat (`cfg.chat.model`, or the top tier when unset). This is
/// the one place both halves are combined -- `review_roster_line` below is
/// its only caller.
struct ReviewModelChoice {
    model: String,
    configured: bool,
}

fn resolve_review_model(
    cfg: &CtxConfig,
    name: &str,
    adapter: &dyn AgentAdapter,
) -> ReviewModelChoice {
    let configured = match name {
        "claude" => cfg.review.claude.as_deref(),
        "codex" => cfg.review.codex.as_deref(),
        _ => None,
    };
    if let Some(model) = configured {
        return ReviewModelChoice {
            model: model.to_string(),
            configured: true,
        };
    }
    ReviewModelChoice {
        model: adapter
            .review_model_below(cfg.chat.model.as_deref())
            .to_string(),
        configured: false,
    }
}

/// The resolved `worker.<name>` model for a delegated headless worker: the
/// operator's own `cfg.worker.<name>` value if set, else `adapter`'s own
/// `AgentAdapter::default_worker_model`. `None` means neither exists, so a
/// delegation spawn adds no `--model` flag at all and the agent's own
/// configuration picks (codex, with no `worker.codex` set). Unlike
/// `resolve_review_model` above, there is no ladder to fall back to: a
/// delegated worker has no orchestrator seat of its own to be "one tier
/// below", so the adapter-owned default is a fixed model name, not a
/// function of `cfg.chat.model`.
fn resolve_worker_model<'a>(
    cfg: &'a CtxConfig,
    name: &str,
    adapter: &'a dyn AgentAdapter,
) -> Option<&'a str> {
    let configured = match name {
        "claude" => cfg.worker.claude.as_deref(),
        "codex" => cfg.worker.codex.as_deref(),
        _ => None,
    };
    configured.or_else(|| adapter.default_worker_model())
}

/// Argv tokens (`AgentAdapter::model_args`) for the resolved worker model, or
/// empty when `resolve_worker_model` resolves nothing. The one place a
/// delegation spawn (`zirv ctx agent`'s own headless path in `agent.rs`, and
/// the dashboard's own spawn-request pane variant in `dash/mod.rs`) turns the
/// resolved model into a flag; neither caller applies this when the
/// operator's own trailing flags already pin a model explicitly (see each
/// caller's own doc comment for why that check lives there and not here).
pub fn worker_model_args(cfg: &CtxConfig, name: &str, adapter: &dyn AgentAdapter) -> Vec<String> {
    match resolve_worker_model(cfg, name, adapter) {
        Some(model) => adapter.model_args(model),
        None => Vec::new(),
    }
}

/// The trailing "- code review: ..." line `harness_prompt_lines` appends
/// after its per-harness lines: names every *enabled* harness's resolved
/// review model (an operator override or the ladder default, each marked as
/// such) and states the rule that outranks any other model-routing guidance
/// a session's own base prompt carries (see `ORCHESTRATOR_PROMPT`'s
/// model-routing bullet in claude.rs, which now points back at this line).
/// Returns `None` when no harness is enabled at all -- absence, not a line
/// naming zero harnesses.
///
/// A disabled harness's entry is simply absent, the same "absence, not
/// silence" rule its own per-harness line above follows -- readiness is
/// deliberately not checked here (unlike the per-harness lines): the rule
/// this line states applies to a harness the moment it is enabled, whether
/// or not its binary happens to be on disk on this machine right now.
///
/// The rendered sentence must never be false for any entry. Two cases would
/// otherwise make it false: a harness whose ladder default is already at
/// the floor tier (seat "haiku" resolves claude's own default to "haiku"
/// too -- neither "one tier below the seat" nor "never on the seat's own
/// model" holds), and an operator who explicitly configures `review.<agent>`
/// equal to the seat (allowed -- the operator's choice wins -- but then
/// "never on the seat's own model" is false of that entry). Both are the
/// same underlying condition -- the resolved model's text equals the seat's
/// text, case-insensitively -- so both are detected by one `equals_seat`
/// check per entry, regardless of whether the model came from the ladder
/// default or an operator override. (Deliberately a plain text comparison,
/// not a second call into the ladder: re-running `review_model_below` on
/// the *resolved* model would also self-map at the floor tier for a seat
/// one rung *above* the floor -- e.g. seat "sonnet" resolves to "haiku",
/// and "haiku" maps to itself too -- which would wrongly flag a perfectly
/// true "one tier below the seat" note as a floor case.) When any entry's
/// `equals_seat` is true, the trailing clause softens from the strict
/// "never on an orchestrator seat's own model" to the weaker but always-true
/// "never on a model above the named one" (the named model is by
/// construction never ranked above the seat, so this holds in every case).
fn review_roster_line(cfg: &CtxConfig) -> Option<String> {
    let bin = cfg.agent_bin.as_deref();
    let seat = cfg.chat.model.as_deref();
    let mut any_equals_seat = false;
    let entries: Vec<String> = ADAPTERS
        .iter()
        .filter(|(name, _)| cfg.agents.is_enabled(name))
        .map(|(name, ctor)| {
            let adapter = if agent_bin_names_a_different_adapter(bin, name).is_some() {
                ctor(None)
            } else {
                ctor(bin)
            };
            let choice = resolve_review_model(cfg, name, adapter.as_ref());
            let equals_seat = seat.is_some_and(|s| s.eq_ignore_ascii_case(&choice.model));
            if equals_seat {
                any_equals_seat = true;
            }
            let note = if choice.configured {
                "configured".to_string()
            } else if equals_seat {
                "floor tier: the seat is already at the bottom rung".to_string()
            } else {
                "default: one tier below the seat".to_string()
            };
            format!("{name} -> \"{}\" ({note})", choice.model)
        })
        .collect();
    if entries.is_empty() {
        return None;
    }
    let never_clause = if any_equals_seat {
        "never on a model above the named one"
    } else {
        "never on an orchestrator seat's own model"
    };
    Some(format!(
        "- code review: {} -- run every code review on the named model, {never_clause}. This \
         outranks any other model-routing guidance.",
        entries.join(", ")
    ))
}

/// A `Capabilities` predicate paired with its user-facing label -- factored
/// out purely to keep `CAPABILITY_LABELS`'s type simple enough for clippy's
/// `type_complexity` lint.
type CapabilityLabel = (fn(Capabilities) -> bool, &'static str);

/// The user-facing label for each `Capabilities` flag this disclosure cares
/// about, in a fixed reporting order. `marker_signal` is deliberately not
/// included: it is a sub-feature of `events` (no event parsing means no
/// marker detection either), so listing both would say the same thing twice.
const CAPABILITY_LABELS: &[CapabilityLabel] = &[
    (|c| c.events, "rot score"),
    (|c| c.token_usage, "usage"),
    (|c| c.turn_signal, "turn signal"),
    (|c| c.system_prompt, "injected prompt"),
];

/// Which of [`CAPABILITY_LABELS`] this adapter's `capabilities()` reports as
/// missing, in the same fixed order.
fn missing_capability_labels(caps: Capabilities) -> Vec<&'static str> {
    CAPABILITY_LABELS
        .iter()
        .filter(|(has, _)| !has(caps))
        .map(|(_, label)| *label)
        .collect()
}

/// `["a", "b", "c"]` -> `"a, b, or c"`; `["a", "b"]` -> `"a or b"`; `["a"]` ->
/// `"a"`. Plain English list join for a short, human-readable sentence.
fn join_with_or(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => (*one).to_string(),
        [first, second] => format!("{first} or {second}"),
        _ => {
            let (last, rest) = items.split_last().expect("non-empty, matched above");
            format!("{}, or {last}", rest.join(", "))
        }
    }
}

/// A short clause naming every adapter that is not ready yet, plus one
/// naming every *ready* adapter whose own `capabilities()` still leaves its
/// launches degraded (no rot score, usage, turn signal, or injected
/// system prompt) -- for `zirv ctx --help`'s `about` text. Both halves are
/// generated from each adapter's own `ready()`/`capabilities()` rather than
/// hardcoded, so a newly wired-up adapter (or one that later closes a
/// capability gap) falls in or out of the sentence on its own. Empty only
/// once every adapter is both ready and fully capable.
///
/// Codex is the adapter this currently discloses: `ready()` no longer
/// hard-errors (its shim gap and `resolve_program` routing are closed, see
/// [[Known Issues]] via CLAUDE.md), but its `capabilities()` is still
/// honestly all-`false` -- `--agent codex` works, silently missing the four
/// things claude gets for free, which a user reading `--help` deserves to
/// see stated plainly rather than only discovering by surprise.
pub fn readiness_note() -> String {
    let mut clauses: Vec<String> = Vec::new();

    // Item 11: each adapter is constructed and `ready()`-checked exactly
    // once here, in one pass -- the two-pass version used to build a fresh
    // adapter and re-call `ready()` a second time for every adapter, once
    // per clause. `ctx_about()`'s `OnceLock` already caps this to once per
    // process, but the hook/statusline path still goes through it on every
    // invocation before that cache is warm.
    let mut not_ready: Vec<&str> = Vec::new();
    let mut degraded: Vec<String> = Vec::new();
    for (name, ctor) in ADAPTERS {
        let adapter = ctor(None);
        if adapter.ready().is_err() {
            not_ready.push(name);
            continue;
        }
        let missing = missing_capability_labels(adapter.capabilities());
        if !missing.is_empty() {
            degraded.push(format!(
                "{name} (launch-level: no {})",
                join_with_or(&missing)
            ));
        }
    }

    if !not_ready.is_empty() {
        clauses.push(format!(
            "Not ready yet: {} (see issue #11).",
            not_ready.join(", ")
        ));
    }
    if !degraded.is_empty() {
        clauses.push(format!(
            "Degraded surface: {} (see issue #11).",
            degraded.join("; ")
        ));
    }

    clauses.join(" ")
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
///
/// G: a repo checkout's own `.settings.toml` may narrow this fallback (take
/// an adapter off the table) but must never *select* a different one for the
/// operator as a side effect of that narrowing -- a repo-only disable
/// (`AgentGate::disabled_only_by_repo`) that would otherwise leave the
/// fallback silently landing on a different, still-enabled adapter refuses
/// instead, naming both adapters and the fix. Skipping past a repo-disabled
/// adapter when *nothing else* qualifies either is unaffected: no different
/// provider was ever silently chosen, so the ordinary aggregate error still
/// applies and still names every candidate.
///
/// G2 (fix): `repo_narrowed` is only recorded when the repo-disabled adapter
/// would *also* have passed `ready()` -- otherwise it was never a candidate
/// this fallback could have landed on in the first place (an unlaunchable
/// bare name, say), and the refusal's own claim that it "would otherwise
/// have been the default agent" would be false. Without this, disabling an
/// already-unlaunchable adapter via `.settings.toml` could still block a
/// perfectly good fallback to the next one, over a hypothetical that was
/// never true.
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
        refuse_if_agent_bin_names_another_adapter(bin, adapter.name())?;
        return Ok((adapter, DefaultOrigin::Configured));
    }

    let mut reasons = Vec::new();
    let mut repo_narrowed: Option<&str> = None;
    for (name, ctor) in ADAPTERS {
        let adapter = ctor(bin);
        if let Some(refusal) = cfg.agents.refusal(name) {
            // Final wave item 3: the same cross-adapter skip Medium 2 gave
            // the enabled-and-ready arm below, applied here too. Without
            // it, `ctor(bin)` on this line always builds the candidate
            // with the *global* `agent_bin`, even when `agent_bin` names a
            // different adapter entirely -- so `adapter.ready()` could
            // report "ready" for, say, a claude adapter whose `program` is
            // actually pointed at a real codex binary. That is not a
            // candidate this fallback could ever have genuinely landed on
            // (the cross-adapter guard would refuse it exactly the way
            // Medium 2 does below), so recording `repo_narrowed` from it
            // would refuse on a false premise: "claude would otherwise
            // have been the default agent" when `agent_bin` never actually
            // named claude's own binary at all.
            if repo_narrowed.is_none()
                && cfg.agents.disabled_only_by_repo(name)
                && agent_bin_names_a_different_adapter(bin, name).is_none()
                && adapter.ready().is_ok()
            {
                repo_narrowed = Some(name);
            }
            reasons.push(format!("{name}: {refusal}"));
            continue;
        }
        match adapter.ready() {
            Ok(()) => {
                if let Some(narrowed) = repo_narrowed {
                    return Err(format!(
                        "the repository checkout disabled '{narrowed}' via .settings.toml, \
                         which would otherwise have been the default agent; a repo may narrow \
                         this fallback but not choose '{name}' for you instead. Pass --agent \
                         explicitly, or set `agent` in your own operator config or environment, \
                         to pick one."
                    )
                    .into());
                }
                // Medium 2: recorded and skipped, not `?`-aborted. `bin`
                // is one value tried against *every* candidate in this
                // loop in registry order -- if it names a different
                // adapter than this one (`name`), the right answer is to
                // keep walking to the adapter it actually does name, not
                // to abort the whole fallback here. An operator with no
                // `agent =` configured, only `agent_bin` pointing at a
                // real codex install, used to get a hard error at claude
                // (first in registry order) instead of landing on codex.
                // The explicit-`--agent` arm above still hard-refuses:
                // there the operator named the mismatch directly, so
                // there is nothing left to fall back to.
                if let Some(other) = agent_bin_names_a_different_adapter(bin, name) {
                    reasons.push(format!("{name}: agent_bin names '{other}', not '{name}'"));
                    continue;
                }
                return Ok((adapter, DefaultOrigin::FirstEnabledReady));
            }
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
        refuse_if_agent_bin_names_another_adapter(bin, adapter.name())?;
        return Ok(adapter);
    }

    if let Some(adapter) = adapters.into_iter().find(|a| a.detect(command)) {
        if let Some(refusal) = cfg.agents.refusal(adapter.name()) {
            return Err(refusal.into());
        }
        adapter.ready()?;
        refuse_if_agent_bin_names_another_adapter(bin, adapter.name())?;
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

    #[test]
    fn join_with_or_reads_like_plain_english() {
        assert_eq!(join_with_or(&[]), "");
        assert_eq!(join_with_or(&["a"]), "a");
        assert_eq!(join_with_or(&["a", "b"]), "a or b");
        assert_eq!(join_with_or(&["a", "b", "c"]), "a, b, or c");
    }

    #[test]
    fn missing_capability_labels_names_only_the_false_flags() {
        let all_false = Capabilities::default();
        assert_eq!(
            missing_capability_labels(all_false),
            vec!["rot score", "usage", "turn signal", "injected prompt"]
        );

        let all_true = Capabilities {
            marker_signal: true,
            token_usage: true,
            turn_signal: true,
            system_prompt: true,
            events: true,
        };
        assert!(missing_capability_labels(all_true).is_empty());

        let mixed = Capabilities {
            events: true,
            ..Capabilities::default()
        };
        assert_eq!(
            missing_capability_labels(mixed),
            vec!["usage", "turn signal", "injected prompt"],
            "an adapter with real events but nothing else"
        );
    }

    /// F: codex is ready (its own `ready()` no longer hard-errors) but still
    /// honestly all-`false` in `capabilities()`, so `--help`'s about text
    /// must keep disclosing the degraded surface even though codex no longer
    /// shows up in the "not ready yet" clause at all.
    #[test]
    fn the_readiness_note_discloses_codexs_degraded_surface_now_that_it_is_ready() {
        let note = readiness_note();
        assert!(
            !note.to_lowercase().contains("not ready"),
            "codex is ready now, not unready: {note}"
        );
        assert!(note.contains("codex"), "got {note}");
        assert!(note.contains("rot score"), "got {note}");
        assert!(note.contains("usage"), "got {note}");
        assert!(note.contains("turn signal"), "got {note}");
        assert!(note.contains("injected prompt"), "got {note}");
        assert!(note.contains("issue #11"), "got {note}");
        assert!(
            !note.contains("claude (launch-level"),
            "claude is fully capable and must not appear in the degraded clause: {note}"
        );
    }

    #[test]
    fn harness_prompt_lines_returns_one_line_per_registered_adapter() {
        let lines = harness_prompt_lines(&permissive_cfg(), "");
        // One line per adapter, plus one trailing "- code review: ..." line
        // naming every enabled harness's resolved review model.
        assert_eq!(lines.len(), ADAPTERS.len() + 1);
        for (name, _) in ADAPTERS {
            assert!(
                lines.iter().any(|l| l.starts_with(&format!("- {name}:"))),
                "missing a line for '{name}': {lines:?}"
            );
        }
        assert!(
            lines
                .last()
                .is_some_and(|l| l.starts_with("- code review:")),
            "the review line comes last: {lines:?}"
        );
    }

    /// Unconfigured `review.claude`/`review.codex`: the roster line names
    /// each enabled harness's ladder-computed default (one tier below the
    /// seat -- unset `chat.model` assumes the top tier), marks each entry as
    /// a default rather than an operator choice, and states the never-the-
    /// seat / outranks-other-routing rule.
    #[test]
    fn harness_prompt_lines_review_line_shows_computed_defaults_when_unset() {
        let lines = harness_prompt_lines(&permissive_cfg(), "");
        let review_line = lines.last().expect("at least the review line");
        assert!(
            review_line.contains("claude -> \"opus\" (default: one tier below the seat)"),
            "got {review_line}"
        );
        assert!(
            review_line.contains("codex -> \"gpt-5.6-terra\" (default: one tier below the seat)"),
            "got {review_line}"
        );
        assert!(
            review_line.contains("never on an orchestrator seat's own model"),
            "got {review_line}"
        );
        assert!(
            review_line.contains("outranks"),
            "states it outranks other routing guidance: {review_line}"
        );
    }

    /// An operator-configured `review.<agent>` wins over the ladder default
    /// and is marked `(configured)` rather than `(default: ...)`.
    #[test]
    fn harness_prompt_lines_review_line_uses_the_operators_configured_model() {
        let cfg = CtxConfig {
            review: crate::commands::ctx::config::ReviewConfig {
                claude: Some("custom-review-model".to_string()),
                codex: None,
            },
            ..permissive_cfg()
        };
        let lines = harness_prompt_lines(&cfg, "");
        let review_line = lines.last().expect("at least the review line");
        assert!(
            review_line.contains("claude -> \"custom-review-model\" (configured)"),
            "got {review_line}"
        );
        assert!(
            review_line.contains("codex -> \"gpt-5.6-terra\" (default: one tier below the seat)"),
            "codex stays on its computed default: {review_line}"
        );
    }

    /// A disabled harness gets no entry in the review line at all -- same
    /// absence-not-silence rule its own per-harness line above follows.
    #[test]
    fn harness_prompt_lines_review_line_omits_a_disabled_harnesses_entry() {
        let cfg = cfg_disabling("codex");
        let lines = harness_prompt_lines(&cfg, "");
        let review_line = lines.last().expect("at least the review line");
        assert!(
            review_line.contains("claude ->"),
            "claude stays: {review_line}"
        );
        assert!(
            !review_line.contains("codex ->"),
            "a disabled harness must not appear in the review line: {review_line}"
        );
    }

    /// Normal case (no entry's resolved model equals the orchestrator seat):
    /// the strict "never on an orchestrator seat's own model" clause is
    /// true for every entry, so it stays.
    #[test]
    fn review_roster_line_normal_case_keeps_the_strict_clause() {
        let lines = harness_prompt_lines(&permissive_cfg(), "");
        let review_line = lines.last().expect("at least the review line");
        assert!(
            review_line.contains("never on an orchestrator seat's own model"),
            "got {review_line}"
        );
        assert!(
            !review_line.contains("never on a model above the named one"),
            "the softened clause must not appear when nothing is contradictory: {review_line}"
        );
    }

    /// Floor-tier case: seat "haiku" resolves (unconfigured) to claude's own
    /// floor default "haiku" too -- neither "one tier below the seat" nor
    /// "never on an orchestrator seat's own model" would be true of that
    /// entry, so both the per-entry note and the global clause must adjust
    /// to stay honest.
    #[test]
    fn review_roster_line_floor_seat_case_is_not_contradictory() {
        let cfg = CtxConfig {
            chat: crate::commands::ctx::config::ChatConfig {
                model: Some("haiku".to_string()),
            },
            ..permissive_cfg()
        };
        let lines = harness_prompt_lines(&cfg, "");
        let review_line = lines.last().expect("at least the review line");
        assert!(
            review_line.contains("claude -> \"haiku\""),
            "got {review_line}"
        );
        assert!(
            !review_line.contains("claude -> \"haiku\" (default: one tier below the seat)"),
            "that note would be false when the seat is already the floor: {review_line}"
        );
        assert!(review_line.contains("floor tier"), "got {review_line}");
        assert!(
            !review_line.contains("never on an orchestrator seat's own model"),
            "that clause would be false for the floor-tier entry: {review_line}"
        );
    }

    /// Configured-equals-seat case: the operator's own `review.claude`
    /// explicitly names the same model as the orchestrator seat -- allowed
    /// (the operator's choice wins), but then "never on an orchestrator
    /// seat's own model" is false of that entry, so the global clause must
    /// soften to something that stays true.
    #[test]
    fn review_roster_line_configured_equals_seat_case_is_not_contradictory() {
        let cfg = CtxConfig {
            chat: crate::commands::ctx::config::ChatConfig {
                model: Some("opus".to_string()),
            },
            review: crate::commands::ctx::config::ReviewConfig {
                claude: Some("opus".to_string()),
                codex: None,
            },
            ..permissive_cfg()
        };
        let lines = harness_prompt_lines(&cfg, "");
        let review_line = lines.last().expect("at least the review line");
        assert!(
            review_line.contains("claude -> \"opus\" (configured)"),
            "got {review_line}"
        );
        assert!(
            !review_line.contains("never on an orchestrator seat's own model"),
            "that clause would be false for the operator's own configured entry: {review_line}"
        );
    }

    /// The seat threads all the way from `cfg.chat.model` through to the
    /// rendered claude entry: seat "sonnet" resolves claude's own ladder
    /// default to "haiku" (one tier below sonnet).
    #[test]
    fn harness_prompt_lines_review_line_threads_the_seat_for_claude() {
        let cfg = CtxConfig {
            chat: crate::commands::ctx::config::ChatConfig {
                model: Some("sonnet".to_string()),
            },
            ..permissive_cfg()
        };
        let lines = harness_prompt_lines(&cfg, "");
        let review_line = lines.last().expect("at least the review line");
        assert!(
            review_line.contains("claude -> \"haiku\" (default: one tier below the seat)"),
            "got {review_line}"
        );
    }

    /// No enabled harness at all: `review_roster_line` must not emit a
    /// line naming zero harnesses -- absence, not an empty-handed line.
    #[test]
    fn harness_prompt_lines_omits_the_review_line_when_no_harness_is_enabled() {
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
        let lines = harness_prompt_lines(&cfg, "");
        assert!(
            !lines.iter().any(|l| l.starts_with("- code review:")),
            "no harness enabled: there must be no review line at all: {lines:?}"
        );
    }

    /// The one call site (`prompt::compose` for an Orchestrator session) must
    /// never learn about a disabled adapter as if it were offered for
    /// delegation: a disabled line names where the disable came from and
    /// never the `zirv agent <name>` invitation.
    #[test]
    fn harness_prompt_lines_names_the_disabled_adapter_and_its_location() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let env: std::collections::HashMap<String, String> =
            [("ZIRV_AGENT_CODEX_ENABLED".to_string(), "false".to_string())]
                .into_iter()
                .collect();
        let cfg = CtxConfig {
            agents: crate::settings::AgentGate::load(repo.path(), &|k| env.get(k).cloned())
                .expect("load"),
            ..CtxConfig::default()
        };

        let lines = harness_prompt_lines(&cfg, "");
        let codex_line = lines
            .iter()
            .find(|l| l.starts_with("- codex:"))
            .expect("codex line present");
        assert!(codex_line.contains("disabled"), "got {codex_line}");
        assert!(
            codex_line.contains("ZIRV_AGENT_CODEX_ENABLED"),
            "names the environment source: {codex_line}"
        );
        assert!(
            !codex_line.contains("zirv agent codex"),
            "a disabled adapter is never offered for delegation: {codex_line}"
        );
    }

    /// Finding 1: `ready()` alone is fail-open for a program that simply is
    /// not on disk anywhere -- `resolve_program` deliberately returns `Ok`
    /// for it (see its own doc comment), and several other call sites lean on
    /// that. `harness_prompt_lines` must not repeat the same fail-open claim
    /// in a roster line an orchestrator can act on immediately: a name that
    /// resolves to nothing must read as not installed, not ready.
    #[test]
    fn harness_prompt_lines_reports_not_installed_when_the_resolved_program_is_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("nonexistent-agent-binary");
        let cfg = CtxConfig {
            agent_bin: Some(missing.display().to_string()),
            ..permissive_cfg()
        };

        let lines = harness_prompt_lines(&cfg, "");
        let claude_line = lines
            .iter()
            .find(|l| l.starts_with("- claude:"))
            .expect("claude line present");
        assert!(claude_line.contains("not installed"), "got {claude_line}");
        assert!(
            !claude_line.contains("zirv agent"),
            "a binary that is not there must never be offered for delegation: {claude_line}"
        );
    }

    /// Finding 1's positive case, plus Finding 4: a program that genuinely
    /// exists on disk is still offered for delegation, except when it is
    /// this session's own adapter, which is marked as such instead of
    /// inviting a session to delegate to itself.
    ///
    /// Each adapter's own default program name (no `agent_bin` override) is
    /// planted as a real stub on a PATH restricted to one temp dir, so
    /// codex's "ready" verdict here is earned by codex's own binary, never
    /// borrowed from an unrelated stub -- see the follow-up regression right
    /// below this test for the case where a *shared* override used to make
    /// that borrowing happen.
    #[test]
    fn harness_prompt_lines_offers_delegation_only_to_a_present_non_self_adapter() {
        let dir = tempfile::tempdir().expect("tempdir");
        for name in ["claude", "codex"] {
            std::fs::write(dir.path().join(name), "").expect("write stub");
        }
        let _path_guard = crate::commands::ctx::testenv::VarGuard::set(&[(
            "PATH",
            Some(dir.path().to_str().expect("utf8 tempdir path")),
        )]);
        let cfg = permissive_cfg();

        let lines = harness_prompt_lines(&cfg, "claude");
        let claude_line = lines
            .iter()
            .find(|l| l.starts_with("- claude:"))
            .expect("claude line present");
        assert!(
            claude_line.contains("this session's harness"),
            "got {claude_line}"
        );
        assert!(
            !claude_line.contains("zirv agent claude"),
            "a session never invites itself to delegate: {claude_line}"
        );

        let codex_line = lines
            .iter()
            .find(|l| l.starts_with("- codex:"))
            .expect("codex line present");
        assert!(
            codex_line.contains("zirv agent codex"),
            "a present, non-self adapter is still offered on the strength of its own binary: \
             {codex_line}"
        );
    }

    /// Item 1 regression: `harness_prompt_lines` used to build *every*
    /// adapter with the same global `agent_bin` override, so `agent_bin`
    /// naming claude's binary made codex's line borrow claude's presence
    /// verdict and falsely offer `zirv agent codex` -- a wasted delegation
    /// every review round, since `select` would go on to refuse it
    /// ("agent_bin names 'claude', not 'codex'"). With no `codex` binary
    /// anywhere on this test's restricted `PATH`, codex must read as not
    /// installed regardless of how present claude's own stub is.
    #[test]
    fn harness_prompt_lines_never_borrows_a_named_adapters_presence_for_another() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("claude"), "").expect("write stub");
        let _path_guard = crate::commands::ctx::testenv::VarGuard::set(&[(
            "PATH",
            Some(dir.path().to_str().expect("utf8 tempdir path")),
        )]);
        let cfg = CtxConfig {
            agent_bin: Some(dir.path().join("claude").display().to_string()),
            ..permissive_cfg()
        };

        let lines = harness_prompt_lines(&cfg, "claude");
        let claude_line = lines
            .iter()
            .find(|l| l.starts_with("- claude:"))
            .expect("claude line present");
        assert!(
            claude_line.contains("this session's harness"),
            "claude's own named override is present: {claude_line}"
        );

        let codex_line = lines
            .iter()
            .find(|l| l.starts_with("- codex:"))
            .expect("codex line present");
        assert!(
            !codex_line.contains("zirv agent codex"),
            "agent_bin naming claude must never make codex's line claim delegable: {codex_line}"
        );
        assert!(
            codex_line.contains("not installed"),
            "codex is judged on its own (absent) binary, not claude's override: {codex_line}"
        );
    }

    /// A capacity-limited harness's roster line gets the `-- small tasks
    /// only` suffix; an unmarked harness's line does not. This is the
    /// signal `HARNESS_PROMPT`'s final paragraph tells an orchestrator to
    /// route only small, bounded briefs by, for both reviews and `zirv
    /// agent` delegations.
    #[test]
    fn harness_prompt_lines_marks_a_capacity_limited_harness_small_tasks_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        for name in ["claude", "codex"] {
            std::fs::write(dir.path().join(name), "").expect("write stub");
        }
        let _path_guard = crate::commands::ctx::testenv::VarGuard::set(&[(
            "PATH",
            Some(dir.path().to_str().expect("utf8 tempdir path")),
        )]);

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/.settings.toml"),
            "[agents.codex]\ncapacity = \"small\"\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home_guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let cfg = CtxConfig {
            agents: crate::settings::AgentGate::load(repo.path(), &|k| empty.get(k).cloned())
                .expect("load"),
            ..CtxConfig::default()
        };

        let lines = harness_prompt_lines(&cfg, "claude");
        let codex_line = lines
            .iter()
            .find(|l| l.starts_with("- codex:"))
            .expect("codex line present");
        assert!(
            codex_line.contains("ready -- small tasks only"),
            "got {codex_line}"
        );

        let claude_line = lines
            .iter()
            .find(|l| l.starts_with("- claude:"))
            .expect("claude line present");
        assert!(
            !claude_line.contains("small tasks only"),
            "claude was never marked capacity-small: {claude_line}"
        );
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

    /// H1/H2: both `readiness_note`'s "not ready yet" clause and `resolve_
    /// default`'s `Err(e) => continue` unready-skip branch lost their only
    /// coverage once codex's own `ready()` stopped hard-erroring -- nothing
    /// in the real registry is ever actually unready anymore, so a test that
    /// only reads the real `ADAPTERS` table can no longer exercise either
    /// branch at all. This forces claude's own bare `"claude"` name to
    /// resolve to an unlaunchable `.py` (the same PATH/PATHEXT rig `an_
    /// unlaunchable_program_on_path_is_named_rather_than_left_to_error_193`
    /// uses), which is the one real way `ready()` fails on this codebase,
    /// leaving codex genuinely unaffected (codex.cmd, wherever it resolves
    /// or fails to, is never a `ready()` error case) to prove the skip-and-
    /// continue path lands on it.
    #[cfg(windows)]
    #[test]
    fn readiness_note_and_the_fallback_skip_both_stay_covered_when_an_adapter_is_genuinely_unready()
    {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("claude.py"), "print('x')\n").expect("write");

        let path = std::env::var("PATH").unwrap_or_default();
        let _path_guard = crate::commands::ctx::testenv::VarGuard::set(&[
            (
                "PATH",
                Some(format!("{};{}", dir.path().display(), path).as_str()),
            ),
            ("PATHEXT", Some(".EXE;.CMD;.PY")),
        ]);

        // H1: the "not ready yet" clause is genuinely exercised again.
        let note = readiness_note();
        assert!(
            note.to_lowercase().contains("not ready"),
            "claude must be reported not ready under this rig: {note}"
        );
        assert!(note.contains("claude"), "got {note}");

        // H2: `resolve_default`'s fallback must skip claude's `Err` and land
        // on codex, exercising the `Err(e) => reasons.push(...); continue`
        // arm rather than the `Ok(())` one.
        let (adapter, origin) = resolve_default(&permissive_cfg()).expect("codex still qualifies");
        assert_eq!(adapter.name(), "codex");
        assert_eq!(origin, DefaultOrigin::FirstEnabledReady);
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

    /// FINDING 3: an argv that is *already resolved* to the `cmd.exe /c <shim>`
    /// launcher form (what the interactive path hands `injection_args_for_
    /// session`) is recognised as reparsing, where re-resolving the literal
    /// head `cmd.exe` would have found a plain `.exe` and missed it. A direct
    /// `.exe` argv is not a launcher form and is not flagged.
    #[cfg(windows)]
    #[test]
    fn an_already_resolved_launcher_argv_is_recognised_as_reparsing() {
        let resolved_cmd = vec![
            "cmd.exe".to_string(),
            "/c".to_string(),
            "C:\\tools\\claude.cmd".to_string(),
            "the prompt".to_string(),
        ];
        assert!(launch_reparses_through_shim(&resolved_cmd));

        let resolved_ps = vec![
            "powershell".to_string(),
            "-NoProfile".to_string(),
            "-File".to_string(),
            "C:\\tools\\agent.ps1".to_string(),
            "arg".to_string(),
        ];
        assert!(launch_reparses_through_shim(&resolved_ps));

        let direct = vec!["C:\\tools\\claude.exe".to_string(), "--resume".to_string()];
        assert!(!launch_reparses_through_shim(&direct));
    }

    /// Off Windows there is no launcher reparse, so the detection is always
    /// `false` -- including for an argv that structurally looks like one.
    #[cfg(not(windows))]
    #[test]
    fn launch_reparse_detection_is_a_noop_off_windows() {
        let looks_like_cmd = vec![
            "cmd.exe".to_string(),
            "/c".to_string(),
            "claude.cmd".to_string(),
        ];
        assert!(!launch_reparses_through_shim(&looks_like_cmd));
        assert!(!launch_reparses_through_shim(&[]));
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

    /// `agent_bin` is one global override applied to whichever adapter gets
    /// selected. Naming codex explicitly while `agent_bin` points at a real
    /// `claude` install (stale config left over from switching agents is the
    /// plausible way this happens) would otherwise launch claude's binary
    /// dressed up in codex's own `exec <prompt>` argv shape -- wrong account,
    /// wrong safety model, no error naming what happened. Both names appear
    /// in the refusal, and it is basename-only: the full path is never a
    /// factor.
    #[test]
    fn agent_bin_naming_a_different_adapter_than_selected_is_refused() {
        let mut cfg = permissive_cfg();
        cfg.agent_bin = Some("/opt/homebrew/bin/claude".to_string());
        let err = select(Some("codex"), &[], &cfg).expect_err("cross-adapter agent_bin refuses");
        let msg = err.to_string();
        assert!(
            msg.contains("claude"),
            "names the binary's own agent: {msg}"
        );
        assert!(
            msg.contains("codex"),
            "names the one that was selected: {msg}"
        );
    }

    /// The same collision reached through `resolve_default`'s own
    /// *configured* arm (`cfg.agent` set explicitly, just not on the CLI) --
    /// still a hard refusal, unlike the fallback loop below.
    #[test]
    fn agent_bin_naming_a_different_adapter_is_refused_through_the_default_fallback_too() {
        let mut cfg = permissive_cfg();
        cfg.agent = Some("codex".to_string());
        cfg.agent_bin = Some("claude.exe".to_string());
        let err = resolve_default(&cfg).expect_err("cross-adapter agent_bin refuses");
        let msg = err.to_string();
        assert!(msg.contains("claude"), "got {msg}");
        assert!(msg.contains("codex"), "got {msg}");
    }

    /// Medium 2 (fix): with *no* `cfg.agent` configured, `resolve_default`'s
    /// own fallback loop tries `ADAPTERS` in registry order (`claude` first)
    /// -- before this fix, `agent_bin` naming a real codex install still hit
    /// claude first, and the cross-adapter guard's `?` aborted the whole
    /// fallback right there instead of continuing on to codex, the adapter
    /// that binary actually is. It must resolve to codex, not error.
    #[test]
    fn agent_bin_naming_codex_with_no_agent_configured_falls_through_to_codex() {
        let cfg = CtxConfig {
            agent_bin: Some("/definitely/not/a/real/path/codex".to_string()),
            ..permissive_cfg()
        };
        let (adapter, origin) =
            resolve_default(&cfg).expect("falls through past claude to codex, not an error");
        assert_eq!(adapter.name(), "codex");
        assert_eq!(origin, DefaultOrigin::FirstEnabledReady);
    }

    /// The other half of the same fix: a basename that names *no* registered
    /// adapter at all -- a stub path, or the `sh <fixture>.sh` wrapper shape
    /// this codebase's own tests use throughout -- is never a collision, no
    /// matter how unrelated it looks, and a value that happens to name the
    /// *same* adapter as the one selected (a differently located install) is
    /// explicitly fine too.
    #[test]
    fn agent_bin_naming_no_adapter_or_the_same_one_stays_allowed() {
        let cfg = permissive_cfg();
        assert_eq!(
            agent_bin_names_a_different_adapter(Some("/tmp/fake-codex"), "codex"),
            None,
            "a stub path matches nothing"
        );
        assert_eq!(
            agent_bin_names_a_different_adapter(
                Some("sh /repo/tests/fixtures/fake-codex-agent.sh"),
                "codex"
            ),
            None,
            "the wrapper shape's own basename is \"sh\", not an adapter name"
        );
        assert_eq!(
            agent_bin_names_a_different_adapter(Some("/opt/codex-beta/codex"), "codex"),
            None,
            "naming the selected adapter itself is not a collision"
        );

        let mut cfg = cfg;
        cfg.agent_bin = Some("/opt/codex-beta/codex".to_string());
        let adapter =
            select(Some("codex"), &[], &cfg).expect("same-adapter agent_bin is never refused");
        assert_eq!(adapter.name(), "codex");
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
    /// enabled and ready. Now that codex's own `ready()` succeeds too (see
    /// `CodexAdapter::ready`), both adapters qualify, so this also pins
    /// `ADAPTERS`' registry order (`("claude", ...)` first) as what actually
    /// decides the winner -- both are asserted, the property for its own
    /// sake and the concrete name because losing it silently would be a
    /// regression worth catching too.
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

    /// G: `select`'s empty-command default no longer silently lands on a
    /// different provider just because the repo checkout narrowed claude
    /// off the table -- codex's own `ready()` succeeding too used to make
    /// the fallback pick it automatically, which handed a repo checkout the
    /// power to select which vendor account gets spent. It must refuse
    /// instead, naming both adapters and the fix, exercised here through the
    /// public `select` entry point rather than `resolve_default` directly.
    #[test]
    fn the_default_fallback_refuses_rather_than_silently_switching_provider() {
        let cfg = cfg_disabling("claude");
        let err = select(None, &[], &cfg).expect_err("a repo may narrow, not select");
        let msg = err.to_string();
        assert!(msg.contains("claude"), "got {msg}");
        assert!(msg.contains("codex"), "got {msg}");
        assert!(msg.contains("--agent"), "must say how to fix it: {msg}");
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

    /// A provider slug names a usage file, so it has to already *be* a slug:
    /// lowercase `[a-z0-9-]`, non-empty, and unchanged by the sanitiser that
    /// turns it into a file name. It is also the account, not the program --
    /// claude's is `anthropic`, not `claude`.
    #[test]
    fn every_adapter_names_the_account_its_limits_belong_to() {
        for adapter in all(None) {
            let provider = adapter.provider();
            assert!(!provider.is_empty(), "{} has no provider", adapter.name());
            assert_eq!(
                crate::commands::ctx::state::provider_slug(provider),
                provider,
                "{provider} is not already a filesystem-safe lowercase slug"
            );
        }

        let claude = claude::ClaudeAdapter::new(None);
        assert_ne!(
            claude.provider(),
            claude.name(),
            "the provider is the account, not the binary: two harnesses can share one"
        );
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

    /// G: now that codex's own `ready()` only checks that its program
    /// resolves (exactly like claude's, see `CodexAdapter::ready`),
    /// disabling claude via a repo-only `.settings.toml` does not leave the
    /// fallback with nothing enabled-and-ready -- codex, next in registry
    /// order, would qualify. `resolve_default` must refuse rather than
    /// silently landing on it: the repo checkout narrowed claude off the
    /// table, but selecting codex *instead* is not the repo's call to make
    /// (`AgentGate::disabled_only_by_repo`).
    #[test]
    fn the_fallback_refuses_to_silently_switch_provider_when_the_repo_disabled_the_default() {
        let cfg = cfg_disabling("claude");
        let err = resolve_default(&cfg).expect_err("a repo may narrow, not select");
        let msg = err.to_string();
        assert!(msg.contains("claude"), "names the narrowed adapter: {msg}");
        assert!(
            msg.contains("codex"),
            "names what it would have silently picked: {msg}"
        );
        assert!(msg.contains("--agent"), "says how to fix it: {msg}");
    }

    /// G2 (fix): the "would otherwise have been the default agent" refusal
    /// must not fire when the repo-disabled adapter was never actually a
    /// candidate -- here claude is *both* repo-disabled *and* genuinely
    /// unready (the same PATH/PATHEXT rig `readiness_note_and_the_fallback_
    /// skip_both_stay_covered_when_an_adapter_is_genuinely_unready` uses), so
    /// disabling it changed nothing: codex was always going to be the
    /// fallback either way, and the refusal's own premise would be false.
    #[cfg(windows)]
    #[test]
    fn the_narrowed_refusal_does_not_fire_for_an_adapter_that_was_never_ready_anyway() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("claude.py"), "print('x')\n").expect("write");
        let path = std::env::var("PATH").unwrap_or_default();
        let _path_guard = crate::commands::ctx::testenv::VarGuard::set(&[
            (
                "PATH",
                Some(format!("{};{}", dir.path().display(), path).as_str()),
            ),
            ("PATHEXT", Some(".EXE;.CMD;.PY")),
        ]);

        let cfg = cfg_disabling("claude");
        let (adapter, origin) =
            resolve_default(&cfg).expect("codex qualifies; claude was never a real candidate");
        assert_eq!(adapter.name(), "codex");
        assert_eq!(origin, DefaultOrigin::FirstEnabledReady);
    }

    /// Final wave item 3: the same false-premise class as the test above,
    /// but reached through `agent_bin` instead of an unresolvable `PATH`.
    /// Claude is repo-disabled, and `agent_bin`'s own basename names codex,
    /// not claude -- so the pre-check used to build `ClaudeAdapter::new(bin)`
    /// (a claude adapter whose `program` actually points at a codex binary)
    /// and ask *that* whether it is `ready()`, which can genuinely answer
    /// yes without claude's own real binary ever being consulted at all.
    /// Recording `repo_narrowed` from that would refuse with a false claim
    /// ("claude would otherwise have been the default agent") over a
    /// candidate `agent_bin_names_a_different_adapter` was always going to
    /// refuse anyway (Medium 2). It must instead land on codex.
    #[test]
    fn the_narrowed_refusal_does_not_fire_when_agent_bin_names_a_different_adapter() {
        let cfg = CtxConfig {
            agent_bin: Some("/definitely/not/a/real/path/codex".to_string()),
            ..cfg_disabling("claude")
        };
        let (adapter, origin) = resolve_default(&cfg)
            .expect("codex qualifies; agent_bin never actually named claude's own binary");
        assert_eq!(adapter.name(), "codex");
        assert_eq!(origin, DefaultOrigin::FirstEnabledReady);
    }

    /// G: the refusal is specific to a *repo-only* disable. An operator who
    /// disabled claude themselves (home file or environment) has already
    /// made the choice the fallback would otherwise be accused of making for
    /// them, so codex is picked normally, exactly as before this fix.
    #[test]
    fn an_operator_disable_still_falls_through_normally() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/.settings.toml"),
            "[agents.claude]\nenabled = false\n",
        )
        .expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let cfg = CtxConfig {
            agents: crate::settings::AgentGate::load(repo.path(), &|k| empty.get(k).cloned())
                .expect("load"),
            ..CtxConfig::default()
        };

        let (adapter, origin) = resolve_default(&cfg).expect("the operator's own choice");
        assert_eq!(adapter.name(), "codex");
        assert_eq!(origin, DefaultOrigin::FirstEnabledReady);
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
    /// detection named an adapter; either one must bypass it entirely.
    #[test]
    fn an_explicit_or_detected_agent_still_bypasses_the_fallback_entirely() {
        let cfg = cfg_disabling("claude");

        // H3: `resolve_default`'s own fallback would *refuse* under this
        // exact cfg (G: claude is disabled only by the repo layer, and
        // codex would otherwise be silently picked instead) -- proving that
        // if `select(Some("codex"), ...)` below were ever accidentally
        // routed through the fallback instead of truly bypassing it, this
        // test would see that refusal, not a quiet "codex" answer. The two
        // assertions below are provably distinguishable outcomes, not the
        // same value reached two different ways.
        resolve_default(&cfg).expect_err("the fallback itself must refuse here");

        // Explicit name: codex is still enabled by this gate and now
        // resolves successfully, so it is selected directly without ever
        // consulting the fallback.
        let adapter = select(Some("codex"), &[], &cfg).expect("codex is enabled and ready");
        assert_eq!(adapter.name(), "codex");

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

    // Worker model resolution (`resolve_worker_model`/`worker_model_args`):
    // the delegated-headless-worker analogue of `resolve_review_model`
    // above, but with a fixed adapter-owned default instead of a ladder.

    #[test]
    fn worker_model_args_uses_the_configured_value_over_the_adapter_default() {
        let adapter = claude::ClaudeAdapter::new(None);
        let cfg = CtxConfig {
            worker: crate::commands::ctx::config::WorkerConfig {
                claude: Some("opus".to_string()),
                codex: None,
            },
            ..permissive_cfg()
        };
        assert_eq!(
            worker_model_args(&cfg, "claude", &adapter),
            vec!["--model".to_string(), "opus".to_string()],
            "the operator's own worker.claude wins over the hard default"
        );
    }

    #[test]
    fn worker_model_args_falls_back_to_claudes_hard_sonnet_default() {
        let adapter = claude::ClaudeAdapter::new(None);
        let cfg = permissive_cfg();
        assert_eq!(cfg.worker.claude, None, "nothing configured");
        assert_eq!(
            worker_model_args(&cfg, "claude", &adapter),
            vec!["--model".to_string(), "sonnet".to_string()],
            "claude's own hard default stops a worker inheriting the operator's seat model"
        );
    }

    #[test]
    fn worker_model_args_adds_nothing_for_codex_with_no_configured_default() {
        let adapter = codex::CodexAdapter::new(None);
        let cfg = permissive_cfg();
        assert_eq!(cfg.worker.codex, None, "nothing configured");
        assert!(
            worker_model_args(&cfg, "codex", &adapter).is_empty(),
            "codex has no adapter-owned default, so its own CLI/config default applies untouched"
        );
    }

    #[test]
    fn worker_model_args_uses_the_configured_codex_value_when_set() {
        let adapter = codex::CodexAdapter::new(None);
        let cfg = CtxConfig {
            worker: crate::commands::ctx::config::WorkerConfig {
                claude: None,
                codex: Some("gpt-5.6-terra".to_string()),
            },
            ..permissive_cfg()
        };
        assert_eq!(
            worker_model_args(&cfg, "codex", &adapter),
            vec!["--model".to_string(), "gpt-5.6-terra".to_string()],
        );
    }

    // FIX A: `last_model_flag` recognises codex's `-m` short alias in every
    // form, not just claude's long `--model`.

    fn flags(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn last_model_flag_reads_the_separated_short_form() {
        assert_eq!(last_model_flag(&flags(&["-m", "opus"])), Some("opus"));
    }

    #[test]
    fn last_model_flag_reads_the_joined_equals_short_form() {
        assert_eq!(last_model_flag(&flags(&["-m=opus"])), Some("opus"));
    }

    #[test]
    fn last_model_flag_reads_the_attached_short_form() {
        assert_eq!(last_model_flag(&flags(&["-mopus"])), Some("opus"));
    }

    /// Last occurrence wins across every mixed spelling -- long, short
    /// separated, short joined, short attached -- in argv order.
    #[test]
    fn last_model_flag_last_wins_across_mixed_forms() {
        assert_eq!(
            last_model_flag(&flags(&["--model", "opus", "-mhaiku"])),
            Some("haiku"),
            "a later attached -m overrides an earlier long --model"
        );
        assert_eq!(
            last_model_flag(&flags(&["-mopus", "--model=sonnet"])),
            Some("sonnet"),
            "a later joined --model= overrides an earlier attached -m"
        );
        assert_eq!(
            last_model_flag(&flags(&["-m", "opus", "-m=haiku", "-msonnet"])),
            Some("sonnet"),
            "every short form in argv order, last wins"
        );
    }

    /// `--model-foo` starts with `-m` once its own leading `-` is peeled,
    /// but it is a `--`-prefixed long flag, not codex's short alias, and
    /// must never be misread as `-m` with an attached value of `odel-foo`.
    #[test]
    fn a_long_flag_that_merely_starts_with_m_does_not_match() {
        assert_eq!(last_model_flag(&flags(&["--model-foo", "opus"])), None);
    }

    /// A bare `-m` with nothing after it (end of args) has no value to
    /// contribute -- it must not be read as naming an empty/wrong model, and
    /// must not clear an earlier real match either.
    #[test]
    fn a_trailing_bare_short_flag_with_no_value_contributes_nothing() {
        assert_eq!(last_model_flag(&flags(&["-m"])), None);
        assert_eq!(
            last_model_flag(&flags(&["-m", "opus", "-m"])),
            Some("opus"),
            "a later dangling -m must not erase the earlier real match"
        );
    }

    #[test]
    fn last_model_flag_returns_none_with_no_model_flag_at_all() {
        assert_eq!(last_model_flag(&flags(&["--verbose", "-x"])), None);
    }

    // `model_only_flags`: the one trailing-flag shape a dashboard pane can
    // honour, in every spelling `classify_model_flag` reads.

    #[test]
    fn model_only_flags_reads_every_spelling_of_a_lone_model_pin() {
        for spelling in [
            vec!["--model", "haiku"],
            vec!["--model=haiku"],
            vec!["-m", "haiku"],
            vec!["-m=haiku"],
            vec!["-mhaiku"],
        ] {
            assert_eq!(
                model_only_flags(&flags(&spelling)),
                Some("haiku"),
                "{spelling:?} pins a model and nothing else"
            );
        }
    }

    /// Anything beyond a model pin means the pane cannot honour what the
    /// operator typed, so the delegation goes headless instead of silently
    /// dropping the rest.
    #[test]
    fn model_only_flags_rejects_flags_a_pane_cannot_honour() {
        for other in [
            vec![],
            vec!["--verbose"],
            vec!["--model", "haiku", "--verbose"],
            vec!["--dangerously-skip-permissions", "--model=haiku"],
        ] {
            assert_eq!(
                model_only_flags(&flags(&other)),
                None,
                "{other:?} is not a lone model pin"
            );
        }
    }

    /// A pin with no usable value is not a pin: a dangling bare flag, a blank
    /// value, and a flag-shaped value all decline the pane rather than build a
    /// `--model` argv token out of nonsense.
    #[test]
    fn model_only_flags_rejects_a_pin_with_no_usable_value() {
        assert_eq!(model_only_flags(&flags(&["--model"])), None);
        assert_eq!(model_only_flags(&flags(&["--model", "  "])), None);
        assert_eq!(model_only_flags(&flags(&["--model="])), None);
        assert_eq!(model_only_flags(&flags(&["--model", "--verbose"])), None);
    }
}
