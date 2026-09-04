//! Issue #308 stage 1: a post-edit diagnostics channel for the Stop hook.
//!
//! After a turn in which the agent edited files, [`post_edit_nudge`] runs a
//! fast local checker (`cargo check`/`tsc --noEmit`) over the repo, diffs the
//! diagnostics it reports against a per-session baseline, and renders only
//! the NEW ones as one bounded advisory line-block. Off by default
//! (`cfg.diagnostics.enabled`, see [`super::config::DiagnosticsConfig`]) --
//! this spawns a real compiler/type-checker process every qualifying turn,
//! a cost an operator must opt into.
//!
//! Every piece that touches the filesystem or spawns a process
//! (`checker_for`/`run_checker`) is kept separate from the pure parsing/
//! diffing/rendering helpers below it, so the bulk of this module's own
//! logic is tested without ever running a real compiler.
//!
//! Codex's `structural_context` never populates `files_modified` (see that
//! type's own doc comment) -- until it does, `post_edit_nudge` always sees an
//! empty `adapter_files_modified` for a codex session and returns `None`
//! before ever looking for a checker. That makes this feature a permanent,
//! silent no-op under codex today, not a bug to chase.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::config::CtxConfig;
use super::event::input_hash;
use super::state::StateDir;

/// One diagnostic line a checker reported, normalized across `cargo`/`tsc`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub file: String,
    pub line: u64,
    pub level: String,
    pub message: String,
}

impl Diagnostic {
    /// The stable identity a session's baseline keys on -- two diagnostics
    /// with the same file/line/level/message are the same finding even if a
    /// later run's process reordered them.
    pub fn key(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.file, self.line, self.level, self.message
        )
    }
}

/// Which fast checker a repo resolves to. Kept small and `Copy` so a test
/// closure standing in for [`run_checker`] can take it by value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Checker {
    Cargo,
    Tsc,
}

/// `Cargo.toml` at the repo root wins outright (a Rust workspace is checked
/// as a whole, then filtered to the modified files below); otherwise a
/// `tsconfig.json` selects `tsc`, but only when `tsc` genuinely resolves on
/// `PATH` -- a repo that merely vendors a `tsconfig.json` without a
/// TypeScript toolchain installed must never turn into a spawn failure on
/// every qualifying turn. `None` when neither applies -- the feature is
/// simply a no-op for a repo shape this stage does not know how to check.
pub fn checker_for(repo: &Path) -> Option<Checker> {
    if repo.join("Cargo.toml").is_file() {
        return Some(Checker::Cargo);
    }
    if repo.join("tsconfig.json").is_file() && super::adapters::program_is_present("tsc") {
        return Some(Checker::Tsc);
    }
    None
}

/// Spawn `checker`, capture its stdout, and return it -- `None` on a spawn
/// failure or once `timeout_secs` elapses (the child and everything it
/// spawned are killed first). Never panics, never blocks past the timeout:
/// stdout/stderr are drained on background threads while the main thread
/// only polls `try_wait`, so a checker that floods either stream can never
/// deadlock this call the way a naive "wait then read" would.
pub fn run_checker(repo: &Path, checker: Checker, timeout_secs: u64) -> Option<String> {
    let mut command = match checker {
        Checker::Cargo => {
            let mut c = Command::new("cargo");
            c.args(["check", "--message-format=json", "--all-targets"]);
            c
        }
        Checker::Tsc => {
            let mut c = Command::new("tsc");
            c.args(["--noEmit", "--pretty", "false"]);
            c
        }
    };
    command
        .current_dir(repo)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::commands::workflow::isolate_process_tree(&mut command);

    let started = Instant::now();
    let mut child = command.spawn().ok()?;
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let stdout_thread = std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(pipe) = stdout_pipe.as_mut() {
            let _ = pipe.read_to_string(&mut buf);
        }
        buf
    });
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(pipe) = stderr_pipe.as_mut() {
            let _ = pipe.read_to_string(&mut buf);
        }
        buf
    });

    let timeout = Duration::from_secs(timeout_secs);
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => break,
            Ok(None) => {
                if started.elapsed() >= timeout {
                    let _ = crate::commands::workflow::terminate_process_tree(&mut child);
                    let _ = stdout_thread.join();
                    let _ = stderr_thread.join();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return None;
            }
        }
    }
    let stdout = stdout_thread.join().unwrap_or_default();
    let _ = stderr_thread.join();
    Some(stdout)
}

/// `cargo check --message-format=json`: one JSON object per line. Only
/// `"reason":"compiler-message"` lines with `message.level` of `error` or
/// `warning` count; everything else (build-script output, artifact
/// notifications, a line that fails to parse as JSON at all) is silently
/// skipped -- tolerant by construction, since this reads a real compiler's
/// output and must never panic on a shape it did not expect.
pub fn parse_cargo_json(stdout: &str) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if row.get("reason").and_then(Value::as_str) != Some("compiler-message") {
            continue;
        }
        let Some(message) = row.get("message") else {
            continue;
        };
        let level = message.get("level").and_then(Value::as_str).unwrap_or("");
        if level != "error" && level != "warning" {
            continue;
        }
        let Some(spans) = message.get("spans").and_then(Value::as_array) else {
            continue;
        };
        let Some(primary) = spans
            .iter()
            .find(|span| span.get("is_primary").and_then(Value::as_bool) == Some(true))
        else {
            continue;
        };
        let Some(file_name) = primary.get("file_name").and_then(Value::as_str) else {
            continue;
        };
        let line_start = primary
            .get("line_start")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let text = message
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        out.push(Diagnostic {
            file: file_name.to_string(),
            line: line_start,
            level: level.to_string(),
            message: text,
        });
    }
    out
}

/// `tsc --noEmit --pretty false`: one line per diagnostic, shaped
/// `path(line,col): error TSxxxx: message`. A line that does not match (a
/// blank line, a summary line, anything else `tsc` prints) is skipped.
static TSC_LINE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?P<file>.+?)\((?P<line>\d+),\d+\): (?P<level>error|warning) (?:TS\d+: )?(?P<message>.+)$")
        .expect("static tsc diagnostic regex must compile")
});

pub fn parse_tsc(stdout: &str) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some(caps) = TSC_LINE_RE.captures(line) else {
            continue;
        };
        let line_start = caps["line"].parse::<u64>().unwrap_or(0);
        out.push(Diagnostic {
            file: caps["file"].to_string(),
            line: line_start,
            level: caps["level"].to_string(),
            message: caps["message"].to_string(),
        });
    }
    out
}

/// Lexical join, never touches the filesystem: an absolute `path` is
/// returned as-is, a relative one is resolved against `repo`. Both checkers
/// run with `repo` as their working directory, so a normal diagnostic's
/// `file` is already relative to it and this is a no-op join.
fn resolve_against_repo(path: &Path, repo: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo.join(path)
    }
}

/// Separator- and (on Windows only) case-normalized form used for every path
/// comparison in this module -- Windows paths are case-insensitive, and a
/// tool or the agent may spell the same file with either slash style.
fn normalize_path(path: &Path) -> String {
    let normalized = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        normalized.to_lowercase()
    } else {
        normalized
    }
}

/// Whether `path` (resolved against `repo` like every other path in this
/// module) falls under `repo` at all -- the gate `post_edit_nudge` applies to
/// the adapter's `files_modified` before ever looking for a checker.
fn path_under_repo(path: &Path, repo: &Path) -> bool {
    let resolved = normalize_path(&resolve_against_repo(path, repo));
    let repo_norm = normalize_path(repo);
    resolved.starts_with(&repo_norm)
}

/// Keeps only the diagnostics whose file resolves (against `repo`) to one of
/// `modified`'s paths -- a whole-workspace `cargo check` reports far more
/// than the files this turn touched, and only the latter are worth an
/// advisory. The returned diagnostics carry `file` relative to `repo` when
/// the resolved path is under it (the common case, since both checkers run
/// with `repo` as their working directory), so [`render`] never has to know
/// about `repo` itself.
pub fn filter_to_paths(diags: &[Diagnostic], modified: &[PathBuf], repo: &Path) -> Vec<Diagnostic> {
    let modified_norm: Vec<String> = modified
        .iter()
        .map(|path| normalize_path(&resolve_against_repo(path, repo)))
        .collect();
    diags
        .iter()
        .filter_map(|diag| {
            let diag_path = resolve_against_repo(Path::new(&diag.file), repo);
            let diag_norm = normalize_path(&diag_path);
            if !modified_norm.contains(&diag_norm) {
                return None;
            }
            let mut out = diag.clone();
            out.file = match diag_path.strip_prefix(repo) {
                Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
                Err(_) => diag_path.to_string_lossy().replace('\\', "/"),
            };
            Some(out)
        })
        .collect()
}

/// The diagnostics in `current` this session's baseline has never reported
/// before -- order-preserving.
pub fn new_only(baseline: &BTreeSet<String>, current: &[Diagnostic]) -> Vec<Diagnostic> {
    current
        .iter()
        .filter(|diag| !baseline.contains(&diag.key()))
        .cloned()
        .collect()
}

/// One `file:line: message` line per new diagnostic, capped at `cap` with a
/// trailing `(+N more)` line when there are more than that. `None` for an
/// empty slice -- there is nothing for the Stop hook to add.
pub fn render(new: &[Diagnostic], cap: u32) -> Option<String> {
    if new.is_empty() {
        return None;
    }
    let cap = cap as usize;
    let mut lines: Vec<String> = new
        .iter()
        .take(cap)
        .map(|diag| format!("{}:{}: {}", diag.file, diag.line, diag.message))
        .collect();
    if new.len() > cap {
        lines.push(format!("(+{} more)", new.len() - cap));
    }
    Some(lines.join("\n"))
}

/// Bumped whenever [`DiagnosticsRecord`]'s own shape changes -- an
/// independent schema from every other per-session checkpoint this module's
/// sibling `hook.rs` keeps.
const DIAGNOSTICS_RECORD_VERSION: u32 = 1;

/// This session's diagnostics baseline: every key [`Diagnostic::key`] has
/// ever produced for this session, so a finding reported once is never
/// reported again for the life of the session -- even if the underlying
/// compiler keeps re-emitting it turn after turn.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DiagnosticsRecord {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    keys: BTreeSet<String>,
}

fn diagnostics_record_path(state: &StateDir, session: &str) -> PathBuf {
    state
        .scoring()
        .join(format!("{:016x}-diagnostics.json", input_hash(session)))
}

/// Empty (no keys reported yet) on any doubt at all, or a different schema
/// version -- like every other hook state read, never a hook failure.
fn load_diagnostics_record(path: &Path) -> DiagnosticsRecord {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|body| serde_json::from_str::<DiagnosticsRecord>(&body).ok())
        .filter(|record| record.version == DIAGNOSTICS_RECORD_VERSION)
        .unwrap_or_default()
}

/// Best-effort, like `save_verify_on_stop_record`.
fn save_diagnostics_record(path: &Path, record: &DiagnosticsRecord) {
    let Ok(json) = serde_json::to_string(record) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = super::state::create_private_dir_all(dir);
    }
    let _ = super::state::write_private(path, &json);
}

/// Issue #308 stage 1: the Stop-hook advisory itself.
///
/// Strict early-return order, every step of which must fail silently (`None`
/// on the slightest doubt) -- a diagnostics advisory is never worth turning a
/// hook into a failure:
///
/// 1. `cfg.diagnostics.enabled` -- the whole feature is off by default.
/// 2. `session_has_modification` -- this session's transcript must show at
///    least one edit-like tool call (the same incremental, session-scoped
///    fact `verify_on_stop_nudge` gates on).
/// 3. At least one of `adapter_files_modified` resolves under `repo`.
/// 4. `checker_for(repo)` finds a checker at all.
/// 5. `run` (the real [`run_checker`] in production; a closure in tests)
///    returns some stdout.
/// 6. Parsing, filtering to the modified files, and diffing against the
///    session's own baseline leaves at least one new diagnostic.
///
/// `run` is a parameter (rather than this function calling [`run_checker`]
/// directly) purely for testability: a test can hand it a counting closure
/// and assert the checker never ran, without needing a real `cargo`/`tsc` on
/// the test machine.
pub fn post_edit_nudge(
    state: &StateDir,
    cfg: &CtxConfig,
    transcript: &Path,
    repo: &Path,
    session: &str,
    adapter_files_modified: Vec<String>,
    run: &dyn Fn(&Path, Checker, u64) -> Option<String>,
) -> Option<String> {
    if !cfg.diagnostics.enabled {
        return None;
    }
    if !super::hook::session_has_modification(state, transcript, cfg) {
        return None;
    }
    let modified: Vec<PathBuf> = adapter_files_modified
        .into_iter()
        .map(PathBuf::from)
        .filter(|path| path_under_repo(path, repo))
        .collect();
    if modified.is_empty() {
        return None;
    }
    let checker = checker_for(repo)?;
    let stdout = run(repo, checker, cfg.diagnostics.timeout_secs)?;
    let parsed = match checker {
        Checker::Cargo => parse_cargo_json(&stdout),
        Checker::Tsc => parse_tsc(&stdout),
    };
    let filtered = filter_to_paths(&parsed, &modified, repo);
    if filtered.is_empty() {
        return None;
    }

    let path = diagnostics_record_path(state, session);
    let mut record = load_diagnostics_record(&path);
    let new = new_only(&record.keys, &filtered);
    let rendered = render(&new, cfg.diagnostics.max_diagnostics);

    record.version = DIAGNOSTICS_RECORD_VERSION;
    for diag in &filtered {
        record.keys.insert(diag.key());
    }
    save_diagnostics_record(&path, &record);

    rendered.map(|body| format!("zirv ctx: new diagnostics since your last edit:\n{body}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARGO_JSON_FIXTURE: &str = concat!(
        r#"{"reason":"compiler-message","message":{"level":"error","message":"mismatched types","spans":[{"is_primary":true,"file_name":"src/main.rs","line_start":10}]}}"#,
        "\n",
        r#"{"reason":"compiler-message","message":{"level":"error","message":"unresolved import","spans":[{"is_primary":true,"file_name":"src/lib.rs","line_start":3}]}}"#,
        "\n",
        r#"{"reason":"compiler-message","message":{"level":"warning","message":"unused variable","spans":[{"is_primary":true,"file_name":"src/main.rs","line_start":20}]}}"#,
        "\n",
        r#"{"reason":"build-script-executed","message":"irrelevant"}"#,
        "\n",
        "not even json\n",
    );

    #[test]
    fn parse_cargo_json_keeps_only_error_and_warning_compiler_messages() {
        let diags = parse_cargo_json(CARGO_JSON_FIXTURE);
        assert_eq!(diags.len(), 3, "{diags:?}");
        assert!(
            diags
                .iter()
                .any(|d| d.file == "src/main.rs" && d.line == 10)
        );
        assert!(diags.iter().any(|d| d.file == "src/lib.rs" && d.line == 3));
        assert!(diags.iter().any(|d| d.level == "warning"));
    }

    #[test]
    fn parse_tsc_reads_the_path_line_col_shape() {
        let stdout = "src/app.ts(12,5): error TS2322: Type 'string' is not assignable.\n\
             src/app.ts(30,1): warning TS0000: something odd\n\
             Found 2 errors.\n";
        let diags = parse_tsc(stdout);
        assert_eq!(diags.len(), 2, "{diags:?}");
        assert_eq!(diags[0].file, "src/app.ts");
        assert_eq!(diags[0].line, 12);
        assert_eq!(diags[0].level, "error");
        assert!(diags[0].message.contains("not assignable"));
        assert_eq!(diags[1].level, "warning");
    }

    fn diag(file: &str, line: u64, message: &str) -> Diagnostic {
        Diagnostic {
            file: file.to_string(),
            line,
            level: "error".to_string(),
            message: message.to_string(),
        }
    }

    #[test]
    fn new_only_with_an_empty_baseline_returns_everything() {
        let current = vec![diag("a.rs", 1, "x"), diag("b.rs", 2, "y")];
        let baseline = BTreeSet::new();
        assert_eq!(new_only(&baseline, &current), current);
    }

    #[test]
    fn new_only_with_an_unchanged_baseline_returns_nothing() {
        let current = vec![diag("a.rs", 1, "x"), diag("b.rs", 2, "y")];
        let baseline: BTreeSet<String> = current.iter().map(Diagnostic::key).collect();
        assert!(new_only(&baseline, &current).is_empty());
    }

    #[test]
    fn new_only_reports_exactly_the_one_new_diagnostic() {
        let seen = diag("a.rs", 1, "x");
        let fresh = diag("b.rs", 2, "y");
        let baseline: BTreeSet<String> = [seen.key()].into_iter().collect();
        let current = vec![seen, fresh.clone()];
        assert_eq!(new_only(&baseline, &current), vec![fresh]);
    }

    #[test]
    fn render_is_none_for_an_empty_slice() {
        assert_eq!(render(&[], 10), None);
    }

    #[test]
    fn render_caps_at_n_with_a_trailing_more_line() {
        let diags: Vec<Diagnostic> = (0..5).map(|i| diag("a.rs", i, "msg")).collect();
        let text = render(&diags, 3).expect("non-empty");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 4, "{lines:?}");
        assert_eq!(lines[3], "(+2 more)");
        assert!(lines[0].starts_with("a.rs:0:"));
    }

    #[test]
    fn render_under_the_cap_has_no_more_line() {
        let diags = vec![diag("a.rs", 1, "msg")];
        let text = render(&diags, 10).expect("non-empty");
        assert_eq!(text, "a.rs:1: msg");
    }

    #[test]
    fn filter_to_paths_keeps_only_the_modified_files_case_insensitively_on_windows() {
        let repo = tempfile::tempdir().expect("tempdir");
        let diags = vec![
            diag("src/main.rs", 1, "kept"),
            diag("SRC/MAIN.RS", 2, "kept-different-case"),
            diag("src/other.rs", 3, "dropped"),
        ];
        let modified = vec![PathBuf::from("src/main.rs")];
        let filtered = filter_to_paths(&diags, &modified, repo.path());
        if cfg!(windows) {
            assert_eq!(filtered.len(), 2, "{filtered:?}");
        } else {
            assert_eq!(filtered.len(), 1, "{filtered:?}");
        }
        assert!(filtered.iter().all(|d| d.message.starts_with("kept")));
    }

    #[test]
    fn diagnostics_record_round_trips_through_a_tempdir_state_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let path = diagnostics_record_path(&state, "sess-round-trip");

        assert!(
            load_diagnostics_record(&path).keys.is_empty(),
            "no file yet must read as an empty baseline"
        );

        let mut record = DiagnosticsRecord {
            version: DIAGNOSTICS_RECORD_VERSION,
            keys: BTreeSet::new(),
        };
        record.keys.insert(diag("a.rs", 1, "x").key());
        save_diagnostics_record(&path, &record);

        let reloaded = load_diagnostics_record(&path);
        assert_eq!(reloaded.keys, record.keys);
    }
}
