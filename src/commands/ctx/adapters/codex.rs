use std::path::{Path, PathBuf};
use std::process::Command;

use super::super::CtxResult;
use super::super::event::{
    Capabilities, NormalizedEvent, SessionId, SessionRef, StructuralContext,
};
use super::{AgentAdapter, ResolvedProgram, TurnSignalSetup};

/// Verified facts backing this adapter live in
/// `docs/superpowers/notes/2026-07-31-codex-cli-facts.md`. `parse_events` and
/// `structural_context` stay empty on purpose (out of scope; tracked in
/// [issue #11](https://github.com/Glubiz/zirv-dynamic-cli/issues/11)): that
/// file records the assistant/tool-call/token-usage event shapes and the
/// notify contract as unverified, since codex requires authentication and
/// exposes a notify mechanism (a `hooks` feature flag) that does not match
/// the plan's originally assumed `notify = [...]` config array at all.
///
/// `ready()` no longer hard-errors, though: codex is a supported adapter with
/// an honestly degraded capability set (`capabilities()` below is all-false,
/// so no rot score, no turn signal, no injected system prompt, and `provider()`
/// reports "openai: no usage source" until a usage collector exists for it).
/// It is selectable and launchable in the common case (`codex` resolves to a
/// real binary) and also when nothing named `codex` is installed at all --
/// `resolve_program` fails open for that case, so `--agent codex` on a
/// machine without it fails at spawn time with the OS's own "not found",
/// not here. The one launch `ready()` actually refuses is a bare `codex`
/// that resolves via `PATH` to a file this OS cannot execute at all.
#[derive(Debug, Clone)]
pub struct CodexAdapter {
    program: String,
    bin_args: Vec<String>,
    home: Option<PathBuf>,
}

impl CodexAdapter {
    /// `bin` may carry arguments, so `"sh /tmp/stub.sh"` and
    /// `"/usr/bin/env codex"` both work, mirroring `ClaudeAdapter::new`.
    pub fn new(bin: Option<&str>) -> Self {
        let raw = bin.unwrap_or("codex").trim();
        let mut parts = raw.split_whitespace().map(str::to_string);
        let program = parts.next().unwrap_or_else(|| "codex".to_string());
        Self {
            program,
            bin_args: parts.collect(),
            home: None,
        }
    }

    /// Test seam: pins the home directory the transcript path is built from.
    #[cfg(test)]
    pub fn with_home(mut self, home: PathBuf) -> Self {
        self.home = Some(home);
        self
    }

    /// Every command starts here so the program and its leading arguments are
    /// applied uniformly to headless, interactive and distiller invocations.
    ///
    /// SECURITY (FINDING 6, closed): this now mirrors `ClaudeAdapter::base`
    /// exactly -- the program is routed through `super::resolve_program` so an
    /// npm-installed `codex.cmd` shim launches at all on Windows, and
    /// `launches_through_cmd_shim` below reports the same shim shape so a
    /// caller moves the headless prompt onto stdin
    /// (`headless_cmd_stdin`) rather than argv on exactly that launch shape.
    /// There is no `system_prompt_file_flag` override because there is no
    /// verified per-run system-prompt mechanism at all for codex (see
    /// `system_prompt_args` below) -- nothing to force off argv, since
    /// nothing is ever put on it in the first place.
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

/// Codex nests rollout files under `<sessions>/<YYYY>/<MM>/<DD>/`, a depth
/// `SessionRef` cannot predict (it carries only the session id, not the
/// session's start time), so the transcript is found by filename suffix
/// rather than a computed path.
fn find_rollout(dir: &Path, filename_suffix: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_rollout(&path, filename_suffix) {
                return Some(found);
            }
        } else if path.to_string_lossy().ends_with(filename_suffix) {
            return Some(path);
        }
    }
    None
}

impl AgentAdapter for CodexAdapter {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn program(&self) -> &str {
        &self.program
    }

    /// Codex spends an OpenAI account's limits. Nothing collects readings for
    /// it yet, which is exactly why the provider is named: a usage readout
    /// can then say "openai: no usage source" rather than imply zero.
    fn provider(&self) -> &'static str {
        "openai"
    }

    /// Mirrors `ClaudeAdapter::ready` exactly: the one thing that can make
    /// this adapter unusable before it is asked to do anything is a bare
    /// name that *does* resolve (via `PATH`) to a file this OS has no way to
    /// execute. `resolve_program` fails open for the opposite case -- a name
    /// that resolves to nothing at all -- so `--agent codex` succeeds even
    /// when `codex` is not installed anywhere; that case is left to surface
    /// as the OS's own "not found" at spawn time, not caught here. Codex
    /// support is otherwise honestly degraded (see the module doc comment)
    /// but not refused.
    fn ready(&self) -> CtxResult<()> {
        super::resolve_program(&self.program)?;
        Ok(())
    }

    fn detect(&self, command: &[String]) -> bool {
        command
            .first()
            .and_then(|p| Path::new(p).file_name())
            .map(|f| f.to_string_lossy() == "codex")
            .unwrap_or(false)
    }

    /// `codex exec` has no `--session-id` flag (verified): codex always mints
    /// its own session id, so `session` cannot appear in the built command.
    fn headless_cmd(&self, prompt: &str, _session: &SessionId, extra: &[String]) -> Command {
        let mut cmd = self.base();
        cmd.arg("exec").arg(prompt).args(extra);
        cmd
    }

    /// True on a Windows npm install, where `codex` is a `.cmd` shim that
    /// [`super::resolve_program`] routes through `cmd.exe /c`. Exactly
    /// `ClaudeAdapter::launches_through_cmd_shim`'s own body: that is the one
    /// launch shape where a headless prompt on argv would be reparsed by
    /// cmd.exe, so on it the prompt is delivered via stdin instead
    /// (`headless_cmd_stdin`).
    fn launches_through_cmd_shim(&self) -> bool {
        super::launches_through_cmd_shim(&self.program)
    }

    /// `codex exec [PROMPT]` reads its prompt from stdin when the positional
    /// argument is omitted (verified:
    /// docs/superpowers/notes/2026-07-31-codex-cli-facts.md, line 8 -- "If
    /// `[PROMPT]` is omitted (or is `-`), instructions are read from stdin"),
    /// so the same `exec` invocation `headless_cmd` builds works here with
    /// just the positional prompt token dropped. This is what lets
    /// `launches_through_cmd_shim` above move a headless prompt off argv on a
    /// Windows `.cmd` shim launch, exactly like claude's own stdin form.
    fn headless_cmd_stdin(&self, _session: &SessionId, extra: &[String]) -> Option<Command> {
        let mut cmd = self.base();
        cmd.arg("exec").args(extra);
        Some(cmd)
    }

    /// With no subcommand, `codex [PROMPT]` forwards straight to the
    /// interactive CLI (verified via `codex --help`), exactly like claude.
    fn interactive_cmd(&self, initial_prompt: Option<&str>, extra: &[String]) -> Command {
        let mut cmd = self.base();
        if let Some(prompt) = initial_prompt {
            cmd.arg(prompt);
        }
        cmd.args(extra);
        cmd
    }

    fn system_prompt_args(&self, _prompt: &str) -> Vec<String> {
        // No verified per-run mechanism (see
        // docs/superpowers/notes/2026-08-01-system-prompt-injection-facts.md).
        Vec::new()
    }

    /// Spelled out rather than left to the trait default, for the same reason
    /// `system_prompt_args` is empty: the only base layer zirv has is written
    /// around Claude Code's tools (the Agent tool, `.claude/agents`, the
    /// `/code-review` skill), none of which codex has. Instructions about
    /// tools an agent does not have are worse than no instructions.
    fn base_system_prompt(&self) -> Option<&'static str> {
        None
    }

    /// Verified via `codex exec --help` (quoted verbatim in the notes file):
    /// `-m, --model <MODEL>` is a real flag, and the prompt is read from
    /// stdin when none is given as an argument, so the distillation prompt
    /// never hits an argv length limit. `model` is empty when neither the
    /// operator's own config nor `default_distiller_model` (`None` for
    /// codex, since zirv has no verified cheap model name for this lineup)
    /// named one -- omitting `--model` entirely rather than passing an empty
    /// value lets codex's own `~/.codex/config.toml` default apply instead
    /// of zirv guessing a model name that may not exist on the operator's
    /// account.
    ///
    /// `--sandbox read-only` is codex's own analogue of claude's
    /// `--disallowedTools=Write,Edit,Bash,NotebookEdit` pin: it is what backs
    /// `zirv ctx optimize`'s report-only guarantee for a codex judgment
    /// child. Verified against the real installed CLI (`codex exec --help`,
    /// codex-cli 0.105.0): `-s, --sandbox <SANDBOX_MODE>`, possible values
    /// `read-only`/`workspace-write`/`danger-full-access`. It blocks the
    /// class of risk claude's own restriction was verified to close (this
    /// child writing a file, running a shell command, or otherwise mutating
    /// the checkout via a tool) -- but it is not the identical guarantee:
    /// `--sandbox` restricts what codex-*executed shell commands* may touch,
    /// not which of codex's own tools may run at all, and codex's own
    /// AGENTS.md (this repo's equivalent of CLAUDE.md, read into context the
    /// same way) is still embedded in this child's prompt just like claude's
    /// distiller embeds CLAUDE.md -- read-only scopes what the sandbox lets
    /// an executed command do, it does not stop the model from reading that
    /// text or from answering based on it.
    ///
    /// KNOWN RESIDUAL (checked 2026-08-15, see docs/superpowers/notes/
    /// 2026-07-31-codex-cli-facts.md's addendum and its `--ignore-rules`/
    /// `--ignore-user-config` capture): codex-cli 0.146.0's `codex exec
    /// --help` (the brew-installed capture that note quotes verbatim) *does*
    /// document `--ignore-rules` (skip project/user execpolicy `.rules`
    /// files) and `--ignore-user-config` (skip `$CODEX_HOME/config.toml`),
    /// which would close this distiller's remaining "still reads repo/
    /// operator config" gap the same way `guard_cmd_shim_reparse`-style
    /// fixes close others. They are deliberately **not** added here: the
    /// version most operators actually get (`npm install -g @openai/codex`,
    /// verified as `codex-cli 0.105.0` on a real Windows machine, the exact
    /// version this whole function is verified against) has neither flag on
    /// its own `codex exec --help` -- passing either would very likely be an
    /// unrecognized-argument error on 0.105.0, breaking every distiller call
    /// for the common install rather than sandboxing it further. So: a
    /// repo's own `.rules` execpolicy files and the operator's own
    /// `~/.codex/config.toml` still shape this judgment child's behavior
    /// (unlike claude's distiller, whose CLAUDE.md-reading is the one thing
    /// `--disallowedTools` cannot touch either, so the two residuals are not
    /// symmetric: claude's is "still reads the file", codex's is "still
    /// reads the file *and* still honors config it did not ask for"). Add
    /// `--ignore-rules --ignore-user-config` once 0.105.0 (or whatever the
    /// npm-published version is by then) ships them too, verified the same
    /// way `-s, --sandbox` was.
    fn distiller_cmd(&self, model: &str) -> Command {
        let mut cmd = self.base();
        cmd.arg("exec");
        if !model.is_empty() {
            cmd.arg("--model").arg(model);
        }
        cmd.arg("--sandbox").arg("read-only");
        cmd
    }

    /// Codex's own model ladder, top to bottom: `gpt-5.6-sol` (the default
    /// used when no `-m` is given), `gpt-5.6-terra`, `gpt-5.6-luna`, and the
    /// older, hidden `gpt-5.4-mini` -- verified via `codex debug models` in
    /// docs/superpowers/notes/2026-07-31-codex-cli-facts.md's "Cheap model
    /// alias for distillation" section (codex-cli 0.146.0). Matched by
    /// substring on `seat`, lowercased first (same as claude's own ladder)
    /// so a mixed-case seat still lands on the right rung instead of
    /// falling through to the unknown arm. `gpt-5.4-mini` is already the
    /// floor, so it maps to itself; an absent or unrecognised seat
    /// (including one naming another adapter's model, e.g. a claude
    /// orchestrator's own `chat.model`) assumes the top tier -- the
    /// deliberate consequence is that the computed default can then resolve
    /// to a model *more expensive* than the seat actually in use (an
    /// accepted spend-up default; the operator can override it with
    /// `[review]` or by setting `chat.model`).
    fn review_model_below(&self, seat: Option<&str>) -> &'static str {
        let seat = seat.map(str::to_lowercase);
        match seat.as_deref() {
            Some(s) if s.contains("gpt-5.6-sol") => "gpt-5.6-terra",
            Some(s) if s.contains("gpt-5.6-terra") => "gpt-5.6-luna",
            Some(s) if s.contains("gpt-5.6-luna") => "gpt-5.4-mini",
            Some(s) if s.contains("gpt-5.4-mini") => "gpt-5.4-mini",
            _ => "gpt-5.6-terra",
        }
    }

    /// Codex's descriptors come from the repo's **recorded** facts
    /// (docs/superpowers/notes/2026-07-31-codex-cli-facts.md, and the
    /// `distiller_cmd` notes above), not from a live CLI: codex is not
    /// runnable on the machine this was written on, so anything not in those
    /// notes is reported unsupported rather than guessed at.
    ///
    /// `-s, --sandbox <read-only|workspace-write|danger-full-access>` is the
    /// one verified enforcement flag, and it scopes what a codex-*executed
    /// shell command may write*, not whether shell commands may execute or
    /// which of codex's own tools may run (see `docs/obsidian/Concepts/
    /// Untrusted Configuration.md`). That distinction matters per capability:
    ///
    /// - **Repo filesystem writes** are `Degraded` at `Deny`: read-only really
    ///   does block writes inside the repo, just not by denying any tool.
    /// - **Writes outside the repo** are `Degraded` at `Deny` the same way,
    ///   via `--sandbox workspace-write` (writes confined to the workspace).
    /// - **Shell execution** is `Unsupported` at `Deny`: read-only scopes what
    ///   a command may write, it does not stop a command from running at
    ///   all. A worker under `--sandbox read-only` can still run, say,
    ///   `cat ~/.aws/credentials` -- reading is untouched by a write-scoping
    ///   flag, so claiming `Degraded` here would overstate what the sandbox
    ///   does.
    /// - **Approval** is `Unsupported` at `Deny`: the sandbox flag is not
    ///   codex's approval mechanism at all. Only codex's own `approval`
    ///   setting in `~/.codex/config.toml` governs that (it appears in
    ///   `codex exec`'s stdout preamble as `approval: never`), which zirv
    ///   reads and never rewrites -- so an `Ask` stance is
    ///   operator-controlled, but a `Deny` stance has no verified per-run pin
    ///   at all (the only bypass flag verified on the CLI,
    ///   `--dangerously-bypass-approvals-and-sandbox`, only ever *widens*).
    /// - **Network** and **MCP/tool access** have no verified per-run flag.
    ///   `--disable <FEATURE>` is a feature-flag switch, not a tool deny-list.
    /// - **git push / destructive git** has none either, same as claude.
    fn policy_support(
        &self,
        capability: crate::commands::ctx::policy::Capability,
        stance: crate::commands::ctx::policy::Stance,
    ) -> crate::commands::ctx::policy::CapabilityDescriptor {
        use crate::commands::ctx::policy::{Capability, CapabilityDescriptor, Stance};

        const SANDBOX: &str = "--sandbox read-only, which scopes what an executed shell command may write rather \
             than which of codex's own tools may run (recorded facts only -- not verified against \
             a live codex CLI)";
        const WORKSPACE: &str = "--sandbox workspace-write, which keeps writes inside the workspace (documented, not \
             verified against a live codex CLI)";
        const CONFIG: &str = "codex's own `approval` setting in ~/.codex/config.toml, which zirv reads and never \
             rewrites";
        const SHELL_EXEC_DENY_UNSUPPORTED: &str = "--sandbox read-only scopes what a command may write, not whether it may run at \
             all -- a command still executes under it and can read anything the process can \
             reach (e.g. `cat ~/.aws/credentials`); codex has no verified per-run flag that \
             denies shell execution itself";
        const APPROVAL_DENY_UNSUPPORTED: &str = "--sandbox read-only is not codex's approval mechanism; only codex's own \
             `approval` setting in ~/.codex/config.toml governs that, and zirv reads it but \
             never rewrites it";

        match (capability, stance) {
            (Capability::RepoFsWrite, Stance::Deny) => CapabilityDescriptor::degraded(SANDBOX),
            (Capability::OutsideRepoFsWrite, Stance::Deny) => {
                CapabilityDescriptor::degraded(WORKSPACE)
            }
            (Capability::ShellExec, Stance::Deny) => {
                CapabilityDescriptor::unsupported(SHELL_EXEC_DENY_UNSUPPORTED)
            }
            (Capability::Approval, Stance::Deny) => {
                CapabilityDescriptor::unsupported(APPROVAL_DENY_UNSUPPORTED)
            }
            (Capability::ShellExec | Capability::Approval, Stance::Ask) => {
                CapabilityDescriptor::operator_controlled(CONFIG)
            }
            // Network, MCP/tool access, git operations, and every `Ask` stance
            // codex has no verified mechanism for -- see this method's own doc.
            _ => CapabilityDescriptor::advisory_only(),
        }
    }

    fn launch_prefix_len(&self) -> usize {
        1 + self.bin_args.len()
    }

    fn transcript_path(&self, session: &SessionRef) -> PathBuf {
        let sessions_root = self.home_dir().join(".codex").join("sessions");
        let suffix = format!("-{}.jsonl", session.id);
        find_rollout(&sessions_root, &suffix)
            .unwrap_or_else(|| sessions_root.join(format!("rollout{suffix}")))
    }

    fn parse_events(&self, _jsonl: &str) -> Vec<NormalizedEvent> {
        Vec::new()
    }

    fn structural_context(&self, _jsonl: &str, _last_n: usize) -> StructuralContext {
        StructuralContext::default()
    }

    fn compact_command(&self) -> Option<&'static str> {
        None
    }

    fn quit_sequence(&self) -> &'static str {
        "/quit\r"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            marker_signal: false,
            token_usage: false,
            turn_signal: false,
            system_prompt: false,
            // D: parse_events/structural_context are stubbed to empty/default
            // (out of scope, issue #11) -- an honest `false` here is what
            // keeps score.rs from reading that emptiness as a real
            // `Healthy`/`0` verdict. See the doc comment on `Capabilities::
            // events`.
            events: false,
        }
    }

    /// Verified (docs/superpowers/notes/2026-07-31-codex-cli-facts.md, line
    /// 139): `-m, --model <MODEL>` is present on top-level `codex --help`
    /// with the same description as on `codex exec --help`, so the
    /// interactive launch this feeds (`interactive_cmd`) accepts it too.
    fn model_args(&self, model: &str) -> Vec<String> {
        vec!["--model".to_string(), model.to_string()]
    }

    fn register_turn_signal(&self, _session: &SessionRef, _socket: &Path) -> TurnSignalSetup {
        TurnSignalSetup {
            env: Vec::new(),
            instructions: String::new(),
        }
    }

    // No `resume_args` override: codex's own resume story (a `--last`/
    // session-id flag for the interactive launch) is unverified against the
    // real CLI, unlike `model_args` above. The trait default (`None`) is
    // correct here -- `dash::roster::restore_argv` falls back to a plain
    // prompt-carrying relaunch for this adapter rather than a guessed flag.
    //
    // No `session_pin_args` override either, for the same reason and one
    // stronger: `headless_cmd` above already records the verified fact that
    // codex has no `--session-id` flag at all and always mints its own id, so
    // there is nothing to pin an interactive dashboard pane with. The trait
    // default (empty) is what "no verified mechanism" has to ship as.
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::adapters::{AgentAdapter, select};
    use crate::commands::ctx::config::CtxConfig;

    /// I: `super::super::built_args` (`adapters/mod.rs`) takes the program
    /// string rather than the whole adapter, since `program` is private to
    /// this module -- this thin wrapper is what lets every call site below
    /// keep passing `&adapter` unchanged. Mirrors `claude.rs`'s own wrapper.
    fn built_args(adapter: &CodexAdapter, cmd: &Command) -> Vec<String> {
        super::super::built_args(&adapter.program, cmd)
    }

    #[test]
    fn codex_detects_its_own_binary() {
        let adapter = CodexAdapter::new(None);
        assert!(adapter.detect(&["/usr/local/bin/codex".to_string()]));
        assert!(!adapter.detect(&["/usr/local/bin/claude".to_string()]));
    }

    #[test]
    fn codex_has_no_marker_signal() {
        let caps = CodexAdapter::new(None).capabilities();
        assert!(!caps.marker_signal, "the spec gives codex no marker signal");
    }

    #[test]
    fn codex_ships_without_injection_until_a_mechanism_is_verified() {
        let adapter = CodexAdapter::new(None);
        assert!(
            adapter.system_prompt_args("be consistent").is_empty(),
            "no verified mechanism means no arguments, not a guessed flag"
        );
        assert!(!adapter.capabilities().system_prompt);
        assert_eq!(
            adapter.user_system_prompt_flag(),
            None,
            "nothing to merge when there is no flag at all"
        );
    }

    /// Codex is now supported out of the box: `--agent codex` selects it
    /// directly wherever the `codex` program resolves, the same contract
    /// `ClaudeAdapter::ready` already gives claude.
    #[test]
    fn selecting_codex_succeeds_once_the_binary_resolves() {
        let adapter =
            select(Some("codex"), &[], &CtxConfig::default()).expect("codex resolves and is ready");
        assert_eq!(adapter.name(), "codex");
    }

    /// Mirrors `ClaudeAdapter::ready`'s own contract exactly:
    /// `resolve_program` is the only thing that can fail it, and a bare
    /// `"codex"` that never resolves to anything at all is not an error here
    /// (the OS raises its own "not found" at spawn time instead).
    #[test]
    fn ready_succeeds_for_the_default_program_name() {
        assert!(CodexAdapter::new(None).ready().is_ok());
    }

    /// argv auto-detection now selects codex directly instead of refusing:
    /// once `ready()` no longer hard-errors, `select`'s detection arm has
    /// nothing left to refuse on for a plain `codex ...` command.
    #[test]
    fn detecting_codex_argv_selects_codex() {
        let cmd = vec!["codex".to_string(), "exec".to_string(), "do it".to_string()];
        let adapter = select(None, &cmd, &CtxConfig::default()).expect("codex is ready");
        assert_eq!(adapter.name(), "codex");
    }

    /// Verified via `codex exec --help`: there is no `--session-id` flag, so
    /// the session parameter cannot appear in the built command at all.
    #[test]
    fn headless_cmd_uses_exec_and_has_no_session_flag() {
        let adapter = CodexAdapter::new(Some("/tmp/fake-codex"));
        let cmd = adapter.headless_cmd(
            "do the work",
            &SessionId::parse("abc"),
            &["--model".to_string(), "gpt-5.6-luna".to_string()],
        );
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(cmd.get_program().to_string_lossy(), "/tmp/fake-codex");
        assert_eq!(
            args,
            vec![
                "exec".to_string(),
                "do the work".to_string(),
                "--model".to_string(),
                "gpt-5.6-luna".to_string(),
            ],
            "codex exec takes no session flag; codex mints its own session id"
        );
    }

    /// Verified via `codex --help`: with no subcommand, the prompt goes
    /// straight to the interactive CLI, exactly like claude's positional form.
    #[test]
    fn interactive_cmd_passes_the_initial_prompt_positionally_with_no_subcommand() {
        let adapter = CodexAdapter::new(None);
        let with = adapter.interactive_cmd(Some("resume this"), &[]);
        assert_eq!(built_args(&adapter, &with), vec!["resume this".to_string()]);

        let without = adapter.interactive_cmd(None, &["--last".to_string()]);
        assert_eq!(built_args(&adapter, &without), vec!["--last".to_string()]);
    }

    /// Verified via `codex exec --help` (quoted verbatim in the notes file):
    /// `-m, --model <MODEL>` is a real flag, and the prompt is read from
    /// stdin when omitted, so the distiller never needs an argv prompt.
    #[test]
    fn distiller_cmd_uses_exec_with_a_cheap_model_and_reads_stdin() {
        let adapter = CodexAdapter::new(None);
        let cmd = adapter.distiller_cmd("gpt-5.6-luna");
        assert_eq!(
            built_args(&adapter, &cmd),
            vec![
                "exec".to_string(),
                "--model".to_string(),
                "gpt-5.6-luna".to_string(),
                "--sandbox".to_string(),
                "read-only".to_string(),
            ]
        );
    }

    /// C: an empty `model` (no operator config, and `default_distiller_
    /// model` is `None` for codex) must omit `--model` entirely rather than
    /// pass an empty value -- codex's own `~/.codex/config.toml` default
    /// then applies instead of zirv guessing a model name.
    #[test]
    fn distiller_cmd_omits_the_model_flag_when_none_is_given() {
        let adapter = CodexAdapter::new(None);
        let cmd = adapter.distiller_cmd("");
        let args = built_args(&adapter, &cmd);
        assert!(
            !args.iter().any(|a| a == "--model"),
            "no model resolved means no --model flag at all: {args:?}"
        );
        assert_eq!(
            args,
            vec![
                "exec".to_string(),
                "--sandbox".to_string(),
                "read-only".to_string()
            ]
        );
    }

    /// C: codex has no verified cheap-model default of its own -- a
    /// hardcoded model name is specific to claude's lineup.
    #[test]
    fn codex_has_no_default_distiller_model() {
        assert_eq!(CodexAdapter::new(None).default_distiller_model(), None);
    }

    /// Codex has no adapter-owned hard default for a delegated worker
    /// either: its own CLI/config default applies untouched when the
    /// operator has not set `worker.codex` -- see
    /// `adapters::resolve_worker_model`.
    #[test]
    fn codex_has_no_default_worker_model() {
        assert_eq!(CodexAdapter::new(None).default_worker_model(), None);
    }

    /// Same "nothing verified to guess" answer for the role layers: codex
    /// contributes neither an orchestrator nor a worker layer of its own, so
    /// `prompt::with_adapter_layer` splices nothing in for either role rather
    /// than handing codex text written for claude's tools.
    #[test]
    fn codex_contributes_no_worker_layer_of_its_own() {
        assert_eq!(CodexAdapter::new(None).worker_system_prompt(), None);
    }

    /// The codex ladder, top to bottom: `gpt-5.6-sol` (the default when no
    /// `-m` is given), `gpt-5.6-terra`, `gpt-5.6-luna`, `gpt-5.4-mini` --
    /// verified via `codex debug models` in docs/superpowers/notes/
    /// 2026-07-31-codex-cli-facts.md's "Cheap model alias for distillation"
    /// section, sourced from a 0.146.0 capture; codex is not installed on
    /// this machine to re-verify the catalog against 0.105.0 (the version
    /// most operators actually get, per `distiller_cmd`'s own doc comment),
    /// so treat this ladder as unverified for that version specifically --
    /// the cited note documents the `--ignore-rules`/`--ignore-user-config`
    /// gap only, not this catalog. `review_model_below` returns the tier one
    /// below `seat`; an unknown or absent seat assumes the top tier
    /// (`gpt-5.6-sol`), and `gpt-5.4-mini` (already the floor) maps to
    /// itself.
    #[test]
    fn review_model_below_walks_the_codex_ladder() {
        let adapter = CodexAdapter::new(None);
        assert_eq!(
            adapter.review_model_below(Some("gpt-5.6-sol")),
            "gpt-5.6-terra"
        );
        assert_eq!(
            adapter.review_model_below(Some("gpt-5.6-terra")),
            "gpt-5.6-luna"
        );
        assert_eq!(
            adapter.review_model_below(Some("gpt-5.6-luna")),
            "gpt-5.4-mini"
        );
        assert_eq!(
            adapter.review_model_below(Some("gpt-5.4-mini")),
            "gpt-5.4-mini"
        );
        assert_eq!(
            adapter.review_model_below(None),
            "gpt-5.6-terra",
            "no seat configured: assume the top tier"
        );
        assert_eq!(
            adapter.review_model_below(Some("claude-fable-5")),
            "gpt-5.6-terra",
            "a seat naming another adapter's model is unrecognised: assume the top tier"
        );
    }

    /// Seat matching must be case-insensitive: a mixed-case seat must land
    /// on the same ladder rung as its lowercase form, not fall through to
    /// the unknown arm and assume the top tier.
    #[test]
    fn review_model_below_matches_the_seat_case_insensitively() {
        let adapter = CodexAdapter::new(None);
        assert_eq!(
            adapter.review_model_below(Some("GPT-5.6-Sol")),
            "gpt-5.6-terra"
        );
        assert_eq!(
            adapter.review_model_below(Some("Gpt-5.6-Terra")),
            "gpt-5.6-luna"
        );
        assert_eq!(
            adapter.review_model_below(Some("GPT-5.6-LUNA")),
            "gpt-5.4-mini"
        );
        assert_eq!(
            adapter.review_model_below(Some("GPT-5.4-Mini")),
            "gpt-5.4-mini"
        );
    }

    /// B: `--sandbox read-only` (verified against `codex exec --help` on
    /// codex-cli 0.105.0: `-s, --sandbox <SANDBOX_MODE>`, possible values
    /// `read-only`/`workspace-write`/`danger-full-access`) is the pin behind
    /// `zirv ctx optimize`'s report-only guarantee for a codex judgment
    /// child, the same role claude's `--disallowedTools=...` plays. Pinned
    /// as its own test so a future edit to `distiller_cmd` cannot drop the
    /// flag without a test failing here specifically.
    #[test]
    fn the_distiller_is_pinned_to_the_read_only_sandbox() {
        let adapter = CodexAdapter::new(None);
        let cmd = adapter.distiller_cmd("gpt-5.6-luna");
        let args = built_args(&adapter, &cmd);
        assert!(
            args.windows(2).any(|w| w == ["--sandbox", "read-only"]),
            "the distiller must be pinned to codex's own read-only sandbox: {args:?}"
        );
    }

    /// A multi-word agent bin (`ZIRV_CTX_AGENT_BIN="sh /tmp/stub.sh"`) must work
    /// the same way it does for claude, so `ZIRV_CTX_AGENT_BIN` behaves
    /// identically across adapters.
    #[test]
    fn a_multi_word_agent_bin_is_split_across_every_command_kind() {
        let adapter = CodexAdapter::new(Some("sh /tmp/stub.sh"));

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
                "exec".to_string(),
                "go".to_string()
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
    }

    /// Codex nests rollouts under a date directory that `SessionRef` cannot
    /// predict (it carries only the id, not the session's start time), so
    /// `transcript_path` must scan for the id rather than compute the path.
    #[test]
    fn transcript_path_scans_the_dated_sessions_tree_for_the_session_id() {
        let home = tempfile::tempdir().expect("tempdir");
        let day_dir = home.path().join(".codex/sessions/2026/07/31");
        std::fs::create_dir_all(&day_dir).expect("mkdir");
        let expected =
            day_dir.join("rollout-2026-07-31T20-16-08-11111111-2222-4333-8444-555555555555.jsonl");
        std::fs::write(&expected, "").expect("write");

        let adapter = CodexAdapter::new(None).with_home(home.path().to_path_buf());
        let session = SessionRef {
            id: SessionId::parse("11111111-2222-4333-8444-555555555555"),
            cwd: std::path::PathBuf::from("/work/repo"),
        };
        assert_eq!(adapter.transcript_path(&session), expected);
    }

    /// `-m, --model <MODEL>` is verified on top-level `codex --help` too (see
    /// the `model_args` doc comment), so the dashboard's orchestrator pane
    /// can select a model on codex exactly as it does on claude.
    #[test]
    fn model_args_uses_the_verified_flag() {
        let adapter = CodexAdapter::new(None);
        assert_eq!(
            adapter.model_args("gpt-5.6-sol"),
            vec!["--model".to_string(), "gpt-5.6-sol".to_string()]
        );
    }

    /// SECURITY (FINDING 6, closed): an npm-installed `codex` on Windows is a
    /// `.cmd` shim, exactly the shape `ClaudeAdapter`'s own equivalent test
    /// (`a_cmd_shim_is_launched_through_cmd_exe_with_its_arguments_intact`)
    /// covers. `base()` must route it through `cmd.exe /c`, and
    /// `launches_through_cmd_shim` must report that shape so a caller moves
    /// the headless prompt onto stdin instead of leaving it on the reparsed
    /// argv.
    #[cfg(windows)]
    #[test]
    fn a_cmd_shim_codex_is_launched_through_cmd_exe_and_reports_the_shim_shape() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shim = dir.path().join("codex.cmd");
        std::fs::write(&shim, "@echo off\r\n").expect("write shim");

        let adapter = CodexAdapter::new(Some(&shim.display().to_string()));
        assert!(
            adapter.launches_through_cmd_shim(),
            "a .cmd resolution must be reported as the shim shape"
        );

        let cmd = adapter.interactive_cmd(Some("resume this"), &["--last".to_string()]);
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
                "--last".to_string(),
            ]
        );
    }

    /// FIX B, extended to codex: on the shim launch shape the headless prompt
    /// must never be the `exec <prompt>` argv token cmd.exe would reparse.
    /// `headless_cmd_stdin` omits the positional prompt entirely, relying on
    /// codex's own verified stdin fallback (`codex exec` with `[PROMPT]`
    /// omitted reads from stdin).
    #[test]
    fn headless_cmd_stdin_omits_the_prompt_and_reads_it_from_stdin() {
        let adapter = CodexAdapter::new(Some("/tmp/fake-codex"));
        let cmd = adapter
            .headless_cmd_stdin(
                &SessionId::parse("abc"),
                &["--model".to_string(), "gpt-5.6-luna".to_string()],
            )
            .expect("codex has a verified stdin form");
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(
            args,
            vec![
                "exec".to_string(),
                "--model".to_string(),
                "gpt-5.6-luna".to_string(),
            ],
            "no positional prompt token: the prompt travels on stdin"
        );
    }

    /// A directly executable program (no `.cmd` extension, and not one that
    /// resolves to anything on `PATH` at all) is never the shim shape, off
    /// Windows or on it -- mirrors claude's own
    /// `a_program_is_spawned_exactly_as_written_off_windows` /
    /// `launches_through_cmd_shim` contract. Deliberately not the bare
    /// `"codex"` default: on a machine with a real npm-installed `codex.cmd`
    /// on `PATH`, that bare name legitimately *does* resolve to the shim
    /// shape, which is the behavior under test elsewhere in this file.
    #[test]
    fn a_direct_program_never_reports_the_shim_shape() {
        let adapter = CodexAdapter::new(Some("/tmp/fake-codex"));
        assert!(!adapter.launches_through_cmd_shim());
    }
}
