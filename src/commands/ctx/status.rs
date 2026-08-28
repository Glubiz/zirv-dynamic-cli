use std::io::Write;
use std::path::Path;

use super::adapters::{self, AGENT_ENV, DefaultOrigin};
use super::config::{CtxConfig, EnvLookup, env_from_process};
use super::group;
use super::handoff::latest_for_repo;
use super::mail;
use super::permit;
use super::sessions::{self, Liveness};
use super::state::{StateDir, repo_slug};
use super::{CtxResult, log};

/// One unit, whichever is largest without going to zero: seconds under a
/// minute, then minutes, hours, days. A session registry entry's age is
/// usually minutes to days old, never sub-second, so this deliberately does
/// not go finer than seconds.
fn format_age(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3600)
    } else {
        format!("{}d", seconds / 86_400)
    }
}

/// Issue #139: whether `record`'s pinned launch-time safety-policy
/// fingerprint (`sessions::Record::safety_policy_sha256`) differs from the
/// fingerprint of the policy `record.repo` resolves to RIGHT NOW. Degrades
/// silently (`false`) on any doubt at all -- no fingerprint was ever
/// recorded (an older build, a launch that never attempted attestation, or
/// one this field is not yet threaded through to), the repo's own config
/// fails to load, or fingerprinting itself fails -- because a diagnostic
/// line must never be the reason `zirv ctx status` looks broken. `status.rs`
/// keeps this one bit of policy I/O at the edge: [`safety::policy_
/// fingerprint`] itself stays pure, and this function's only job is calling
/// it and comparing.
fn policy_snapshot_is_stale(record: &sessions::Record, env: EnvLookup<'_>) -> bool {
    let Some(stored) = record.safety_policy_sha256.as_deref() else {
        return false;
    };
    let Ok(cfg) = CtxConfig::load(&record.repo, env) else {
        return false;
    };
    let Ok(current) = super::safety::policy_fingerprint(&cfg.safety) else {
        return false;
    };
    stored != current
}

/// N7: one line per registry record (`<short> <agent> <verb> pid <pid> <age>
/// live|unreachable|dead <repo_slug>`), plus one line for any `s/*.sock` file that has
/// no matching registry record -- an older zirv binary that predates the
/// registry still wrote sockets, and a mixed-version machine must not make
/// those supervisors disappear from `status` entirely, just less detailed.
///
/// Takes an already-fetched `sessions::list` result rather than calling
/// `list` itself: `list` sweeps a stale record's file from disk as a side
/// effect of being called at all, so a caller that needs the registry for
/// more than one purpose this pass (the heavy-worker count in `run_with`,
/// below) must fetch it exactly once and share the result, or a second call
/// would find that record's file already gone.
fn sessions_lines(
    records: &[(sessions::Record, Liveness)],
    state: &StateDir,
    now: u64,
    env: EnvLookup<'_>,
) -> Vec<String> {
    let mut records = records.to_vec();
    records.sort_by(|a, b| a.0.short.cmp(&b.0.short));

    let known: std::collections::BTreeSet<String> = records
        .iter()
        .map(|(record, _)| record.short.clone())
        .collect();

    let mut lines: Vec<String> = records
        .iter()
        .map(|(record, liveness)| {
            let mut line = format!(
                "  {}  {}  {}  pid {}  {}  {}  {}",
                record.short,
                record.agent,
                record.verb,
                record.pid,
                format_age(now.saturating_sub(record.started_at)),
                // NEW-3: `unreachable` is a third state, not a flavour of
                // live: the process is running, but it bound no turn-signal
                // socket, so it can never notice a `zirv ctx nudge`. Showing
                // it as plain `live` invited an operator to nudge something
                // that would silently ignore them. A record whose pid no
                // longer exists (`Liveness::Stale`; swept from disk as a
                // side effect of this very listing, see `sessions::list`'s
                // own doc comment) still reports `dead`, unambiguously --
                // issue #166 -- rather than `stale`, which read as one more
                // shade of live: being gone outranks being unreachable.
                match (liveness, record.reachable) {
                    (Liveness::Stale, _) => "dead",
                    (Liveness::Live, true) => "live",
                    (Liveness::Live, false) => "unreachable",
                },
                record.repo_slug,
            );
            // Issue #139: named here, not just silently folded into the
            // stricter verdict a hook prompt would show -- an operator
            // reading `status` has no other way to learn a live session is
            // running a narrower policy than the repo currently resolves
            // to.
            if policy_snapshot_is_stale(record, env) {
                line.push_str(
                    "  policy snapshot stale (current policy is wider); relaunch to adopt",
                );
            }
            let delivery = mail::session_delivery_metrics(state, &record.short, now);
            line.push_str(&format!(
                "  mail queue {} unread {} recent in:{} out:{}",
                delivery.queued, delivery.unread, delivery.recent_in, delivery.recent_out
            ));
            line
        })
        .collect();

    let mut orphan_sockets: Vec<String> = std::fs::read_dir(state.sockets())
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("sock"))
                .filter_map(|e| {
                    e.path()
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                })
                .filter(|short| !known.contains(short))
                .collect()
        })
        .unwrap_or_default();
    orphan_sockets.sort();
    lines.extend(
        orphan_sockets
            .into_iter()
            .map(|short| format!("  {short}  (no record)")),
    );

    lines
}

/// The header line for one work-group block: scope/limits/budget/deadline
/// when `group` is a record this listing actually loaded (`group::list`), or
/// just the bare `fallback_id` when it is not -- a delegation naming an
/// unknown or never-written group id must still be shown, not dropped.
///
/// Issue #170: `live_shorts` -- built by `group_tree_lines` from its own
/// `records` parameter, keyed by SHORT id (`sessions::Record::short`), the
/// same address `WorkGroup::sub_orchestrator_session` is stored as -- is
/// reused here to answer the liveness question `group::is_abandoned` needs,
/// so `group_header` stays as pure as `group_tree_lines` itself (no
/// fs/clock/env of its own). Deliberately NOT `group_tree_lines`' own
/// `live_sessions` set, which is keyed by the FULL session id
/// `DelegationRow::session` carries instead.
fn group_header(
    group: Option<&group::WorkGroup>,
    fallback_id: &str,
    live_shorts: &std::collections::BTreeSet<&str>,
) -> String {
    let Some(wg) = group else {
        return format!("  {fallback_id}");
    };
    let status = if wg.closed_at.is_some() {
        "closed"
    } else {
        "open"
    };
    let mut header = format!(
        "  {} [{status}] scope=\"{}\" child_limit={}",
        wg.work_group_id, wg.scope, wg.child_limit
    );
    if let Some(budget) = wg.token_budget {
        header.push_str(&format!(" budget={budget}"));
    }
    if let Some(deadline) = wg.deadline_secs {
        header.push_str(&format!(" deadline={deadline}s"));
    }
    if let Some(sub) = &wg.sub_orchestrator_session {
        header.push_str(&format!(" sub-orchestrator={sub}"));
    }
    let claimant_alive = wg
        .sub_orchestrator_session
        .as_deref()
        .is_some_and(|s| live_shorts.contains(s));
    if group::is_abandoned(wg, claimant_alive) {
        header.push_str(" ABANDONED");
    }
    header
}

/// Appends one block to `lines`: `header`, then one indented line per
/// delegation in `children` (its session id, agent, model, the four raw
/// token classes -- same `"input {} | cache_creation {} | cache_read {} |
/// output {}"` phrasing `usage::render_sessions` already uses for these same
/// four classes -- wall time, and outcome), then a per-group total. A
/// delegation whose session is still `Liveness::Live` shows `running`
/// instead of its logged `outcome`, which is a snapshot from before the
/// session finished.
fn push_group_block(
    lines: &mut Vec<String>,
    header: String,
    children: &[&log::DelegationRow],
    live_sessions: &std::collections::BTreeSet<&str>,
) {
    lines.push(header);

    let mut totals = [0u64; 4];
    for row in children {
        let outcome = if live_sessions.contains(row.session.as_str()) {
            "running"
        } else {
            row.outcome.as_str()
        };
        let model = row
            .model
            .as_deref()
            .map(|m| format!(" ({m})"))
            .unwrap_or_default();
        lines.push(format!(
            "    {}  {}{model}  input {} | cache_creation {} | cache_read {} | output {}  wall {}  {outcome}",
            row.session,
            row.agent,
            row.input_tokens,
            row.cache_creation_input_tokens,
            row.cache_read_input_tokens,
            row.output_tokens,
            format_age(row.wall_ms / 1000),
        ));
        totals[0] = totals[0].saturating_add(row.input_tokens);
        totals[1] = totals[1].saturating_add(row.cache_creation_input_tokens);
        totals[2] = totals[2].saturating_add(row.cache_read_input_tokens);
        totals[3] = totals[3].saturating_add(row.output_tokens);
    }

    lines.push(format!(
        "    total: input {} | cache_creation {} | cache_read {} | output {}",
        totals[0], totals[1], totals[2], totals[3]
    ));
}

/// Issue #155, Phase 5(f): the work-group tree -- what each delegated child
/// has cost so far, which is the question "was delegating cheaper than doing
/// it here" reduces to. PURE (no fs/clock/env), so it is tested without a
/// state directory; `status::run_with` supplies its three inputs from
/// `group::list`, `log::read_delegations` and the same `sessions::list`
/// result it already fetched once for the sessions section below.
///
/// One block per group that actually has at least one delegation (`group::
/// list`'s own newest-first order) -- a group with none is not shown, same
/// "nothing to show is nothing shown" rule the rest of `status` follows.
/// Then one block per group id a delegation names that this listing never
/// loaded (the group's own file was never written, or has since been swept)
/// -- still shown, headed by the bare id, never silently dropped. Then,
/// last, every delegation naming no group at all, under a final "ungrouped"
/// heading -- a one-off delegation is still spend.
pub fn group_tree_lines(
    groups: &[group::WorkGroup],
    delegations: &[log::DelegationRow],
    records: &[(sessions::Record, Liveness)],
) -> Vec<String> {
    if delegations.is_empty() {
        return Vec::new();
    }

    let live_sessions: std::collections::BTreeSet<&str> = records
        .iter()
        .filter(|(_, liveness)| *liveness == Liveness::Live)
        .map(|(record, _)| record.session.as_str())
        .collect();
    // Issue #170: `WorkGroup::sub_orchestrator_session` is a SHORT id (the
    // same address `parent_session`/`SpawnRequest::parent_session` already
    // use throughout this codebase), unlike `live_sessions` above which is
    // keyed by the FULL session id `DelegationRow::session` carries -- a
    // second set, keyed the other way, so `group_header`'s abandoned check
    // asks the right question of the right key space.
    let live_shorts: std::collections::BTreeSet<&str> = records
        .iter()
        .filter(|(_, liveness)| *liveness == Liveness::Live)
        .map(|(record, _)| record.short.as_str())
        .collect();

    let mut lines = Vec::new();
    let mut seen: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();

    for wg in groups {
        let children: Vec<&log::DelegationRow> = delegations
            .iter()
            .filter(|d| d.work_group_id.as_deref() == Some(wg.work_group_id.as_str()))
            .collect();
        if children.is_empty() {
            continue;
        }
        seen.insert(wg.work_group_id.as_str());
        push_group_block(
            &mut lines,
            group_header(Some(wg), "", &live_shorts),
            &children,
            &live_sessions,
        );
    }

    let mut orphan_ids: Vec<&str> = delegations
        .iter()
        .filter_map(|d| d.work_group_id.as_deref())
        .filter(|id| !seen.contains(id))
        .collect();
    orphan_ids.sort_unstable();
    orphan_ids.dedup();
    for id in orphan_ids {
        let children: Vec<&log::DelegationRow> = delegations
            .iter()
            .filter(|d| d.work_group_id.as_deref() == Some(id))
            .collect();
        push_group_block(
            &mut lines,
            group_header(None, id, &live_shorts),
            &children,
            &live_sessions,
        );
    }

    let ungrouped: Vec<&log::DelegationRow> = delegations
        .iter()
        .filter(|d| d.work_group_id.is_none())
        .collect();
    if !ungrouped.is_empty() {
        push_group_block(
            &mut lines,
            group_header(None, "ungrouped", &live_shorts),
            &ungrouped,
            &live_sessions,
        );
    }

    lines
}

/// The `chat:` status line: the adapter `zirv ctx chat` would launch and the
/// rule that picked it (`adapters::resolve_default`'s own `DefaultOrigin`),
/// or -- degrading rather than failing the whole command -- a summary of why
/// nothing qualifies. `resolve_default`'s own error already names each
/// candidate adapter and its reason, one per line; those are joined with
/// "; " here (dropping the "no agent is both enabled and ready:" summary
/// line) so the status line stays on one row like every other line `status`
/// prints, instead of splitting a single logical fact across several.
fn describe_chat(cfg: &CtxConfig) -> String {
    match adapters::resolve_default(cfg) {
        Ok((adapter, origin)) => {
            let rule = match origin {
                DefaultOrigin::Configured => "configured",
                DefaultOrigin::FirstEnabledReady => "first enabled and ready",
            };
            format!("chat: {} ({rule})", adapter.name())
        }
        Err(e) => {
            let full = e.to_string();
            let reasons: Vec<&str> = full.lines().skip(1).collect();
            let detail = if reasons.is_empty() {
                full.clone()
            } else {
                reasons.join("; ")
            };
            format!("chat: unavailable ({detail})")
        }
    }
}

/// Issue #85: on a launch shape that cannot safely carry an adapter's own
/// system-prompt injection argv (the Windows `cmd.exe /c <shim>` form an
/// npm-installed `codex.cmd` resolves to -- `CodexAdapter::system_prompt_
/// supported` narrows the answer for exactly this shape), zirv falls back
/// to folding the composed session context onto the task-prompt text
/// itself (`prompt::task_prompt_with_composed_fallback`). That fallback is
/// silent otherwise: the operator gets a materially weaker session -- text
/// a model can drift from mid-session, instead of the harness's own
/// authoritative-instructions channel -- with nothing telling them why.
/// This surfaces it as a persistent status line rather than only a
/// transient `zirv ▸` announcement (`prompt::injection_event`, emitted at
/// each launch decision).
///
/// General over any adapter, not codex-specific: the condition is "this
/// adapter has a verified injection mechanism in general
/// (`capabilities().system_prompt`) but not for the launch shape it would
/// actually use right now (`system_prompt_supported(&[])`)" -- exactly
/// `injection_event`'s own `(Some(_), false)` branch, minus needing a
/// composed prompt in hand, since this is a standing fact about the
/// configuration, not a one-shot launch decision.
fn describe_injection_fallback(cfg: &CtxConfig) -> Option<String> {
    let (adapter, _) = adapters::resolve_default(cfg).ok()?;
    if adapter.capabilities().system_prompt && !adapter.system_prompt_supported(&[]) {
        Some(format!(
            "{}: context via task-text fallback (npm shim launch cannot carry the injected \
             system prompt safely)",
            adapter.name()
        ))
    } else {
        None
    }
}

#[derive(Debug, clap::Args)]
pub struct StatusArgs {
    /// How many recent supervisor decisions to show.
    #[arg(long, default_value_t = 10)]
    pub decisions: usize,
}

pub fn run_with<W: Write>(
    args: &StatusArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    let state = StateDir::resolve(env)?;
    writeln!(w, "state dir: {}", state.root().display())?;

    match crate::settings::AgentGate::load(repo, env) {
        Ok(gate) => {
            writeln!(w, "\nagents:")?;
            for adapter in crate::commands::ctx::adapters::all(None) {
                let name = adapter.name();
                let (enabled, location) = gate
                    .states()
                    .find(|(n, _)| *n == name)
                    .map(|(_, s)| (s.enabled, s.location()))
                    .unwrap_or((true, "default".to_string()));
                writeln!(
                    w,
                    "  {name:<8} {:<8} ({location})",
                    if enabled { "enabled" } else { "disabled" }
                )?;
            }
        }
        Err(e) => writeln!(w, "\nagents: (settings unreadable: {e})")?,
    }

    // `zirv ctx status` is the diagnostic verb, so a config load failure
    // must still render the rest of the report rather than bailing out --
    // but the two ways `CtxConfig::load` can fail are not the same kind of
    // event, and must not read or exit the same way. A layer that merely
    // failed to *parse* (`cfg.unparsable_layers`, below) is not even an
    // `Err` any more: it degraded to defaults, so `describe_chat` still runs
    // normally and the exit code stays 0. A `REPO_FORBIDDEN` rejection is a
    // security refusal -- a repository checkout tried to set something only
    // the operator may -- so it gets its own prominent line and a non-zero
    // exit, unlike every other config-load error (an unreadable file, a
    // schema/typo error), which keeps the pre-existing "still exit 0, name
    // it inline" behaviour. See the Decision Log entry on this split.
    let cfg_result = CtxConfig::load(repo, env);
    let repo_forbidden =
        matches!(&cfg_result, Err(e) if super::config::is_repo_forbidden(e.as_ref()));
    match &cfg_result {
        Ok(cfg) => {
            writeln!(w, "\n{}", describe_chat(cfg))?;
            for layer in &cfg.unparsable_layers {
                writeln!(
                    w,
                    "config: {} unparsable ({}) \u{2014} layer ignored",
                    layer.path.display(),
                    layer.message
                )?;
            }
            if let Some(line) = describe_injection_fallback(cfg) {
                writeln!(w, "{line}")?;
            }
            writeln!(
                w,
                "fallback: {} | order {} | steer below {:.0}% headroom | candidate min {:.0}% | unknown assumes {:.0}%",
                if cfg.fallback.enabled { "enabled" } else { "disabled" },
                if cfg.fallback.order.is_empty() {
                    "(none)".to_string()
                } else {
                    cfg.fallback.order.join(" -> ")
                },
                cfg.fallback.predictive_headroom_pct,
                cfg.fallback.min_candidate_headroom_pct,
                cfg.fallback.unknown_headroom_pct,
            )?;
            if cfg.fallback.enabled {
                let now = crate::commands::ctx::state::now_secs();
                for name in &cfg.fallback.order {
                    let provider = adapters::provider_for_agent_name(Some(name));
                    let (collector, estimator) =
                        super::pace::current_windows(&state, &cfg.pace, now, provider);
                    let headroom =
                        super::pace::spawn_headroom(&collector, estimator.as_ref(), now, &cfg.pace)
                            .map(|reading| format!("{:.0}% measured", reading.headroom_pct))
                            .unwrap_or_else(|| {
                                if cfg.fallback.unknown_headroom_pct > 0.0 {
                                    format!("{:.0}% assumed", cfg.fallback.unknown_headroom_pct)
                                } else {
                                    "unknown / opted out".to_string()
                                }
                            });
                    let capacity = if cfg.agents.is_capacity_small(name) {
                        "small-only"
                    } else {
                        "full"
                    };
                    writeln!(
                        w,
                        "  fallback {name}: {} / {capacity} / {headroom}",
                        if cfg.agents.is_enabled(name) {
                            "enabled"
                        } else {
                            "disabled"
                        }
                    )?;
                }
            }
        }
        Err(e) if repo_forbidden => writeln!(w, "\nCONFIG REJECTED: {e}")?,
        Err(e) => writeln!(w, "\nchat: unavailable (configuration error: {e})")?,
    }

    let mail_slug = repo_slug(repo);
    // Item 3 (read-once contract): this must apply the same visibility
    // `mail::list` gives every other caller identity, or a message queued
    // for one idle session inflated *every* session's "unread" count in this
    // same repo -- `None`/`None` here means "no filter at all", not "this
    // session's view". `mail::session_identity` is the exact same
    // ZIRV_CTX_SESSION-derived short id `zirv ctx inbox`'s default (now
    // consuming) read uses, so this count and what a plain `zirv ctx inbox`
    // would actually consume never disagree.
    let mail_agent = env(AGENT_ENV);
    let mail_session = mail::session_identity(env);
    // Issue #100 (2026-08-23): a message whose `To-session` names a session
    // that no longer exists used to inflate "unread" forever -- swept here,
    // before the count below, and reported separately so an operator can
    // tell a stale addressee from a message actually waiting to be read.
    let mail_swept = mail::sweep_undeliverable(&state, &mail_slug);
    match mail::list(
        &state,
        &mail_slug,
        mail_agent.as_deref(),
        mail_session.as_deref(),
    ) {
        Ok(messages) if mail_swept > 0 => writeln!(
            w,
            "mail: {} unread ({mail_swept} undeliverable, swept)",
            messages.len()
        )?,
        Ok(messages) => writeln!(w, "mail: {} unread", messages.len())?,
        Err(_) => writeln!(w, "mail: (unreadable)")?,
    }
    let recent_mail = mail::recent_flow_lines(&state, crate::commands::ctx::state::now_secs(), 5);
    if !recent_mail.is_empty() {
        writeln!(w, "mail flow (last hour):")?;
        for line in recent_mail {
            writeln!(w, "  {line}")?;
        }
    }

    let session_records = sessions::list(&state);

    // Issue #155, Phase 5(e): the machine-wide heavy-OPERATION budget's
    // current occupancy -- live permits (`permit::live_records`), not live
    // `Verb::Exec | Verb::Dash` session records: a session registration is
    // no longer a heavy event by itself, only an actual classified command
    // running inside one (`script_runner::Command::invoke`) is. The
    // ceiling shown is `cfg.supervise.max_heavy_operations` as THIS
    // invocation's own `CtxConfig::load` just resolved it -- never a cached
    // or stale value -- so it is always the limit actually in force for the
    // process rendering this line (issue #162(c)). Degrades silently (omits
    // the line entirely) on a config load error -- `cfg_result` may already
    // be an `Err` reported above (a `REPO_FORBIDDEN` rejection or any other
    // load failure), and this is not a second place to repeat that failure.
    //
    // Issue #162(b): a refusal or wait that cannot say WHO holds the budget
    // is undiagnosable, so every live permit is also listed by pid and
    // label -- the identical `permit::live_records` read `script_runner`'s
    // own wait message uses, so the two can never name a holder
    // differently.
    if let Ok(cfg) = &cfg_result {
        let live_permits = permit::live_records(&state);
        writeln!(
            w,
            "heavy operations: {} of {} slots in use",
            live_permits.len(),
            cfg.supervise.max_heavy_operations
        )?;
        for record in &live_permits {
            writeln!(w, "  pid {} -- {}", record.pid, record.label)?;
        }
    }

    // Issue #155, Phase 5(f): the work-group tree, right after the
    // heavy-operations block above (that line is the machine-wide OPERATION
    // budget; this is one orchestrator's own delegation tree, a different
    // question) and before the plain sessions list below. `group::list` and
    // `log::read_delegations` are both already best-effort, tolerant readers
    // of their own on-disk state -- a missing/empty `delegations.jsonl`, a
    // corrupt line, or a group id that names no group record ever written,
    // all degrade to "nothing to add" rather than an error, so nothing extra
    // is checked here. `session_records` is the exact same `sessions::list`
    // result already fetched above -- `sessions_lines`'s own doc comment
    // explains why a second call here would be wrong (it sweeps a stale
    // record's file off disk as a side effect of being called at all).
    let groups = group::list(&state);
    let delegations = log::read_delegations(&state, 200);
    let group_tree = group_tree_lines(&groups, &delegations, &session_records);
    if !group_tree.is_empty() {
        writeln!(w, "\nwork groups:")?;
        for line in &group_tree {
            writeln!(w, "{line}")?;
        }
    }

    writeln!(w, "\nsessions:")?;
    let session_lines = sessions_lines(
        &session_records,
        &state,
        crate::commands::ctx::state::now_secs(),
        env,
    );
    if session_lines.is_empty() {
        writeln!(w, "  no supervised sessions")?;
    } else {
        for line in &session_lines {
            writeln!(w, "{line}")?;
        }
    }

    // N7: the memory bank's own summary line, reusing `optimize::
    // memory_bank_summary` (count, oldest age, staleness) rather than a
    // second reader of the same on-disk format.
    let memory_summary = super::optimize::memory_bank_summary(
        &state,
        &mail_slug,
        crate::commands::ctx::state::now_secs(),
    );
    if memory_summary.count == 0 {
        writeln!(w, "memory: empty")?;
    } else {
        writeln!(
            w,
            "memory: {} entries, oldest {}d, {} stale >30d",
            memory_summary.count,
            memory_summary.oldest_written_days.unwrap_or(0),
            memory_summary.stale_count
        )?;
    }

    // Third surface, same fix as `usage.rs`'s no-subcommand branch and
    // `wrap.rs`'s status bar: the machine-wide `window::load` used to show
    // whichever provider's numbers happened to be on disk regardless of
    // which adapter this repo is actually configured for, so a codex-only
    // repo could show a stale claude session's Anthropic percentages as if
    // they were its own.
    //
    // Low 5: `provider` is derived from the *configured* agent, same as
    // `usage.rs`, rather than from a successful `adapters::select` -- a
    // repo-disabled or unready adapter used to make this whole line vanish
    // silently (`select(...).ok()` collapsing straight to `None`), so
    // `zirv ctx usage` and `zirv ctx status` could disagree about whether a
    // usage line existed at all for the exact same repo. A config-load
    // failure still omits the line: that failure already has its own
    // `chat: unavailable (...)` line above, and there is no `cfg.agent` to
    // read a name from at all in that case.
    //
    // Final wave item 4: `adapters::provider_for_usage_readout` (not the
    // bare `provider_for_agent_name`) so an *unset* `agent` with an
    // operator-disabled claude reports codex's own provider -- what
    // `resolve_default`'s own fallback loop would actually select --
    // rather than guessing the legacy default.
    let provider = cfg_result
        .as_ref()
        .ok()
        .map(adapters::provider_for_usage_readout);
    match provider {
        Some(provider) if crate::commands::ctx::window::has_no_usage_source(&state, provider) => {
            // T7 follow-up 2: a bare "no usage source" told an operator
            // nothing about *why* -- credentials file absent, macOS Keychain
            // access needed, or the statusline tee simply never wired.
            // `poll::usage_source_hint` is the one place that reasoning
            // lives, shared with nothing else so this line and a live
            // `Event::MacosKeychainPromptExpected` announcement (`poll.rs`)
            // never drift apart on what they tell the operator to do.
            writeln!(
                w,
                "\nusage windows: {provider}: no usage source ({})",
                crate::commands::ctx::poll::usage_source_hint(provider)
            )?;
        }
        Some(provider) => {
            // A window whose `resets_at` has provably passed (or, absent a
            // `resets_at`, that has outlived its own span) says nothing about
            // current usage, so it is filtered out before `describe` ever
            // sees it -- the same rule `wrap`'s status bar and the dashboard
            // header apply, so all three usage surfaces agree on what
            // "unknown" means.
            let windows = crate::commands::ctx::window::available(
                &crate::commands::ctx::window::load_for(&state, provider).unwrap_or_default(),
                crate::commands::ctx::state::now_secs(),
            );
            let describe =
                |name: &str, window: Option<&crate::commands::ctx::window::Window>| match window {
                    Some(found) => format!("{name} {:.0}%", found.used_percentage),
                    None => format!("{name} unknown"),
                };
            writeln!(
                w,
                "\nusage windows: {}, {} (see `zirv ctx usage` for detail)",
                describe("five_hour", windows.five_hour.as_ref()),
                describe("seven_day", windows.seven_day.as_ref())
            )?;
        }
        None => {}
    }

    writeln!(w, "\nlatest handoff for {}:", repo.display())?;
    match latest_for_repo(&state, repo)? {
        Some((path, handoff)) => {
            writeln!(w, "  {}", path.display())?;
            writeln!(w, "  task: {}", handoff.task)?;
            writeln!(w, "  next: {}", handoff.next_step)?;
        }
        None => writeln!(w, "  no handoff stored")?,
    }

    writeln!(w, "\nrecent decisions:")?;
    let lines = log::tail(&state, args.decisions)?;
    if lines.is_empty() {
        writeln!(w, "  none recorded")?;
    } else {
        for line in lines.iter().rev() {
            writeln!(w, "  {line}")?;
        }
    }

    // Non-zero only for a `REPO_FORBIDDEN` security refusal -- see the doc
    // comment above the `cfg_result` match for why every other config-load
    // outcome (success, a skipped-unparsable layer, or any other load error)
    // keeps exiting 0.
    Ok(if repo_forbidden { 1 } else { 0 })
}

pub fn run<W: Write>(args: &StatusArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    run_with(args, w, &repo, &env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::handoff::Handoff;
    use crate::commands::ctx::log;
    #[cfg(unix)]
    use crate::commands::ctx::signal;
    use crate::commands::ctx::state::{STATE_ENV, StateDir};

    fn env_for(state: &std::path::Path) -> std::collections::HashMap<String, String> {
        [(STATE_ENV.to_string(), state.display().to_string())].into()
    }

    #[test]
    fn an_empty_state_dir_reports_nothing_supervised() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        let env = env_for(&state);

        let mut out = Vec::new();
        let code = run_with(&StatusArgs { decisions: 10 }, &mut out, tmp.path(), &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        assert_eq!(code, 0);

        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains(&state.display().to_string()),
            "name the state dir: {text}"
        );
        assert!(text.contains("no supervised sessions"), "got {text}");
        assert!(text.contains("no handoff"), "got {text}");
    }

    /// End-to-end exit-code contract for the parse-skip half of the fix: a
    /// repo `ctx.toml` that fails to *parse* must not fail the command --
    /// `status` still renders, names the skipped layer, and exits 0.
    #[test]
    fn a_repo_layer_that_fails_to_parse_exits_zero_and_names_itself() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".zirv")).expect("mkdir repo");
        std::fs::write(repo.join(".zirv/ctx.toml"), "1").expect("write");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).expect("mkdir home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);

        let env = env_for(&state);
        let mut out = Vec::new();
        let code = run_with(&StatusArgs { decisions: 10 }, &mut out, &repo, &|k| {
            env.get(k).cloned()
        })
        .expect("runs");

        assert_eq!(
            code, 0,
            "a skipped-unparsable layer must not fail the command"
        );
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("unparsable"), "names the skip: {text}");
        assert!(text.contains("ctx.toml"), "names the file: {text}");
        assert!(
            text.contains("layer ignored"),
            "says the layer was skipped, not the whole config: {text}"
        );
    }

    /// `status` is a read-only/diagnostic verb, so it must keep using plain
    /// `CtxConfig::load` (skip-and-report), not `load_for_launch`'s fatal
    /// refusal, even for a broken HOME layer: a security finding fix for the
    /// launching verbs must not turn every `zirv ctx status` call into a hard
    /// failure just because `~/.zirv/ctx.toml` has a stray keystroke.
    #[test]
    fn a_home_layer_that_fails_to_parse_exits_zero_and_names_itself() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".zirv")).expect("mkdir repo");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(home.join(".zirv")).expect("mkdir home");
        std::fs::write(home.join(".zirv/ctx.toml"), "[score\n").expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);

        let env = env_for(&state);
        let mut out = Vec::new();
        let code = run_with(&StatusArgs { decisions: 10 }, &mut out, &repo, &|k| {
            env.get(k).cloned()
        })
        .expect("runs");

        assert_eq!(
            code, 0,
            "a broken home layer must not fail the diagnostic verb"
        );
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("unparsable"), "names the skip: {text}");
        assert!(text.contains("ctx.toml"), "names the file: {text}");
        assert!(
            text.contains("layer ignored"),
            "says the layer was skipped, not the whole config: {text}"
        );
    }

    /// End-to-end exit-code contract for the security-refusal half: a
    /// `REPO_FORBIDDEN` key is a different kind of failure from a parse
    /// error -- `status` still renders (it is the diagnostic verb) but gets
    /// its own prominent line and a non-zero exit.
    #[test]
    fn a_repo_forbidden_key_exits_nonzero_and_is_named_prominently() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".zirv")).expect("mkdir repo");
        std::fs::write(repo.join(".zirv/ctx.toml"), "agent_bin = \"/tmp/x\"\n").expect("write");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).expect("mkdir home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);

        let env = env_for(&state);
        let mut out = Vec::new();
        let code = run_with(&StatusArgs { decisions: 10 }, &mut out, &repo, &|k| {
            env.get(k).cloned()
        })
        .expect("runs");

        assert_eq!(code, 1, "a REPO_FORBIDDEN rejection must fail the command");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("CONFIG REJECTED"),
            "the refusal gets its own prominent line: {text}"
        );
        assert!(
            text.contains("agent_bin"),
            "names the offending key: {text}"
        );
    }

    #[test]
    fn it_lists_sockets_decisions_and_the_latest_handoff() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let env = env_for(state.root());

        log::append(
            &state,
            &log::Decision {
                ts: 1_700_000_000,
                session: "11111111-2222",
                verb: "wrap",
                verdict: "compact",
                score: 64,
                action: "inject",
                detail: "cooldown armed",
            },
        )
        .expect("append");

        crate::commands::ctx::handoff::store(
            &state,
            tmp.path(),
            "11111111-2222",
            &Handoff {
                task: "Wire the webhook".to_string(),
                next_step: "Write the test".to_string(),
                ..Handoff::default()
            },
        )
        .expect("store");

        let mut out = Vec::new();
        run_with(&StatusArgs { decisions: 10 }, &mut out, tmp.path(), &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");

        assert!(
            text.contains("compact"),
            "verdict in the decision list: {text}"
        );
        assert!(text.contains("inject"));
        assert!(
            text.contains("Wire the webhook"),
            "latest handoff task: {text}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_live_socket_shows_up_as_a_supervised_session() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let env = env_for(state.root());

        let _server = signal::SignalServer::bind(&state.socket_for("abcdef12-3456")).expect("bind");

        let mut out = Vec::new();
        run_with(&StatusArgs { decisions: 5 }, &mut out, tmp.path(), &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("abcdef12"), "session prefix listed: {text}");
        assert!(!text.contains("no supervised sessions"));
    }

    /// Issue #139: a session whose recorded launch-time policy fingerprint
    /// no longer matches its OWN repo's currently-resolved policy gets a
    /// visible "stale" note in `zirv ctx status` -- the whole point of this
    /// fix is that an operator can see this from `status` instead of only
    /// experiencing it as an unexplained storm of hook prompts.
    #[test]
    fn a_session_whose_launch_snapshot_no_longer_matches_the_repos_policy_is_flagged_stale() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let env = env_for(state.root());

        // A fingerprint that cannot possibly match ANY real policy's own
        // hash -- the point is only that it differs from whatever
        // `CtxConfig::load(&repo, env)` resolves for this repo right now.
        let stale_fingerprint = "0".repeat(64);
        let _guard = crate::commands::ctx::sessions::SessionGuard::register(
            &state,
            crate::commands::ctx::sessions::Record::new(
                "aaaa1111-2222-4333-8444-555555555555",
                "claude",
                &repo,
                crate::commands::ctx::sessions::Verb::Wrap,
            )
            .with_safety_policy_sha256(Some(stale_fingerprint)),
        );

        let mut out = Vec::new();
        run_with(&StatusArgs { decisions: 5 }, &mut out, &repo, &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("policy snapshot stale"),
            "must surface the divergence: {text}"
        );
        assert!(
            text.contains("relaunch to adopt"),
            "must name the remedy: {text}"
        );
    }

    /// The absent half: a session whose recorded fingerprint agrees with its
    /// repo's current policy gets no note at all -- this is not a permanent
    /// fixture of every session line, only the divergent case.
    #[test]
    fn a_session_whose_launch_snapshot_still_matches_the_repos_policy_is_not_flagged() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let env = env_for(state.root());

        let current_fingerprint = crate::commands::ctx::safety::policy_fingerprint(
            &crate::commands::ctx::safety::SafetyPolicy::default(),
        )
        .expect("fingerprint");
        let _guard = crate::commands::ctx::sessions::SessionGuard::register(
            &state,
            crate::commands::ctx::sessions::Record::new(
                "bbbb2222-2222-4333-8444-555555555555",
                "claude",
                &repo,
                crate::commands::ctx::sessions::Verb::Wrap,
            )
            .with_safety_policy_sha256(Some(current_fingerprint)),
        );

        let mut out = Vec::new();
        run_with(&StatusArgs { decisions: 5 }, &mut out, &repo, &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            !text.contains("policy snapshot stale"),
            "a matching fingerprint must not be flagged: {text}"
        );
    }

    /// The load-error half: a session whose OWN repo's config fails to load
    /// (here, a `REPO_FORBIDDEN` key) must degrade silently -- no stale
    /// line, no crash -- exactly like every other config-load failure this
    /// diagnostic verb already tolerates.
    #[test]
    fn a_repo_config_load_error_silently_skips_the_stale_check() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        // A DIFFERENT repo than the one `status` is invoked against below --
        // this is the session's own repo, and the one whose config load
        // must fail without taking the whole command down with it.
        let broken_repo = tmp.path().join("broken-repo");
        std::fs::create_dir_all(broken_repo.join(".zirv")).expect("mkdir");
        std::fs::write(
            broken_repo.join(".zirv/ctx.toml"),
            "agent_bin = \"/tmp/x\"\n",
        )
        .expect("write");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let env = env_for(state.root());

        let _guard = crate::commands::ctx::sessions::SessionGuard::register(
            &state,
            crate::commands::ctx::sessions::Record::new(
                "cccc3333-2222-4333-8444-555555555555",
                "claude",
                &broken_repo,
                crate::commands::ctx::sessions::Verb::Wrap,
            )
            .with_safety_policy_sha256(Some("0".repeat(64))),
        );

        let mut out = Vec::new();
        let code = run_with(&StatusArgs { decisions: 5 }, &mut out, &repo, &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        assert_eq!(
            code, 0,
            "the session's own broken repo must not fail status"
        );
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            !text.contains("policy snapshot stale"),
            "a load error must degrade silently, not fabricate a stale claim: {text}"
        );
        assert!(
            text.contains("cccc3333"),
            "the session itself is still listed: {text}"
        );
    }

    #[test]
    fn the_decision_limit_is_honored() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let env = env_for(state.root());

        for i in 0..5 {
            log::append(
                &state,
                &log::Decision {
                    ts: 1_700_000_000 + i,
                    session: "s",
                    verb: "exec",
                    verdict: "healthy",
                    score: 0,
                    action: &format!("tick{i}"),
                    detail: "",
                },
            )
            .expect("append");
        }

        let mut out = Vec::new();
        run_with(&StatusArgs { decisions: 2 }, &mut out, tmp.path(), &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("tick4"));
        assert!(text.contains("tick3"));
        assert!(!text.contains("tick0"));
    }

    /// `zirv ctx status` surfaces the `.settings.toml` gate: every known
    /// adapter, whether it is enabled, and (when disabled) why.
    #[test]
    fn status_lists_each_adapter_with_whether_it_is_enabled_and_why() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let env = env_for(state.root());
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        std::fs::create_dir_all(tmp.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            tmp.path().join(".zirv/.settings.toml"),
            "[agents.codex]\nenabled = false\n",
        )
        .expect("write");

        let mut out = Vec::new();
        run_with(&StatusArgs { decisions: 5 }, &mut out, tmp.path(), &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");

        assert!(text.contains("agents:"), "got {text}");
        let claude_line = text.lines().find(|l| l.contains("claude")).unwrap_or("");
        assert!(claude_line.contains("enabled"), "got {claude_line}");
        let codex_line = text.lines().find(|l| l.contains("codex")).unwrap_or("");
        assert!(codex_line.contains("disabled"), "got {codex_line}");
        assert!(
            codex_line.contains(".settings.toml"),
            "names the file that disabled it: {codex_line}"
        );
    }

    /// The `chat:` line names both the adapter `zirv ctx chat` would launch
    /// and the rule that picked it: no explicit `agent` configured falls
    /// back to the first enabled-and-ready adapter, while an explicit
    /// `agent = "claude"` in `ctx.toml` is reported as configured instead.
    #[test]
    fn status_names_the_agent_chat_would_launch_and_the_rule_that_chose_it() {
        let default_cfg = CtxConfig::default();
        assert_eq!(
            describe_chat(&default_cfg),
            "chat: claude (first enabled and ready)"
        );

        let configured_cfg = CtxConfig {
            agent: Some("claude".to_string()),
            ..CtxConfig::default()
        };
        assert_eq!(describe_chat(&configured_cfg), "chat: claude (configured)");
    }

    /// When nothing is both enabled and ready, `describe_chat` degrades to
    /// `resolve_default`'s own aggregated reasons rather than failing --
    /// `status` must keep printing everything else even when chat has
    /// nothing to launch.
    #[test]
    fn status_explains_why_chat_cannot_launch_when_nothing_is_enabled_and_ready() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let env = env_for(state.root());
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        // Both known adapters disabled: codex's own `ready()` now only
        // checks that its program resolves (`CodexAdapter::ready`, mirrors
        // claude), so disabling claude alone would leave codex as a usable
        // fallback -- both have to be disabled by the gate to reach
        // "nothing enabled and ready" at all.
        std::fs::create_dir_all(tmp.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            tmp.path().join(".zirv/.settings.toml"),
            "[agents.claude]\nenabled = false\n[agents.codex]\nenabled = false\n",
        )
        .expect("write");

        let mut out = Vec::new();
        run_with(&StatusArgs { decisions: 5 }, &mut out, tmp.path(), &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");

        let chat_line = text.lines().find(|l| l.starts_with("chat:")).unwrap_or("");
        assert!(chat_line.contains("unavailable"), "got {chat_line}");
        assert!(chat_line.contains("claude"), "got {chat_line}");
        assert!(chat_line.contains("codex"), "got {chat_line}");
        assert!(chat_line.contains("disabled"), "got {chat_line}");
        assert!(
            !chat_line.contains('\u{2014}'),
            "no em dashes in user-facing copy: {chat_line}"
        );
    }

    /// `mail: N unread` counts messages stored for this repo's slug via
    /// `mail::list`, the same store/list path `zirv ctx send`/`zirv ctx
    /// inbox` use.
    #[test]
    fn status_reports_the_unread_mail_count_for_this_repo() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let env = env_for(state.root());
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let slug = repo_slug(tmp.path());
        let cfg = CtxConfig::default();
        for from_session in ["sender-one", "sender-two"] {
            mail::store(
                &state,
                &slug,
                &mail::Message {
                    from_session: from_session.to_string(),
                    from_agent: "claude".to_string(),
                    to: "any".to_string(),
                    to_session: None,
                    sent: 1_700_000_000,
                    body: "heads up".to_string(),
                },
                &cfg,
            )
            .expect("store");
        }

        let mut out = Vec::new();
        run_with(&StatusArgs { decisions: 5 }, &mut out, tmp.path(), &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");

        assert!(text.contains("mail: 2 unread"), "got {text}");
    }

    /// Issue #155, Phase 5(e): `heavy operations: N of M slots in use`
    /// counts live permits (`permit::live_records`), not live session
    /// records -- a session that is not actually running a classified heavy
    /// command holds nothing.
    ///
    /// Issue #162(b): the occupant is also named by pid and label, so an
    /// operator who sees a full budget can tell WHAT is holding it rather
    /// than only how many things are.
    #[test]
    fn status_reports_the_heavy_operations_budget_occupancy() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let mut env = env_for(state.root());
        env.insert(
            "ZIRV_CTX_SUPERVISE_MAX_HEAVY_OPERATIONS".to_string(),
            "3".to_string(),
        );
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let held =
            permit::acquire(&state, 3, "session ab12cd34: cargo build").expect("permit granted");

        let mut out = Vec::new();
        run_with(&StatusArgs { decisions: 5 }, &mut out, tmp.path(), &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");

        assert!(
            text.contains("heavy operations: 1 of 3 slots in use"),
            "got {text}"
        );
        assert!(
            text.contains(&format!(
                "pid {} -- session ab12cd34: cargo build",
                std::process::id()
            )),
            "the occupant must be named, not just counted: got {text}"
        );

        drop(held);
    }

    /// A `REPO_FORBIDDEN` config rejection must not add a second failure
    /// mode on top of the `CONFIG REJECTED` line already reported above --
    /// the heavy-operations line is simply omitted, degrading silently the
    /// same way `describe_injection_fallback` already does for an `Err`
    /// config.
    #[test]
    fn status_omits_the_heavy_operations_line_when_config_fails_to_load() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let env = env_for(state.root());
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        std::fs::create_dir_all(tmp.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            tmp.path().join(".zirv/ctx.toml"),
            "[supervise]\nmax_heavy_operations = 99\n",
        )
        .expect("write");

        let mut out = Vec::new();
        run_with(&StatusArgs { decisions: 5 }, &mut out, tmp.path(), &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");

        assert!(
            !text.contains("heavy operations:"),
            "must degrade silently on a config load error: {text}"
        );
    }

    /// Item 3 (read-once contract): a message directed at a different
    /// session must not inflate *this* session's "unread" count -- observed
    /// as a message queued for one idle pane showing as unread to every
    /// session in the repo. Broadcast mail (no `to_session`) and mail
    /// directed at this exact session still count.
    ///
    /// Both `aaaa1111` and `bbbb2222` are registered as live sessions here
    /// (issue #100, 2026-08-23): this test is pinning the *visibility*
    /// filter specifically, and without a live record for each, the new
    /// undeliverable-mail sweep would remove the "directed elsewhere"
    /// message before the visibility filter ever got a chance to -- a
    /// different mechanism reaching the same count, which would leave this
    /// test unable to tell a filtering regression from a sweeping one.
    #[test]
    fn status_mail_count_ignores_mail_directed_at_a_different_session() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tmp.path().to_path_buf();
        let _asking_guard = crate::commands::ctx::sessions::SessionGuard::register(
            &state,
            crate::commands::ctx::sessions::Record::new(
                "aaaa1111-2222-4333-8444-555555555555",
                "claude",
                &repo,
                crate::commands::ctx::sessions::Verb::Exec,
            ),
        );
        let _elsewhere_guard = crate::commands::ctx::sessions::SessionGuard::register(
            &state,
            crate::commands::ctx::sessions::Record::new(
                "bbbb2222-2222-4333-8444-555555555555",
                "claude",
                &repo,
                crate::commands::ctx::sessions::Verb::Exec,
            ),
        );

        let slug = repo_slug(tmp.path());
        let cfg = CtxConfig::default();
        // Directed at a session other than the one asking: must not count.
        mail::store(
            &state,
            &slug,
            &mail::Message {
                from_session: "sender-one".to_string(),
                from_agent: "claude".to_string(),
                to: "any".to_string(),
                to_session: Some("bbbb2222".to_string()),
                sent: 1_700_000_000,
                body: "not for you".to_string(),
            },
            &cfg,
        )
        .expect("store directed elsewhere");
        // Directed at the asking session itself: must count.
        mail::store(
            &state,
            &slug,
            &mail::Message {
                from_session: "sender-two".to_string(),
                from_agent: "claude".to_string(),
                to: "any".to_string(),
                to_session: Some("aaaa1111".to_string()),
                sent: 1_700_000_100,
                body: "for you".to_string(),
            },
            &cfg,
        )
        .expect("store directed here");
        // Broadcast: visible and counted regardless of identity.
        mail::store(
            &state,
            &slug,
            &mail::Message {
                from_session: "sender-three".to_string(),
                from_agent: "claude".to_string(),
                to: "any".to_string(),
                to_session: None,
                sent: 1_700_000_200,
                body: "for everyone".to_string(),
            },
            &cfg,
        )
        .expect("store broadcast");

        let mut env = env_for(state.root());
        env.insert(
            crate::commands::ctx::adapters::SESSION_ENV.to_string(),
            "aaaa1111".to_string(),
        );

        let mut out = Vec::new();
        run_with(&StatusArgs { decisions: 5 }, &mut out, tmp.path(), &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");

        assert!(
            text.contains("mail: 2 unread"),
            "only the broadcast and the directed-here message count, not the one \
             directed at bbbb2222: {text}"
        );
    }

    /// Issue #100 (2026-08-23): a message addressed to a session that no
    /// longer exists at all (no live registry record was ever registered for
    /// it in this test) must be swept out of the unread count, and reported
    /// separately rather than silently dropped.
    #[test]
    fn status_reports_undeliverable_mail_as_swept_and_excludes_it_from_unread() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let slug = repo_slug(tmp.path());
        let cfg = CtxConfig::default();
        mail::store(
            &state,
            &slug,
            &mail::Message {
                from_session: "sender-one".to_string(),
                from_agent: "claude".to_string(),
                to: "any".to_string(),
                to_session: Some("deadbeef".to_string()),
                sent: 1_700_000_000,
                body: "nobody will ever read this".to_string(),
            },
            &cfg,
        )
        .expect("store directed at a dead session");
        mail::store(
            &state,
            &slug,
            &mail::Message {
                from_session: "sender-two".to_string(),
                from_agent: "claude".to_string(),
                to: "any".to_string(),
                to_session: None,
                sent: 1_700_000_100,
                body: "still pending".to_string(),
            },
            &cfg,
        )
        .expect("store undirected");

        let env = env_for(state.root());
        let mut out = Vec::new();
        run_with(&StatusArgs { decisions: 5 }, &mut out, tmp.path(), &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");

        assert!(
            text.contains("mail: 1 unread (1 undeliverable, swept)"),
            "got {text}"
        );
    }

    #[test]
    fn status_mentions_the_usage_windows() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(tmp.path());
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let env = env_for(state.root());

        crate::commands::ctx::window::store(
            &state,
            &crate::commands::ctx::window::UsageWindows {
                five_hour: Some(crate::commands::ctx::window::Window {
                    used_percentage: 77.0,
                    // A window still ahead of us in wall-clock time, not a
                    // fixed timestamp from whenever this test was written --
                    // `available` now filters on real `now_secs()`, so a
                    // hardcoded past instant would eventually go stale and
                    // start failing this test for reasons unrelated to it.
                    resets_at: crate::commands::ctx::state::now_secs() + 1000,
                    observed_at: crate::commands::ctx::state::now_secs(),
                }),
                seven_day: None,
            },
        )
        .expect("store");

        let mut out = Vec::new();
        run_with(&StatusArgs { decisions: 5 }, &mut out, tmp.path(), &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("usage"), "got {text}");
        assert!(text.contains("77"), "got {text}");
    }

    /// The fourth surface change: a window whose `resets_at` has provably
    /// passed must read as "unknown", the same wording the line already uses
    /// for a genuinely absent window -- never a stale percent presented as
    /// current.
    #[test]
    fn status_shows_unknown_for_a_usage_window_that_has_expired() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(tmp.path());
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let env = env_for(state.root());

        crate::commands::ctx::window::store(
            &state,
            &crate::commands::ctx::window::UsageWindows {
                five_hour: Some(crate::commands::ctx::window::Window {
                    used_percentage: 77.0,
                    resets_at: 1, // long past any real wall clock
                    observed_at: 1,
                }),
                seven_day: None,
            },
        )
        .expect("store");

        let mut out = Vec::new();
        run_with(&StatusArgs { decisions: 5 }, &mut out, tmp.path(), &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            !text.contains("77%"),
            "an expired window must not render as a current percent: {text}"
        );
        assert!(
            text.contains("five_hour unknown"),
            "expired reads the same as never-recorded: {text}"
        );
    }

    /// The third of the three usage surfaces this fixes (alongside `zirv ctx
    /// usage`'s no-subcommand branch and `wrap`'s status bar): `window::load`
    /// used to read the one machine-wide file regardless of which adapter
    /// this repo is actually configured for, so a codex-only repo's `zirv
    /// ctx status` could show a stale claude session's Anthropic percentages
    /// as its own. When nothing has been recorded for a provider
    /// (`window::has_no_usage_source`), the status shows "no source" instead
    /// of a number. Task 6 wired an active source refresh ahead of this same
    /// check in the pacing gate (`pace::wait_for_window`'s own
    /// `refresh_sources`) and in `zirv ctx usage`'s own report -- this
    /// `status` readout does not itself refresh anything, so it still shows
    /// whatever either of those two paths (or the statusline tee) last
    /// stored.
    #[test]
    fn status_shows_no_usage_source_for_a_codex_configured_repo_rather_than_anthropic_numbers() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let mut env = env_for(state.root());
        env.insert("ZIRV_CTX_AGENT".to_string(), "codex".to_string());

        // The legacy global file a claude session left behind: still on
        // disk, but must not be misattributed to this operator-configured
        // codex session (`ZIRV_CTX_AGENT`; a repo cannot set `agent`).
        crate::commands::ctx::window::store(
            &state,
            &crate::commands::ctx::window::UsageWindows {
                five_hour: Some(crate::commands::ctx::window::Window {
                    used_percentage: 77.0,
                    resets_at: 1_785_509_000,
                    observed_at: crate::commands::ctx::state::now_secs(),
                }),
                seven_day: None,
            },
        )
        .expect("store");

        let mut out = Vec::new();
        run_with(&StatusArgs { decisions: 5 }, &mut out, tmp.path(), &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("openai: no usage source"), "got {text}");
        assert!(
            !text.contains("77"),
            "the claude-only legacy file must not leak into a codex repo's usage line: {text}"
        );
    }

    /// Low 5 (fix): the case above configures codex while it is still
    /// enabled, so `adapters::select` never actually refuses there. Here
    /// codex is *disabled* via the repo's own `.settings.toml` -- before
    /// this fix, a `select` refusal made the `usage windows:` line vanish
    /// entirely (`provider` collapsed to `None`), so `zirv ctx status` and
    /// `zirv ctx usage` disagreed about whether a codex-configured repo had
    /// a usage line at all. It must still say "openai: no usage source",
    /// derived from the configured name directly.
    ///
    /// `agent` is configured via `ZIRV_CTX_AGENT` (the operator layer), not
    /// the repo's own `ctx.toml`: `agent` is `REPO_FORBIDDEN` (final wave
    /// item 1) precisely so a checkout cannot pick which vendor account
    /// gets spent -- this test's own scenario if it tried.
    #[test]
    fn status_names_no_usage_source_for_a_disabled_codex_rather_than_hiding_the_line() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let mut env = env_for(state.root());
        env.insert("ZIRV_CTX_AGENT".to_string(), "codex".to_string());
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        std::fs::create_dir_all(tmp.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            tmp.path().join(".zirv/.settings.toml"),
            "[agents.codex]\nenabled = false\n",
        )
        .expect("write");

        let mut out = Vec::new();
        run_with(&StatusArgs { decisions: 5 }, &mut out, tmp.path(), &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("openai: no usage source"),
            "the line must still name the configured agent's provider, not disappear: {text}"
        );
    }

    /// Issue #85: on a Windows npm-shim codex launch (`codex.cmd`, resolved
    /// through `cmd.exe /c`), `zirv ctx status` must report the degraded
    /// injection channel as a persistent line, not just a transient
    /// announcement -- an operator who only ever runs `zirv ctx status`
    /// still has to be told the session is running on weaker instructions.
    #[cfg(windows)]
    #[test]
    fn status_reports_the_degraded_injection_channel_for_a_codex_shim_launch() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let shim_dir = tempfile::tempdir().expect("tempdir");
        let shim = shim_dir.path().join("codex.cmd");
        std::fs::write(&shim, "@echo off\r\n").expect("write shim");

        let mut env = env_for(state.root());
        env.insert("ZIRV_CTX_AGENT".to_string(), "codex".to_string());
        env.insert("ZIRV_CTX_AGENT_BIN".to_string(), shim.display().to_string());

        let mut out = Vec::new();
        run_with(&StatusArgs { decisions: 5 }, &mut out, tmp.path(), &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("codex: context via task-text fallback"),
            "got {text}"
        );
    }

    /// A direct (non-shim) codex launch must not report the fallback: its
    /// own `system_prompt_args`/`-c developer_instructions=...` channel is
    /// safe there, so the status line must not falsely claim degradation.
    #[test]
    fn status_does_not_report_the_fallback_for_a_direct_codex_launch() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let mut env = env_for(state.root());
        env.insert("ZIRV_CTX_AGENT".to_string(), "codex".to_string());
        env.insert(
            "ZIRV_CTX_AGENT_BIN".to_string(),
            "/tmp/fake-codex-not-a-real-path".to_string(),
        );

        let mut out = Vec::new();
        run_with(&StatusArgs { decisions: 5 }, &mut out, tmp.path(), &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            !text.contains("context via task-text fallback"),
            "got {text}"
        );
    }

    // N7: the registry-backed `sessions:` block and the `memory:` line.

    /// NEW-3: a `wrap` that bound no turn-signal socket is running but
    /// cannot answer a nudge. It used to be dropped from the registry
    /// outright, so it disappeared from `status` too and an operator whose
    /// session had failed to bind could not see it at all. It must be
    /// visible, and visibly different from a healthy one.
    #[test]
    fn status_shows_an_unreachable_session_rather_than_hiding_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let env = env_for(state.root());
        let repo = tmp.path().join("repo");

        let record = crate::commands::ctx::sessions::Record::new(
            "abcdef12-3456-4789-8abc-def012345678",
            "claude",
            &repo,
            crate::commands::ctx::sessions::Verb::Wrap,
        )
        .unreachable();
        let short = record.short.clone();
        let _guard = crate::commands::ctx::sessions::SessionGuard::register(&state, record);

        let mut out = Vec::new();
        run_with(&StatusArgs { decisions: 5 }, &mut out, &repo, &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");

        let line = text
            .lines()
            .find(|l| l.contains(&short))
            .unwrap_or_else(|| panic!("the session must still be listed: {text}"));
        assert!(
            line.contains("unreachable"),
            "and must be marked unreachable: {line}"
        );
        assert!(
            !line.contains("  live  "),
            "it is not a healthy live session: {line}"
        );
        assert!(line.contains("wrap"), "still names the verb: {line}");
        assert!(!text.contains("no supervised sessions"));
    }

    #[test]
    fn status_lists_each_live_session_with_its_agent_verb_and_age() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let env = env_for(state.root());
        let repo = tmp.path().join("repo");

        let record = crate::commands::ctx::sessions::Record::new(
            "abcdef12-3456-4789-8abc-def012345678",
            "claude",
            &repo,
            crate::commands::ctx::sessions::Verb::Exec,
        );
        let short = record.short.clone();
        let _guard = crate::commands::ctx::sessions::SessionGuard::register(&state, record);

        let mut out = Vec::new();
        run_with(&StatusArgs { decisions: 5 }, &mut out, &repo, &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");

        let line = text.lines().find(|l| l.contains(&short)).unwrap_or("");
        assert!(line.contains("claude"), "names the agent: {line}");
        assert!(line.contains("exec"), "names the verb: {line}");
        assert!(line.contains("pid"), "names the pid: {line}");
        assert!(line.contains("live"), "reports live: {line}");
        assert!(!text.contains("no supervised sessions"));
    }

    /// A pid guaranteed dead by the time it is used, the same idiom
    /// `sessions.rs`'s own tests use.
    fn dead_pid() -> u32 {
        let mut cmd = if cfg!(windows) {
            let mut c = std::process::Command::new("cmd");
            c.args(["/C", "exit", "0"]);
            c
        } else {
            std::process::Command::new("true")
        };
        let mut child = cmd.spawn().expect("spawn a short-lived process");
        let pid = child.id();
        let _ = child.wait();
        pid
    }

    /// Issue #166: an operator reading `status` must see this unambiguously
    /// as `dead`, not `live` -- and not the old `stale` wording either, which
    /// read as merely one more shade of live rather than "this process is
    /// gone."
    #[test]
    fn status_marks_a_session_whose_process_is_gone_as_dead() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let env = env_for(state.root());
        let repo = tmp.path().join("repo");

        let mut record = crate::commands::ctx::sessions::Record::new(
            "dddddddd-2222-4333-8444-555555555555",
            "claude",
            &repo,
            crate::commands::ctx::sessions::Verb::Loop,
        );
        record.pid = dead_pid();
        let short = record.short.clone();
        let _guard = crate::commands::ctx::sessions::SessionGuard::register(&state, record);

        let mut out = Vec::new();
        run_with(&StatusArgs { decisions: 5 }, &mut out, &repo, &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");

        let line = text.lines().find(|l| l.contains(&short)).unwrap_or("");
        assert!(line.contains("dead"), "got {line}");
        assert!(!line.contains("live"), "got {line}");
    }

    #[test]
    fn status_still_lists_a_socket_that_has_no_registry_record() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let env = env_for(state.root());

        // An older zirv wrote only the socket, never a registry record: the
        // listing must still surface it, labeled as having none.
        let _server =
            crate::commands::ctx::signal::SignalServer::bind(&state.socket_for("abcdef12-3456"))
                .expect("bind");

        let mut out = Vec::new();
        run_with(&StatusArgs { decisions: 5 }, &mut out, tmp.path(), &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");

        let line = text.lines().find(|l| l.contains("abcdef12")).unwrap_or("");
        assert!(
            line.contains("no record"),
            "a socket with no registry entry is still listed: {line}"
        );
    }

    /// Issue #99 (2026-08-23): a dead endpoint marker (nothing is listening
    /// behind it, and no registry record ever named it) must not accumulate
    /// as a permanent `(no record)` line -- `sessions::list`'s own orphan
    /// sweep, exercised here through `status` end to end, removes it.
    #[test]
    fn status_no_longer_lists_a_dead_socket_that_has_no_registry_record() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let env = env_for(state.root());

        crate::commands::ctx::state::create_private_dir_all(&state.sockets()).expect("mkdir");
        std::fs::write(
            state.sockets().join("dead12ab.sock"),
            "leftover, nothing is listening",
        )
        .expect("write leftover marker");

        let mut out = Vec::new();
        run_with(&StatusArgs { decisions: 5 }, &mut out, tmp.path(), &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");

        assert!(
            !text.contains("dead12ab"),
            "a swept dead marker must no longer be listed: {text}"
        );
        assert!(
            !state.sockets().join("dead12ab.sock").exists(),
            "and the leftover file itself is removed from disk"
        );
    }

    #[test]
    fn status_reports_the_memory_bank_size_and_its_oldest_entry() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let env = env_for(state.root());

        assert!(
            {
                let mut out = Vec::new();
                run_with(&StatusArgs { decisions: 5 }, &mut out, tmp.path(), &|k| {
                    env.get(k).cloned()
                })
                .expect("runs");
                String::from_utf8(out)
                    .expect("utf8")
                    .contains("memory: empty")
            },
            "an empty bank reports empty"
        );

        let slug = repo_slug(tmp.path());
        let cfg = CtxConfig::default();
        let now = crate::commands::ctx::state::now_secs();
        crate::commands::ctx::memory::remember(
            &state,
            &slug,
            &crate::commands::ctx::memory::Entry {
                key: "build-cmd".to_string(),
                written_by: "claude".to_string(),
                written: now - 5 * 86_400,
                verified: now - 5 * 86_400,
                source: "explicit".to_string(),
                body: "cargo build --release".to_string(),
                importance: None,
                confidence: None,
                tags: Vec::new(),
                paths: Vec::new(),
            },
            &cfg,
        )
        .expect("remember");

        let mut out = Vec::new();
        run_with(&StatusArgs { decisions: 5 }, &mut out, tmp.path(), &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        let line = text
            .lines()
            .find(|l| l.starts_with("memory:"))
            .unwrap_or("");
        assert!(line.contains('1'), "one entry: {line}");
        assert!(line.contains("5d"), "the oldest entry's age: {line}");
    }

    fn sample_group(id: &str, scope: &str) -> group::WorkGroup {
        group::WorkGroup {
            work_group_id: id.to_string(),
            parent_session_id: "sess-parent".to_string(),
            scope: scope.to_string(),
            child_limit: 3,
            token_budget: None,
            deadline_secs: None,
            completion_contract: "report by mail".to_string(),
            created_at: 1_700_000_000,
            closed_at: None,
            admitted_children: 0,
            sub_orchestrator_session: None,
        }
    }

    fn delegation_row(
        work_group_id: &str,
        session: &str,
        agent: &str,
        input_tokens: u64,
        cache_read_input_tokens: u64,
        output_tokens: u64,
    ) -> log::DelegationRow {
        log::DelegationRow {
            ts: 1_700_000_000,
            session: session.to_string(),
            parent_session: "sess-parent".to_string(),
            work_group_id: Some(work_group_id.to_string()),
            agent: agent.to_string(),
            model: None,
            input_tokens,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens,
            output_tokens,
            wall_ms: 5_000,
            exit_code: 0,
            outcome: "ok".to_string(),
        }
    }

    fn ungrouped_delegation_row(session: &str, agent: &str) -> log::DelegationRow {
        let mut row = delegation_row("unused", session, agent, 0, 0, 0);
        row.work_group_id = None;
        row
    }

    /// Issue #155, Phase 5(f): an orchestrator can see what its own
    /// delegation tree has cost, per child, in raw classes -- which is the
    /// question "was delegating cheaper than doing it here" reduces to.
    #[test]
    fn the_group_tree_shows_each_child_with_its_own_spend() {
        let groups = vec![sample_group("wg-1", "phase 5 implementation")];
        let delegations = vec![
            delegation_row("wg-1", "sess-a", "codex", 1_000, 91_000, 500),
            delegation_row("wg-1", "sess-b", "claude", 2_000, 40_000, 900),
        ];
        let lines = group_tree_lines(&groups, &delegations, &[]);
        let text = lines.join("\n");

        assert!(text.contains("wg-1"), "got {text}");
        assert!(text.contains("phase 5 implementation"), "the scope: {text}");
        assert!(text.contains("sess-a"), "each child: {text}");
        assert!(text.contains("sess-b"), "each child: {text}");
        assert!(text.contains("codex"), "and which harness ran it: {text}");
        assert!(text.contains("91000"), "raw cache-read spend: {text}");
    }

    /// A delegation with no group is not lost -- it is listed under a plain
    /// "ungrouped" heading, because a one-off delegation is still spend.
    #[test]
    fn ungrouped_delegations_are_still_listed() {
        let delegations = vec![ungrouped_delegation_row("sess-c", "codex")];
        let text = group_tree_lines(&[], &delegations, &[]).join("\n");
        assert!(text.contains("ungrouped"), "got {text}");
        assert!(text.contains("sess-c"), "got {text}");
    }

    /// Nothing to show is nothing shown -- no empty heading on a machine that
    /// has never delegated.
    #[test]
    fn no_groups_and_no_delegations_render_nothing() {
        assert!(group_tree_lines(&[], &[], &[]).is_empty());
    }

    /// A delegation's `work_group_id` naming a group this listing never
    /// loaded (the group's own file was never written, or has since been
    /// swept) is not dropped -- either way the spend happened, and the bare
    /// id still identifies which batch it belongs to.
    #[test]
    fn a_delegation_naming_an_unknown_group_still_shows_up() {
        let delegations = vec![delegation_row("wg-ghost", "sess-d", "codex", 100, 200, 50)];
        let text = group_tree_lines(&[], &delegations, &[]).join("\n");
        assert!(
            text.contains("wg-ghost"),
            "the bare id still names it: {text}"
        );
        assert!(text.contains("sess-d"), "got {text}");
    }

    /// A session that is still live overrides its own last-logged outcome:
    /// that logged value is a snapshot from before the session finished, and
    /// "running" is more accurate than replaying a stale outcome.
    #[test]
    fn a_still_live_session_shows_running_not_its_recorded_outcome() {
        let groups = vec![sample_group("wg-1", "phase 5 implementation")];
        let mut row = delegation_row("wg-1", "sess-a", "codex", 100, 200, 50);
        row.session = "sess-a-full-id".to_string();
        row.outcome = "failed".to_string();
        let record = sessions::Record::new(
            "sess-a-full-id",
            "codex",
            std::path::Path::new("/repo"),
            sessions::Verb::Exec,
        );
        let text = group_tree_lines(&groups, &[row], &[(record, Liveness::Live)]).join("\n");
        assert!(text.contains("running"), "got {text}");
        assert!(
            !text.contains("failed"),
            "must not show the stale outcome: {text}"
        );
    }

    /// Issue #170: the tree names an ABANDONED group -- claimed by a
    /// SubOrchestrator whose own session is no longer live, still open. The
    /// SAME group with its claimant still alive must not carry the marker.
    #[test]
    fn the_group_tree_marks_a_group_whose_claimed_coordinator_died() {
        let mut group = sample_group("wg-1", "phase 5 implementation");
        group.sub_orchestrator_session = Some("deadbeef".to_string());
        let row = delegation_row("wg-1", "sess-a", "codex", 100, 200, 50);

        // `sessions::short_id` takes the first 8 ASCII-alphanumeric
        // characters, so this session id derives to exactly "deadbeef" --
        // matching `group.sub_orchestrator_session` above.
        let record = sessions::Record::new(
            "deadbeef-2222-4333-8444-555555555555",
            "claude",
            std::path::Path::new("/repo"),
            sessions::Verb::Exec,
        );
        assert_eq!(record.short, "deadbeef");

        // Dead: no matching live record at all.
        let dead_text = group_tree_lines(
            std::slice::from_ref(&group),
            std::slice::from_ref(&row),
            &[],
        )
        .join("\n");
        assert!(dead_text.contains("ABANDONED"), "got {dead_text}");

        // Alive: the exact same group, with its claimant now live.
        let alive_text = group_tree_lines(&[group], &[row], &[(record, Liveness::Live)]).join("\n");
        assert!(!alive_text.contains("ABANDONED"), "got {alive_text}");
    }

    /// End-to-end: no `delegations.jsonl` at all (the common case -- `zirv
    /// ctx agent` has never successfully delegated on this machine) must not
    /// add a "work groups:" section, and must not disturb anything else
    /// `status` renders.
    #[test]
    fn status_shows_no_work_groups_section_when_no_delegations_file_exists() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let env = env_for(state.root());

        let mut out = Vec::new();
        let code = run_with(&StatusArgs { decisions: 5 }, &mut out, tmp.path(), &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        assert_eq!(code, 0);
        let text = String::from_utf8(out).expect("utf8");
        assert!(!text.contains("work groups:"), "got {text}");
        assert!(text.contains("no supervised sessions"), "got {text}");
    }

    /// End-to-end: a present-but-empty `delegations.jsonl` behaves exactly
    /// like an absent one.
    #[test]
    fn status_shows_no_work_groups_section_when_the_delegations_file_is_empty() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        std::fs::write(state.logs().join(log::DELEGATION_FILE), "").expect("write empty");
        let env = env_for(state.root());

        let mut out = Vec::new();
        let code = run_with(&StatusArgs { decisions: 5 }, &mut out, tmp.path(), &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        assert_eq!(code, 0);
        let text = String::from_utf8(out).expect("utf8");
        assert!(!text.contains("work groups:"), "got {text}");
    }

    /// End-to-end: every real, on-disk delegation currently has
    /// `work_group_id: None` (Task 5.3 hasn't threaded a real id through
    /// yet) -- it must still show up, under the ungrouped heading, right
    /// after the heavy-operations block Task 5.5 renders.
    #[test]
    fn status_lists_ungrouped_delegations_after_the_heavy_operations_block() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        log::append_delegation(
            &state,
            &log::Delegation {
                ts: 1_700_000_000,
                session: "sess-child",
                parent_session: "sess-parent",
                work_group_id: None,
                agent: "codex",
                model: Some("gpt-5-codex"),
                input_tokens: 1_000,
                cache_creation_input_tokens: 8_000,
                cache_read_input_tokens: 91_000,
                output_tokens: 500,
                wall_ms: 42_000,
                exit_code: 0,
                outcome: "ok",
            },
        )
        .expect("append");
        let env = env_for(state.root());

        let mut out = Vec::new();
        let code = run_with(&StatusArgs { decisions: 5 }, &mut out, tmp.path(), &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        assert_eq!(code, 0);
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("work groups:"), "got {text}");
        assert!(text.contains("ungrouped"), "got {text}");
        assert!(text.contains("sess-child"), "got {text}");

        let heavy_at = text.find("heavy operations:");
        let sessions_at = text.find("\nsessions:");
        let groups_at = text.find("work groups:");
        if let (Some(heavy_at), Some(groups_at), Some(sessions_at)) =
            (heavy_at, groups_at, sessions_at)
        {
            assert!(
                heavy_at < groups_at && groups_at < sessions_at,
                "the group tree sits right after the heavy-operations block \
                 and before the sessions list: {text}"
            );
        }
    }

    /// End-to-end: a delegation whose `work_group_id` names a group this
    /// state dir never wrote a record for must not crash `status` or drop
    /// the rest of its output -- it still shows up, quietly, by its bare id.
    #[test]
    fn status_shows_a_delegation_with_an_unknown_group_id_without_failing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        log::append_delegation(
            &state,
            &log::Delegation {
                ts: 1_700_000_000,
                session: "sess-ghost-child",
                parent_session: "sess-parent",
                work_group_id: Some("wg-never-written"),
                agent: "claude",
                model: None,
                input_tokens: 10,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                output_tokens: 5,
                wall_ms: 1_000,
                exit_code: 0,
                outcome: "ok",
            },
        )
        .expect("append");
        let env = env_for(state.root());

        let mut out = Vec::new();
        let code = run_with(&StatusArgs { decisions: 5 }, &mut out, tmp.path(), &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        assert_eq!(code, 0, "an unknown group id must not fail the command");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("wg-never-written"), "got {text}");
        assert!(
            text.contains("mail:"),
            "the rest of status still renders: {text}"
        );
    }

    /// End-to-end: a corrupt line in `delegations.jsonl` (a truncated
    /// concurrent write) must not take down `status` -- the rest of the
    /// file's rows still render.
    #[test]
    fn status_skips_a_corrupt_delegation_line_without_failing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        log::append_delegation(
            &state,
            &log::Delegation {
                ts: 1_700_000_000,
                session: "sess-good",
                parent_session: "sess-parent",
                work_group_id: None,
                agent: "codex",
                model: None,
                input_tokens: 1,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                output_tokens: 1,
                wall_ms: 100,
                exit_code: 0,
                outcome: "ok",
            },
        )
        .expect("append");
        {
            let mut file = crate::commands::ctx::state::open_private_append(
                &state.logs().join(log::DELEGATION_FILE),
            )
            .expect("open");
            use std::io::Write as _;
            writeln!(file, "not json").expect("write corrupt line");
        }
        let env = env_for(state.root());

        let mut out = Vec::new();
        let code = run_with(&StatusArgs { decisions: 5 }, &mut out, tmp.path(), &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        assert_eq!(code, 0, "a corrupt line must not fail the command");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("sess-good"), "got {text}");
    }
}
