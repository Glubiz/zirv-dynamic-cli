//! Issue #413: per-family structural extractors for test-runner output.
//!
//! `output::diagnostic_severity`/`continues_diagnostic_block` (the generic
//! scan) only recognise rustc/cargo's own vocabulary (`error:`, `panicked
//! at`, `-->`), so a `pytest`/`vitest`/`jest`/`go test` failure -- shapes
//! like a pytest `FAILED path::test - message` short-summary line, a jest
//! `● suite \u{203a} test` block, a vitest `\u{2717} file > test` bullet, or
//! a go `--- FAIL: TestName` block -- either falls through unrecognised or,
//! worse, has some unrelated line inside it misread as a diagnostic trigger.
//! Precision matters more here than anywhere else in compaction: the calling
//! agent trusts the summary without ever seeing the raw text.
//!
//! [`extract_streaming`] picks a family from the command's own argv, then
//! requires that family's own output markers to confirm the guess before
//! trusting anything it found -- argv alone is not enough (`npm test` says
//! nothing about which runner it wraps), and a marker alone is not enough
//! either (a coincidental `FAILED` substring in unrelated output must not
//! masquerade as a test failure). `None` means "apply the generic scan
//! unchanged": the argv suggested no known family, the suggested family's
//! markers were never seen (a compile error before any test ran, say), or a
//! "some tests failed" marker was seen with nothing to correlate it to -- in
//! every one of those cases the generic scan is the more honest answer than
//! a family extraction with nothing in it.
//!
//! Review finding F4: runs as a bounded STREAMING line scan over the FULL
//! stored file, never only the capped display tail
//! `output::summarize_stored`'s Pass 1 (`workflow::verification::
//! read_capped_tail_and_scan`) retains -- a failure earlier than that tail
//! (a chatty run whose failure scrolled past 16 KiB before the rest of the
//! output finished) used to be invisible to every extractor here. Memory
//! stays bounded the way [`workflow::verification::FailureNameScanner`]
//! bounds its own full-stream cargo scan: at most `MAX_FAMILY_FAILURES`
//! recovered failures are ever held at once, never a second unbounded
//! accumulator or a second full read of a potentially huge log (the file is
//! read once, line by line, discarding each line immediately after it is
//! observed).

use std::sync::LazyLock;

use regex::Regex;

/// One failing test recovered from a family-shaped log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TestFailure {
    pub(crate) name: String,
    pub(crate) location: Option<String>,
    /// The first line of the failure's own message/assertion detail. Empty
    /// when the shape carries a name and location but no message line of
    /// its own.
    pub(crate) message: String,
}

/// How many failing tests one family extraction keeps before falling back to
/// a "... and N more" line, the identical role `output::MAX_FAILURE_BLOCKS`
/// plays for the generic scan.
const MAX_FAMILY_FAILURES: usize = 20;

/// What one family's extractor recovered. Only ever returned once that
/// family's own markers were confirmed (see [`extract_streaming`]'s own doc
/// comment); there is no "maybe" state past this point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FamilyExtraction {
    pub(crate) family: &'static str,
    pub(crate) failures: Vec<TestFailure>,
    pub(crate) truncated: bool,
}

impl FamilyExtraction {
    fn new(family: &'static str, mut failures: Vec<TestFailure>) -> Self {
        let truncated = failures.len() > MAX_FAMILY_FAILURES;
        failures.truncate(MAX_FAMILY_FAILURES);
        Self {
            family,
            failures,
            truncated,
        }
    }

    /// A run with a confirmed pass marker and nothing to report -- the
    /// property `output::render_summary` must never contradict: a run this
    /// extractor calls green is never allowed to render red.
    pub(crate) fn is_green(&self) -> bool {
        self.failures.is_empty()
    }

    /// Renders each failure as an `output::DisplayScan::failures`-shaped
    /// block (a trigger line plus its continuation lines) -- the shape
    /// `output::render_summary` already knows how to lay out, so replacing
    /// `DisplayScan::failures` with this is the entire integration.
    pub(crate) fn failure_blocks(&self) -> Vec<Vec<String>> {
        self.failures
            .iter()
            .map(|f| {
                let header = match &f.location {
                    Some(loc) => format!("FAILED {} -- {loc}", f.name),
                    None => format!("FAILED {}", f.name),
                };
                let mut block = vec![header];
                if !f.message.is_empty() {
                    block.push(format!("  {}", f.message));
                }
                block
            })
            .collect()
    }

    /// The one-line clean form for a green run, or the failing count for a
    /// red one -- pushed into `DisplayScan::summaries`, which
    /// `render_summary` always shows regardless of its own "structured"
    /// heuristic. Cargo/nextest already get an equivalent line from the
    /// generic scan's own `test result:`/`Summary [...]` recognition; for
    /// every other family this is the ONLY compact confirmation a green run
    /// gets.
    ///
    /// `pass1_failures` is `output::summarize_stored`'s Pass 1 (`workflow::
    /// verification::FailureNameScanner`, run over the full stream regardless
    /// of family): review finding F8, this extractor's own count comes only
    /// from `thread '...' panicked at ...:` lines, so a failed
    /// `#[should_panic]` test -- cargo still prints its own `test <name> ...
    /// FAILED` line for it, just never a panic line -- was undercounted here
    /// even though Pass 1 already had its name. The rendered count is the
    /// UNION of both sources' names, never just this extractor's own,
    /// smaller one.
    pub(crate) fn summary_line(
        &self,
        pass1_failures: &std::collections::BTreeSet<String>,
    ) -> String {
        if self.is_green() {
            format!("{}: all tests passed", self.family)
        } else {
            let mut names: std::collections::BTreeSet<&str> =
                self.failures.iter().map(|f| f.name.as_str()).collect();
            names.extend(pass1_failures.iter().map(String::as_str));
            format!(
                "{}: {} failed{}",
                self.family,
                names.len(),
                if self.truncated { " (truncated)" } else { "" }
            )
        }
    }
}

// Argv recognition, factored out once `matches_known_family` (review finding
// F4) needed the identical family-routing checks `extract`/`extract_streaming`
// already used -- one source of truth for "does this argv claim to be a
// `cargo test`/pytest/vitest/jest/go test run" rather than three copies that
// could drift apart.
fn is_cargo_family(lower: &str) -> bool {
    lower.contains("cargo test") || lower.contains("cargo-nextest") || lower.contains("nextest")
}
fn is_pytest_family(lower: &str) -> bool {
    lower.contains("pytest")
}
fn is_vitest_jest_family(lower: &str) -> bool {
    lower.contains("vitest") || lower.contains("jest")
}
fn is_go_test_family(lower: &str) -> bool {
    lower.contains("go test")
}

/// Review finding F4: the family extractors, run as a bounded streaming line
/// scan over the FULL stored file at `path` rather than only the capped
/// display tail (`output::MAX_FAILURE_OUTPUT_BYTES`) --
/// a `pytest`/`vitest`/`jest`/`go test` failure earlier than the last ~16
/// KiB of a chatty run must still be found. Memory stays bounded the same
/// way [`workflow::verification::FailureNameScanner`] bounds its own
/// full-stream cargo scan: at most [`MAX_FAMILY_FAILURES`] recovered
/// failures are ever held at once, past which further ones are dropped and
/// `truncated` is set, never a second unbounded accumulator.
pub(crate) fn extract_streaming(command: &str, path: &std::path::Path) -> Option<FamilyExtraction> {
    let lower = command.to_ascii_lowercase();
    if is_cargo_family(&lower) {
        return extract_cargo_nextest_streaming(path);
    }
    if is_pytest_family(&lower) {
        return extract_pytest_streaming(path);
    }
    if is_vitest_jest_family(&lower) {
        return extract_vitest_jest_streaming(path);
    }
    if is_go_test_family(&lower) {
        return extract_go_test_streaming(path);
    }
    None
}

/// Whether `command`'s argv identifies one of the test-runner families this
/// module models, independent of whether that family's own markers were
/// ever confirmed in its output. Review finding F4: lets
/// `output::render_summary` refuse to call a run "clean" on ambiguous
/// silence alone when the command itself claims to be a test run -- an
/// unconfirmed family extraction (nothing recognisable, or a read error cut
/// the scan short) is not the same thing as a confirmed green run, and must
/// fall back to the ordinary head/tail summary instead of a false "ok (...)"
/// one-liner.
pub(crate) fn matches_known_family(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    // `pytest --version`, `cargo nextest list`, `jest --help` name a runner
    // without running any test; their clean silence is genuine.
    let introspects = lower.split_whitespace().any(|token| {
        matches!(
            token,
            "--version" | "-v" | "--help" | "-h" | "--list" | "list" | "--collect-only"
        )
    });
    !introspects
        && (is_cargo_family(&lower)
            || is_pytest_family(&lower)
            || is_vitest_jest_family(&lower)
            || is_go_test_family(&lower))
}

/// Reads `path` as lines (byte-split on `\n`, lossily decoded, `\r` and the
/// trailing `\n` stripped -- the same treatment `output::scan_for_display`
/// gives a stored capture), calling `observe` once per line and discarding
/// each line immediately afterwards. Never loads the file whole: a
/// multi-hundred-MiB log costs one line's worth of memory at a time, plus
/// whatever bounded state `observe`'s closure keeps. Silently stops (rather
/// than propagating the error) on a read failure or a missing file --
/// whatever `observe` already collected is used as-is, the same fail-open
/// discipline `read_capped_tail_and_scan` applies to its own read errors.
fn stream_lines(path: &std::path::Path, mut observe: impl FnMut(&str)) {
    use std::io::BufRead as _;
    let Ok(file) = std::fs::File::open(path) else {
        return;
    };
    let mut reader = std::io::BufReader::new(file);
    let mut raw = Vec::new();
    loop {
        raw.clear();
        let n = match reader.read_until(b'\n', &mut raw) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let bytes: &[u8] = if raw.last() == Some(&b'\n') {
            &raw[..n - 1]
        } else {
            &raw[..n]
        };
        let line = String::from_utf8_lossy(bytes);
        let line = line.strip_suffix('\r').unwrap_or(&line);
        observe(line);
    }
}

fn extract_cargo_nextest_streaming(path: &std::path::Path) -> Option<FamilyExtraction> {
    let mut prev_trigger: Option<(String, String)> = None;
    let mut failures: Vec<TestFailure> = Vec::new();
    let mut truncated = false;
    let mut has_pass_marker = false;
    stream_lines(path, |line| {
        if let Some((name, location)) = prev_trigger.take() {
            let message = line.trim();
            if failures.len() < MAX_FAMILY_FAILURES {
                failures.push(TestFailure {
                    name,
                    location: Some(location),
                    message: message.to_string(),
                });
            } else {
                truncated = true;
            }
        }
        if let Some(caps) = CARGO_PANIC_RE.captures(line.trim_start()) {
            prev_trigger = Some((caps["name"].to_string(), caps["location"].to_string()));
        }
        let t = line.trim_start();
        if t.starts_with("test result: ok.")
            || (t.starts_with("Summary [") && !t.contains(" failed"))
        {
            has_pass_marker = true;
        }
    });
    // Flush a trailing trigger line that never got a following line (the
    // capture ended right after it) -- same empty-message fallback
    // `extract_cargo_nextest`'s `unwrap_or_default()` gives it.
    if let Some((name, location)) = prev_trigger.take() {
        if failures.len() < MAX_FAMILY_FAILURES {
            failures.push(TestFailure {
                name,
                location: Some(location),
                message: String::new(),
            });
        } else {
            truncated = true;
        }
    }
    if !confirmed(&failures, has_pass_marker) {
        return None;
    }
    let mut extraction = FamilyExtraction::new("cargo test", failures);
    extraction.truncated |= truncated;
    Some(extraction)
}

fn extract_pytest_streaming(path: &std::path::Path) -> Option<FamilyExtraction> {
    let mut failures: Vec<TestFailure> = Vec::new();
    let mut truncated = false;
    let mut has_pass_marker = false;
    stream_lines(path, |line| {
        let trimmed = line.trim();
        if let Some(caps) = PYTEST_FAILED_RE.captures(trimmed) {
            let nodeid = caps["nodeid"].to_string();
            let location = nodeid.split("::").next().map(|f| f.to_string());
            let message = caps
                .name("message")
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            if failures.len() < MAX_FAMILY_FAILURES {
                failures.push(TestFailure {
                    name: nodeid,
                    location,
                    message,
                });
            } else {
                truncated = true;
            }
            return;
        }
        if trimmed.starts_with("===")
            && trimmed.ends_with("===")
            && trimmed.contains(" passed")
            && !trimmed.contains(" failed")
        {
            has_pass_marker = true;
        }
    });
    if !confirmed(&failures, has_pass_marker) {
        return None;
    }
    let mut extraction = FamilyExtraction::new("pytest", failures);
    extraction.truncated |= truncated;
    Some(extraction)
}

/// The streaming counterpart of [`scan_blocks`]: a block starts on every line
/// `is_start` accepts (finalizing whatever block was open, exactly like
/// `scan_blocks` implicitly closing one block when the next start line
/// appears) and grows up to `max_block_lines`. Each finished block is handed
/// to `finalize` immediately and then dropped -- only the resulting
/// [`TestFailure`]s accumulate, capped at [`MAX_FAMILY_FAILURES`], so a
/// pathological log with thousands of tiny blocks still costs O(1) blocks'
/// worth of memory rather than growing without bound.
fn stream_blocks(
    path: &std::path::Path,
    max_block_lines: usize,
    mut is_start: impl FnMut(&str) -> bool,
    mut pass_marker: impl FnMut(&str) -> bool,
    mut finalize: impl FnMut(&[String]) -> Option<TestFailure>,
) -> (Vec<TestFailure>, bool, bool) {
    let mut current: Vec<String> = Vec::new();
    let mut failures: Vec<TestFailure> = Vec::new();
    let mut truncated = false;
    let mut has_pass_marker = false;
    let finish_current =
        |current: &mut Vec<String>,
         failures: &mut Vec<TestFailure>,
         truncated: &mut bool,
         finalize: &mut dyn FnMut(&[String]) -> Option<TestFailure>| {
            if current.is_empty() {
                return;
            }
            if let Some(failure) = finalize(current) {
                if failures.len() < MAX_FAMILY_FAILURES {
                    failures.push(failure);
                } else {
                    *truncated = true;
                }
            }
            current.clear();
        };
    stream_lines(path, |line| {
        if pass_marker(line) {
            has_pass_marker = true;
        }
        if is_start(line) {
            finish_current(&mut current, &mut failures, &mut truncated, &mut finalize);
            current.push(line.to_string());
        } else if !current.is_empty() && current.len() < max_block_lines {
            current.push(line.to_string());
        }
    });
    finish_current(&mut current, &mut failures, &mut truncated, &mut finalize);
    (failures, truncated, has_pass_marker)
}

fn extract_vitest_jest_streaming(path: &std::path::Path) -> Option<FamilyExtraction> {
    let is_start = |line: &str| {
        let t = line.trim_start();
        t.starts_with("\u{25cf} ") // jest: "● <suite> \u{203a} <test>"
            || t.starts_with("\u{2717} ") // vitest (older): "✗ <file> > <test>  <dur>"
            || t.starts_with("\u{d7} ") // vitest (newer): "× <file> > <test>  <dur>"
    };
    let pass_marker = |line: &str| {
        let t = line.trim_start();
        (t.starts_with("Tests:") || t.starts_with("Test Files") || t.starts_with("Tests "))
            && t.contains("passed")
            && !t.contains("failed")
    };
    let finalize = |block: &[String]| -> Option<TestFailure> {
        let header = block[0].trim_start();
        let name = if let Some(rest) = header.strip_prefix("\u{25cf} ") {
            rest.trim().to_string()
        } else {
            let rest = header.trim_start_matches(['\u{2717}', '\u{d7}']).trim();
            TRAILING_DURATION_RE.replace(rest, "").trim().to_string()
        };
        let refs: Vec<&str> = block.iter().map(String::as_str).collect();
        Some(TestFailure {
            name,
            location: block_location(&refs, 1),
            message: block_message(&refs, 1),
        })
    };
    let (failures, truncated, has_pass_marker) =
        stream_blocks(path, 12, is_start, pass_marker, finalize);
    if !confirmed(&failures, has_pass_marker) {
        return None;
    }
    let mut extraction = FamilyExtraction::new("vitest/jest", failures);
    extraction.truncated |= truncated;
    Some(extraction)
}

fn extract_go_test_streaming(path: &std::path::Path) -> Option<FamilyExtraction> {
    let is_start = |line: &str| line.trim_start().starts_with("--- FAIL: ");
    let pass_marker = |line: &str| {
        let t = line.trim();
        t == "PASS" || t.starts_with("ok ") || t.starts_with("ok\t")
    };
    let finalize = |block: &[String]| -> Option<TestFailure> {
        let caps = GO_FAIL_HEADER_RE.captures(block[0].trim_start())?;
        let mut location = None;
        let mut message = String::new();
        for line in block.iter().skip(1) {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if trimmed.starts_with("--- ") || trimmed.starts_with("=== RUN") {
                break;
            }
            if let Some(dcaps) = GO_DETAIL_RE.captures(trimmed) {
                location = Some(dcaps["location"].to_string());
                message = dcaps["message"].to_string();
            } else {
                message = trimmed.to_string();
            }
            break;
        }
        Some(TestFailure {
            name: caps["name"].to_string(),
            location,
            message,
        })
    };
    let (failures, truncated, has_pass_marker) =
        stream_blocks(path, 15, is_start, pass_marker, finalize);
    if !confirmed(&failures, has_pass_marker) {
        return None;
    }
    let mut extraction = FamilyExtraction::new("go test", failures);
    extraction.truncated |= truncated;
    Some(extraction)
}

/// A trigger line has been confirmed and there is nothing to correlate a
/// "some tests failed" marker to, OR nothing recognisable at all: in both
/// cases the generic scan is the more honest answer than an extraction with
/// nothing in it (see the module doc comment).
fn confirmed(failures: &[TestFailure], has_pass_marker: bool) -> bool {
    !failures.is_empty() || has_pass_marker
}

// ---------------------------------------------------------------------
// cargo test / cargo nextest -- families #1, sharing one extractor since a
// panic prints the identical "thread '<name>' panicked at <location>:" line
// under either runner; only the pass/fail summary line's own shape differs.
// ---------------------------------------------------------------------

static CARGO_PANIC_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^thread '(?P<name>.+)' panicked at (?P<location>.+):$")
        .expect("static cargo panic regex must compile")
});

// ---------------------------------------------------------------------
// pytest -- family #2. Failures are single self-contained lines in the
// default "short test summary info" section, so no block scan is needed.
// ---------------------------------------------------------------------

static PYTEST_FAILED_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^FAILED (?P<nodeid>\S+)(?: - (?P<message>.+))?$")
        .expect("static pytest FAILED regex must compile")
});

/// The first line within `block` (searched from `skip`) that looks like a
/// source location: jest's `at ... (file:line:col)` or vitest's
/// `\u{276f} file:line:col`.
static LOCATION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:at .*\(|\u{276f}\s*)(?P<loc>[^\s()]+:\d+:\d+)\)?")
        .expect("static jest/vitest location regex must compile")
});

fn block_location(block: &[&str], skip: usize) -> Option<String> {
    block
        .iter()
        .skip(skip)
        .find_map(|line| LOCATION_RE.captures(line))
        .map(|caps| caps["loc"].to_string())
}

/// The first non-blank line within `block` (searched from `skip`) that is
/// not itself a location line or another block's own trigger glyph.
fn block_message(block: &[&str], skip: usize) -> String {
    block
        .iter()
        .skip(skip)
        .map(|l| l.trim())
        .find(|t| {
            !t.is_empty()
                && !t.starts_with("at ")
                && !t.starts_with('\u{276f}')
                && !t.starts_with('\u{25cf}')
        })
        .unwrap_or_default()
        .to_string()
}

// ---------------------------------------------------------------------
// vitest / jest -- family #3, grouped together since both report one
// failure as a bullet/header line plus a nearby location line, differing
// only in glyph and whether the name and location share a line.
// ---------------------------------------------------------------------

/// Strips a trailing duration vitest appends to its own bullet line
/// (`  12ms`, `(12ms)`).
static TRAILING_DURATION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\s*\(?\d+(?:\.\d+)?\s*m?s\)?\s*$").expect("static duration regex must compile")
});

// ---------------------------------------------------------------------
// go test -- family #4.
// ---------------------------------------------------------------------

static GO_FAIL_HEADER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^--- FAIL: (?P<name>\S+) \(")
        .expect("static go test FAIL header regex must compile")
});

/// `go test`'s own default failure-detail shape: `t.Errorf`/`t.Fatalf` print
/// `<file>.go:<line>: <message>` with no further structure.
static GO_DETAIL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?P<location>\S+\.go:\d+): (?P<message>.+)$")
        .expect("static go test detail regex must compile")
});

#[cfg(test)]
mod tests {
    use super::*;

    /// Test-only shim over [`extract_streaming`] (review finding F4 replaced
    /// the old tail-scanning `extract` production entry point with the
    /// full-file streaming one): every fixture below is simplest to write as
    /// an in-memory string, so this stands it up as a temp file and calls
    /// the actual production function -- exercising the SAME parsing code
    /// every other caller exercises, never a second, only-tested-in-theory
    /// copy of it.
    fn extract(command: &str, tail: &str) -> Option<FamilyExtraction> {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("capture.log");
        std::fs::write(&path, tail).expect("write fixture");
        extract_streaming(command, &path)
    }

    // -- cargo test / nextest ------------------------------------------

    const CARGO_TWO_FAILURES: &str = "running 3 tests\n\
        test tests::alpha ... ok\n\
        test tests::beta ... FAILED\n\
        test tests::gamma ... FAILED\n\
        \n\
        failures:\n\
        \n\
        ---- tests::beta stdout ----\n\
        thread 'tests::beta' panicked at src/lib.rs:10:5:\n\
        assertion `left == right` failed\n\
        \n\
        ---- tests::gamma stdout ----\n\
        thread 'tests::gamma' panicked at src/other.rs:20:9:\n\
        explicit panic message\n\
        \n\
        failures:\n\
        \x20   tests::beta\n\
        \x20   tests::gamma\n\
        \n\
        test result: FAILED. 1 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out\n";

    #[test]
    fn cargo_extractor_names_every_failure_with_its_location() {
        let extraction = extract("cargo test", CARGO_TWO_FAILURES).expect("must confirm");
        assert_eq!(extraction.family, "cargo test");
        assert_eq!(extraction.failures.len(), 2);
        assert_eq!(extraction.failures[0].name, "tests::beta");
        assert_eq!(
            extraction.failures[0].location.as_deref(),
            Some("src/lib.rs:10:5")
        );
        assert_eq!(
            extraction.failures[0].message,
            "assertion `left == right` failed"
        );
        assert_eq!(extraction.failures[1].name, "tests::gamma");
        assert_eq!(
            extraction.failures[1].location.as_deref(),
            Some("src/other.rs:20:9")
        );
        assert!(!extraction.is_green());
    }

    const CARGO_CLEAN: &str = "running 3 tests\n\
        test tests::a ... ok\n\
        test tests::b ... ok\n\
        test tests::c ... ok\n\
        \n\
        test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n";

    #[test]
    fn cargo_green_run_is_never_reported_red() {
        let extraction = extract("cargo test", CARGO_CLEAN).expect("must confirm a clean run");
        assert!(extraction.is_green());
        assert!(extraction.failures.is_empty());
        assert!(extraction.failure_blocks().is_empty());
    }

    const NEXTEST_TWO_FAILURES: &str = "    Starting 3 tests across 1 binary\n\
        FAIL [   0.010s] zirv tests::beta\n\
        --- STDOUT:              tests::beta ---\n\
        thread 'tests::beta' panicked at src/lib.rs:10:5:\n\
        assertion failed\n\
        FAIL [   0.012s] zirv tests::gamma\n\
        --- STDOUT:              tests::gamma ---\n\
        thread 'tests::gamma' panicked at src/other.rs:20:9:\n\
        boom\n\
        Summary [   0.020s] 3 tests run: 1 passed, 2 failed, 0 skipped\n";

    #[test]
    fn nextest_extractor_names_every_failure_with_its_location() {
        let extraction = extract("cargo nextest run", NEXTEST_TWO_FAILURES).expect("must confirm");
        assert_eq!(extraction.failures.len(), 2);
        assert_eq!(extraction.failures[0].name, "tests::beta");
        assert_eq!(
            extraction.failures[0].location.as_deref(),
            Some("src/lib.rs:10:5")
        );
    }

    const NEXTEST_CLEAN: &str = "    Starting 3 tests across 1 binary\n\
        PASS [   0.003s] zirv tests::a\n\
        Summary [   0.010s] 3 tests run: 3 passed, 0 skipped\n";

    #[test]
    fn nextest_green_run_is_never_reported_red() {
        let extraction = extract("cargo nextest run", NEXTEST_CLEAN).expect("must confirm");
        assert!(extraction.is_green());
    }

    /// A compile error prints `error[...]`/`-->` but no `test result:`/
    /// `Summary [...]` line and no panic at all -- the extractor must decline
    /// so the generic scan (which already knows rustc's own vocabulary)
    /// reports it.
    #[test]
    fn cargo_extractor_declines_a_compile_error_before_tests_ran() {
        let text = "error[E0308]: mismatched types\n  --> src/lib.rs:1:1\n";
        assert_eq!(extract("cargo test", text), None);
    }

    // -- pytest ----------------------------------------------------------

    const PYTEST_TWO_FAILURES: &str = "============================= test session starts ==============================\n\
        collected 4 items\n\
        \n\
        test_foo.py::test_alpha PASSED\n\
        test_foo.py::test_beta FAILED\n\
        test_bar.py::test_gamma FAILED\n\
        \n\
        =================================== FAILURES ===================================\n\
        _______________________________ test_beta _______________________________\n\
        E   assert 1 == 2\n\
        =========================== short test summary info ===========================\n\
        FAILED test_foo.py::test_beta - assert 1 == 2\n\
        FAILED test_bar.py::test_gamma - ValueError: bad value\n\
        ========================= 2 failed, 2 passed in 0.05s =========================\n";

    #[test]
    fn pytest_extractor_names_every_failure_with_its_file() {
        let extraction = extract("pytest -q", PYTEST_TWO_FAILURES).expect("must confirm");
        assert_eq!(extraction.family, "pytest");
        assert_eq!(extraction.failures.len(), 2);
        assert_eq!(extraction.failures[0].name, "test_foo.py::test_beta");
        assert_eq!(
            extraction.failures[0].location.as_deref(),
            Some("test_foo.py")
        );
        assert_eq!(extraction.failures[0].message, "assert 1 == 2");
        assert_eq!(extraction.failures[1].name, "test_bar.py::test_gamma");
        assert_eq!(extraction.failures[1].message, "ValueError: bad value");
    }

    const PYTEST_CLEAN: &str = "============================= test session starts ==============================\n\
        collected 5 items\n\
        test_foo.py .....\n\
        ============================== 5 passed in 0.05s ===============================\n";

    #[test]
    fn pytest_green_run_is_never_reported_red() {
        let extraction = extract("pytest", PYTEST_CLEAN).expect("must confirm");
        assert!(extraction.is_green());
    }

    #[test]
    fn pytest_extractor_declines_unrelated_output() {
        assert_eq!(
            extract("pytest", "collecting ...\nnothing recognisable\n"),
            None
        );
    }

    // -- vitest / jest -----------------------------------------------------

    const JEST_TWO_FAILURES: &str = "FAIL src/foo.test.js\n\
        \u{2713} passes (2ms)\n\
        \u{2715} fails alpha (3ms)\n\
        \u{2715} fails beta (1ms)\n\
        \n\
        \u{25cf} Suite \u{203a} fails alpha\n\
        \n\
        expect(received).toBe(expected)\n\
        \n\
        Expected: 2\n\
        Received: 1\n\
        \n\
        at Object.<anonymous> (src/foo.test.js:10:20)\n\
        \n\
        \u{25cf} Suite \u{203a} fails beta\n\
        \n\
        expect(received).toBe(expected)\n\
        \n\
        at Object.<anonymous> (src/foo.test.js:22:15)\n\
        \n\
        Tests:       2 failed, 1 passed, 3 total\n";

    #[test]
    fn jest_extractor_names_every_failure_with_its_location() {
        let extraction = extract("jest", JEST_TWO_FAILURES).expect("must confirm");
        assert_eq!(extraction.family, "vitest/jest");
        assert_eq!(extraction.failures.len(), 2);
        assert_eq!(extraction.failures[0].name, "Suite \u{203a} fails alpha");
        assert_eq!(
            extraction.failures[0].location.as_deref(),
            Some("src/foo.test.js:10:20")
        );
        assert_eq!(
            extraction.failures[0].message,
            "expect(received).toBe(expected)"
        );
        assert_eq!(extraction.failures[1].name, "Suite \u{203a} fails beta");
        assert_eq!(
            extraction.failures[1].location.as_deref(),
            Some("src/foo.test.js:22:15")
        );
    }

    const JEST_CLEAN: &str = "PASS src/foo.test.js\n\
        \u{2713} passes (2ms)\n\
        \n\
        Tests:       3 passed, 3 total\n";

    #[test]
    fn jest_green_run_is_never_reported_red() {
        let extraction = extract("jest", JEST_CLEAN).expect("must confirm");
        assert!(extraction.is_green());
    }

    const VITEST_TWO_FAILURES: &str = "\u{2713} src/foo.test.ts > adds numbers 2ms\n\
        \u{2717} src/foo.test.ts > subtracts numbers 3ms\n\
        AssertionError: expected 1 to be 2\n\
        \u{276f} src/foo.test.ts:10:5\n\
        \u{2717} src/bar.test.ts > divides numbers 1ms\n\
        AssertionError: expected Infinity to be 0\n\
        \u{276f} src/bar.test.ts:4:3\n\
        \n\
        Test Files  1 failed | 1 passed (2)\n\
        Tests  2 failed | 1 passed (3)\n";

    #[test]
    fn vitest_extractor_names_every_failure_with_its_location() {
        let extraction = extract("vitest run", VITEST_TWO_FAILURES).expect("must confirm");
        assert_eq!(extraction.failures.len(), 2);
        assert_eq!(
            extraction.failures[0].name,
            "src/foo.test.ts > subtracts numbers"
        );
        assert_eq!(
            extraction.failures[0].location.as_deref(),
            Some("src/foo.test.ts:10:5")
        );
        assert_eq!(
            extraction.failures[0].message,
            "AssertionError: expected 1 to be 2"
        );
        assert_eq!(
            extraction.failures[1].name,
            "src/bar.test.ts > divides numbers"
        );
    }

    const VITEST_CLEAN: &str = "\u{2713} src/foo.test.ts > adds numbers 2ms\n\
        \n\
        Test Files  1 passed (1)\n\
        Tests  1 passed (1)\n";

    #[test]
    fn vitest_green_run_is_never_reported_red() {
        let extraction = extract("vitest run", VITEST_CLEAN).expect("must confirm");
        assert!(extraction.is_green());
    }

    // -- go test -----------------------------------------------------------

    const GO_TWO_FAILURES: &str = "=== RUN   TestAlpha\n\
        --- PASS: TestAlpha (0.00s)\n\
        === RUN   TestBeta\n\
        --- FAIL: TestBeta (0.00s)\n\
        \x20   main_test.go:15: expected foo, got bar\n\
        === RUN   TestGamma\n\
        --- FAIL: TestGamma (0.00s)\n\
        \x20   other_test.go:8: unexpected nil error\n\
        FAIL\n\
        FAIL\texample.com/mypkg\t0.004s\n";

    #[test]
    fn go_test_extractor_names_every_failure_with_its_location() {
        let extraction = extract("go test ./...", GO_TWO_FAILURES).expect("must confirm");
        assert_eq!(extraction.family, "go test");
        assert_eq!(extraction.failures.len(), 2);
        assert_eq!(extraction.failures[0].name, "TestBeta");
        assert_eq!(
            extraction.failures[0].location.as_deref(),
            Some("main_test.go:15")
        );
        assert_eq!(extraction.failures[0].message, "expected foo, got bar");
        assert_eq!(extraction.failures[1].name, "TestGamma");
        assert_eq!(
            extraction.failures[1].location.as_deref(),
            Some("other_test.go:8")
        );
    }

    const GO_CLEAN: &str = "=== RUN   TestAlpha\n\
        --- PASS: TestAlpha (0.00s)\n\
        PASS\n\
        ok  \texample.com/mypkg\t0.003s\n";

    #[test]
    fn go_test_green_run_is_never_reported_red() {
        let extraction = extract("go test ./...", GO_CLEAN).expect("must confirm");
        assert!(extraction.is_green());
    }

    #[test]
    fn go_test_extractor_declines_unrelated_output() {
        assert_eq!(extract("go test ./...", "downloading modules ...\n"), None);
    }

    // -- dispatch ------------------------------------------------------

    /// An argv the dispatcher does not model at all falls straight through
    /// with no attempt at extraction.
    #[test]
    fn extract_returns_none_for_an_unrecognised_command() {
        assert_eq!(extract("make all", "anything at all\n"), None);
    }

    /// `failure_blocks` never carries a `@@`/hunk-shaped artifact and always
    /// renders the location alongside the name when one was found.
    #[test]
    fn failure_blocks_render_the_name_location_and_message() {
        let extraction = extract("cargo test", CARGO_TWO_FAILURES).expect("must confirm");
        let blocks = extraction.failure_blocks();
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0][0].contains("tests::beta"));
        assert!(blocks[0][0].contains("src/lib.rs:10:5"));
        assert!(blocks[0][1].contains("assertion"));
    }

    #[test]
    fn a_runner_introspection_query_is_not_a_test_run() {
        for command in [
            "pytest --version",
            "cargo nextest list",
            "jest --help",
            "go test -h",
        ] {
            assert!(!matches_known_family(command), "{command}");
        }
        for command in [
            "pytest tests/",
            "cargo nextest run",
            "npx vitest",
            "go test ./...",
        ] {
            assert!(matches_known_family(command), "{command}");
        }
    }
}
