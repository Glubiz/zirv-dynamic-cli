//! Issue #422: a durable ledger of `hook::run_posttool`'s own compaction
//! decisions -- one row per `Bash` tool result the compact-output hook
//! looked at, so `zirv ctx savings` (and the one-line summary
//! `status_line` adds to `zirv ctx status`) can answer "how much has
//! compaction actually saved" without re-deriving it from the raw
//! output-capture files `output.rs` owns, which record what was captured
//! but never what the summary against it was.
//!
//! A separate SQLite file (`<state>/ledger.sqlite`), not the shadow-
//! transcript database `transcript_source.rs` already owns: that db is
//! opened read-only by design and can be a harness's own live file, so
//! sharing it would mean either fighting an external writer for the lock or
//! never being able to write at all. [`record`] is fail-open, the same
//! discipline `hook::run_posttool` itself holds for every path in that
//! function: a compaction hook that could not persist its own bookkeeping
//! must still return exactly what it was already going to return, never
//! turn a swallowed sqlite error into a blocked or altered tool result.

use std::collections::BTreeMap;
use std::io::Write;

use super::CtxResult;
use super::config::{CtxConfig, env_from_process};
use super::price;
use super::state::{self, StateDir};

/// `PRAGMA user_version` stamped on a freshly created `ledger.sqlite`.
/// Bumped only when the `compactions` table's own shape changes; there is
/// exactly one schema version today, so [`open`] never migrates, only
/// creates.
const SCHEMA_VERSION: i64 = 1;

/// How long a `compactions` row survives once written -- the same 90-day
/// horizon `log::SAFETY_DECISION_RETENTION_DAYS` uses for its own decision
/// log, chosen so a savings report can always look back a full quarter
/// without the ledger growing forever on a machine that runs zirv for
/// months.
const RETENTION_DAYS: u64 = 90;

/// [`record`] runs the retention `DELETE` on only one write in this many --
/// a per-write gate on the INSERTED ROW ID (review finding F6; a `ts %
/// PRUNE_EVERY == 0` gate re-ran the sweep on every single call inside a
/// burst that shared one matching wall-clock second) rather than a scheduled
/// sweep, so `hook::run_posttool` (a hot path: every large `Bash` result
/// reaches it) pays the cost of a prune scan on exactly one write in 64, not
/// every write whose timestamp happens to land on a multiple of it.
const PRUNE_EVERY: u64 = 64;

/// How `hook::run_posttool` disposed of one `Bash` tool result -- the
/// `outcome` column's own closed vocabulary. String form is always
/// `snake_case` ([`Outcome::as_str`]), matching every other stored
/// string-enum column in this codebase (e.g. `log::TaskClass`'s `kebab-case`
/// sibling convention, just snake rather than kebab because this one is a
/// raw SQL column, not a serde field name).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The result was replaced with a summary. The only outcome where
    /// `bytes_out` differs from `bytes_in`.
    Compacted,
    /// Below `cfg.output.compact_min_bytes`/`compact_generic_min_bytes`.
    BelowThreshold,
    /// A reader command (`output::classify_compaction`'s verbatim scope) --
    /// never compacted regardless of size.
    Verbatim,
    /// Claude's own harness had already spilled the result to a file
    /// (`hook::already_offloaded`).
    Offloaded,
    /// `output::capture_text` failed, or its summary could not carry its own
    /// mandatory failure content within the byte cap -- the result stands
    /// unmodified either way.
    PersistFailed,
    /// A summary was produced but was not smaller than the raw output.
    /// `hook::run_posttool` does not set this itself today; exposed so a
    /// "summary not smaller than raw" check landing elsewhere has a row to
    /// record -- the same "real, tested, no in-tree caller yet" allowance
    /// `transcript_source.rs`'s own module doc comment already documents.
    #[allow(dead_code)]
    NotSmaller,
    /// `cfg.output.compact` is off.
    Disabled,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Compacted => "compacted",
            Self::BelowThreshold => "below_threshold",
            Self::Verbatim => "verbatim",
            Self::Offloaded => "offloaded",
            Self::PersistFailed => "persist_failed",
            Self::NotSmaller => "not_smaller",
            Self::Disabled => "disabled",
        }
    }
}

/// One `compactions` row, as recorded by [`record`]. Borrowed fields, the
/// same shape `log::Decision`/`log::Delegation` already use for a
/// write-only record: nothing here outlives the call that builds it.
pub struct CompactionRow<'a> {
    pub ts: u64,
    pub tool_use_id: &'a str,
    pub session: &'a str,
    pub repo: &'a str,
    pub program: &'a str,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub outcome: Outcome,
    pub retrieval_id: Option<&'a str>,
}

/// Opens (creating if absent) `<state>/ledger.sqlite`, ensuring the
/// `compactions` table and its `ts` index exist. Never migrates: today
/// there is exactly one schema version, stamped via `PRAGMA user_version`
/// only on a fresh file.
///
/// Review finding F7: the pragma/DDL batch and the `user_version` write used
/// to run on EVERY call -- both CREATE statements plus a write to the
/// database header -- for every `Bash` result that reaches `hook::
/// run_posttool`, including every one below its compaction threshold. Now
/// runs only when the file did not already exist before this call, or (a
/// stray zero-byte file, or a previous run that crashed between creating it
/// and running the schema step) `user_version` still reads its
/// freshly-created-file default of `0`. A steady-state call -- the file
/// exists and already carries this module's own `SCHEMA_VERSION` -- opens
/// and returns, leaving the header and `sqlite_master` untouched.
fn open(state: &StateDir) -> rusqlite::Result<rusqlite::Connection> {
    let root = state.root();
    let _ = state::create_private_dir_all(root);
    let db_path = root.join("ledger.sqlite");
    let existed = db_path.exists();
    let conn = rusqlite::Connection::open(&db_path)?;
    let needs_schema = if existed {
        conn.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap_or(0)
            == 0
    } else {
        true
    };
    if needs_schema {
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS compactions (
                 id INTEGER PRIMARY KEY,
                 tool_use_id TEXT,
                 session TEXT,
                 repo TEXT,
                 program TEXT,
                 bytes_in INTEGER,
                 bytes_out INTEGER,
                 outcome TEXT,
                 retrieval_id TEXT,
                 ts INTEGER
             );
             CREATE INDEX IF NOT EXISTS compactions_ts ON compactions (ts);",
        )?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }
    Ok(conn)
}

/// Records one row. Fail-open: any error (an unresolvable state dir, a
/// locked or corrupt database, a write failure) is swallowed and never
/// changes the caller's own behaviour -- see this module's own doc comment
/// for why `hook::run_posttool` requires that.
pub fn record(state: &StateDir, row: &CompactionRow<'_>) {
    let _ = try_record(state, row);
}

fn try_record(state: &StateDir, row: &CompactionRow<'_>) -> rusqlite::Result<()> {
    let conn = open(state)?;
    conn.execute(
        "INSERT INTO compactions
             (tool_use_id, session, repo, program, bytes_in, bytes_out, outcome, retrieval_id, ts)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        rusqlite::params![
            row.tool_use_id,
            row.session,
            row.repo,
            row.program,
            row.bytes_in as i64,
            row.bytes_out as i64,
            row.outcome.as_str(),
            row.retrieval_id,
            row.ts as i64,
        ],
    )?;
    // Retention: only one write in `PRUNE_EVERY` also sweeps rows older than
    // `RETENTION_DAYS` -- see `PRUNE_EVERY`'s own doc comment for why this
    // hot path may not scan+delete on every single insert. Review finding
    // F6: gated on the ROW ID sqlite just assigned, not on `row.ts` -- a
    // burst of calls sharing one wall-clock second (many `Bash` results in
    // the same PostToolUse second) must still run the sweep once per 64
    // WRITES, not once per call whose timestamp happens to be a multiple of
    // it.
    let row_id = conn.last_insert_rowid();
    if row_id > 0 && (row_id as u64).is_multiple_of(PRUNE_EVERY) {
        let cutoff = row.ts.saturating_sub(RETENTION_DAYS * 86_400);
        conn.execute(
            "DELETE FROM compactions WHERE ts < ?1",
            rusqlite::params![cutoff as i64],
        )?;
    }
    Ok(())
}

/// One row as read back for [`totals`]/[`status_line`]. Only the columns
/// either consumer needs.
struct StoredRow {
    bytes_in: u64,
    bytes_out: u64,
    outcome: String,
}

/// Reads every row with `ts >= since_ts`, optionally restricted to `repo`.
/// Best-effort: a missing file, an unresolvable state dir, or any sqlite
/// error reads as "no rows" -- `savings`/`status_line` both treat an empty
/// ledger as a normal, expected state (see their own doc comments), never a
/// hard failure.
fn read_since(state: &StateDir, since_ts: u64, repo: Option<&str>) -> Vec<StoredRow> {
    let Ok(conn) = open(state) else {
        return Vec::new();
    };
    let since_ts = since_ts as i64;
    let result = if let Some(repo) = repo {
        conn.prepare(
            "SELECT bytes_in, bytes_out, outcome FROM compactions
             WHERE ts >= ?1 AND repo = ?2 ORDER BY ts",
        )
        .and_then(|mut stmt| {
            let rows = stmt
                .query_map(rusqlite::params![since_ts, repo], row_from_query)?
                .filter_map(Result::ok)
                .collect();
            Ok(rows)
        })
    } else {
        conn.prepare(
            "SELECT bytes_in, bytes_out, outcome FROM compactions WHERE ts >= ?1 ORDER BY ts",
        )
        .and_then(|mut stmt| {
            let rows = stmt
                .query_map(rusqlite::params![since_ts], row_from_query)?
                .filter_map(Result::ok)
                .collect();
            Ok(rows)
        })
    };
    result.unwrap_or_default()
}

/// Test-only accessor: every stored row's `(outcome, bytes_in, bytes_out)`,
/// oldest first. `hook::run_posttool`'s own tests use this to verify exactly
/// one row lands with the right outcome on each fail-open path (issue
/// #422).
#[cfg(test)]
pub(crate) fn rows_for_test(state: &StateDir) -> Vec<(String, u64, u64)> {
    read_since(state, 0, None)
        .into_iter()
        .map(|r| (r.outcome, r.bytes_in, r.bytes_out))
        .collect()
}

fn row_from_query(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredRow> {
    let bytes_in: i64 = row.get(0)?;
    let bytes_out: i64 = row.get(1)?;
    Ok(StoredRow {
        bytes_in: bytes_in as u64,
        bytes_out: bytes_out as u64,
        outcome: row.get(2)?,
    })
}

/// The aggregate `savings`/`status_line` both render: row count, summed
/// bytes in/out, and a per-outcome row count. `saved_bytes` is always
/// `bytes_in.saturating_sub(bytes_out)` -- non-negative by construction,
/// since every recorded outcome except [`Outcome::Compacted`] stores
/// `bytes_out == bytes_in`.
struct Totals {
    rows: u64,
    bytes_in: u64,
    bytes_out: u64,
    by_outcome: BTreeMap<String, u64>,
}

impl Totals {
    fn saved_bytes(&self) -> u64 {
        self.bytes_in.saturating_sub(self.bytes_out)
    }

    fn saved_pct(&self) -> f64 {
        if self.bytes_in == 0 {
            0.0
        } else {
            (self.saved_bytes() as f64 / self.bytes_in as f64) * 100.0
        }
    }
}

fn totals(rows: &[StoredRow]) -> Totals {
    let mut t = Totals {
        rows: 0,
        bytes_in: 0,
        bytes_out: 0,
        by_outcome: BTreeMap::new(),
    };
    for row in rows {
        t.rows += 1;
        t.bytes_in = t.bytes_in.saturating_add(row.bytes_in);
        t.bytes_out = t.bytes_out.saturating_add(row.bytes_out);
        *t.by_outcome.entry(row.outcome.clone()).or_insert(0) += 1;
    }
    t
}

/// Renders `n` bytes as a short human-readable size (`512 B`, `64.0 KiB`,
/// `1.2 MiB`). Kept local to this module rather than reusing a formatter
/// elsewhere in the codebase (`screen.rs`'s own `human_bytes` makes the same
/// choice, for the same reason): the exact rounding/unit choice here only
/// ever needs to match this module's own two callers.
fn human_bytes(n: u64) -> String {
    const UNITS: [(&str, f64); 4] = [
        ("GiB", 1024.0 * 1024.0 * 1024.0),
        ("MiB", 1024.0 * 1024.0),
        ("KiB", 1024.0),
        ("B", 1.0),
    ];
    let bytes = n as f64;
    for (unit, size) in UNITS {
        if bytes >= size {
            if unit == "B" {
                return format!("{n} B");
            }
            return format!("{:.1} {unit}", bytes / size);
        }
    }
    format!("{n} B")
}

/// The model whose input rate prices a savings report, when the operator
/// has not chosen a chat model explicitly -- the same widely priced default
/// alias `agent.rs`/`handover.rs` already fall back to for an unconfigured
/// claude seat.
const DEFAULT_PRICING_MODEL: &str = "sonnet";

/// Bytes assumed per token when turning a byte count into a token count for
/// pricing -- there is no real tokenizer call on this path, so the dollar
/// figure is explicitly an estimate; every render names both the model and
/// this assumption rather than presenting the number as exact.
const ASSUMED_BYTES_PER_TOKEN: u64 = 4;

/// A dollar estimate for `saved_bytes`, at `model`'s own input rate from
/// `table`, assuming [`ASSUMED_BYTES_PER_TOKEN`] bytes/token -- `None` when
/// `model` has no entry in `table` at all (`price::price`'s own "unknown
/// model -> None, never 0" contract).
fn estimate_saved_usd(saved_bytes: u64, model: &str, table: &price::PriceTable) -> Option<u64> {
    let tokens = saved_bytes / ASSUMED_BYTES_PER_TOKEN;
    let usage = super::event::TranscriptUsage {
        input_tokens: tokens,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
        output_tokens: 0,
    };
    price::price(model, &usage, table)
}

#[derive(Debug, Clone, clap::Args)]
pub struct SavingsArgs {
    /// Restrict to compactions recorded within this window, e.g. `24h`,
    /// `7d`, `30d`, or a bare number of seconds.
    #[arg(long, default_value = "7d")]
    pub since: String,
    /// Restrict to the current repository's own rows.
    #[arg(long, default_value_t = false)]
    pub project: bool,
}

pub fn run<W: Write>(args: &SavingsArgs, w: &mut W) -> CtxResult<i32> {
    let env = env_from_process();
    let state = StateDir::resolve(&env)?;
    let cfg = CtxConfig::load(&std::env::current_dir()?, &env)?;
    let repo = std::env::current_dir()?;
    run_with(&state, &cfg, args, &repo, w, state::now_secs())
}

pub fn run_with<W: Write>(
    state: &StateDir,
    cfg: &CtxConfig,
    args: &SavingsArgs,
    repo: &std::path::Path,
    w: &mut W,
    now: u64,
) -> CtxResult<i32> {
    let since_secs = super::spend::parse_since(&args.since).ok_or_else(|| {
        format!(
            "--since '{}': expected a duration like 30m, 24h, or 7d (or a bare number of seconds)",
            args.since
        )
    })?;
    let since_ts = now.saturating_sub(since_secs);
    let project = if args.project {
        Some(state::repo_slug(repo))
    } else {
        None
    };
    let rows = read_since(state, since_ts, project.as_deref());
    if rows.is_empty() {
        writeln!(
            w,
            "no compaction rows in the ledger for --since {}",
            args.since
        )?;
        return Ok(0);
    }

    let t = totals(&rows);
    let model = cfg.chat.model.as_deref().unwrap_or(DEFAULT_PRICING_MODEL);
    let table = price::resolve_table(cfg);
    let stale = table.is_stale(now, cfg.price.stale_after_days);
    let usd = estimate_saved_usd(t.saved_bytes(), model, &table);

    writeln!(w, "compaction savings, --since {}", args.since)?;
    writeln!(w, "rows:      {}", t.rows)?;
    writeln!(w, "bytes in:  {}", human_bytes(t.bytes_in))?;
    writeln!(w, "bytes out: {}", human_bytes(t.bytes_out))?;
    writeln!(
        w,
        "saved:     {} ({:.1}%)",
        human_bytes(t.saved_bytes()),
        t.saved_pct()
    )?;
    writeln!(w, "by outcome:")?;
    for (outcome, count) in &t.by_outcome {
        writeln!(w, "  {outcome:<15} {count}")?;
    }
    match usd {
        Some(micros) => writeln!(
            w,
            "assuming {model} input pricing at {ASSUMED_BYTES_PER_TOKEN} bytes/token: saved ~{}",
            price::format_usd(micros, stale)
        )?,
        None => writeln!(
            w,
            "assuming {model} input pricing at {ASSUMED_BYTES_PER_TOKEN} bytes/token: \
             {model} has no known price, cost unknown"
        )?,
    }
    Ok(0)
}

/// The `compaction:` line `zirv ctx status` adds when the ledger holds at
/// least one row from the trailing 7 days -- `None` (no line at all,
/// matching `status::orchestrator_blocks_status_line`'s own "silent unless
/// there is something to report" rule) otherwise.
pub fn status_line(state: &StateDir, now: u64) -> Option<String> {
    let since_ts = now.saturating_sub(7 * 86_400);
    let rows = read_since(state, since_ts, None);
    if rows.is_empty() {
        return None;
    }
    let t = totals(&rows);
    Some(format!(
        "compaction: saved {} ({:.0}%) over {} results this week",
        human_bytes(t.saved_bytes()),
        t.saved_pct(),
        t.rows
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row<'a>(
        ts: u64,
        outcome: Outcome,
        bytes_in: u64,
        bytes_out: u64,
        repo: &'a str,
    ) -> CompactionRow<'a> {
        CompactionRow {
            ts,
            tool_use_id: "toolu_1",
            session: "sess-a",
            repo,
            program: "cargo",
            bytes_in,
            bytes_out,
            outcome,
            retrieval_id: None,
        }
    }

    /// A compacted row and a below-threshold row each land as one row apiece
    /// with the right stored outcome and byte columns.
    #[test]
    fn record_writes_one_row_per_call_with_the_right_outcome() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());

        record(&state, &row(1, Outcome::Compacted, 10_000, 500, "repo-a"));
        record(&state, &row(2, Outcome::BelowThreshold, 100, 100, "repo-a"));

        let rows = read_since(&state, 0, None);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].outcome, "compacted");
        assert_eq!(rows[0].bytes_in, 10_000);
        assert_eq!(rows[0].bytes_out, 500);
        assert_eq!(rows[1].outcome, "below_threshold");
        assert_eq!(rows[1].bytes_in, 100);
        assert_eq!(rows[1].bytes_out, 100);
    }

    /// The hot path (fewer than `PRUNE_EVERY` rows inserted so far) never
    /// deletes anything, even rows far outside the retention window -- only
    /// the write that lands on the `PRUNE_EVERY`th INSERTED ROW may run the
    /// sweep at all (review finding F6: gated on the row id, not on any
    /// particular `ts` value).
    #[test]
    fn the_hot_path_never_prunes_outside_its_one_in_prune_every_write() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());

        let ancient_ts = 1;
        record(
            &state,
            &row(ancient_ts, Outcome::Compacted, 1_000, 100, "repo-a"),
        );

        // A dozen more writes -- well under PRUNE_EVERY rows total, so the
        // gate cannot have fired regardless of what these timestamps are.
        for ts in 1_000u64..1_010 {
            record(&state, &row(ts, Outcome::Compacted, 10, 10, "repo-a"));
        }

        let rows = read_since(&state, 0, None);
        assert!(
            rows.len() >= 11,
            "no row should have been pruned on the hot path: {} rows",
            rows.len()
        );
    }

    /// On the `PRUNE_EVERY`th row INSERTED, rows older than `RETENTION_DAYS`
    /// are actually removed.
    #[test]
    fn the_prune_write_removes_rows_older_than_retention() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());

        let far_past = 1_000; // well before the cutoff below
        record(
            &state,
            &row(far_past, Outcome::Compacted, 1_000, 100, "repo-a"),
        ); // row id 1

        let now = far_past + RETENTION_DAYS * 86_400 + 1_000;
        // Enough more rows, all well within retention relative to `now`, to
        // reach the PRUNE_EVERY-th (64th) row inserted -- that write is the
        // only one that may run the sweep.
        for i in 0..(PRUNE_EVERY - 2) {
            record(&state, &row(now + i, Outcome::Compacted, 10, 10, "repo-a"));
        }
        record(&state, &row(now, Outcome::Compacted, 10, 10, "repo-a")); // row id 64

        let rows = read_since(&state, 0, None);
        assert_eq!(rows.len(), (PRUNE_EVERY - 1) as usize);
        assert!(
            rows.iter().all(|r| r.bytes_in == 10),
            "the far-past row must have been pruned; a survivor still carries its 1_000 byte count"
        );
    }

    /// Review finding F6: the prune gate is keyed on the INSERTED ROW ID,
    /// not on the timestamp -- a burst of `record` calls that all share ONE
    /// `ts % PRUNE_EVERY == 0` wall-clock second must not each re-run the
    /// retention sweep. Before the fix, the second call below (row id 2,
    /// `ts` a multiple of `PRUNE_EVERY`) would already have pruned the
    /// seeded old row; after it, nothing runs the sweep before the 64th row.
    #[test]
    fn a_burst_sharing_one_timestamp_prunes_only_on_the_64th_row_not_every_call() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());

        let far_past = 1;
        record(
            &state,
            &row(far_past, Outcome::Compacted, 1_000, 100, "repo-a"),
        ); // row id 1

        // A `now` that is itself a multiple of PRUNE_EVERY -- the exact
        // shape that made the old `ts`-keyed gate re-fire on every one of
        // these calls.
        let now: u64 = 20_000_000 - (20_000_000 % PRUNE_EVERY);
        assert!(now.is_multiple_of(PRUNE_EVERY));
        for _ in 0..(PRUNE_EVERY - 2) {
            record(&state, &row(now, Outcome::Compacted, 10, 10, "repo-a"));
        }
        // 63 rows total (row id 63): the fixed gate must not have fired yet.
        assert_eq!(
            read_since(&state, 0, None).len(),
            (PRUNE_EVERY - 1) as usize,
            "no row should have been pruned before the 64th insert"
        );

        record(&state, &row(now, Outcome::Compacted, 10, 10, "repo-a")); // row id 64
        let rows = read_since(&state, 0, None);
        assert_eq!(
            rows.len(),
            (PRUNE_EVERY - 1) as usize,
            "the 64th insert must prune exactly the seeded old row: {} rows left",
            rows.len()
        );
        assert!(
            rows.iter().all(|r| r.bytes_in == 10),
            "the far-past seeded row (bytes_in = 1_000) must be the one pruned"
        );
    }

    /// Review finding F7: a steady-state `record` call on an EXISTING,
    /// already-schema'd ledger must run no DDL and leave `user_version`
    /// untouched -- only the very first call against a fresh file may create
    /// the table/index and stamp the schema version.
    #[test]
    fn a_steady_state_record_runs_no_ddl_and_leaves_user_version_untouched() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());

        // First call: creates the file and its schema.
        record(&state, &row(1, Outcome::Compacted, 100, 10, "repo-a"));
        let conn = open(&state).expect("open after the first record");
        let user_version_after_first: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .expect("user_version");
        let table_count_after_first: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table'",
                [],
                |r| r.get(0),
            )
            .expect("sqlite_master count");
        assert_eq!(user_version_after_first, SCHEMA_VERSION);
        drop(conn);

        // Second call, against the now-existing file: must be schema-silent.
        record(&state, &row(2, Outcome::BelowThreshold, 50, 50, "repo-a"));
        let conn = open(&state).expect("open after the second record");
        let user_version_after_second: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .expect("user_version");
        let table_count_after_second: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table'",
                [],
                |r| r.get(0),
            )
            .expect("sqlite_master count");
        assert_eq!(
            user_version_after_second, user_version_after_first,
            "a steady-state call must never re-stamp user_version"
        );
        assert_eq!(
            table_count_after_second, table_count_after_first,
            "a steady-state call must never re-run the CREATE TABLE DDL"
        );

        // Both rows still made it in -- the fast path still inserts.
        assert_eq!(read_since(&state, 0, None).len(), 2);
    }

    /// `savings` on an empty ledger prints a clear "nothing yet" line and
    /// exits 0 -- never an error, never a panic on an empty aggregate.
    #[test]
    fn savings_on_an_empty_ledger_prints_a_no_rows_line_and_exits_zero() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let cfg = CtxConfig::default();
        let args = SavingsArgs {
            since: "7d".to_string(),
            project: false,
        };
        let mut out = Vec::new();
        let code = run_with(&state, &cfg, &args, tmp.path(), &mut out, 1_700_000_000)
            .expect("runs on an empty ledger");
        assert_eq!(code, 0);
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("no compaction rows"), "{text}");
    }

    /// `savings` over a small fixture ledger prints totals and a dollar
    /// figure, using the built-in `sonnet` rate when no chat model is
    /// configured.
    #[test]
    fn savings_prints_totals_and_a_dollar_figure_over_a_fixture_ledger() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let now = 1_700_000_000u64;

        record(
            &state,
            &row(now - 100, Outcome::Compacted, 40_000, 4_000, "repo-a"),
        );
        record(
            &state,
            &row(now - 200, Outcome::BelowThreshold, 500, 500, "repo-a"),
        );

        let cfg = CtxConfig::default();
        let args = SavingsArgs {
            since: "7d".to_string(),
            project: false,
        };
        let mut out = Vec::new();
        let code =
            run_with(&state, &cfg, &args, tmp.path(), &mut out, now).expect("runs over rows");
        assert_eq!(code, 0);
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("rows:      2"), "{text}");
        assert!(text.contains("saved:"), "{text}");
        assert!(text.contains("compacted"), "{text}");
        assert!(text.contains("below_threshold"), "{text}");
        assert!(
            text.contains("saved ~$"),
            "must print a dollar figure: {text}"
        );
    }

    /// `--project` restricts the ledger read to the current repo's own
    /// slug; a row recorded under a different repo is excluded.
    #[test]
    fn project_flag_restricts_to_the_current_repo() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let now = 1_700_000_000u64;
        let here = tmp.path().join("here");
        let elsewhere_slug = state::repo_slug(&tmp.path().join("elsewhere"));

        record(
            &state,
            &row(
                now - 10,
                Outcome::Compacted,
                1_000,
                100,
                &state::repo_slug(&here),
            ),
        );
        record(
            &state,
            &row(now - 10, Outcome::Compacted, 1_000, 100, &elsewhere_slug),
        );

        let cfg = CtxConfig::default();
        let args = SavingsArgs {
            since: "7d".to_string(),
            project: true,
        };
        let mut out = Vec::new();
        run_with(&state, &cfg, &args, &here, &mut out, now).expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("rows:      1"), "{text}");
    }

    /// `status_line` is silent on an empty ledger, and names the right
    /// numbers once rows exist within the trailing 7 days.
    #[test]
    fn status_line_is_silent_when_empty_and_reports_over_the_week() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let now = 1_700_000_000u64;

        assert_eq!(status_line(&state, now), None);

        record(
            &state,
            &row(now - 10, Outcome::Compacted, 10_000, 1_000, "repo-a"),
        );
        let line = status_line(&state, now).expect("a line once there are rows");
        assert!(line.starts_with("compaction: saved"), "{line}");
        assert!(line.contains("1 results this week"), "{line}");
    }

    /// A row outside the trailing 7 days does not feed `status_line`, even
    /// though it is still inside the 90-day retention window.
    #[test]
    fn status_line_ignores_rows_older_than_seven_days() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let now = 1_700_000_000u64;
        record(
            &state,
            &row(
                now - 8 * 86_400,
                Outcome::Compacted,
                10_000,
                1_000,
                "repo-a",
            ),
        );
        assert_eq!(status_line(&state, now), None);
    }
}
