//! `zirv context lint` (issue #275): pure analysis over the instructional
//! layers `compile.rs` assembles at session launch -- budget headroom
//! (CTX001), sentence-level cross-layer near-duplicates (CTX002),
//! contradiction candidates (CTX003), per-layer proportionality (CTX004),
//! and a dedupe-leak check (CTX005).
//!
//! **Pure, like `rot.rs`/`compile.rs`.** This module reads no file, no
//! clock, no environment variable, and never iterates a `HashMap` into
//! output order. Every fact it needs -- a layer's already-read text, its
//! configured budget (if any), whether it participates in cross-layer
//! comparison, and whether `context.dedupe_native` is silently inactive --
//! is handed in by its caller ([`super::context_cli::run_lint`], which does
//! the actual reading). Two calls with identical [`LintLayer`] input always
//! produce an identical [`LintReport`] (`analysis_is_deterministic`).
//!
//! **Never writes anything.** Neither this module nor `--fix-plan`
//! ([`fix_plan`]) ever touches the filesystem: a fix plan is a list of
//! strings describing what a human could remove or move, never an edit
//! zirv makes itself.
//!
//! **Bounded cost.** CTX002/CTX003 are pairwise over every imperative
//! sentence in every cross-comparable layer, so cost is quadratic in the
//! amount of instructional prose. [`analyze`]'s `max_pairs` argument (from
//! `context.lint_max_pairs`) stops comparing once spent and sets
//! [`LintReport::degraded`] instead of hanging on a very large repository.

use std::collections::BTreeSet;

/// The `--json` output schema version. Bump only on a breaking change to
/// [`LintReport`]'s own shape.
pub const SCHEMA_VERSION: u32 = 1;

pub const CTX001_BUDGET_HEADROOM: &str = "CTX001";
pub const CTX002_DUPLICATE_RULE: &str = "CTX002";
pub const CTX003_CONTRADICTION_CANDIDATE: &str = "CTX003";
pub const CTX004_PROPORTIONALITY: &str = "CTX004";
pub const CTX005_DEDUPE_LEAK: &str = "CTX005";

/// Warn threshold for CTX001: a layer at or above this fraction of its
/// configured budget is flagged before it actually overflows.
const HEADROOM_WARN_RATIO: f64 = 0.9;
/// Near-duplicate threshold for CTX002 (issue's own required value).
const DUPLICATE_JACCARD_THRESHOLD: f64 = 0.6;
/// Minimum shared content-token count for CTX003 (issue's own required
/// value).
const CONTRADICTION_MIN_SHARED_TOKENS: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warn,
    Error,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warn => "warn",
            Severity::Error => "error",
        }
    }
}

/// One instructional layer handed to [`analyze`] by its (impure) caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintLayer {
    /// A stable, human-readable name for this layer (e.g. `"common.md"`,
    /// `"default prompt"`), used verbatim in every finding it participates
    /// in and as the primary sort key for determinism.
    pub name: String,
    pub text: String,
    /// `(ctx.toml key, cap bytes)` when the caller already resolved a real,
    /// enforced budget for this layer from `CtxConfig`. `None` for a layer
    /// with no configured cap (a native compat file, a role file with no
    /// budget of its own).
    pub budget: Option<(String, usize)>,
    /// Whether this layer's imperative sentences are compared against every
    /// OTHER `cross_compare` layer's for CTX002/CTX003. `false` for a
    /// zirv-managed native file (a verbatim render of canonical content --
    /// comparing it against the canonical layer it was rendered from is a
    /// tautology, the same exclusion `context_cli::surfaces_for_drift`
    /// already applies) and for a built-in prompt (zirv's own product copy
    /// is not the kind of repo-authored instruction drift this check looks
    /// for). Every layer still gets its own CTX004 proportionality row
    /// regardless of this flag.
    pub cross_compare: bool,
}

impl LintLayer {
    pub fn new(name: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            text: text.into(),
            budget: None,
            cross_compare: false,
        }
    }

    pub fn with_budget(mut self, key: impl Into<String>, cap_bytes: usize) -> Self {
        self.budget = Some((key.into(), cap_bytes));
        self
    }

    pub fn cross_compared(mut self) -> Self {
        self.cross_compare = true;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Finding {
    pub id: &'static str,
    pub severity: Severity,
    pub layer: String,
    pub byte_offset: usize,
    pub message: String,
    /// Byte cost this finding names, when it has one: for CTX001, the
    /// layer's own size; for CTX002, the second copy's own byte length (the
    /// bytes a fix could recover); `0` for a finding with no single byte
    /// figure of its own (CTX003, CTX004, CTX005).
    #[serde(default)]
    pub bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct LintReport {
    pub schema: u32,
    pub findings: Vec<Finding>,
    /// `true` when `max_pairs` was exhausted before every CTX002/CTX003
    /// sentence pair had been compared -- every other check in this report
    /// is still complete; only duplicate/contradiction detection is partial.
    pub degraded: bool,
}

impl LintReport {
    /// Exit-code contract (issue #275): `1` when any CTX001 or CTX005
    /// finding is `Severity::Error`, `0` otherwise -- every other id/
    /// severity is advisory.
    pub fn has_blocking_error(&self) -> bool {
        self.findings.iter().any(|f| {
            (f.id == CTX001_BUDGET_HEADROOM || f.id == CTX005_DEDUPE_LEAK)
                && f.severity == Severity::Error
        })
    }
}

/// A dedupe-leak fact the (impure) caller already established by reading
/// the native file and comparing it against a fresh render -- CTX005 is
/// built from this, never derived by this module itself. `None` in means
/// dedupe is either off, not applicable, or genuinely working; no CTX005
/// finding either way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DedupeLeak {
    /// The native file's bare name (`"CLAUDE.md"`/`"AGENTS.md"`), used as
    /// this finding's `layer`.
    pub native_name: String,
}

// -- sentence splitting / normalisation -------------------------------------

/// Splits `text` into raw (not yet normalised) sentences, each paired with
/// its byte offset in `text`. A boundary is `.`, `!`, `?`, or a line break:
/// canonical context files are largely bulleted Markdown (one instruction
/// per line, not always terminated with a period), so a line break alone
/// must also end a sentence or an entire bullet list would be read as one
/// giant "sentence". Empty/whitespace-only spans are dropped.
fn split_sentences(text: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let ch = bytes[i];
        if ch == b'.' || ch == b'!' || ch == b'?' || ch == b'\n' {
            let end = i + 1;
            let raw = &text[start..end];
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                let offset = start + (raw.len() - raw.trim_start().len());
                out.push((offset, trimmed));
            }
            start = end;
        }
        i += 1;
    }
    if start < bytes.len() {
        let raw = &text[start..];
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            let offset = start + (raw.len() - raw.trim_start().len());
            out.push((offset, trimmed));
        }
    }
    out
}

/// Lowercases and strips punctuation/backticks, collapsing whitespace runs
/// to single spaces -- the normal form every comparison in this module
/// works on. Word-internal characters (letters, digits, `-`, `_`) survive;
/// everything else becomes a space, so `"Zirv's \`common.md\`!"` normalises
/// to `"zirv s common md"`.
fn normalize(sentence: &str) -> String {
    let mut out = String::with_capacity(sentence.len());
    let mut last_was_space = true;
    for ch in sentence.chars() {
        let lower = ch.to_ascii_lowercase();
        let keep = lower.is_alphanumeric() || lower == '-' || lower == '_';
        if keep {
            out.push(lower);
            last_was_space = false;
        } else if !last_was_space {
            out.push(' ');
            last_was_space = true;
        }
    }
    out.trim().to_string()
}

fn tokens(normalized: &str) -> Vec<String> {
    normalized.split_whitespace().map(str::to_string).collect()
}

const IMPERATIVE_MARKERS: &[&str] = &[
    "must", "never", "always", "only", "do not", "before", "after", "every",
];

/// Whether `normalized` (already lowercased/stripped) contains one of
/// [`IMPERATIVE_MARKERS`] as a whole word (or, for a two-word marker like
/// `"do not"`, as a whole phrase) -- padded with boundary spaces so `"before"`
/// never matches inside `"beforehand"`.
fn is_imperative(normalized: &str) -> bool {
    let padded = format!(" {normalized} ");
    IMPERATIVE_MARKERS
        .iter()
        .any(|marker| padded.contains(format!(" {marker} ").as_str()))
}

const NEGATION_MARKERS: &[&str] = &["never", "not", "no", "without", "don't", "nothing", "none"];

fn is_negated(normalized: &str) -> bool {
    let padded = format!(" {normalized} ");
    NEGATION_MARKERS
        .iter()
        .any(|marker| padded.contains(format!(" {marker} ").as_str()))
}

/// Function words excluded from CTX003's "content tokens" (the issue's own
/// requirement: "sharing >= 3 CONTENT tokens", not just any 3 tokens) --
/// short enough that its own presence/absence never changes CTX002's plain
/// token-Jaccard, which is deliberately NOT filtered (the issue specifies
/// token-Jaccard over the normalised sentence, not a content-token subset).
const STOPWORDS: &[&str] = &[
    "a", "an", "the", "is", "are", "be", "to", "of", "in", "on", "for", "and", "or", "with",
    "this", "that", "it", "as", "by", "at", "from", "not", "no", "its", "your", "you", "if",
    "into", "than", "then", "so", "but", "own", "their", "they", "these", "those", "will", "can",
    "may", "should", "would", "s",
];

fn content_tokens(tokens: &[String]) -> BTreeSet<&str> {
    tokens
        .iter()
        .map(String::as_str)
        .filter(|t| t.len() > 1 && !STOPWORDS.contains(t))
        .collect()
}

fn jaccard(a: &BTreeSet<&str>, b: &BTreeSet<&str>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let intersection = a.intersection(b).count();
    let union = a.union(b).count();
    if union == 0 {
        0.0
    } else {
        intersection as f64 / union as f64
    }
}

/// One imperative sentence extracted from a layer, ready for cross-layer
/// comparison.
struct ImperativeSentence<'a> {
    byte_offset: usize,
    original: &'a str,
    token_set: BTreeSet<&'a str>,
    content_set: BTreeSet<&'a str>,
    negated: bool,
}

/// One split-and-normalised sentence: its byte offset, the original
/// (unnormalised) text, the normalised text, and its tokens. Named so
/// neither this signature nor `analyze`'s own cache declaration repeats the
/// same four-tuple type twice.
type NormalizedSentence<'a> = (usize, &'a str, String, Vec<String>);

fn imperative_sentences<'a>(
    normalized_cache: &'a [NormalizedSentence<'a>],
) -> Vec<ImperativeSentence<'a>> {
    normalized_cache
        .iter()
        .filter(|(_, _, normalized, _)| is_imperative(normalized))
        .map(|(offset, original, normalized, toks)| ImperativeSentence {
            byte_offset: *offset,
            original,
            token_set: toks.iter().map(String::as_str).collect(),
            content_set: content_tokens(toks),
            negated: is_negated(normalized),
        })
        .collect()
}

// -- CTX004: proportionality -------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Proportionality {
    pub imperative_sentences: usize,
    pub narrative_sentences: usize,
    pub code_fences: usize,
    pub bytes: usize,
}

fn proportionality(text: &str) -> Proportionality {
    let mut imperative_sentences = 0usize;
    let mut narrative_sentences = 0usize;
    for (_, sentence) in split_sentences(text) {
        let normalized = normalize(sentence);
        if normalized.is_empty() {
            continue;
        }
        if is_imperative(&normalized) {
            imperative_sentences += 1;
        } else {
            narrative_sentences += 1;
        }
    }
    // Each fenced block opens and closes with its own "```" line; an odd
    // count (an unterminated fence) still counts every full pair it can.
    let fence_markers = text.matches("```").count();
    Proportionality {
        imperative_sentences,
        narrative_sentences,
        code_fences: fence_markers / 2,
        bytes: text.len(),
    }
}

impl Proportionality {
    /// A one-line ratio, e.g. `"imperative:narrative = 12:3 (80% imperative)"`
    /// -- the issue's own required shape for CTX004's message.
    fn ratio_line(&self) -> String {
        let total = self.imperative_sentences + self.narrative_sentences;
        if total == 0 {
            return "imperative:narrative = 0:0 (no prose sentences)".to_string();
        }
        let pct = (self.imperative_sentences as f64 / total as f64 * 100.0).round() as u64;
        format!(
            "imperative:narrative = {}:{} ({pct}% imperative), {} code fence(s), {} bytes",
            self.imperative_sentences, self.narrative_sentences, self.code_fences, self.bytes
        )
    }
}

// -- byte-range spans of fenced code blocks (for --fix-plan) ----------------

/// `(start, len)` for each fenced code block (paired "```" markers) in
/// `text`, in order. An unterminated trailing fence (odd marker count) is
/// not reported: there is no closing byte to name a length against.
fn code_fence_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut positions: Vec<usize> = text.match_indices("```").map(|(i, _)| i).collect();
    while positions.len() >= 2 {
        let start = positions.remove(0);
        let close = positions.remove(0);
        let end = close + 3;
        spans.push((start, end - start));
    }
    spans
}

// -- CTX001: budget headroom --------------------------------------------------

fn budget_headroom_findings(layers: &[LintLayer]) -> Vec<Finding> {
    let mut findings = Vec::new();
    for layer in layers {
        let Some((key, cap)) = &layer.budget else {
            continue;
        };
        let bytes = layer.text.len();
        if *cap == 0 {
            if bytes > 0 {
                findings.push(Finding {
                    id: CTX001_BUDGET_HEADROOM,
                    severity: Severity::Error,
                    layer: layer.name.clone(),
                    byte_offset: 0,
                    message: format!(
                        "{}: {bytes} bytes, but {key} is 0 -- every byte overflows",
                        layer.name
                    ),
                    bytes,
                });
            }
            continue;
        }
        let ratio = bytes as f64 / *cap as f64;
        if bytes > *cap {
            findings.push(Finding {
                id: CTX001_BUDGET_HEADROOM,
                severity: Severity::Error,
                layer: layer.name.clone(),
                byte_offset: 0,
                message: format!(
                    "{}: {bytes}/{cap} bytes -- over budget ({key}), {} bytes will be truncated",
                    layer.name,
                    bytes - cap
                ),
                bytes,
            });
        } else if ratio >= HEADROOM_WARN_RATIO {
            findings.push(Finding {
                id: CTX001_BUDGET_HEADROOM,
                severity: Severity::Warn,
                layer: layer.name.clone(),
                byte_offset: 0,
                message: format!(
                    "{}: {bytes}/{cap} bytes ({:.0}% of budget {key})",
                    layer.name,
                    ratio * 100.0
                ),
                bytes,
            });
        }
    }
    findings
}

// -- entry point --------------------------------------------------------------

/// Runs every CTX00x check over `layers` and returns a deterministic,
/// sorted [`LintReport`]. `max_pairs` bounds CTX002/CTX003's combined
/// sentence-pair comparisons (`context.lint_max_pairs`); `dedupe_leak`, when
/// `Some`, becomes exactly one CTX005 finding.
///
/// Deterministic given deterministic input: `layers` is walked in the order
/// given (the caller sorts by name before calling, so a stable input order
/// is this function's responsibility to preserve, not to invent), sentence
/// extraction and comparison never touch a `HashMap`, and the final sort is
/// total (`(layer, byte_offset, id)`).
pub fn analyze(
    layers: &[LintLayer],
    max_pairs: usize,
    dedupe_leak: Option<DedupeLeak>,
) -> LintReport {
    let mut findings = budget_headroom_findings(layers);

    if let Some(leak) = dedupe_leak {
        findings.push(Finding {
            id: CTX005_DEDUPE_LEAK,
            severity: Severity::Error,
            layer: leak.native_name.clone(),
            byte_offset: 0,
            message: format!(
                "context.dedupe_native is on and {} claims (via its embedded canonical hash) to \
                 match the current .zirv/context/ content, but its actual bytes differ from a \
                 fresh render -- likely EOL/whitespace drift from a checkout normalising line \
                 endings. Compile-time dedupe will NOT fire, so canonical content is being \
                 injected twice. Fix: run `zirv context sync --generate --force`.",
                leak.native_name
            ),
            bytes: 0,
        });
    }

    for layer in layers {
        let metrics = proportionality(&layer.text);
        findings.push(Finding {
            id: CTX004_PROPORTIONALITY,
            severity: Severity::Info,
            layer: layer.name.clone(),
            byte_offset: 0,
            message: format!("{}: {}", layer.name, metrics.ratio_line()),
            bytes: metrics.bytes,
        });
    }

    // Cache each layer's split+normalised sentences once, reused for both
    // the imperative-sentence extraction below and (via the same owned
    // `String`/`Vec<String>` data) never re-split per comparison.
    let normalized: Vec<Vec<NormalizedSentence<'_>>> = layers
        .iter()
        .map(|layer| {
            split_sentences(&layer.text)
                .into_iter()
                .map(|(offset, sentence)| {
                    let normalized = normalize(sentence);
                    let toks = tokens(&normalized);
                    (offset, sentence, normalized, toks)
                })
                .collect()
        })
        .collect();

    let imperative_per_layer: Vec<Vec<ImperativeSentence<'_>>> = normalized
        .iter()
        .map(|cache| imperative_sentences(cache))
        .collect();

    let mut pairs_checked = 0usize;
    let mut degraded = false;
    'outer: for i in 0..layers.len() {
        if !layers[i].cross_compare {
            continue;
        }
        for j in (i + 1)..layers.len() {
            if !layers[j].cross_compare {
                continue;
            }
            for a in &imperative_per_layer[i] {
                for b in &imperative_per_layer[j] {
                    if pairs_checked >= max_pairs {
                        degraded = true;
                        break 'outer;
                    }
                    pairs_checked += 1;

                    let sim = jaccard(&a.token_set, &b.token_set);
                    if sim >= DUPLICATE_JACCARD_THRESHOLD {
                        findings.push(Finding {
                            id: CTX002_DUPLICATE_RULE,
                            severity: Severity::Warn,
                            layer: layers[j].name.clone(),
                            byte_offset: b.byte_offset,
                            message: format!(
                                "\"{}\" ({} byte {}) near-duplicates \"{}\" ({} byte {}) -- \
                                 {:.0}% token overlap",
                                b.original,
                                layers[j].name,
                                b.byte_offset,
                                a.original,
                                layers[i].name,
                                a.byte_offset,
                                sim * 100.0
                            ),
                            bytes: b.original.len(),
                        });
                    }

                    let overlap = a.content_set.intersection(&b.content_set).count();
                    if overlap >= CONTRADICTION_MIN_SHARED_TOKENS && a.negated != b.negated {
                        findings.push(Finding {
                            id: CTX003_CONTRADICTION_CANDIDATE,
                            severity: Severity::Warn,
                            layer: layers[j].name.clone(),
                            byte_offset: b.byte_offset,
                            message: format!(
                                "candidate: \"{}\" ({} byte {}) may contradict \"{}\" ({} byte \
                                 {}) -- {overlap} shared content token(s), one negated and the \
                                 other not; heuristic, verify by hand",
                                b.original,
                                layers[j].name,
                                b.byte_offset,
                                a.original,
                                layers[i].name,
                                a.byte_offset
                            ),
                            bytes: 0,
                        });
                    }
                }
            }
        }
    }

    findings.sort_by(|a, b| {
        a.layer
            .cmp(&b.layer)
            .then(a.byte_offset.cmp(&b.byte_offset))
            .then(a.id.cmp(b.id))
            .then(a.message.cmp(&b.message))
    });

    LintReport {
        schema: SCHEMA_VERSION,
        findings,
        degraded,
    }
}

/// `--fix-plan` (issue #275): for every layer at or above the CTX001 warn
/// threshold, the top duplicate sentences and code fences by byte cost that
/// could be removed or moved. Pure, read-only: never writes or edits
/// anything, the same contract [`analyze`] itself holds.
pub fn fix_plan(layers: &[LintLayer], report: &LintReport) -> Vec<String> {
    let mut lines = Vec::new();
    for layer in layers {
        let Some((key, cap)) = &layer.budget else {
            continue;
        };
        let bytes = layer.text.len();
        if *cap == 0 || (bytes as f64 / *cap as f64) < HEADROOM_WARN_RATIO {
            continue;
        }
        lines.push(format!(
            "{}: {bytes}/{cap} bytes ({key}) -- candidates to remove or move:",
            layer.name
        ));

        let mut dups: Vec<&Finding> = report
            .findings
            .iter()
            .filter(|f| f.id == CTX002_DUPLICATE_RULE && f.layer == layer.name)
            .collect();
        dups.sort_by(|a, b| {
            b.bytes
                .cmp(&a.bytes)
                .then(a.byte_offset.cmp(&b.byte_offset))
        });
        for finding in dups.iter().take(5) {
            lines.push(format!(
                "  duplicate ({} bytes) at byte {}: {}",
                finding.bytes, finding.byte_offset, finding.message
            ));
        }
        if dups.is_empty() {
            lines.push("  (no duplicate sentences found in this layer)".to_string());
        }

        let mut fences = code_fence_spans(&layer.text);
        fences.sort_by_key(|&(_, len)| std::cmp::Reverse(len));
        for (start, len) in fences.into_iter().take(5) {
            lines.push(format!(
                "  code fence ({len} bytes) at byte {start} -- consider moving to a linked doc \
                 instead of inlining it here"
            ));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer(name: &str, text: &str) -> LintLayer {
        LintLayer::new(name, text).cross_compared()
    }

    // -- sentence splitting / normalisation ---------------------------------

    #[test]
    fn splits_on_terminators_and_line_breaks() {
        let text = "Never taskkill zirv.\n- Always branch before a PR\nRun tests!";
        let sentences: Vec<&str> = split_sentences(text).into_iter().map(|(_, s)| s).collect();
        assert_eq!(
            sentences,
            vec![
                "Never taskkill zirv.",
                "- Always branch before a PR",
                "Run tests!"
            ]
        );
    }

    #[test]
    fn normalize_lowercases_and_strips_punctuation_and_backticks() {
        assert_eq!(normalize("Zirv's `common.md`!"), "zirv s common md");
        assert_eq!(normalize("Do-not skip_it."), "do-not skip_it");
    }

    #[test]
    fn imperative_marker_matches_whole_words_only() {
        assert!(is_imperative(&normalize("Never taskkill a zirv process.")));
        assert!(is_imperative(&normalize("Do not skip hooks.")));
        assert!(!is_imperative(&normalize(
            "This beforehand note is just narrative prose without any marker word."
        )));
    }

    // -- CTX001 ---------------------------------------------------------------

    #[test]
    fn ctx001_warns_at_ninety_percent_and_errors_over_budget() {
        let warn = LintLayer::new("common.md", "x".repeat(3690))
            .with_budget("context.max_common_bytes", 4096);
        let report = analyze(std::slice::from_ref(&warn), 1000, None);
        let warn_finding = report
            .findings
            .iter()
            .find(|f| f.id == CTX001_BUDGET_HEADROOM)
            .expect("a CTX001 finding");
        assert_eq!(warn_finding.severity, Severity::Warn);

        let over = LintLayer::new("common.md", "x".repeat(4097))
            .with_budget("context.max_common_bytes", 4096);
        let report = analyze(std::slice::from_ref(&over), 1000, None);
        let error_finding = report
            .findings
            .iter()
            .find(|f| f.id == CTX001_BUDGET_HEADROOM)
            .expect("a CTX001 finding");
        assert_eq!(error_finding.severity, Severity::Error);
        assert!(report.has_blocking_error());
    }

    #[test]
    fn ctx001_stays_silent_comfortably_under_budget() {
        let ok = LintLayer::new("common.md", "short").with_budget("context.max_common_bytes", 4096);
        let report = analyze(std::slice::from_ref(&ok), 1000, None);
        assert!(
            !report
                .findings
                .iter()
                .any(|f| f.id == CTX001_BUDGET_HEADROOM)
        );
    }

    // -- fixtures (tests/fixtures/context-lint/) -----------------------------

    const DUPLICATE_A: &str = include_str!("../../../tests/fixtures/context-lint/duplicate-a.md");
    const DUPLICATE_B: &str = include_str!("../../../tests/fixtures/context-lint/duplicate-b.md");
    const CONTRADICTION_A: &str =
        include_str!("../../../tests/fixtures/context-lint/contradiction-a.md");
    const CONTRADICTION_B: &str =
        include_str!("../../../tests/fixtures/context-lint/contradiction-b.md");

    #[test]
    fn the_planted_duplicate_fixture_pair_fires_ctx002() {
        let a = layer("fixture-a", DUPLICATE_A);
        let b = layer("fixture-b", DUPLICATE_B);
        let report = analyze(&[a, b], 1000, None);
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.id == CTX002_DUPLICATE_RULE),
            "the planted duplicate fixture must fire CTX002: {:?}",
            report.findings
        );
    }

    #[test]
    fn the_planted_contradiction_fixture_pair_fires_ctx003() {
        let a = layer("fixture-a", CONTRADICTION_A);
        let b = layer("fixture-b", CONTRADICTION_B);
        let report = analyze(&[a, b], 1000, None);
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.id == CTX003_CONTRADICTION_CANDIDATE),
            "the planted contradiction fixture must fire CTX003: {:?}",
            report.findings
        );
    }

    // -- CTX002 -----------------------------------------------------------

    #[test]
    fn ctx002_fires_on_a_planted_cross_layer_duplicate() {
        let a = layer(
            "common.md",
            "Never commit directly to the main branch without a pull request.",
        );
        let b = layer(
            "claude.md",
            "Never commit directly to the main branch without a pull request.",
        );
        let report = analyze(&[a, b], 1000, None);
        let dup = report
            .findings
            .iter()
            .find(|f| f.id == CTX002_DUPLICATE_RULE)
            .expect("expected a CTX002 duplicate finding");
        assert_eq!(dup.layer, "claude.md");
        assert!(dup.bytes > 0);
    }

    #[test]
    fn ctx002_does_not_fire_within_a_single_layer() {
        let a = layer(
            "common.md",
            "Never commit directly to the main branch without a pull request.\n\
             Never commit directly to the main branch without a pull request.",
        );
        let report = analyze(std::slice::from_ref(&a), 1000, None);
        assert!(
            !report
                .findings
                .iter()
                .any(|f| f.id == CTX002_DUPLICATE_RULE),
            "duplicate detection is cross-layer only: {:?}",
            report.findings
        );
    }

    #[test]
    fn ctx002_ignores_a_layer_not_marked_cross_compare() {
        let a = LintLayer::new(
            "default prompt",
            "Never commit directly to the main branch without a pull request.",
        );
        let b = layer(
            "common.md",
            "Never commit directly to the main branch without a pull request.",
        );
        let report = analyze(&[a, b], 1000, None);
        assert!(
            !report
                .findings
                .iter()
                .any(|f| f.id == CTX002_DUPLICATE_RULE)
        );
    }

    // -- CTX003 -----------------------------------------------------------

    #[test]
    fn ctx003_fires_on_a_planted_contradiction() {
        let a = layer(
            "common.md",
            "Always taskkill orphaned zirv worker processes after a crash.",
        );
        let b = layer(
            "claude.md",
            "Never taskkill any zirv worker process for any reason.",
        );
        let report = analyze(&[a, b], 1000, None);
        let candidate = report
            .findings
            .iter()
            .find(|f| f.id == CTX003_CONTRADICTION_CANDIDATE)
            .expect("expected a CTX003 contradiction candidate");
        assert_eq!(candidate.severity, Severity::Warn);
        assert!(
            !report.has_blocking_error(),
            "CTX003 must never be blocking: {:?}",
            report.findings
        );
    }

    #[test]
    fn ctx003_requires_both_sentences_to_be_imperative_and_disagree_on_negation() {
        let a = layer(
            "common.md",
            "Always run the full test suite before committing.",
        );
        let b = layer(
            "claude.md",
            "Always run the full test suite before committing.",
        );
        let report = analyze(&[a, b], 1000, None);
        assert!(
            !report
                .findings
                .iter()
                .any(|f| f.id == CTX003_CONTRADICTION_CANDIDATE),
            "two sentences that agree (same negation state) must not be a contradiction \
             candidate: {:?}",
            report.findings
        );
    }

    // -- CTX004 -------------------------------------------------------------

    #[test]
    fn ctx004_reports_one_info_row_per_layer() {
        let a = layer(
            "common.md",
            "Never skip a hook. This is a narrative sentence.",
        );
        let report = analyze(std::slice::from_ref(&a), 1000, None);
        let rows: Vec<&Finding> = report
            .findings
            .iter()
            .filter(|f| f.id == CTX004_PROPORTIONALITY)
            .collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].severity, Severity::Info);
        assert!(rows[0].message.contains("imperative:narrative"));
    }

    #[test]
    fn ctx004_counts_fenced_code_blocks() {
        let text = "Some prose.\n```\ncode here\n```\nMore prose.";
        let metrics = proportionality(text);
        assert_eq!(metrics.code_fences, 1);
    }

    // -- CTX005 -----------------------------------------------------------

    #[test]
    fn ctx005_fires_exactly_once_when_a_dedupe_leak_is_supplied() {
        let report = analyze(
            &[],
            1000,
            Some(DedupeLeak {
                native_name: "CLAUDE.md".to_string(),
            }),
        );
        let leaks: Vec<&Finding> = report
            .findings
            .iter()
            .filter(|f| f.id == CTX005_DEDUPE_LEAK)
            .collect();
        assert_eq!(leaks.len(), 1);
        assert_eq!(leaks[0].severity, Severity::Error);
        assert!(leaks[0].message.contains("--generate --force"));
        assert!(report.has_blocking_error());
    }

    #[test]
    fn no_dedupe_leak_means_no_ctx005_finding() {
        let report = analyze(&[], 1000, None);
        assert!(!report.findings.iter().any(|f| f.id == CTX005_DEDUPE_LEAK));
    }

    // -- determinism / sort order --------------------------------------------

    #[test]
    fn analysis_is_deterministic() {
        let a = layer("common.md", "Never commit directly to main.");
        let b = layer("claude.md", "Never commit directly to main.");
        let first = analyze(&[a.clone(), b.clone()], 1000, None);
        let second = analyze(&[a, b], 1000, None);
        assert_eq!(first, second);
    }

    #[test]
    fn findings_are_sorted_by_layer_then_byte_offset() {
        let a = layer(
            "common.md",
            "Never commit to main.\nAlways branch before a pull request.",
        );
        let b = layer(
            "claude.md",
            "Never commit to main.\nAlways branch before a pull request.",
        );
        let report = analyze(&[a, b], 1000, None);
        let mut sorted = report.findings.clone();
        sorted.sort_by(|x, y| {
            x.layer
                .cmp(&y.layer)
                .then(x.byte_offset.cmp(&y.byte_offset))
                .then(x.id.cmp(y.id))
                .then(x.message.cmp(&y.message))
        });
        assert_eq!(
            report.findings, sorted,
            "report must already be in sorted order"
        );
    }

    // -- cap / degraded -------------------------------------------------------

    #[test]
    fn exceeding_max_pairs_sets_degraded_and_stops_comparing() {
        let a = layer(
            "common.md",
            "Never do the first thing.\nNever do the second thing.\nNever do the third thing.",
        );
        let b = layer(
            "claude.md",
            "Never do the first thing.\nNever do the second thing.\nNever do the third thing.",
        );
        let full = analyze(&[a.clone(), b.clone()], 1000, None);
        assert!(!full.degraded);
        let capped = analyze(&[a, b], 1, None);
        assert!(capped.degraded, "a tiny cap must be reported as degraded");
    }

    // -- no-write guarantee ---------------------------------------------------

    #[test]
    fn fix_plan_never_touches_the_filesystem() {
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("common.md");
        std::fs::write(&marker, "x".repeat(4000)).expect("write fixture");
        let before = std::fs::metadata(&marker).expect("meta").modified().ok();

        let text = std::fs::read_to_string(&marker).expect("read");
        let layer = LintLayer::new("common.md", text).with_budget("context.max_common_bytes", 4096);
        let report = analyze(std::slice::from_ref(&layer), 1000, None);
        let _ = fix_plan(std::slice::from_ref(&layer), &report);

        let after = std::fs::metadata(&marker).expect("meta").modified().ok();
        assert_eq!(before, after, "fix_plan must never modify the fixture file");
        assert_eq!(
            std::fs::read_to_string(&marker).expect("read"),
            "x".repeat(4000),
            "fix_plan must never rewrite the fixture's content"
        );
    }

    #[test]
    fn fix_plan_lists_top_duplicates_and_fences_only_for_over_threshold_layers() {
        let big_dup = "Never skip the pre-commit hook under any circumstance whatsoever.";
        let a = LintLayer::new(
            "common.md",
            format!("{}\n```\ncode\n```", "x".repeat(3700) + "\n" + big_dup),
        )
        .with_budget("context.max_common_bytes", 4096)
        .cross_compared();
        let b = layer("claude.md", big_dup);
        let small = LintLayer::new("codex.md", "short and sweet")
            .with_budget("context.max_harness_bytes", 4096)
            .cross_compared();

        let report = analyze(&[a.clone(), b.clone(), small.clone()], 1000, None);
        let plan = fix_plan(&[a, b, small], &report);
        let joined = plan.join("\n");
        assert!(joined.contains("common.md"), "got {joined}");
        assert!(
            !joined.contains("codex.md"),
            "an under-budget layer must not appear: {joined}"
        );
    }
}
