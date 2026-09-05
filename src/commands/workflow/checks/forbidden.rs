//! ZCHK-FORBIDDEN-WIDENING: every `ctx.toml` key `config.rs`'s own `ENV_MAP`
//! table already knows an operator can override is either `REPO_FORBIDDEN`
//! or on this file's explicit narrow-only allow-list below. A brand new
//! `ENV_MAP` entry landing without touching either list fails this check --
//! today the untrusted-config posture ("repo-owned surfaces may only
//! NARROW", CLAUDE.md) is a checklist item a reviewer has to remember; this
//! makes it a check.
//!
//! `ENV_MAP` is not a complete enumeration of every key `config.rs` parses --
//! a handful of list-shaped keys (`workflow.check_env_passthrough`,
//! `sandbox.extra_allow`, `dash.workdir_roots`, ...) are read via bespoke
//! comma-separated env parsing instead of `EnvKind`'s scalar dispatch, so
//! they never appear in `ENV_MAP` at all -- see `config.rs`'s own comment
//! next to `EnvKind` ("has no list-shaped variant"). Every key this check
//! actually knows about today is REPO_FORBIDDEN already (checked directly,
//! not derived from `ENV_MAP`), so this gap does not currently hide
//! anything; a *future* list-shaped key that is both new and NOT
//! REPO_FORBIDDEN would not be caught by this check. Per issue #276's own
//! escape hatch ("if no complete enumeration exists, build one ... and say
//! so") -- this is that disclosure.

use std::collections::BTreeSet;
use std::path::Path;

use regex::Regex;

use super::BuiltinCheckResult;

pub const ID: &str = "ZCHK-FORBIDDEN-WIDENING";
const PROVES: &str = "every ctx.toml key config.rs's own ENV_MAP table enumerates is classified \
     as REPO_FORBIDDEN or explicitly narrow-only (workflow::checks::forbidden::\
     NARROW_ONLY_ALLOWLIST)";
const FIX: &str = "classify the new key: add its dotted path to config.rs's REPO_FORBIDDEN table \
     (operator-only -- the default for anything a repo checkout could widen) or, if a repo \
     checkout setting it can only ever narrow behavior, to \
     workflow::checks::forbidden::NARROW_ONLY_ALLOWLIST with a comment saying why";
const ORIGIN: &str = "untrusted-config posture (CLAUDE.md: repo-owned surfaces may only narrow) \
     was a checklist item, not a check -- issue #276";

/// Dotted `ctx.toml` key paths a repository checkout MAY set. Two different
/// reasons land a key here, and both are safe for the identical reason a
/// `REPO_FORBIDDEN` key is refused -- the repo cannot widen zirv's OWN
/// authority over the machine/process it did not already have:
///
/// - an explicit narrow-only fold (`config.rs`'s own doc comment on the
///   field documents the fold: a repo may disable a feature, lower a
///   ceiling, or tighten a stance, never the reverse); or
/// - a plain preference/timing/budget knob with no capability behind it at
///   all (how long to wait, how many entries to keep, how sensitive a
///   scoring threshold is) -- repo-settable by original design, not merely
///   overlooked.
///
/// Every entry below is reviewed alongside the PR that adds it; a new key
/// landing in `config.rs`'s `ENV_MAP` with neither an explicit narrow-only
/// fold nor a plain-preference justification belongs in `REPO_FORBIDDEN`
/// instead, not here.
pub const NARROW_ONLY_ALLOWLIST: &[&str] = &[
    // Narrow-only folds (config.rs's own doc comment on each field states
    // the fold in full):
    "workflow.deploy.minimum_tier", // folds as max(home, repo): repo may only RAISE its own floor.
    "supervise.orchestrator_writes", // repo may only tighten allow -> advise -> deny.
    "fallback.adaptive_delegation", // repo may only disable, per its own doc comment.
    "fallback.auto_orchestrator_rollover", // same AND-fold as adaptive_delegation.
    // `chat.model` is deliberately not `REPO_FORBIDDEN` (see [[Untrusted
    // Configuration]] / README.md's own trust-boundary intro): the one model
    // key a repo may set at all, because a wrong model choice costs money,
    // not authority.
    "chat.model",
    // Plain preference/timing/budget knobs: no capability, no vendor
    // account, no filesystem/network/shell reach behind any of these --
    // adjusting them changes how zirv behaves for THIS repo, never what it
    // is allowed to do.
    "chrome.banner",
    "chrome.bar",
    "compact_advisory.min_reclaim_tokens",
    "compact_advisory.window_fraction",
    "dash.idle_quiet_ms",
    "fallback.enabled",
    "fallback.min_candidate_headroom_pct",
    "fallback.predictive_headroom_pct",
    "fallback.small_task_max_tokens",
    "fallback.small_task_max_tool_calls",
    "fallback.unknown_headroom_pct",
    "handoff.timeout_secs",
    "mail.keep",
    "mail.max_message_bytes",
    "optimize.enabled",
    "optimize.sessions_sampled",
    "pace.enabled",
    "pace.fallback_delay_secs",
    "pace.five_hour_budget_tokens",
    "pace.jitter_secs",
    "pace.max_percent",
    "pace.max_wait_secs",
    "pace.seven_day_budget_tokens",
    "pace.soft_percent",
    "pace.wait_slack_secs",
    "score.marker",
    "score.min_turns",
    "score.window",
    "supervise.interval_secs",
    "supervise.loop_backoff_ceiling_secs", // #311: repo may only lower the self-pacing ceiling (min-merge).
    "supervise.max_cycle_secs",
    "supervise.max_failures",
    "supervise.max_nudges",
    "supervise.max_restarts",
    "supervise.poll_ms",
    "worker.deny_network", // narrow-only fold: repo may only turn network OFF.
    "worker.max_depth",    // narrow-only fold: repo may only lower the depth cap.
    "wrap.debounce_ms",
    "wrap.inject_timeout_ms",
];

pub fn run(repo: &Path) -> BuiltinCheckResult {
    let path = repo.join("src/commands/ctx/config.rs");
    let source = match std::fs::read_to_string(&path) {
        Ok(source) => source,
        Err(err) => {
            return BuiltinCheckResult::inconclusive(
                ID,
                PROVES,
                FIX,
                ORIGIN,
                format!("cannot read {}: {err}", path.display()),
            );
        }
    };

    let env_map_paths = match extract_table_paths(&source, "const ENV_MAP") {
        Some(paths) if !paths.is_empty() => paths,
        _ => {
            return BuiltinCheckResult::inconclusive(
                ID,
                PROVES,
                FIX,
                ORIGIN,
                format!(
                    "could not locate/parse config.rs's ENV_MAP table -- its shape changed \
                     since this check was written ({})",
                    path.display()
                ),
            );
        }
    };
    let repo_forbidden_paths = match extract_table_paths(&source, "const REPO_FORBIDDEN") {
        Some(paths) if !paths.is_empty() => paths,
        _ => {
            return BuiltinCheckResult::inconclusive(
                ID,
                PROVES,
                FIX,
                ORIGIN,
                format!(
                    "could not locate/parse config.rs's REPO_FORBIDDEN table -- its shape \
                     changed since this check was written ({})",
                    path.display()
                ),
            );
        }
    };

    let forbidden_set: BTreeSet<&str> = repo_forbidden_paths.iter().map(String::as_str).collect();
    let allow_set: BTreeSet<&str> = NARROW_ONLY_ALLOWLIST.iter().copied().collect();

    let mut unclassified: Vec<String> = env_map_paths
        .iter()
        .filter(|path| {
            !is_repo_forbidden(path, &forbidden_set) && !allow_set.contains(path.as_str())
        })
        .cloned()
        .collect();
    unclassified.sort();
    unclassified.dedup();

    if unclassified.is_empty() {
        BuiltinCheckResult::pass(
            ID,
            PROVES,
            FIX,
            ORIGIN,
            format!(
                "{} ENV_MAP keys checked: {} REPO_FORBIDDEN, {} narrow-only allow-listed, 0 \
                 unclassified",
                env_map_paths.len(),
                env_map_paths
                    .iter()
                    .filter(|p| is_repo_forbidden(p, &forbidden_set))
                    .count(),
                env_map_paths
                    .iter()
                    .filter(|p| allow_set.contains(p.as_str()))
                    .count(),
            ),
        )
    } else {
        BuiltinCheckResult::fail(
            ID,
            PROVES,
            FIX,
            ORIGIN,
            format!(
                "unclassified ctx.toml key(s), neither REPO_FORBIDDEN nor narrow-only \
                 allow-listed: {}",
                unclassified.join(", ")
            ),
        )
    }
}

/// Whether `path` (an `ENV_MAP` entry's dotted key, e.g. `"review.claude"`)
/// is covered by `forbidden` (the dotted `REPO_FORBIDDEN` paths) -- either
/// exactly, or because `REPO_FORBIDDEN` names an ancestor TABLE rather than
/// the leaf (`config.rs`'s own comment on `(&["review"], ...)`: "`value_at`
/// matches a table node the same way it matches a leaf ... this one entry
/// blocks both `review.claude` and `review.codex` together"). Component-wise
/// (splits on `.`), not a raw string prefix, so `"reviewer"` is never
/// wrongly covered by a `"review"` entry.
fn is_repo_forbidden(path: &str, forbidden: &BTreeSet<&str>) -> bool {
    if forbidden.contains(path) {
        return true;
    }
    let segments: Vec<&str> = path.split('.').collect();
    for prefix_len in 1..segments.len() {
        let prefix = segments[..prefix_len].join(".");
        if forbidden.contains(prefix.as_str()) {
            return true;
        }
    }
    false
}

/// Finds `const <name>: &[...] = &[ ... ];` in `source` and returns the
/// dotted path (`"score.window"`) for every `&["a", "b", ...]` bracketed
/// string-array literal found inside its body -- both `ENV_MAP` (whose path
/// array is the tuple's 2nd field) and `REPO_FORBIDDEN` (whose path array is
/// the tuple's 1st field) shape their entries this way, and neither table's
/// body contains any OTHER `&[...]` bracketed literal, so one pattern covers
/// both without needing to know which field position the path is in.
fn extract_table_paths(source: &str, const_marker: &str) -> Option<Vec<String>> {
    let start = source.find(const_marker)?;
    let assign = source[start..].find("= &[")? + start + 3; // position of the opening '['
    let mut depth = 0i32;
    let mut end = None;
    for (offset, ch) in source[assign..].char_indices() {
        match ch {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(assign + offset + 1);
                    break;
                }
            }
            _ => {}
        }
    }
    let body = &source[assign..end?];

    let bracket_re = Regex::new(r#"&\[\s*((?:"(?:[^"\\]|\\.)*"\s*,?\s*)*)\]"#).ok()?;
    let string_re = Regex::new(r#""((?:[^"\\]|\\.)*)""#).ok()?;

    let mut paths = Vec::new();
    for outer in bracket_re.captures_iter(body) {
        let inner = &outer[1];
        let segments: Vec<String> = string_re
            .captures_iter(inner)
            .map(|cap| cap[1].to_string())
            .collect();
        if !segments.is_empty() {
            paths.push(segments.join("."));
        }
    }
    Some(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_config_rs(repo: &Path, body: &str) {
        std::fs::create_dir_all(repo.join("src/commands/ctx")).unwrap();
        std::fs::write(repo.join("src/commands/ctx/config.rs"), body).unwrap();
    }

    #[test]
    fn missing_config_rs_is_inconclusive() {
        let repo = tempdir().unwrap();
        let result = run(repo.path());
        assert_eq!(result.outcome, super::super::BuiltinOutcome::Inconclusive);
    }

    #[test]
    fn a_new_env_map_key_with_no_classification_fails() {
        let repo = tempdir().unwrap();
        write_config_rs(
            repo.path(),
            r#"
const ENV_MAP: &[(&str, &[&str], u8)] = &[
    ("ZIRV_CTX_AGENT", &["agent"], 0),
    ("ZIRV_CTX_NEW_WIDENING_KEY", &["new", "widening_key"], 0),
];

const REPO_FORBIDDEN: &[(&[&str], &str)] = &[
    (&["agent"], "ZIRV_CTX_AGENT"),
];
"#,
        );
        let result = run(repo.path());
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Fail,
            "{result:?}"
        );
        assert!(result.details.contains("new.widening_key"), "{result:?}");
    }

    #[test]
    fn every_env_map_key_classified_passes() {
        let repo = tempdir().unwrap();
        write_config_rs(
            repo.path(),
            r#"
const ENV_MAP: &[(&str, &[&str], u8)] = &[
    ("ZIRV_CTX_AGENT", &["agent"], 0),
    ("ZIRV_CTX_CHAT_MODEL", &["chat", "model"], 0),
];

const REPO_FORBIDDEN: &[(&[&str], &str)] = &[
    (&["agent"], "ZIRV_CTX_AGENT"),
];
"#,
        );
        let result = run(repo.path());
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Pass,
            "{result:?}"
        );
    }

    #[test]
    fn the_real_repo_config_rs_has_no_unclassified_keys() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
        let result = run(repo);
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Pass,
            "{result:?}"
        );
    }
}
