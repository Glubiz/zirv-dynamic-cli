//! ZCHK-HOOK-WINDOWS: every hook command `zirv setup apply` installs into a
//! harness's own hook config (`HARNESS_HOOKS`/`CLAUDE_ONLY_HOOKS` in
//! `commands/setup.rs`) actually runs unmodified on Windows -- CD ships a
//! Windows binary, and every installed hook is a plain, unquoted `zirv ...`
//! invocation spawned directly (never through a shell), so a bash-only
//! construct (`&&`, a pipe, a subshell, a leading `~/`, an `export`) would
//! silently never fire there. Calls the in-binary hook tables directly, the
//! repo argument is irrelevant -- same "assert the invariant, not an
//! installed-binary probe" posture as the argv checks.

use std::path::Path;

use super::BuiltinCheckResult;

pub const ID: &str = "ZCHK-HOOK-WINDOWS";
const PROVES: &str = "every command in setup::HARNESS_HOOKS/CLAUDE_ONLY_HOOKS is a plain zirv \
     invocation with no bash-only syntax, so it runs unmodified on the Windows binary CD ships";
const FIX: &str = "rewrite the new hook command as a plain `zirv <verb> ...` invocation with no \
     shell operators (&&, |, $(...), backticks, a leading ~/, export); if the hook genuinely \
     needs shell logic, add a dedicated `zirv ctx hook <name>` subcommand instead of shelling out";
const ORIGIN: &str = "setup regressions -- Ruflo round-2 audit-plugin-hooks-cross-platform.mjs \
     precedent (POSIX-only hook commands need a referenced Windows shim), issue #276";

/// Bash-only constructs that a command spawned directly (never through a
/// shell -- `Command::new(program).arg(...)`, exactly how `setup.rs` builds
/// these) would either fail on outright on Windows or silently mean
/// something different: shell operators cmd.exe/PowerShell either lack or
/// spell differently, POSIX path/env conventions, and `.sh`-only scripts.
const BASH_ONLY_MARKERS: &[&str] = &["&&", "||", "|", "$(", "`", "export ", ".sh", "~/"];

pub fn run(_repo: &Path) -> BuiltinCheckResult {
    let mut commands: Vec<&str> = crate::commands::setup::HARNESS_HOOKS
        .iter()
        .map(|(_, _, command)| *command)
        .collect();
    commands.extend(
        crate::commands::setup::CLAUDE_ONLY_HOOKS
            .iter()
            .map(|(_, _, command)| *command),
    );
    evaluate(&commands)
}

fn evaluate(commands: &[&str]) -> BuiltinCheckResult {
    let mut problems = Vec::new();
    for command in commands {
        for marker in BASH_ONLY_MARKERS {
            if command.contains(marker) {
                problems.push(format!("`{command}` contains bash-only syntax `{marker}`"));
            }
        }
    }

    if problems.is_empty() {
        BuiltinCheckResult::pass(
            ID,
            PROVES,
            FIX,
            ORIGIN,
            format!(
                "{} installed hook command(s) checked, all Windows-safe",
                commands.len()
            ),
        )
    } else {
        BuiltinCheckResult::fail(ID, PROVES, FIX, ORIGIN, problems.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_real_hook_tables_are_windows_safe() {
        let result = run(Path::new("."));
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Pass,
            "{result:?}"
        );
    }

    #[test]
    fn a_bash_only_construct_fails_the_check() {
        let commands = ["zirv ctx hook stop && echo done"];
        let result = evaluate(&commands);
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Fail,
            "{result:?}"
        );
        assert!(result.details.contains("&&"), "{result:?}");
    }

    #[test]
    fn a_plain_zirv_invocation_passes() {
        let commands = ["zirv ctx hook stop", "zirv ctx safety check"];
        let result = evaluate(&commands);
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Pass,
            "{result:?}"
        );
    }
}
