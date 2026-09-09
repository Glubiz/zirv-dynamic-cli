//! Issue #381: one pure model catalogue replacing the four hand-written
//! ladders that used to live separately in `adapters::claude`,
//! `adapters::codex`, `handover::tier_default` and `price::built_in_table`.
//! Pure -- no fs, clock, env or net -- so identical lookups give identical
//! answers everywhere this module is consulted, the same purity discipline
//! `rot.rs`/`permit::is_heavy`/`price.rs` itself already hold.
//!
//! A [`Vendor`] is a named model family (`anthropic`, `openai`, and the
//! survey vendors added alongside this module) with a strength-ordered
//! ladder of [`Rung`]s, strongest first. Every lookup here is the same
//! substring-on-lowercased-string match the two adapters used to each
//! implement by hand: a seat/model string can carry a full id
//! (`claude-opus-4-5`) or a bare alias (`opus`), and both must land on the
//! same rung.
//!
//! `anthropic` and `openai` carry the exact prices, strengths, windows and
//! ids the four call sites priced/ranked before this module existed
//! (verified by the equivalence tests in `adapters::claude`,
//! `adapters::codex`, `handover` and `price`); every other vendor is new
//! data from the 2026-09-07 survey, dated by [`CATALOGUE_AS_OF`] rather than
//! `price::BUILT_IN_AS_OF` because it was not priced on the same day. Every
//! new vendor reuses one pricing shortcut, noted once here rather than on
//! each rung: `cache_write_micros` equals `input_micros` (no separate
//! cache-write rate is published for any of them) and `cache_read_micros` is
//! `input_micros / 10` (the common ~90% cache-read discount every vendor
//! zirv already prices -- claude and codex included -- happens to share).

use std::borrow::Cow;

use super::price::ModelPrice;

/// The survey date behind every vendor in this module other than
/// `anthropic`/`openai` -- deliberately separate from [`super::price::
/// BUILT_IN_AS_OF`], which stays pinned to when the pre-existing prices were
/// last verified. Approximate on purpose: see this module's own doc comment
/// and `price.rs`'s for why a dated-but-approximate number beats an
/// undated-but-precise one. Carried on each survey [`Vendor`]'s own `as_of`
/// field, not just here, so a reader of one vendor never has to cross-check
/// a module constant to know how fresh its prices are.
pub const CATALOGUE_AS_OF: &str = "2026-09-07";

/// The three generic cost/capability tiers `handover::TIERS` resolves
/// against. Not every vendor fills all three -- a two-rung vendor like
/// `deepseek` has no `Standard` entry, and a one-rung vendor like `minimax`
/// fills only one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Cheap,
    Standard,
    Deep,
}

/// One rung on a vendor's model ladder. `alias` is the short name an
/// operator types (`opus`, `sonnet`); `id` is the canonical model string a
/// real transcript or `--model` flag carries. Claude's `fable`/`mythos`
/// orchestrator-tier aliases are two separate `Rung`s at the same
/// `strength` -- openai's `gpt-6-astra` shares the top `strength` with
/// `gpt-5.6-sol` the same way -- see [`rung_below`] for why that is enough
/// to keep both resolving identically.
///
/// `context_window` is `None` when this specific rung has no verified
/// capacity -- the same "never guess" rule `AgentAdapter::context_window_
/// tokens` documents applies per rung, not just per vendor: every codex rung
/// is `None` today because no capacity is verified for any of them, even
/// though the vendor itself might one day state a fallback. [`context_
/// window`] only falls back to the vendor's own default when the rung says
/// nothing at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rung {
    pub alias: &'static str,
    pub id: &'static str,
    pub strength: u8,
    pub context_window: Option<u64>,
    pub price: Option<ModelPrice>,
    pub tier: Option<Tier>,
}

/// A model family: a strength-ordered ladder (strongest first) plus the
/// vendor-wide fallback context window used when a model is unstated or not
/// on the ladder at all, and any priced ids that are not ladder rungs in
/// their own right (a long-context variant, a product model priced at an
/// existing rung's rate). `as_of` is `Some(catalogue_date)` for a survey
/// vendor priced on a specific day (see [`CATALOGUE_AS_OF`]) and `None` for
/// `anthropic`/`openai`, whose prices are governed by `price::
/// BUILT_IN_AS_OF` instead -- one table-wide stamp, not a per-vendor one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Vendor {
    pub slug: &'static str,
    pub rungs: &'static [Rung],
    pub default_context_window: Option<u64>,
    pub extra_prices: &'static [(&'static str, ModelPrice)],
    pub as_of: Option<&'static str>,
}

// Anthropic's own tier ladder (issue #155/#84's own verified names), copied
// verbatim from `adapters::claude`/`price::built_in_table` -- the
// equivalence tests in those modules pin the result.
const OPUS: ModelPrice = ModelPrice {
    input_micros: 15_000_000,
    cache_write_micros: 18_750_000,
    cache_read_micros: 1_500_000,
    output_micros: 75_000_000,
};
const OPUS_1M: ModelPrice = ModelPrice {
    input_micros: 30_000_000,
    cache_write_micros: 37_500_000,
    cache_read_micros: 3_000_000,
    output_micros: 150_000_000,
};
// The orchestrator tier above opus, priced AT the opus rate -- see
// `price.rs`'s own doc comment for why an unpriced top-of-fleet seat is the
// worse failure mode.
const FABLE: ModelPrice = OPUS;
const FABLE_1M: ModelPrice = OPUS_1M;
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

const ANTHROPIC_RUNGS: &[Rung] = &[
    Rung {
        alias: "fable",
        id: "claude-fable-5-1",
        strength: 4,
        context_window: Some(200_000),
        price: Some(FABLE),
        tier: None,
    },
    Rung {
        alias: "mythos",
        id: "claude-mythos-5",
        strength: 4,
        context_window: Some(200_000),
        price: Some(FABLE),
        tier: None,
    },
    Rung {
        alias: "opus",
        id: "claude-opus-5",
        strength: 3,
        context_window: Some(200_000),
        price: Some(OPUS),
        tier: Some(Tier::Deep),
    },
    Rung {
        alias: "sonnet",
        id: "claude-sonnet-5",
        strength: 2,
        context_window: Some(200_000),
        price: Some(SONNET),
        tier: Some(Tier::Standard),
    },
    Rung {
        alias: "haiku",
        id: "claude-haiku-5",
        strength: 1,
        context_window: Some(200_000),
        price: Some(HAIKU),
        tier: Some(Tier::Cheap),
    },
];

// codex's own tier ladder -- OpenAI's public pricing carries no separate
// cache-WRITE class, so each rung reuses its own input rate (copied
// verbatim from `price::built_in_table`).
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

// Codex's context window is not verified for any rung -- `adapters::codex`
// keeps the trait default `None` for `context_window_tokens`, and every
// rung here matches that with its own `None` rather than an invented number
// (see `Rung`'s own doc comment for why that is a per-rung answer, not just
// a per-vendor one). `gpt-6-astra` leads the ladder at the same strength as
// `gpt-5.6-sol` and, like claude's `fable`/`mythos`, carries `tier: None` --
// `rung_below`'s and `tier_model(Tier::Deep)`'s answers stay on `sol`.
const OPENAI_RUNGS: &[Rung] = &[
    Rung {
        alias: "gpt-6-astra",
        id: "gpt-6-astra",
        strength: 4,
        context_window: None,
        price: Some(SOL),
        tier: None,
    },
    Rung {
        alias: "gpt-5.6-sol",
        id: "gpt-5.6-sol",
        strength: 4,
        context_window: None,
        price: Some(SOL),
        tier: Some(Tier::Deep),
    },
    Rung {
        alias: "gpt-5.6-terra",
        id: "gpt-5.6-terra",
        strength: 3,
        context_window: None,
        price: Some(TERRA),
        tier: Some(Tier::Standard),
    },
    Rung {
        alias: "gpt-5.6-luna",
        id: "gpt-5.6-luna",
        strength: 2,
        context_window: None,
        price: Some(LUNA),
        tier: None,
    },
    Rung {
        alias: "gpt-5.4-mini",
        id: "gpt-5.4-mini",
        strength: 1,
        context_window: None,
        price: Some(MINI),
        tier: Some(Tier::Cheap),
    },
];

// -- Survey vendors (`CATALOGUE_AS_OF`), 2026-09-07. Every rung below reuses
// the shortcut this module's own doc comment names: `cache_write_micros`
// equals `input_micros`, `cache_read_micros` is `input_micros / 10`.

const GOOGLE_RUNGS: &[Rung] = &[
    Rung {
        alias: "gemini-3.1-pro-preview",
        id: "gemini-3.1-pro-preview",
        strength: 3,
        context_window: Some(1_000_000),
        price: Some(ModelPrice {
            input_micros: 2_000_000,
            cache_write_micros: 2_000_000,
            cache_read_micros: 200_000,
            output_micros: 12_000_000,
        }),
        tier: Some(Tier::Deep),
    },
    Rung {
        alias: "gemini-3.7-flash",
        id: "gemini-3.7-flash",
        strength: 2,
        context_window: Some(1_000_000),
        price: Some(ModelPrice {
            input_micros: 750_000,
            cache_write_micros: 750_000,
            cache_read_micros: 75_000,
            output_micros: 3_750_000,
        }),
        tier: Some(Tier::Standard),
    },
    Rung {
        alias: "gemini-3.5-flash-lite",
        id: "gemini-3.5-flash-lite",
        strength: 1,
        context_window: Some(1_000_000),
        price: Some(ModelPrice {
            input_micros: 300_000,
            cache_write_micros: 300_000,
            cache_read_micros: 30_000,
            output_micros: 2_500_000,
        }),
        tier: Some(Tier::Cheap),
    },
];

const XAI_RUNGS: &[Rung] = &[
    Rung {
        alias: "grok-4.6",
        id: "grok-4.6",
        strength: 3,
        context_window: Some(500_000),
        price: Some(ModelPrice {
            input_micros: 2_000_000,
            cache_write_micros: 2_000_000,
            cache_read_micros: 200_000,
            output_micros: 6_000_000,
        }),
        tier: Some(Tier::Deep),
    },
    Rung {
        alias: "grok-4.3",
        id: "grok-4.3",
        strength: 2,
        context_window: Some(1_000_000),
        price: Some(ModelPrice {
            input_micros: 1_250_000,
            cache_write_micros: 1_250_000,
            cache_read_micros: 125_000,
            output_micros: 2_500_000,
        }),
        tier: Some(Tier::Standard),
    },
    Rung {
        alias: "grok-build-0.1",
        id: "grok-build-0.1",
        strength: 1,
        context_window: Some(256_000),
        price: Some(ModelPrice {
            input_micros: 1_000_000,
            cache_write_micros: 1_000_000,
            cache_read_micros: 100_000,
            output_micros: 2_000_000,
        }),
        tier: Some(Tier::Cheap),
    },
];

const QWEN_RUNGS: &[Rung] = &[
    Rung {
        alias: "qwen3.8-max",
        id: "qwen3.8-max",
        strength: 3,
        context_window: Some(1_000_000),
        price: Some(ModelPrice {
            input_micros: 2_000_000,
            cache_write_micros: 2_000_000,
            cache_read_micros: 200_000,
            output_micros: 6_000_000,
        }),
        tier: Some(Tier::Deep),
    },
    Rung {
        alias: "qwen3-coder-plus",
        id: "qwen3-coder-plus",
        strength: 2,
        context_window: Some(1_000_000),
        price: Some(ModelPrice {
            input_micros: 1_000_000,
            cache_write_micros: 1_000_000,
            cache_read_micros: 100_000,
            output_micros: 5_000_000,
        }),
        tier: Some(Tier::Standard),
    },
    Rung {
        alias: "qwen3.8-flash",
        id: "qwen3.8-flash",
        strength: 1,
        context_window: Some(1_000_000),
        price: None,
        tier: Some(Tier::Cheap),
    },
];

const MOONSHOT_KIMI_STANDARD: ModelPrice = ModelPrice {
    input_micros: 950_000,
    cache_write_micros: 950_000,
    cache_read_micros: 95_000,
    output_micros: 4_000_000,
};

const MOONSHOT_RUNGS: &[Rung] = &[
    Rung {
        alias: "kimi-k3",
        id: "kimi-k3",
        strength: 3,
        context_window: Some(1_000_000),
        price: None,
        tier: Some(Tier::Deep),
    },
    Rung {
        alias: "kimi-k2.7-code",
        id: "kimi-k2.7-code",
        strength: 2,
        context_window: Some(256_000),
        price: Some(MOONSHOT_KIMI_STANDARD),
        tier: Some(Tier::Standard),
    },
    Rung {
        alias: "kimi-k2.6",
        id: "kimi-k2.6",
        strength: 1,
        context_window: Some(262_144),
        price: Some(MOONSHOT_KIMI_STANDARD),
        tier: Some(Tier::Cheap),
    },
];

const MISTRAL_RUNGS: &[Rung] = &[
    Rung {
        alias: "devstral-2",
        id: "devstral-2",
        strength: 3,
        context_window: Some(256_000),
        price: Some(ModelPrice {
            input_micros: 400_000,
            cache_write_micros: 400_000,
            cache_read_micros: 40_000,
            output_micros: 2_000_000,
        }),
        tier: Some(Tier::Deep),
    },
    Rung {
        alias: "mistral-medium-3.5",
        id: "mistral-medium-3.5",
        strength: 2,
        context_window: Some(256_000),
        price: None,
        tier: Some(Tier::Standard),
    },
    Rung {
        alias: "devstral-small-2",
        id: "devstral-small-2",
        strength: 1,
        context_window: Some(256_000),
        price: Some(ModelPrice {
            input_micros: 100_000,
            cache_write_micros: 100_000,
            cache_read_micros: 10_000,
            output_micros: 300_000,
        }),
        tier: Some(Tier::Cheap),
    },
];

// A two-rung vendor: no verified `Standard` model at all, so
// `tier_model(deepseek, Tier::Standard)` is `None` rather than a guess.
const DEEPSEEK_RUNGS: &[Rung] = &[
    Rung {
        alias: "deepseek-v4-pro",
        id: "deepseek-v4-pro",
        strength: 2,
        context_window: Some(1_000_000),
        price: Some(ModelPrice {
            input_micros: 660_000,
            cache_write_micros: 660_000,
            cache_read_micros: 66_000,
            output_micros: 1_980_000,
        }),
        tier: Some(Tier::Deep),
    },
    Rung {
        alias: "deepseek-v4-flash",
        id: "deepseek-v4-flash",
        strength: 1,
        context_window: Some(1_000_000),
        price: Some(ModelPrice {
            input_micros: 220_000,
            cache_write_micros: 220_000,
            cache_read_micros: 22_000,
            output_micros: 660_000,
        }),
        tier: Some(Tier::Cheap),
    },
];

const ZHIPU_RUNGS: &[Rung] = &[
    Rung {
        alias: "glm-5.3",
        id: "glm-5.3",
        strength: 3,
        context_window: Some(1_000_000),
        price: Some(ModelPrice {
            input_micros: 1_400_000,
            cache_write_micros: 1_400_000,
            cache_read_micros: 140_000,
            output_micros: 4_400_000,
        }),
        tier: Some(Tier::Deep),
    },
    Rung {
        alias: "glm-4.6",
        id: "glm-4.6",
        strength: 2,
        context_window: Some(205_000),
        price: Some(ModelPrice {
            input_micros: 430_000,
            cache_write_micros: 430_000,
            cache_read_micros: 43_000,
            output_micros: 1_740_000,
        }),
        tier: Some(Tier::Standard),
    },
    Rung {
        // Zhipu's own published free tier: a real, verified $0 rate, not an
        // unpriced model -- see `price.rs`'s own doc comment for why those
        // two are never the same thing.
        alias: "glm-4.7-flash",
        id: "glm-4.7-flash",
        strength: 1,
        context_window: Some(200_000),
        price: Some(ModelPrice {
            input_micros: 0,
            cache_write_micros: 0,
            cache_read_micros: 0,
            output_micros: 0,
        }),
        tier: Some(Tier::Cheap),
    },
];

// A one-rung vendor.
const MINIMAX_RUNGS: &[Rung] = &[Rung {
    alias: "minimax-m2.7",
    id: "minimax-m2.7",
    strength: 1,
    context_window: Some(204_800),
    price: Some(ModelPrice {
        input_micros: 240_000,
        cache_write_micros: 240_000,
        cache_read_micros: 24_000,
        output_micros: 960_000,
    }),
    tier: Some(Tier::Standard),
}];

const META_RUNGS: &[Rung] = &[
    Rung {
        alias: "muse-spark-1.2",
        id: "muse-spark-1.2",
        strength: 2,
        context_window: Some(200_000),
        price: Some(ModelPrice {
            input_micros: 1_250_000,
            cache_write_micros: 1_250_000,
            cache_read_micros: 125_000,
            output_micros: 4_250_000,
        }),
        tier: Some(Tier::Standard),
    },
    Rung {
        alias: "llama-4-maverick",
        id: "llama-4-maverick",
        strength: 1,
        context_window: Some(1_000_000),
        price: Some(ModelPrice {
            input_micros: 200_000,
            cache_write_micros: 200_000,
            cache_read_micros: 20_000,
            output_micros: 600_000,
        }),
        tier: Some(Tier::Cheap),
    },
];

const AMAZON_RUNGS: &[Rung] = &[
    Rung {
        alias: "nova-premier",
        id: "nova-premier",
        strength: 3,
        context_window: Some(1_000_000),
        price: None,
        tier: Some(Tier::Deep),
    },
    Rung {
        alias: "nova-pro",
        id: "nova-pro",
        strength: 2,
        context_window: Some(300_000),
        price: Some(ModelPrice {
            input_micros: 800_000,
            cache_write_micros: 800_000,
            cache_read_micros: 80_000,
            output_micros: 3_200_000,
        }),
        tier: Some(Tier::Standard),
    },
    Rung {
        alias: "nova-lite",
        id: "nova-lite",
        strength: 1,
        context_window: Some(1_000_000),
        price: Some(ModelPrice {
            input_micros: 60_000,
            cache_write_micros: 60_000,
            cache_read_micros: 6_000,
            output_micros: 240_000,
        }),
        tier: Some(Tier::Cheap),
    },
];

const VENDORS: &[Vendor] = &[
    Vendor {
        slug: "anthropic",
        rungs: ANTHROPIC_RUNGS,
        default_context_window: Some(200_000),
        extra_prices: &[
            ("claude-fable-5", FABLE),
            ("claude-fable-5[1m]", FABLE_1M),
            ("claude-fable-5-1[1m]", FABLE_1M),
            ("claude-mythos-5[1m]", FABLE_1M),
            ("claude-opus-5[1m]", OPUS_1M),
        ],
        as_of: None,
    },
    Vendor {
        slug: "openai",
        rungs: OPENAI_RUNGS,
        default_context_window: None,
        extra_prices: &[("gpt-5-codex", TERRA)],
        as_of: None,
    },
    Vendor {
        slug: "google",
        rungs: GOOGLE_RUNGS,
        default_context_window: None,
        extra_prices: &[],
        as_of: Some(CATALOGUE_AS_OF),
    },
    Vendor {
        slug: "xai",
        rungs: XAI_RUNGS,
        default_context_window: None,
        extra_prices: &[],
        as_of: Some(CATALOGUE_AS_OF),
    },
    Vendor {
        slug: "qwen",
        rungs: QWEN_RUNGS,
        default_context_window: None,
        extra_prices: &[],
        as_of: Some(CATALOGUE_AS_OF),
    },
    Vendor {
        slug: "moonshot",
        rungs: MOONSHOT_RUNGS,
        default_context_window: None,
        extra_prices: &[],
        as_of: Some(CATALOGUE_AS_OF),
    },
    Vendor {
        slug: "mistral",
        rungs: MISTRAL_RUNGS,
        default_context_window: None,
        extra_prices: &[],
        as_of: Some(CATALOGUE_AS_OF),
    },
    Vendor {
        slug: "deepseek",
        rungs: DEEPSEEK_RUNGS,
        default_context_window: None,
        extra_prices: &[],
        as_of: Some(CATALOGUE_AS_OF),
    },
    Vendor {
        slug: "zhipu",
        rungs: ZHIPU_RUNGS,
        default_context_window: None,
        extra_prices: &[],
        as_of: Some(CATALOGUE_AS_OF),
    },
    Vendor {
        slug: "minimax",
        rungs: MINIMAX_RUNGS,
        default_context_window: None,
        extra_prices: &[],
        as_of: Some(CATALOGUE_AS_OF),
    },
    Vendor {
        slug: "meta",
        rungs: META_RUNGS,
        default_context_window: None,
        extra_prices: &[],
        as_of: Some(CATALOGUE_AS_OF),
    },
    Vendor {
        slug: "amazon",
        rungs: AMAZON_RUNGS,
        default_context_window: None,
        extra_prices: &[],
        as_of: Some(CATALOGUE_AS_OF),
    },
];

/// The vendor named `slug`, or `None` when this catalogue does not carry it.
pub fn vendor(slug: &str) -> Option<&'static Vendor> {
    vendors().iter().find(|v| v.slug == slug)
}

/// Every vendor this catalogue knows about.
pub fn vendors() -> &'static [Vendor] {
    VENDORS
}

/// The rung `model` matches on `vendor`'s ladder: substring match on the
/// lowercased `model` against each rung's alias, then its id, scanning
/// strongest to weakest and returning the first hit -- the same rule both
/// adapters used to apply by hand.
pub fn rung_of(vendor: &Vendor, model: &str) -> Option<&'static Rung> {
    let model = model.to_lowercase();
    vendor
        .rungs
        .iter()
        .find(|r| model.contains(r.alias) || model.contains(r.id))
}

/// One tier below `seat` on `vendor`'s ladder, by alias.
///
/// An absent or unrecognised `seat` is treated exactly like a seat that
/// matched the *strongest* rung, then this steps one *strength* level down
/// from there -- not literally rung zero. That is why an unset claude seat
/// resolves to `opus`, not `fable`: `fable`/`mythos` are a second rung at
/// the same top strength as far as review escalation is concerned, and the
/// practical top an unrecognised seat should assume is the rung below them.
/// The same shape gives codex's unset seat `gpt-5.6-terra`, one below
/// `gpt-6-astra`/`gpt-5.6-sol`. A seat already on the floor rung maps to
/// itself rather than falling off the ladder.
pub fn rung_below(vendor: &Vendor, seat: Option<&str>) -> &'static str {
    let rungs = vendor.rungs;
    if rungs.is_empty() {
        return "";
    }
    let idx = seat
        .map(str::to_lowercase)
        .and_then(|s| {
            rungs
                .iter()
                .position(|r| s.contains(r.alias) || s.contains(r.id))
        })
        .unwrap_or(0);
    let strength = rungs[idx].strength;
    rungs[idx + 1..]
        .iter()
        .find(|r| r.strength < strength)
        .map_or(rungs[idx].alias, |r| r.alias)
}

/// `model`'s ladder strength on `vendor`, or `None` when it matches no rung.
pub fn strength(vendor: &Vendor, model: &str) -> Option<u8> {
    rung_of(vendor, model).map(|r| r.strength)
}

/// The usable context window for `model` on `vendor`: the matched rung's own
/// window when `model` is recognised AND that rung states one, else the
/// vendor's fallback -- which also covers an unstated (`None`) model and a
/// recognised rung with no verified figure of its own (every codex rung
/// today). `None` overall means this vendor states no fallback either and
/// `model` did not resolve to a rung with one (codex's case today: no
/// verified capacity to report at any level).
pub fn context_window(vendor: &Vendor, model: Option<&str>) -> Option<u64> {
    model
        .and_then(|m| rung_of(vendor, m))
        .and_then(|r| r.context_window)
        .or(vendor.default_context_window)
}

/// The model id for `vendor`'s `tier` rung, or `None` when no rung on this
/// vendor is tagged with it.
pub fn tier_model(vendor: &Vendor, tier: Tier) -> Option<&'static str> {
    vendor
        .rungs
        .iter()
        .find(|r| r.tier == Some(tier))
        .map(|r| r.alias)
}

/// Strips the vendor-namespacing decoration real-world model strings carry
/// so the bare id underneath can be matched against this catalogue:
/// OpenRouter's `vendor/model` prefix, Bedrock's `vendor.model` prefix, a
/// trailing `:suffix` (Bedrock's `:0`/`:256k` version and context tags, and
/// OpenRouter's `:free` marker -- both are just "everything from the first
/// colon onward"), and Vertex AI's trailing `@date` pin. Only a prefix
/// naming a vendor this catalogue actually knows about is stripped, so a
/// model id that legitimately contains `/` or `.` without meaning "vendor
/// namespace" passes through untouched.
#[allow(dead_code)] // first callers are the multi-provider wave-1 adapters (#385 OpenCode, #386 Pi), which resolve the billed vendor from a `provider/model` pin
pub fn normalize_id(model: &str) -> Cow<'_, str> {
    let mut s = model;
    match s.split_once('/') {
        // A `/` decides the outcome on its own: whether or not the prefix
        // names a known vendor, this is OpenRouter-shaped and the `.`
        // (Bedrock) check below never applies to it.
        Some((prefix, rest)) if vendor(&prefix.to_lowercase()).is_some() => s = rest,
        Some(_) => {}
        None => {
            if let Some((prefix, rest)) = s.split_once('.')
                && vendor(&prefix.to_lowercase()).is_some()
            {
                s = rest;
            }
        }
    }
    if let Some((head, _)) = s.split_once('@') {
        s = head;
    }
    if let Some((head, _)) = s.split_once(':') {
        s = head;
    }
    Cow::Borrowed(s)
}

/// The vendor `model` belongs to, after [`normalize_id`]: the first vendor
/// whose ladder or `extra_prices` names the normalised id, or -- when
/// nothing recognises it -- the vendor named by an explicit OpenRouter
/// `vendor/` prefix even though the id itself is unrecognised (a model this
/// catalogue has not caught up to yet, on a vendor it has).
#[allow(dead_code)] // first callers are the multi-provider wave-1 adapters (#385 OpenCode, #386 Pi), which resolve the billed vendor from a `provider/model` pin
pub fn vendor_of(model: &str) -> Option<&'static str> {
    let normalized = normalize_id(model).to_lowercase();
    for v in vendors() {
        if rung_of(v, &normalized).is_some() {
            return Some(v.slug);
        }
        if v.extra_prices.iter().any(|(id, _)| normalized.contains(id)) {
            return Some(v.slug);
        }
    }
    let (prefix, _) = model.split_once('/')?;
    vendor(&prefix.to_lowercase()).map(|v| v.slug)
}

/// Every priced alias, id and extra row across every vendor, as owned
/// `(model, price)` pairs -- `price::built_in_table` collects this straight
/// into its `BTreeMap`, so a duplicate key (an alias and id row sharing one
/// price, as every existing rung does) simply overwrites itself with the
/// same value.
pub fn built_in_prices() -> impl Iterator<Item = (String, ModelPrice)> {
    vendors().iter().flat_map(|v| {
        let rungs = v.rungs.iter().flat_map(|r| {
            r.price
                .into_iter()
                .flat_map(move |p| [(r.alias.to_string(), p), (r.id.to_string(), p)])
        });
        let extras = v.extra_prices.iter().map(|(id, p)| (id.to_string(), *p));
        rungs.chain(extras)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anthropic() -> &'static Vendor {
        vendor("anthropic").expect("anthropic is a built-in vendor")
    }

    fn openai() -> &'static Vendor {
        vendor("openai").expect("openai is a built-in vendor")
    }

    #[test]
    fn vendor_looks_up_by_slug_and_rejects_unknown_ones() {
        assert!(vendor("anthropic").is_some());
        assert!(vendor("openai").is_some());
        assert!(vendor("no-such-vendor").is_none());
    }

    #[test]
    fn vendors_lists_every_registered_vendor() {
        let slugs: Vec<&str> = vendors().iter().map(|v| v.slug).collect();
        for expected in [
            "anthropic",
            "openai",
            "google",
            "xai",
            "qwen",
            "moonshot",
            "mistral",
            "deepseek",
            "zhipu",
            "minimax",
            "meta",
            "amazon",
        ] {
            assert!(slugs.contains(&expected), "missing vendor {expected}");
        }
    }

    #[test]
    fn rung_of_matches_alias_and_id_case_insensitively() {
        let a = anthropic();
        assert_eq!(rung_of(a, "opus").map(|r| r.alias), Some("opus"));
        assert_eq!(rung_of(a, "claude-Opus-4-5").map(|r| r.alias), Some("opus"));
        assert_eq!(rung_of(a, "MYTHOS").map(|r| r.alias), Some("mythos"));
        assert_eq!(rung_of(a, "no-such-model"), None);
    }

    #[test]
    fn rung_below_walks_a_ladder_and_floors_at_the_bottom() {
        let a = anthropic();
        assert_eq!(rung_below(a, Some("fable")), "opus");
        assert_eq!(rung_below(a, Some("mythos")), "opus");
        assert_eq!(rung_below(a, Some("opus")), "sonnet");
        assert_eq!(rung_below(a, Some("sonnet")), "haiku");
        assert_eq!(rung_below(a, Some("haiku")), "haiku");
        assert_eq!(rung_below(a, None), "opus");
        assert_eq!(rung_below(a, Some("unreleased-model")), "opus");
    }

    #[test]
    fn strength_ranks_the_anthropic_ladder() {
        let a = anthropic();
        assert_eq!(strength(a, "fable"), Some(4));
        assert_eq!(strength(a, "mythos"), Some(4));
        assert_eq!(strength(a, "opus"), Some(3));
        assert_eq!(strength(a, "sonnet"), Some(2));
        assert_eq!(strength(a, "haiku"), Some(1));
        assert_eq!(strength(a, "unknown"), None);
    }

    #[test]
    fn context_window_falls_back_to_the_vendor_default() {
        let a = anthropic();
        assert_eq!(context_window(a, Some("opus")), Some(200_000));
        assert_eq!(context_window(a, None), Some(200_000));
        assert_eq!(context_window(a, Some("unknown-model")), Some(200_000));

        let o = openai();
        assert_eq!(context_window(o, None), None, "openai states no fallback");
        assert_eq!(
            context_window(o, Some("gpt-5.6-sol")),
            None,
            "a recognised rung with no verified figure falls to the vendor \
             default too, not to a guess"
        );
    }

    #[test]
    fn as_of_is_set_only_on_survey_vendors() {
        assert_eq!(anthropic().as_of, None);
        assert_eq!(openai().as_of, None);
        for v in vendors() {
            if v.slug == "anthropic" || v.slug == "openai" {
                continue;
            }
            assert_eq!(
                v.as_of,
                Some(CATALOGUE_AS_OF),
                "{}: survey vendors carry CATALOGUE_AS_OF on the vendor itself",
                v.slug
            );
        }
    }

    #[test]
    fn tier_model_resolves_each_generic_tier() {
        let a = anthropic();
        assert_eq!(tier_model(a, Tier::Cheap), Some("haiku"));
        assert_eq!(tier_model(a, Tier::Standard), Some("sonnet"));
        assert_eq!(tier_model(a, Tier::Deep), Some("opus"));

        let d = vendor("deepseek").expect("deepseek is built in");
        assert_eq!(
            tier_model(d, Tier::Standard),
            None,
            "a two-rung vendor may leave a tier unfilled"
        );
    }

    #[test]
    fn openai_ladder_gains_gpt_6_astra_as_a_ranked_rung() {
        let o = openai();
        let rung = rung_of(o, "gpt-6-astra").expect("gpt-6-astra is now a ranked rung");
        assert_eq!(rung.strength, 4);
        assert_eq!(rung.tier, None);
        assert_eq!(
            tier_model(o, Tier::Deep),
            Some("gpt-5.6-sol"),
            "astra shares the top strength but carries no tier of its own"
        );
        assert_eq!(rung_below(o, Some("gpt-6-astra")), "gpt-5.6-terra");
    }

    #[test]
    fn normalize_id_strips_every_documented_decoration() {
        assert_eq!(
            normalize_id("anthropic/claude-opus-5"),
            Cow::Borrowed("claude-opus-5")
        );
        assert_eq!(
            normalize_id("anthropic.claude-opus-5:0"),
            Cow::Borrowed("claude-opus-5")
        );
        assert_eq!(
            normalize_id("amazon.nova-pro-v1:0:256k"),
            Cow::Borrowed("nova-pro-v1")
        );
        assert_eq!(
            normalize_id("claude-opus-4-5@20251101"),
            Cow::Borrowed("claude-opus-4-5")
        );
        assert_eq!(
            normalize_id("deepseek/deepseek-chat:free"),
            Cow::Borrowed("deepseek-chat")
        );
        assert_eq!(
            normalize_id("sonnet"),
            Cow::Borrowed("sonnet"),
            "a bare alias with no decoration passes through unchanged"
        );
    }

    #[test]
    fn vendor_of_resolves_normalised_ids_across_vendors() {
        assert_eq!(vendor_of("anthropic/claude-opus-5"), Some("anthropic"));
        assert_eq!(vendor_of("anthropic.claude-opus-5:0"), Some("anthropic"));
        assert_eq!(vendor_of("amazon.nova-pro-v1:0:256k"), Some("amazon"));
        assert_eq!(vendor_of("claude-opus-4-5@20251101"), Some("anthropic"));
        assert_eq!(vendor_of("deepseek/deepseek-chat:free"), Some("deepseek"));
        assert_eq!(vendor_of("gpt-5.6-sol"), Some("openai"));
        assert_eq!(vendor_of("gpt-6-astra"), Some("openai"));
        assert_eq!(vendor_of("totally-unknown-model"), None);
    }

    #[test]
    fn vendor_of_falls_back_to_an_explicit_prefix_for_an_unrecognised_id() {
        assert_eq!(
            vendor_of("google/some-future-gemini-nobody-has-catalogued-yet"),
            Some("google")
        );
    }

    #[test]
    fn built_in_prices_yields_every_alias_id_and_extra_row() {
        let table: std::collections::BTreeMap<String, ModelPrice> = built_in_prices().collect();
        for key in [
            "fable",
            "mythos",
            "opus",
            "sonnet",
            "haiku",
            "claude-fable-5",
            "claude-fable-5[1m]",
            "claude-fable-5-1",
            "claude-fable-5-1[1m]",
            "claude-mythos-5",
            "claude-mythos-5[1m]",
            "claude-opus-5",
            "claude-opus-5[1m]",
            "claude-sonnet-5",
            "claude-haiku-5",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
            "gpt-5.4-mini",
            "gpt-5-codex",
            "gpt-6-astra",
        ] {
            assert!(table.contains_key(key), "missing priced key {key}");
        }
        assert!(
            table.contains_key("gemini-3.1-pro-preview"),
            "new-vendor rows must be included too"
        );
    }

    #[test]
    fn every_new_vendor_rung_reuses_the_documented_cache_shortcut() {
        for v in vendors() {
            if v.slug == "anthropic" || v.slug == "openai" {
                continue;
            }
            for r in v.rungs {
                if let Some(p) = r.price {
                    assert_eq!(
                        p.cache_write_micros, p.input_micros,
                        "{}/{}: cache-write must equal input",
                        v.slug, r.id
                    );
                    assert_eq!(
                        p.cache_read_micros,
                        p.input_micros / 10,
                        "{}/{}: cache-read must be input/10",
                        v.slug,
                        r.id
                    );
                }
            }
        }
    }
}
