//! Issue #264: the cost ledger's own pricing. [`price`] is pure -- no fs,
//! clock, or env -- so identical inputs give identical costs, the same
//! purity discipline `permit::is_heavy`/`rot.rs` already hold; only
//! [`resolve_table`] touches the filesystem.
//!
//! Money is integer MICRO-USD everywhere, never a float (ADR-322C's own rule
//! for any ledger value): a [`ModelPrice`] field is micro-USD PER MILLION
//! TOKENS of its own class, and [`price`] scales a real token count against
//! that per-million rate with `u128` intermediate arithmetic so nothing
//! overflows before the final division.
//!
//! An unknown model prices as `None`, never `0` -- a model this table has
//! never priced is not free, it is UNKNOWN, and treating the two the same
//! would silently undercount the ledger's own totals for exactly the models
//! an operator most needs visibility into (a model named in `handover.rs`
//! before this table's own built-in list catches up to it).
//!
//! The built-in table covers the models zirv's own `handover.rs` already
//! names (the tier aliases `equivalent_model` resolves through, plus the
//! canonical ids those tiers and the harness transcripts themselves surface)
//! at `BUILT_IN_AS_OF`. Prices are deliberately approximate -- the `as_of`
//! stamp, and [`PriceTable::is_stale`] checking it, are what actually
//! matters; a real operator who cares about exact numbers overrides via
//! `~/.zirv/prices.toml` (or `price.table_path`), never by editing this
//! file's constants to chase a vendor's own price-list revision.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::config::CtxConfig;
use super::event::TranscriptUsage;

/// The built-in table's own pricing date -- see this module's own doc
/// comment for why "approximate but dated" beats "exact but silently stale".
pub const BUILT_IN_AS_OF: &str = "2026-09-01";

/// The filename `resolve_table` reads under the operator's own `~/.zirv/`
/// when `price.table_path` is unset -- the same "operator override, home
/// directory only" shape `ctx.toml` itself already establishes.
const DEFAULT_TABLE_FILENAME: &str = "prices.toml";

/// One model's price, in MICRO-USD PER MILLION TOKENS, split by the same
/// four raw token classes [`TranscriptUsage`]/`log::Delegation` already
/// carry -- cache read and cache write are priced very differently from a
/// fresh input token on every vendor zirv talks to, and folding them into
/// one number would hide exactly the number `Delegation`'s own doc comment
/// says this ledger exists to answer (a worker's cache-hit ratio).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPrice {
    pub input_micros: u64,
    pub cache_write_micros: u64,
    pub cache_read_micros: u64,
    pub output_micros: u64,
}

/// A dated price list, keyed by the exact model string a transcript/
/// `Delegation` row carries. `as_of` is a plain `YYYY-MM-DD` stamp, checked
/// by [`PriceTable::is_stale`] -- never trusted as fresh forever.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PriceTable {
    pub as_of: String,
    pub models: BTreeMap<String, ModelPrice>,
}

impl PriceTable {
    /// Whether this table's own `as_of` stamp is more than `stale_after_days`
    /// old, as of `now_epoch_secs`. An `as_of` that fails to parse is treated
    /// as stale -- fail SAFE, the same "never silently trust unverified data"
    /// posture the rest of this ledger holds: a table that cannot even say
    /// when it was priced must never be presented as current.
    pub fn is_stale(&self, now_epoch_secs: u64, stale_after_days: u64) -> bool {
        let Some(as_of_days) = parse_iso_date_days(&self.as_of) else {
            return true;
        };
        let now_days = (now_epoch_secs / 86_400) as i64;
        now_days.saturating_sub(as_of_days) > i64::try_from(stale_after_days).unwrap_or(i64::MAX)
    }
}

/// The cost of `usage` under `model`'s own rate in `table`, in micro-USD --
/// `None` when `model` has no entry in `table` at all, never `0`. Pure: no
/// fs, clock, network, or env -- identical inputs give identical costs.
///
/// `u128` intermediate arithmetic on every class: a session's own token
/// counts can run into the tens of millions, and `tokens * micros_per_
/// million` in `u64` would overflow well before the division that scales it
/// back down.
pub fn price(model: &str, usage: &TranscriptUsage, table: &PriceTable) -> Option<u64> {
    let rate = table.models.get(model)?;
    let scaled = |tokens: u64, micros_per_million: u64| -> u64 {
        u64::try_from((u128::from(tokens) * u128::from(micros_per_million)) / 1_000_000)
            .unwrap_or(u64::MAX)
    };
    Some(
        scaled(usage.input_tokens, rate.input_micros)
            .saturating_add(scaled(
                usage.cache_creation_input_tokens,
                rate.cache_write_micros,
            ))
            .saturating_add(scaled(
                usage.cache_read_input_tokens,
                rate.cache_read_micros,
            ))
            .saturating_add(scaled(usage.output_tokens, rate.output_micros)),
    )
}

/// Resolves the effective price table: the built-in one ([`built_in_table`]),
/// overridden WHOLESALE by an operator's own file when present and
/// parseable -- `cfg.price.table_path` if set (a literal path, `~/` expanded
/// against the real home directory), else `~/.zirv/prices.toml`. A missing or
/// unparseable override file is not an error: the built-in table is what "no
/// override" already meant, and a mistyped operator file must not turn every
/// cost line in the ledger into a hard failure -- the same best-effort
/// posture every other state-dir/config reader in this codebase holds.
pub fn resolve_table(cfg: &CtxConfig) -> PriceTable {
    let Some(path) = override_path(cfg) else {
        return built_in_table();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return built_in_table();
    };
    toml::from_str(&text).unwrap_or_else(|_| built_in_table())
}

/// The path [`resolve_table`] reads its override from, or `None` when no
/// home directory can be determined at all (the same fallback every other
/// `~/.zirv/...` reader in this codebase already tolerates).
fn override_path(cfg: &CtxConfig) -> Option<PathBuf> {
    match &cfg.price.table_path {
        Some(configured) => Some(expand_home(configured)),
        None => crate::utils::home_dir()
            .ok()
            .map(|home| home.join(".zirv").join(DEFAULT_TABLE_FILENAME)),
    }
}

/// Expands a leading `~/` against the real home directory; any other path
/// (absolute, or one that already resolved a home some other way) passes
/// through unchanged. An unresolvable home falls back to the literal `~/...`
/// path text, which will then simply fail to read -- resolved to "use the
/// built-in table" by [`resolve_table`]'s own caller, same as any other
/// missing file.
fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Ok(home) = crate::utils::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(path)
}

/// The built-in price table zirv ships, covering the models `handover.rs`
/// already names (`equivalent_model`'s own tier aliases, plus the canonical
/// ids those tiers and the harness transcripts themselves surface). See this
/// module's own doc comment for why the numbers are deliberately
/// approximate.
pub fn built_in_table() -> PriceTable {
    const OPUS: ModelPrice = ModelPrice {
        input_micros: 15_000_000,
        cache_write_micros: 18_750_000,
        cache_read_micros: 1_500_000,
        output_micros: 75_000_000,
    };
    // The long-context ("[1m]") variant `claude.rs`'s own capability probe
    // names: priced at the vendor's usual long-context surcharge, roughly
    // double the ordinary-context rate.
    const OPUS_1M: ModelPrice = ModelPrice {
        input_micros: 30_000_000,
        cache_write_micros: 37_500_000,
        cache_read_micros: 3_000_000,
        output_micros: 150_000_000,
    };
    const SONNET: ModelPrice = ModelPrice {
        input_micros: 3_000_000,
        cache_write_micros: 3_750_000,
        cache_read_micros: 300_000,
        output_micros: 15_000_000,
    };
    const HAIKU: ModelPrice = ModelPrice {
        input_micros: 800_000,
        cache_write_micros: 1_000_000,
        cache_read_micros: 80_000,
        output_micros: 4_000_000,
    };
    // codex's own tier ladder (`adapters::codex::review_model_below`'s own
    // rungs) -- OpenAI's public pricing carries no separate cache-WRITE
    // class, so each rung reuses its own input rate there rather than
    // guessing a number this table cannot verify.
    const SOL: ModelPrice = ModelPrice {
        input_micros: 15_000_000,
        cache_write_micros: 15_000_000,
        cache_read_micros: 1_500_000,
        output_micros: 60_000_000,
    };
    const TERRA: ModelPrice = ModelPrice {
        input_micros: 2_500_000,
        cache_write_micros: 2_500_000,
        cache_read_micros: 250_000,
        output_micros: 10_000_000,
    };
    const LUNA: ModelPrice = ModelPrice {
        input_micros: 1_000_000,
        cache_write_micros: 1_000_000,
        cache_read_micros: 100_000,
        output_micros: 4_000_000,
    };
    const MINI: ModelPrice = ModelPrice {
        input_micros: 250_000,
        cache_write_micros: 250_000,
        cache_read_micros: 25_000,
        output_micros: 1_000_000,
    };

    let models = BTreeMap::from([
        // Tier aliases (`handover::equivalent_model`'s own vocabulary).
        ("opus".to_string(), OPUS),
        ("sonnet".to_string(), SONNET),
        ("haiku".to_string(), HAIKU),
        // Canonical claude model ids a real transcript/`--model` flag names.
        ("claude-opus-5".to_string(), OPUS),
        ("claude-opus-5[1m]".to_string(), OPUS_1M),
        ("claude-sonnet-5".to_string(), SONNET),
        ("claude-haiku-5".to_string(), HAIKU),
        // codex's own tier ladder.
        ("gpt-5.6-sol".to_string(), SOL),
        ("gpt-5.6-terra".to_string(), TERRA),
        ("gpt-5.6-luna".to_string(), LUNA),
        ("gpt-5.4-mini".to_string(), MINI),
        // codex's coding-specific product model, named directly by workers
        // that pin it rather than going through the tier ladder.
        ("gpt-5-codex".to_string(), TERRA),
    ]);

    PriceTable {
        as_of: BUILT_IN_AS_OF.to_string(),
        models,
    }
}

/// Integer-only USD formatting -- two decimal places, no float anywhere in
/// the ledger's own arithmetic (ADR-322C). A `stale` reading prefixes the
/// figure with `~`, the shared "this cost line is approximate" flag `zirv
/// ctx spend`/`zirv ctx status` both apply per [`PriceTable::is_stale`].
pub fn format_usd(micros: u64, stale: bool) -> String {
    let dollars = micros / 1_000_000;
    let cents = (micros % 1_000_000) / 10_000;
    if stale {
        format!("~${dollars}.{cents:02}")
    } else {
        format!("${dollars}.{cents:02}")
    }
}

/// Days since the Unix epoch (1970-01-01) for a proleptic-Gregorian
/// `(year, month, day)` -- Howard Hinnant's `days_from_civil` algorithm, pure
/// integer arithmetic. No date-handling crate dependency for what only ever
/// needs to answer "how many days apart are these two calendar dates".
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (m + 9) % 12; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// Parses a plain `YYYY-MM-DD` stamp into [`days_from_civil`]'s day count,
/// `None` for anything that does not parse as three dot-free integers in
/// range -- deliberately strict rather than guessing at a partial or
/// malformed stamp.
fn parse_iso_date_days(text: &str) -> Option<i64> {
    let mut parts = text.splitn(3, '-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: i64 = parts.next()?.parse().ok()?;
    let d: i64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, m, d))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u64, cache_write: u64, cache_read: u64, output: u64) -> TranscriptUsage {
        TranscriptUsage {
            input_tokens: input,
            cache_creation_input_tokens: cache_write,
            cache_read_input_tokens: cache_read,
            output_tokens: output,
        }
    }

    fn table_with(model: &str, rate: ModelPrice) -> PriceTable {
        PriceTable {
            as_of: BUILT_IN_AS_OF.to_string(),
            models: BTreeMap::from([(model.to_string(), rate)]),
        }
    }

    /// Each of the four token classes is priced independently, at its own
    /// rate -- the whole reason `Delegation`/`TranscriptUsage` keep them raw
    /// rather than pre-summed (see this module's own doc comment).
    #[test]
    fn price_sums_each_token_class_at_its_own_rate() {
        let rate = ModelPrice {
            input_micros: 1_000_000,
            cache_write_micros: 2_000_000,
            cache_read_micros: 100_000,
            output_micros: 5_000_000,
        };
        let table = table_with("test-model", rate);

        // 1_000_000 input tokens @ $1/M = $1 = 1_000_000 micros.
        assert_eq!(
            price("test-model", &usage(1_000_000, 0, 0, 0), &table),
            Some(1_000_000)
        );
        // 1_000_000 cache-write tokens @ $2/M = $2.
        assert_eq!(
            price("test-model", &usage(0, 1_000_000, 0, 0), &table),
            Some(2_000_000)
        );
        // 1_000_000 cache-read tokens @ $0.10/M = $0.10.
        assert_eq!(
            price("test-model", &usage(0, 0, 1_000_000, 0), &table),
            Some(100_000)
        );
        // 1_000_000 output tokens @ $5/M = $5.
        assert_eq!(
            price("test-model", &usage(0, 0, 0, 1_000_000), &table),
            Some(5_000_000)
        );
        // All four together sum: $1 + $2 + $0.10 + $5 = $8.10.
        assert_eq!(
            price(
                "test-model",
                &usage(1_000_000, 1_000_000, 1_000_000, 1_000_000),
                &table
            ),
            Some(8_100_000)
        );
    }

    /// A partial-million token count still scales correctly -- proves the
    /// `u128`/division arithmetic, not just the round-number case above.
    #[test]
    fn price_scales_a_partial_million_token_count() {
        let rate = ModelPrice {
            input_micros: 3_000_000, // $3/M
            cache_write_micros: 0,
            cache_read_micros: 0,
            output_micros: 0,
        };
        let table = table_with("test-model", rate);
        // 500 tokens @ $3/M = 500 * 3_000_000 / 1_000_000 = 1_500 micros.
        assert_eq!(
            price("test-model", &usage(500, 0, 0, 0), &table),
            Some(1_500)
        );
    }

    /// The load-bearing acceptance criterion: an unknown model prices as
    /// `None`, never `0` -- a model this table has never priced is not free.
    #[test]
    fn an_unknown_model_prices_as_none_never_zero() {
        let table = table_with(
            "known-model",
            ModelPrice {
                input_micros: 1_000_000,
                cache_write_micros: 1_000_000,
                cache_read_micros: 1_000_000,
                output_micros: 1_000_000,
            },
        );
        assert_eq!(
            price("some-model-nobody-priced", &usage(1_000, 0, 0, 0), &table),
            None
        );
    }

    /// Every model the built-in table's own doc comment promises actually
    /// has an entry, and every entry is non-zero on every class -- a `0`
    /// anywhere would be indistinguishable from "free", which no real model
    /// is.
    #[test]
    fn the_built_in_table_prices_every_model_it_names_on_every_class() {
        let table = built_in_table();
        for name in [
            "opus",
            "sonnet",
            "haiku",
            "claude-opus-5",
            "claude-opus-5[1m]",
            "claude-sonnet-5",
            "claude-haiku-5",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
            "gpt-5.4-mini",
            "gpt-5-codex",
        ] {
            let rate = table
                .models
                .get(name)
                .unwrap_or_else(|| panic!("the built-in table must price `{name}`"));
            assert!(rate.input_micros > 0, "{name}: input must not be free");
            assert!(rate.output_micros > 0, "{name}: output must not be free");
        }
        assert_eq!(table.as_of, BUILT_IN_AS_OF);
    }

    /// A table older than `stale_after_days` is stale; one within it is not
    /// -- pinned at the exact boundary, same discipline `permit::is_stale`'s
    /// own test holds for its identical shape.
    #[test]
    fn is_stale_flags_only_strictly_past_the_threshold() {
        let table = PriceTable {
            as_of: "2026-01-01".to_string(),
            models: BTreeMap::new(),
        };
        let as_of_secs = days_from_civil(2026, 1, 1) as u64 * 86_400;

        assert!(
            !table.is_stale(as_of_secs + 90 * 86_400, 90),
            "exactly at the threshold is not yet stale"
        );
        assert!(
            table.is_stale(as_of_secs + 91 * 86_400, 90),
            "one day past the threshold is stale"
        );
        assert!(
            !table.is_stale(as_of_secs, 90),
            "a table dated today is never stale"
        );
    }

    /// An `as_of` that fails to parse is treated as stale -- fail SAFE, never
    /// presented as current when this ledger cannot even verify its own age.
    #[test]
    fn an_unparseable_as_of_is_always_stale() {
        let table = PriceTable {
            as_of: "not-a-date".to_string(),
            models: BTreeMap::new(),
        };
        assert!(table.is_stale(0, 90));
        assert!(table.is_stale(u64::MAX, u64::MAX));
    }

    /// Two decimal places, no rounding surprises at exact cent boundaries,
    /// and the `~` prefix only when `stale` is true.
    #[test]
    fn format_usd_renders_two_decimal_places_and_the_stale_prefix() {
        assert_eq!(format_usd(1_000_000, false), "$1.00");
        assert_eq!(format_usd(1_234_567, false), "$1.23");
        assert_eq!(format_usd(0, false), "$0.00");
        assert_eq!(format_usd(1_000_000, true), "~$1.00");
    }

    /// `resolve_table` falls back to the built-in table when no override
    /// file exists at all -- the ordinary, no-operator-file case.
    #[test]
    fn resolve_table_falls_back_to_built_in_with_no_override_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(tmp.path());
        let cfg = CtxConfig::default();
        assert_eq!(resolve_table(&cfg), built_in_table());
    }

    /// `price.table_path` is honoured over the default `~/.zirv/prices.toml`
    /// location, and a `~/`-prefixed path expands against the real home
    /// directory rather than being read literally.
    #[test]
    fn resolve_table_honours_an_explicit_tilde_table_path_override() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(tmp.path());
        std::fs::create_dir_all(tmp.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            tmp.path().join(".zirv").join("my-prices.toml"),
            "as_of = \"2020-01-01\"\n\n[models.custom]\ninput_micros = 1\ncache_write_micros = 1\ncache_read_micros = 1\noutput_micros = 1\n",
        )
        .expect("write override");

        let cfg = CtxConfig {
            price: crate::commands::ctx::config::PriceConfig {
                stale_after_days: 90,
                table_path: Some("~/.zirv/my-prices.toml".to_string()),
            },
            ..CtxConfig::default()
        };
        let table = resolve_table(&cfg);
        assert_eq!(table.as_of, "2020-01-01");
        assert!(table.models.contains_key("custom"));
        assert!(
            !table.models.contains_key("opus"),
            "an explicit override replaces the built-in table wholesale, it does not merge"
        );
    }

    /// A present-but-unparseable override file must never turn pricing into
    /// a hard failure -- it degrades to the built-in table, the same
    /// best-effort posture every other state-dir/config reader in this
    /// codebase holds for a corrupt file.
    #[test]
    fn resolve_table_falls_back_to_built_in_on_an_unparseable_override() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(tmp.path());
        std::fs::create_dir_all(tmp.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            tmp.path().join(".zirv").join(DEFAULT_TABLE_FILENAME),
            "not valid toml {{{",
        )
        .expect("write bad override");

        let cfg = CtxConfig::default();
        assert_eq!(resolve_table(&cfg), built_in_table());
    }
}
