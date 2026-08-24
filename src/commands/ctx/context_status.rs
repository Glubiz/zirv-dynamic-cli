//! `zirv context status` (issue #46, "Context 8/8"): a read-only report of
//! everything Zirv-managed context a session would receive, built without
//! ever starting one.
//!
//! **No session, no process, no network.** This module calls only pure or
//! filesystem-reading functions: `optimize::collect_surfaces` (issues #39/
//! #40/#41), `drift::analyze` (issue #42), `compile::compile` (issue #44,
//! which itself calls `policy::evaluate`, issue #43), `memory::render_for_
//! prompt`/`prompt::memory_injection_summary`, `mail::list`, and `handoff::
//! latest_for_repo`. None of these spawn the agent binary or make a network
//! call -- `compile::compile` in particular only ever calls `adapter.name()`
//! and `adapter.policy_support()`, both pure, never `adapter.ready()` or any
//! `headless_cmd`/`interactive_cmd` construction. See
//! `status_never_spawns_the_configured_agent_binary` below for the test that
//! actually proves it with a real (marker-writing) fake binary, not just an
//! absent one.
//!
//! **Bytes are measured, tokens are estimated.** Every byte figure in this
//! report comes from an actual `.len()` on text read from disk or state.
//! Every "~N tok" figure is [`estimate_tokens`] -- a fixed, documented
//! bytes-per-token divisor, not a real tokenizer (no tokenizer dependency is
//! added to `Cargo.toml`; see the issue). The method and its limits are
//! printed in the report itself ([`render_token_estimate_note`]), not just
//! documented here.
//!
//! **Zirv only knows what it injects.** [`render_unknown_context_section`]
//! is the honesty disclosure issue #46 requires: every number in this report
//! is the *zirv-managed* portion of a session's context, never a claim about
//! the harness's own hidden system prompt or built-in tool scaffolding,
//! which zirv cannot see at all. There is no "total context" line anywhere
//! in this report -- see `output_never_claims_a_total_context_figure`.
//!
//! **No false equivalence.** The per-harness section
//! ([`render_per_harness_section`]) prints each adapter's own
//! `PolicyReport::render()` verbatim (issue #43): claude and codex render
//! different `Support` states for the same capability where that is the
//! honest answer, and this module does nothing to smooth that over.
//!
//! **Determinism.** Every collection this module walks is sorted by a stable
//! key before rendering (surfaces by path, drift findings tallied into a
//! `BTreeMap<&'static str, _>` keyed by kind, adapters by name) -- no
//! `HashMap` iteration ever reaches the rendered output. `now` is a plain
//! `u64` the caller supplies (`state::now_secs()` in [`run`], a fixed value
//! in tests), the same clock-free discipline `compile.rs`/`rot.rs` hold
//! themselves to, so two calls with identical on-disk state produce
//! byte-identical output regardless of wall-clock timing.

use std::io::Write;
use std::path::Path;

use super::CtxResult;
use super::adapters;
use super::compile;
use super::config::{ContextConfig, CtxConfig, EnvLookup, env_from_process};
use super::context_cli;
use super::drift;
use super::handoff;
use super::mail;
use super::memory;
use super::optimize::{self, Finding, Layer, Severity, Surface};
use super::prompt::{self, PromptRole};
use super::resume;
use super::state::{StateDir, now_secs, repo_slug};
use super::surface::Kind;

/// Bytes-per-token divisor for [`estimate_tokens`]: a rough heuristic for
/// English prose and source code under common vendor tokenizers (roughly
/// 3.5-4.5 bytes/token for ASCII text), not a measurement of any specific
/// vendor's real tokenizer. Deliberately a plain constant, not a dependency:
/// the issue this module implements explicitly forbids adding a tokenizer
/// crate to `Cargo.toml`.
const BYTES_PER_TOKEN: f64 = 4.0;

/// Rounds up: an estimate that undercounts is more likely to surprise an
/// operator ("it said N tokens but I got billed for more") than one that
/// overcounts slightly.
fn estimate_tokens(bytes: usize) -> usize {
    if bytes == 0 {
        return 0;
    }
    ((bytes as f64) / BYTES_PER_TOKEN).ceil() as usize
}

#[derive(Debug, clap::Args)]
pub struct StatusArgs {
    /// Show every drift finding's full evidence and detail, not just the
    /// per-kind counts.
    #[arg(long)]
    pub verbose: bool,
}

/// Whether `layer` is part of the canonical `.zirv/context/` layer (issue
/// #41), as opposed to a native/harness-owned instruction file zirv reads
/// only for drift detection and never injects itself.
fn is_canonical(layer: Layer) -> bool {
    matches!(
        layer,
        Layer::ContextCommon | Layer::ContextClaude | Layer::ContextCodex
    )
}

/// The budget a surface's raw byte count is measured against for the
/// `[OVERSIZED]` flag (acceptance criterion: oversized CLAUDE.md/AGENTS.md/
/// canonical surfaces must be visible before they silently consume budget).
/// `None` for a non-instructions surface (settings are structured, not
/// prose bloat).
///
/// The two canonical files use their own real, enforced budgets
/// (`context.max_common_bytes`/`max_harness_bytes` -- see `compile.rs`).
/// Every native/harness-owned instructions file (`CLAUDE.md`, `AGENTS.md`,
/// nested or global) has no configured budget of its own -- zirv does not
/// inject these itself, only reads them for drift detection -- so
/// `context.max_harness_bytes` is reused as the reference size for "does
/// this look oversized", the same number an operator already tuned for a
/// canonical harness-specific file. This is a reporting-only comparison: it
/// never truncates a native file the way the canonical budgets truncate
/// what is actually injected.
fn oversized_threshold(layer: Layer, cfg: &ContextConfig) -> Option<usize> {
    if layer.kind() != Kind::Instructions {
        return None;
    }
    Some(match layer {
        Layer::ContextCommon => cfg.max_common_bytes,
        Layer::ContextClaude | Layer::ContextCodex => cfg.max_harness_bytes,
        _ => cfg.max_harness_bytes,
    })
}

fn render_surface_line<W: Write>(
    w: &mut W,
    surface: &Surface,
    cfg: &ContextConfig,
    indent: &str,
) -> CtxResult<()> {
    let bytes = surface.text.len();
    let tokens = estimate_tokens(bytes);
    let oversized = oversized_threshold(surface.layer, cfg).is_some_and(|budget| bytes > budget);
    let flag = if oversized { "  [OVERSIZED]" } else { "" };
    // Issue #105: a managed native file is still listed here -- its size
    // still counts toward budgets -- but is excluded from the duplicate/
    // precedence-level drift analysis below (`context_cli::
    // surfaces_for_drift`), since it is a verbatim render of the canonical
    // layer it would otherwise be reported as "duplicating". The note
    // explains that exclusion right where a reader would otherwise wonder
    // why this file never shows up in a drift finding.
    let managed_note = if context_cli::is_managed(&surface.text) {
        "  (zirv-managed, rendered from .zirv/context/)"
    } else {
        ""
    };
    writeln!(
        w,
        "{indent}{} ({}) -- {bytes}B, ~{tokens} tok (est.){flag}{managed_note}",
        surface.path.display(),
        surface.layer.label()
    )?;
    Ok(())
}

/// "Discovered instruction surfaces with byte counts and token estimates;
/// canonical vs native/harness-specific split" -- one of the acceptance
/// bullets, rendered directly. `surfaces` is sorted by path before rendering
/// (determinism requirement): `optimize::collect_surfaces`'s own order is
/// already deterministic, but sorting here makes that a property of this
/// function rather than an assumption about its caller.
fn render_instruction_surfaces<W: Write>(
    w: &mut W,
    surfaces: &[Surface],
    cfg: &ContextConfig,
) -> CtxResult<()> {
    let mut instructions: Vec<&Surface> = surfaces
        .iter()
        .filter(|s| s.layer.kind() == Kind::Instructions)
        .collect();
    instructions.sort_by(|a, b| a.path.cmp(&b.path));
    let (canonical, native): (Vec<&Surface>, Vec<&Surface>) = instructions
        .into_iter()
        .partition(|s| is_canonical(s.layer));

    writeln!(
        w,
        "\ninstruction surfaces (bytes measured on disk; tokens are estimated -- see the method \
         note at the end of this report):"
    )?;
    writeln!(
        w,
        "  canonical (.zirv/context/, shared across harnesses unless noted):"
    )?;
    if canonical.is_empty() {
        writeln!(w, "    (none found)")?;
    }
    for surface in &canonical {
        render_surface_line(w, surface, cfg, "    ")?;
    }

    writeln!(
        w,
        "  native / harness-specific (zirv reads these for drift detection only; it never \
         injects them itself -- see `zirv context sync`):"
    )?;
    if native.is_empty() {
        writeln!(w, "    (none found)")?;
    }
    for surface in &native {
        render_surface_line(w, surface, cfg, "    ")?;
    }

    let settings_count = surfaces
        .iter()
        .filter(|s| s.layer.kind() == Kind::PolicySettings)
        .count();
    if settings_count > 0 {
        writeln!(
            w,
            "  + {settings_count} settings surface(s) discovered (not size-flagged: structured \
             content, not prose)"
        )?;
    }
    Ok(())
}

/// "Duplicate/conflict counts from `drift::analyze`", tallied into a
/// `BTreeMap<&'static str, _>` keyed by finding kind so the per-kind
/// breakdown renders in a fixed, deterministic order regardless of how many
/// findings of each kind exist or what order `analyze` produced them in.
fn render_drift_section<W: Write>(w: &mut W, findings: &[Finding], verbose: bool) -> CtxResult<()> {
    writeln!(
        w,
        "\nduplicate / conflict findings (drift analysis over the surfaces above):"
    )?;
    if findings.is_empty() {
        writeln!(w, "  none found")?;
        return Ok(());
    }

    let warning_count = findings
        .iter()
        .filter(|f| f.severity == Severity::Warning)
        .count();
    let info_count = findings.len() - warning_count;
    writeln!(
        w,
        "  {} total ({warning_count} warning -- \"contradiction\" is the only warning-severity \
         kind --, {info_count} info)",
        findings.len()
    )?;

    let mut tally: std::collections::BTreeMap<&'static str, (Severity, usize)> =
        std::collections::BTreeMap::new();
    for finding in findings {
        let entry = tally.entry(finding.kind).or_insert((finding.severity, 0));
        entry.1 += 1;
    }
    for (kind, (severity, count)) in &tally {
        writeln!(w, "    {kind}: {count} ({})", severity.as_str())?;
    }

    if verbose {
        writeln!(w, "\n  findings (verbose):")?;
        for finding in findings {
            writeln!(w, "    [{}] {}", finding.severity.as_str(), finding.title)?;
            for evidence in &finding.evidence {
                writeln!(w, "        {evidence}")?;
            }
            writeln!(w, "        {}", finding.detail)?;
        }
    }
    Ok(())
}

/// "Selected memory entry count and injected memory size", via
/// `prompt::memory_injection_summary` -- the exact selection/rendering logic
/// a real launch uses, so this can never disagree with what a session would
/// actually receive.
fn render_memory_section<W: Write>(
    w: &mut W,
    state: &StateDir,
    repo: &Path,
    slug: &str,
    cfg: &CtxConfig,
) -> CtxResult<()> {
    let entries = memory::render_for_prompt(state, repo, slug, cfg);
    let summary = prompt::memory_injection_summary(&entries, cfg.memory.core_max_bytes);

    writeln!(w, "\nmemory bank:")?;
    if !cfg.memory.enabled {
        writeln!(
            w,
            "  disabled (memory.enabled = false): nothing is injected"
        )?;
    } else if summary.total_entries == 0 {
        writeln!(w, "  empty")?;
    } else {
        writeln!(
            w,
            "  {} entries total, {} selected within budget, {} bytes injected (~{} tok, est.), \
             {} omitted",
            summary.total_entries,
            summary.selected_entries,
            summary.injected_bytes,
            estimate_tokens(summary.injected_bytes),
            summary.omitted_entries
        )?;
    }
    writeln!(
        w,
        "  budget (memory.core_max_bytes): {} bytes",
        cfg.memory.core_max_bytes
    )?;
    Ok(())
}

/// "Session/handoff/mail contribution where applicable": mail's own half.
/// Reads pending mail non-destructively (`mail::list` with no
/// agent/session filter -- the same broad, idempotent view `zirv ctx
/// status`/`zirv ctx inbox --peek` already use), so running this report
/// never consumes a message a real session would otherwise have seen.
///
/// Issue #100 (2026-08-23): the one exception is a message whose
/// `To-session` names a session that no longer exists at all -- nothing
/// will ever read it, so `mail::sweep_undeliverable` moves it into `read/`
/// (the same move an ordinary read does) before the count below, and the
/// swept count is reported separately rather than folded into "pending".
/// This does not weaken the non-destructive promise above: it is cleanup of
/// mail no live session could ever have seen, not a read on any session's
/// behalf.
fn render_mail_section<W: Write>(
    w: &mut W,
    state: &StateDir,
    slug: &str,
    cfg: &CtxConfig,
) -> CtxResult<()> {
    writeln!(
        w,
        "\nmail (pending, read non-destructively -- this report never consumes a message):"
    )?;
    let swept = mail::sweep_undeliverable(state, slug);
    let messages = match mail::list(state, slug, None, None) {
        Ok(messages) => messages,
        Err(e) => {
            writeln!(w, "  (unreadable: {e})")?;
            return Ok(());
        }
    };
    if messages.is_empty() {
        if swept > 0 {
            writeln!(w, "  none pending ({swept} undeliverable, swept)")?;
        } else {
            writeln!(w, "  none pending")?;
        }
        return Ok(());
    }

    let bytes: usize = messages.iter().map(|(_, m)| m.body.len()).sum();
    let exceeds = bytes > cfg.mail.max_delivered_bytes;
    let swept_note = if swept > 0 {
        format!(" ({swept} undeliverable, swept)")
    } else {
        String::new()
    };
    writeln!(
        w,
        "  {} message(s) pending{swept_note}, {bytes} raw body bytes (~{} tok, est.) -- \
         mail.max_delivered_bytes budget: {} ({} budget)",
        messages.len(),
        estimate_tokens(bytes),
        cfg.mail.max_delivered_bytes,
        if exceeds { "exceeds" } else { "fits within" }
    )?;
    if !cfg.mail.enabled {
        writeln!(
            w,
            "  mail delivery is disabled (mail.enabled = false): these messages are stored but \
             will not be injected into any session"
        )?;
    }
    Ok(())
}

/// "Session/handoff/mail contribution where applicable": handoff's own
/// half. A handoff is only ever injected via `zirv ctx resume`, not by a
/// normal launch, so this is explicitly labeled conditional rather than
/// folded into the per-harness composed total below.
fn render_handoff_section<W: Write>(w: &mut W, state: &StateDir, repo: &Path) -> CtxResult<()> {
    writeln!(
        w,
        "\nhandoff (only injected via `zirv ctx resume`; not part of a normal launch):"
    )?;
    match handoff::latest_for_repo(state, repo) {
        Ok(Some((path, handoff))) => {
            let resume_bytes = resume::resume_prompt(&handoff).len();
            writeln!(
                w,
                "  latest: {} -- would inject {resume_bytes}B (~{} tok, est.) if resumed",
                path.display(),
                estimate_tokens(resume_bytes)
            )?;
        }
        Ok(None) => writeln!(w, "  none available")?,
        Err(e) => writeln!(w, "  (unreadable: {e})")?,
    }
    Ok(())
}

/// "Harness/orchestration contribution": the one layer that had no
/// configured budget before this issue (`context.max_harness_roster_bytes`,
/// added by this task -- see `config.rs`). Reads truncation straight off
/// `CompiledContext::harness_roster`, the same raw/delivered/truncated
/// provenance `prompt::compose` itself computes when it truncates this
/// layer (via `prompt::harness_roster_injection`) -- this section can never
/// disagree with what a real launch actually delivers, because it is
/// reading the same computation, not a second one.
fn render_harness_roster_section<W: Write>(
    w: &mut W,
    cfg: &CtxConfig,
    compiled_by_adapter: &[(&'static str, compile::CompiledContext)],
) -> CtxResult<()> {
    writeln!(
        w,
        "\nharness / orchestration layer (Orchestrator sessions only):"
    )?;
    writeln!(
        w,
        "  static harness-teaching text: {}B",
        prompt::HARNESS_PROMPT.len()
    )?;
    if !cfg.prompt.harnesses {
        writeln!(
            w,
            "  derived harness roster: disabled (prompt.harnesses = false)"
        )?;
        return Ok(());
    }

    for (name, compiled) in compiled_by_adapter {
        match &compiled.harness_roster {
            Some(roster) => {
                let flag = if roster.truncated { " [TRUNCATED]" } else { "" };
                writeln!(
                    w,
                    "  derived roster as seen by {name}: {}B raw, {}B delivered (~{} tok, \
                     est.){flag} -- budget (context.max_harness_roster_bytes): {}",
                    roster.raw_bytes,
                    roster.delivered_bytes,
                    estimate_tokens(roster.delivered_bytes),
                    cfg.context.max_harness_roster_bytes
                )?;
            }
            None => writeln!(w, "  derived roster as seen by {name}: none (empty roster)")?,
        }
    }
    Ok(())
}

/// "Total Zirv-managed context estimate" and "policy alignment per harness":
/// the two bullets `compile::compile`/`policy::evaluate` already build,
/// rendered per adapter (sorted by name for determinism) rather than
/// aggregated into one cross-harness figure, since the canonical
/// harness-specific file and the roster both differ per adapter -- a single
/// combined number would either double-count or silently pick one harness.
///
/// **Never starts a session**: `compile::compile` (already run once per
/// adapter by the caller, `run_with`) only reads config/files and calls
/// `adapter.name()`/`adapter.policy_support()`, both pure.
fn render_per_harness_section<W: Write>(
    w: &mut W,
    compiled_by_adapter: &[(&'static str, compile::CompiledContext)],
) -> CtxResult<()> {
    writeln!(
        w,
        "\nper-harness zirv-managed context (Orchestrator session; excludes mail, task text and \
         handoff, which are shown separately above and are only added at an actual launch):"
    )?;

    for (name, compiled) in compiled_by_adapter {
        writeln!(w, "\n  -- {name} --")?;
        match &compiled.composed {
            Some(composed) => {
                let bytes = composed.text.len();
                writeln!(
                    w,
                    "    total zirv-managed context estimate: {bytes}B (~{} tok, est.)",
                    estimate_tokens(bytes)
                )?;
            }
            None => writeln!(
                w,
                "    prompt injection is disabled (prompt.enabled = false)"
            )?,
        }

        if compiled.provenance.is_empty() {
            writeln!(w, "    canonical context (.zirv/context/): none found")?;
        }
        for provenance in &compiled.provenance {
            let flag = if provenance.truncated {
                " [TRUNCATED]"
            } else {
                ""
            };
            writeln!(
                w,
                "    canonical context: {} -- {}B raw, {}B delivered{flag}",
                provenance.surface.path().display(),
                provenance.raw_bytes,
                provenance.delivered_bytes
            )?;
        }

        writeln!(
            w,
            "    core memory: {} selected, {}B delivered, {} omitted",
            compiled.core_memory.selected_entries,
            compiled.core_memory.injected_bytes,
            compiled.core_memory.omitted_entries
        )?;
        writeln!(
            w,
            "    retrieved memory: {} selected, {}B delivered, {} omitted",
            compiled.retrieved_memory.selected_entries,
            compiled.retrieved_memory.injected_bytes,
            compiled.retrieved_memory.omitted_entries
        )?;

        writeln!(
            w,
            "    policy alignment (no false equivalence across harnesses):"
        )?;
        for line in compiled.policy.render().lines() {
            writeln!(w, "      {line}")?;
        }
    }
    Ok(())
}

/// "Every configured hard budget with whether it truncated or omitted
/// anything" -- the summary line. Per-surface/per-layer truncation detail is
/// already shown inline in the sections above (canonical context and harness
/// roster provenance, memory's omitted count, mail's exceeds/fits
/// comparison); this section is the flat reference list an operator can
/// scan without re-reading every section.
fn render_budgets_section<W: Write>(w: &mut W, cfg: &CtxConfig) -> CtxResult<()> {
    writeln!(w, "\nconfigured hard budgets:")?;
    writeln!(
        w,
        "  prompt.max_repo_bytes = {} (repo <.zirv/system-prompt.md> layer)",
        cfg.prompt.max_repo_bytes
    )?;
    writeln!(
        w,
        "  context.max_common_bytes = {} (.zirv/context/common.md)",
        cfg.context.max_common_bytes
    )?;
    writeln!(
        w,
        "  context.max_harness_bytes = {} (.zirv/context/claude.md or codex.md)",
        cfg.context.max_harness_bytes
    )?;
    writeln!(
        w,
        "  context.max_harness_roster_bytes = {} (derived harness roster, truncated at compose \
         time the same way memory and canonical context are)",
        cfg.context.max_harness_roster_bytes
    )?;
    writeln!(
        w,
        "  mail.max_delivered_bytes = {} (pending mail, at actual delivery)",
        cfg.mail.max_delivered_bytes
    )?;
    writeln!(
        w,
        "  memory.core_max_bytes = {} (always-present core memory)",
        cfg.memory.core_max_bytes
    )?;
    writeln!(
        w,
        "  memory.retrieval_max_bytes = {} (context-ranked memory)",
        cfg.memory.retrieval_max_bytes
    )?;
    writeln!(
        w,
        "  memory.retrieval_max_entries = {} (context-ranked memory entry cap)",
        cfg.memory.retrieval_max_entries
    )?;
    Ok(())
}

/// The honesty disclosure the issue requires verbatim: never implies zirv
/// knows a harness's full context. The bare phrase "total context" never
/// appears anywhere in this report -- every total figure is spelled "total
/// zirv-managed context", always qualified. See
/// `unknown_vendor_context_is_disclosed_and_no_total_context_claim_is_made`.
fn render_unknown_context_section<W: Write>(w: &mut W) -> CtxResult<()> {
    writeln!(
        w,
        "\nwhat zirv cannot see:\n  Every figure in this report measures or estimates only \
         zirv-managed context: the surfaces, memory, mail, handoff and harness layers listed \
         above. Each harness's own hidden system prompt and built-in tool/context scaffolding \
         are UNKNOWN to zirv and are never folded into any number above. This report never \
         states a single combined figure for everything a harness sees -- only for the \
         zirv-managed portion of it."
    )?;
    Ok(())
}

fn render_token_estimate_note<W: Write>(w: &mut W) -> CtxResult<()> {
    writeln!(
        w,
        "\ntoken estimate method:\n  Every \"~N tok (est.)\" figure above is bytes / \
         {BYTES_PER_TOKEN} rounded up -- a rough heuristic for English prose and source code, \
         not a real tokenizer (zirv does not depend on one). Byte counts are measured directly \
         from disk or state; token counts are always estimates, and real tokenization varies by \
         vendor, model, language and content type (code vs. prose vs. non-ASCII), so actual \
         counts can differ meaningfully from this estimate."
    )?;
    Ok(())
}

pub fn run_with<W: Write>(
    args: &StatusArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
    now: u64,
) -> CtxResult<i32> {
    let home = crate::utils::home_dir().ok();
    let state = StateDir::resolve(env)?;
    let cfg = CtxConfig::load(repo, env)?;
    let slug = repo_slug(repo);

    writeln!(
        w,
        "zirv context status -- read-only: starts no session, spawns no agent binary, makes no \
         network call\n"
    )?;

    let surfaces =
        optimize::collect_surfaces(home.as_deref(), repo, cfg.optimize.max_surface_bytes);
    render_instruction_surfaces(w, &surfaces, &cfg.context)?;

    let findings = drift::analyze(&context_cli::surfaces_for_drift(&surfaces));
    render_drift_section(w, &findings, args.verbose)?;

    render_memory_section(w, &state, repo, &slug, &cfg)?;
    render_mail_section(w, &state, &slug, &cfg)?;
    render_handoff_section(w, &state, repo)?;

    // Computed once, here, and shared by both sections below: the harness
    // roster section and the per-harness section both need `compile::
    // compile`'s output per adapter, and computing it twice would risk the
    // two sections silently disagreeing about the same truncation.
    let mut registered = adapters::all(None);
    registered.sort_by_key(|a| a.name());
    let compiled_by_adapter: Vec<(&'static str, compile::CompiledContext)> = registered
        .iter()
        .map(|adapter| {
            let compiled = compile::compile(
                home.as_deref(),
                repo,
                false,
                &cfg,
                adapter.as_ref(),
                PromptRole::Orchestrator,
                &state,
                now,
            );
            (adapter.name(), compiled)
        })
        .collect();

    render_harness_roster_section(w, &cfg, &compiled_by_adapter)?;
    render_per_harness_section(w, &compiled_by_adapter)?;
    render_budgets_section(w, &cfg)?;
    render_unknown_context_section(w)?;
    render_token_estimate_note(w)?;

    Ok(0)
}

pub fn run<W: Write>(args: &StatusArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    run_with(args, w, &repo, &env, now_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use crate::commands::ctx::state::STATE_ENV;

    fn env_map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    struct Fixture {
        _repo_dir: tempfile::TempDir,
        _state_dir: tempfile::TempDir,
        // Isolates HOME/USERPROFILE for the fixture's lifetime: without this
        // `crate::utils::home_dir()` (read directly from process env, not
        // through the `env` closure below) resolves to the *real* operator's
        // home directory, and this report would read their actual global
        // CLAUDE.md/settings.json into every test's output -- nondeterministic
        // across machines and a privacy leak in a test run besides.
        _home: crate::commands::ctx::testenv::HomeGuard,
        _home_dir: tempfile::TempDir,
        repo: std::path::PathBuf,
        env: HashMap<String, String>,
    }

    impl Fixture {
        fn new() -> Self {
            let repo_dir = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir_all(repo_dir.path().join(".zirv/context")).expect("mkdir");
            let state_dir = tempfile::tempdir().expect("tempdir");
            let home_dir = tempfile::tempdir().expect("tempdir");
            let home = crate::commands::ctx::testenv::HomeGuard::set(home_dir.path());
            let env = env_map(&[(STATE_ENV, state_dir.path().to_str().expect("utf8 path"))]);
            let repo = repo_dir.path().to_path_buf();
            Self {
                _repo_dir: repo_dir,
                _state_dir: state_dir,
                _home: home,
                _home_dir: home_dir,
                repo,
                env,
            }
        }

        fn write_canonical(&self, name: &str, text: &str) {
            std::fs::write(self.repo.join(".zirv/context").join(name), text).expect("write");
        }

        fn run(&self, args: &StatusArgs) -> (i32, String) {
            self.run_at(args, 1_700_000_000)
        }

        fn run_at(&self, args: &StatusArgs, now: u64) -> (i32, String) {
            let mut out = Vec::new();
            let code = run_with(
                args,
                &mut out,
                &self.repo,
                &|k| self.env.get(k).cloned(),
                now,
            )
            .expect("run_with");
            (code, String::from_utf8(out).expect("utf8"))
        }
    }

    fn default_args() -> StatusArgs {
        StatusArgs { verbose: false }
    }

    #[test]
    fn status_runs_end_to_end_and_exits_zero() {
        let fixture = Fixture::new();
        let (code, out) = fixture.run(&default_args());
        assert_eq!(code, 0);
        assert!(out.contains("zirv context status"));
    }

    /// The requirement stated most literally in the issue: status must not
    /// start an AI session or spawn the configured agent binary. Proven with
    /// a REAL executable (not merely an absent path): if anything on this
    /// report's path actually ran it, the marker file would exist afterward.
    #[test]
    fn status_never_spawns_the_configured_agent_binary() {
        let fixture = Fixture::new();
        let marker = fixture.repo.join("marker.txt");
        let script_ext = if cfg!(windows) { "cmd" } else { "sh" };
        let fake_bin = fixture.repo.join(format!("fake-agent.{script_ext}"));
        let script = if cfg!(windows) {
            format!("@echo off\r\necho ran > \"{}\"\r\n", marker.display())
        } else {
            format!("#!/bin/sh\necho ran > \"{}\"\n", marker.display())
        };
        std::fs::write(&fake_bin, script).expect("write fake bin");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&fake_bin).expect("meta").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&fake_bin, perms).expect("chmod");
        }

        let mut env = fixture.env.clone();
        env.insert(
            "ZIRV_CTX_AGENT_BIN".to_string(),
            fake_bin.to_str().expect("utf8 path").to_string(),
        );

        let mut out = Vec::new();
        let code = run_with(
            &default_args(),
            &mut out,
            &fixture.repo,
            &|k| env.get(k).cloned(),
            1_700_000_000,
        )
        .expect("run_with");

        assert_eq!(code, 0);
        assert!(
            !marker.exists(),
            "the fake agent binary must never actually run"
        );
    }

    #[test]
    fn bytes_and_estimated_tokens_are_distinguished_and_labeled() {
        let fixture = Fixture::new();
        fixture.write_canonical("common.md", &"word ".repeat(50));
        let (_, out) = fixture.run(&default_args());

        assert!(out.contains('B'), "byte counts are shown: {out}");
        assert!(
            out.contains("tok (est.)"),
            "token counts are explicitly labeled as estimates: {out}"
        );
        assert!(
            out.to_lowercase().contains("not a real tokenizer"),
            "the estimation method must disclose it is not a real tokenizer: {out}"
        );
        assert!(
            out.contains("bytes / 4"),
            "the divisor itself is stated: {out}"
        );
    }

    #[test]
    fn unknown_vendor_context_is_disclosed_and_no_total_context_claim_is_made() {
        let fixture = Fixture::new();
        let (_, out) = fixture.run(&default_args());

        let lower = out.to_lowercase();
        assert!(
            lower.contains("unknown to zirv"),
            "must disclose vendor-hidden context is unknown: {out}"
        );
        assert!(
            lower.contains("hidden system prompt"),
            "must name what is unknown: {out}"
        );
        assert!(
            !lower.contains("total context"),
            "must never claim a bare total-context figure: {out}"
        );
        assert!(
            lower.contains("total zirv-managed context estimate"),
            "the zirv-managed total must be explicitly qualified: {out}"
        );
    }

    #[test]
    fn duplicate_and_conflict_counts_come_through_from_drift_analysis() {
        let fixture = Fixture::new();
        fixture.write_canonical("common.md", "- always run the full test suite\n");
        std::fs::write(
            fixture.repo.join("CLAUDE.md"),
            "- always run the full test suite\n",
        )
        .expect("write");

        let (_, out) = fixture.run(&default_args());
        assert!(
            out.contains("duplicate-redundant-with-canonical"),
            "got {out}"
        );
        assert!(
            out.contains("duplicate / conflict findings"),
            "the section header names what this is: {out}"
        );
    }

    /// Issue #105: a native `CLAUDE.md` that is itself zirv-managed
    /// (rendered verbatim from `.zirv/context/` by `zirv context sync
    /// --generate`) is a tautological "duplicate" of the canonical layer it
    /// was rendered from -- pairing it against `common.md` in a drift
    /// finding is noise, not real drift.
    #[test]
    fn a_zirv_managed_native_claude_md_produces_no_duplicate_or_precedence_findings() {
        let fixture = Fixture::new();
        fixture.write_canonical("common.md", "- always run the full test suite\n");
        std::fs::write(
            fixture.repo.join("CLAUDE.md"),
            format!(
                "{}\n\n- always run the full test suite\n",
                crate::commands::ctx::context_cli::MANAGED_MARKER
            ),
        )
        .expect("write");

        let (_, out) = fixture.run(&default_args());
        assert!(
            !out.contains("duplicate-redundant-with-canonical"),
            "a zirv-managed native file must not be diffed against the canonical layer it was \
             rendered from: {out}"
        );
        assert!(
            !out.contains("precedence-shadowing"),
            "nor treated as a precedence conflict with it: {out}"
        );
        assert!(out.contains("duplicate / conflict findings"), "got {out}");
        assert!(out.contains("  none found"), "got {out}");
    }

    /// Companion to the test above: the SAME content, minus the managed
    /// marker, is a real hand-authored duplicate and must still be flagged
    /// -- proves the exclusion is keyed on the marker, not merely on the
    /// path being `CLAUDE.md`.
    #[test]
    fn the_same_content_without_the_managed_marker_still_gets_the_duplicate_finding() {
        let fixture = Fixture::new();
        fixture.write_canonical("common.md", "- always run the full test suite\n");
        std::fs::write(
            fixture.repo.join("CLAUDE.md"),
            "- always run the full test suite\n",
        )
        .expect("write");

        let (_, out) = fixture.run(&default_args());
        assert!(
            out.contains("duplicate-redundant-with-canonical"),
            "an unmanaged native file duplicating canonical content is still real drift: {out}"
        );
    }

    /// The exclusion narrows the drift *analysis* only -- the surfaces
    /// listing (sizes/budgets) must still show the managed file, now with a
    /// note explaining why it never appears as a duplicate above.
    #[test]
    fn the_surfaces_section_still_lists_a_managed_native_file_with_its_note() {
        let fixture = Fixture::new();
        fixture.write_canonical("common.md", "- always run the full test suite\n");
        std::fs::write(
            fixture.repo.join("CLAUDE.md"),
            format!(
                "{}\n\n- always run the full test suite\n",
                crate::commands::ctx::context_cli::MANAGED_MARKER
            ),
        )
        .expect("write");

        let (_, out) = fixture.run(&default_args());
        assert!(
            out.contains("CLAUDE.md")
                && out.contains("(zirv-managed, rendered from .zirv/context/)"),
            "the managed native file must still be listed, with its note: {out}"
        );
    }

    #[test]
    fn an_oversized_canonical_surface_is_flagged() {
        let fixture = Fixture::new();
        fixture.write_canonical("common.md", &"x".repeat(10_000));
        let (_, out) = fixture.run(&default_args());
        assert!(
            out.contains("[OVERSIZED]"),
            "a common.md far over the 4096-byte default budget must be flagged: {out}"
        );
    }

    #[test]
    fn an_oversized_native_claude_md_is_flagged_even_with_no_canonical_budget_of_its_own() {
        let fixture = Fixture::new();
        std::fs::write(fixture.repo.join("CLAUDE.md"), "x".repeat(10_000)).expect("write");
        let (_, out) = fixture.run(&default_args());
        assert!(
            out.contains("[OVERSIZED]"),
            "an oversized native CLAUDE.md must be visible too: {out}"
        );
    }

    #[test]
    fn a_small_surface_is_never_flagged_oversized() {
        let fixture = Fixture::new();
        fixture.write_canonical("common.md", "short and sweet");
        let (_, out) = fixture.run(&default_args());
        assert!(!out.contains("[OVERSIZED]"), "got {out}");
    }

    /// The false-equivalence guard: claude and codex must not render the
    /// same policy support state for a capability where the real answer
    /// differs (issue #43's own findings: claude enforces repo_fs_write via
    /// a verified tool pin, codex can only degrade it via its sandbox flag).
    #[test]
    fn policy_alignment_never_shows_false_equivalence_between_harnesses() {
        let fixture = Fixture::new();
        std::fs::write(
            fixture.repo.join(".zirv/ctx.toml"),
            "[policy]\nrepo_fs_write = \"deny\"\n",
        )
        .expect("write");

        let (_, out) = fixture.run(&default_args());
        let claude_start = out.find("-- claude --").expect("claude section present");
        let codex_start = out.find("-- codex --").expect("codex section present");
        let (claude_section, codex_section) = if claude_start < codex_start {
            (&out[claude_start..codex_start], &out[codex_start..])
        } else {
            (&out[claude_start..], &out[codex_start..claude_start])
        };

        assert!(
            claude_section.contains("repository filesystem writes: deny -- enforced"),
            "claude section: {claude_section}"
        );
        assert!(
            codex_section.contains("repository filesystem writes: deny -- degraded"),
            "codex section: {codex_section}"
        );
        assert!(
            !codex_section.contains("repository filesystem writes: deny -- enforced ("),
            "codex must never claim full enforcement: {codex_section}"
        );
    }

    #[test]
    fn budgets_are_reported_with_their_configured_values() {
        let fixture = Fixture::new();
        let (_, out) = fixture.run(&default_args());
        assert!(out.contains("prompt.max_repo_bytes = 4096"), "got {out}");
        assert!(out.contains("context.max_common_bytes = 4096"), "got {out}");
        assert!(
            out.contains("context.max_harness_bytes = 4096"),
            "got {out}"
        );
        assert!(
            out.contains("context.max_harness_roster_bytes = 4096"),
            "the new harness/orchestration budget must be reported too: {out}"
        );
        assert!(out.contains("mail.max_delivered_bytes = 4096"), "got {out}");
        assert!(out.contains("memory.core_max_bytes = 2048"), "got {out}");
        assert!(
            out.contains("memory.retrieval_max_bytes = 2048"),
            "got {out}"
        );
        assert!(
            out.contains("memory.retrieval_max_entries = 6"),
            "got {out}"
        );
    }

    /// Issue #46 follow-up: `context.max_harness_roster_bytes` is a real,
    /// enforced budget, and the report must show when it actually bit --
    /// not merely compare against it for visibility. `REPO_FORBIDDEN`
    /// (like every other budget key), so the override goes through the
    /// environment, the same way an operator would actually set it.
    #[test]
    fn an_over_budget_harness_roster_is_flagged_truncated_in_the_report() {
        let fixture = Fixture::new();
        let mut env = fixture.env.clone();
        env.insert(
            "ZIRV_CTX_CONTEXT_MAX_HARNESS_ROSTER_BYTES".to_string(),
            "5".to_string(),
        );

        let mut out = Vec::new();
        let code = run_with(
            &default_args(),
            &mut out,
            &fixture.repo,
            &|k| env.get(k).cloned(),
            1_700_000_000,
        )
        .expect("run_with");
        let out = String::from_utf8(out).expect("utf8");

        assert_eq!(code, 0);
        assert!(
            out.contains("context.max_harness_roster_bytes = 5"),
            "got {out}"
        );
        assert!(
            out.contains("[TRUNCATED]"),
            "an over-budget harness roster must be flagged truncated, not just measured: {out}"
        );
        assert!(
            !out.to_lowercase().contains("not yet enforced"),
            "the budget is real now, so the report must not disclaim it as unenforced: {out}"
        );
    }

    #[test]
    fn verbose_adds_full_finding_evidence_the_default_report_omits() {
        let fixture = Fixture::new();
        fixture.write_canonical("common.md", "- always run the full test suite\n");
        std::fs::write(
            fixture.repo.join("CLAUDE.md"),
            "- always run the full test suite\n",
        )
        .expect("write");

        let (_, compact) = fixture.run(&default_args());
        let (_, verbose) = fixture.run(&StatusArgs { verbose: true });

        assert!(
            !compact.contains("findings (verbose)"),
            "default output stays compact: {compact}"
        );
        assert!(verbose.contains("findings (verbose)"), "got {verbose}");
        assert!(
            verbose.len() > compact.len(),
            "verbose must add strictly more detail"
        );
    }

    #[test]
    fn output_is_deterministic_for_identical_inputs() {
        let fixture = Fixture::new();
        fixture.write_canonical("common.md", "- one canonical rule\n");
        fixture.write_canonical("claude.md", "- claude-only addition\n");
        std::fs::write(fixture.repo.join("CLAUDE.md"), "- a native rule\n").expect("write");
        std::fs::write(fixture.repo.join("AGENTS.md"), "- another native rule\n").expect("write");

        let (_, first) = fixture.run_at(&default_args(), 1_700_000_000);
        let (_, second) = fixture.run_at(&default_args(), 1_700_000_000);
        assert_eq!(first, second);
    }

    #[test]
    fn mail_contribution_is_reported_without_consuming_it() {
        let fixture = Fixture::new();
        let state = StateDir::resolve(&|k| fixture.env.get(k).cloned()).expect("state");
        let slug = repo_slug(&fixture.repo);
        let message = mail::Message {
            from_session: "sess1".to_string(),
            from_agent: "claude".to_string(),
            to: "any".to_string(),
            to_session: None,
            sent: 1_700_000_000,
            body: "a note for whoever picks this up".to_string(),
        };
        mail::store(&state, &slug, &message, &CtxConfig::default()).expect("store mail");

        let before = mail::list(&state, &slug, None, None).expect("list before");
        assert_eq!(before.len(), 1);

        let (_, out) = fixture.run(&default_args());
        assert!(out.contains("1 message(s) pending"), "got {out}");

        let after = mail::list(&state, &slug, None, None).expect("list after");
        assert_eq!(
            after.len(),
            1,
            "the report must never consume the mail it reports on"
        );
    }

    /// Issue #100 (2026-08-23): a message addressed to a session that no
    /// longer exists is swept out of "pending" and reported separately --
    /// this is cleanup of mail no live session could ever read, distinct
    /// from the non-destructive promise the test above pins for ordinary
    /// mail.
    #[test]
    fn mail_addressed_to_a_dead_session_is_swept_and_reported_separately() {
        let fixture = Fixture::new();
        let state = StateDir::resolve(&|k| fixture.env.get(k).cloned()).expect("state");
        let slug = repo_slug(&fixture.repo);
        let dead = mail::Message {
            from_session: "sess1".to_string(),
            from_agent: "claude".to_string(),
            to: "any".to_string(),
            to_session: Some("deadbeef".to_string()),
            sent: 1_700_000_000,
            body: "nobody will ever read this".to_string(),
        };
        let pending = mail::Message {
            from_session: "sess2".to_string(),
            from_agent: "claude".to_string(),
            to: "any".to_string(),
            to_session: None,
            sent: 1_700_000_100,
            body: "still pending".to_string(),
        };
        mail::store(&state, &slug, &dead, &CtxConfig::default()).expect("store dead");
        mail::store(&state, &slug, &pending, &CtxConfig::default()).expect("store pending");

        let (_, out) = fixture.run(&default_args());
        assert!(
            out.contains("1 message(s) pending (1 undeliverable, swept)"),
            "got {out}"
        );

        let after = mail::list(&state, &slug, None, None).expect("list after");
        assert_eq!(
            after.len(),
            1,
            "the swept message is gone; the still-pending one remains"
        );
    }
}
