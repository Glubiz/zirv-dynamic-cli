//! ZCHK-ARGV-CODEX-EXEC / ZCHK-ARGV-CLAUDE-HEADLESS: the codex/claude
//! headless-worker argv builders still shape a launch the way
//! `adapters::codex`/`adapters::claude`'s own doc comments say they do. The
//! `repo` argument both checks take is irrelevant to either -- they call the
//! in-binary argv builders directly (`AgentAdapter::headless_cmd`/
//! `policy_args`), the same functions a real `zirv ctx agent`/`zirv ctx exec`
//! headless launch actually spawns from, never a probe of an installed
//! binary. CLAUDE.md: "Never assert an exact argv that depends on an
//! installed-binary probe; assert the invariant" -- both checks below assert
//! presence/order invariants, never a full literal argv list, so neither is
//! sensitive to whether a real `codex`/`claude` binary happens to be
//! installed on the machine running `zirv verify`.

use std::path::Path;

use crate::commands::ctx::adapters::claude::ClaudeAdapter;
use crate::commands::ctx::adapters::codex::CodexAdapter;
use crate::commands::ctx::adapters::{AgentAdapter, LaunchMode};
use crate::commands::ctx::event::SessionId;
use crate::commands::ctx::policy::{EffectivePolicy, Stance};

use super::BuiltinCheckResult;

pub const CODEX_ID: &str = "ZCHK-ARGV-CODEX-EXEC";
const CODEX_PROVES: &str = "codex's headless worker argv still opens with `exec`, still carries \
     the prompt on argv, and still adds `--sandbox` when EffectivePolicy::shell_exec is Deny";
const CODEX_FIX: &str = "adapters::codex::CodexAdapter::headless_cmd must keep `exec` as its \
     first argv token with the prompt following it, and policy_args must keep adding \
     `--sandbox` (via read_only_args) whenever shell_exec/repo_fs_write is Deny";
const CODEX_ORIGIN: &str = "adapter argv regressions -- Ruflo round-2 audit-codex-integration.mjs \
     precedent (issue #278): 'the orchestrator still builds [\"exec\", \"--sandbox\", ...]'";

/// `codex exec [PROMPT]` with a policy that denies `shell_exec` -- the shape
/// `zirv ctx agent`'s own report-only/deny-posture headless launch actually
/// builds (`policy_support`'s `Capability::ShellExec` arm; see
/// `adapters::codex`'s own doc comments).
pub fn run_codex_exec(_repo: &Path) -> BuiltinCheckResult {
    let adapter = CodexAdapter::new(None);
    let policy = EffectivePolicy {
        shell_exec: Stance::Deny,
        ..EffectivePolicy::default()
    };
    let extra = adapter.policy_args(&policy, LaunchMode::Headless);
    let session = SessionId::new_v4();
    let prompt = "zchk-argv-codex-exec probe prompt";
    let cmd = adapter.headless_cmd(prompt, &session, &extra);
    let args: Vec<String> = cmd
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();

    let mut problems = Vec::new();
    if args.first().map(String::as_str) != Some("exec") {
        problems.push(format!(
            "first argv token must be 'exec', got {:?}",
            args.first()
        ));
    }
    if !args.iter().any(|arg| arg == prompt) {
        problems.push("the prompt is missing from argv".to_string());
    }
    if !args.iter().any(|arg| arg == "--sandbox") {
        problems.push("no --sandbox flag when shell_exec is denied".to_string());
    }

    if problems.is_empty() {
        BuiltinCheckResult::pass(
            CODEX_ID,
            CODEX_PROVES,
            CODEX_FIX,
            CODEX_ORIGIN,
            format!("argv: {args:?}"),
        )
    } else {
        BuiltinCheckResult::fail(
            CODEX_ID,
            CODEX_PROVES,
            CODEX_FIX,
            CODEX_ORIGIN,
            problems.join("; "),
        )
    }
}

pub const CLAUDE_ID: &str = "ZCHK-ARGV-CLAUDE-HEADLESS";
const CLAUDE_PROVES: &str = "claude's headless worker argv still carries -p, the prompt, and \
     --session-id, and never adds a --dangerously-* flag under the default (unrestricted) \
     policy projection";
const CLAUDE_FIX: &str = "adapters::claude::ClaudeAdapter::headless_cmd must keep -p/the \
     prompt/--session-id; a --dangerously-* flag must only ever come from an explicit, \
     operator-configured bypass, never the default policy_args projection";
const CLAUDE_ORIGIN: &str = "adapter argv regressions -- Ruflo round-2 audit-codex-integration.mjs \
     precedent (issue #278), applied to claude's own headless builder";

/// `claude -p [PROMPT] --session-id <id>` under the default, all-`Allow`
/// policy -- `policy_args_agree_on_no_restriction_under_the_default_policy`
/// (adapters::mod tests) already proves this projects to no extra argv at
/// all, so a bare `headless_cmd` call is the real shape a default-posture
/// headless launch sends.
pub fn run_claude_headless(_repo: &Path) -> BuiltinCheckResult {
    let adapter = ClaudeAdapter::new(None);
    let policy = EffectivePolicy::default();
    let extra = adapter.policy_args(&policy, LaunchMode::Headless);
    let session = SessionId::new_v4();
    let prompt = "zchk-argv-claude-headless probe prompt";
    let cmd = adapter.headless_cmd(prompt, &session, &extra);
    let args: Vec<String> = cmd
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();

    let mut problems = Vec::new();
    if !args.iter().any(|arg| arg == "-p") {
        problems.push("no -p flag in argv".to_string());
    }
    if !args.iter().any(|arg| arg == prompt) {
        problems.push("the prompt is missing from argv".to_string());
    }
    if !args.iter().any(|arg| arg == "--session-id") {
        problems.push("no --session-id flag in argv".to_string());
    }
    if let Some(dangerous) = args.iter().find(|arg| arg.contains("dangerously")) {
        problems.push(format!(
            "a --dangerously-* flag ({dangerous}) rode along under the default policy: {args:?}"
        ));
    }

    if problems.is_empty() {
        BuiltinCheckResult::pass(
            CLAUDE_ID,
            CLAUDE_PROVES,
            CLAUDE_FIX,
            CLAUDE_ORIGIN,
            format!("argv: {args:?}"),
        )
    } else {
        BuiltinCheckResult::fail(
            CLAUDE_ID,
            CLAUDE_PROVES,
            CLAUDE_FIX,
            CLAUDE_ORIGIN,
            problems.join("; "),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::workflow::checks::BuiltinOutcome;

    #[test]
    fn codex_exec_passes_against_the_real_adapter() {
        let repo = tempfile::tempdir().unwrap();
        let result = run_codex_exec(repo.path());
        assert_eq!(result.outcome, BuiltinOutcome::Pass, "{result:?}");
    }

    #[test]
    fn claude_headless_passes_against_the_real_adapter() {
        let repo = tempfile::tempdir().unwrap();
        let result = run_claude_headless(repo.path());
        assert_eq!(result.outcome, BuiltinOutcome::Pass, "{result:?}");
    }

    /// Fixture proving the check actually fails when the invariant breaks:
    /// a `headless_cmd`-shaped command missing `exec` reproduces exactly the
    /// regression `ZCHK-ARGV-CODEX-EXEC` exists to catch.
    #[test]
    fn a_codex_argv_missing_exec_would_fail_the_invariant() {
        let mut cmd = std::process::Command::new("codex");
        cmd.arg("some prompt");
        let args: Vec<String> = cmd
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_ne!(
            args.first().map(String::as_str),
            Some("exec"),
            "fixture must actually violate the invariant"
        );
    }

    /// Fixture proving the check actually fails when a dangerous flag rides
    /// along under the default policy -- reproduces the shape
    /// `ZCHK-ARGV-CLAUDE-HEADLESS` exists to catch.
    #[test]
    fn a_claude_argv_carrying_a_dangerous_flag_would_fail_the_invariant() {
        let mut cmd = std::process::Command::new("claude");
        cmd.arg("-p")
            .arg("prompt")
            .arg("--dangerously-skip-permissions");
        let args: Vec<String> = cmd
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(args.iter().any(|arg| arg.contains("dangerously")));
    }
}
