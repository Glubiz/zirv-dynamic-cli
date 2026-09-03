//! `zirv ctx spend`: a read-only aggregate view over `delegations.jsonl`
//! (issue #264) -- the answer `log::Delegation`'s own doc comment says the
//! ledger exists to give: "was delegating this cheaper than doing it on the
//! orchestrator seat", now summed by harness, model, task class, or worker
//! instead of read one row at a time.
//!
//! [`aggregate`]/[`totals`]/[`render_table`] are pure -- given the same rows
//! and the same price table, they produce byte-identical output -- so the
//! acceptance test drives them directly against a fixture ledger rather than
//! a real state directory. [`run`]/[`run_with`] are the thin I/O shell:
//! resolve the state dir and price table, read+filter the real ledger, hand
//! off to the pure core.

use std::collections::BTreeMap;
use std::io::Write;

use serde::Serialize;

use super::config::{CtxConfig, env_from_process};
use super::event::TranscriptUsage;
use super::log::DelegationRow;
use super::price::{self, PriceTable};
use super::sessions;
use super::state::{self, StateDir};

/// `zirv ctx spend --json`'s own schema version -- bumped only when the JSON
/// shape itself changes, never on a rendering-only change to the plain-text
/// table. Issue #264's own routing-feedback consumer keys off this.
pub const SPEND_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, clap::Args)]
pub struct SpendArgs {
    /// Restrict to delegations spawned by (or naming) this session's own
    /// short id -- `sessions::short_id`'s vocabulary, the same one `zirv ctx
    /// status`/mail addressing already use.
    #[arg(long)]
    pub session: Option<String>,
    /// Restrict to one work group (`zirv ctx group create`'s own id).
    #[arg(long)]
    pub group: Option<String>,
    /// Restrict to delegations completed within this window, e.g. `24h`,
    /// `30m`, `7d`, or a bare number of seconds.
    #[arg(long)]
    pub since: Option<String>,
    /// Which dimension to group rows by. Defaults to `harness` -- the
    /// dimension the acceptance criteria's own worked example (`zirv ctx
    /// spend --by harness`) uses.
    #[arg(long, value_enum, default_value_t = SpendDimension::Harness)]
    pub by: SpendDimension,
    /// Machine-readable output, schema-versioned (`"schema": 1`).
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum SpendDimension {
    Harness,
    Model,
    TaskClass,
    Worker,
}

/// One aggregated row -- a dimension key plus the summed columns the design's
/// own worked table names: `runs · ok · failed · input · cache-read ·
/// cache-write · output · wall · cost`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SpendRow {
    pub key: String,
    pub runs: u64,
    pub ok: u64,
    pub failed: u64,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    pub wall_ms: u64,
    /// `None` only when EVERY row folded into this key priced as unknown
    /// (see `price::price`'s own "unknown model -> None, never 0" contract).
    /// A row group with a mix of known and unknown models still sums the
    /// known ones -- `unpriced_runs` below is what flags the gap, not a
    /// silently wrong total.
    pub cost_micros: Option<u64>,
    /// How many of `runs` had no price at all (an unrecognised model) and so
    /// contributed nothing to `cost_micros` -- never silently folded into a
    /// total that would then look complete but is not.
    pub unpriced_runs: u64,
}

impl SpendRow {
    fn empty(key: String) -> Self {
        Self {
            key,
            runs: 0,
            ok: 0,
            failed: 0,
            input_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            output_tokens: 0,
            wall_ms: 0,
            cost_micros: None,
            unpriced_runs: 0,
        }
    }

    fn fold_in(&mut self, row: &DelegationRow, table: &PriceTable) {
        self.runs += 1;
        if row.outcome == "ok" {
            self.ok += 1;
        } else {
            self.failed += 1;
        }
        self.input_tokens = self.input_tokens.saturating_add(row.input_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(row.cache_read_input_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(row.cache_creation_input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(row.output_tokens);
        self.wall_ms = self.wall_ms.saturating_add(row.wall_ms);

        match row
            .model
            .as_deref()
            .and_then(|model| price::price(model, &usage_of(row), table))
        {
            Some(cost) => {
                self.cost_micros = Some(self.cost_micros.unwrap_or(0).saturating_add(cost));
            }
            None => self.unpriced_runs += 1,
        }
    }
}

/// The raw four token classes `price::price` needs, read off one
/// [`DelegationRow`] -- the same shape `TranscriptUsage` already is.
fn usage_of(row: &DelegationRow) -> TranscriptUsage {
    TranscriptUsage {
        input_tokens: row.input_tokens,
        cache_creation_input_tokens: row.cache_creation_input_tokens,
        cache_read_input_tokens: row.cache_read_input_tokens,
        output_tokens: row.output_tokens,
    }
}

/// This row's own key under `dimension` -- `"(unknown)"`/`"(none)"` for a row
/// with nothing to report on that axis, never an empty string (which would
/// sort first and render as a blank, indistinguishable line).
fn dimension_key(row: &DelegationRow, dimension: SpendDimension) -> String {
    match dimension {
        SpendDimension::Harness => row.agent.clone(),
        SpendDimension::Model => row.model.clone().unwrap_or_else(|| "(unknown)".to_string()),
        SpendDimension::TaskClass => row
            .task_class
            .map(|t| t.to_string())
            .unwrap_or_else(|| "(none)".to_string()),
        SpendDimension::Worker => sessions::short_id(&row.session),
    }
}

/// Groups `rows` by `dimension`, pricing each with `table`. Pure: no fs,
/// clock, or env. Ordering is NOT decided here -- callers needing a stable
/// order (every real caller does) sort the result with
/// [`sort_deterministically`].
pub fn aggregate(
    rows: &[DelegationRow],
    dimension: SpendDimension,
    table: &PriceTable,
) -> Vec<SpendRow> {
    let mut by_key: BTreeMap<String, SpendRow> = BTreeMap::new();
    for row in rows {
        let key = dimension_key(row, dimension);
        by_key
            .entry(key.clone())
            .or_insert_with(|| SpendRow::empty(key))
            .fold_in(row, table);
    }
    by_key.into_values().collect()
}

/// Cost desc, then name asc -- the deterministic order the acceptance
/// criteria's own worked example and every render below rely on. A `None`
/// cost (every contributing row unpriced) sorts as if it were zero, last
/// among equals, never randomly interleaved by map iteration order.
pub fn sort_deterministically(rows: &mut [SpendRow]) {
    rows.sort_by(|a, b| {
        b.cost_micros
            .unwrap_or(0)
            .cmp(&a.cost_micros.unwrap_or(0))
            .then_with(|| a.key.cmp(&b.key))
    });
}

/// The grand total across every row -- the table's own totals line.
pub fn totals(rows: &[SpendRow]) -> SpendRow {
    let mut total = SpendRow::empty("total".to_string());
    for row in rows {
        total.runs += row.runs;
        total.ok += row.ok;
        total.failed += row.failed;
        total.input_tokens = total.input_tokens.saturating_add(row.input_tokens);
        total.cache_read_tokens = total
            .cache_read_tokens
            .saturating_add(row.cache_read_tokens);
        total.cache_write_tokens = total
            .cache_write_tokens
            .saturating_add(row.cache_write_tokens);
        total.output_tokens = total.output_tokens.saturating_add(row.output_tokens);
        total.wall_ms = total.wall_ms.saturating_add(row.wall_ms);
        total.unpriced_runs += row.unpriced_runs;
        if let Some(cost) = row.cost_micros {
            total.cost_micros = Some(total.cost_micros.unwrap_or(0).saturating_add(cost));
        }
    }
    total
}

fn format_cost(micros: Option<u64>, stale: bool) -> String {
    match micros {
        Some(m) => price::format_usd(m, stale),
        None => "n/a".to_string(),
    }
}

/// `<m>m<s>s` -- the same wall-clock shape `workflow::engine::format_wall_
/// clock` already uses for a workflow step's own duration.
fn format_wall(ms: u64) -> String {
    let total_secs = ms / 1000;
    format!("{}m{}s", total_secs / 60, total_secs % 60)
}

/// Renders `rows` (already sorted) plus `total` as the plain-text table the
/// design's own columns name: `runs · ok · failed · input · cache-read ·
/// cache-write · output · wall · cost`. `stale`/`as_of` come from the same
/// price table every row was priced against.
pub fn render_table(rows: &[SpendRow], total: &SpendRow, stale: bool, as_of: &str) -> String {
    let mut out = String::new();
    out.push_str("key            runs   ok  failed      input  cache-read cache-write     output    wall       cost\n");
    for row in rows {
        out.push_str(&render_row(row, stale));
        out.push('\n');
    }
    out.push_str(&render_row(total, stale));
    out.push('\n');
    if stale {
        out.push_str(&format!(
            "prices as of {as_of} -- stale, treat costs as approximate\n"
        ));
    }
    if total.unpriced_runs > 0 {
        out.push_str(&format!(
            "{} run(s) used a model with no known price and contributed $0.00 to cost\n",
            total.unpriced_runs
        ));
    }
    out
}

fn render_row(row: &SpendRow, stale: bool) -> String {
    format!(
        "{:<14} {:>4} {:>4} {:>7} {:>10} {:>11} {:>11} {:>10} {:>7} {:>10}",
        row.key,
        row.runs,
        row.ok,
        row.failed,
        row.input_tokens,
        row.cache_read_tokens,
        row.cache_write_tokens,
        row.output_tokens,
        format_wall(row.wall_ms),
        format_cost(row.cost_micros, stale),
    )
}

#[derive(Debug, Serialize)]
struct SpendJson<'a> {
    schema: u32,
    stale: bool,
    price_as_of: &'a str,
    rows: &'a [SpendRow],
    total: &'a SpendRow,
}

fn render_json(rows: &[SpendRow], total: &SpendRow, stale: bool, as_of: &str) -> String {
    let body = SpendJson {
        schema: SPEND_SCHEMA_VERSION,
        stale,
        price_as_of: as_of,
        rows,
        total,
    };
    serde_json::to_string_pretty(&body).unwrap_or_else(|_| "{}".to_string())
}

/// Parses `--since`: a bare number of seconds, or a number suffixed `s`/`m`/
/// `h`/`d`. `None` for anything that does not parse -- the caller treats an
/// unparseable `--since` as "no time filter" rather than failing the whole
/// command over a typo'd duration.
fn parse_since(text: &str) -> Option<u64> {
    let text = text.trim();
    let (digits, multiplier) = match text.strip_suffix('s') {
        Some(d) => (d, 1u64),
        None => match text.strip_suffix('m') {
            Some(d) => (d, 60),
            None => match text.strip_suffix('h') {
                Some(d) => (d, 3_600),
                None => match text.strip_suffix('d') {
                    Some(d) => (d, 86_400),
                    None => (text, 1),
                },
            },
        },
    };
    digits.parse::<u64>().ok().map(|n| n * multiplier)
}

/// Whether `row` survives every filter `args` names -- every unset filter
/// passes everything, so no filters at all means "the whole ledger".
fn matches_filters(row: &DelegationRow, args: &SpendArgs, now: u64) -> bool {
    if let Some(session) = &args.session
        && row.parent_session != *session
        && sessions::short_id(&row.session) != *session
    {
        return false;
    }
    if let Some(group) = &args.group
        && row.work_group_id.as_deref() != Some(group.as_str())
    {
        return false;
    }
    if let Some(since) = &args.since
        && let Some(secs) = parse_since(since)
        && row.ts < now.saturating_sub(secs)
    {
        return false;
    }
    true
}

pub fn run<W: Write>(args: &SpendArgs, w: &mut W) -> super::CtxResult<i32> {
    let env = env_from_process();
    let state = StateDir::resolve(&env)?;
    let repo = std::env::current_dir()?;
    let cfg = CtxConfig::load(&repo, &env)?;
    run_with(&state, &cfg, args, w, state::now_secs())
}

pub fn run_with<W: Write>(
    state: &StateDir,
    cfg: &CtxConfig,
    args: &SpendArgs,
    w: &mut W,
    now: u64,
) -> super::CtxResult<i32> {
    // No cap: `zirv ctx spend` reads the whole ledger, unlike `status`'s own
    // bounded tail -- an aggregate over only the newest N rows would silently
    // under-report an operator's own total spend.
    let rows: Vec<DelegationRow> = super::log::read_delegations(state, usize::MAX)
        .into_iter()
        .filter(|row| matches_filters(row, args, now))
        .collect();

    let table = price::resolve_table(cfg);
    let stale = table.is_stale(now, cfg.price.stale_after_days);

    let mut aggregated = aggregate(&rows, args.by, &table);
    sort_deterministically(&mut aggregated);
    let total = totals(&aggregated);

    if args.json {
        writeln!(
            w,
            "{}",
            render_json(&aggregated, &total, stale, &table.as_of)
        )?;
    } else {
        write!(
            w,
            "{}",
            render_table(&aggregated, &total, stale, &table.as_of)
        )?;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::log::TaskClass;
    use crate::commands::ctx::permit::WorkerMode;

    #[allow(clippy::too_many_arguments)]
    fn row(
        session: &str,
        agent: &str,
        model: Option<&str>,
        input: u64,
        cache_read: u64,
        output: u64,
        outcome: &str,
        task_class: Option<TaskClass>,
        ts: u64,
    ) -> DelegationRow {
        DelegationRow {
            ts,
            session: session.to_string(),
            parent_session: "sess-parent".to_string(),
            work_group_id: None,
            agent: agent.to_string(),
            model: model.map(str::to_string),
            input_tokens: input,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: cache_read,
            output_tokens: output,
            wall_ms: 1_000,
            exit_code: if outcome == "ok" { 0 } else { 1 },
            outcome: outcome.to_string(),
            mode: Some(WorkerMode::Writing),
            task_class,
        }
    }

    /// Loads the fixture ledger, then hand-verifies `--by harness`'s totals
    /// against the exact arithmetic the fixture's own header comment lays
    /// out. This is the acceptance test: deterministic totals over a fixture,
    /// matching a hand-computed expectation.
    #[test]
    fn spend_by_harness_over_the_fixture_ledger_matches_hand_computed_totals() {
        let text = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests")
                .join("fixtures")
                .join("delegations-spend.jsonl"),
        )
        .expect("read fixture");
        let rows: Vec<DelegationRow> = text
            .lines()
            .map(|line| serde_json::from_str(line).expect("parse fixture row"))
            .collect();
        assert_eq!(rows.len(), 5, "the fixture's own row count");

        let table = price::built_in_table();
        let mut aggregated = aggregate(&rows, SpendDimension::Harness, &table);
        sort_deterministically(&mut aggregated);

        assert_eq!(aggregated.len(), 2, "claude and codex");
        // claude: rows 1, 2, 5 -- $1.32 + $0.48 + $0 (unpriced) = $1.80.
        let claude = &aggregated[0];
        assert_eq!(claude.key, "claude", "claude's $1.80 beats codex's $1.7775");
        assert_eq!(claude.runs, 3);
        assert_eq!(claude.ok, 2);
        assert_eq!(claude.failed, 1);
        assert_eq!(claude.input_tokens, 151_000);
        assert_eq!(claude.cache_read_tokens, 1_000_000);
        assert_eq!(claude.output_tokens, 70_500);
        assert_eq!(claude.wall_ms, 15_500);
        assert_eq!(claude.cost_micros, Some(1_800_000), "$1.80 exactly");
        assert_eq!(claude.unpriced_runs, 1, "the unknown-model-x row");

        // codex: rows 3, 4 -- $1.70 + $0.0775 = $1.7775.
        let codex = &aggregated[1];
        assert_eq!(codex.key, "codex");
        assert_eq!(codex.runs, 2);
        assert_eq!(codex.ok, 2);
        assert_eq!(codex.failed, 0);
        assert_eq!(codex.input_tokens, 210_000);
        assert_eq!(codex.cache_read_tokens, 810_000);
        assert_eq!(codex.output_tokens, 105_000);
        assert_eq!(codex.wall_ms, 21_000);
        assert_eq!(codex.cost_micros, Some(1_777_500), "$1.7775 exactly");
        assert_eq!(codex.unpriced_runs, 0);

        let total = totals(&aggregated);
        assert_eq!(total.runs, 5);
        assert_eq!(total.ok, 4);
        assert_eq!(total.failed, 1);
        assert_eq!(total.input_tokens, 361_000);
        assert_eq!(total.cache_read_tokens, 1_810_000);
        assert_eq!(total.output_tokens, 175_500);
        assert_eq!(total.wall_ms, 36_500);
        assert_eq!(total.cost_micros, Some(3_577_500), "$3.5775 total");
        assert_eq!(total.unpriced_runs, 1);
    }

    /// `--by model`/`--by task-class`/`--by worker` each key rows on a
    /// different field -- a smoke test that every dimension actually
    /// produces distinct groupings, not just `harness` wired correctly.
    #[test]
    fn every_dimension_groups_by_its_own_field() {
        let rows = vec![
            row(
                "sess-a",
                "claude",
                Some("sonnet"),
                1_000_000,
                0,
                0,
                "ok",
                Some(TaskClass::Review),
                1,
            ),
            row(
                "sess-b",
                "claude",
                Some("opus"),
                1_000_000,
                0,
                0,
                "ok",
                Some(TaskClass::Test),
                2,
            ),
        ];
        let table = price::built_in_table();

        let by_harness = aggregate(&rows, SpendDimension::Harness, &table);
        assert_eq!(by_harness.len(), 1, "both rows share the same harness");

        let by_model = aggregate(&rows, SpendDimension::Model, &table);
        assert_eq!(by_model.len(), 2, "sonnet and opus are distinct models");

        let by_task_class = aggregate(&rows, SpendDimension::TaskClass, &table);
        assert_eq!(by_task_class.len(), 2, "review and test are distinct");

        let by_worker = aggregate(&rows, SpendDimension::Worker, &table);
        assert_eq!(by_worker.len(), 2, "sess-a and sess-b are distinct workers");
    }

    /// An unrecognised model contributes to `runs`/tokens but never a phantom
    /// non-zero cost -- `price::price`'s own "None, never 0" contract must
    /// survive aggregation, not get silently defaulted to zero-and-hidden.
    #[test]
    fn an_unpriced_model_is_flagged_not_silently_zeroed() {
        let rows = vec![row(
            "sess-a",
            "claude",
            Some("not-a-real-model"),
            1_000,
            0,
            0,
            "ok",
            None,
            1,
        )];
        let table = price::built_in_table();
        let aggregated = aggregate(&rows, SpendDimension::Harness, &table);
        assert_eq!(aggregated.len(), 1);
        assert_eq!(aggregated[0].cost_micros, None);
        assert_eq!(aggregated[0].unpriced_runs, 1);
    }

    /// A row with no model at all groups under `(unknown)` for `--by model`,
    /// never an empty-string key.
    #[test]
    fn a_row_with_no_model_groups_under_the_unknown_placeholder() {
        let rows = vec![row("sess-a", "claude", None, 1_000, 0, 0, "ok", None, 1)];
        let table = price::built_in_table();
        let aggregated = aggregate(&rows, SpendDimension::Model, &table);
        assert_eq!(aggregated.len(), 1);
        assert_eq!(aggregated[0].key, "(unknown)");
    }

    /// Deterministic ordering's second clause: equal cost breaks the tie by
    /// name, ascending -- never by map iteration order, which would vary
    /// across runs/platforms.
    #[test]
    fn equal_cost_groups_sort_by_name_ascending() {
        let mut rows = vec![
            SpendRow {
                cost_micros: Some(500),
                ..SpendRow::empty("zebra".to_string())
            },
            SpendRow {
                cost_micros: Some(500),
                ..SpendRow::empty("alpha".to_string())
            },
        ];
        sort_deterministically(&mut rows);
        assert_eq!(rows[0].key, "alpha");
        assert_eq!(rows[1].key, "zebra");
    }

    /// `--since` filters by wall-clock age; a bare number of seconds and
    /// every documented suffix all parse.
    #[test]
    fn parse_since_accepts_every_documented_suffix() {
        assert_eq!(parse_since("90"), Some(90));
        assert_eq!(parse_since("90s"), Some(90));
        assert_eq!(parse_since("5m"), Some(300));
        assert_eq!(parse_since("24h"), Some(86_400));
        assert_eq!(parse_since("7d"), Some(604_800));
        assert_eq!(parse_since("not-a-duration"), None);
    }

    /// `--since` actually narrows the ledger: a row older than the window is
    /// excluded, one inside it is kept.
    #[test]
    fn since_filters_out_rows_older_than_the_window() {
        let rows = [
            row("old", "claude", Some("sonnet"), 1, 0, 0, "ok", None, 1_000),
            row("new", "claude", Some("sonnet"), 1, 0, 0, "ok", None, 99_000),
        ];
        let now = 100_000;
        let args = SpendArgs {
            session: None,
            group: None,
            since: Some("2000s".to_string()),
            by: SpendDimension::Worker,
            json: false,
        };
        let kept: Vec<_> = rows
            .iter()
            .filter(|r| matches_filters(r, &args, now))
            .collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].session, "new");
    }

    /// `--group` narrows to exactly one work group's own rows.
    #[test]
    fn group_filter_keeps_only_matching_rows() {
        let mut a = row("a", "claude", Some("sonnet"), 1, 0, 0, "ok", None, 1);
        a.work_group_id = Some("wg-1".to_string());
        let mut b = row("b", "claude", Some("sonnet"), 1, 0, 0, "ok", None, 1);
        b.work_group_id = Some("wg-2".to_string());
        let rows = [a, b];
        let args = SpendArgs {
            session: None,
            group: Some("wg-1".to_string()),
            since: None,
            by: SpendDimension::Worker,
            json: false,
        };
        let kept: Vec<_> = rows
            .iter()
            .filter(|r| matches_filters(r, &args, 0))
            .collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].session, "a");
    }

    /// A stale table prefixes every rendered cost with `~` and names its own
    /// `as_of` -- the render-path half of the stale-price acceptance
    /// criterion (the pricing half lives in `price.rs`'s own tests).
    #[test]
    fn a_stale_table_flags_every_rendered_cost() {
        let rows = vec![SpendRow {
            cost_micros: Some(1_000_000),
            runs: 1,
            ok: 1,
            ..SpendRow::empty("claude".to_string())
        }];
        let total = totals(&rows);
        let text = render_table(&rows, &total, true, "2020-01-01");
        assert!(text.contains("~$1.00"), "got {text}");
        assert!(text.contains("2020-01-01"), "got {text}");

        let fresh = render_table(&rows, &total, false, "2020-01-01");
        assert!(!fresh.contains('~'), "got {fresh}");
    }

    /// `--json` carries the schema version and every column the plain-text
    /// table does, machine-readably.
    #[test]
    fn json_output_carries_the_schema_version_and_every_column() {
        let rows = vec![SpendRow {
            cost_micros: Some(1_000_000),
            runs: 1,
            ok: 1,
            ..SpendRow::empty("claude".to_string())
        }];
        let total = totals(&rows);
        let text = render_json(&rows, &total, false, "2026-09-01");
        let value: serde_json::Value = serde_json::from_str(&text).expect("valid json");
        assert_eq!(value["schema"], 1);
        assert_eq!(value["stale"], false);
        assert_eq!(value["price_as_of"], "2026-09-01");
        assert_eq!(value["rows"][0]["key"], "claude");
        assert_eq!(value["total"]["runs"], 1);
    }

    /// End-to-end through `run_with` against a real (temp) state dir: reads
    /// the ledger it just wrote, and defaults `--by` to `harness`.
    #[test]
    fn run_with_reads_the_real_ledger_and_prints_a_table() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        crate::commands::ctx::log::append_delegation(
            &state,
            &crate::commands::ctx::log::Delegation {
                ts: 1_700_000_000,
                session: "sess-child",
                parent_session: "sess-parent",
                work_group_id: None,
                agent: "claude",
                model: Some("sonnet"),
                input_tokens: 1_000_000,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                output_tokens: 0,
                wall_ms: 1_000,
                exit_code: 0,
                outcome: "ok",
                mode: Some(WorkerMode::Writing),
                task_class: None,
            },
        )
        .expect("append");

        let cfg = CtxConfig::default();
        let args = SpendArgs {
            session: None,
            group: None,
            since: None,
            by: SpendDimension::Harness,
            json: false,
        };
        let mut out = Vec::new();
        let code = run_with(&state, &cfg, &args, &mut out, 1_700_000_100).expect("runs");
        assert_eq!(code, 0);
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("claude"), "got {text}");
        assert!(text.contains("$3.00"), "1M input tokens @ $3/M: {text}");
    }
}
