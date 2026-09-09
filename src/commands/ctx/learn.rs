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
//!
//! The TDD/flaky-retry filter only ever excludes an EXACT repeat of the same
//! command (no token differs, so [`single_token_diff`] returns `None`);
//! everything else that keeps a wrong correction out -- an unrelated pair of
//! calls that happen to fall in the same window, a diff that does not match
//! its own error class's shape -- is [`classify_error`]'s and
//! [`diff_matches_class`]'s job, not a second, separate TDD detector.

use std::collections::{BTreeSet, HashMap};
use std::io::Write;
use std::path::Path;

use serde_json::Value;

use super::config::{CtxConfig, env_from_process};
use super::discover::{MAX_TRANSCRIPT_CANDIDATES, TRANSCRIPT_SCAN_BYTE_BUDGET, human_bytes};
use super::memory::{self, Entry};
use super::output::bare_program;
use super::search::{claude_candidates, codex_candidates};
use super::search_index::Source;
use super::state::{self, StateDir, repo_slug};
use super::{CtxResult, pace, safety};

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
    /// `at` is the token index (in the FAILING command's own token list)
    /// where the change happened -- used by [`diff_matches_class`] to check
    /// a diff's shape against its error class (e.g. `CommandNotFound` must
    /// touch the program name itself, token 0).
    Substitute {
        at: usize,
        from: String,
        to: String,
    },
    Insert {
        at: usize,
        token: String,
    },
    Remove {
        at: usize,
        token: String,
    },
}

impl TokenDiff {
    fn at(&self) -> usize {
        match self {
            TokenDiff::Substitute { at, .. }
            | TokenDiff::Insert { at, .. }
            | TokenDiff::Remove { at, .. } => *at,
        }
    }

    /// The token(s) this diff actually changed, for [`diff_matches_class`]'s
    /// flag-shape check -- both sides of a substitution (either could be the
    /// flag), or the one inserted/removed token.
    fn changed_tokens(&self) -> [Option<&str>; 2] {
        match self {
            TokenDiff::Substitute { from, to, .. } => [Some(from.as_str()), Some(to.as_str())],
            TokenDiff::Insert { token, .. } | TokenDiff::Remove { token, .. } => {
                [Some(token.as_str()), None]
            }
        }
    }

    /// Redacted through `pace::redact_for_log` (issue #425 review, round 2):
    /// belt and braces alongside [`diff_looks_secret`] filtering a
    /// secret-shaped diff out of [`find_corrections`] entirely -- this is
    /// the LAST place raw diff text turns into something written to disk
    /// (`learned_key`) or printed (`--dry-run`), so it redacts on its own
    /// rather than trusting every caller to have filtered already.
    fn key_fragment(&self) -> String {
        match self {
            TokenDiff::Substitute { from, to, .. } => format!(
                "{}->{}",
                pace::redact_for_log(from),
                pace::redact_for_log(to)
            ),
            TokenDiff::Insert { token, .. } => format!("+{}", pace::redact_for_log(token)),
            TokenDiff::Remove { token, .. } => format!("-{}", pace::redact_for_log(token)),
        }
    }

    fn describe(&self) -> String {
        match self {
            TokenDiff::Substitute { from, to, .. } => format!("use `{to}` instead of `{from}`"),
            TokenDiff::Insert { token, .. } => format!("add `{token}`"),
            TokenDiff::Remove { token, .. } => format!("drop `{token}`"),
        }
    }
}

/// Whether `diff`'s own shape is even plausible for `class` -- a cheap
/// consistency check alongside [`classify_error`]'s text-based read of the
/// FAILURE, now checking the FIX's shape too (issue #425 review): an
/// `UnknownFlag` correction must actually touch a `-`-leading token (on
/// either side of a substitution, or the inserted/removed token), and a
/// `CommandNotFound` correction must touch the program name itself, token 0
/// -- anything else pairs a real class with an implausible fix (a quoted
/// commit-message argument that merely reworded itself, an unrelated
/// argument that changed for its own reason) and is rejected here rather
/// than written down as if it were the actual correction. Every other class
/// is unconstrained: a wrong path or a missing/extra argument can land
/// anywhere in the command.
fn diff_matches_class(class: ErrorClass, diff: &TokenDiff) -> bool {
    match class {
        ErrorClass::UnknownFlag => diff
            .changed_tokens()
            .into_iter()
            .flatten()
            .any(|t| t.starts_with('-')),
        ErrorClass::CommandNotFound => diff.at() == 0,
        ErrorClass::WrongPath | ErrorClass::MissingArgument | ErrorClass::PermissionDenied => true,
    }
}

/// Whether any token this diff touches looks secret-shaped under
/// `pace::redact_for_log` (issue #425 review, round 2): with quote-aware
/// tokens, the differing token itself can be a whole secret-bearing
/// argument (`"Authorization: Bearer sk-OLD"` -> `"Authorization: Bearer
/// sk-NEW"`, classified `PermissionDenied` -- a class `diff_matches_class`
/// deliberately leaves unconstrained, since a wrong path or missing
/// argument fix can land anywhere). A secret ROTATION is never a learnable
/// command correction -- it is not a mistake anyone should be reminded not
/// to repeat -- so a diff that redacts to something different than it
/// started as is rejected here, before it ever reaches a group, a memory
/// key, or a printed line.
fn diff_looks_secret(diff: &TokenDiff) -> bool {
    diff.changed_tokens()
        .into_iter()
        .flatten()
        .any(|t| pace::redact_for_log(t) != t)
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
/// threshold. `a`/`b` are quote-aware tokens (see [`quoted_tokens`]), so a
/// quoted argument containing embedded whitespace (`-m "fix the bug"`) is
/// one token, not several.
fn single_token_diff(a: &[String], b: &[String]) -> Option<TokenDiff> {
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
            at: i,
            from: a[i].clone(),
            to: b[i].clone(),
        });
    }
    if b.len() == a.len() + 1 {
        return single_insertion(a, b).map(|(at, token)| TokenDiff::Insert { at, token });
    }
    if a.len() == b.len() + 1 {
        return single_insertion(b, a).map(|(at, token)| TokenDiff::Remove { at, token });
    }
    None
}

/// `longer` is exactly one token longer than `shorter`; returns `(index,
/// token)` for the extra token when removing it from `longer` (at whichever
/// position) leaves `shorter` exactly, `None` if no single removal does.
fn single_insertion(shorter: &[String], longer: &[String]) -> Option<(usize, String)> {
    let mut i = 0;
    while i < shorter.len() && shorter[i] == longer[i] {
        i += 1;
    }
    if shorter[i..] == longer[i + 1..] {
        Some((i, longer[i].clone()))
    } else {
        None
    }
}

/// Quote-aware tokenization of a raw command string, reusing `safety::
/// tokenize_quoted` (issue #425 review) rather than `split_whitespace`: a
/// quoted argument with embedded whitespace (`git commit -m "fix the bug"`)
/// must stay one token, or an unrelated wording change inside the quotes
/// reads as a spurious multi-token diff.
fn quoted_tokens(command: &str) -> Vec<String> {
    let chars: Vec<char> = command.chars().collect();
    safety::tokenize_quoted(&chars)
        .into_iter()
        .map(|t| t.text)
        .collect()
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
        let fail_tokens = quoted_tokens(&attempts[i].command);
        let end = (i + 1 + LOOKAHEAD).min(attempts.len());
        for candidate in attempts.iter().take(end).skip(i + 1) {
            if candidate.outcome != Outcome::Success {
                continue;
            }
            let fix_tokens = quoted_tokens(&candidate.command);
            let Some(diff) = single_token_diff(&fail_tokens, &fix_tokens) else {
                continue;
            };
            if !diff_matches_class(class, &diff) {
                continue;
            }
            if diff_looks_secret(&diff) {
                continue;
            }
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

/// Every piece of transcript-derived text goes through `pace::redact_for_log`
/// before it is embedded (issue #425 review): a failing command carrying a
/// bearer token or API key (`curl -H "Authorization: Bearer sk-..." --badflag`)
/// must never write that secret into cross-session memory just because it
/// happened to sit next to the actual mistake.
fn describe_group(group: &CorrectionGroup) -> String {
    let fix = pace::redact_for_log(&group.diff.describe());
    let example_from = pace::redact_for_log(&group.example_from);
    let example_to = pace::redact_for_log(&group.example_to);
    format!(
        "{} -- seen {} time{} across {} session{}.\n\n- Program: `{}`\n- Fix: {}\n- Example failing command: `{}`\n- Example fixed command: `{}`\n",
        group.class.label(),
        group.occurrences,
        if group.occurrences == 1 { "" } else { "s" },
        group.sessions.len(),
        if group.sessions.len() == 1 { "" } else { "s" },
        group.program,
        fix,
        example_from,
        example_to,
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

/// [`gather_sessions`]'s outcome: the sessions found, plus enough to print
/// the same "stopped after ... narrow --since" note `discover.rs` prints
/// when its own identical budget cuts a scan short (issue #425 review).
struct ScanOutcome {
    sessions: Vec<(String, Vec<Attempt>)>,
    files_scanned: usize,
    bytes_scanned: u64,
    truncated: bool,
}

/// Reads `path` up to `remaining` bytes, never more. Lossy-decoded so a cut
/// mid multi-byte character degrades to one replacement character rather
/// than failing the whole read -- an incomplete final JSONL line already
/// fails `serde_json::from_str` and is silently skipped, the identical
/// tolerance this module's own extractors already give any malformed row.
/// Returns `(text, bytes_read, file_had_more_left)`.
fn read_bounded(path: &Path, remaining: u64) -> Option<(String, u64, bool)> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let file_len = file.metadata().ok()?.len();
    let mut buf = Vec::new();
    file.take(remaining).read_to_end(&mut buf).ok()?;
    let consumed = buf.len() as u64;
    Some((
        String::from_utf8_lossy(&buf).into_owned(),
        consumed,
        file_len > consumed,
    ))
}

/// Scans every claude/codex transcript candidate modified at or after
/// `since_ts`, newest-first, stopping at whichever of `discover.rs`'s own
/// scan budget limits (`TRANSCRIPT_SCAN_BYTE_BUDGET`/
/// `MAX_TRANSCRIPT_CANDIDATES`, shared rather than a second hardcoded pair,
/// issue #425 review) comes first -- a `--since` window wide enough to
/// match a machine with years of transcript history must never OOM or
/// stall this command any more than it may stall `discover`.
fn gather_sessions(repo: &Path, since_ts: u64) -> ScanOutcome {
    let mut candidates: Vec<(std::path::PathBuf, Source, u64)> = Vec::new();
    for (path, source) in claude_candidates(repo, false)
        .into_iter()
        .chain(codex_candidates())
    {
        let Some(modified) = modified_secs(&path) else {
            continue;
        };
        if modified < since_ts {
            continue;
        }
        candidates.push((path, source, modified));
    }
    candidates.sort_by_key(|(_, _, modified)| std::cmp::Reverse(*modified));

    let mut sessions = Vec::new();
    let mut files_scanned = 0usize;
    let mut bytes_scanned: u64 = 0;
    let mut truncated = false;
    for (path, source, _modified) in candidates {
        if files_scanned >= MAX_TRANSCRIPT_CANDIDATES {
            truncated = true;
            break;
        }
        let remaining = TRANSCRIPT_SCAN_BYTE_BUDGET.saturating_sub(bytes_scanned);
        if remaining == 0 {
            truncated = true;
            break;
        }
        let Some((text, consumed, file_truncated)) = read_bounded(&path, remaining) else {
            continue;
        };
        files_scanned += 1;
        bytes_scanned += consumed;
        let attempts = match source {
            Source::Claude => extract_claude_attempts(&text),
            Source::Codex => extract_codex_attempts(&text),
            _ => continue,
        };
        if !attempts.is_empty() {
            sessions.push((session_id_from_path(&path), attempts));
        }
        if file_truncated {
            truncated = true;
            break;
        }
    }
    ScanOutcome {
        sessions,
        files_scanned,
        bytes_scanned,
        truncated,
    }
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

    let outcome = gather_sessions(repo, since_ts);
    if outcome.truncated {
        writeln!(
            w,
            "note: stopped after {} files / {}; narrow --since",
            outcome.files_scanned,
            human_bytes(outcome.bytes_scanned)
        )?;
    }
    let groups = analyze(&outcome.sessions);
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

    // Written in RANK order (`analyze` already sorts strongest-first) with a
    // distinct, descending `written` second per entry rather than one shared
    // `now` (issue #425 review): `memory::remember`'s own `prune_to_cap`
    // evicts whichever entries have the SMALLEST `written` first once a bank
    // is over its cap (default `memory.max_entries = 50`), and every entry
    // sharing one identical timestamp would tie-break on directory-listing
    // order instead -- arbitrary, and just as likely to evict this run's
    // strongest finding as its weakest. Staggering by rank makes the weakest
    // of THIS run's own entries the first ones sacrificed, never the
    // strongest, without changing anything for a bank nowhere near its cap.
    let slug = repo_slug(repo);
    for (idx, group) in groups.iter().enumerate() {
        let written = now.saturating_sub(idx as u64);
        let entry = Entry {
            key: learned_key(group),
            written_by: "learn".to_string(),
            written,
            verified: written,
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

    fn tokens(command: &str) -> Vec<String> {
        quoted_tokens(command)
    }

    #[test]
    fn single_token_diff_finds_a_substitution() {
        let a = tokens("mytool --foo build");
        let b = tokens("mytool --bar build");
        assert_eq!(
            single_token_diff(&a, &b),
            Some(TokenDiff::Substitute {
                at: 1,
                from: "--foo".to_string(),
                to: "--bar".to_string(),
            })
        );
    }

    #[test]
    fn single_token_diff_finds_an_insertion() {
        let a = tokens("mytool build");
        let b = tokens("mytool build --release");
        assert_eq!(
            single_token_diff(&a, &b),
            Some(TokenDiff::Insert {
                at: 2,
                token: "--release".to_string(),
            })
        );
    }

    #[test]
    fn single_token_diff_finds_a_removal() {
        let a = tokens("mytool --bogus build");
        let b = tokens("mytool build");
        assert_eq!(
            single_token_diff(&a, &b),
            Some(TokenDiff::Remove {
                at: 1,
                token: "--bogus".to_string(),
            })
        );
    }

    #[test]
    fn single_token_diff_is_none_for_identical_commands() {
        let a = tokens("cargo test");
        assert_eq!(single_token_diff(&a, &a), None);
    }

    #[test]
    fn single_token_diff_is_none_when_more_than_one_token_differs() {
        let a = tokens("mytool --foo build");
        let b = tokens("othertool --bar test");
        assert_eq!(single_token_diff(&a, &b), None);
    }

    #[test]
    fn single_token_diff_keeps_a_quoted_argument_as_one_token() {
        let a = tokens(r#"git commit -m "fix bug""#);
        let b = tokens(r#"git commit -m "fix the bug""#);
        assert_eq!(
            single_token_diff(&a, &b),
            Some(TokenDiff::Substitute {
                at: 3,
                from: "\"fix bug\"".to_string(),
                to: "\"fix the bug\"".to_string(),
            }),
            "a quoted, multi-word argument must diff as ONE token, not several"
        );
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
                at: 1,
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

    // -- quote-awareness and class-consistency (issue #425 review) -----------

    #[test]
    fn find_corrections_never_reads_a_quoted_message_reword_as_an_unknown_flag_fix() {
        // Only the QUOTED commit message changed ("bug" -> "the bug"); the
        // actual `-m` flag never moved. A whitespace-split tokenizer would
        // have split the message into words and seen a plausible one-word
        // diff here -- the quote-aware tokenizer keeps it one token, and the
        // class-consistency check rejects it outright since neither side of
        // that token starts with `-`.
        let attempts = vec![
            attempt(r#"git commit -m "fix bug""#, fail(ErrorClass::UnknownFlag)),
            attempt(r#"git commit -m "fix the bug""#, Outcome::Success),
        ];
        assert!(find_corrections(&attempts).is_empty());
    }

    #[test]
    fn find_corrections_pairs_a_program_name_typo_fix_on_command_not_found() {
        let attempts = vec![
            attempt("foo x", fail(ErrorClass::CommandNotFound)),
            attempt("./foo x", Outcome::Success),
        ];
        let corrections = find_corrections(&attempts);
        assert_eq!(corrections.len(), 1, "expected exactly one correction");
        assert_eq!(
            corrections[0].diff,
            TokenDiff::Substitute {
                at: 0,
                from: "foo".to_string(),
                to: "./foo".to_string(),
            }
        );
    }

    #[test]
    fn find_corrections_rejects_an_unknown_flag_diff_that_never_touches_a_flag() {
        // The class says "unknown flag", but the only thing that changed
        // between the two commands is an ordinary positional argument --
        // not a plausible fix for that failure, so no correction forms.
        let attempts = vec![
            attempt("cargo test a", fail(ErrorClass::UnknownFlag)),
            attempt("cargo test b", Outcome::Success),
        ];
        assert!(find_corrections(&attempts).is_empty());
    }

    #[test]
    fn find_corrections_rejects_a_bearer_token_rotation_even_under_an_unconstrained_class() {
        // issue #425 review, round 2: `PermissionDenied` is deliberately
        // unconstrained by `diff_matches_class` (a wrong path or missing
        // argument fix can land anywhere), so without the secret filter this
        // would otherwise pair as a "correction" -- but rotating a bearer
        // token is not a learnable command mistake, and must never be
        // described or persisted.
        let attempts = vec![
            attempt(
                r#"curl -H "Authorization: Bearer sk-OLD" example.com"#,
                fail(ErrorClass::PermissionDenied),
            ),
            attempt(
                r#"curl -H "Authorization: Bearer sk-NEW" example.com"#,
                Outcome::Success,
            ),
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

    // -- describe_group: redaction (issue #425 review) -------------------------

    #[test]
    fn describe_group_never_persists_a_bearer_token_from_the_example_commands() {
        let group = CorrectionGroup {
            program: "curl".to_string(),
            class: ErrorClass::UnknownFlag,
            diff: TokenDiff::Substitute {
                at: 3,
                from: "--badflag".to_string(),
                to: "--good-flag".to_string(),
            },
            occurrences: 3,
            sessions: BTreeSet::from(["sess-1".to_string(), "sess-2".to_string()]),
            example_from: "curl -H \"Authorization: Bearer sk-abc123XYZ\" --badflag".to_string(),
            example_to: "curl -H \"Authorization: Bearer sk-abc123XYZ\" --good-flag".to_string(),
        };
        let body = describe_group(&group);
        assert!(
            !body.contains("sk-abc123XYZ"),
            "the raw token must never be persisted: {body}"
        );
        assert!(body.contains("[redacted]"), "{body}");
    }

    #[test]
    fn learned_key_never_persists_a_raw_secret_value_from_the_diff() {
        // Belt and braces (issue #425 review, round 2): even a diff that
        // somehow reached `learned_key` still carrying a secret-shaped
        // `token=`/`key=` value must never write that value into the
        // persisted key -- `find_corrections`'s own `diff_looks_secret`
        // filter is the first line of defense, this is the last.
        let group = CorrectionGroup {
            program: "curl".to_string(),
            class: ErrorClass::PermissionDenied,
            diff: TokenDiff::Substitute {
                at: 1,
                from: "token=sk-abc123OLD".to_string(),
                to: "token=sk-abc123NEW".to_string(),
            },
            occurrences: 3,
            sessions: BTreeSet::from(["sess-1".to_string(), "sess-2".to_string()]),
            example_from: "curl --token=sk-abc123OLD".to_string(),
            example_to: "curl --token=sk-abc123NEW".to_string(),
        };
        let key = learned_key(&group);
        assert!(!key.contains("sk-abc123OLD"), "{key}");
        assert!(!key.contains("sk-abc123NEW"), "{key}");
        assert!(key.contains("[redacted]"), "{key}");
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

    // -- scan budget (issue #425 review) ---------------------------------------

    #[test]
    fn run_with_stops_at_the_file_cap_and_reports_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("claude-home");
        let project_dir = claude_project_dir(&home, tmp.path());
        std::fs::create_dir_all(&project_dir).expect("mkdir project dir");
        // One more candidate file than the shared file cap allows -- none of
        // them need a matched tool_result; `gather_sessions`'s file-cap
        // bookkeeping runs regardless of what (if anything) is extracted.
        for i in 0..(MAX_TRANSCRIPT_CANDIDATES + 1) {
            std::fs::write(
                project_dir.join(format!("sess-{i}.jsonl")),
                claude_line("tu0", "ls"),
            )
            .expect("write fixture");
        }
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
        assert!(
            text.contains(&format!("stopped after {MAX_TRANSCRIPT_CANDIDATES} files")),
            "{text}"
        );
        assert!(text.contains("narrow --since"), "{text}");
    }

    // -- prune_to_cap ordering (issue #425 review) -----------------------------

    fn program_session_jsonl(program: &str, occurrences: usize) -> String {
        let mut jsonl = String::new();
        for i in 0..occurrences {
            let fail_id = format!("{program}-fail-{i}");
            jsonl.push_str(&claude_line(&fail_id, &format!("{program} --foo build")));
            jsonl.push('\n');
            jsonl.push_str(&claude_result(
                &fail_id,
                "error: unrecognized option '--foo'",
                true,
            ));
            jsonl.push('\n');
            let fix_id = format!("{program}-fix-{i}");
            jsonl.push_str(&claude_line(&fix_id, &format!("{program} --bar build")));
            jsonl.push('\n');
            jsonl.push_str(&claude_result(&fix_id, "build ok", false));
            jsonl.push('\n');
        }
        jsonl
    }

    /// `memory.max_entries` defaults to 50 (`config.rs`'s own
    /// `MemoryConfig::default`); this test pins it to 1 so a single
    /// `remember` call is already enough to force `prune_to_cap` to choose.
    /// Both findings clear the `analyze` threshold (at least 3 occurrences
    /// across at least 2 sessions) -- "strong" simply has more occurrences,
    /// so it must be the one still standing once the private bank cannot
    /// hold both.
    #[test]
    fn run_with_writes_the_strongest_finding_first_so_a_near_cap_bank_evicts_the_weakest() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("claude-home");
        let project_dir = claude_project_dir(&home, tmp.path());
        std::fs::create_dir_all(&project_dir).expect("mkdir project dir");
        std::fs::write(
            project_dir.join("strong-1.jsonl"),
            program_session_jsonl("strong", 4),
        )
        .expect("write fixture");
        std::fs::write(
            project_dir.join("strong-2.jsonl"),
            program_session_jsonl("strong", 1),
        )
        .expect("write fixture");
        std::fs::write(
            project_dir.join("weak-1.jsonl"),
            program_session_jsonl("weak", 2),
        )
        .expect("write fixture");
        std::fs::write(
            project_dir.join("weak-2.jsonl"),
            program_session_jsonl("weak", 1),
        )
        .expect("write fixture");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);

        let state = StateDir::from_root(tmp.path().join("state"));
        let mut cfg = cfg_with_memory_enabled(true);
        cfg.memory.max_entries = 1;
        let args = LearnArgs {
            since: "30d".to_string(),
            dry_run: false,
        };
        let mut out = Vec::new();
        run_with(&state, &cfg, &args, tmp.path(), &mut out, state::now_secs()).expect("runs");

        let slug = repo_slug(tmp.path());
        let entries = memory::list(&state, &slug).expect("list");
        let learned: Vec<&str> = entries
            .iter()
            .map(|(_, e)| e.key.as_str())
            .filter(|k| k.starts_with("learned:"))
            .collect();
        assert_eq!(learned.len(), 1, "{learned:?}");
        assert!(
            learned[0].starts_with("learned:strong:"),
            "the higher-ranked finding must survive the cap, got {learned:?}"
        );
    }
}
