//! `zirv ctx learn` (issue #425): promotes a command mistake this machine's
//! agents keep making -- and keep FIXING the same way -- from transcripts
//! into memory, so the next session starts already knowing the fix instead
//! of rediscovering it.
//!
//! `rot.rs` already tracks two shapes of in-session repetition
//! (`repetition`/`longest_same_error_run`), but both key on a HASH of the
//! call/error text (`NormalizedEvent::ToolCall::input_hash`,
//! `ToolErrorText::hash`) -- deliberately, so `rot.rs` can stay pure and
//! never hold raw transcript text. Learning a correction needs the OPPOSITE
//! shape: two DIFFERENT commands (a failing one and the one that fixed it),
//! described well enough to write down what changed -- `--foo` became
//! `--bar`, say. That needs the actual text, which `rot.rs`'s model never
//! carries, so this module keeps its own small extraction (mirroring
//! `discover.rs`'s own documented precedent for a local, single-purpose
//! `tool_use`/`tool_result` pairing copy) rather than reusing or extending
//! `rot.rs`.
//!
//! Pipeline: [`extract_claude_attempts`]/[`extract_codex_attempts`] (I/O:
//! read a transcript, pair each `Bash`/`exec` call with its own result) feed
//! [`analyze`] (pure): [`find_corrections`] pairs a classified failure with
//! the next successful, single-token-different call within a three-call
//! window (this IS the TDD/path-exploration filter -- see its own doc
//! comment), [`group_corrections`] collapses identical corrections seen
//! anywhere into one candidate each, and a threshold+cap keeps only what
//! recurred enough to be worth writing down. [`run_with`] is the thin I/O
//! shell: resolve transcripts, run the pure pipeline, write (or, with
//! `--dry-run`, just print) one `learned:`-prefixed private-bank entry per
//! surviving group.

use std::collections::{BTreeSet, HashMap};
use std::io::Write;
use std::path::Path;

use serde_json::Value;

use super::config::{CtxConfig, env_from_process};
use super::memory::{self, Entry};
use super::output::bare_program;
use super::search::{claude_candidates, codex_candidates};
use super::search_index::Source;
use super::state::{self, StateDir, repo_slug};
use super::{CtxResult, safety};

#[derive(Debug, Clone, clap::Args)]
pub struct LearnArgs {
    /// Restrict to transcripts modified within this window, e.g. `24h`,
    /// `7d`, `30d`, or a bare number of seconds.
    #[arg(long, default_value = "30d")]
    pub since: String,
    /// Print what would be written without changing the memory bank.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
}

/// A correction must recur at least this many times overall...
const MIN_OCCURRENCES: usize = 3;
/// ...across at least this many distinct sessions, so a streak inside one
/// long session (the exact case `rot.rs`'s own repeat detector already
/// covers) never counts as something LEARNED across sessions.
const MIN_SESSIONS: usize = 2;
/// How many attempts past a failing one are checked for its fix -- "a
/// three-call window" counting the failing call itself as the first of the
/// three.
const LOOKAHEAD: usize = 2;
/// Per-repo cap on how many `learned:` entries one run ever writes, so a
/// noisy transcript history cannot flood the memory bank.
pub const MAX_LEARNED_ENTRIES: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ErrorClass {
    UnknownFlag,
    CommandNotFound,
    WrongPath,
    MissingArgument,
    PermissionDenied,
}

impl ErrorClass {
    fn slug(self) -> &'static str {
        match self {
            ErrorClass::UnknownFlag => "unknown-flag",
            ErrorClass::CommandNotFound => "command-not-found",
            ErrorClass::WrongPath => "wrong-path",
            ErrorClass::MissingArgument => "missing-argument",
            ErrorClass::PermissionDenied => "permission-denied",
        }
    }

    fn label(self) -> &'static str {
        match self {
            ErrorClass::UnknownFlag => "unknown flag",
            ErrorClass::CommandNotFound => "command not found",
            ErrorClass::WrongPath => "wrong path",
            ErrorClass::MissingArgument => "missing argument",
            ErrorClass::PermissionDenied => "permission denied",
        }
    }
}

/// Classifies a failing tool result's own text into one of the five coarse
/// classes this verb learns, purely by keyword membership over a lowercased
/// copy. `None` -- never a guess -- for text that matches none of them: the
/// confidence floor the issue asks for is exactly this, an unrecognized
/// failure contributes no learning signal rather than a low-confidence one.
/// Order matters where phrases could overlap (checked before generic
/// missing-argument/wrong-path phrasing, so one message never double-counts).
pub fn classify_error(text: &str) -> Option<ErrorClass> {
    let hay = text.to_lowercase();
    const UNKNOWN_FLAG: &[&str] = &[
        "unrecognized option",
        "unrecognized flag",
        "unknown option",
        "unknown flag",
        "unexpected argument",
        "invalid option",
        "illegal option",
        "no such option",
    ];
    const COMMAND_NOT_FOUND: &[&str] = &[
        "command not found",
        "not recognized as an internal or external command",
        "not recognized as an internal",
        "no such command",
    ];
    const PERMISSION_DENIED: &[&str] = &[
        "permission denied",
        "eacces",
        "operation not permitted",
        "access is denied",
    ];
    const MISSING_ARGUMENT: &[&str] = &[
        "required argument",
        "missing argument",
        "the following arguments are required",
        "requires a value",
        "missing required",
        "argument required",
        "expected 1 argument",
        "expected at least 1 argument",
    ];
    const WRONG_PATH: &[&str] = &[
        "no such file or directory",
        "cannot find the path",
        "cannot find path",
        "is not a directory",
        "does not exist",
    ];
    if UNKNOWN_FLAG.iter().any(|k| hay.contains(k)) {
        return Some(ErrorClass::UnknownFlag);
    }
    if COMMAND_NOT_FOUND.iter().any(|k| hay.contains(k)) {
        return Some(ErrorClass::CommandNotFound);
    }
    if PERMISSION_DENIED.iter().any(|k| hay.contains(k)) {
        return Some(ErrorClass::PermissionDenied);
    }
    if MISSING_ARGUMENT.iter().any(|k| hay.contains(k)) {
        return Some(ErrorClass::MissingArgument);
    }
    if WRONG_PATH.iter().any(|k| hay.contains(k)) {
        return Some(ErrorClass::WrongPath);
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum TokenDiff {
    Substitute { from: String, to: String },
    Insert { token: String },
    Remove { token: String },
}

impl TokenDiff {
    fn key_fragment(&self) -> String {
        match self {
            TokenDiff::Substitute { from, to } => format!("{from}->{to}"),
            TokenDiff::Insert { token } => format!("+{token}"),
            TokenDiff::Remove { token } => format!("-{token}"),
        }
    }

    fn describe(&self) -> String {
        match self {
            TokenDiff::Substitute { from, to } => format!("use `{to}` instead of `{from}`"),
            TokenDiff::Insert { token } => format!("add `{token}`"),
            TokenDiff::Remove { token } => format!("drop `{token}`"),
        }
    }
}

/// The single-token edit (substitution, or a one-token insertion/removal)
/// that turns `a` into `b`, or `None` when they are identical or differ by
/// more than that. This is the mechanism behind every filter the issue asks
/// for at once: two IDENTICAL commands (a flaky test re-run, or a TDD
/// edit-then-rerun loop where the rerun is the same command) diff to
/// `None`, and so does a pair of commands that just happen to fall in the
/// same window without being a plausible fix of one another (more than one
/// token apart) -- "path exploration" (trying different, unrelated paths)
/// is filtered the same way one level up, in [`group_corrections`]: it
/// never repeats the SAME wrong/right pair often enough to cross the
/// threshold.
fn single_token_diff(a: &[&str], b: &[&str]) -> Option<TokenDiff> {
    if a == b {
        return None;
    }
    if a.len() == b.len() {
        let mut diff_at = None;
        for i in 0..a.len() {
            if a[i] != b[i] {
                if diff_at.is_some() {
                    return None;
                }
                diff_at = Some(i);
            }
        }
        let i = diff_at?;
        return Some(TokenDiff::Substitute {
            from: a[i].to_string(),
            to: b[i].to_string(),
        });
    }
    if b.len() == a.len() + 1 {
        return single_insertion(a, b).map(|token| TokenDiff::Insert { token });
    }
    if a.len() == b.len() + 1 {
        return single_insertion(b, a).map(|token| TokenDiff::Remove { token });
    }
    None
}

/// `longer` is exactly one token longer than `shorter`; returns that extra
/// token when removing it from `longer` (at whichever position) leaves
/// `shorter` exactly, `None` if no single removal does.
fn single_insertion(shorter: &[&str], longer: &[&str]) -> Option<String> {
    let mut i = 0;
    while i < shorter.len() && shorter[i] == longer[i] {
        i += 1;
    }
    if shorter[i..] == longer[i + 1..] {
        Some(longer[i].to_string())
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Success,
    /// A classified failure.
    Failure(ErrorClass),
    /// A failure `classify_error` did not recognize -- the confidence
    /// floor: excluded from serving as either half of a correction.
    Unclassified,
}

#[derive(Debug, Clone, PartialEq)]
struct Attempt {
    command: String,
    outcome: Outcome,
}

struct Correction {
    program: String,
    class: ErrorClass,
    diff: TokenDiff,
    from: String,
    to: String,
}

/// Pairs each classified failure in `attempts` with the next SUCCESS within
/// [`LOOKAHEAD`] attempts whose command is a [`single_token_diff`] away from
/// it -- the first such match wins, exactly the "fail-then-succeed ...
/// within a three-call window" mechanism the issue describes. An
/// [`Outcome::Unclassified`] failure never anchors a correction, and never
/// serves as anyone else's fix either.
fn find_corrections(attempts: &[Attempt]) -> Vec<Correction> {
    let mut out = Vec::new();
    for i in 0..attempts.len() {
        let Outcome::Failure(class) = attempts[i].outcome else {
            continue;
        };
        let fail_tokens: Vec<&str> = attempts[i].command.split_whitespace().collect();
        let end = (i + 1 + LOOKAHEAD).min(attempts.len());
        for candidate in attempts.iter().take(end).skip(i + 1) {
            if candidate.outcome != Outcome::Success {
                continue;
            }
            let fix_tokens: Vec<&str> = candidate.command.split_whitespace().collect();
            let Some(diff) = single_token_diff(&fail_tokens, &fix_tokens) else {
                continue;
            };
            let program = fix_tokens
                .first()
                .map(|t| bare_program(t))
                .unwrap_or_default();
            out.push(Correction {
                program,
                class,
                diff,
                from: attempts[i].command.clone(),
                to: candidate.command.clone(),
            });
            break;
        }
    }
    out
}

#[derive(Debug, Clone)]
struct CorrectionGroup {
    program: String,
    class: ErrorClass,
    diff: TokenDiff,
    occurrences: usize,
    sessions: BTreeSet<String>,
    example_from: String,
    example_to: String,
}

/// Collapses every `(session, Correction)` into one [`CorrectionGroup`] per
/// distinct `(program, class, diff)`, counting how many times it occurred
/// and in how many distinct sessions.
fn group_corrections(corrections: Vec<(String, Correction)>) -> Vec<CorrectionGroup> {
    let mut groups: HashMap<(String, ErrorClass, TokenDiff), CorrectionGroup> = HashMap::new();
    for (session, c) in corrections {
        let key = (c.program.clone(), c.class, c.diff.clone());
        let group = groups.entry(key).or_insert_with(|| CorrectionGroup {
            program: c.program.clone(),
            class: c.class,
            diff: c.diff.clone(),
            occurrences: 0,
            sessions: BTreeSet::new(),
            example_from: c.from.clone(),
            example_to: c.to.clone(),
        });
        group.occurrences += 1;
        group.sessions.insert(session);
    }
    groups.into_values().collect()
}

/// Every session's own ordered attempts -> ranked, thresholded correction
/// groups. Pure: no fs/clock/env, so a fixture can pin this end to end with
/// no transcript files at all.
fn analyze(sessions: &[(String, Vec<Attempt>)]) -> Vec<CorrectionGroup> {
    let mut corrections = Vec::new();
    for (session, attempts) in sessions {
        corrections.extend(
            find_corrections(attempts)
                .into_iter()
                .map(|c| (session.clone(), c)),
        );
    }
    let mut groups: Vec<CorrectionGroup> = group_corrections(corrections)
        .into_iter()
        .filter(|g| g.occurrences >= MIN_OCCURRENCES && g.sessions.len() >= MIN_SESSIONS)
        .collect();
    groups.sort_by(|a, b| {
        b.sessions
            .len()
            .cmp(&a.sessions.len())
            .then_with(|| b.occurrences.cmp(&a.occurrences))
            .then_with(|| a.program.cmp(&b.program))
            .then_with(|| a.diff.key_fragment().cmp(&b.diff.key_fragment()))
    });
    groups.truncate(MAX_LEARNED_ENTRIES);
    groups
}

fn learned_key(group: &CorrectionGroup) -> String {
    format!(
        "learned:{}:{}:{}",
        group.program,
        group.class.slug(),
        group.diff.key_fragment()
    )
}

fn describe_group(group: &CorrectionGroup) -> String {
    format!(
        "{} -- seen {} time{} across {} session{}.\n\n- Program: `{}`\n- Fix: {}\n- Example failing command: `{}`\n- Example fixed command: `{}`\n",
        group.class.label(),
        group.occurrences,
        if group.occurrences == 1 { "" } else { "s" },
        group.sessions.len(),
        if group.sessions.len() == 1 { "" } else { "s" },
        group.program,
        group.diff.describe(),
        group.example_from,
        group.example_to,
    )
}

// ---------------------------------------------------------------------
// I/O: transcript extraction (own small copy, see this module's own doc
// comment for why it does not reuse rot.rs's hash-only model)
// ---------------------------------------------------------------------

/// Same shape as `permissions::tool_result_text`/`discover::tool_result_text`
/// -- see `discover.rs`'s own doc comment on why this is its own small copy
/// rather than a shared helper.
fn claude_result_text(block: &Value) -> String {
    match block.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|t| t.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

fn claude_outcome(is_error: bool, text: &str) -> Outcome {
    if !is_error {
        return Outcome::Success;
    }
    match classify_error(text) {
        Some(class) => Outcome::Failure(class),
        None => Outcome::Unclassified,
    }
}

/// Every `Bash` tool call/result pair in one claude transcript, in
/// transcript order, paired by `tool_use_id` (identical pairing shape to
/// `discover::apply_transcript_line`, minus the byte-size gate that module
/// needs and this one does not).
fn extract_claude_attempts(jsonl: &str) -> Vec<Attempt> {
    let mut pending: HashMap<String, String> = HashMap::new();
    let mut out = Vec::new();
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
            Some("assistant") => {
                let Some(blocks) = message.get("content").and_then(Value::as_array) else {
                    continue;
                };
                for block in blocks
                    .iter()
                    .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
                {
                    let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                    if !name.eq_ignore_ascii_case("Bash") {
                        continue;
                    }
                    if let (Some(id), Some(command)) = (
                        block.get("id").and_then(Value::as_str),
                        block
                            .get("input")
                            .and_then(|input| input.get("command"))
                            .and_then(Value::as_str),
                    ) {
                        pending.insert(id.to_string(), command.to_string());
                    }
                }
            }
            Some("user") => {
                let Some(blocks) = message.get("content").and_then(Value::as_array) else {
                    continue;
                };
                for block in blocks
                    .iter()
                    .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
                {
                    let Some(id) = block.get("tool_use_id").and_then(Value::as_str) else {
                        continue;
                    };
                    let Some(command) = pending.remove(id) else {
                        continue;
                    };
                    let is_error = block
                        .get("is_error")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let text = claude_result_text(block);
                    out.push(Attempt {
                        command,
                        outcome: claude_outcome(is_error, &text),
                    });
                }
            }
            _ => {}
        }
    }
    out
}

fn codex_command_string(payload: &Value) -> Option<String> {
    if let Some(s) = payload.get("command").and_then(Value::as_str) {
        return Some(s.to_string());
    }
    let arr = payload.get("command").and_then(Value::as_array)?;
    let parts: Vec<&str> = arr.iter().filter_map(Value::as_str).collect();
    if parts.is_empty() {
        return None;
    }
    let joined = parts.join(" ");
    // Codex wraps an exec call as `["bash", "-lc", "<script>"]`; unwrap that
    // one layer so the diff/classify below sees the same shape a claude
    // `Bash` command already is -- the real script text, not the wrapper.
    Some(safety::unwrap_shell_wrapper(&joined).unwrap_or(joined))
}

fn codex_output_text(payload: &Value) -> String {
    payload
        .get("output")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|t| t.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}

/// Every `exec` custom-tool-call/output pair in one codex rollout, paired by
/// `call_id` (the shape `permissions::extract_codex_requests` already reads
/// for escalated calls; this reads every `exec` call, not only escalated
/// ones). Codex's rollout carries no explicit success/failure flag for a
/// `custom_tool_call_output` (unlike claude's `is_error`), so a codex
/// attempt's outcome comes from [`classify_error`] over the output text
/// alone -- a deliberate, documented gap: an output that happens to contain
/// one of the five keyword phrases without actually being that failure
/// would misclassify, but codex's rollout shape gives nothing better to key
/// on today.
fn extract_codex_attempts(jsonl: &str) -> Vec<Attempt> {
    let mut pending: HashMap<String, String> = HashMap::new();
    let mut out = Vec::new();
    for line in jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if row.get("type").and_then(Value::as_str) != Some("response_item") {
            continue;
        }
        let Some(payload) = row.get("payload") else {
            continue;
        };
        match payload.get("type").and_then(Value::as_str) {
            Some("custom_tool_call") => {
                if payload.get("name").and_then(Value::as_str) != Some("exec") {
                    continue;
                }
                let (Some(call_id), Some(command)) = (
                    payload.get("call_id").and_then(Value::as_str),
                    codex_command_string(payload),
                ) else {
                    continue;
                };
                pending.insert(call_id.to_string(), command);
            }
            Some("custom_tool_call_output") => {
                let Some(call_id) = payload.get("call_id").and_then(Value::as_str) else {
                    continue;
                };
                let Some(command) = pending.remove(call_id) else {
                    continue;
                };
                let text = codex_output_text(payload);
                let outcome = match classify_error(&text) {
                    Some(class) => Outcome::Failure(class),
                    None => Outcome::Success,
                };
                out.push(Attempt { command, outcome });
            }
            _ => {}
        }
    }
    out
}

fn session_id_from_path(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string()
}

fn modified_secs(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
}

fn gather_sessions(repo: &Path, since_ts: u64) -> Vec<(String, Vec<Attempt>)> {
    let mut sessions = Vec::new();
    let candidates = claude_candidates(repo, false)
        .into_iter()
        .chain(codex_candidates());
    for (path, source) in candidates {
        let Some(modified) = modified_secs(&path) else {
            continue;
        };
        if modified < since_ts {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let attempts = match source {
            Source::Claude => extract_claude_attempts(&text),
            Source::Codex => extract_codex_attempts(&text),
            _ => continue,
        };
        if !attempts.is_empty() {
            sessions.push((session_id_from_path(&path), attempts));
        }
    }
    sessions
}

pub fn run<W: Write>(args: &LearnArgs, w: &mut W) -> CtxResult<i32> {
    let env = env_from_process();
    let state = StateDir::resolve(&env)?;
    let repo = std::env::current_dir()?;
    let cfg = CtxConfig::load(&repo, &env)?;
    run_with(&state, &cfg, args, &repo, w, state::now_secs())
}

pub fn run_with<W: Write>(
    state: &StateDir,
    cfg: &CtxConfig,
    args: &LearnArgs,
    repo: &Path,
    w: &mut W,
    now: u64,
) -> CtxResult<i32> {
    if !cfg.memory.enabled {
        writeln!(
            w,
            "zirv ctx learn: memory is disabled (memory.enabled = false); nothing to do"
        )?;
        return Ok(0);
    }
    let since_secs = super::spend::parse_since(&args.since).ok_or_else(|| {
        format!(
            "--since '{}': expected a duration like 30m, 24h, or 7d (or a bare number of seconds)",
            args.since
        )
    })?;
    let since_ts = now.saturating_sub(since_secs);

    let sessions = gather_sessions(repo, since_ts);
    let groups = analyze(&sessions);
    if groups.is_empty() {
        writeln!(w, "no recurring corrections found, --since {}", args.since)?;
        return Ok(0);
    }

    if args.dry_run {
        writeln!(
            w,
            "would write {} learned entr{} (--dry-run, nothing stored):",
            groups.len(),
            if groups.len() == 1 { "y" } else { "ies" }
        )?;
        for group in &groups {
            writeln!(
                w,
                "  {}  ({} occurrences across {} sessions)",
                learned_key(group),
                group.occurrences,
                group.sessions.len()
            )?;
        }
        return Ok(0);
    }

    let slug = repo_slug(repo);
    for group in &groups {
        let entry = Entry {
            key: learned_key(group),
            written_by: "learn".to_string(),
            written: now,
            verified: now,
            source: "learned".to_string(),
            body: describe_group(group),
            importance: None,
            confidence: None,
            tags: vec![group.class.slug().to_string()],
            paths: Vec::new(),
        };
        memory::remember(state, &slug, &entry, cfg)?;
    }
    writeln!(w, "wrote {} learned entries", groups.len())?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attempt(command: &str, outcome: Outcome) -> Attempt {
        Attempt {
            command: command.to_string(),
            outcome,
        }
    }

    fn fail(class: ErrorClass) -> Outcome {
        Outcome::Failure(class)
    }

    // -- classify_error ----------------------------------------------------

    #[test]
    fn classify_error_detects_each_class() {
        assert_eq!(
            classify_error("error: unrecognized option '--foo'"),
            Some(ErrorClass::UnknownFlag)
        );
        assert_eq!(
            classify_error("bash: zrv: command not found"),
            Some(ErrorClass::CommandNotFound)
        );
        assert_eq!(
            classify_error("cat: /tmp/nope.txt: No such file or directory"),
            Some(ErrorClass::WrongPath)
        );
        assert_eq!(
            classify_error("error: the following arguments are required: <key>"),
            Some(ErrorClass::MissingArgument)
        );
        assert_eq!(
            classify_error("bash: /root/secrets: Permission denied"),
            Some(ErrorClass::PermissionDenied)
        );
    }

    #[test]
    fn classify_error_returns_none_for_unrecognized_text() {
        assert_eq!(classify_error("build succeeded, 3 warnings"), None);
    }

    // -- single_token_diff ---------------------------------------------------

    #[test]
    fn single_token_diff_finds_a_substitution() {
        let a: Vec<&str> = "mytool --foo build".split_whitespace().collect();
        let b: Vec<&str> = "mytool --bar build".split_whitespace().collect();
        assert_eq!(
            single_token_diff(&a, &b),
            Some(TokenDiff::Substitute {
                from: "--foo".to_string(),
                to: "--bar".to_string(),
            })
        );
    }

    #[test]
    fn single_token_diff_finds_an_insertion() {
        let a: Vec<&str> = "mytool build".split_whitespace().collect();
        let b: Vec<&str> = "mytool build --release".split_whitespace().collect();
        assert_eq!(
            single_token_diff(&a, &b),
            Some(TokenDiff::Insert {
                token: "--release".to_string(),
            })
        );
    }

    #[test]
    fn single_token_diff_finds_a_removal() {
        let a: Vec<&str> = "mytool --bogus build".split_whitespace().collect();
        let b: Vec<&str> = "mytool build".split_whitespace().collect();
        assert_eq!(
            single_token_diff(&a, &b),
            Some(TokenDiff::Remove {
                token: "--bogus".to_string(),
            })
        );
    }

    #[test]
    fn single_token_diff_is_none_for_identical_commands() {
        let a: Vec<&str> = "cargo test".split_whitespace().collect();
        assert_eq!(single_token_diff(&a, &a), None);
    }

    #[test]
    fn single_token_diff_is_none_when_more_than_one_token_differs() {
        let a: Vec<&str> = "mytool --foo build".split_whitespace().collect();
        let b: Vec<&str> = "othertool --bar test".split_whitespace().collect();
        assert_eq!(single_token_diff(&a, &b), None);
    }

    // -- find_corrections ----------------------------------------------------

    #[test]
    fn find_corrections_pairs_a_failure_with_the_next_success_within_the_window() {
        let attempts = vec![
            attempt("mytool --foo build", fail(ErrorClass::UnknownFlag)),
            attempt("ls", Outcome::Success),
            attempt("mytool --bar build", Outcome::Success),
        ];
        let corrections = find_corrections(&attempts);
        assert_eq!(corrections.len(), 1);
        assert_eq!(corrections[0].program, "mytool");
        assert_eq!(corrections[0].class, ErrorClass::UnknownFlag);
        assert_eq!(
            corrections[0].diff,
            TokenDiff::Substitute {
                from: "--foo".to_string(),
                to: "--bar".to_string(),
            }
        );
    }

    #[test]
    fn find_corrections_does_not_pair_across_more_than_the_window() {
        let attempts = vec![
            attempt("mytool --foo build", fail(ErrorClass::UnknownFlag)),
            attempt("ls", Outcome::Success),
            attempt("git status", Outcome::Success),
            attempt("mytool --bar build", Outcome::Success),
        ];
        assert!(find_corrections(&attempts).is_empty());
    }

    #[test]
    fn find_corrections_skips_unclassified_failures() {
        let attempts = vec![
            attempt("mytool --foo build", Outcome::Unclassified),
            attempt("mytool --bar build", Outcome::Success),
        ];
        assert!(find_corrections(&attempts).is_empty());
    }

    #[test]
    fn find_corrections_never_uses_an_unclassified_failure_as_a_fix() {
        let attempts = vec![
            attempt("mytool --foo build", fail(ErrorClass::UnknownFlag)),
            attempt("mytool --bar build", Outcome::Unclassified),
        ];
        assert!(find_corrections(&attempts).is_empty());
    }

    // -- analyze: thresholds and TDD/flaky exclusion --------------------------

    fn foo_bar_session(session: &str) -> (String, Vec<Attempt>) {
        (
            session.to_string(),
            vec![
                attempt("mytool --foo build", fail(ErrorClass::UnknownFlag)),
                attempt("mytool --bar build", Outcome::Success),
            ],
        )
    }

    #[test]
    fn analyze_produces_one_group_for_a_correction_seen_three_times_across_two_sessions() {
        let sessions = vec![
            foo_bar_session("sess-1"),
            foo_bar_session("sess-1"),
            foo_bar_session("sess-2"),
        ];
        let groups = analyze(&sessions);
        assert_eq!(groups.len(), 1, "{groups:?}");
        assert_eq!(groups[0].occurrences, 3);
        assert_eq!(groups[0].sessions.len(), 2);
        assert_eq!(groups[0].program, "mytool");
    }

    #[test]
    fn analyze_requires_at_least_three_occurrences() {
        let sessions = vec![foo_bar_session("sess-1"), foo_bar_session("sess-2")];
        assert!(analyze(&sessions).is_empty());
    }

    #[test]
    fn analyze_requires_at_least_two_sessions() {
        let sessions = vec![
            foo_bar_session("sess-1"),
            foo_bar_session("sess-1"),
            foo_bar_session("sess-1"),
        ];
        assert!(
            analyze(&sessions).is_empty(),
            "three occurrences in ONE session must not qualify"
        );
    }

    #[test]
    fn analyze_excludes_a_fail_pass_retry_of_the_identical_command() {
        // A flaky/TDD loop: the exact same command fails, then later passes,
        // repeated across sessions. No token differs, so no correction is
        // ever formed at all -- this must never become a `learned:` entry.
        let flaky = |session: &str| {
            (
                session.to_string(),
                vec![
                    attempt("cargo test", fail(ErrorClass::MissingArgument)),
                    attempt("cargo test", Outcome::Success),
                ],
            )
        };
        let sessions = vec![flaky("sess-1"), flaky("sess-1"), flaky("sess-2")];
        assert!(analyze(&sessions).is_empty());
    }

    #[test]
    fn analyze_caps_at_the_per_repo_entry_limit() {
        let mut sessions = Vec::new();
        for i in 0..(MAX_LEARNED_ENTRIES + 5) {
            let program = format!("tool{i}");
            for session in ["sess-1", "sess-2"] {
                sessions.push((
                    session.to_string(),
                    vec![
                        attempt(
                            &format!("{program} --foo build"),
                            fail(ErrorClass::UnknownFlag),
                        ),
                        attempt(&format!("{program} --bar build"), Outcome::Success),
                        attempt(
                            &format!("{program} --foo build"),
                            fail(ErrorClass::UnknownFlag),
                        ),
                        attempt(&format!("{program} --bar build"), Outcome::Success),
                    ],
                ));
            }
        }
        assert_eq!(analyze(&sessions).len(), MAX_LEARNED_ENTRIES);
    }

    #[test]
    fn learned_key_is_stable_for_the_same_group() {
        let sessions = vec![
            foo_bar_session("sess-1"),
            foo_bar_session("sess-1"),
            foo_bar_session("sess-2"),
        ];
        let first = analyze(&sessions);
        let second = analyze(&sessions);
        assert_eq!(learned_key(&first[0]), learned_key(&second[0]));
        assert!(learned_key(&first[0]).starts_with("learned:mytool:unknown-flag:"));
    }

    // -- claude/codex transcript extraction -----------------------------------

    fn claude_line(id: &str, command: &str) -> String {
        format!(
            "{{\"type\":\"assistant\",\"message\":{{\"content\":[\
             {{\"type\":\"tool_use\",\"id\":\"{id}\",\"name\":\"Bash\",\"input\":{{\"command\":\"{command}\"}}}}\
             ]}}}}"
        )
    }

    fn claude_result(id: &str, content: &str, is_error: bool) -> String {
        format!(
            "{{\"type\":\"user\",\"message\":{{\"content\":[\
             {{\"type\":\"tool_result\",\"tool_use_id\":\"{id}\",\"content\":\"{content}\",\"is_error\":{is_error}}}\
             ]}}}}"
        )
    }

    #[test]
    fn extract_claude_attempts_pairs_bash_tool_use_and_result_by_id() {
        let jsonl = format!(
            "{}\n{}\n",
            claude_line("tu1", "mytool --foo build"),
            claude_result("tu1", "error: unrecognized option '--foo'", true),
        );
        let attempts = extract_claude_attempts(&jsonl);
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].command, "mytool --foo build");
        assert_eq!(attempts[0].outcome, fail(ErrorClass::UnknownFlag));
    }

    #[test]
    fn extract_claude_attempts_skips_non_bash_tools() {
        let jsonl = "{\"type\":\"assistant\",\"message\":{\"content\":[\
             {\"type\":\"tool_use\",\"id\":\"tu1\",\"name\":\"Read\",\"input\":{\"file_path\":\"a.rs\"}}\
             ]}}\n";
        assert!(extract_claude_attempts(jsonl).is_empty());
    }

    #[test]
    fn extract_codex_attempts_unwraps_the_bash_lc_wrapper_and_pairs_by_call_id() {
        let jsonl = r#"{"type":"response_item","payload":{"type":"custom_tool_call","call_id":"call_1","name":"exec","command":["bash","-lc","mytool --foo build"]}}
{"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_1","output":[{"type":"input_text","text":"error: unrecognized option '--foo'"}]}}
"#;
        let attempts = extract_codex_attempts(jsonl);
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].command, "mytool --foo build");
        assert_eq!(attempts[0].outcome, fail(ErrorClass::UnknownFlag));
    }

    #[test]
    fn extract_codex_attempts_classifies_success_from_output_text_alone() {
        let jsonl = r#"{"type":"response_item","payload":{"type":"custom_tool_call","call_id":"call_1","name":"exec","command":["bash","-lc","mytool --bar build"]}}
{"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_1","output":[{"type":"input_text","text":"build ok"}]}}
"#;
        let attempts = extract_codex_attempts(jsonl);
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].outcome, Outcome::Success);
    }

    // -- run_with: dry-run and write/update paths -----------------------------

    fn cfg_with_memory_enabled(enabled: bool) -> CtxConfig {
        let mut cfg = CtxConfig::default();
        cfg.memory.enabled = enabled;
        cfg
    }

    fn write_claude_session(project_dir: &Path, session: &str, occurrences: usize) {
        let mut jsonl = String::new();
        for i in 0..occurrences {
            let id = format!("tu{i}");
            jsonl.push_str(&claude_line(&id, "mytool --foo build"));
            jsonl.push('\n');
            jsonl.push_str(&claude_result(
                &id,
                "error: unrecognized option '--foo'",
                true,
            ));
            jsonl.push('\n');
            let fix_id = format!("tu{i}-fix");
            jsonl.push_str(&claude_line(&fix_id, "mytool --bar build"));
            jsonl.push('\n');
            jsonl.push_str(&claude_result(&fix_id, "build ok", false));
            jsonl.push('\n');
        }
        std::fs::write(project_dir.join(format!("{session}.jsonl")), jsonl).expect("write fixture");
    }

    fn claude_project_dir(home: &Path, repo: &Path) -> std::path::PathBuf {
        home.join(".claude")
            .join("projects")
            .join(super::super::permissions::claude_project_dir_name(repo))
    }

    #[test]
    fn run_with_writes_exactly_one_learned_entry_for_a_recurring_correction() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("claude-home");
        let project_dir = claude_project_dir(&home, tmp.path());
        std::fs::create_dir_all(&project_dir).expect("mkdir project dir");
        write_claude_session(&project_dir, "sess-1", 2);
        write_claude_session(&project_dir, "sess-2", 1);
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);

        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = cfg_with_memory_enabled(true);
        let args = LearnArgs {
            since: "30d".to_string(),
            dry_run: false,
        };
        let mut out = Vec::new();
        let code =
            run_with(&state, &cfg, &args, tmp.path(), &mut out, state::now_secs()).expect("runs");
        assert_eq!(code, 0);

        let slug = repo_slug(tmp.path());
        let entries = memory::list(&state, &slug).expect("list");
        let learned: Vec<_> = entries
            .iter()
            .filter(|(_, e)| e.key.starts_with("learned:"))
            .collect();
        assert_eq!(learned.len(), 1, "{entries:?}");
        assert!(learned[0].1.key.starts_with("learned:mytool:unknown-flag:"));
    }

    #[test]
    fn run_with_dry_run_writes_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("claude-home");
        let project_dir = claude_project_dir(&home, tmp.path());
        std::fs::create_dir_all(&project_dir).expect("mkdir project dir");
        write_claude_session(&project_dir, "sess-1", 2);
        write_claude_session(&project_dir, "sess-2", 1);
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);

        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = cfg_with_memory_enabled(true);
        let args = LearnArgs {
            since: "30d".to_string(),
            dry_run: true,
        };
        let mut out = Vec::new();
        run_with(&state, &cfg, &args, tmp.path(), &mut out, state::now_secs()).expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("would write 1 learned entry"), "{text}");

        let slug = repo_slug(tmp.path());
        let entries = memory::list(&state, &slug).expect("list");
        assert!(entries.is_empty(), "{entries:?}");
    }

    #[test]
    fn run_with_updates_the_existing_entry_on_a_second_run_instead_of_duplicating() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("claude-home");
        let project_dir = claude_project_dir(&home, tmp.path());
        std::fs::create_dir_all(&project_dir).expect("mkdir project dir");
        write_claude_session(&project_dir, "sess-1", 2);
        write_claude_session(&project_dir, "sess-2", 1);
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);

        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = cfg_with_memory_enabled(true);
        let args = LearnArgs {
            since: "30d".to_string(),
            dry_run: false,
        };
        let mut out = Vec::new();
        run_with(&state, &cfg, &args, tmp.path(), &mut out, state::now_secs()).expect("first run");
        let mut out2 = Vec::new();
        run_with(
            &state,
            &cfg,
            &args,
            tmp.path(),
            &mut out2,
            state::now_secs(),
        )
        .expect("second run");

        let slug = repo_slug(tmp.path());
        let entries = memory::list(&state, &slug).expect("list");
        let learned: Vec<_> = entries
            .iter()
            .filter(|(_, e)| e.key.starts_with("learned:"))
            .collect();
        assert_eq!(
            learned.len(),
            1,
            "a rerun must update, not duplicate: {entries:?}"
        );
    }

    #[test]
    fn run_with_a_correction_seen_in_only_one_session_writes_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("claude-home");
        let project_dir = claude_project_dir(&home, tmp.path());
        std::fs::create_dir_all(&project_dir).expect("mkdir project dir");
        write_claude_session(&project_dir, "sess-1", 3);
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);

        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = cfg_with_memory_enabled(true);
        let args = LearnArgs {
            since: "30d".to_string(),
            dry_run: false,
        };
        let mut out = Vec::new();
        run_with(&state, &cfg, &args, tmp.path(), &mut out, state::now_secs()).expect("runs");

        let slug = repo_slug(tmp.path());
        let entries = memory::list(&state, &slug).expect("list");
        assert!(entries.is_empty(), "{entries:?}");
    }

    #[test]
    fn run_with_memory_disabled_writes_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("claude-home");
        let project_dir = claude_project_dir(&home, tmp.path());
        std::fs::create_dir_all(&project_dir).expect("mkdir project dir");
        write_claude_session(&project_dir, "sess-1", 2);
        write_claude_session(&project_dir, "sess-2", 1);
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);

        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = cfg_with_memory_enabled(false);
        let args = LearnArgs {
            since: "30d".to_string(),
            dry_run: false,
        };
        let mut out = Vec::new();
        run_with(&state, &cfg, &args, tmp.path(), &mut out, state::now_secs()).expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("memory is disabled"), "{text}");

        let slug = repo_slug(tmp.path());
        let entries = memory::list(&state, &slug).expect("list");
        assert!(entries.is_empty(), "{entries:?}");
    }
}
