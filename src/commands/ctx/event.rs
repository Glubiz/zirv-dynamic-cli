use std::path::PathBuf;

/// FNV-1a 64. Hand-rolled rather than `DefaultHasher` because the rot engine
/// must be deterministic across compiler versions.
pub fn input_hash(input: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// How much of a tool result's error text [`normalize_error_text`] keeps
/// before capping: enough to distinguish genuinely different errors, short
/// enough that a pathological transcript (a megabyte stack trace) costs
/// nothing to hash.
const ERROR_TEXT_CHAR_CAP: usize = 400;

/// Normalizes tool-result error text into a fuzzy fingerprint for "looks
/// like the same error", feeding `rot::Signals::same_error_repeats`: a hex
/// literal (`0x...`) collapses to `0x#`, a run of decimal digits collapses
/// to a single `#` (which also folds most line numbers and randomized
/// temp-path segments), and a run of whitespace collapses to a single
/// space, before the result is capped to [`ERROR_TEXT_CHAR_CAP`] characters.
/// Deliberately small, like `rot::MARKER_LEAD`'s own hand-rolled approach:
/// this is a fingerprint for "same error, different attempt", not a lexer,
/// so it never tries to fully canonicalize a filesystem path.
pub fn normalize_error_text(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(chars.len().min(ERROR_TEXT_CHAR_CAP));
    let mut out_len = 0usize;
    let mut i = 0usize;
    let mut last_was_space = false;

    while i < chars.len() && out_len < ERROR_TEXT_CHAR_CAP {
        let c = chars[i];
        if c == '0'
            && chars.get(i + 1) == Some(&'x')
            && chars.get(i + 2).is_some_and(char::is_ascii_hexdigit)
        {
            let mut j = i + 2;
            while j < chars.len() && chars[j].is_ascii_hexdigit() {
                j += 1;
            }
            out.push_str("0x#");
            out_len += 3;
            i = j;
            last_was_space = false;
            continue;
        }
        if c.is_ascii_digit() {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            out.push('#');
            out_len += 1;
            i = j;
            last_was_space = false;
            continue;
        }
        if c.is_whitespace() {
            if !last_was_space {
                out.push(' ');
                out_len += 1;
            }
            last_was_space = true;
            i += 1;
            continue;
        }
        out.push(c);
        out_len += 1;
        last_was_space = false;
        i += 1;
    }
    out.trim().to_string()
}

/// [`input_hash`] of [`normalize_error_text`]'s output -- the fingerprint
/// `rot::Signals::same_error_repeats` actually compares.
pub fn error_text_hash(text: &str) -> u64 {
    input_hash(&normalize_error_text(text))
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(String);

impl SessionId {
    pub fn new_v4() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    pub fn parse(raw: &str) -> Self {
        Self(raw.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone)]
pub struct SessionRef {
    pub id: SessionId,
    pub cwd: PathBuf,
}

/// Cumulative token usage read from a harness transcript, in the four RAW
/// classes the provider bills separately. Adapters return `None` when their
/// transcript does not expose a verified usage shape, and `0` for a class
/// their transcript genuinely does not report (codex's cumulative
/// `TokenCount` totals carry no cache classes) -- never a guess.
///
/// Recorded raw, not pre-summed (issue #155, 2026-08-26): cache CREATION is
/// expensive and written once, cache READ is cheap and dominant in a healthy
/// session, and folding them together at the adapter boundary made the
/// cache-hit ratio -- the one number that says whether prompt-shape work
/// helped -- uncomputable anywhere downstream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TranscriptUsage {
    pub input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub output_tokens: u64,
}

impl TranscriptUsage {
    /// The combined "real context size" figure this type carried in
    /// `input_tokens` before 2.34.0: uncached input plus both cache classes.
    /// Every caller that genuinely wants ONE context-size number -- rot's
    /// token gate, status display -- calls this. Saturating, like every other
    /// token arithmetic in this crate.
    pub fn context_total(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.cache_creation_input_tokens)
            .saturating_add(self.cache_read_input_tokens)
    }
}

/// The only currency the rot engine and supervisors understand.
///
/// `AssistantFinal` is emitted for every assistant message: `text` holds the
/// concatenated text blocks and is empty for tool-only or thinking-only
/// messages. The marker signal groups by turn and takes the last non-empty
/// text; the token gate takes the most recent event's `input_tokens`
/// regardless of text, so mid-turn token growth is visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderErrorClass {
    Overflow,
    RateLimit,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ModelChange {
    pub from: String,
    pub to: String,
    pub turns_ago: usize,
    pub limit_pressure: bool,
}

/// Wall-clock speed signals for the events a `Score` was computed over
/// (issue #293): per-turn latency (`TurnStart` to that turn's last
/// `AssistantFinal`), time-to-first-text (`TurnStart` to that turn's first
/// `AssistantFirstText`), and the tool-error rate over the same events.
/// Derived in `score::derive_speed_metrics`, attached onto `Score` the same
/// post-hoc way `score::attach_model_change` already attaches
/// `Score::model_change` -- `rot.rs`'s own scoring functions always leave
/// this `None`, never reason about it, and never call a clock to fill it.
/// Every field is `None`, never `0`, when its underlying samples are empty:
/// a session with no timed turns yet has UNKNOWN speed, not zero-latency
/// speed.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct SpeedMetrics {
    pub turn_p50_ms: Option<u64>,
    pub turn_max_ms: Option<u64>,
    pub ttft_p50_ms: Option<u64>,
    pub tool_error_rate: Option<f64>,
}

impl SpeedMetrics {
    /// All-`None`: no timed samples at all. `derive_speed_metrics` returns
    /// this for an events slice with no usable timestamps, and it is what a
    /// consumer should treat as "nothing to report".
    pub fn is_empty(&self) -> bool {
        self.turn_p50_ms.is_none()
            && self.turn_max_ms.is_none()
            && self.ttft_p50_ms.is_none()
            && self.tool_error_rate.is_none()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum NormalizedEvent {
    /// `at_ms` is the unix-millisecond wall-clock time this turn began, read
    /// by the adapter from the transcript line it already parses (issue
    /// #293, `window::parse_iso8601_utc_ms`). `None` when the line carried no
    /// parseable timestamp -- never a guess, and never `0`.
    TurnStart {
        at_ms: Option<u64>,
    },
    /// A CANDIDATE first-non-empty-assistant-text point, emitted right
    /// before the `AssistantFinal` that carries the same text, whenever that
    /// text is non-empty (issue #293). Deliberately per-row, not tracked
    /// across a `parse_events` call: `AgentAdapter::parse_events` must stay
    /// line-local (a transcript cut at newlines and parsed piecewise must
    /// yield exactly the events one whole-file parse yields -- the
    /// incremental scoring path depends on it), so an adapter cannot
    /// remember "have I already seen this turn's first text" across calls.
    /// More than one of these can appear inside a single turn; the ONE
    /// consumer that needs the true first-per-turn value
    /// (`score::derive_speed_metrics`) reduces the stream itself, keeping
    /// only the earliest one seen since the last `TurnStart`. This is the
    /// distinction Prime's own telemetry draws between "visible TTFT" (first
    /// non-empty text) and "first model event" (any model activity at all,
    /// which `AssistantFinal`/`ToolCall` already cover) -- see the issue's
    /// Origin section. An adapter that cannot identify any assistant text at
    /// all (a future one with no verified shape for it) simply never emits
    /// this variant.
    AssistantFirstText {
        at_ms: Option<u64>,
    },
    AssistantFinal {
        text: String,
        input_tokens: u64,
        /// Issue #293: same rules as `TurnStart::at_ms`.
        at_ms: Option<u64>,
    },
    ToolCall {
        name: String,
        input_hash: u64,
        /// Issue #293: same rules as `TurnStart::at_ms`.
        at_ms: Option<u64>,
    },
    ToolResult {
        is_error: bool,
    },
    /// The normalized, hashed error text of the immediately preceding
    /// `ToolResult { is_error: true }`, emitted right after it -- never in
    /// place of it -- whenever the adapter could extract error text.
    /// Deliberately a SEPARATE variant rather than a new field on
    /// `ToolResult`: `ToolResult` is matched with exhaustive field patterns
    /// (no `..`) in several places across the crate, and a fielded addition
    /// there would have broken every one of them, whereas a new variant
    /// only ever needs the `_ => {}` fallthrough every one of those matches
    /// already has. Feeds `rot::Signals::same_error_repeats`.
    ToolErrorText {
        hash: u64,
    },
    /// The wall-clock timestamp of the immediately preceding `ToolResult`,
    /// emitted right after it -- never in place of it -- whenever the
    /// adapter could extract one (issue #293). A SEPARATE variant for
    /// exactly the reason `ToolErrorText` (above) already is one: `ToolResult`
    /// is matched with exhaustive field patterns in several places across
    /// the crate, and no metric `score.rs` derives from it today
    /// (`tool_error_rate` is a ratio, not time-based) needs a per-result
    /// timestamp, so a fielded addition there would have touched every one
    /// of those matches for a signal nothing yet reads. The issue's own
    /// decision explicitly authorizes this fallback for `ToolResult`
    /// specifically (unlike `TurnStart`/`AssistantFinal`/`ToolCall`, which
    /// took the field directly).
    ToolResultTimestamp {
        at_ms: Option<u64>,
    },
    /// The literal byte length of a human-typed user turn's text (issue
    /// #312), emitted alongside `TurnStart` whenever that row carries
    /// extractable text -- never in place of it, and never emitted at all for
    /// a `TurnStart` whose row had no text (a tool-only or empty row). Feeds
    /// `breakdown::attribute_window`'s `user_text` bucket; nothing else reads
    /// it. A sibling variant rather than a field on `TurnStart` for the same
    /// reason `ToolErrorText` is one: an exhaustive-pattern addition would
    /// have touched every existing `TurnStart { .. }` match for a signal only
    /// one consumer needs.
    UserText {
        byte_len: u64,
    },
    /// The combined byte length of every `thinking`-block text in one
    /// assistant row (issue #312), emitted right after that row's
    /// `AssistantFinal` whenever the row carried at least one such block.
    /// `text_of` already drops thinking blocks entirely when building
    /// `AssistantFinal::text`, so without this sibling event that content is
    /// invisible to `breakdown::attribute_window`'s `thinking` bucket.
    AssistantThinking {
        byte_len: u64,
    },
    /// The byte length and content fingerprint of the immediately preceding
    /// `ToolResult`'s raw content (issue #312), emitted right after it --
    /// never in place of it, mirroring `ToolErrorText`/`ToolResultTimestamp`'s
    /// own sibling-variant rationale. `content_hash` is `input_hash` of the
    /// FULL result text (not `normalize_error_text`'s fuzzy, capped
    /// fingerprint): `breakdown::attribute_window` uses it to dedupe
    /// byte-identical results, which needs an exact match, not a fuzzy one.
    ToolResultSize {
        byte_len: u64,
        content_hash: u64,
    },
    /// The file-shaped path argument of the immediately preceding `ToolCall`
    /// (issue #312), emitted right after it whenever the adapter's transcript
    /// shape exposes one -- omitted entirely for a call with no recognizable
    /// path argument, never a guessed empty string. `is_modification` mirrors
    /// `StructuralContext::files_modified`'s own read/write split: `true` for
    /// an edit-shaped tool (`Edit`/`Write`/`MultiEdit`/`NotebookEdit`), `false`
    /// for a read-shaped one. Lets `breakdown::attribute_window` mark an
    /// earlier live tool result STALE once a later `ToolCallPath` with
    /// `is_modification: true` names the same path -- a sibling variant for
    /// the same reason `ToolErrorText` is one.
    ToolCallPath {
        path: String,
        is_modification: bool,
    },
    ProviderError {
        class: ProviderErrorClass,
    },
    ModelId {
        id: String,
    },
    Compaction,
}

/// Which rot signals an adapter can actually feed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Capabilities {
    pub marker_signal: bool,
    pub token_usage: bool,
    pub turn_signal: bool,
    /// Whether this agent has a verified per-run system-prompt mechanism.
    pub system_prompt: bool,
    /// Whether `AgentAdapter::parse_events`/`structural_context` can ever
    /// produce real data for this agent, as opposed to the empty/default
    /// stub every "no verified mechanism" adapter ships (codex today, see
    /// issue #11). `false` here is not "unhealthy" -- `rot.rs` stays pure
    /// and never reads this field at all, since a session this adapter
    /// cannot parse is not zero signals of health, it is *no data*. The
    /// scoring callers that build a `Score`/verdict from real events
    /// (`score.rs`'s `full_score`/`IncrementalScorer::poll`) are what read
    /// it, to report "no data" instead of a false `Healthy`/`0` built from
    /// an always-empty parse -- the same distinction `score::cached_score`'s
    /// own doc comment already draws for a missing transcript.
    pub events: bool,
    /// Whether this agent's own composer folds a same-write trailing `\r`
    /// into pasted text, so an injection into it must write the text and its
    /// submitting `\r` as two genuinely separate writes rather than one
    /// burst (issue #118). Verified `true` for codex against its ratatui
    /// composer (issue #114, fixed for the dashboard in PR #116); `false`
    /// for claude, whose composer submits a same-burst trailing `\r`
    /// correctly. See `wrap::write_mail_advisory` and
    /// `dash::pane::inject_visible` for the two callers this actually
    /// changes behavior for.
    pub defer_injection_submit: bool,
    /// The model's usable context window, when the adapter can state one
    /// (issue #155). `None` means "unknown", which `rot::token_gates` reads
    /// as "use the absolute `score.token_floor`/`token_ceiling` defaults" --
    /// never as a guess. Delivered inside `Capabilities` deliberately: this
    /// struct is already an input to `rot::score_events` and
    /// `RotState::score`, so capacity reaches the rot engine without adding
    /// a single fs, clock or env read to a module that must stay pure.
    pub context_window_tokens: Option<u64>,
}

/// Raw material for handoffs, extracted per-agent because it needs fields the
/// normalized stream deliberately drops (message text, tool inputs).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct StructuralContext {
    pub user_messages: Vec<String>,
    pub assistant_texts: Vec<String>,
    /// Paths seen only through a read-shaped tool (`Read`/`Grep`/`Glob`, or
    /// an unrecognised tool carrying a file key -- the conservative
    /// direction, since claiming a file was edited when it was not is the
    /// damaging error).
    pub files_read: Vec<String>,
    /// Paths seen through a modification-shaped tool (`Edit`/`Write`/
    /// `MultiEdit`/`NotebookEdit`, and the codex equivalents).
    pub files_modified: Vec<String>,
    pub tool_errors: Vec<String>,
    /// The session's last build/test/lint run, when an adapter could
    /// identify one ([`last_verification_run`]). `None` means no verified
    /// invocation looked like one -- distinct from one the adapter saw and
    /// confirmed passed. Rendered by `handoff::structural`'s `Verification`
    /// section.
    pub last_verification: Option<VerificationOutcome>,
}

/// One command-shaped tool invocation and whether its result errored,
/// captured with its raw command text -- unlike `NormalizedEvent::ToolCall`,
/// which only ever carries a hash of the input, this exists purely for
/// [`last_verification_run`]'s command-text heuristic and is never fed into
/// the rot engine. An adapter builds these locally while parsing (they are
/// never stored on [`StructuralContext`] themselves) for every invocation
/// whose command and result it can verify, not only failing ones, so a
/// session's last build/test run is visible even when it was green.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolInvocation {
    pub command: String,
    pub is_error: bool,
    /// The result's error text. Empty when `is_error` is `false`.
    pub error_text: String,
}

/// Whether a [`VerificationOutcome`]'s status could actually be attributed
/// to its command, or merely could not be (review finding F1): a shell's
/// reported exit status describes the LAST simple command it ran, so a
/// verification marker that is not in that position (`cargo test; echo
/// done`, `cargo test | tee out.log`) or that only ran conditionally
/// (`true || cargo test`, which never even ran the test) tells us nothing
/// about whether the verification itself passed or failed. `Unknown` is
/// deliberately its own state rather than a bare `Option<bool>` collapsing
/// into `errored: false`, because "the wrapper reported success" and "we
/// have no idea what the wrapper's status even measured" must never render
/// the same way to a successor session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationStatus {
    Passed,
    Failed,
    Unknown,
}

/// What a fresh session most needs to know about the LAST build/test/lint
/// run before it does anything else: whether it passed, and if not, why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationOutcome {
    pub command: String,
    pub status: VerificationStatus,
    /// The first 1-2 non-blank lines of the error text. Empty unless
    /// `status` is [`VerificationStatus::Failed`].
    pub error_excerpt: Vec<String>,
}

/// Command-text substrings (matched case-insensitively) that mark a tool
/// invocation as a build/test/lint run rather than an ordinary command.
/// Deliberately small and literal, like [`ERROR_TEXT_CHAR_CAP`]'s own
/// normalizer: a fuzzy "looks like verification" fingerprint, not a shell
/// parser.
const VERIFICATION_MARKERS: &[&str] = &[
    "cargo test",
    "cargo nextest",
    "cargo build",
    "cargo clippy",
    "cargo fmt",
    "npm test",
    "npm run test",
    "pytest",
    "go test",
    "make",
];

/// Splits a (lowercased) shell command into simple-command segments on the
/// separators a marker must never straddle: `&&`, `||`, `;`, `|`, newline,
/// `(`, `)`. A bare `&` (background job) is deliberately NOT a separator --
/// only the doubled `&&` is -- so a segment like `cd crate && make test`
/// splits into `cd crate` and ` make test`, each checked independently.
fn split_into_segments(command: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = command.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        if c == '&' && chars.get(i + 1) == Some(&'&') {
            segments.push(std::mem::take(&mut current));
            i += 2;
            continue;
        }
        if matches!(c, ';' | '|' | '\n' | '(' | ')') {
            segments.push(std::mem::take(&mut current));
            i += 1;
            continue;
        }
        current.push(c);
        i += 1;
    }
    segments.push(current);
    segments
}

/// Wrapper words that can prefix a real verification command without
/// changing what it is: `sudo make check`, `time pytest -q`. Deliberately
/// small and literal, like [`VERIFICATION_MARKERS`] itself.
const COMMAND_WRAPPER_WORDS: &[&str] = &["sudo", "time", "nice", "nohup", "exec", "env"];

/// Whether `word` is a shell `NAME=value` assignment prefix (`RUST_LOG=debug
/// cargo test`), recognized the same way a POSIX shell would: a leading
/// name made of ASCII letters/digits/underscore, not starting with a digit,
/// followed by `=`.
fn is_assignment_word(word: &str) -> bool {
    match word.split_once('=') {
        Some((name, _)) if !name.is_empty() => {
            let mut chars = name.chars();
            let first_ok = chars
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
            first_ok && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        _ => false,
    }
}

/// Strips leading wrapper words (`sudo`, `time`, ...) and `NAME=value`
/// assignments from the front of a segment's words, so the marker check
/// below sees the real command that follows them.
fn strip_leading_wrappers<'a>(words: &'a [&'a str]) -> &'a [&'a str] {
    let mut idx = 0usize;
    while idx < words.len()
        && (COMMAND_WRAPPER_WORDS.contains(&words[idx]) || is_assignment_word(words[idx]))
    {
        idx += 1;
    }
    &words[idx..]
}

/// Whether `command`'s text looks like a build/test/lint run. A marker must
/// match at the START of a shell segment (after stripping leading wrapper
/// words/assignments), not merely appear anywhere in the command -- so
/// `echo cargo test`, `rg "cargo test"`, and `git grep cargo test` do not
/// count as a `cargo test` run, while `sudo make check`, `RUST_LOG=debug
/// cargo test`, `(cargo test) | tee out.log`, and `cd crate && make test`
/// still do (review finding). Markers still match at word boundaries within
/// that leading position, so `cmake --build .`, `chmod +x make_release.sh`,
/// and `cat Makefile` do not count as a `make` run (earlier review finding).
pub fn looks_like_verification(command: &str) -> bool {
    let lower = command.to_lowercase();
    split_into_segments(&lower)
        .iter()
        .any(|segment| segment_matches_verification_marker(segment))
}

/// Whether a single (already-lowercased) shell segment starts with a known
/// verification marker, after stripping its leading wrapper words/
/// assignments. Factored out of [`looks_like_verification`] so the
/// attribution check below ([`verification_segment_is_attributable`]) can
/// apply the exact same "is this segment a verification command" test to
/// just the LAST segment, rather than "any segment", which is all
/// `looks_like_verification` itself needs.
fn segment_matches_verification_marker(segment: &str) -> bool {
    let words: Vec<&str> = segment.split_whitespace().collect();
    let words = strip_leading_wrappers(&words);
    VERIFICATION_MARKERS.iter().any(|marker| {
        let marker_words: Vec<&str> = marker.split_whitespace().collect();
        if words.len() < marker_words.len() {
            return false;
        }
        words[..marker_words.len()]
            .iter()
            .zip(&marker_words)
            .all(|(word, marker)| {
                // `npm run test:unit` still counts as `npm run test`.
                *word == *marker
                    || word
                        .strip_prefix(marker)
                        .is_some_and(|rest| rest.starts_with(':'))
            })
    })
}

/// Whether `command`'s reported exit status can actually be attributed to
/// its LAST verification-marker segment (review finding F1). A shell's exit
/// status is always the LAST simple command's, so:
/// - a `&&` chain ending in the verification segment is fine (`cd x &&
///   cargo test`) -- attributable;
/// - any `||` anywhere in the command makes it unattributable, since `a ||
///   cargo test` only runs the test when `a` fails, and `cargo test ||
///   true` reports `true`'s status regardless of the test;
/// - a `;`, newline, or trailing `|` after the verification segment
///   (`cargo test; echo done`, `cargo test | tee out.log`) is
///   unattributable too, since the reported status is whatever ran last.
///
/// Implemented as "the LAST segment (after lowercasing) matches a
/// verification marker, and the command contains no `||`" -- deliberately
/// simple, like every other check in this module: a fingerprint, not a
/// shell parser.
fn verification_segment_is_attributable(command: &str) -> bool {
    let lower = command.to_lowercase();
    if lower.contains("||") {
        return false;
    }
    match split_into_segments(&lower).last() {
        Some(last) => segment_matches_verification_marker(last),
        None => false,
    }
}

/// The most recent (last) invocation in `invocations` whose command text
/// looks like a verification run, or `None` when none does. Pure: a
/// straight reverse scan, no fs/clock/env/net.
pub fn last_verification_run(invocations: &[ToolInvocation]) -> Option<VerificationOutcome> {
    let last = invocations
        .iter()
        .rev()
        .find(|inv| looks_like_verification(&inv.command))?;

    // F1: the invocation's own recorded `is_error` is only trustworthy when
    // the verification command's exit status is what the whole compound
    // command actually reported -- see `verification_segment_is_
    // attributable`'s own doc comment for the cases this excludes.
    let status = if !verification_segment_is_attributable(&last.command) {
        VerificationStatus::Unknown
    } else if last.is_error {
        VerificationStatus::Failed
    } else {
        VerificationStatus::Passed
    };

    let error_excerpt = if status == VerificationStatus::Failed {
        last.error_text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .take(2)
            .map(str::to_string)
            .collect()
    } else {
        Vec::new()
    };
    Some(VerificationOutcome {
        command: last.command.clone(),
        status,
        error_excerpt,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_hash_is_fnv1a_64_and_stable() {
        assert_eq!(input_hash(""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(input_hash("a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(input_hash("foobar"), 0x8594_4171_f739_67e8);
        assert_eq!(
            input_hash("{\"command\":\"ls\"}"),
            input_hash("{\"command\":\"ls\"}")
        );
        assert_ne!(
            input_hash("{\"command\":\"ls\"}"),
            input_hash("{\"command\":\"ls -l\"}")
        );
    }

    #[test]
    fn session_ids_are_unique_uuids() {
        let a = SessionId::new_v4();
        let b = SessionId::new_v4();
        assert_ne!(a.as_str(), b.as_str());
        assert_eq!(a.as_str().len(), 36, "canonical hyphenated uuid");
        assert_eq!(a.to_string(), a.as_str());
    }

    #[test]
    fn capabilities_default_to_nothing_available() {
        let caps = Capabilities::default();
        assert!(!caps.marker_signal);
        assert!(!caps.token_usage);
        assert!(!caps.turn_signal);
    }

    /// Issue #155, Phase 6(a): capacity is a CAPABILITY, delivered inside the
    /// struct `rot.rs` already receives. That is what lets rotation
    /// thresholds become ratios of a real window without adding any fs,
    /// clock or env access to a module that must stay pure.
    #[test]
    fn capabilities_default_to_an_unknown_context_window() {
        assert_eq!(Capabilities::default().context_window_tokens, None);
    }

    #[test]
    fn normalize_error_text_collapses_digits_hex_and_whitespace_the_same_way() {
        let a = normalize_error_text("error at line 42, addr 0x1a2b3c\tin  file");
        let b = normalize_error_text("error at line 999, addr 0x9f9f9f\tin  file");
        assert_eq!(a, b, "differing digits/hex must normalize identically");
        assert!(a.contains("0x#"), "got {a}");
        assert!(a.contains("line #"), "got {a}");
    }

    #[test]
    fn normalize_error_text_folds_a_randomized_temp_path_and_line_number() {
        let a = normalize_error_text("failed to resolve at /tmp/build123/src/foo.rs:42:10");
        let b = normalize_error_text("failed to resolve at /tmp/build987/src/foo.rs:57:3");
        assert_eq!(
            a, b,
            "a randomized temp dir and differing line/col must not defeat this"
        );
    }

    #[test]
    fn normalize_error_text_caps_length() {
        let long = "x".repeat(2_000);
        assert!(normalize_error_text(&long).chars().count() <= ERROR_TEXT_CHAR_CAP);
    }

    #[test]
    fn error_text_hash_matches_for_normalized_equal_text_and_differs_otherwise() {
        assert_eq!(
            error_text_hash("failed at /tmp/build123/foo.rs:42"),
            error_text_hash("failed at /tmp/build987/foo.rs:57"),
        );
        assert_ne!(error_text_hash("error A"), error_text_hash("error B"));
    }

    #[test]
    fn looks_like_verification_matches_known_command_markers() {
        for cmd in [
            "cargo test",
            "cargo nextest run rot::",
            "cargo build --release",
            "cargo clippy --all-targets -- -D warnings",
            "cargo fmt -- --check",
            "npm test",
            "npm run test:unit",
            "pytest -q",
            "go test ./...",
            "make check",
        ] {
            assert!(looks_like_verification(cmd), "should match: {cmd}");
        }
        assert!(!looks_like_verification("ls -la"));
        assert!(!looks_like_verification("git status"));
    }

    /// Review finding: markers match whole words, so commands that merely
    /// contain `make` as a substring are not verification runs.
    #[test]
    fn looks_like_verification_does_not_match_marker_substrings() {
        for cmd in [
            "cmake --build .",
            "chmod +x make_release.sh",
            "cat Makefile",
            "echo pytest-style",
        ] {
            assert!(!looks_like_verification(cmd), "should not match: {cmd}");
        }
        assert!(looks_like_verification("cd crate && make test"));
        assert!(looks_like_verification("(cargo test) | tee out.log"));
    }

    /// Review finding (F2): a marker must match at the START of a shell
    /// segment, not merely appear anywhere in the command -- so a command
    /// that only mentions a marker as an argument to something else (`echo`,
    /// `rg`, `git grep`) is not a verification run.
    #[test]
    fn looks_like_verification_does_not_match_markers_used_as_mere_arguments() {
        for cmd in [
            "echo cargo test",
            "rg \"cargo test\"",
            "git grep cargo test",
        ] {
            assert!(!looks_like_verification(cmd), "should not match: {cmd}");
        }
    }

    /// Review finding (F2): wrapper words and leading shell-variable
    /// assignments must not defeat the marker check.
    #[test]
    fn looks_like_verification_matches_through_wrapper_words_and_assignments() {
        for cmd in [
            "sudo make check",
            "RUST_LOG=debug cargo test",
            "time pytest -q",
        ] {
            assert!(looks_like_verification(cmd), "should match: {cmd}");
        }
    }

    fn invocation(command: &str, is_error: bool, error_text: &str) -> ToolInvocation {
        ToolInvocation {
            command: command.to_string(),
            is_error,
            error_text: error_text.to_string(),
        }
    }

    #[test]
    fn last_verification_run_picks_the_most_recent_matching_invocation() {
        let invocations = vec![
            invocation("cargo test", true, "assertion failed\nleft: 1\n"),
            invocation("git status", false, ""),
            invocation("cargo nextest run", false, ""),
        ];
        let outcome = last_verification_run(&invocations).expect("a verification run exists");
        assert_eq!(outcome.command, "cargo nextest run");
        assert_eq!(outcome.status, VerificationStatus::Passed);
        assert!(outcome.error_excerpt.is_empty());
    }

    #[test]
    fn last_verification_run_carries_a_short_error_excerpt_when_red() {
        let invocations = vec![invocation(
            "cargo test",
            true,
            "\nassertion failed: `(left == right)`\n  left: 40\n  right: 70\nmore noise\n",
        )];
        let outcome = last_verification_run(&invocations).expect("exists");
        assert_eq!(outcome.status, VerificationStatus::Failed);
        assert_eq!(outcome.error_excerpt.len(), 2);
        assert_eq!(
            outcome.error_excerpt[0],
            "assertion failed: `(left == right)`"
        );
        assert_eq!(outcome.error_excerpt[1], "left: 40");
    }

    #[test]
    fn last_verification_run_is_none_when_nothing_matches() {
        let invocations = vec![invocation("git status", false, "")];
        assert!(last_verification_run(&invocations).is_none());
    }

    /// Review finding F1: the harness's own recorded `is_error` for the
    /// WHOLE command is only trustworthy when the compound command's exit
    /// status can be attributed to the verification segment specifically.
    /// These four shapes must all report `Unknown`, regardless of the
    /// recorded `is_error`.
    #[test]
    fn last_verification_run_reports_unknown_for_unattributable_compound_commands() {
        for (command, is_error) in [
            ("cargo test || true", false),
            ("cargo test; echo done", false),
            ("true || cargo test", false),
            ("cargo test | tee out.log", false),
            // Even a recorded failure must not be attributed to the test
            // when the status cannot be trusted.
            ("cargo test || true", true),
        ] {
            let invocations = vec![invocation(command, is_error, "some error text")];
            let outcome = last_verification_run(&invocations).unwrap_or_else(|| {
                panic!("{command} should still be recognized as verification-shaped")
            });
            assert_eq!(
                outcome.status,
                VerificationStatus::Unknown,
                "command: {command}"
            );
            assert!(
                outcome.error_excerpt.is_empty(),
                "an unknown outcome carries no error excerpt: {command}"
            );
        }
    }

    /// Review finding F1: a `&&` chain ending in the verification segment is
    /// fine -- the shell's own exit status IS the last command's.
    #[test]
    fn last_verification_run_attributes_a_trailing_and_chain() {
        let invocations = vec![invocation("cd x && cargo test", true, "boom")];
        let outcome = last_verification_run(&invocations).expect("exists");
        assert_eq!(outcome.status, VerificationStatus::Failed);
        assert_eq!(outcome.error_excerpt, vec!["boom".to_string()]);
    }

    /// Review finding F1: a leading assignment does not defeat attribution.
    #[test]
    fn last_verification_run_attributes_through_a_leading_assignment() {
        let invocations = vec![invocation("RUST_LOG=debug cargo test", false, "")];
        let outcome = last_verification_run(&invocations).expect("exists");
        assert_eq!(outcome.status, VerificationStatus::Passed);
    }

    #[test]
    fn events_compare_by_value() {
        assert_eq!(
            NormalizedEvent::ToolResult { is_error: true },
            NormalizedEvent::ToolResult { is_error: true }
        );
        assert_ne!(
            NormalizedEvent::ToolResult { is_error: true },
            NormalizedEvent::ToolResult { is_error: false }
        );
    }

    /// Issue #155, Phase 2: the four categories are recorded RAW. Before
    /// this, claude's adapter summed input + cache_creation + cache_read into
    /// `input_tokens` at the adapter boundary, which made a cache-hit ratio
    /// -- the one number that says whether prompt-shape work helped --
    /// uncomputable anywhere downstream.
    #[test]
    fn transcript_usage_keeps_the_cache_classes_apart_and_can_still_combine_them() {
        let usage = TranscriptUsage {
            input_tokens: 1_000,
            cache_creation_input_tokens: 8_000,
            cache_read_input_tokens: 91_000,
            output_tokens: 500,
        };
        assert_eq!(
            usage.context_total(),
            100_000,
            "the pre-2.34.0 combined number"
        );
        assert_eq!(TranscriptUsage::default().context_total(), 0);
    }
}
