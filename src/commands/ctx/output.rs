//! Issue #326: reversible compression of verbose command output.
//!
//! `zirv ctx run --compact -- <argv...>` runs `argv` DIRECTLY (no shell),
//! writes every byte both streams produce to one file under the state dir,
//! and prints a small, information-preserving summary instead of the output
//! itself. Nothing is lost: `zirv ctx output show <id> [--range A-B]` hands
//! back the exact stored lines, and the summary always ends with the one
//! line that names it.
//!
//! The point is the calling agent's context window. A `cargo test` run is
//! tens of thousands of tokens of compiler progress lines wrapping a few
//! hundred tokens that matter; a summary that keeps the `test result:` lines,
//! every failing test name, every `error`/`warning:` line with its `-->`
//! location, and every panic message keeps all of the signal at a fraction of
//! the cost -- and the retrieval line means the agent can still get the rest
//! when it genuinely needs it.
//!
//! Classification is NOT a second implementation: failing test names and the
//! `test result:`/`Summary [...]` recognition come from
//! [`crate::commands::workflow::verification::read_capped_tail_and_scan`] and
//! its `FailureNameScanner`, the same streaming scanner every workflow check
//! already goes through. Only the display shaping (head/tail lines,
//! diagnostic lines, counts) is new, and it is a bounded second pass over the
//! stored file rather than a second classifier.

use std::collections::VecDeque;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::CtxResult;
use super::config::{CtxConfig, EnvLookup, env_from_process};
use super::output_shape;
use super::state::{self, StateDir};
use crate::commands::workflow::verification::{
    MAX_FAILURE_OUTPUT_BYTES, read_capped_tail_and_scan, scrub_output,
};

/// How many stored outputs one repository keeps. Old logs are pruned newest-
/// first on every run, so a machine that compacts every build for months does
/// not accumulate one file per command forever.
pub(crate) const KEEP_NEWEST_OUTPUTS: usize = 50;

/// Generic-output head/tail budgets. Deliberately asymmetric: a failure is
/// far more often at the end of a build log than at its start.
const HEAD_LINES: usize = 20;
const TAIL_LINES: usize = 40;

/// How much tail survives even in structured mode. A test/compile summary
/// lifts the lines that matter out of the output, but "the last thing that
/// happened" is signal a lifted line cannot replace -- and dropping it
/// entirely is what let a `warning:` in an otherwise-generic result hide the
/// `fatal:` that followed it.
const STRUCTURED_TAIL_LINES: usize = 10;

/// Ceilings on the collected sections, so one pathological producer cannot
/// spend the whole summary budget on a single section before the retrieval
/// line is even reached. Failures and warnings are bounded SEPARATELY: sixty
/// warnings must never exhaust the budget a later error needs.
const MAX_FAILURE_BLOCKS: usize = 20;
const MAX_WARNING_BLOCKS: usize = 20;
const MAX_BLOCK_LINES: usize = 12;
const MAX_FAILURE_LINES: usize = 60;
const MAX_SUMMARY_LINES: usize = 10;

/// Per-line display cap. One 200 KiB line (a minified bundle, a base64 blob)
/// would otherwise eat the entire summary on its own.
const MAX_LINE_BYTES: usize = 200;

const OUTPUT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, clap::Args)]
pub struct RunArgs {
    /// Print a compact summary instead of the command's own output. The
    /// default, and the documented spelling.
    #[arg(long)]
    pub compact: bool,
    /// Print the stored output verbatim after the command finishes, instead
    /// of the summary. The full output is persisted either way.
    #[arg(long, conflicts_with = "compact")]
    pub full: bool,
    /// The command to run, after `--`. Executed directly: no shell, no
    /// metacharacter interpretation, no `PATH`-less magic beyond the ordinary
    /// program lookup.
    #[arg(allow_hyphen_values = true, last = true, required = true)]
    pub command: Vec<String>,
}

#[derive(Debug, clap::Args)]
pub struct OutputArgs {
    #[command(subcommand)]
    pub command: OutputVerb,
}

#[derive(Debug, clap::Subcommand)]
pub enum OutputVerb {
    /// Print the exact stored lines of one captured output.
    Show(ShowArgs),
    /// List this repository's stored outputs, newest first.
    List(ListArgs),
}

#[derive(Debug, clap::Args)]
pub struct ShowArgs {
    /// The id printed by `zirv ctx run`'s own retrieval line.
    pub id: String,
    /// 1-based inclusive line range, `START-END`. `START-` runs to the end
    /// of the file. Omitted means "from line 1".
    #[arg(long)]
    pub range: Option<String>,
    /// 1-based inclusive BYTE range within the selected line, `START-END`
    /// (`START-` runs to the end of the line). For a single line larger than
    /// the whole output window, where no `--range` can narrow further: the
    /// window slides within the line instead, and each cut names the next
    /// offset to ask for.
    #[arg(long)]
    pub bytes: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct ListArgs {
    /// How many entries to print, newest first.
    #[arg(long, default_value_t = 20)]
    pub limit: usize,
}

/// The sidecar record for one stored output. Written next to the `.log` it
/// describes so `output list` can render a listing without reading (or
/// counting the lines of) every log it names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct OutputRecord {
    pub(crate) schema_version: u32,
    pub(crate) id: String,
    pub(crate) command: Vec<String>,
    /// `None` for a capture that never saw a status of its own -- claude's
    /// Bash tool result carries the output, not the exit code (see
    /// `hook::run_posttool`).
    #[serde(default)]
    pub(crate) exit_code: Option<i32>,
    pub(crate) started_at: u64,
    pub(crate) lines: usize,
    pub(crate) bytes: u64,
}

// ---------------------------------------------------------------------
// Pure: summary shaping
// ---------------------------------------------------------------------

/// What the display pass recovered from the stored file. Every field is
/// bounded by the constants above, so this struct's size never depends on how
/// much output the command produced.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct DisplayScan {
    pub(crate) total_lines: usize,
    pub(crate) total_bytes: u64,
    pub(crate) head: Vec<String>,
    pub(crate) tail: VecDeque<String>,
    /// Complete failure/panic blocks -- each a trigger line plus the
    /// continuation lines that belong to it (a rustc `-->` location, a
    /// panic's `assertion`/`left:`/`right:` lines, an indented snippet).
    /// MANDATORY content: a summary that cannot fit these is not emitted.
    pub(crate) failures: Vec<Vec<String>>,
    /// The same shape for `warning:` blocks, kept apart and bounded apart so
    /// a flood of warnings can never crowd out a later failure, and so the
    /// renderer can drop them first when the budget runs short.
    pub(crate) warnings: Vec<Vec<String>>,
    pub(crate) summaries: Vec<String>,
    pub(crate) failures_truncated: bool,
    pub(crate) warnings_truncated: bool,
    /// The stored file could not be read to the end. Never silently folded
    /// into a clean scan: an empty scan that looks complete is worse than an
    /// honest one that says it is short.
    pub(crate) read_error: bool,
}

/// One display line: control characters scrubbed (a build log is repository-
/// controlled text on its way to an operator's terminal, exactly the threat
/// `verification::scrub_output` already handles) and capped, so no single
/// line can dominate the summary.
fn display_line(raw: &str) -> String {
    let scrubbed = scrub_output(raw).replace(['\n', '\t'], " ");
    let trimmed = scrubbed.trim_end();
    if trimmed.len() <= MAX_LINE_BYTES {
        return trimmed.to_string();
    }
    let mut cut = MAX_LINE_BYTES;
    while cut > 0 && !trimmed.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{} [...]", &trimmed[..cut])
}

/// How much a diagnostic line is allowed to cost. The distinction is the
/// whole point of issue #326's review finding 3: a `warning:` is nice to
/// have, a `fatal:`/`error:`/panic is the reason anyone reads the summary at
/// all, and a summary that drops the second to make room for sixty of the
/// first is worse than no summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Severity {
    Failure,
    Warning,
}

/// Whether `line` opens a diagnostic block, and how severe it is. The
/// continuation lines that belong to it are collected by the caller's own
/// `continues_diagnostic_block` walk, so an unrelated `-->` elsewhere in
/// ordinary output is never promoted on its own.
pub(crate) fn diagnostic_severity(line: &str) -> Option<Severity> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("error:")
        || trimmed.starts_with("error[")
        || trimmed.starts_with("error TS")
        // `git`'s own failure vocabulary, which carries no `error:` prefix.
        || trimmed.starts_with("fatal:")
        || trimmed.contains("panicked at")
        || trimmed.starts_with("thread '")
    {
        return Some(Severity::Failure);
    }
    trimmed.starts_with("warning:").then_some(Severity::Warning)
}

/// Whether `line` is a test-runner summary line worth keeping verbatim.
/// Matches the two shapes `verification`'s own scanner already recognizes
/// (`cargo test`'s `test result:` and `cargo nextest`'s `Summary [...]`).
pub(crate) fn is_summary_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("test result:") || trimmed.starts_with("Summary [")
}

/// Whether `line` continues the diagnostic block that opened above it: a
/// rustc `--> file:line:col`, an indented continuation, or one of the
/// assertion fields a Rust panic prints on its own following lines
/// (`left:`/`right:`/`assertion ...`). A blank line, or any line that starts
/// a new unindented statement, ends the block.
fn continues_diagnostic_block(line: &str) -> bool {
    if line.trim().is_empty() {
        return false;
    }
    let trimmed = line.trim_start();
    trimmed.starts_with("-->")
        || trimmed.starts_with("left:")
        || trimmed.starts_with("right:")
        || trimmed.starts_with("assertion")
        || trimmed.starts_with("note:")
        || trimmed.starts_with("help:")
        || trimmed.starts_with('|')
        || trimmed.starts_with('=')
        // Any indented line: rustc's own snippet body, cargo's nested detail,
        // and a panic's message continuation all arrive this way.
        || line.starts_with(' ')
        || line.starts_with('\t')
}

/// Reads `reader` as BYTE lines, never `BufRead::lines`.
///
/// `lines()` yields `Err` on the first non-UTF-8 byte, and the previous loop
/// stopped there -- so one 0xFF from a legacy-code-page compiler diagnostic
/// produced an EMPTY scan that then rendered as a confident "captured 0
/// lines" summary. Splitting on `b'\n'` and decoding each line lossily keeps
/// every line, and a genuine I/O failure sets `read_error` instead of quietly
/// looking like a clean end of file. The stored file itself is untouched
/// either way: lossy decoding happens only on the way to the summary.
///
/// Collects, all bounded: the counts, the first [`HEAD_LINES`], the last
/// [`TAIL_LINES`], the test-runner summary lines, complete FAILURE blocks
/// (each trigger line plus its continuation lines), and -- separately, so
/// they can be dropped first when the budget runs short -- warning blocks.
pub(crate) fn scan_for_display(mut reader: impl BufRead) -> DisplayScan {
    let mut scan = DisplayScan::default();
    let mut open_block: Option<Severity> = None;
    loop {
        let mut raw = Vec::new();
        // `read_until` directly, never `BufRead::split`: its `Ok` count is
        // the EXACT number of bytes consumed (delimiter included when one
        // was found), which is what issue #410's never-worse guard compares
        // a summary's length against. Reconstructing that count from each
        // chunk's post-strip length (the previous approach) always assumed a
        // trailing delimiter was present, overcounting by one byte whenever
        // the captured output did not actually end in a newline.
        let n = match reader.read_until(b'\n', &mut raw) {
            Ok(n) => n,
            Err(_) => {
                scan.read_error = true;
                break;
            }
        };
        if n == 0 {
            break;
        }
        scan.total_bytes = scan.total_bytes.saturating_add(n as u64);
        let bytes: &[u8] = if raw.last() == Some(&b'\n') {
            &raw[..raw.len() - 1]
        } else {
            &raw[..]
        };
        let line = String::from_utf8_lossy(bytes);
        let line = line.strip_suffix('\r').unwrap_or(&line);
        scan.total_lines += 1;

        if scan.head.len() < HEAD_LINES {
            scan.head.push(display_line(line));
        }
        scan.tail.push_back(display_line(line));
        if scan.tail.len() > TAIL_LINES {
            scan.tail.pop_front();
        }

        if is_summary_line(line) {
            if scan.summaries.len() < MAX_SUMMARY_LINES {
                scan.summaries.push(display_line(line));
            }
            open_block = None;
            continue;
        }

        // A new trigger always opens its own block, even in the middle of
        // another one: sixty warnings must never stop a later error from
        // being collected, which is why the two severities have separate,
        // separately-bounded lists.
        if let Some(severity) = diagnostic_severity(line) {
            let block = vec![display_line(line)];
            match severity {
                Severity::Failure if scan.failures.len() < MAX_FAILURE_BLOCKS => {
                    scan.failures.push(block);
                }
                Severity::Failure => scan.failures_truncated = true,
                Severity::Warning if scan.warnings.len() < MAX_WARNING_BLOCKS => {
                    scan.warnings.push(block);
                }
                Severity::Warning => scan.warnings_truncated = true,
            }
            open_block = Some(severity);
            continue;
        }

        if let Some(severity) = open_block {
            if continues_diagnostic_block(line) {
                let block = match severity {
                    Severity::Failure => scan.failures.last_mut(),
                    Severity::Warning => scan.warnings.last_mut(),
                };
                if let Some(block) = block
                    && block.len() < MAX_BLOCK_LINES
                {
                    block.push(display_line(line));
                }
                continue;
            }
            open_block = None;
        }
    }
    scan
}

/// The one retrieval line every summary ends with. Kept short and literal:
/// it is meant to be copied verbatim by whatever read the summary.
pub(crate) fn retrieval_line(id: &str) -> String {
    format!("full output: zirv ctx output show {id} [--range START-END]")
}

fn push_section(body: &mut String, title: &str, lines: impl IntoIterator<Item = String>) {
    let lines: Vec<String> = lines.into_iter().collect();
    if lines.is_empty() {
        return;
    }
    body.push_str(title);
    body.push('\n');
    for line in lines {
        body.push_str("  ");
        body.push_str(&line);
        body.push('\n');
    }
}

fn push_blocks(body: &mut String, title: &str, blocks: &[Vec<String>], truncated: bool) {
    if blocks.is_empty() {
        return;
    }
    body.push_str(title);
    body.push('\n');
    for block in blocks {
        for line in block {
            body.push_str("  ");
            body.push_str(line);
            body.push('\n');
        }
    }
    if truncated {
        body.push_str("  ... [more in the full output]\n");
    }
}

/// Names the lines the summary did NOT show, so nothing has to infer it from
/// a missing count. `None` when head and tail between them cover the file.
fn omitted_range_line(
    id: &str,
    shown_head: usize,
    shown_tail: usize,
    total: usize,
) -> Option<String> {
    let covered = shown_head.saturating_add(shown_tail);
    if covered >= total {
        return None;
    }
    let first = shown_head + 1;
    let last = total - shown_tail;
    Some(format!(
        "{} lines omitted between line {first} and line {last} -- fetch with \
         zirv ctx output show {id} --range {first}-{last}\n",
        total - covered
    ))
}

/// Renders the summary, or `None` when the MANDATORY content -- the header,
/// the test-result lines, every failing test name and every failure/panic
/// block -- does not fit inside `max_bytes` alongside the retrieval line.
///
/// `None` means FAIL OPEN, and the callers honour it: `run --compact` prints
/// the raw tail instead, and `hook::run_posttool` prints nothing at all so
/// claude keeps the original result. That is the whole discipline behind this
/// function. Emitting a summary that silently dropped a `fatal:` because
/// sixty warnings came first is not compression, it is corruption -- and the
/// caller has no way to tell the difference from the outside, so it has to be
/// decided here.
///
/// Budget order, strictest first: header and counts, the test-runner summary
/// lines, the failing test names, the failure/panic blocks (all mandatory);
/// then a bounded tail, which is kept even in structured mode because "the
/// last thing that happened" is signal no lifted line replaces; then, for an
/// unrecognised shape, the head; then warnings, with whatever is left. The
/// retrieval line is appended last and always survives.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_summary(
    id: &str,
    command: &str,
    exit_code: Option<i32>,
    scan: &DisplayScan,
    failures: &std::collections::BTreeSet<String>,
    summary_seen: bool,
    max_bytes: usize,
) -> Option<String> {
    let retrieval = retrieval_line(id);
    // The retrieval line plus its newline is the floor; `config::
    // MIN_MAX_SUMMARY_BYTES` keeps a configured cap above it, and this guard
    // makes the invariant local rather than assumed.
    let budget = max_bytes.checked_sub(retrieval.len() + 1)?;
    let raw_bytes = scan.total_bytes as usize;

    let mut body = String::new();
    body.push_str(&match exit_code {
        // The PostToolUse path (`hook::run_posttool`) has no exit code of its
        // own: claude's Bash tool result carries the output, not the status.
        Some(code) => format!(
            "zirv compacted output: exit {code} -- {}\n",
            display_line(command)
        ),
        None => format!("zirv compacted output: {}\n", display_line(command)),
    });
    body.push_str(&format!(
        "captured {} lines, {} bytes\n",
        scan.total_lines, scan.total_bytes
    ));
    if scan.read_error {
        body.push_str("note: the stored output ended in a read error; it may be truncated\n");
    }

    push_section(&mut body, "test summary:", scan.summaries.clone());
    if !failures.is_empty() {
        let shown: Vec<String> = failures.iter().take(MAX_FAILURE_LINES).cloned().collect();
        push_section(
            &mut body,
            &format!("failing tests ({}):", failures.len()),
            shown,
        );
        if failures.len() > MAX_FAILURE_LINES {
            body.push_str("  ... [more failing tests in the full output]\n");
        }
    }
    // Issue #408: repeated diagnostics that share a signature collapse to
    // one `[x N]` line before rendering, but only when doing so actually
    // shrinks the section -- a handful of already-distinct blocks render
    // exactly as `scan_for_display` collected them.
    let failure_blocks = output_shape::shaped_diagnostic_blocks(&scan.failures);
    push_blocks(
        &mut body,
        "failures:",
        &failure_blocks,
        scan.failures_truncated,
    );

    // Everything above is mandatory. If it does not fit, there is no honest
    // summary to emit at all.
    if body.len() > budget {
        return None;
    }

    let structured = summary_seen || !failures.is_empty() || !scan.failures.is_empty();
    let tail_lines = if structured {
        STRUCTURED_TAIL_LINES.min(scan.tail.len())
    } else {
        scan.tail.len()
    };
    let head_lines = if structured { 0 } else { scan.head.len() };

    let mut optional = String::new();
    if tail_lines > 0 {
        push_section(
            &mut optional,
            &format!("tail ({tail_lines} of {}):", scan.total_lines),
            scan.tail
                .iter()
                .skip(scan.tail.len() - tail_lines)
                .cloned()
                .collect::<Vec<_>>(),
        );
    }
    let tail_pushed = body.len() + optional.len() <= budget;
    if tail_pushed {
        body.push_str(&optional);
    }

    if head_lines > 0 {
        let mut head = String::new();
        push_section(
            &mut head,
            &format!("head ({head_lines} of {}):", scan.total_lines),
            scan.head.clone(),
        );
        if body.len() + head.len() <= budget {
            // The head goes ABOVE the tail when both are shown, so the
            // summary still reads in the order the output was produced.
            // Review finding 3: that subtraction is only valid when the tail
            // was actually appended above -- when it did not fit the budget
            // (`tail_pushed` false), `body` carries none of `optional`'s
            // bytes at all, and subtracting its length spliced the head
            // somewhere inside the mandatory block instead of after it.
            let tail_at = if tail_pushed {
                body.len() - optional.len().min(body.len())
            } else {
                body.len()
            };
            body.insert_str(tail_at, &head);
        }
    }

    if let Some(note) = omitted_range_line(id, head_lines, tail_lines, scan.total_lines)
        && body.len() + note.len() <= budget
    {
        body.push_str(&note);
    }

    let warning_blocks = output_shape::shaped_diagnostic_blocks(&scan.warnings);
    let mut warnings = String::new();
    push_blocks(
        &mut warnings,
        "warnings:",
        &warning_blocks,
        scan.warnings_truncated,
    );
    if !warnings.is_empty() && body.len() + warnings.len() <= budget {
        body.push_str(&warnings);
    }

    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    body.push_str(&retrieval);
    body.push('\n');
    debug_assert!(body.len() <= max_bytes);
    // Issue #410: the whole point of a summary is to be smaller than what it
    // replaces. A raw output just above `compact_min_bytes` can still gain
    // bytes back from section headers and the retrieval line -- if that
    // happened, there is no honest summary to emit, same as the mandatory
    // content not fitting the budget at all.
    if body.len() >= raw_bytes {
        return None;
    }
    Some(body)
}

// ---------------------------------------------------------------------
// Pure: what may be compacted at all
// ---------------------------------------------------------------------

/// Programs whose output a model READS, verbatim, before acting on it. A
/// head/tail summary of `cat`, `sed -n`, `rg` or `git diff` does not lose
/// noise, it loses the middle of the thing about to be edited -- and the
/// model has no way to tell, so it edits against text it never saw. These are
/// never compacted at any size.
const VERBATIM_PROGRAMS: &[&str] = &[
    "cat",
    "type",
    "get-content",
    "gc",
    "sed",
    "head",
    "tail",
    "less",
    "more",
    "awk",
    "cut",
    "grep",
    "egrep",
    "fgrep",
    "rg",
    "find",
    "fd",
    "ls",
    "dir",
    "tree",
    "jq",
    "yq",
    "diff",
    "xxd",
    "od",
    "hexdump",
    "strings",
];

/// `git` subcommands that are reads of content rather than progress logs.
/// `diff`/`show` moved to [`DIFF_GIT_SUBCOMMANDS`] (issue #412): a bounded,
/// lossless-shape summary is possible for those because their grammar is
/// known, unlike `blame`/`grep`, which stay verbatim at any size.
const VERBATIM_GIT_SUBCOMMANDS: &[&str] = &["blame", "grep"];

/// `git` subcommands whose output is a unified diff: bounded above
/// [`crate::commands::ctx::config::OutputConfig::diff_max_bytes`] by a
/// per-file listing (`output_diff::render_diff_summary`) rather than a
/// head/tail, since a diff's omitted middle is exactly the part a model is
/// about to edit against (issue #412). `log -p`/`--patch` is caught
/// separately below, since plain `git log` stays a `Known` progress log.
const DIFF_GIT_SUBCOMMANDS: &[&str] = &["diff", "show", "format-patch"];

/// Programs whose output shape zirv actually models -- test runners,
/// compilers, package managers, VCS progress. For these the summary provably
/// keeps the `test result:` lines, the failing test names and every failure
/// block, so compacting early is a straight win.
const KNOWN_PROGRAMS: &[&str] = &[
    "cargo", "nextest", "rustc", "npm", "pnpm", "yarn", "npx", "pytest", "python", "python3", "go",
    "dotnet", "make", "mvn", "gradle", "tsc", "eslint", "docker", "git",
];

/// `git` subcommands that are progress logs rather than content reads.
/// `log` (without `-p`/`--patch`, which `classify_compaction` catches
/// separately as `Verbatim`) is a commit-summary progress log in exactly
/// this sense.
const KNOWN_GIT_SUBCOMMANDS: &[&str] = &["fetch", "pull", "push", "clone", "status", "log"];

/// How much of a command's output may be replaced by a summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompactionScope {
    /// Never compacted, at any size.
    Verbatim,
    /// A shape zirv models: compacted from `[output] compact_min_bytes`.
    Known,
    /// Everything else: compacted only past `compact_generic_min_bytes`, and
    /// the summary says explicitly which lines it omitted.
    Generic,
    /// A unified diff (`git diff`/`show`/`log -p`/`format-patch`, issue
    /// #412): compacted only past `[output] diff_max_bytes`, and never into a
    /// head/tail -- the replacement is a bounded per-file `+N -M` listing
    /// derived from the diff's own grammar (`output_diff::
    /// render_diff_summary`), which never shows a hunk, partial or
    /// otherwise.
    Diff,
}

/// Global `git` flags that consume a SEPARATE value token (`-C dir`,
/// `-c k=v`, `--git-dir x`, `--work-tree x`) rather than folding the value
/// into the flag itself (`--git-dir=x`) or taking no value at all
/// (`--no-pager`, `-p`, `--bare`). Not exhaustive of every global flag git
/// accepts -- just common enough that a subcommand reader must not choke on
/// it.
const GIT_GLOBAL_VALUE_FLAGS: &[&str] = &["-C", "-c", "--git-dir", "--work-tree"];

/// The index in `tokens` of the git SUBCOMMAND -- `tokens[0]` is always
/// `git` itself -- skipping past any leading global flags first (`-C dir`,
/// `-c k=v`, `--git-dir=x`, `--no-pager`, ...).
///
/// Reading `tokens[1]` unconditionally (the bug this replaces) took `-C`/
/// `--no-pager`/etc. itself for the subcommand on `git -C dir diff` and
/// `git --no-pager diff`, so neither ever matched
/// [`VERBATIM_GIT_SUBCOMMANDS`] or [`KNOWN_GIT_SUBCOMMANDS`] and both fell
/// through to [`CompactionScope::Generic`] -- a content-reading `git diff`
/// silently became compactable.
fn git_subcommand_index(tokens: &[&str]) -> usize {
    let mut i = 1usize;
    while let Some(token) = tokens.get(i) {
        if !token.starts_with('-') {
            break;
        }
        i += if GIT_GLOBAL_VALUE_FLAGS.contains(token) {
            2
        } else {
            1
        };
    }
    i
}

/// `pub(crate)`: also reused by `ledger.rs`/`hook::run_posttool` to name the
/// `program` column of one compaction-ledger row (issue #422).
pub(crate) fn bare_program(token: &str) -> String {
    let bare = token.rsplit(['/', '\\']).next().unwrap_or(token);
    bare.to_ascii_lowercase()
        .trim_end_matches(".exe")
        .trim_end_matches(".cmd")
        .trim_end_matches(".bat")
        .to_string()
}

/// Decides how much of `command`'s output may be replaced.
///
/// Verbatim wins over everything, and it is reached by three independent
/// routes, because getting this wrong corrupts an edit rather than merely
/// wasting tokens:
///
/// - any pipe or redirect (`|`, `>`), since the command was already shaped by
///   its author into exactly what they wanted to read;
/// - any executable segment naming a reader (`VERBATIM_PROGRAMS`, a
///   content-reading `git` subcommand, or an operator's own `[output]
///   verbatim` entry) -- segments come from `safety::normalize_segments`, so
///   a reader hidden behind `sh -c`, an env prefix or a launcher is still
///   found;
/// - zirv's own retrieval surface (`zirv ctx output ...`, `zirv ctx run
///   --full`), whose whole purpose is handing back text a summary already
///   elided. Compacting THAT produced a second summary, and no `--range`
///   could ever reach the original.
///
/// `git diff`/`show`/`log -p`/`format-patch` (`DIFF_GIT_SUBCOMMANDS`) are a
/// second, weaker protection (issue #412): also never shown as a head/tail,
/// but -- unlike a true reader -- a unified diff's own grammar is known, so a
/// bounded per-file listing (never a partial hunk) is possible once the
/// output passes `[output] diff_max_bytes`. `blame`/`grep` stay full
/// `Verbatim`, since their output has no such bounded shape.
pub(crate) fn classify_compaction(command: &str, extra_verbatim: &[String]) -> CompactionScope {
    if command.contains('|') || command.contains('>') {
        return CompactionScope::Verbatim;
    }
    let extra: Vec<String> = extra_verbatim
        .iter()
        .map(|name| bare_program(name))
        .collect();
    let mut known = false;
    for segment in super::safety::normalize_segments(command) {
        let collapsed = super::safety::collapse_whitespace(&segment);
        let tokens: Vec<&str> = collapsed.split(' ').filter(|t| !t.is_empty()).collect();
        let Some(first) = tokens.first() else {
            continue;
        };
        let program = bare_program(first);
        let sub_index = if program == "git" {
            git_subcommand_index(&tokens)
        } else {
            1
        };
        let sub = tokens.get(sub_index).map(|t| t.to_ascii_lowercase());
        let sub = sub.as_deref();

        if program == "zirv"
            && sub == Some("ctx")
            && matches!(
                tokens.get(2).map(|t| t.to_ascii_lowercase()).as_deref(),
                Some("output")
            )
        {
            return CompactionScope::Verbatim;
        }
        if program == "zirv"
            && sub == Some("ctx")
            && matches!(
                tokens.get(2).map(|t| t.to_ascii_lowercase()).as_deref(),
                Some("run")
            )
            && tokens.contains(&"--full")
        {
            return CompactionScope::Verbatim;
        }
        if VERBATIM_PROGRAMS.contains(&program.as_str()) || extra.contains(&program) {
            return CompactionScope::Verbatim;
        }
        if program == "git" && sub.is_some_and(|sub| VERBATIM_GIT_SUBCOMMANDS.contains(&sub)) {
            return CompactionScope::Verbatim;
        }
        if program == "git" && sub.is_some_and(|sub| DIFF_GIT_SUBCOMMANDS.contains(&sub)) {
            return CompactionScope::Diff;
        }
        // `git log -p`/`--patch` prints the same unified-diff grammar as
        // `git diff`/`show` -- issue #412 bounds it the same way, rather than
        // leaving it verbatim at any size like every other `git log`.
        if program == "git"
            && sub == Some("log")
            && tokens.iter().any(|t| *t == "-p" || *t == "--patch")
        {
            return CompactionScope::Diff;
        }

        if KNOWN_PROGRAMS.contains(&program.as_str()) {
            // Review finding 5: OR'd across every segment/candidate this
            // loop visits, never assigned outright -- an unrecognised git
            // subcommand in one candidate (or the same command's own
            // "whole" candidate) must never erase a `Known` match an
            // earlier segment already established.
            known |=
                program != "git" || sub.is_some_and(|sub| KNOWN_GIT_SUBCOMMANDS.contains(&sub));
        }
    }
    if known {
        CompactionScope::Known
    } else {
        CompactionScope::Generic
    }
}

/// Parses a `START-END` range. `START-` (and a bare `START`) run to the end
/// of the file. 1-based and inclusive, matching how every tool that prints
/// line numbers spells one.
pub(crate) fn parse_range(raw: &str) -> CtxResult<(usize, usize)> {
    let raw = raw.trim();
    let (start, end) = match raw.split_once('-') {
        Some((start, end)) => (start.trim(), end.trim()),
        None => (raw, ""),
    };
    let start: usize = start
        .parse()
        .map_err(|_| format!("--range: '{raw}' is not a START-END line range"))?;
    if start == 0 {
        return Err("--range: line numbers are 1-based".into());
    }
    let end: usize = if end.is_empty() {
        usize::MAX
    } else {
        end.parse()
            .map_err(|_| format!("--range: '{raw}' is not a START-END line range"))?
    };
    if end < start {
        return Err(format!("--range: '{raw}' ends before it starts").into());
    }
    Ok((start, end))
}

// ---------------------------------------------------------------------
// I/O shell
// ---------------------------------------------------------------------

impl StateDir {
    /// `<state>/outputs/<repo_slug>/` -- the stored verbatim command outputs
    /// (issue #326), one `<id>.log` plus its `<id>.json` sidecar per run,
    /// pruned to [`KEEP_NEWEST_OUTPUTS`] per repository.
    pub fn outputs(&self) -> PathBuf {
        self.root().join("outputs")
    }
}

fn outputs_dir(state: &StateDir, repo: &Path) -> PathBuf {
    state.outputs().join(state::repo_slug(repo))
}

/// Short, unique, filesystem-safe: seconds since the epoch in hex, plus 16
/// bits of within-second nonce. Two runs in the same second on the same
/// machine collide only if the nonce collides too, and the caller retries on
/// an id whose log already exists.
fn mint_id(now: u64, nonce: u64) -> String {
    format!("{now:x}{:04x}", nonce & 0xffff)
}

fn nonce() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()))
        .unwrap_or(0);
    nanos ^ u64::from(std::process::id())
}

/// Drops all but the `keep` newest stored outputs, sidecar included. Not
/// `state::prune_to_newest`: that treats every file in a directory as its own
/// unit, which here would strand a `.json` whose `.log` was pruned (or worse,
/// count the pair as two entries and keep half of `keep`). Best-effort in
/// every direction, like every other retention sweep in this codebase.
pub(crate) fn prune_outputs(dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut logs: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "log") {
                return None;
            }
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, path))
        })
        .collect();
    if logs.len() <= keep {
        return;
    }
    logs.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    for (_, path) in logs.iter().skip(keep) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(path.with_extension("json"));
    }
}

/// Reserves a fresh, unused `<id>.log` under `dir`. The id is short and
/// unique; a collision (two runs in the same second whose nonces also match)
/// simply mints another.
fn reserve_log(dir: &Path, started_at: u64) -> (String, PathBuf) {
    let mut id = mint_id(started_at, nonce());
    let mut path = dir.join(format!("{id}.log"));
    for _ in 0..8 {
        if !path.exists() {
            break;
        }
        id = mint_id(started_at, nonce());
        path = dir.join(format!("{id}.log"));
    }
    (id, path)
}

/// Everything that happens AFTER a stored log exists: the scanning pass(es),
/// the sidecar, retention, and the rendered summary. Shared by
/// `zirv ctx run --compact` (which fills the log by handing a child both of
/// its stream handles) and by claude's `PostToolUse` hook (which fills it
/// with a tool result claude already collected) -- one engine, one store, so
/// the two surfaces can never drift apart, even though `scope` (issue #412)
/// now picks between two summary shapes: the generic scan below for
/// `Known`/`Generic`/`Verbatim` callers, or `output_diff::
/// render_diff_summary`'s bounded per-file listing for `Diff`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn summarize_stored(
    dir: &Path,
    id: &str,
    path: &Path,
    command: &[String],
    exit_code: Option<i32>,
    started_at: u64,
    max_summary_bytes: usize,
    scope: CompactionScope,
) -> CtxResult<(OutputRecord, Option<String>)> {
    if scope == CompactionScope::Diff {
        let diff_scan = super::output_diff::scan_diff_file(path);
        let record = OutputRecord {
            schema_version: OUTPUT_SCHEMA_VERSION,
            id: id.to_string(),
            command: command.to_vec(),
            exit_code,
            started_at,
            lines: diff_scan.total_lines,
            bytes: diff_scan.total_bytes,
        };
        let _ = state::write_private(
            &dir.join(format!("{id}.json")),
            &serde_json::to_string(&record)?,
        );
        prune_outputs(dir, KEEP_NEWEST_OUTPUTS);
        let summary = super::output_diff::render_diff_summary(
            id,
            &command.join(" "),
            exit_code,
            &diff_scan,
            max_summary_bytes,
        );
        return Ok((record, summary));
    }

    // Pass 1 -- the SHARED classifier: failing test names and whether a
    // `test result:`/`Summary [...]` line was seen anywhere in the full
    // stream. Never a second implementation of either; see this module's own
    // doc comment.
    let (tail_bytes, read_errored, failures, summary_seen, _) = match std::fs::File::open(path) {
        Ok(file) => read_capped_tail_and_scan(file, MAX_FAILURE_OUTPUT_BYTES),
        Err(_) => (
            Vec::new(),
            true,
            std::collections::BTreeSet::new(),
            false,
            0,
        ),
    };
    // Pass 2 -- display shaping only: counts, head/tail, diagnostic blocks.
    let mut scan = match std::fs::File::open(path) {
        Ok(file) => scan_for_display(std::io::BufReader::new(file)),
        Err(_) => DisplayScan {
            read_error: true,
            ..DisplayScan::default()
        },
    };
    scan.read_error |= read_errored;

    // Issue #413: a known test family's own structural shape (a pytest
    // `FAILED path::test - message` line, a jest/vitest bullet, a go
    // `--- FAIL:` block) replaces the generic diagnostic blocks above with
    // exact failing-test names and locations, over the SAME bounded tail
    // Pass 1 already retained -- never a third read of a potentially huge
    // log. Declines (leaving `scan.failures` untouched) for anything that is
    // not a confirmed match, so an unrecognised producer or a compile error
    // before any test ran still gets the generic scan's own answer.
    if scope == CompactionScope::Known
        && let Some(family) =
            super::testrun::extract(&command.join(" "), &String::from_utf8_lossy(&tail_bytes))
    {
        scan.failures = family.failure_blocks();
        scan.failures_truncated = family.truncated;
        scan.summaries.push(family.summary_line());
    }

    let record = OutputRecord {
        schema_version: OUTPUT_SCHEMA_VERSION,
        id: id.to_string(),
        command: command.to_vec(),
        exit_code,
        started_at,
        lines: scan.total_lines,
        bytes: scan.total_bytes,
    };
    let _ = state::write_private(
        &dir.join(format!("{id}.json")),
        &serde_json::to_string(&record)?,
    );
    prune_outputs(dir, KEEP_NEWEST_OUTPUTS);

    let summary = render_summary(
        id,
        &command.join(" "),
        exit_code,
        &scan,
        &failures,
        summary_seen,
        max_summary_bytes,
    );
    Ok((record, summary))
}

/// Writes `path` verbatim to `w`, byte for byte. Deliberately not
/// `read_to_string`: the stored file may hold bytes that are not valid UTF-8
/// (a legacy-code-page compiler diagnostic), and a lossy read on this path
/// would hand back something that is no longer the output it names.
fn write_stored_verbatim<W: Write>(w: &mut W, path: &Path) -> CtxResult<()> {
    let mut file = std::fs::File::open(path)
        .map_err(|e| format!("zirv ctx run: could not re-read the stored output: {e}"))?;
    std::io::copy(&mut file, w)
        .map_err(|e| format!("zirv ctx run: could not re-read the stored output: {e}"))?;
    Ok(())
}

/// The last `lines` display lines of the stored file, used when there is no
/// honest summary to print (see [`render_summary`]'s `None`).
fn stored_tail(path: &Path, lines: usize) -> Vec<String> {
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let mut tail: VecDeque<String> = VecDeque::new();
    for chunk in std::io::BufReader::new(file).split(b'\n') {
        let Ok(bytes) = chunk else { break };
        let text = String::from_utf8_lossy(&bytes);
        tail.push_back(display_line(text.strip_suffix('\r').unwrap_or(&text)));
        if tail.len() > lines {
            tail.pop_front();
        }
    }
    tail.into()
}

/// Stores `output` verbatim as a new capture for `repo` and returns its id
/// and rendered summary. The bytes are written exactly as given: the stored
/// file is byte-identical to what the caller was handed, which is the whole
/// contract behind the retrieval line.
pub(crate) fn capture_text(
    state: &StateDir,
    repo: &Path,
    command: &[String],
    exit_code: Option<i32>,
    output: &str,
    max_summary_bytes: usize,
    scope: CompactionScope,
) -> CtxResult<(String, Option<String>)> {
    let dir = outputs_dir(state, repo);
    state::create_private_dir_all(&dir)?;
    let started_at = state::now_secs();
    let (id, path) = reserve_log(&dir, started_at);
    {
        use std::io::Write as _;
        let mut sink = state::open_private_append(&path)?;
        sink.write_all(output.as_bytes())?;
        sink.flush()?;
    }
    let (_, summary) = summarize_stored(
        &dir,
        &id,
        &path,
        command,
        exit_code,
        started_at,
        max_summary_bytes,
        scope,
    )?;
    Ok((id, summary))
}

pub fn run<W: Write>(args: &RunArgs, w: &mut W) -> CtxResult<i32> {
    let env = env_from_process();
    let repo = std::env::current_dir()?;
    run_with(args, w, &repo, &env)
}

pub fn run_with<W: Write>(
    args: &RunArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    let Some(program) = args.command.first() else {
        return Err("zirv ctx run: no command after `--`".into());
    };
    let cfg = CtxConfig::load(repo, env)?;
    let state = StateDir::resolve(env)?;
    let dir = outputs_dir(&state, repo);
    state::create_private_dir_all(&dir)?;

    let started_at = state::now_secs();
    let (id, path) = reserve_log(&dir, started_at);

    let sink = state::open_private_append(&path)?;
    let stdout = sink.try_clone()?;
    let stderr = sink.try_clone()?;
    // One file object behind both streams, so their writes interleave in the
    // order the child actually produced them -- the closest a portable
    // implementation gets to a merged pty without taking on a pty.
    let spawned = std::process::Command::new(program)
        .args(&args.command[1..])
        .current_dir(repo)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(stdout))
        .stderr(std::process::Stdio::from(stderr))
        .spawn();
    drop(sink);

    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => {
            let _ = std::fs::remove_file(&path);
            return Err(format!("zirv ctx run: could not launch '{program}': {e}").into());
        }
    };
    let exit_code = child.wait()?.code().unwrap_or(1);

    // `zirv ctx run --compact` is an explicit ask to compact THIS command's
    // output, unlike the automatic `PostToolUse` interception -- so it always
    // used the generic scan, and issue #412 does not change that here: only
    // `hook::run_posttool`'s own `classify_compaction` call picks `Diff`.
    let (_, summary) = summarize_stored(
        &dir,
        &id,
        &path,
        &args.command,
        Some(exit_code),
        started_at,
        cfg.output.max_summary_bytes,
        CompactionScope::Generic,
    )?;

    if args.full && !args.compact {
        write_stored_verbatim(w, &path)?;
        writeln!(w, "{}", retrieval_line(&id))?;
        return Ok(exit_code);
    }

    match summary {
        Some(summary) => write!(w, "{summary}")?,
        // FAIL OPEN: the mandatory failure content did not fit the cap, so
        // there is no honest summary to print. The raw tail is what an
        // operator would have reached for anyway, and the retrieval line
        // still names the whole thing.
        None => {
            writeln!(
                w,
                "zirv ctx run: exit {exit_code} -- summary omitted (failure detail exceeds \
                 [output] max_summary_bytes); raw tail follows"
            )?;
            for line in stored_tail(&path, TAIL_LINES) {
                writeln!(w, "{line}")?;
            }
            writeln!(w, "{}", retrieval_line(&id))?;
        }
    }
    Ok(exit_code)
}

pub fn run_output<W: Write>(args: &OutputArgs, w: &mut W) -> CtxResult<i32> {
    let env = env_from_process();
    let repo = std::env::current_dir()?;
    run_output_with(args, w, &repo, &env)
}

pub fn run_output_with<W: Write>(
    args: &OutputArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    let cfg = CtxConfig::load(repo, env)?;
    let state = StateDir::resolve(env)?;
    let dir = outputs_dir(&state, repo);
    match &args.command {
        OutputVerb::Show(show) => show_output(show, w, &dir, cfg.search.max_output_bytes),
        OutputVerb::List(list) => list_outputs(list, w, &dir),
    }
}

/// Rejects anything that is not a bare id, so `--` a caller-supplied id can
/// never walk out of this repository's own output directory.
fn log_path_for(dir: &Path, id: &str) -> CtxResult<PathBuf> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(format!("zirv ctx output: '{id}' is not an output id").into());
    }
    Ok(dir.join(format!("{id}.log")))
}

fn show_output<W: Write>(
    args: &ShowArgs,
    w: &mut W,
    dir: &Path,
    max_output_bytes: usize,
) -> CtxResult<i32> {
    let path = log_path_for(dir, &args.id)?;
    let file =
        std::fs::File::open(&path).map_err(|e| format!("zirv ctx output show {}: {e}", args.id))?;
    let (start, end) = match &args.range {
        Some(raw) => parse_range(raw)?,
        None => (1, usize::MAX),
    };

    let record: Option<OutputRecord> =
        std::fs::read_to_string(dir.join(format!("{}.json", args.id)))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok());
    let total = record.as_ref().map(|r| r.lines);
    let header = match total {
        Some(total) => format!("output {} -- lines {start}-{} of {total}\n", args.id, {
            if end == usize::MAX { total } else { end }
        }),
        None => format!("output {} -- from line {start}\n", args.id),
    };
    let budget = max_output_bytes.saturating_sub(header.len());
    let (byte_start, byte_end) = match &args.bytes {
        Some(raw) => {
            let (start, end) = parse_range(raw)?;
            (start - 1, end)
        }
        None => (0, usize::MAX),
    };

    let mut body = String::new();
    let mut next_line: Option<usize> = None;
    let mut next_byte: Option<(usize, usize)> = None;
    // BYTE lines, never `BufRead::lines`: one non-UTF-8 byte from a legacy
    // code page used to end the loop early and render as a confident,
    // header-only success. Every line is kept; only the DISPLAY decoding is
    // lossy, and the stored file itself is never rewritten.
    for (index, chunk) in std::io::BufReader::new(file).split(b'\n').enumerate() {
        let number = index + 1;
        let bytes = chunk.map_err(|e| {
            format!(
                "zirv ctx output show {}: could not read line {number}: {e}",
                args.id
            )
        })?;
        if number < start {
            continue;
        }
        if number > end {
            break;
        }
        // The exact stored line, scrubbed of terminal control sequences the
        // way every other relayed-text surface in this codebase scrubs them,
        // never reflowed or reordered.
        let text = String::from_utf8_lossy(&bytes);
        let line =
            scrub_output(text.strip_suffix('\r').unwrap_or(&text)).replace(['\n', '\t'], " ");
        // A single line can be larger than the whole window on its own, and
        // no `--range` can ever narrow it further -- so the window slides by
        // BYTES within that line instead, and the hint names the next byte
        // offset rather than a line number that would return the same cut.
        let slice = slice_from(&line, byte_start);
        // Review finding 4: `byte_end` used to do nothing but stop the LINE
        // loop after this iteration -- the slice itself was never truncated
        // to the requested window, so `--bytes 5-10` returned everything
        // from byte 5 to the end of the line rather than exactly bytes 5
        // through 10. Truncating here (char-boundary-safe, like every other
        // cut in this function) keeps the existing continuation-hint
        // semantics below: a truncation that still leaves more of the line
        // unread is reported exactly the way a budget-driven mid-line cut
        // already is.
        let slice = if byte_end == usize::MAX {
            slice
        } else {
            let requested = byte_end.saturating_sub(byte_start);
            &slice[..floor_boundary(slice, requested)]
        };
        if slice.len() > budget.saturating_sub(body.len()).max(1) && body.is_empty() {
            let room = budget.max(1);
            let cut = floor_boundary(slice, room);
            body.push_str(&slice[..cut]);
            body.push('\n');
            if byte_start + cut < line.len() {
                next_byte = Some((number, byte_start + cut + 1));
            }
            break;
        }
        let rendered = format!("{slice}\n");
        if !body.is_empty() && body.len() + rendered.len() > budget {
            next_line = Some(number);
            break;
        }
        body.push_str(&rendered);
        if byte_end != usize::MAX {
            break;
        }
    }

    write!(w, "{header}{body}")?;
    if let Some((number, offset)) = next_byte {
        writeln!(
            w,
            "... [cut mid-line at the output cap] next: zirv ctx output show {} --range \
             {number}-{number} --bytes {offset}-",
            args.id
        )?;
    } else if let Some(next) = next_line {
        let end = if end == usize::MAX {
            String::new()
        } else {
            end.to_string()
        };
        writeln!(
            w,
            "... [cut at the output cap] next range: zirv ctx output show {} --range {next}-{end}",
            args.id
        )?;
    }
    Ok(0)
}

/// `line` from byte `start`, snapped forward to a character boundary. An
/// out-of-range start yields the empty string rather than panicking: the
/// offset comes from an operator's own `--bytes`.
fn slice_from(line: &str, start: usize) -> &str {
    if start >= line.len() {
        return "";
    }
    let mut start = start;
    while start < line.len() && !line.is_char_boundary(start) {
        start += 1;
    }
    &line[start..]
}

/// The largest index `<= room` that is a character boundary of `text`.
fn floor_boundary(text: &str, room: usize) -> usize {
    let mut cut = room.min(text.len());
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    cut
}

fn list_outputs<W: Write>(args: &ListArgs, w: &mut W, dir: &Path) -> CtxResult<i32> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        writeln!(w, "no stored outputs for this repository")?;
        return Ok(0);
    };
    let mut records: Vec<(u64, OutputRecord)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                return None;
            }
            let text = std::fs::read_to_string(&path).ok()?;
            let record: OutputRecord = serde_json::from_str(&text).ok()?;
            Some((record.started_at, record))
        })
        .collect();
    if records.is_empty() {
        writeln!(w, "no stored outputs for this repository")?;
        return Ok(0);
    }
    records.sort_by_key(|(started_at, _)| std::cmp::Reverse(*started_at));
    for (_, record) in records.iter().take(args.limit) {
        let exit = match record.exit_code {
            Some(code) => format!("exit {code}"),
            None => "exit -".to_string(),
        };
        writeln!(
            w,
            "{}  {exit:<8} {:>7} lines  {:>9} bytes  {}",
            record.id,
            record.lines,
            record.bytes,
            display_line(&record.command.join(" "))
        )?;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeSet, HashMap};

    fn env_map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    /// The fixture that prints many lines, a diagnostic, a `failures:` block
    /// and a `test result: FAILED` line, then exits 3 -- run through the
    /// platform's own shell so one fixture pair covers both. `zirv ctx run`
    /// itself still executes this argv DIRECTLY; the shell here is the
    /// fixture's interpreter, not zirv's.
    fn noisy_command() -> Vec<String> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        #[cfg(windows)]
        {
            let script = root.join("noisy-output.cmd");
            vec![
                "cmd".to_string(),
                "/D".to_string(),
                "/S".to_string(),
                "/C".to_string(),
                script.display().to_string(),
            ]
        }
        #[cfg(not(windows))]
        {
            let script = root.join("noisy-output.sh");
            vec!["sh".to_string(), script.display().to_string()]
        }
    }

    /// End to end: the full output is persisted verbatim, the summary carries
    /// the exit code and every failure line, and the exit code passes through
    /// unchanged.
    #[test]
    fn a_failing_command_is_captured_verbatim_and_summarized() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).expect("mkdir home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let env = env_map(&[(
            crate::commands::ctx::state::STATE_ENV,
            &state.display().to_string(),
        )]);

        let args = RunArgs {
            compact: true,
            full: false,
            command: noisy_command(),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, &repo, &|k| env.get(k).cloned()).expect("runs");
        let summary = String::from_utf8(out).expect("utf8");

        assert_eq!(
            code, 3,
            "the child's exit code must pass through: {summary}"
        );
        assert!(summary.contains("exit 3"), "{summary}");
        assert!(
            summary.contains("test result: FAILED"),
            "the summary must keep the test-result line: {summary}"
        );
        for name in ["module::tests::alpha", "module::tests::beta"] {
            assert!(
                summary.contains(name),
                "the summary must name every failing test ({name}): {summary}"
            );
        }
        assert!(
            summary.contains("error[E0308]") && summary.contains("--> src/lib.rs:42:9"),
            "the summary must keep each diagnostic and its location: {summary}"
        );
        assert!(
            summary.contains("full output: zirv ctx output show"),
            "{summary}"
        );

        // The stored file is the full output, not the summary.
        let dir = state
            .join("outputs")
            .join(crate::commands::ctx::state::repo_slug(&repo));
        let log = std::fs::read_dir(&dir)
            .expect("outputs dir")
            .flatten()
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|ext| ext == "log"))
            .expect("a stored log");
        let stored = std::fs::read_to_string(&log).expect("read log");
        assert!(stored.contains("filler line 1"), "{stored}");
        assert!(stored.contains("filler line 120"), "{stored}");
        assert!(
            stored.lines().count() > summary.lines().count() * 3,
            "the stored output must be much larger than the summary"
        );
    }

    /// The byte cap is hard, and the retrieval line survives it: a summary
    /// that lost its own id would be lossy compression.
    #[test]
    fn the_summary_respects_the_byte_cap_and_still_carries_the_retrieval_line() {
        let scan = DisplayScan {
            total_lines: 9000,
            total_bytes: 900_000,
            head: (0..HEAD_LINES).map(|i| format!("head {i}")).collect(),
            tail: (0..TAIL_LINES).map(|i| format!("tail {i}")).collect(),
            failures: vec![vec!["error: one thing went wrong".to_string()]],
            warnings: (0..MAX_WARNING_BLOCKS)
                .map(|i| vec![format!("warning: number {i}")])
                .collect(),
            summaries: vec!["test result: FAILED. 1 passed; 2 failed".to_string()],
            warnings_truncated: true,
            ..DisplayScan::default()
        };
        let failures: BTreeSet<String> = ["suite::a".to_string(), "suite::b".to_string()].into();
        let summary = render_summary(
            "abc123",
            "cargo test",
            Some(101),
            &scan,
            &failures,
            true,
            600,
        )
        .expect("the mandatory content fits in 600 bytes");
        assert!(
            summary.len() <= 600,
            "summary was {} bytes: {summary}",
            summary.len()
        );
        assert!(
            summary.ends_with(&format!("{}\n", retrieval_line("abc123"))),
            "the retrieval line must survive the cap: {summary}"
        );
        assert!(
            summary.contains("suite::a") && summary.contains("suite::b"),
            "{summary}"
        );
        assert!(summary.contains("error: one thing went wrong"), "{summary}");
    }

    /// Review finding 3, the headline case: a `warning:` used to flip the
    /// whole output to "structured", which suppressed head and tail, so the
    /// `fatal:` that followed it vanished from the summary entirely.
    #[test]
    fn a_warning_never_hides_a_later_fatal_line() {
        let mut text: String = (1..=200).map(|i| format!("progress {i}\n")).collect();
        text.push_str("warning: this is only a notice\n");
        text.push_str("fatal: bad revision 'nope'\n");
        let scan = scan_for_display(std::io::BufReader::new(text.as_bytes()));
        let summary = render_summary(
            "id1",
            "git rev-parse nope",
            Some(128),
            &scan,
            &BTreeSet::new(),
            false,
            4096,
        )
        .expect("a summary");
        assert!(
            summary.contains("fatal: bad revision 'nope'"),
            "the fatal line is the reason anyone reads this summary: {summary}"
        );
    }

    /// A Rust panic is one BLOCK, not one line: the location header without
    /// the assertion message and its `left:`/`right:` values says nothing
    /// about what actually failed.
    #[test]
    fn a_panic_keeps_its_assertion_message_and_values() {
        let mut text: String = (1..=50)
            .map(|i| format!("test case_{i} ... ok\n"))
            .collect();
        text.push_str("thread 'tests::x' panicked at src/lib.rs:12:5:\n");
        text.push_str("assertion `left == right` failed: totals must agree\n");
        text.push_str("  left: 41\n");
        text.push_str(" right: 42\n");
        text.push('\n');
        text.push_str("test result: FAILED. 50 passed; 1 failed\n");
        let scan = scan_for_display(std::io::BufReader::new(text.as_bytes()));
        let summary = render_summary(
            "id1",
            "cargo test",
            Some(101),
            &scan,
            &BTreeSet::new(),
            true,
            4096,
        )
        .expect("a summary");
        for needle in [
            "panicked at src/lib.rs:12:5",
            "totals must agree",
            "left: 41",
            "right: 42",
        ] {
            assert!(summary.contains(needle), "must keep {needle:?}: {summary}");
        }
    }

    /// Failures and warnings are bounded separately, so a flood of warnings
    /// can never spend the budget an error needs.
    #[test]
    fn sixty_warnings_never_crowd_out_a_later_error() {
        let mut text = String::new();
        for i in 0..60 {
            text.push_str(&format!("warning: unused variable number {i}\n"));
        }
        text.push_str("error[E0308]: mismatched types\n");
        text.push_str("  --> src/late.rs:9:1\n");
        let scan = scan_for_display(std::io::BufReader::new(text.as_bytes()));
        assert_eq!(scan.failures.len(), 1, "the error must be collected");
        let summary = render_summary(
            "id1",
            "cargo build",
            Some(101),
            &scan,
            &BTreeSet::new(),
            false,
            4096,
        )
        .expect("a summary");
        assert!(summary.contains("error[E0308]"), "{summary}");
        assert!(summary.contains("--> src/late.rs:9:1"), "{summary}");
    }

    /// When the MANDATORY failure content cannot fit the cap, there is no
    /// honest summary: the renderer fails open rather than emitting one that
    /// silently dropped failures.
    #[test]
    fn failure_data_that_cannot_fit_the_cap_produces_no_summary_at_all() {
        let scan = DisplayScan {
            total_lines: 400,
            total_bytes: 40_000,
            failures: (0..MAX_FAILURE_BLOCKS)
                .map(|i| {
                    (0..MAX_BLOCK_LINES)
                        .map(|j| format!("error: failure {i} detail line {j} {}", "x".repeat(80)))
                        .collect()
                })
                .collect(),
            ..DisplayScan::default()
        };
        assert_eq!(
            render_summary(
                "id1",
                "cargo build",
                Some(101),
                &scan,
                &BTreeSet::new(),
                false,
                4096
            ),
            None,
            "a summary missing failures must not be emitted at all"
        );
    }

    /// Review finding 3: `tail_at` assumed the tail had actually been
    /// appended to `body` and always subtracted `optional.len()` when
    /// splicing the head in -- so when the tail did NOT fit the budget (and
    /// was therefore never pushed) but the head did, the head landed at
    /// whatever position that wrong subtraction produced, potentially before
    /// the mandatory header itself, rather than after the mandatory block.
    #[test]
    fn a_head_that_fits_lands_after_the_mandatory_block_when_the_tail_does_not() {
        let scan = DisplayScan {
            total_lines: 100,
            total_bytes: 100_000,
            head: (0..5).map(|i| format!("h{i}")).collect(),
            tail: (0..TAIL_LINES).map(|_| "x".repeat(60)).collect(),
            ..DisplayScan::default()
        };
        let retrieval = retrieval_line("id1");
        // Comfortably fits the mandatory header plus the (tiny) head
        // section, but nowhere near enough for the (huge) tail section --
        // chosen with a wide margin so the exact byte counts of the
        // mandatory header never matter.
        let internal_budget = 200usize;
        let max_bytes = retrieval.len() + 1 + internal_budget;
        let summary = render_summary(
            "id1",
            "gen",
            None,
            &scan,
            &BTreeSet::new(),
            false,
            max_bytes,
        )
        .expect("the mandatory content fits");
        assert!(
            summary.starts_with("zirv compacted output:"),
            "the mandatory header must stay first, never get spliced after a head that \
             lands before it: {summary}"
        );
        assert!(
            summary.contains("head (5 of 100):"),
            "the head must still be shown when it fits: {summary}"
        );
        assert!(
            !summary.contains("tail ("),
            "the tail must not appear at all when it does not fit the budget: {summary}"
        );
        let header_pos = summary
            .find("captured 100 lines")
            .expect("the mandatory captured-lines line");
        let head_pos = summary.find("head (5 of 100):").expect("the head section");
        assert!(
            header_pos < head_pos,
            "the head must land AFTER the mandatory block, not spliced inside it: {summary}"
        );
    }

    /// `run --compact` honours that same fail-open by printing the raw tail
    /// and the retrieval line instead of a misleading summary.
    #[test]
    fn run_compact_prints_the_raw_tail_when_no_honest_summary_fits() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        let repo = tmp.path().join("repo");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&repo).expect("mkdir");
        std::fs::create_dir_all(&home).expect("mkdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let env = env_map(&[
            (
                crate::commands::ctx::state::STATE_ENV,
                &state.display().to_string(),
            ),
            ("ZIRV_CTX_OUTPUT_MAX_SUMMARY_BYTES", "512"),
        ]);

        let args = RunArgs {
            compact: true,
            full: false,
            command: noisy_command(),
        };
        let mut out = Vec::new();
        // 512 is the configured floor; the noisy fixture's own failure block
        // plus its failing test names do not fit inside it.
        let code = run_with(&args, &mut out, &repo, &|k| env.get(k).cloned()).expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert_eq!(code, 3);
        assert!(
            text.contains("full output: zirv ctx output show"),
            "the retrieval line survives every path: {text}"
        );
    }

    /// Review finding 5: one non-UTF-8 byte used to end the scan loop, so the
    /// summary confidently reported an empty capture. Every line is kept and
    /// only the DISPLAY decoding is lossy.
    #[test]
    fn a_non_utf8_byte_does_not_truncate_the_scan() {
        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend_from_slice(b"cl: warning C4996: \xff deprecated\n");
        bytes.extend_from_slice(b"error: link failed\n");
        for i in 0..40 {
            bytes.extend_from_slice(format!("step {i}\n").as_bytes());
        }
        let scan = scan_for_display(std::io::BufReader::new(&bytes[..]));
        assert_eq!(scan.total_lines, 42, "every line must be counted");
        assert!(!scan.read_error, "a decode issue is not a read failure");
        assert_eq!(scan.failures.len(), 1, "the error line must still be found");
    }

    /// The same for `output show`: a header-only success with exit 0 hid the
    /// rest of the file.
    #[test]
    fn output_show_reads_past_a_non_utf8_byte() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("outputs");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend_from_slice(b"bad \xff byte\n");
        bytes.extend_from_slice(b"second line\n");
        bytes.extend_from_slice(b"third line\n");
        std::fs::write(dir.join("beef02.log"), &bytes).expect("write");

        let args = ShowArgs {
            id: "beef02".to_string(),
            range: None,
            bytes: None,
        };
        let mut out = Vec::new();
        show_output(&args, &mut out, &dir, 4096).expect("show");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("second line") && text.contains("third line"),
            "{text}"
        );
    }

    /// Review finding 4b: a single line larger than the whole window can
    /// never be narrowed by `--range`, so the window slides by BYTES within
    /// that line and each cut names the next offset to ask for.
    #[test]
    fn output_show_pages_through_one_over_cap_line_by_bytes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("outputs");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let line: String = (0..500)
            .map(|i| char::from(b'a' + (i % 26) as u8))
            .collect();
        std::fs::write(dir.join("beef03.log"), format!("{line}\n")).expect("write");

        let mut out = Vec::new();
        show_output(
            &ShowArgs {
                id: "beef03".to_string(),
                range: Some("1-1".to_string()),
                bytes: None,
            },
            &mut out,
            &dir,
            200,
        )
        .expect("show");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("--bytes"),
            "an over-cap single line must name a byte continuation: {text}"
        );
        let offset: usize = text
            .rsplit("--bytes ")
            .next()
            .and_then(|rest| rest.split('-').next())
            .and_then(|n| n.trim().parse().ok())
            .expect("a next byte offset");

        let mut rest = Vec::new();
        show_output(
            &ShowArgs {
                id: "beef03".to_string(),
                range: Some("1-1".to_string()),
                bytes: Some(format!("{offset}-")),
            },
            &mut rest,
            &dir,
            4096,
        )
        .expect("show");
        let rest = String::from_utf8(rest).expect("utf8");
        assert!(
            rest.contains(&line[offset - 1..]),
            "the continuation must actually reach the rest of the line"
        );
    }

    /// Review finding 4: `--bytes START-END`'s END only ever stopped the LINE
    /// loop -- the selected line's own slice was never truncated to the
    /// requested window, so `--bytes 5-10` returned everything from byte 5 to
    /// the end of the line instead of exactly bytes 5 through 10.
    #[test]
    fn output_show_bytes_end_truncates_to_the_requested_window() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("outputs");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let line: String = (0..50).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
        std::fs::write(dir.join("beef04.log"), format!("{line}\n")).expect("write");

        let mut out = Vec::new();
        show_output(
            &ShowArgs {
                id: "beef04".to_string(),
                range: Some("1-1".to_string()),
                bytes: Some("5-10".to_string()),
            },
            &mut out,
            &dir,
            4096,
        )
        .expect("show");
        let text = String::from_utf8(out).expect("utf8");
        let body_line = text
            .lines()
            .nth(1)
            .expect("the body line, after the header");
        assert_eq!(
            body_line,
            &line[4..10],
            "--bytes 5-10 must return exactly those 6 bytes, not the rest of the line: {text}"
        );
    }

    /// Review finding 6: a reader's output is never compacted, a known
    /// build/test family is compacted early, and anything else only past the
    /// much higher generic threshold.
    #[test]
    fn compaction_scope_protects_readers_and_paces_the_rest() {
        for command in [
            "cat src/lib.rs",
            "sed -n '1,200p' src/lib.rs",
            "rg TODO src",
            "git blame src/lib.rs",
            "cargo test | tail -5",
            "cargo build > out.txt",
            "zirv ctx output show abc123",
            "zirv ctx run --full -- cargo test",
            "sh -c 'cat src/lib.rs'",
        ] {
            assert_eq!(
                classify_compaction(command, &[]),
                CompactionScope::Verbatim,
                "{command} must never be compacted"
            );
        }
        // Issue #412: these leave the verbatim list -- a unified diff's
        // grammar is known, so it gets a bounded per-file listing rather than
        // being either shown verbatim at any size or head/tail scanned.
        // `git grep` stays fully `Verbatim` alongside `git blame` above.
        for command in ["git diff HEAD~1", "git show HEAD", "git log -p"] {
            assert_eq!(
                classify_compaction(command, &[]),
                CompactionScope::Diff,
                "{command} must be bounded, never a head/tail scan"
            );
        }
        for command in [
            "cargo test",
            "cargo build --release",
            "npm install",
            "pytest -q",
            "go test ./...",
            "dotnet build",
            "git fetch origin",
            "git status",
            "make all",
        ] {
            assert_eq!(
                classify_compaction(command, &[]),
                CompactionScope::Known,
                "{command} is a modelled build/test/log family"
            );
        }
        for command in ["some-tool --report", "./bin/generate"] {
            assert_eq!(
                classify_compaction(command, &[]),
                CompactionScope::Generic,
                "{command}"
            );
        }
        // The operator's own list only ever adds.
        assert_eq!(
            classify_compaction("mydump --all", &["mydump".to_string()]),
            CompactionScope::Verbatim
        );
    }

    /// Review finding 2: `classify_compaction` used to read `tokens[1]`
    /// unconditionally as the git subcommand, so a leading global flag
    /// (`-C dir`, `--no-pager`, `-c k=v`) was mistaken for the subcommand
    /// itself and a content-reading `git diff` silently fell through to
    /// `Generic` (compactable).
    #[test]
    fn a_git_global_flag_never_hides_the_subcommand() {
        for command in ["git -C some/dir diff", "git --no-pager diff"] {
            assert_eq!(
                classify_compaction(command, &[]),
                CompactionScope::Diff,
                "{command} must still be recognised as a content-reading git diff"
            );
        }
        assert_eq!(
            classify_compaction("git -c a=b log", &[]),
            CompactionScope::Known,
            "git -c a=b log must still be recognised as a modelled git subcommand"
        );
    }

    /// Review finding 5: `known` used to be overwritten per candidate instead
    /// of OR'd across every candidate `classify_compaction` visits, so a
    /// `Known` match from one segment could be erased by a later candidate's
    /// unrecognised git subcommand.
    #[test]
    fn known_is_or_ed_across_every_segment_not_overwritten() {
        assert_eq!(
            classify_compaction("cargo test && git frobnicate-subcommand", &[]),
            CompactionScope::Known,
            "an unrecognised git subcommand must never erase an earlier Known match"
        );
    }

    /// A generic summary states which lines it dropped, so nothing has to
    /// infer the cut from a missing count.
    #[test]
    fn a_generic_summary_names_the_lines_it_omitted() {
        let text: String = (1..=500).map(|i| format!("plain line {i}\n")).collect();
        let scan = scan_for_display(std::io::BufReader::new(text.as_bytes()));
        let summary = render_summary(
            "id1",
            "some-tool",
            Some(0),
            &scan,
            &BTreeSet::new(),
            false,
            4096,
        )
        .expect("a summary");
        assert!(
            summary.contains("lines omitted between line 21 and line 460"),
            "{summary}"
        );
        assert!(
            summary.contains("zirv ctx output show id1 --range 21-460"),
            "the omitted-range line must be directly actionable: {summary}"
        );
    }

    /// `output show --range` returns exactly the requested slice, in order.
    #[test]
    fn output_show_range_returns_the_exact_slice() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("outputs");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let body: String = (1..=50).map(|i| format!("line {i}\n")).collect();
        std::fs::write(dir.join("deadbeef.log"), &body).expect("write");

        let args = ShowArgs {
            id: "deadbeef".to_string(),
            range: Some("10-13".to_string()),
            bytes: None,
        };
        let mut out = Vec::new();
        show_output(&args, &mut out, &dir, 4096).expect("show");
        let text = String::from_utf8(out).expect("utf8");
        let lines: Vec<&str> = text.lines().skip(1).collect();
        assert_eq!(lines, vec!["line 10", "line 11", "line 12", "line 13"]);
    }

    /// A window past the output cap is cut with a next-range hint rather than
    /// silently short, the same contract `search::render_window_text` holds.
    #[test]
    fn output_show_cut_at_the_cap_names_the_next_range() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("outputs");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let body: String = (1..=200).map(|i| format!("line {i}\n")).collect();
        std::fs::write(dir.join("cafe01.log"), &body).expect("write");

        let args = ShowArgs {
            id: "cafe01".to_string(),
            range: None,
            bytes: None,
        };
        let mut out = Vec::new();
        show_output(&args, &mut out, &dir, 120).expect("show");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("next range: zirv ctx output show cafe01 --range"),
            "{text}"
        );
    }

    /// An id is a bare token or nothing: no separators, no traversal.
    #[test]
    fn output_show_refuses_a_path_shaped_id() {
        let dir = Path::new("/nonexistent");
        for id in ["../secrets", "a/b", "a.b", ""] {
            assert!(
                log_path_for(dir, id).is_err(),
                "'{id}' must not resolve to a path"
            );
        }
        assert!(log_path_for(dir, "abc123").is_ok());
    }

    #[test]
    fn range_parsing_covers_open_ended_and_invalid_forms() {
        assert_eq!(parse_range("3-9").expect("parse"), (3, 9));
        assert_eq!(parse_range("3-").expect("parse"), (3, usize::MAX));
        assert_eq!(parse_range("3").expect("parse"), (3, usize::MAX));
        assert!(parse_range("0-9").is_err());
        assert!(parse_range("9-3").is_err());
        assert!(parse_range("x-9").is_err());
    }

    /// Generic output keeps a full head and tail; a test/compile-shaped one
    /// keeps only the bounded tail, because the lines that matter have
    /// already been lifted out of it -- but it never keeps NOTHING, which is
    /// what let a `fatal:` after a `warning:` disappear.
    #[test]
    fn generic_output_keeps_head_and_tail_and_structured_output_keeps_a_bounded_tail() {
        let text: String = (1..=500).map(|i| format!("plain line {i}\n")).collect();
        let scan = scan_for_display(std::io::BufReader::new(text.as_bytes()));
        assert_eq!(scan.total_lines, 500);
        let generic = render_summary(
            "id1",
            "some-tool",
            Some(0),
            &scan,
            &BTreeSet::new(),
            false,
            4096,
        )
        .expect("a summary");
        assert!(generic.contains("plain line 1\n"), "{generic}");
        assert!(generic.contains("plain line 500"), "{generic}");

        let structured = render_summary(
            "id1",
            "cargo test",
            Some(1),
            &scan,
            &BTreeSet::new(),
            true,
            4096,
        )
        .expect("a summary");
        assert!(
            !structured.contains("plain line 250"),
            "a structured summary must not carry the whole head/tail: {structured}"
        );
        assert!(
            structured.contains("plain line 500"),
            "but it must always keep a bounded tail: {structured}"
        );
    }

    /// One pathological line cannot spend the whole budget.
    #[test]
    fn a_single_enormous_line_is_capped_for_display() {
        let text = format!("{}\n", "x".repeat(100_000));
        let scan = scan_for_display(std::io::BufReader::new(text.as_bytes()));
        assert_eq!(scan.head.len(), 1);
        assert!(scan.head[0].len() <= MAX_LINE_BYTES + 8, "{}", scan.head[0]);
    }

    /// Retention drops a stored log together with its sidecar, never one
    /// without the other.
    #[test]
    fn pruning_removes_a_log_and_its_sidecar_together() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path();
        for i in 0..5 {
            std::fs::write(dir.join(format!("id{i}.log")), "x").expect("write");
            std::fs::write(dir.join(format!("id{i}.json")), "{}").expect("write");
            // Distinct mtimes, so "newest" is well defined on every filesystem.
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        prune_outputs(dir, 2);
        let remaining: Vec<String> = std::fs::read_dir(dir)
            .expect("read")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(remaining.len(), 4, "{remaining:?}");
        for name in &remaining {
            let stem = name.split('.').next().unwrap_or_default();
            assert!(
                remaining.contains(&format!("{stem}.log"))
                    && remaining.contains(&format!("{stem}.json")),
                "a log and its sidecar must be pruned together: {remaining:?}"
            );
        }
    }

    /// A temp home/state/repo triple wired for `capture_text`, mirroring the
    /// setup every `run_with`/`run_post` test above already builds by hand.
    fn capture_rig() -> (
        tempfile::TempDir,
        StateDir,
        PathBuf,
        crate::commands::ctx::testenv::HomeGuard,
    ) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state_path = tmp.path().join("state");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).expect("mkdir home");
        let home_guard = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let env = env_map(&[(
            crate::commands::ctx::state::STATE_ENV,
            &state_path.display().to_string(),
        )]);
        let state = StateDir::resolve(&|k: &str| env.get(k).cloned()).expect("state dir");
        (tmp, state, repo, home_guard)
    }

    // -- Issue #410: never-worse guard ------------------------------------

    /// A small number of long lines mean head AND tail both show every one
    /// of them (no distinct middle to omit), so a head/tail summary of that
    /// shape repeats the whole raw output twice, plus its own header and
    /// retrieval line -- it can never be smaller than what it replaces, so
    /// none is emitted and the original stands.
    #[test]
    fn a_summary_that_would_not_shrink_the_raw_output_is_never_emitted() {
        let (_tmp, state, repo, _home) = capture_rig();

        let threshold = crate::commands::ctx::config::OutputConfig::default().compact_min_bytes;
        let mut output = String::new();
        for i in 0..15u32 {
            output.push_str(&format!("{i:02} {}", "x".repeat(270)));
            output.push('\n');
        }
        match output.len().cmp(&(threshold + 1)) {
            std::cmp::Ordering::Less => output.push_str(&"q".repeat(threshold + 1 - output.len())),
            std::cmp::Ordering::Greater => output.truncate(threshold + 1),
            std::cmp::Ordering::Equal => {}
        }
        assert_eq!(output.len(), threshold + 1, "issue #410's own example size");

        let (_, summary) = capture_text(
            &state,
            &repo,
            &["some-tool".to_string()],
            // Non-zero: keeps this on the ordinary head/tail path regardless
            // of exit code semantics later shaping passes might add.
            Some(1),
            &output,
            // A generous `max_summary_bytes` -- large enough that the head
            // and tail sections both render in full rather than being cut
            // for budget reasons, so what is left to test is specifically
            // whether their (near-total) duplication of the raw content
            // still beats the raw byte count, not whether they fit a small
            // cap.
            20_000,
        )
        .expect("capture");
        assert!(
            summary.is_none(),
            "a summary that cannot beat its own raw output must not be emitted"
        );
    }

    /// A property-style check over several representative shapes (a failing
    /// build, a docker-style pull, a plain generic transcript, a large JSON
    /// array): whenever ANY of them produces a summary at all, it must be
    /// strictly smaller than the raw text it replaces.
    #[test]
    fn every_emitted_summary_is_strictly_smaller_than_its_raw_output() {
        let (_tmp, state, repo, _home) = capture_rig();

        let mut failing_build: String = (1..=400).map(|i| format!("filler line {i}\n")).collect();
        failing_build.push_str("error[E0308]: mismatched types\n  --> src/lib.rs:42:9\n");
        failing_build.push_str("failures:\n\n    module::tests::alpha\n\n");
        failing_build.push_str("test result: FAILED. 1 passed; 1 failed\n");

        let mut docker_pull = String::from("Using default tag: latest\n");
        for i in 0..40u64 {
            docker_pull.push_str(&format!("{:012x}: Pull complete\n", 0xabc000000000u64 + i));
        }
        docker_pull.push_str("Status: Downloaded newer image for alpine:latest\n");

        let generic: String = (1..=600).map(|i| format!("plain line {i}\n")).collect();

        let json_array = serde_json::to_string(&serde_json::Value::Array(
            (0..800)
                .map(|i| serde_json::json!({"id": i, "name": format!("item-{i}")}))
                .collect(),
        ))
        .expect("json");

        for (command, raw, exit_code) in [
            ("cargo test", failing_build, Some(3)),
            ("docker pull alpine", docker_pull, Some(1)),
            ("some-tool --report", generic, Some(1)),
            ("gh api /repos/x/y/issues", json_array, Some(0)),
        ] {
            let (_, summary) =
                capture_text(&state, &repo, &[command.to_string()], exit_code, &raw, 4096)
                    .unwrap_or_else(|e| panic!("{command}: {e}"));
            if let Some(summary) = summary {
                assert!(
                    summary.len() < raw.len(),
                    "{command}: summary ({} bytes) must be smaller than raw ({} bytes): {summary}",
                    summary.len(),
                    raw.len()
                );
            }
        }
    }

    // -- Issue #408: diagnostic grouping -----------------------------------

    /// Forty occurrences of the same clippy warning at different locations
    /// spend one line, not forty.
    #[test]
    fn forty_identical_warnings_group_into_one_line_within_budget() {
        let warnings: Vec<Vec<String>> = (0..40)
            .map(|i| {
                vec![
                    "warning: unused variable: `x`".to_string(),
                    format!("  --> src/file{i}.rs:{i}:5"),
                ]
            })
            .collect();
        let scan = DisplayScan {
            total_lines: 200,
            total_bytes: 200_000,
            warnings,
            ..DisplayScan::default()
        };
        let summary = render_summary(
            "id1",
            "cargo clippy",
            Some(0),
            &scan,
            &BTreeSet::new(),
            false,
            4096,
        )
        .expect("a summary");
        assert!(
            summary.contains("[x 40] warning: unused variable: `x`"),
            "{summary}"
        );
        assert!(summary.len() <= 4096, "{} bytes", summary.len());
    }

    /// Three genuinely distinct errors are never folded into each other.
    #[test]
    fn three_distinct_errors_all_appear_in_the_rendered_summary() {
        let scan = DisplayScan {
            total_lines: 10,
            total_bytes: 1000,
            failures: vec![
                vec![
                    "error[E0308]: mismatched types".to_string(),
                    "  --> a.rs:1:1".to_string(),
                ],
                vec![
                    "error[E0502]: cannot borrow".to_string(),
                    "  --> b.rs:2:2".to_string(),
                ],
                vec!["error: linking failed".to_string()],
            ],
            ..DisplayScan::default()
        };
        let summary = render_summary(
            "id1",
            "cargo build",
            Some(101),
            &scan,
            &BTreeSet::new(),
            false,
            4096,
        )
        .expect("a summary");
        for needle in ["error[E0308]", "error[E0502]", "error: linking failed"] {
            assert!(summary.contains(needle), "{summary}");
        }
        assert!(
            !summary.contains("[x "),
            "distinct errors must never be grouped: {summary}"
        );
    }
}
