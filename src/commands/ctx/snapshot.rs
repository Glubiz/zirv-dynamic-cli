//! Issue #320 (second half): `zirv ctx snapshot` -- a redacted, capped,
//! pasteable summary of a session's/repo's diagnostic state, so an operator
//! (or `zirv report bug --snapshot`) can attach real context to a bug report
//! without hand-reconstructing it from memory.
//!
//! Mirrors the `rot.rs`/`score.rs` split (CLAUDE.md): [`assemble`] (plus its
//! own [`cap_head_tail`]/[`redact_text`] helpers) is pure -- given
//! already-rendered section bodies, it redacts and caps them, touching no
//! fs/clock/env/net -- while [`build`] is the impure caller that gathers
//! every section's content from the modules that already compute it, and
//! [`run`]/[`run_with`] are the CLI entry points.
//!
//! No network call of any kind is made here (not even a passive usage-window
//! refresh): a snapshot reads only what earlier polls/hooks already
//! persisted. No automatic issue filing happens from this module either --
//! see `report.rs` for the one caller that turns a snapshot into part of an
//! issue body, only on the operator's own `--snapshot`/`--snapshot-file`.

use std::io::Write;
use std::path::Path;
use std::process::Command;

use super::config::{CtxConfig, EnvLookup, env_from_process};
use super::event::{SessionId, SessionRef};
use super::state::StateDir;
use super::{CtxResult, adapters, group, log, permissions, safety, screen};

#[derive(Debug, clap::Args)]
pub struct SnapshotArgs {
    /// Scope the session-specific sections (rot verdicts, safety decisions,
    /// permission escalations) to one session id. Without it, those sections
    /// report machine-wide history instead of one session's own.
    #[arg(long)]
    pub session: Option<String>,
    /// Emit `{"snapshot": "<redacted text>"}` instead of plain text.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

// ---------------------------------------------------------------------
// Pure assembly: redaction + capping. No fs/clock/env/net past this point.
// ---------------------------------------------------------------------

/// Hard ceiling on the whole rendered snapshot (issue #320's own design: a
/// GitHub issue body needs a much smaller ceiling than Hermes's 512 KB
/// per-file diagnostics bundle cap).
pub const MAX_SNAPSHOT_BYTES: usize = 64 * 1024;

/// Per-section ceiling applied before the whole-document cap ever has to
/// act, so one runaway section (a config file with hundreds of keys, a
/// noisy safety-decision history) cannot crowd every section after it out
/// of the final document.
const SECTION_CAP_BYTES: usize = 6 * 1024;

/// One section of the snapshot: a title and its already-rendered body.
/// Assembled in declaration order.
#[derive(Debug, Clone)]
pub struct Section {
    pub title: &'static str,
    pub body: String,
}

impl Section {
    pub fn new(title: &'static str, body: impl Into<String>) -> Self {
        Self {
            title,
            body: body.into(),
        }
    }
}

fn char_boundary_at_or_before(text: &str, mut idx: usize) -> usize {
    if idx > text.len() {
        idx = text.len();
    }
    while idx > 0 && !text.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn char_boundary_at_or_after(text: &str, idx: usize) -> usize {
    let mut idx = idx.min(text.len());
    while idx < text.len() && !text.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

/// Truncates `text` to at most `budget` bytes, keeping roughly its first and
/// last half and inserting a marker line naming how many bytes were dropped
/// from the middle -- never a silent drop, and never a head-only cut that
/// would hide how a section ends. A no-op when `text` already fits.
fn cap_head_tail(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.to_string();
    }
    let marker_for = |omitted: usize| format!("\n... [{omitted} bytes truncated] ...\n");
    // A budget too small to hold even the marker gets the marker itself,
    // truncated -- an honest "cannot show this" beats a mangled partial cut.
    let rough_marker = marker_for(text.len().saturating_sub(budget));
    if rough_marker.len() >= budget {
        return crate::utils::truncate_bytes(rough_marker, Some(budget));
    }
    let keep = budget - rough_marker.len();
    let head_len = keep / 2;
    let tail_len = keep - head_len;

    let head_end = char_boundary_at_or_before(text, head_len);
    let tail_start_min = text.len().saturating_sub(tail_len);
    let tail_start = char_boundary_at_or_after(text, tail_start_min).max(head_end);

    let omitted = tail_start.saturating_sub(head_end);
    let marker = marker_for(omitted);
    format!("{}{}{}", &text[..head_end], marker, &text[tail_start..])
}

/// Runs every line of `text` through `screen::screen`'s pure credential-shape
/// and high-entropy detectors, replacing any flagged line wholesale with a
/// safe summary of why it was flagged. `ScreenFlag::describe` (what
/// `ScreenReport::summary` is built from) never echoes the flagged text
/// itself, so the replacement can never leak a fragment of the secret that
/// triggered it. Line granularity, not span granularity: `screen::screen`
/// reports only THAT a piece of text is flagged, never where within it, so
/// replacing the smallest safe unit -- the containing line -- is the only
/// sound thing to do with that.
fn redact_text(text: &str) -> String {
    text.lines()
        .map(|line| {
            let report = screen::screen(line);
            if report.is_clean() {
                line.to_string()
            } else {
                format!("[redacted -- {}]", report.summary())
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Pure: redacts and caps each section, joins them, then applies one more
/// whole-document cap so the total never exceeds [`MAX_SNAPSHOT_BYTES`] no
/// matter how many sections there are. Redaction always runs BEFORE any
/// capping (per-section or whole-document): capping only ever cuts through
/// text `redact_text` has already declared secret-free, so a byte-level cut
/// can garble a line cosmetically but can never split a live secret across
/// the visible head and the discarded middle.
pub fn assemble(sections: &[Section]) -> String {
    let mut out = String::new();
    for section in sections {
        let redacted = redact_text(&section.body);
        let capped = cap_head_tail(&redacted, SECTION_CAP_BYTES);
        out.push_str("## ");
        out.push_str(section.title);
        out.push('\n');
        out.push_str(&capped);
        out.push_str("\n\n");
    }
    let trimmed = out.trim_end_matches('\n').to_string();
    cap_head_tail(&trimmed, MAX_SNAPSHOT_BYTES)
}

// ---------------------------------------------------------------------
// Config-override rendering: keys always shown, values only for keys that
// do not look secret-shaped (secrets render as `<set>`). This is a first,
// cheap line of defense; `assemble`'s whole-text `screen::screen` pass is
// the second, and catches a credential-shaped value sitting under an
// innocuous key name that this heuristic would otherwise miss.
// ---------------------------------------------------------------------

const SECRET_KEY_MARKERS: &[&str] = &[
    "token",
    "secret",
    "password",
    "passwd",
    "credential",
    "auth",
    "key",
];

fn is_secret_key(key: &str) -> bool {
    let lower = key.to_lowercase();
    SECRET_KEY_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
}

fn render_toml_value(value: &toml::Value) -> String {
    match value {
        toml::Value::String(s) => s.clone(),
        toml::Value::Integer(i) => i.to_string(),
        toml::Value::Float(f) => f.to_string(),
        toml::Value::Boolean(b) => b.to_string(),
        toml::Value::Datetime(d) => d.to_string(),
        toml::Value::Array(_) => "<array>".to_string(),
        toml::Value::Table(_) => "<table>".to_string(),
    }
}

fn flatten_into(value: &toml::Value, prefix: &str, out: &mut Vec<(String, String)>) {
    let toml::Value::Table(table) = value else {
        return;
    };
    for (k, v) in table {
        let key = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix}.{k}")
        };
        if let toml::Value::Table(_) = v {
            flatten_into(v, &key, out);
        } else {
            let rendered = if is_secret_key(&key) {
                "<set>".to_string()
            } else {
                render_toml_value(v)
            };
            out.push((key, rendered));
        }
    }
}

/// Flattens a `ctx.toml` layer's text into dotted `key = value` rows, keys
/// only when a key looks secret-shaped. Pure: text in, rows out. `None` (an
/// empty vec) on any parse failure -- a broken layer is `config.rs`'s own
/// concern to report; a snapshot must never fail just because a layer has a
/// syntax error.
fn flatten_toml_overrides(text: &str) -> Vec<(String, String)> {
    // Same parse entry point `config.rs::read_layer` itself uses
    // (`toml::from_str::<toml::Table>`), not `str::parse`, which this toml
    // version does not implement for the bare `Value` type.
    let Ok(table) = toml::from_str::<toml::Table>(text) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    flatten_into(&toml::Value::Table(table), "", &mut out);
    out.sort();
    out
}

/// Renders one `ctx.toml` layer at `path`: `(none)` when the file does not
/// exist, is empty, or fails to parse.
fn config_overrides_section(path: &Path) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return "(none)".to_string();
    };
    let rows = flatten_toml_overrides(&text);
    if rows.is_empty() {
        return "(none)".to_string();
    }
    rows.into_iter()
        .map(|(k, v)| format!("{k} = {v}"))
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------
// Impure gathering: every section's content, best-effort. A missing or
// unreadable source renders `--`/absent, never a stale or guessed value.
// ---------------------------------------------------------------------

fn program_version(program: &str) -> Option<String> {
    if !adapters::program_is_present(program) {
        return None;
    }
    let output = Command::new(program).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let first_line = text.lines().next().unwrap_or("").trim();
    (!first_line.is_empty()).then(|| first_line.to_string())
}

fn git_describe(repo: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["describe", "--always", "--dirty"])
        .current_dir(repo)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// This session's (or, without `--session`, the whole log's) last `limit`
/// rot verdicts/scores, newest last, read from the main decision log
/// (`decisions.jsonl` -- the same file [`recent_log_lines`] tails
/// unfiltered). A generous raw-line sample (500) is read and then filtered,
/// since the log is a flat rotation of every verb's decisions, not only
/// scoring ones.
fn recent_rot_verdicts(state: &StateDir, session: Option<&str>, limit: usize) -> String {
    let Ok(lines) = log::tail(state, 500) else {
        return "--".to_string();
    };
    let mut matched: Vec<String> = Vec::new();
    for line in lines {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if let Some(session) = session {
            let row_session = value.get("session").and_then(|v| v.as_str()).unwrap_or("");
            if row_session != session {
                continue;
            }
        }
        let verdict = value.get("verdict").and_then(|v| v.as_str()).unwrap_or("?");
        let score = value.get("score").and_then(|v| v.as_u64());
        matched.push(match score {
            Some(score) => format!("{verdict} ({score})"),
            None => verdict.to_string(),
        });
    }
    if matched.is_empty() {
        return "--".to_string();
    }
    let from = matched.len().saturating_sub(limit);
    matched[from..].join(", ")
}

/// Safety-decision counts by verdict, and by `matched_pattern`, over
/// `decisions.jsonl`'s own `safety-decisions/` bucket (issue #147's
/// `log::read_safety_decisions`), optionally filtered to one session.
fn safety_decision_counts(state: &StateDir, session: Option<&str>) -> String {
    let mut records = log::read_safety_decisions(state);
    if let Some(session) = session {
        records.retain(|r| r.session == session);
    }
    if records.is_empty() {
        return "--".to_string();
    }
    let mut by_verdict: std::collections::BTreeMap<String, u32> = Default::default();
    let mut by_pattern: std::collections::BTreeMap<String, u32> = Default::default();
    for record in &records {
        *by_verdict.entry(record.verdict.clone()).or_default() += 1;
        if let Some(pattern) = &record.matched_pattern {
            *by_pattern.entry(pattern.clone()).or_default() += 1;
        }
    }
    let verdict_line = by_verdict
        .iter()
        .map(|(v, c)| format!("{v}: {c}"))
        .collect::<Vec<_>>()
        .join(", ");
    let pattern_line = if by_pattern.is_empty() {
        "(none)".to_string()
    } else {
        by_pattern
            .iter()
            .map(|(p, c)| format!("{p}: {c}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!("by verdict -- {verdict_line}\nby matched_pattern -- {pattern_line}")
}

/// The most recent safety-decision record's `attestation` field (issue
/// #320), optionally scoped to one session. Empty-string attestations (a
/// record written before the field was read back at all) are treated as
/// absent, never as a genuine empty state.
fn latest_attestation(state: &StateDir, session: Option<&str>) -> Option<String> {
    let mut records = log::read_safety_decisions(state);
    if let Some(session) = session {
        records.retain(|r| r.session == session);
    }
    records
        .into_iter()
        .rev()
        .map(|r| r.attestation)
        .find(|a| !a.is_empty())
}

/// Permission mode (from the most recent extracted request) and a one-line
/// escalation summary for `session`'s own transcript, reusing `permissions`'
/// existing extraction/audit machinery read-only -- no new detection logic.
/// `None`/`--` when the session's transcript cannot be resolved at all.
fn escalation_info(cfg: &CtxConfig, repo: &Path, session: &str) -> (Option<String>, String) {
    let Ok(adapter) = adapters::select(cfg.agent.as_deref(), &[], cfg) else {
        return (None, "--".to_string());
    };
    let transcript = adapter.transcript_path(&SessionRef {
        id: SessionId::parse(session),
        cwd: repo.to_path_buf(),
    });
    if !transcript.is_file() {
        return (None, "--".to_string());
    }
    let audit_agent = if adapter.name() == "claude" {
        permissions::AuditAgent::Claude
    } else {
        permissions::AuditAgent::Codex
    };
    let report = permissions::audit_report(audit_agent, &[transcript]);
    let permission_mode = report
        .requests
        .iter()
        .rev()
        .find(|r| !r.permission_mode.is_empty())
        .map(|r| r.permission_mode.clone());
    if report.total_requests == 0 {
        return (permission_mode, "none".to_string());
    }
    let escalated = report
        .requests
        .iter()
        .filter(|r| r.result == "escalated")
        .count();
    let denied = report
        .requests
        .iter()
        .filter(|r| r.result == "denied")
        .count();
    (
        permission_mode,
        format!(
            "{} requests ({escalated} escalated, {denied} denied) across {} families",
            report.total_requests,
            report.groups.len()
        ),
    )
}

/// Read-only usage-window report: the same renderer `zirv ctx usage` uses,
/// fed windows already persisted on disk. Deliberately never calls
/// `poll::maybe_poll`/`window::refresh_codex_usage` -- a diagnostic snapshot
/// must not itself trigger a live vendor request or filesystem scan as a
/// side effect of being taken.
fn usage_windows_section(cfg: &CtxConfig, state: &StateDir, now: u64) -> String {
    let provider = adapters::provider_for_usage_readout(cfg);
    if super::window::has_no_usage_source(state, provider)
        && provider != super::window::LEGACY_USAGE_PROVIDER
    {
        return format!("{provider}: no usage source");
    }
    let (collector, estimator) = super::pace::current_windows(state, &cfg.pace, now, provider);
    let mut buf: Vec<u8> = Vec::new();
    if super::usage::report(&mut buf, &collector, estimator.as_ref(), now, &cfg.pace).is_err() {
        return "--".to_string();
    }
    String::from_utf8_lossy(&buf).trim().to_string()
}

fn work_groups_and_delegations_section(state: &StateDir) -> String {
    let groups = group::list(state);
    let open = groups.iter().filter(|g| g.closed_at.is_none()).count();
    let closed = groups.len() - open;
    let delegations = log::read_delegations(state, 200);
    let total = delegations.len();
    let mut by_outcome: std::collections::BTreeMap<String, u32> = Default::default();
    for d in &delegations {
        *by_outcome.entry(d.outcome.clone()).or_default() += 1;
    }
    let outcome_line = if by_outcome.is_empty() {
        "(none)".to_string()
    } else {
        by_outcome
            .iter()
            .map(|(o, c)| format!("{o}: {c}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        "work groups: {open} open, {closed} closed\ndelegations (last {total} recorded): {outcome_line}"
    )
}

fn recent_log_lines(state: &StateDir) -> String {
    let lines = log::tail(state, 20).unwrap_or_default();
    if lines.is_empty() {
        "(none)".to_string()
    } else {
        lines.join("\n")
    }
}

/// Gathers every section, then hands them to the pure [`assemble`]. `session`
/// scopes the session-specific sections; `None` leaves them machine-wide.
/// Never fails outright on a missing source -- only a truly fatal condition
/// (none observed in practice; every sub-call here is itself best-effort)
/// would surface as `Err`.
pub fn build(session: Option<&str>, repo: &Path, env: EnvLookup<'_>) -> CtxResult<String> {
    let cfg = CtxConfig::load(repo, env).unwrap_or_default();
    let state = StateDir::resolve(env).ok();
    let now = super::state::now_secs();
    let home = crate::utils::home_dir().ok();
    let session = session
        .map(str::to_string)
        .or_else(|| env(adapters::SESSION_ENV));

    let launch_mode = if env(adapters::LAUNCH_MODE_ENV).as_deref()
        == Some(adapters::LAUNCH_MODE_INTERACTIVE_VALUE)
    {
        "interactive".to_string()
    } else {
        "--".to_string()
    };

    let mut sections = Vec::new();

    sections.push(Section::new(
        "Version",
        format!(
            "zirv: {}\ngit describe: {}\nplatform: {}/{}",
            env!("CARGO_PKG_VERSION"),
            git_describe(repo).unwrap_or_else(|| "--".to_string()),
            std::env::consts::OS,
            std::env::consts::ARCH,
        ),
    ));

    sections.push(Section::new(
        "Harness",
        format!(
            "claude --version: {}\ncodex --version: {}\nadapter: {}\nlaunch mode: {}",
            program_version("claude").unwrap_or_else(|| "--".to_string()),
            program_version("codex").unwrap_or_else(|| "--".to_string()),
            cfg.agent.clone().unwrap_or_else(|| "--".to_string()),
            launch_mode,
        ),
    ));

    let (permission_mode, escalations_summary) = match &session {
        Some(session) => escalation_info(&cfg, repo, session),
        None => (None, "-- (pass --session to audit escalations)".to_string()),
    };
    sections.push(Section::new(
        "Permissions",
        format!(
            "permission mode: {}\nescalations: {escalations_summary}",
            permission_mode.unwrap_or_else(|| "--".to_string()),
        ),
    ));

    let attestation = state
        .as_ref()
        .and_then(|state| latest_attestation(state, session.as_deref()));
    let policy_fingerprint = safety::policy_fingerprint(&cfg.safety).ok();
    sections.push(Section::new(
        "Safety policy",
        format!(
            "hook attestation: {}\npolicy fingerprint: {}",
            attestation.unwrap_or_else(|| "--".to_string()),
            policy_fingerprint.unwrap_or_else(|| "--".to_string()),
        ),
    ));

    let home_ctx_path = home.as_deref().map(|h| {
        h.join(crate::utils::SCRIPT_DIR_NAME)
            .join(super::config::CTX_CONFIG_FILE)
    });
    let repo_ctx_path = repo
        .join(crate::utils::SCRIPT_DIR_NAME)
        .join(super::config::CTX_CONFIG_FILE);
    sections.push(Section::new(
        "Config overrides",
        format!(
            "operator (~/.zirv/ctx.toml):\n{}\n\nrepo (.zirv/ctx.toml):\n{}",
            home_ctx_path
                .as_deref()
                .map(config_overrides_section)
                .unwrap_or_else(|| "(no home directory resolved)".to_string()),
            config_overrides_section(&repo_ctx_path),
        ),
    ));

    match &state {
        Some(state) => {
            sections.push(Section::new(
                "Usage windows",
                usage_windows_section(&cfg, state, now),
            ));
            sections.push(Section::new(
                "Rot verdicts",
                recent_rot_verdicts(state, session.as_deref(), 10),
            ));
            sections.push(Section::new(
                "Safety decisions",
                safety_decision_counts(state, session.as_deref()),
            ));
            sections.push(Section::new(
                "Work groups and delegations",
                work_groups_and_delegations_section(state),
            ));
            sections.push(Section::new("Recent log", recent_log_lines(state)));
        }
        None => {
            for title in [
                "Usage windows",
                "Rot verdicts",
                "Safety decisions",
                "Work groups and delegations",
                "Recent log",
            ] {
                sections.push(Section::new(title, "-- (no state directory resolved)"));
            }
        }
    }

    Ok(assemble(&sections))
}

pub fn run_with<W: Write>(
    args: &SnapshotArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    let text = build(args.session.as_deref(), repo, env)?;
    if args.json {
        writeln!(
            w,
            "{}",
            serde_json::to_string(&serde_json::json!({ "snapshot": text }))?
        )?;
    } else {
        writeln!(w, "{text}")?;
    }
    Ok(0)
}

pub fn run<W: Write>(args: &SnapshotArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    run_with(args, w, &repo, &env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    // -- cap_head_tail --

    #[test]
    fn cap_head_tail_is_a_no_op_under_budget() {
        assert_eq!(cap_head_tail("short", 100), "short");
    }

    #[test]
    fn cap_head_tail_preserves_both_ends_with_a_marker_never_dropping_silently() {
        let text = format!("{}{}", "A".repeat(500), "Z".repeat(500));
        let capped = cap_head_tail(&text, 200);
        assert!(
            capped.len() <= 200 + 64,
            "budget must be roughly honored: {}",
            capped.len()
        );
        assert!(
            capped.starts_with('A'),
            "head must be preserved: {capped:?}"
        );
        assert!(capped.ends_with('Z'), "tail must be preserved: {capped:?}");
        assert!(
            capped.contains("bytes truncated"),
            "a cut must always carry a marker: {capped}"
        );
        // Never a head-only cut: the tail's own content must still appear.
        assert!(capped.contains('Z'));
    }

    #[test]
    fn cap_head_tail_never_panics_on_a_budget_smaller_than_the_marker() {
        let text = "x".repeat(1000);
        let capped = cap_head_tail(&text, 5);
        assert!(capped.len() <= 5);
    }

    // -- redact_text / assemble --

    #[test]
    fn a_line_with_no_secret_shape_passes_through_unchanged() {
        let body = "adapter: claude\nlaunch mode: interactive";
        let out = assemble(&[Section::new("T", body)]);
        assert!(out.contains("adapter: claude"));
        assert!(out.contains("launch mode: interactive"));
    }

    #[test]
    fn a_github_token_shaped_line_is_redacted_and_the_secret_never_appears() {
        let secret = "ghp_1234567890abcdefghijklmnopqrstuvwx";
        let body = format!("token = {secret}");
        let out = assemble(&[Section::new("T", body)]);
        assert!(
            !out.contains(secret),
            "the raw secret must never appear: {out}"
        );
        assert!(out.contains("[redacted --"), "got {out}");
    }

    #[test]
    fn a_clean_fixture_and_a_secret_fixture_are_distinguishable() {
        let clean = assemble(&[Section::new("T", "hello world, nothing secret here")]);
        let secret = assemble(&[Section::new("T", "sk-abcdefghijklmnopqrstuvwxyzABCDEFGH")]);
        assert!(!clean.contains("[redacted --"));
        assert!(secret.contains("[redacted --"));
    }

    // -- config overrides: secret keys never show their value --

    #[test]
    fn a_secret_named_key_renders_as_set_never_its_value() {
        let toml = "[github]\ntoken = \"super-secret-value\"\nname = \"repo\"\n";
        let rows = flatten_toml_overrides(toml);
        let token_row = rows.iter().find(|(k, _)| k == "github.token").expect("row");
        assert_eq!(token_row.1, "<set>");
        let name_row = rows.iter().find(|(k, _)| k == "github.name").expect("row");
        assert_eq!(name_row.1, "repo");
        // Never present anywhere in a rendered row.
        assert!(!rows.iter().any(|(_, v)| v.contains("super-secret-value")));
    }

    #[test]
    fn config_overrides_section_reports_none_for_a_missing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("does-not-exist.toml");
        assert_eq!(config_overrides_section(&path), "(none)");
    }

    #[test]
    fn config_overrides_section_never_leaks_a_populated_secret_end_to_end() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ctx.toml");
        std::fs::write(&path, "[github]\ntoken = \"super-secret-value\"\n").expect("write");
        let rendered = config_overrides_section(&path);
        assert!(rendered.contains("github.token = <set>"), "got {rendered}");
        assert!(!rendered.contains("super-secret-value"));
    }

    // -- absent sources render `--`, not a stale/default value --

    #[test]
    fn build_with_no_config_and_no_session_renders_dashes_for_every_missing_source() {
        // This machine's own real `~/.zirv/ctx.toml`/decision log must never
        // leak into this assertion -- `HomeGuard` genuinely overrides the
        // process HOME/USERPROFILE `crate::utils::home_dir()` (and
        // `CtxConfig::load`'s own home-layer resolution) reads, and the
        // isolated state dir keeps the decision/safety logs empty.
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let state = tempfile::tempdir().expect("tempdir");
        let _home_guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let vars = env_map(&[(
            crate::commands::ctx::state::STATE_ENV,
            state.path().to_str().expect("utf8"),
        )]);
        let env = |key: &str| vars.get(key).cloned();
        let text = build(None, repo.path(), &env).expect("build must not fail outright");

        assert!(text.contains("adapter: --"), "got {text}");
        assert!(
            text.contains("permission mode: --"),
            "no session means no escalation audit: {text}"
        );
        assert!(
            text.contains("-- (pass --session to audit escalations)"),
            "got {text}"
        );
        assert!(
            text.contains("operator (~/.zirv/ctx.toml):\n(none)"),
            "an empty home layer must render as absent, not a stale value: {text}"
        );
        assert!(
            text.contains("repo (.zirv/ctx.toml):\n(none)"),
            "got {text}"
        );
    }

    #[test]
    fn build_with_an_isolated_state_dir_reports_dashes_for_empty_history() {
        let repo = tempfile::tempdir().expect("tempdir");
        let state = tempfile::tempdir().expect("tempdir");
        let vars = env_map(&[(
            crate::commands::ctx::state::STATE_ENV,
            state.path().to_str().expect("utf8"),
        )]);
        let env = |key: &str| vars.get(key).cloned();
        let text = build(None, repo.path(), &env).expect("build");

        assert!(text.contains("Rot verdicts"), "got {text}");
        assert!(
            text.contains("--"),
            "an empty log must render as absent, not a fabricated 0"
        );
    }

    #[test]
    fn build_never_panics_and_stays_within_the_hard_cap() {
        let repo = tempfile::tempdir().expect("tempdir");
        let state = tempfile::tempdir().expect("tempdir");
        // Seed a large decision log so the "Rot verdicts"/"Recent log"
        // sections have real content to cap.
        let ctx_state = StateDir::from_root(state.path().to_path_buf());
        for i in 0..50 {
            log::append(
                &ctx_state,
                &log::Decision {
                    ts: 1_700_000_000 + i,
                    session: "sess-1",
                    verb: "hook",
                    verdict: "healthy",
                    score: 10,
                    action: "observe",
                    detail: "",
                    observed_at: None,
                },
            )
            .expect("append");
        }
        let vars = env_map(&[(
            crate::commands::ctx::state::STATE_ENV,
            state.path().to_str().expect("utf8"),
        )]);
        let env = |key: &str| vars.get(key).cloned();
        let text = build(Some("sess-1"), repo.path(), &env).expect("build");
        assert!(text.len() <= MAX_SNAPSHOT_BYTES);
        assert!(text.contains("healthy"), "got {text}");
    }

    // -- report.rs's own tests cover `--snapshot`/`--snapshot-file`
    // composition; these stay scoped to this module's own pure/impure split.

    #[test]
    fn run_with_json_wraps_the_same_text_run_with_plain_would_print() {
        // Isolated home/state: without this, two calls a moment apart can
        // read this machine's own live usage-window "observed Ns ago" text
        // and disagree by a second, which is a flake in the test, not in
        // `run_with` itself.
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let state = tempfile::tempdir().expect("tempdir");
        let _home_guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let vars = env_map(&[(
            crate::commands::ctx::state::STATE_ENV,
            state.path().to_str().expect("utf8"),
        )]);
        let env = |key: &str| vars.get(key).cloned();

        let mut plain_out = Vec::new();
        let plain_args = SnapshotArgs {
            session: None,
            json: false,
        };
        run_with(&plain_args, &mut plain_out, repo.path(), &env).expect("plain run");
        let plain_text = String::from_utf8(plain_out).expect("utf8");

        let mut json_out = Vec::new();
        let json_args = SnapshotArgs {
            session: None,
            json: true,
        };
        run_with(&json_args, &mut json_out, repo.path(), &env).expect("json run");
        let json_text = String::from_utf8(json_out).expect("utf8");
        let value: serde_json::Value = serde_json::from_str(&json_text).expect("json");
        assert_eq!(
            value["snapshot"].as_str().expect("snapshot field"),
            plain_text.trim_end()
        );
    }
}
