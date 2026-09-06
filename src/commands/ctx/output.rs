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

/// Ceilings on the structured sections, so one pathological producer cannot
/// spend the whole summary budget on a single section before the retrieval
/// line is even reached.
const MAX_DIAGNOSTIC_LINES: usize = 60;
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
    pub(crate) diagnostics: Vec<String>,
    pub(crate) summaries: Vec<String>,
    pub(crate) diagnostics_truncated: bool,
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

/// Whether `line` is a diagnostic worth keeping verbatim: a rustc/clippy
/// `error`/`warning:` header, or a panic message. The `-->` location line
/// that follows a rustc diagnostic is picked up by the caller's own
/// one-line lookahead rather than matched here, so an unrelated `-->` in
/// ordinary output is not promoted on its own.
pub(crate) fn is_diagnostic_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("error:")
        || trimmed.starts_with("error[")
        || trimmed.starts_with("error TS")
        || trimmed.starts_with("warning:")
        || trimmed.contains("panicked at")
}

/// Whether `line` is a test-runner summary line worth keeping verbatim.
/// Matches the two shapes `verification`'s own scanner already recognizes
/// (`cargo test`'s `test result:` and `cargo nextest`'s `Summary [...]`).
pub(crate) fn is_summary_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("test result:") || trimmed.starts_with("Summary [")
}

/// Streams `reader` once, collecting only bounded display material: the
/// counts, the first [`HEAD_LINES`], the last [`TAIL_LINES`], the diagnostic
/// lines (each with the `-->` location line that follows it, when there is
/// one), and the test-runner summary lines.
pub(crate) fn scan_for_display(reader: impl BufRead) -> DisplayScan {
    let mut scan = DisplayScan::default();
    let mut want_location = false;
    for line in reader.lines() {
        let Ok(line) = line else { break };
        scan.total_lines += 1;
        scan.total_bytes = scan.total_bytes.saturating_add(line.len() as u64 + 1);

        if scan.head.len() < HEAD_LINES {
            scan.head.push(display_line(&line));
        }
        scan.tail.push_back(display_line(&line));
        if scan.tail.len() > TAIL_LINES {
            scan.tail.pop_front();
        }

        if is_summary_line(&line) {
            if scan.summaries.len() < MAX_SUMMARY_LINES {
                scan.summaries.push(display_line(&line));
            }
            want_location = false;
            continue;
        }

        let is_location = line.trim_start().starts_with("-->");
        if is_diagnostic_line(&line) || (want_location && is_location) {
            if scan.diagnostics.len() < MAX_DIAGNOSTIC_LINES {
                scan.diagnostics.push(display_line(&line));
            } else {
                scan.diagnostics_truncated = true;
            }
            // Only a real diagnostic header arms the lookahead; consuming a
            // location line disarms it, so a run of `-->` lines cannot walk
            // away with the whole section.
            want_location = !is_location;
            continue;
        }
        want_location = false;
    }
    scan
}

/// The one retrieval line every summary ends with. Kept short and literal:
/// it is meant to be copied verbatim by whatever read the summary.
pub(crate) fn retrieval_line(id: &str) -> String {
    format!("full output: zirv ctx output show {id} [--range START-END]")
}

/// Joins `body` and `retrieval` under a hard `max_bytes` ceiling, truncating
/// the BODY rather than the retrieval line: a summary that lost its retrieval
/// line would be lossy compression, which is the one thing this feature must
/// never be. When `max_bytes` cannot even hold the retrieval line, the
/// retrieval line still wins -- it is the floor, not a participant in the
/// budget.
pub(crate) fn cap_summary(body: &str, retrieval: &str, max_bytes: usize) -> String {
    if max_bytes <= retrieval.len() + 1 {
        return format!("{retrieval}\n");
    }
    let budget = max_bytes - retrieval.len() - 1;
    let mut out = if body.len() <= budget {
        body.to_string()
    } else {
        const MARKER: &str = "... [summary truncated]\n";
        let mut cut = budget.saturating_sub(MARKER.len());
        while cut > 0 && !body.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}{MARKER}", &body[..cut])
    };
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(retrieval);
    out.push('\n');
    out
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

/// Renders the whole summary, given everything both passes recovered.
///
/// Two shapes. When the output looks like a test/compile run -- a summary
/// line was seen, a failing test name was recovered, or a diagnostic was
/// found -- the structured sections carry the signal and the raw head/tail is
/// dropped entirely, because the lines that matter have already been lifted
/// out of it. Otherwise nothing is known about the shape, so the head and
/// tail are the honest answer.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_summary(
    id: &str,
    command: &str,
    exit_code: Option<i32>,
    scan: &DisplayScan,
    failures: &std::collections::BTreeSet<String>,
    summary_seen: bool,
    read_errored: bool,
    max_bytes: usize,
) -> String {
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
    if read_errored {
        body.push_str("note: the stored output ended in a read error; it may be truncated\n");
    }

    let structured = summary_seen || !failures.is_empty() || !scan.diagnostics.is_empty();

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
    push_section(&mut body, "diagnostics:", scan.diagnostics.clone());
    if scan.diagnostics_truncated {
        body.push_str("  ... [more diagnostics in the full output]\n");
    }

    if !structured {
        push_section(
            &mut body,
            &format!("head ({} of {}):", scan.head.len(), scan.total_lines),
            scan.head.clone(),
        );
        push_section(
            &mut body,
            &format!("tail ({} of {}):", scan.tail.len(), scan.total_lines),
            scan.tail.iter().cloned().collect::<Vec<_>>(),
        );
    }

    cap_summary(&body, &retrieval_line(id), max_bytes)
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

/// Everything that happens AFTER a stored log exists: the two scanning
/// passes, the sidecar, retention, and the rendered summary. Shared by
/// `zirv ctx run --compact` (which fills the log by handing a child both of
/// its stream handles) and by claude's `PostToolUse` hook (which fills it
/// with a tool result claude already collected) -- one engine, one store, one
/// summary shape, so the two surfaces can never drift apart.
pub(crate) fn summarize_stored(
    dir: &Path,
    id: &str,
    path: &Path,
    command: &[String],
    exit_code: Option<i32>,
    started_at: u64,
    max_summary_bytes: usize,
) -> CtxResult<(OutputRecord, String)> {
    // Pass 1 -- the SHARED classifier: failing test names and whether a
    // `test result:`/`Summary [...]` line was seen anywhere in the full
    // stream. Never a second implementation of either; see this module's own
    // doc comment.
    let (_, read_errored, failures, summary_seen, _) = match std::fs::File::open(path) {
        Ok(file) => read_capped_tail_and_scan(file, MAX_FAILURE_OUTPUT_BYTES),
        Err(_) => (
            Vec::new(),
            true,
            std::collections::BTreeSet::new(),
            false,
            0,
        ),
    };
    // Pass 2 -- display shaping only: counts, head/tail, diagnostics.
    let scan = match std::fs::File::open(path) {
        Ok(file) => scan_for_display(std::io::BufReader::new(file)),
        Err(_) => DisplayScan::default(),
    };

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
        read_errored,
        max_summary_bytes,
    );
    Ok((record, summary))
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
) -> CtxResult<(String, String)> {
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

    let (_, summary) = summarize_stored(
        &dir,
        &id,
        &path,
        &args.command,
        Some(exit_code),
        started_at,
        cfg.output.max_summary_bytes,
    )?;

    if args.full && !args.compact {
        if let Ok(text) = std::fs::read_to_string(&path) {
            write!(w, "{text}")?;
        }
        writeln!(w, "{}", retrieval_line(&id))?;
        return Ok(exit_code);
    }

    write!(w, "{summary}")?;
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

    let mut body = String::new();
    let mut next: Option<usize> = None;
    for (index, line) in std::io::BufReader::new(file).lines().enumerate() {
        let number = index + 1;
        if number < start {
            continue;
        }
        if number > end {
            break;
        }
        let Ok(line) = line else { break };
        // The exact stored line, scrubbed of terminal control sequences the
        // way every other relayed-text surface in this codebase scrubs them,
        // never reflowed or reordered.
        let rendered = format!("{}\n", scrub_output(&line).replace(['\n', '\t'], " "));
        if !body.is_empty() && body.len() + rendered.len() > budget {
            next = Some(number);
            break;
        }
        body.push_str(&rendered);
    }

    write!(w, "{header}{body}")?;
    if let Some(next) = next {
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
            diagnostics: (0..MAX_DIAGNOSTIC_LINES)
                .map(|i| format!("error: something went wrong number {i}"))
                .collect(),
            summaries: vec!["test result: FAILED. 1 passed; 40 failed".to_string()],
            diagnostics_truncated: true,
        };
        let failures: BTreeSet<String> = (0..40).map(|i| format!("suite::case_{i}")).collect();
        let summary = render_summary(
            "abc123",
            "cargo test",
            Some(101),
            &scan,
            &failures,
            true,
            false,
            600,
        );
        assert!(
            summary.len() <= 600,
            "summary was {} bytes: {summary}",
            summary.len()
        );
        assert!(
            summary.ends_with(&format!("{}\n", retrieval_line("abc123"))),
            "the retrieval line must survive the cap: {summary}"
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

    /// Generic output keeps head and tail; a test/compile-shaped one drops
    /// them in favour of the lines that were actually lifted out of it.
    #[test]
    fn generic_output_keeps_head_and_tail_but_structured_output_does_not() {
        let text: String = (1..=500).map(|i| format!("plain line {i}\n")).collect();
        let scan = scan_for_display(std::io::BufReader::new(text.as_bytes()));
        assert_eq!(scan.total_lines, 500);
        let generic = render_summary(
            "id1",
            "ls",
            Some(0),
            &scan,
            &BTreeSet::new(),
            false,
            false,
            4096,
        );
        assert!(generic.contains("plain line 1\n"), "{generic}");
        assert!(generic.contains("plain line 500"), "{generic}");

        let structured = render_summary(
            "id1",
            "cargo test",
            Some(1),
            &scan,
            &BTreeSet::new(),
            true,
            false,
            4096,
        );
        assert!(
            !structured.contains("plain line 250"),
            "a structured summary must not fall back to raw head/tail: {structured}"
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
}
