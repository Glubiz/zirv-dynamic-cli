use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::adapters::{self, AGENT_ENV, DefaultOrigin};
use super::chain;
use super::config::{CtxConfig, EnvLookup, env_from_process};
use super::event::{TranscriptUsage, input_hash};
use super::group;
use super::handoff::latest_for_repo;
use super::mail;
use super::permit;
use super::price;
use super::sessions::{self, Liveness};
use super::state::{StateDir, now_secs, repo_slug};
use super::{CtxResult, log};
use crate::commands::workflow::verification;
use crate::style::{self, Tone};

/// Bold section-header line: a blank line, then the painted title and colon
/// -- the same "\ntitle:" shape `style::section_header` documents, with the
/// title painted bold (`Tone::Emphasis`) when `colour` is on.
fn header(colour: bool, title: &str) -> String {
    format!("\n{}:", style::paint(title, Tone::Emphasis, colour))
}

/// Bold inline field label, no leading blank line -- for a line whose label
/// and value share one row (e.g. `mail: 2 unread`).
fn label(colour: bool, title: &str) -> String {
    style::paint(title, Tone::Emphasis, colour)
}

/// Keep a transcript-derived model identifier on one terminal row.
fn terminal_safe_model_id(raw: &str) -> String {
    let mut safe = String::with_capacity(raw.len());
    let mut in_control_run = false;
    for character in raw.chars() {
        if character.is_control() {
            if !in_control_run {
                safe.push(' ');
                in_control_run = true;
            }
        } else {
            safe.push(character);
            in_control_run = false;
        }
    }
    safe
}

fn model_change_status_text(change: &super::event::ModelChange) -> String {
    format!(
        "model changed mid-session {} turns ago: `{}` -> `{}`",
        change.turns_ago,
        terminal_safe_model_id(&change.from),
        terminal_safe_model_id(&change.to)
    )
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
/// Issue #310: the display label for one restart-chain failure class --
/// hyphenated, matching the acceptance criterion's own wording ("restart
/// chain: crash 1, usage-limit 2"), distinct from `FailureClass`'s
/// `snake_case` serde spelling used on disk.
fn chain_class_label(class: chain::FailureClass) -> &'static str {
    match class {
        chain::FailureClass::Crash => "crash",
        chain::FailureClass::Stalled => "stalled",
        chain::FailureClass::UsageLimit => "usage-limit",
        chain::FailureClass::AuthBlocked => "auth-blocked",
        chain::FailureClass::Protocol => "protocol",
        chain::FailureClass::Budget => "budget",
    }
}

fn sessions_lines(
    records: &[(sessions::Record, Liveness)],
    state: &StateDir,
    now: u64,
    env: EnvLookup<'_>,
    colour: bool,
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
            let is_live = matches!(liveness, Liveness::Live) && record.reachable;
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
            let liveness_word = match (liveness, record.reachable) {
                (Liveness::Stale, _) => "dead",
                (Liveness::Live, true) => "live",
                (Liveness::Live, false) => "unreachable",
            };
            let liveness_tone = match liveness_word {
                "live" => Tone::Ok,
                "unreachable" => Tone::Warn,
                _ => Tone::Err,
            };
            let bullet = if is_live {
                style::paint("\u{25cf}", Tone::Ok, colour)
            } else {
                style::paint("\u{25cb}", Tone::Muted, colour)
            };
            let mut line = format!(
                "  {} {}  {}  {}  pid {}  {}  {}  {}",
                bullet,
                style::paint(&record.short, Tone::Accent, colour),
                style::paint(&record.agent, Tone::Accent, colour),
                style::paint(&record.verb.to_string(), Tone::Accent, colour),
                style::paint(&record.pid.to_string(), Tone::Muted, colour),
                style::paint(
                    &crate::style::format_age(now.saturating_sub(record.started_at)),
                    Tone::Muted,
                    colour
                ),
                style::paint(liveness_word, liveness_tone, colour),
                style::paint(&record.repo_slug, Tone::Muted, colour),
            );
            // Issue #139: named here, not just silently folded into the
            // stricter verdict a hook prompt would show -- an operator
            // reading `status` has no other way to learn a live session is
            // running a narrower policy than the repo currently resolves
            // to.
            if policy_snapshot_is_stale(record, env) {
                line.push_str(&format!(
                    "  {}",
                    style::paint(
                        "policy snapshot stale (current policy is wider); relaunch to adopt",
                        Tone::Warn,
                        colour
                    )
                ));
            }
            // Issue #243 (review round, F1): the last scoring cycle's own
            // screening finding -- its own sibling file next to the
            // record, never a field on the record itself (see `sessions::
            // last_screening`/`set_last_screening`'s own doc comments for
            // why: an unlocked write onto the live record could clobber a
            // concurrent `SessionGuard` write).
            if let Some(summary) =
                sessions::last_screening(state, &record.short).filter(|s| !s.is_empty())
            {
                line.push_str(&format!(
                    "  {}",
                    style::paint(&format!("screening: {summary}"), Tone::Warn, colour)
                ));
            }
            if let Some(change) = super::score::model_change_for_session(
                &record.agent,
                &record.session,
                &record.repo,
                env,
            ) {
                line.push_str(&format!(
                    "  {}",
                    style::paint(&model_change_status_text(&change), Tone::Warn, colour)
                ));
            }
            let delivery = mail::session_delivery_metrics(state, &record.short, now);
            line.push_str(&format!(
                "  {}",
                style::paint(
                    &format!(
                        "mail queue {} unread {} recent in:{} out:{}",
                        delivery.queued, delivery.unread, delivery.recent_in, delivery.recent_out
                    ),
                    Tone::Muted,
                    colour
                )
            ));
            // Issue #310 (3b): this session's own restart-chain counters,
            // keyed by repo like every other per-session disk read here --
            // only shown when at least one class has a non-zero count, so a
            // repository that has never tripped anything gains no line.
            if let Ok(Some(chain_record)) = chain::load(state, &record.repo_slug) {
                let counts = chain::counts_by_class(&chain_record);
                if !counts.is_empty() {
                    let parts: Vec<String> = counts
                        .iter()
                        .map(|(class, n)| format!("{} {n}", chain_class_label(*class)))
                        .collect();
                    line.push_str(&format!(
                        "  {}",
                        style::paint(
                            &format!("restart chain: {}", parts.join(", ")),
                            Tone::Warn,
                            colour
                        )
                    ));
                }
            }
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
    lines.extend(orphan_sockets.into_iter().map(|short| {
        format!(
            "  {}  {}",
            style::paint(&short, Tone::Accent, colour),
            style::paint("(no record)", Tone::Muted, colour)
        )
    }));

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
    colour: bool,
) -> String {
    let Some(wg) = group else {
        return format!("  {}", style::paint(fallback_id, Tone::Accent, colour));
    };
    let status = if wg.closed_at.is_some() {
        "closed"
    } else {
        "open"
    };
    let status_tone = if wg.closed_at.is_some() {
        Tone::Muted
    } else {
        Tone::Ok
    };
    let mut header = format!(
        "  {} [{}] {}",
        style::paint(&wg.work_group_id, Tone::Accent, colour),
        style::paint(status, status_tone, colour),
        style::paint(
            &format!("scope=\"{}\" child_limit={}", wg.scope, wg.child_limit),
            Tone::Muted,
            colour
        ),
    );
    if let Some(budget) = wg.token_budget {
        header.push_str(&style::paint(
            &format!(" budget={budget}"),
            Tone::Muted,
            colour,
        ));
    }
    if let Some(deadline) = wg.deadline_secs {
        header.push_str(&style::paint(
            &format!(" deadline={deadline}s"),
            Tone::Muted,
            colour,
        ));
    }
    if let Some(sub) = &wg.sub_orchestrator_session {
        header.push_str(&style::paint(
            &format!(" sub-orchestrator={sub}"),
            Tone::Muted,
            colour,
        ));
    }
    let claimant_alive = wg
        .sub_orchestrator_session
        .as_deref()
        .is_some_and(|s| live_shorts.contains(s));
    if group::is_abandoned(wg, claimant_alive) {
        header.push_str(&format!(
            " {}",
            style::paint("ABANDONED", Tone::Err, colour)
        ));
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
    colour: bool,
) {
    lines.push(header);

    let mut totals = [0u64; 4];
    for row in children {
        let outcome = if live_sessions.contains(row.session.as_str()) {
            "running"
        } else {
            row.outcome.as_str()
        };
        let outcome_tone = if outcome == "running" {
            Tone::Ok
        } else {
            Tone::Plain
        };
        let model = row
            .model
            .as_deref()
            .map(|m| format!(" ({m})"))
            .unwrap_or_default();
        lines.push(format!(
            "    {}  {}{}  {}  wall {}  {}",
            style::paint(&row.session, Tone::Accent, colour),
            style::paint(&row.agent, Tone::Accent, colour),
            style::paint(&model, Tone::Muted, colour),
            style::paint(
                &format!(
                    "input {} | cache_creation {} | cache_read {} | output {}",
                    row.input_tokens,
                    row.cache_creation_input_tokens,
                    row.cache_read_input_tokens,
                    row.output_tokens
                ),
                Tone::Muted,
                colour
            ),
            style::paint(
                &crate::style::format_age(row.wall_ms / 1000),
                Tone::Muted,
                colour
            ),
            style::paint(outcome, outcome_tone, colour),
        ));
        totals[0] = totals[0].saturating_add(row.input_tokens);
        totals[1] = totals[1].saturating_add(row.cache_creation_input_tokens);
        totals[2] = totals[2].saturating_add(row.cache_read_input_tokens);
        totals[3] = totals[3].saturating_add(row.output_tokens);
    }

    lines.push(format!(
        "    {}",
        style::paint(
            &format!(
                "total: input {} | cache_creation {} | cache_read {} | output {}",
                totals[0], totals[1], totals[2], totals[3]
            ),
            Tone::Muted,
            colour
        )
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
    colour: bool,
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
            group_header(Some(wg), "", &live_shorts, colour),
            &children,
            &live_sessions,
            colour,
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
            group_header(None, id, &live_shorts, colour),
            &children,
            &live_sessions,
            colour,
        );
    }

    let ungrouped: Vec<&log::DelegationRow> = delegations
        .iter()
        .filter(|d| d.work_group_id.is_none())
        .collect();
    if !ungrouped.is_empty() {
        push_group_block(
            &mut lines,
            group_header(None, "ungrouped", &live_shorts, colour),
            &ungrouped,
            &live_sessions,
            colour,
        );
    }

    lines
}

/// One raw `[input, cache_creation, cache_read, output]` total across
/// `children`, shared by [`group_tree_lines_brief`]'s per-group line and its
/// own grand total.
fn group_totals(children: &[&log::DelegationRow]) -> [u64; 4] {
    let mut totals = [0u64; 4];
    for row in children {
        totals[0] = totals[0].saturating_add(row.input_tokens);
        totals[1] = totals[1].saturating_add(row.cache_creation_input_tokens);
        totals[2] = totals[2].saturating_add(row.cache_read_input_tokens);
        totals[3] = totals[3].saturating_add(row.output_tokens);
    }
    totals
}

/// Issue #225 (`zirv ctx status --brief`): the same work-group accounting
/// [`group_tree_lines`] renders, collapsed to ONE line per group -- the
/// existing `group_header` line with its own totals appended, no
/// per-delegation row -- plus a final grand total across every group,
/// orphan id, and the ungrouped bucket. A session checking status at every
/// natural checkpoint (`HARNESS_PROMPT`'s own advice) does not need to see
/// each delegation again on every check; it needs "was delegating cheaper
/// than doing it here" at a glance, which is exactly what the per-group
/// total already answers.
pub fn group_tree_lines_brief(
    groups: &[group::WorkGroup],
    delegations: &[log::DelegationRow],
    records: &[(sessions::Record, Liveness)],
    colour: bool,
) -> Vec<String> {
    if delegations.is_empty() {
        return Vec::new();
    }

    let live_shorts: std::collections::BTreeSet<&str> = records
        .iter()
        .filter(|(_, liveness)| *liveness == Liveness::Live)
        .map(|(record, _)| record.short.as_str())
        .collect();

    let mut lines = Vec::new();
    let mut seen: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    let mut grand = [0u64; 4];
    let mut grand_delegations = 0usize;
    let mut grand_groups = 0usize;

    let mut summarize =
        |lines: &mut Vec<String>, header: String, children: &[&log::DelegationRow]| {
            let totals = group_totals(children);
            lines.push(format!(
                "{header}  {}",
                style::paint(
                    &format!(
                        "{} deleg. -- input {} | cache_creation {} | cache_read {} | output {}",
                        children.len(),
                        totals[0],
                        totals[1],
                        totals[2],
                        totals[3]
                    ),
                    Tone::Muted,
                    colour
                )
            ));
            for (i, total) in totals.iter().enumerate() {
                grand[i] = grand[i].saturating_add(*total);
            }
            grand_delegations += children.len();
            grand_groups += 1;
        };

    for wg in groups {
        let children: Vec<&log::DelegationRow> = delegations
            .iter()
            .filter(|d| d.work_group_id.as_deref() == Some(wg.work_group_id.as_str()))
            .collect();
        if children.is_empty() {
            continue;
        }
        seen.insert(wg.work_group_id.as_str());
        summarize(
            &mut lines,
            group_header(Some(wg), "", &live_shorts, colour),
            &children,
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
        summarize(
            &mut lines,
            group_header(None, id, &live_shorts, colour),
            &children,
        );
    }

    let ungrouped: Vec<&log::DelegationRow> = delegations
        .iter()
        .filter(|d| d.work_group_id.is_none())
        .collect();
    if !ungrouped.is_empty() {
        summarize(
            &mut lines,
            group_header(None, "ungrouped", &live_shorts, colour),
            &ungrouped,
        );
    }

    lines.push(format!(
        "  {}",
        style::paint(
            &format!(
                "grand total: {grand_delegations} deleg. across {grand_groups} groups -- input \
                 {} | cache_creation {} | cache_read {} | output {}",
                grand[0], grand[1], grand[2], grand[3]
            ),
            Tone::Emphasis,
            colour
        )
    ));

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
fn describe_chat(cfg: &CtxConfig, colour: bool) -> String {
    match adapters::resolve_default(cfg) {
        Ok((adapter, origin)) => {
            let rule = match origin {
                DefaultOrigin::Configured => "configured",
                DefaultOrigin::FirstEnabledReady => "first enabled and ready",
            };
            format!(
                "{} {} ({})",
                label(colour, "chat:"),
                style::paint(adapter.name(), Tone::Accent, colour),
                style::paint(rule, Tone::Muted, colour)
            )
        }
        Err(e) => {
            let full = e.to_string();
            let reasons: Vec<&str> = full.lines().skip(1).collect();
            let detail = if reasons.is_empty() {
                full.clone()
            } else {
                reasons.join("; ")
            };
            format!(
                "{} {} ({detail})",
                label(colour, "chat:"),
                style::paint("unavailable", Tone::Err, colour)
            )
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
    /// Collapse every unbounded section (work groups, sessions) to its
    /// totals -- every section still appears, just without a line per
    /// delegation or per session. Issue #225: `HARNESS_PROMPT` tells a
    /// session to check `zirv ctx status` at natural checkpoints, so the
    /// default view's one-line-per-delegation work-group tree and
    /// one-line-per-session list were paid on every such check; `--brief`
    /// is the cheap version of the same read. `--decisions` is ignored in
    /// this mode: the "recent decisions" section is omitted, with a note
    /// pointing back at the full view.
    #[arg(long, default_value_t = false)]
    pub brief: bool,
    /// Print only the sections whose rendered text changed since this
    /// session's previous `--diff` call, using a small per-session snapshot
    /// kept in the state dir (`StateDir::status_snapshots`, keyed by
    /// `ZIRV_CTX_SESSION`). The diff view is plain text: sections either
    /// changed or did not, so there is nothing color-coded to preserve.
    /// Without `--diff`, output is unaffected -- no snapshot is read or
    /// written.
    #[arg(long, default_value_t = false)]
    pub diff: bool,
    /// Issue #312: print a single window-attribution table for `<session>`
    /// (a registered session id, `sessions::Record::session`) instead of the
    /// ordinary report -- bucket, tokens, and percentage of the model's
    /// resolved context window, reusing this command's own session
    /// resolution (`sessions::list`) rather than a second lookup mechanism.
    #[arg(long, value_name = "SESSION")]
    pub breakdown: Option<String>,
}

/// Issue #264: the `spend:` line's own computation, factored out so a test
/// can drive it directly against a fixture ledger without rendering the
/// whole report. Sums `zirv ctx spend`'s identical per-row `price::price`
/// call over two slices of the same `delegations.jsonl` ledger: every row
/// this session's own short id delegated ("this session"), and every row
/// completed in the trailing 5 hours regardless of who delegated it ("this
/// 5h window") -- a time-boxed slice of the ledger itself, never the
/// vendor's own rate-limit window (`pace::current_windows` tracks that
/// separately, in tokens, not dollars). A row whose model has no price
/// (`price::price` returning `None`) contributes nothing to either sum,
/// mirroring `spend::SpendRow`'s own "never a phantom zero" rule.
fn spend_status_line(
    state: &StateDir,
    cfg: &CtxConfig,
    env: EnvLookup<'_>,
    colour: bool,
) -> String {
    let table = price::resolve_table(cfg);
    let now = now_secs();
    let stale = table.is_stale(now, cfg.price.stale_after_days);
    let session_ident = mail::session_identity(env);
    let five_hours_ago = now.saturating_sub(5 * 3_600);

    let mut session_micros: u64 = 0;
    let mut window_micros: u64 = 0;
    for row in log::read_delegations(state, usize::MAX) {
        let Some(model) = row.model.as_deref() else {
            continue;
        };
        let usage = TranscriptUsage {
            input_tokens: row.input_tokens,
            cache_creation_input_tokens: row.cache_creation_input_tokens,
            cache_read_input_tokens: row.cache_read_input_tokens,
            output_tokens: row.output_tokens,
        };
        let Some(cost) = price::price(model, &usage, &table) else {
            continue;
        };
        if session_ident.as_deref() == Some(row.parent_session.as_str()) {
            session_micros = session_micros.saturating_add(cost);
        }
        if row.ts >= five_hours_ago {
            window_micros = window_micros.saturating_add(cost);
        }
    }

    format!(
        "{} {} this session \u{b7} {} this 5h window (prices as of {})",
        label(colour, "spend:"),
        price::format_usd(session_micros, stale),
        price::format_usd(window_micros, stale),
        table.as_of,
    )
}

/// Issues #328/#334: one line naming how many writes an orchestrator seat's
/// own guard has refused -- `None` when `log::read_orchestrator_blocks`
/// returns nothing, so an unmodified report's bytes stay byte-identical.
/// "This session" sums rows whose `session` matches `mail::
/// session_identity(env)`, the identical rule `spend_status_line` uses for
/// its own "this session" figure; "total" is every row ever logged, and
/// "last" is the newest row (`log::read_orchestrator_blocks` returns oldest
/// first).
fn orchestrator_blocks_status_line(
    state: &StateDir,
    env: EnvLookup<'_>,
    colour: bool,
) -> Option<String> {
    let rows = log::read_orchestrator_blocks(state);
    let last = rows.last()?;
    let session_ident = mail::session_identity(env);
    let this_session = rows
        .iter()
        .filter(|row| session_ident.as_deref() == Some(row.session.as_str()))
        .count();
    Some(format!(
        "{} {}",
        label(colour, "orchestrator writes blocked:"),
        style::paint(
            &format!(
                "{this_session} this session \u{b7} {} total (last: {} {})",
                rows.len(),
                last.tool,
                last.target,
            ),
            Tone::Warn,
            colour,
        )
    ))
}

fn render_report<W: Write>(
    args: &StatusArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
    colour: bool,
) -> CtxResult<i32> {
    let state = StateDir::resolve(env)?;
    writeln!(
        w,
        "{} {}",
        label(colour, "state dir:"),
        style::paint(&state.root().display().to_string(), Tone::Muted, colour)
    )?;

    // Issue #309: presentation only, off the same `latest_is_fresh_and_
    // passing` call the Stop hook's own verify-on-stop nudge uses -- omitted
    // outright (rather than a third "unknown" wording) when no report has
    // ever been persisted for this repo, or when the git-backed check itself
    // fails: `status` must never fail just because this one line could not
    // be computed.
    if let Ok(Some(_)) = verification::latest_report_id(&state, repo) {
        let fresh = verification::latest_is_fresh_and_passing(&state, repo, false).unwrap_or(false);
        let (text, tone) = if fresh {
            ("gates: fresh".to_string(), Tone::Ok)
        } else {
            (
                "gates: stale (edits after the last passing run)".to_string(),
                Tone::Warn,
            )
        };
        writeln!(w, "{}", style::paint(&text, tone, colour))?;
    }

    match crate::settings::AgentGate::load(repo, env) {
        Ok(gate) => {
            if args.brief {
                let parts: Vec<String> = crate::commands::ctx::adapters::all(None)
                    .into_iter()
                    .map(|adapter| {
                        let name = adapter.name();
                        let (enabled, location) = gate
                            .states()
                            .find(|(n, _)| *n == name)
                            .map(|(_, s)| (s.enabled, s.location()))
                            .unwrap_or((true, "default".to_string()));
                        let state_word = if enabled { "enabled" } else { "disabled" };
                        format!("{name} {state_word} ({location})")
                    })
                    .collect();
                writeln!(w, "{} {}", label(colour, "agents:"), parts.join(", "))?;
            } else {
                writeln!(w, "{}", header(colour, "agents"))?;
                for adapter in crate::commands::ctx::adapters::all(None) {
                    let name = adapter.name();
                    let (enabled, location) = gate
                        .states()
                        .find(|(n, _)| *n == name)
                        .map(|(_, s)| (s.enabled, s.location()))
                        .unwrap_or((true, "default".to_string()));
                    let state_word = if enabled { "enabled" } else { "disabled" };
                    let state_tone = if enabled { Tone::Ok } else { Tone::Muted };
                    writeln!(
                        w,
                        "  {} {} ({})",
                        style::paint(&format!("{name:<8}"), Tone::Accent, colour),
                        style::paint(&format!("{state_word:<8}"), state_tone, colour),
                        style::paint(&location, Tone::Muted, colour),
                    )?;
                }
            }
        }
        Err(e) => writeln!(
            w,
            "\n{}",
            style::paint(
                &format!("agents: (settings unreadable: {e})"),
                Tone::Emphasis,
                colour
            )
        )?,
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
            writeln!(w, "\n{}", describe_chat(cfg, colour))?;
            for layer in &cfg.unparsable_layers {
                writeln!(
                    w,
                    "{}",
                    style::paint(
                        &format!(
                            "config: {} unparsable ({}) \u{2014} layer ignored",
                            layer.path.display(),
                            layer.message
                        ),
                        Tone::Warn,
                        colour
                    )
                )?;
            }
            if let Some(line) = describe_injection_fallback(cfg) {
                writeln!(w, "{}", style::paint(&line, Tone::Warn, colour))?;
            }
            writeln!(
                w,
                "fallback: {} | order {} | steer below {:.0}% headroom | candidate min {:.0}% | unknown assumes {:.0}%",
                if cfg.fallback.enabled {
                    "enabled"
                } else {
                    "disabled"
                },
                if cfg.fallback.order.is_empty() {
                    "(none)".to_string()
                } else {
                    cfg.fallback.order.join(" -> ")
                },
                cfg.fallback.predictive_headroom_pct,
                cfg.fallback.min_candidate_headroom_pct,
                cfg.fallback.unknown_headroom_pct,
            )?;
            // Issue #264: one line naming what delegating has actually cost --
            // the question `log::Delegation`'s own doc comment says the
            // ledger exists to answer, surfaced where an operator already
            // looks first. Unconditional (present in `--brief` too, unlike
            // the per-harness fallback detail just below) and fixed to
            // exactly two numbers, so `--brief`'s own "unchanged in bytes
            // except this line" contract holds no matter how large the
            // ledger grows. Reads the same `delegations.jsonl` `zirv ctx
            // spend` aggregates; "this session" sums rows this session's own
            // short id delegated, "this 5h window" sums every row completed
            // in the trailing 5 hours regardless of who delegated it -- a
            // time-boxed slice of the ledger itself, not the vendor's own
            // rate-limit window (`pace::current_windows` tracks that
            // separately, in tokens, not dollars).
            writeln!(w, "{}", spend_status_line(&state, cfg, env, colour))?;
            // Issues #328/#334: present in `--brief` too, the same allowance
            // `spend:` gets, and silent (no line at all) when nothing has
            // ever been blocked -- see `orchestrator_blocks_status_line`.
            if let Some(line) = orchestrator_blocks_status_line(&state, env, colour) {
                writeln!(w, "{line}")?;
            }
            if cfg.fallback.enabled && !args.brief {
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
                    let ready = adapters::select(Some(name), &[], cfg).is_ok();
                    let readiness = if ready { "ready" } else { "unavailable" };
                    let enabled = cfg.agents.is_enabled(name);
                    let enabled_word = if enabled { "enabled" } else { "disabled" };
                    writeln!(
                        w,
                        "  {} {}: {} / {} / {} / {}",
                        label(colour, "fallback"),
                        style::paint(name, Tone::Accent, colour),
                        style::paint(
                            enabled_word,
                            if enabled { Tone::Ok } else { Tone::Muted },
                            colour
                        ),
                        style::paint(readiness, if ready { Tone::Ok } else { Tone::Err }, colour),
                        style::paint(capacity, Tone::Muted, colour),
                        style::paint(&headroom, Tone::Muted, colour),
                    )?;
                }
            }
        }
        Err(e) if repo_forbidden => writeln!(
            w,
            "\n{}",
            style::paint(&format!("CONFIG REJECTED: {e}"), Tone::Err, colour)
        )?,
        Err(e) => writeln!(
            w,
            "\n{} {}",
            label(colour, "chat:"),
            style::paint(
                &format!("unavailable (configuration error: {e})"),
                Tone::Err,
                colour
            )
        )?,
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
        Ok(messages) => {
            let count = messages.len();
            let count_tone = if count == 0 { Tone::Muted } else { Tone::Plain };
            let swept_note = if mail_swept > 0 {
                format!(" ({mail_swept} undeliverable, swept)")
            } else {
                String::new()
            };
            writeln!(
                w,
                "{} {}",
                label(colour, "mail:"),
                style::paint(
                    &format!("\u{2709} {count} unread{swept_note}"),
                    count_tone,
                    colour
                )
            )?;
        }
        Err(_) => writeln!(
            w,
            "{} {}",
            label(colour, "mail:"),
            style::paint("(unreadable)", Tone::Err, colour)
        )?,
    }
    if !args.brief {
        let recent_mail =
            mail::recent_flow_lines(&state, crate::commands::ctx::state::now_secs(), 5);
        if !recent_mail.is_empty() {
            writeln!(w, "{}", label(colour, "mail flow (last hour):"))?;
            for line in recent_mail {
                writeln!(w, "  {}", style::paint(&line, Tone::Muted, colour))?;
            }
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
            "{} {}",
            label(colour, "heavy operations:"),
            style::paint(
                &format!(
                    "{} of {} slots in use",
                    live_permits.len(),
                    cfg.supervise.max_heavy_operations
                ),
                Tone::Muted,
                colour
            )
        )?;
        if !args.brief {
            for record in &live_permits {
                writeln!(
                    w,
                    "  {}",
                    style::paint(
                        &format!("pid {} -- {}", record.pid, record.label),
                        Tone::Muted,
                        colour
                    )
                )?;
            }
        }

        // Issue #267: the writer-permit pool's own occupancy, the exact
        // same shape as the heavy-operations block right above -- a live
        // writer permit names the tree it holds, so `--worktree`'s own
        // allocation is visible here too.
        let live_writers = permit::live_writer_records(&state);
        writeln!(
            w,
            "{} {}",
            label(colour, "writers:"),
            style::paint(
                &format!(
                    "{} of {} slots in use",
                    live_writers.len(),
                    cfg.supervise.max_writers
                ),
                Tone::Muted,
                colour
            )
        )?;
        if !args.brief {
            for record in &live_writers {
                let tree = record
                    .tree
                    .as_deref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "(unknown tree)".to_string());
                writeln!(
                    w,
                    "  {}",
                    style::paint(
                        &format!("pid {} -- {} -- {tree}", record.pid, record.label),
                        Tone::Muted,
                        colour
                    )
                )?;
            }
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
    let group_tree = if args.brief {
        group_tree_lines_brief(&groups, &delegations, &session_records, colour)
    } else {
        group_tree_lines(&groups, &delegations, &session_records, colour)
    };
    if !group_tree.is_empty() {
        writeln!(w, "{}", header(colour, "work groups"))?;
        for line in &group_tree {
            writeln!(w, "{line}")?;
        }
    }

    if args.brief {
        // Issue #225: the same live-session question `sessions_lines` answers
        // in full below, collapsed to a count plus (when this invocation is
        // itself a registered session) which one it is -- the fact a session
        // reading its own status at a checkpoint actually needs.
        let live_count = session_records
            .iter()
            .filter(|(record, liveness)| matches!(liveness, Liveness::Live) && record.reachable)
            .count();
        let this_line = env(super::adapters::SESSION_ENV)
            .map(|full| sessions::short_id(&full))
            .and_then(|short| {
                session_records
                    .iter()
                    .find(|(record, _)| record.short == short)
            })
            .map(|(record, _)| format!(" (this session {} {})", record.short, record.verb))
            .unwrap_or_default();
        writeln!(
            w,
            "{} {}",
            label(colour, "sessions:"),
            style::paint(
                &format!("{live_count} live{this_line}"),
                Tone::Muted,
                colour
            )
        )?;
    } else {
        writeln!(w, "{}", header(colour, "sessions"))?;
        let session_lines = sessions_lines(
            &session_records,
            &state,
            crate::commands::ctx::state::now_secs(),
            env,
            colour,
        );
        if session_lines.is_empty() {
            writeln!(
                w,
                "  {}",
                style::paint("no supervised sessions", Tone::Muted, colour)
            )?;
        } else {
            for line in &session_lines {
                writeln!(w, "{line}")?;
            }
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
        writeln!(
            w,
            "{} {}",
            label(colour, "memory:"),
            style::paint("empty", Tone::Muted, colour)
        )?;
    } else {
        writeln!(
            w,
            "{} {}",
            label(colour, "memory:"),
            style::paint(
                &format!(
                    "{} entries, oldest {}d, {} stale >30d",
                    memory_summary.count,
                    memory_summary.oldest_written_days.unwrap_or(0),
                    memory_summary.stale_count
                ),
                Tone::Muted,
                colour
            )
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
                "{} {}",
                header(colour, "usage windows"),
                style::paint(
                    &format!(
                        "{provider}: no usage source ({})",
                        crate::commands::ctx::poll::usage_source_hint(provider)
                    ),
                    Tone::Muted,
                    colour
                )
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
                    Some(found) => format!("{name} {}", style::format_pct(found.used_percentage)),
                    None => format!("{name} {}", style::PLACEHOLDER),
                };
            writeln!(
                w,
                "{} {}",
                header(colour, "usage windows"),
                style::paint(
                    &format!(
                        "{}, {} (see `zirv ctx usage` for detail)",
                        describe("five_hour", windows.five_hour.as_ref()),
                        describe("seven_day", windows.seven_day.as_ref())
                    ),
                    Tone::Muted,
                    colour
                )
            )?;
        }
        None => {}
    }

    if args.brief {
        match latest_for_repo(&state, repo)? {
            Some((_, handoff)) => writeln!(
                w,
                "{} {}",
                label(colour, "handoff:"),
                style::paint(
                    &format!("{} -- next: {}", handoff.task, handoff.next_step),
                    Tone::Muted,
                    colour
                )
            )?,
            None => writeln!(
                w,
                "{} {}",
                label(colour, "handoff:"),
                style::paint("none stored", Tone::Muted, colour)
            )?,
        }
    } else {
        writeln!(
            w,
            "\n{}",
            style::paint(
                &format!("latest handoff for {}:", repo.display()),
                Tone::Emphasis,
                colour
            )
        )?;
        match latest_for_repo(&state, repo)? {
            Some((path, handoff)) => {
                writeln!(
                    w,
                    "  {}",
                    style::paint(&path.display().to_string(), Tone::Muted, colour)
                )?;
                writeln!(w, "  task: {}", handoff.task)?;
                writeln!(w, "  next: {}", handoff.next_step)?;
            }
            None => writeln!(
                w,
                "  {}",
                style::paint("no handoff stored", Tone::Muted, colour)
            )?,
        }
    }

    if args.brief {
        writeln!(
            w,
            "{} {}",
            label(colour, "decisions:"),
            style::paint(
                &format!("run without --brief for the last {}", args.decisions),
                Tone::Muted,
                colour
            )
        )?;
    } else {
        writeln!(w, "{}", header(colour, "recent decisions"))?;
        let lines = log::tail(&state, args.decisions)?;
        if lines.is_empty() {
            writeln!(
                w,
                "  {}",
                style::paint("none recorded", Tone::Muted, colour)
            )?;
        } else {
            for line in lines.iter().rev() {
                writeln!(w, "  {line}")?;
            }
        }
    }

    // Non-zero only for a `REPO_FORBIDDEN` security refusal -- see the doc
    // comment above the `cfg_result` match for why every other config-load
    // outcome (success, a skipped-unparsable layer, or any other load error)
    // keeps exiting 0.
    Ok(if repo_forbidden { 1 } else { 0 })
}

/// Issue #246: `status --diff`'s schema version for [`StatusSnapshot`].
/// Bumping this on any future field/shape change makes an old snapshot
/// silently fall back to "no snapshot yet" via [`load_status_snapshot`]
/// rather than fail to parse.
const STATUS_SNAPSHOT_SCHEMA_VERSION: u32 = 1;

/// A `status --diff` session's snapshot of the previous call's rendered
/// sections, one file per session id
/// (`StateDir::status_snapshots().join("<hash>.json")`), mirroring
/// `hook.rs`'s `AdoptionRecord`/`adoption_record_path`/`load_adoption_
/// record`/`save_adoption_record` pattern exactly.
///
/// `brief` and `repo_slug` are both part of the identity a stored snapshot
/// must match to be usable: a `--brief --diff` snapshot and a plain
/// `--diff` snapshot render different section sets for the same session id,
/// and a slug recorded for a since-moved/renamed repo checkout is no longer
/// this repo's own status. Either mismatch -- like a missing or corrupt
/// file, or a schema version this build no longer understands -- is treated
/// as "no snapshot yet" rather than surfaced as an error: `status --diff`
/// must never fail just because its own bookkeeping is stale.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct StatusSnapshot {
    schema_version: u32,
    brief: bool,
    repo_slug: String,
    saved_at_secs: u64,
    sections: Vec<(String, String)>,
}

/// One rendered section of a `status` report: `key` is the text before the
/// first `:` on the section's first line (or the whole line, when there is
/// no colon); `body` is every line belonging to the section, including its
/// first, joined with `\n`. Pure text splitting -- no fs/clock/env -- so
/// `--diff`'s unit tests exercise it directly.
///
/// A section starts at every non-empty line whose first character is not
/// whitespace -- `render_report`'s own convention throughout this file:
/// top-level lines start at column 0 (`key: ...`), continuation lines are
/// indented. Blank lines (used only to visually separate sections) are
/// dropped entirely: they carry no content to diff, and keeping them would
/// force every snapshot to also track exact blank-line placement. A
/// duplicate key (e.g. two "work group:" lines) is disambiguated with
/// `#2`, `#3`, ... suffixes in encounter order, so [`diff_sections`] can
/// still tell separate sections apart by key alone.
fn split_sections(text: &str) -> Vec<(String, String)> {
    let mut raw: Vec<(String, Vec<&str>)> = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let starts_new_section = raw.is_empty() || !line.starts_with(char::is_whitespace);
        if starts_new_section {
            let key = line.split(':').next().unwrap_or(line).to_string();
            raw.push((key, vec![line]));
        } else {
            raw.last_mut()
                .expect("starts_new_section is true when raw is empty")
                .1
                .push(line);
        }
    }

    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    raw.into_iter()
        .map(|(key, lines)| {
            let ordinal = seen.entry(key.clone()).and_modify(|n| *n += 1).or_insert(1);
            let key = if *ordinal == 1 {
                key
            } else {
                format!("{key} #{ordinal}")
            };
            (key, lines.join("\n"))
        })
        .collect()
}

/// The result of comparing two [`split_sections`] outputs, keyed by their
/// already-disambiguated section key.
#[derive(Debug, Clone, Default, PartialEq)]
struct SectionDiff {
    /// Sections new or changed in `cur`, in `cur`'s own order.
    changed: Vec<(String, String)>,
    /// Keys present in `prev` but absent from `cur`.
    removed: Vec<String>,
}

/// Pure diff between two section lists. No fs/clock/env.
fn diff_sections(prev: &[(String, String)], cur: &[(String, String)]) -> SectionDiff {
    let prev_by_key: std::collections::HashMap<&str, &str> = prev
        .iter()
        .map(|(key, body)| (key.as_str(), body.as_str()))
        .collect();
    let cur_keys: std::collections::HashSet<&str> =
        cur.iter().map(|(key, _)| key.as_str()).collect();

    let changed = cur
        .iter()
        .filter(|(key, body)| prev_by_key.get(key.as_str()) != Some(&body.as_str()))
        .cloned()
        .collect();
    let removed = prev
        .iter()
        .filter(|(key, _)| !cur_keys.contains(key.as_str()))
        .map(|(key, _)| key.clone())
        .collect();
    SectionDiff { changed, removed }
}

/// `<state>/status-snapshots/<hash of session>.json`, mirroring `hook.rs`'s
/// `adoption_record_path` exactly.
fn status_snapshot_path(state: &StateDir, session: &str) -> std::path::PathBuf {
    state
        .status_snapshots()
        .join(format!("{:016x}.json", input_hash(session)))
}

/// Best-effort, like `hook.rs`'s `load_adoption_record`: missing, corrupt,
/// a schema version this build does not recognize, or a snapshot saved
/// under a different `--brief`/repo identity all read as "no snapshot yet"
/// (`None`) rather than an error.
fn load_status_snapshot(path: &Path, brief: bool, repo_slug: &str) -> Option<StatusSnapshot> {
    let body = std::fs::read_to_string(path).ok()?;
    let snapshot: StatusSnapshot = serde_json::from_str(&body).ok()?;
    if snapshot.schema_version != STATUS_SNAPSHOT_SCHEMA_VERSION
        || snapshot.brief != brief
        || snapshot.repo_slug != repo_slug
    {
        return None;
    }
    Some(snapshot)
}

/// Best-effort, like `hook.rs`'s `save_adoption_record`: a snapshot that
/// fails to write costs the next `--diff` call a "no snapshot yet" reset,
/// never a `status` failure. Prunes the directory to `KEEP_NEWEST` after a
/// successful write, the same retention `adoption()` gets.
fn save_status_snapshot(state: &StateDir, path: &Path, snapshot: &StatusSnapshot) {
    let Ok(json) = serde_json::to_string(snapshot) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = super::state::create_private_dir_all(dir);
    }
    if super::state::write_private(path, &json).is_ok() {
        super::state::prune_to_newest(&state.status_snapshots(), super::state::KEEP_NEWEST);
    }
}

/// Issue #312: `zirv ctx status --breakdown <session>` -- one deterministic
/// bucket/tokens/percentage-of-window table. `score::breakdown_for_session`
/// (I/O: session lookup, transcript read, compile-context bytes) supplies
/// the numbers; [`render_breakdown_table`] (pure) renders them, so the
/// table's own shape is unit-tested without a real session or transcript.
fn render_breakdown<W: Write>(
    session: &str,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    let (summary, window_tokens) = super::score::breakdown_for_session(session, repo, env)?;
    write!(
        w,
        "{}",
        render_breakdown_table(session, &summary, window_tokens)
    )?;
    Ok(0)
}

/// Pure rendering half of [`render_breakdown`]. Every bucket row prints its
/// token count and its percentage of `window_tokens` (the model's resolved
/// context window) when known, or the literal `unknown` when it is not --
/// never a fabricated percentage against a denominator zirv does not
/// actually have. The `tool_schemas` row appears only when the summary
/// itself carries one (see `BreakdownSummary::tool_schemas`'s own doc
/// comment: absent, not a fabricated zero, when it was not computable), and
/// a `stale source` line appears only when at least one byte is stale.
fn render_breakdown_table(
    session: &str,
    summary: &super::breakdown::BreakdownSummary,
    window_tokens: Option<u64>,
) -> String {
    let pct = |tokens: u64| match window_tokens {
        Some(window) if window > 0 => format!("{:.1}%", (tokens as f64 / window as f64) * 100.0),
        _ => "unknown".to_string(),
    };
    let mut rows: Vec<(&str, u64)> = vec![("system_and_layers", summary.system_and_layers)];
    if let Some(schema) = summary.tool_schemas {
        rows.push(("tool_schemas", schema));
    }
    rows.push(("tool_results_live", summary.tool_results_live));
    rows.push(("tool_results_stale", summary.tool_results_stale));
    rows.push(("assistant_text", summary.assistant_text));
    rows.push(("user_text", summary.user_text));
    rows.push(("thinking", summary.thinking));

    let mut out = format!("window breakdown for {session}\n");
    out.push_str(&format!(
        "{:<20}{:>12}{:>10}\n",
        "bucket", "tokens", "% window"
    ));
    for (name, tokens) in &rows {
        out.push_str(&format!("{name:<20}{tokens:>12}{:>10}\n", pct(*tokens)));
    }
    out.push_str(&format!(
        "{:<20}{:>12}{:>10}\n",
        "total",
        summary.total_tokens,
        pct(summary.total_tokens)
    ));
    if let Some(source) = &summary.stale_source {
        out.push_str(&format!("stale source: {source}\n"));
    }
    out
}

pub fn run_with<W: Write>(
    args: &StatusArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
    colour: bool,
) -> CtxResult<i32> {
    if let Some(session) = &args.breakdown {
        return render_breakdown(session, w, repo, env);
    }
    if !args.diff {
        return render_report(args, w, repo, env, colour);
    }

    let Some(session) = mail::session_identity(env) else {
        writeln!(
            w,
            "status --diff: no session identity (ZIRV_CTX_SESSION unset); showing the full \
             report"
        )?;
        return render_report(args, w, repo, env, colour);
    };

    let state = StateDir::resolve(env)?;
    let slug = repo_slug(repo);
    let path = status_snapshot_path(&state, &session);
    let previous = load_status_snapshot(&path, args.brief, &slug);

    let mut rendered = Vec::new();
    let code = render_report(args, &mut rendered, repo, env, false)?;
    let text = String::from_utf8(rendered)
        .map_err(|e| format!("status --diff: rendered report was not valid UTF-8: {e}"))?;
    let sections = split_sections(&text);

    match &previous {
        None => {
            writeln!(
                w,
                "status --diff: no snapshot for this session yet; full report follows"
            )?;
            write!(w, "{text}")?;
        }
        Some(snapshot) => {
            let SectionDiff { changed, removed } = diff_sections(&snapshot.sections, &sections);
            let age = style::format_age(now_secs().saturating_sub(snapshot.saved_at_secs));
            if changed.is_empty() && removed.is_empty() {
                writeln!(
                    w,
                    "status --diff: no change since {age} ({} sections)",
                    sections.len()
                )?;
            } else {
                writeln!(
                    w,
                    "status --diff: {} of {} sections changed since {age}",
                    changed.len(),
                    sections.len()
                )?;
                for (_, body) in &changed {
                    writeln!(w, "{body}")?;
                }
                for key in &removed {
                    writeln!(w, "{key}: (no longer reported)")?;
                }
            }
        }
    }

    save_status_snapshot(
        &state,
        &path,
        &StatusSnapshot {
            schema_version: STATUS_SNAPSHOT_SCHEMA_VERSION,
            brief: args.brief,
            repo_slug: slug,
            saved_at_secs: now_secs(),
            sections,
        },
    );

    Ok(code)
}

pub fn run<W: Write>(args: &StatusArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    run_with(args, w, &repo, &env, console::colors_enabled())
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

    /// Issue #312: the pure rendering half of `--breakdown`, exercised
    /// directly against a hand-built summary -- no session, transcript, or
    /// compile pass involved. Pins the deterministic shape: every bucket row
    /// with its percentage of the window, the `tool_schemas` row present
    /// only when the summary carries one, and a `stale source` line only
    /// when something is actually stale.
    #[test]
    fn render_breakdown_table_shows_every_bucket_with_its_window_percentage() {
        let summary = super::super::breakdown::BreakdownSummary {
            system_and_layers: 40,
            tool_schemas: Some(10),
            tool_results_live: 20,
            tool_results_stale: 10,
            assistant_text: 15,
            user_text: 3,
            thinking: 2,
            total_tokens: 100,
            stale_source: Some("Read".to_string()),
        };
        let text = render_breakdown_table("sess-1", &summary, Some(1000));
        assert!(
            text.starts_with("window breakdown for sess-1\n"),
            "got {text}"
        );
        assert!(text.contains("system_and_layers"), "got {text}");
        assert!(text.contains("tool_schemas"), "got {text}");
        assert!(text.contains("4.0%"), "40/1000 = 4.0%: got {text}");
        assert!(text.contains("total"), "got {text}");
        assert!(text.contains("10.0%"), "100/1000 = 10.0%: got {text}");
        assert!(text.contains("stale source: Read"), "got {text}");
    }

    /// A model with no known context window renders `unknown`, never a
    /// fabricated percentage against a denominator zirv does not have.
    #[test]
    fn render_breakdown_table_shows_unknown_percentage_without_a_window() {
        let summary = super::super::breakdown::BreakdownSummary {
            system_and_layers: 100,
            tool_schemas: None,
            tool_results_live: 0,
            tool_results_stale: 0,
            assistant_text: 0,
            user_text: 0,
            thinking: 0,
            total_tokens: 100,
            stale_source: None,
        };
        let text = render_breakdown_table("sess-2", &summary, None);
        assert!(text.contains("unknown"), "got {text}");
        assert!(
            !text.contains("tool_schemas"),
            "absent, not fabricated: got {text}"
        );
        assert!(!text.contains("stale source"), "nothing stale: got {text}");
    }

    #[test]
    fn an_empty_state_dir_reports_nothing_supervised() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        let env = env_for(&state);

        let mut out = Vec::new();
        let code = run_with(
            &StatusArgs {
                decisions: 10,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            false,
        )
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

    /// A repository with one commit, mirroring `verification.rs`'s own
    /// `git_repo()` test helper -- the `gates:` line's own git-backed check
    /// (`latest_is_fresh_and_passing`) needs something real to read.
    fn git_repo() -> tempfile::TempDir {
        let repo = tempfile::tempdir().expect("tempdir");
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.email=t@example.com",
                    "-c",
                    "user.name=t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .current_dir(repo.path())
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q"]);
        std::fs::write(repo.path().join("tracked.txt"), "one\n").expect("write");
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);
        repo
    }

    /// A minimal passing report for `repo`, matching its current change
    /// fingerprint -- mirrors `engine.rs`'s own seeded-report test pattern.
    fn passing_report(repo: &Path) -> verification::VerificationReport {
        verification::VerificationReport {
            schema_version: verification::VERIFY_REPORT_SCHEMA_VERSION,
            id: "seeded".into(),
            mode: verification::VerificationMode::Changed,
            source: "configured".into(),
            repo: repo.to_path_buf(),
            change_fingerprint: verification::change_fingerprint(repo).expect("fingerprint"),
            changed_paths: vec![],
            fallback_to_full: false,
            narrowed_to: vec![],
            notes: vec![],
            started_at: 0,
            finished_at: 0,
            checks: vec![verification::CheckResult {
                id: "unit".into(),
                kind: verification::CheckKind::Unit,
                command: "true".into(),
                source: verification::CheckSource::DiscoveredToolchain,
                status: verification::CheckStatus::Passed,
                exit_code: Some(0),
                duration_ms: 1,
                failure_output: None,
                failure_test_names: Vec::new(),
                inconclusive_reason: None,
            }],
        }
    }

    /// Issue #309: a report whose `change_fingerprint` still matches the
    /// current tree renders `gates: fresh`.
    #[test]
    fn status_shows_gates_fresh_for_a_report_matching_the_current_change_set() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state_root = tmp.path().join("state");
        let repo = git_repo();
        let state = StateDir::from_root(state_root.clone());
        verification::save_report(&state, &passing_report(repo.path())).expect("save report");

        let env = env_for(&state_root);
        let mut out = Vec::new();
        run_with(
            &StatusArgs {
                decisions: 10,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut out,
            repo.path(),
            &|k| env.get(k).cloned(),
            false,
        )
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("gates: fresh"), "got {text}");
    }

    /// Issue #309: once the tree changes after the report was persisted, the
    /// same report's fingerprint no longer matches, so the line flips to
    /// `gates: stale ...`.
    #[test]
    fn status_shows_gates_stale_once_the_tree_changes_after_the_last_passing_run() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state_root = tmp.path().join("state");
        let repo = git_repo();
        let state = StateDir::from_root(state_root.clone());
        verification::save_report(&state, &passing_report(repo.path())).expect("save report");
        std::fs::write(repo.path().join("tracked.txt"), "two\n").expect("edit");

        let env = env_for(&state_root);
        let mut out = Vec::new();
        run_with(
            &StatusArgs {
                decisions: 10,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut out,
            repo.path(),
            &|k| env.get(k).cloned(),
            false,
        )
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("gates: stale (edits after the last passing run)"),
            "got {text}"
        );
    }

    /// Issue #309: with no report ever persisted for this repo, the line is
    /// omitted outright rather than printed as some third "unknown" state.
    #[test]
    fn status_omits_the_gates_line_when_no_report_has_ever_been_persisted() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        let repo = git_repo();
        let env = env_for(&state);
        let mut out = Vec::new();
        run_with(
            &StatusArgs {
                decisions: 10,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut out,
            repo.path(),
            &|k| env.get(k).cloned(),
            false,
        )
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(!text.contains("gates:"), "got {text}");
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
        let code = run_with(
            &StatusArgs {
                decisions: 10,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut out,
            &repo,
            &|k| env.get(k).cloned(),
            false,
        )
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
        let code = run_with(
            &StatusArgs {
                decisions: 10,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut out,
            &repo,
            &|k| env.get(k).cloned(),
            false,
        )
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
        let code = run_with(
            &StatusArgs {
                decisions: 10,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut out,
            &repo,
            &|k| env.get(k).cloned(),
            false,
        )
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
        run_with(
            &StatusArgs {
                decisions: 10,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            false,
        )
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
        run_with(
            &StatusArgs {
                decisions: 5,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            false,
        )
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
        run_with(
            &StatusArgs {
                decisions: 5,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut out,
            &repo,
            &|k| env.get(k).cloned(),
            false,
        )
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

    /// Issue #243: a session record carrying a screening summary
    /// (`hook.rs::run_stop`'s own write) shows a `screening:` note on its
    /// status line.
    #[test]
    fn a_session_with_a_flagged_screening_summary_shows_it_on_its_status_line() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let env = env_for(state.root());

        let record = crate::commands::ctx::sessions::Record::new(
            "cccc3333-2222-4333-8444-555555555555",
            "claude",
            &repo,
            crate::commands::ctx::sessions::Verb::Wrap,
        );
        let short = record.short.clone();
        let _guard = crate::commands::ctx::sessions::SessionGuard::register(&state, record);
        // Issue #243 (review round, F1): the sibling-file store, never a
        // field on the record itself.
        crate::commands::ctx::sessions::set_last_screening(
            &state,
            &short,
            Some("1 flag: prompt-injection marker (\"ignore previous instructions\")".to_string()),
        );

        let mut out = Vec::new();
        run_with(
            &StatusArgs {
                decisions: 5,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut out,
            &repo,
            &|k| env.get(k).cloned(),
            false,
        )
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("screening: 1 flag: prompt-injection marker"),
            "got {text}"
        );
    }

    /// Issue #310 (3b): a repository with a non-zero restart-chain count for
    /// at least one failure class shows a `restart chain:` note naming every
    /// class with a non-zero count, hyphen-spelled per the acceptance
    /// criterion's own wording.
    #[test]
    fn a_session_with_a_non_zero_restart_chain_count_shows_it_on_its_status_line() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let env = env_for(state.root());

        let record = crate::commands::ctx::sessions::Record::new(
            "dddd4444-2222-4333-8444-555555555555",
            "claude",
            &repo,
            crate::commands::ctx::sessions::Verb::Wrap,
        );
        let repo_slug = record.repo_slug.clone();
        let _guard = crate::commands::ctx::sessions::SessionGuard::register(&state, record);
        chain::record_boot_and_evaluate(
            &state,
            &repo_slug,
            chain::FailureClass::Crash,
            false,
            0,
            3,
            300,
        );
        chain::record_boot_and_evaluate(
            &state,
            &repo_slug,
            chain::FailureClass::UsageLimit,
            false,
            1,
            3,
            300,
        );
        chain::record_boot_and_evaluate(
            &state,
            &repo_slug,
            chain::FailureClass::UsageLimit,
            false,
            2,
            3,
            300,
        );

        let mut out = Vec::new();
        run_with(
            &StatusArgs {
                decisions: 5,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut out,
            &repo,
            &|k| env.get(k).cloned(),
            false,
        )
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("restart chain: crash 1, usage-limit 2"),
            "got {text}"
        );
    }

    /// The absent half: a repository with no restart-chain record at all
    /// (never respawned, or the state dir has nothing stored) carries no
    /// `restart chain:` note.
    #[test]
    fn a_session_with_no_restart_chain_record_shows_no_note() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let env = env_for(state.root());

        let record = crate::commands::ctx::sessions::Record::new(
            "eeee5555-2222-4333-8444-555555555555",
            "claude",
            &repo,
            crate::commands::ctx::sessions::Verb::Wrap,
        );
        let _guard = crate::commands::ctx::sessions::SessionGuard::register(&state, record);

        let mut out = Vec::new();
        run_with(
            &StatusArgs {
                decisions: 5,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut out,
            &repo,
            &|k| env.get(k).cloned(),
            false,
        )
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(!text.contains("restart chain"), "got {text}");
    }

    /// The absent half: a session with no screening summary carries no
    /// `screening:` note at all.
    #[test]
    fn a_session_with_no_screening_summary_shows_no_note() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let env = env_for(state.root());

        let _guard = crate::commands::ctx::sessions::SessionGuard::register(
            &state,
            crate::commands::ctx::sessions::Record::new(
                "dddd4444-2222-4333-8444-555555555555",
                "claude",
                &repo,
                crate::commands::ctx::sessions::Verb::Wrap,
            ),
        );

        let mut out = Vec::new();
        run_with(
            &StatusArgs {
                decisions: 5,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut out,
            &repo,
            &|k| env.get(k).cloned(),
            false,
        )
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(!text.contains("screening:"), "got {text}");
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
        run_with(
            &StatusArgs {
                decisions: 5,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut out,
            &repo,
            &|k| env.get(k).cloned(),
            false,
        )
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
        let code = run_with(
            &StatusArgs {
                decisions: 5,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut out,
            &repo,
            &|k| env.get(k).cloned(),
            false,
        )
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
        run_with(
            &StatusArgs {
                decisions: 2,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            false,
        )
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
        run_with(
            &StatusArgs {
                decisions: 5,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            false,
        )
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
            describe_chat(&default_cfg, false),
            "chat: claude (first enabled and ready)"
        );

        let configured_cfg = CtxConfig {
            agent: Some("claude".to_string()),
            ..CtxConfig::default()
        };
        assert_eq!(
            describe_chat(&configured_cfg, false),
            "chat: claude (configured)"
        );
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
        run_with(
            &StatusArgs {
                decisions: 5,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            false,
        )
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
        run_with(
            &StatusArgs {
                decisions: 5,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            false,
        )
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");

        assert!(text.contains("mail: \u{2709} 2 unread"), "got {text}");
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
        run_with(
            &StatusArgs {
                decisions: 5,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            false,
        )
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

    /// Issue #267, acceptance criterion: a `--worktree` allocation's writer
    /// permit is listed by `zirv ctx status`, naming the tree it holds --
    /// the writer-pool counterpart of `status_reports_the_heavy_operations_
    /// budget_occupancy` right above.
    #[test]
    fn status_reports_the_writer_pool_occupancy_and_its_tree() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let mut env = env_for(state.root());
        env.insert(
            "ZIRV_CTX_SUPERVISE_MAX_WRITERS".to_string(),
            "2".to_string(),
        );
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let tree = tmp.path().join("worktree-repo");
        std::fs::create_dir_all(&tree).expect("mkdir");
        let held = permit::acquire_writer(&state, 2, "session ab12cd34: claude", &tree)
            .expect("writer permit granted");

        let mut out = Vec::new();
        run_with(
            &StatusArgs {
                decisions: 5,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            false,
        )
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");

        assert!(text.contains("writers: 1 of 2 slots in use"), "got {text}");
        assert!(
            text.contains(&tree.display().to_string()),
            "the held tree must be named: got {text}"
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
        run_with(
            &StatusArgs {
                decisions: 5,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            false,
        )
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
        run_with(
            &StatusArgs {
                decisions: 5,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            false,
        )
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");

        assert!(
            text.contains("mail: \u{2709} 2 unread"),
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
        run_with(
            &StatusArgs {
                decisions: 5,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            false,
        )
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");

        assert!(
            text.contains("mail: \u{2709} 1 unread (1 undeliverable, swept)"),
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
                    overage_covered: false,
                }),
                seven_day: None,
            },
        )
        .expect("store");

        let mut out = Vec::new();
        run_with(
            &StatusArgs {
                decisions: 5,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            false,
        )
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("usage"), "got {text}");
        assert!(text.contains("77"), "got {text}");
    }

    /// Issue #264: `spend:` names both this session's own delegated cost and
    /// the trailing-5h ledger total, priced from the built-in table -- and
    /// appears in `--brief` too (the one line that mode's own
    /// "unchanged in bytes except this line" contract allows for).
    #[test]
    fn status_reports_spend_this_session_and_this_5h_window() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");

        log::append_delegation(
            &state,
            &log::Delegation {
                ts: crate::commands::ctx::state::now_secs(),
                session: "sess-child",
                parent_session: "aaaa1111",
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
                mode: None,
                task_class: None,
                principal: "root",
                envelope_sha256: None,
            },
        )
        .expect("append");

        let mut env = env_for(state.root());
        env.insert(
            crate::commands::ctx::adapters::SESSION_ENV.to_string(),
            "aaaa1111".to_string(),
        );

        for brief in [false, true] {
            let mut out = Vec::new();
            run_with(
                &StatusArgs {
                    decisions: 5,
                    brief,
                    diff: false,
                    breakdown: None,
                },
                &mut out,
                tmp.path(),
                &|k| env.get(k).cloned(),
                false,
            )
            .expect("runs");
            let text = String::from_utf8(out).expect("utf8");
            assert!(text.contains("spend:"), "brief={brief}: got {text}");
            assert!(
                text.contains("$3.00 this session"),
                "1M input tokens @ $3/M (sonnet), attributed to this session: {text}"
            );
            assert!(text.contains("this 5h window"), "brief={brief}: got {text}");
            assert!(text.contains("prices as of"), "brief={brief}: got {text}");
        }
    }

    /// Issues #328/#334: `orchestrator writes blocked:` names both this
    /// session's own refused count and the all-time total, plus the newest
    /// blocked call -- and appears in `--brief` too, the same allowance
    /// `spend:` gets.
    #[test]
    fn status_reports_orchestrator_blocks_this_session_and_total() {
        let tmp = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");

        log::append_orchestrator_block(
            &state,
            &log::OrchestratorBlock {
                ts: crate::commands::ctx::state::now_secs(),
                session: "aaaa1111",
                tool: "Edit",
                target: "/work/repo/src/main.rs",
                reason: "orchestrator seats may not edit repository files",
            },
        )
        .expect("append");
        log::append_orchestrator_block(
            &state,
            &log::OrchestratorBlock {
                ts: crate::commands::ctx::state::now_secs(),
                session: "bbbb2222",
                tool: "Bash",
                target: "sed -i",
                reason: "orchestrator seats may not edit repository files",
            },
        )
        .expect("append");

        let mut env = env_for(state.root());
        env.insert(
            crate::commands::ctx::adapters::SESSION_ENV.to_string(),
            "aaaa1111".to_string(),
        );

        for brief in [false, true] {
            let mut out = Vec::new();
            run_with(
                &StatusArgs {
                    decisions: 5,
                    brief,
                    diff: false,
                    breakdown: None,
                },
                &mut out,
                tmp.path(),
                &|k| env.get(k).cloned(),
                false,
            )
            .expect("runs");
            let text = String::from_utf8(out).expect("utf8");
            assert!(
                text.contains(
                    "orchestrator writes blocked: 1 this session \u{b7} 2 total (last: Bash sed -i)"
                ),
                "brief={brief}: got {text}"
            );
        }
    }

    /// Nothing blocked yet must render byte-identical to before this line
    /// existed -- no `orchestrator writes blocked:` text at all.
    #[test]
    fn status_omits_the_orchestrator_blocks_line_with_no_rows() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let env = env_for(state.root());

        let mut out = Vec::new();
        run_with(
            &StatusArgs {
                decisions: 5,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            false,
        )
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(!text.contains("orchestrator writes blocked"), "got {text}");
    }

    /// The fourth surface change: a window whose `resets_at` has provably
    /// passed must read as `style::PLACEHOLDER` (issue #202: unknown values
    /// use the shared placeholder, never the word "unknown"), the same
    /// treatment the line already uses for a genuinely absent window --
    /// never a stale percent presented as current.
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
                    overage_covered: false,
                }),
                seven_day: None,
            },
        )
        .expect("store");

        let mut out = Vec::new();
        run_with(
            &StatusArgs {
                decisions: 5,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            false,
        )
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            !text.contains("77%"),
            "an expired window must not render as a current percent: {text}"
        );
        assert!(
            text.contains(&format!("five_hour {}", crate::style::PLACEHOLDER)),
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
        let _home = crate::commands::ctx::testenv::HomeGuard::set(tmp.path());
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
                    overage_covered: false,
                }),
                seven_day: None,
            },
        )
        .expect("store");

        let mut out = Vec::new();
        run_with(
            &StatusArgs {
                decisions: 5,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            false,
        )
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
        run_with(
            &StatusArgs {
                decisions: 5,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            false,
        )
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
        run_with(
            &StatusArgs {
                decisions: 5,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            false,
        )
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
        run_with(
            &StatusArgs {
                decisions: 5,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            false,
        )
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
        run_with(
            &StatusArgs {
                decisions: 5,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut out,
            &repo,
            &|k| env.get(k).cloned(),
            false,
        )
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
        run_with(
            &StatusArgs {
                decisions: 5,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut out,
            &repo,
            &|k| env.get(k).cloned(),
            false,
        )
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
        run_with(
            &StatusArgs {
                decisions: 5,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut out,
            &repo,
            &|k| env.get(k).cloned(),
            false,
        )
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
        run_with(
            &StatusArgs {
                decisions: 5,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            false,
        )
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
        run_with(
            &StatusArgs {
                decisions: 5,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            false,
        )
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
                run_with(
                    &StatusArgs {
                        decisions: 5,
                        brief: false,
                        diff: false,
                        breakdown: None,
                    },
                    &mut out,
                    tmp.path(),
                    &|k| env.get(k).cloned(),
                    false,
                )
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
        run_with(
            &StatusArgs {
                decisions: 5,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            false,
        )
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
            spent_tokens: 0,
            reserved_tokens: 0,
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
            mode: None,
            task_class: None,
            principal: "root".to_string(),
            envelope_sha256: None,
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
        let lines = group_tree_lines(&groups, &delegations, &[], false);
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
        let text = group_tree_lines(&[], &delegations, &[], false).join("\n");
        assert!(text.contains("ungrouped"), "got {text}");
        assert!(text.contains("sess-c"), "got {text}");
    }

    /// Nothing to show is nothing shown -- no empty heading on a machine that
    /// has never delegated.
    #[test]
    fn no_groups_and_no_delegations_render_nothing() {
        assert!(group_tree_lines(&[], &[], &[], false).is_empty());
    }

    /// A delegation's `work_group_id` naming a group this listing never
    /// loaded (the group's own file was never written, or has since been
    /// swept) is not dropped -- either way the spend happened, and the bare
    /// id still identifies which batch it belongs to.
    #[test]
    fn a_delegation_naming_an_unknown_group_still_shows_up() {
        let delegations = vec![delegation_row("wg-ghost", "sess-d", "codex", 100, 200, 50)];
        let text = group_tree_lines(&[], &delegations, &[], false).join("\n");
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
        let text = group_tree_lines(&groups, &[row], &[(record, Liveness::Live)], false).join("\n");
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
            false,
        )
        .join("\n");
        assert!(dead_text.contains("ABANDONED"), "got {dead_text}");

        // Alive: the exact same group, with its claimant now live.
        let alive_text =
            group_tree_lines(&[group], &[row], &[(record, Liveness::Live)], false).join("\n");
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
        let code = run_with(
            &StatusArgs {
                decisions: 5,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            false,
        )
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
        let code = run_with(
            &StatusArgs {
                decisions: 5,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            false,
        )
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
                mode: None,
                task_class: None,
                principal: "root",
                envelope_sha256: None,
            },
        )
        .expect("append");
        let env = env_for(state.root());

        let mut out = Vec::new();
        let code = run_with(
            &StatusArgs {
                decisions: 5,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            false,
        )
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
                mode: None,
                task_class: None,
                principal: "root",
                envelope_sha256: None,
            },
        )
        .expect("append");
        let env = env_for(state.root());

        let mut out = Vec::new();
        let code = run_with(
            &StatusArgs {
                decisions: 5,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            false,
        )
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
                mode: None,
                task_class: None,
                principal: "root",
                envelope_sha256: None,
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
        let code = run_with(
            &StatusArgs {
                decisions: 5,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            false,
        )
        .expect("runs");
        assert_eq!(code, 0, "a corrupt line must not fail the command");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("sess-good"), "got {text}");
    }

    /// Issue #225: `group_tree_lines_brief` answers the same "was delegating
    /// cheaper than doing it here" question `group_tree_lines` does, but with
    /// one line per group instead of one per delegation -- each child's own
    /// session id must NOT appear, only the group's own id and its combined
    /// totals, plus a final grand total across every group shown.
    #[test]
    fn the_brief_group_tree_collapses_each_group_to_one_line_with_a_grand_total() {
        let groups = vec![
            sample_group("wg-1", "phase 5 implementation"),
            sample_group("wg-2", "phase 6 review"),
        ];
        let delegations = vec![
            delegation_row("wg-1", "sess-a", "codex", 1_000, 91_000, 500),
            delegation_row("wg-1", "sess-b", "claude", 2_000, 40_000, 900),
            delegation_row("wg-2", "sess-c", "codex", 500, 1_000, 100),
        ];
        let lines = group_tree_lines_brief(&groups, &delegations, &[], false);
        let text = lines.join("\n");

        assert!(text.contains("wg-1"), "got {text}");
        assert!(text.contains("wg-2"), "got {text}");
        assert!(
            !text.contains("sess-a") && !text.contains("sess-b") && !text.contains("sess-c"),
            "brief must not show a per-delegation session id: {text}"
        );
        assert!(
            text.contains("grand total"),
            "must sum across every group shown: {text}"
        );
        // 1000 + 2000 + 500 input tokens across all three delegations.
        assert!(
            text.contains("input 3500"),
            "the grand total must sum every group's totals: {text}"
        );
        assert!(
            text.contains("3 deleg."),
            "the grand total must count every delegation: {text}"
        );
    }

    /// Same "nothing to show is nothing shown" rule the full tree follows.
    #[test]
    fn the_brief_group_tree_renders_nothing_for_no_delegations() {
        assert!(group_tree_lines_brief(&[], &[], &[], false).is_empty());
    }

    /// End-to-end: `--brief` keeps every section present but collapses the
    /// unbounded ones, so a fixture with several delegations across multiple
    /// groups and several live sessions renders strictly fewer bytes than
    /// the default view -- while both views still answer the same questions
    /// (agents, mail, work groups, sessions, handoff, decisions).
    #[test]
    fn brief_status_is_smaller_than_full_status_for_the_same_fixture() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        // Two groups, five delegations total.
        group::create(&state, &sample_group("wg-1", "phase 5 implementation"))
            .expect("create wg-1");
        group::create(&state, &sample_group("wg-2", "phase 6 review")).expect("create wg-2");
        for row in [
            delegation_row("wg-1", "sess-a", "codex", 1_000, 91_000, 500),
            delegation_row("wg-1", "sess-b", "claude", 2_000, 40_000, 900),
            delegation_row("wg-1", "sess-c", "codex", 300, 2_000, 100),
            delegation_row("wg-2", "sess-d", "codex", 500, 1_000, 100),
            delegation_row("wg-2", "sess-e", "claude", 700, 3_000, 200),
        ] {
            log::append_delegation(
                &state,
                &log::Delegation {
                    ts: row.ts,
                    session: &row.session,
                    parent_session: &row.parent_session,
                    work_group_id: row.work_group_id.as_deref(),
                    agent: &row.agent,
                    model: row.model.as_deref(),
                    input_tokens: row.input_tokens,
                    cache_creation_input_tokens: row.cache_creation_input_tokens,
                    cache_read_input_tokens: row.cache_read_input_tokens,
                    output_tokens: row.output_tokens,
                    wall_ms: row.wall_ms,
                    exit_code: row.exit_code,
                    outcome: &row.outcome,
                    mode: row.mode,
                    task_class: row.task_class,
                    principal: &row.principal,
                    envelope_sha256: row.envelope_sha256.as_deref(),
                },
            )
            .expect("append");
        }

        // Three live sessions, one of which is "this" invocation.
        let guard_a = crate::commands::ctx::sessions::SessionGuard::register(
            &state,
            crate::commands::ctx::sessions::Record::new(
                "aaaa1111-2222-4333-8444-555555555555",
                "claude",
                &repo,
                crate::commands::ctx::sessions::Verb::Wrap,
            ),
        );
        let guard_b = crate::commands::ctx::sessions::SessionGuard::register(
            &state,
            crate::commands::ctx::sessions::Record::new(
                "bbbb2222-2222-4333-8444-555555555555",
                "codex",
                &repo,
                crate::commands::ctx::sessions::Verb::Exec,
            ),
        );
        let guard_c = crate::commands::ctx::sessions::SessionGuard::register(
            &state,
            crate::commands::ctx::sessions::Record::new(
                "cccc3333-2222-4333-8444-555555555555",
                "claude",
                &repo,
                crate::commands::ctx::sessions::Verb::Chat,
            ),
        );

        let mut env_map = env_for(state.root());
        env_map.insert(
            crate::commands::ctx::adapters::SESSION_ENV.to_string(),
            "aaaa1111-2222-4333-8444-555555555555".to_string(),
        );
        let env = move |k: &str| env_map.get(k).cloned();

        let mut full_out = Vec::new();
        run_with(
            &StatusArgs {
                decisions: 5,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut full_out,
            &repo,
            &env,
            false,
        )
        .expect("full runs");
        let full_text = String::from_utf8(full_out).expect("utf8");

        let mut brief_out = Vec::new();
        run_with(
            &StatusArgs {
                decisions: 5,
                brief: true,
                diff: false,
                breakdown: None,
            },
            &mut brief_out,
            &repo,
            &env,
            false,
        )
        .expect("brief runs");
        let brief_text = String::from_utf8(brief_out).expect("utf8");

        assert!(
            brief_text.len() < full_text.len(),
            "brief ({} bytes) must be smaller than full ({} bytes)",
            brief_text.len(),
            full_text.len()
        );
        // Every section is still present in brief, just collapsed.
        for section in [
            "agents:",
            "mail:",
            "heavy operations:",
            "work groups:",
            "sessions:",
            "memory:",
            "handoff:",
            "decisions:",
        ] {
            assert!(
                brief_text.contains(section),
                "brief must keep the '{section}' section: {brief_text}"
            );
        }
        // Collapsed content: no per-delegation session id, and the
        // "this session" clause names the live session whose env we set.
        assert!(
            !brief_text.contains("sess-a"),
            "brief must not list a per-delegation session id: {brief_text}"
        );
        assert!(
            brief_text.contains("run without --brief for the last 5"),
            "decisions must point back at the full view: {brief_text}"
        );
        assert!(
            brief_text.contains("this session"),
            "the live invoking session must be named: {brief_text}"
        );
        assert!(
            brief_text.contains("3 live"),
            "all three registered sessions must be counted: {brief_text}"
        );

        drop(guard_a);
        drop(guard_b);
        drop(guard_c);
    }

    fn env_for_session(
        state: &std::path::Path,
        session: &str,
    ) -> std::collections::HashMap<String, String> {
        let mut env = env_for(state);
        env.insert(
            crate::commands::ctx::adapters::SESSION_ENV.to_string(),
            session.to_string(),
        );
        env
    }

    #[test]
    fn split_sections_groups_continuations_handles_no_colon_and_dedups_keys() {
        let text = "state dir: /tmp/x\n\
                     \n\
                     agents:\n\
                     \x20\x20claude enabled (default)\n\
                     \x20\x20codex  disabled (repo)\n\
                     \n\
                     no colon here\n\
                     \n\
                     agents:\n\
                     \x20\x20another\n";
        let sections = split_sections(text);
        assert_eq!(
            sections,
            vec![
                ("state dir".to_string(), "state dir: /tmp/x".to_string()),
                (
                    "agents".to_string(),
                    "agents:\n  claude enabled (default)\n  codex  disabled (repo)".to_string()
                ),
                ("no colon here".to_string(), "no colon here".to_string()),
                ("agents #2".to_string(), "agents:\n  another".to_string()),
            ]
        );
    }

    #[test]
    fn diff_sections_reports_changed_new_and_removed_in_cur_order() {
        let prev = vec![
            ("a".to_string(), "a: 1".to_string()),
            ("b".to_string(), "b: 1".to_string()),
            ("c".to_string(), "c: 1".to_string()),
        ];
        let cur = vec![
            ("b".to_string(), "b: 1".to_string()),
            ("a".to_string(), "a: 2".to_string()),
            ("d".to_string(), "d: 1".to_string()),
        ];

        let SectionDiff { changed, removed } = diff_sections(&prev, &cur);

        assert_eq!(
            changed,
            vec![
                ("a".to_string(), "a: 2".to_string()),
                ("d".to_string(), "d: 1".to_string()),
            ]
        );
        assert_eq!(removed, vec!["c".to_string()]);
    }

    #[test]
    fn diff_first_call_prints_full_report_and_writes_snapshot_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state_root = tmp.path().join("state");
        let env = env_for_session(&state_root, "sess-diff-first");

        let mut out = Vec::new();
        let code = run_with(
            &StatusArgs {
                decisions: 10,
                brief: false,
                diff: true,
                breakdown: None,
            },
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            false,
        )
        .expect("runs");
        assert_eq!(code, 0);

        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.starts_with(
                "status --diff: no snapshot for this session yet; full report follows\n"
            ),
            "got {text}"
        );
        assert!(text.contains("no supervised sessions"), "got {text}");

        let state = StateDir::from_root(state_root);
        let files: Vec<_> = std::fs::read_dir(state.status_snapshots())
            .expect("read snapshot dir")
            .collect();
        assert_eq!(files.len(), 1, "exactly one snapshot file written");
    }

    #[test]
    fn diff_second_call_with_no_change_prints_exactly_one_line() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state_root = tmp.path().join("state");
        let env = env_for_session(&state_root, "sess-diff-nochange");
        let args = StatusArgs {
            decisions: 10,
            brief: false,
            diff: true,
            breakdown: None,
        };

        let mut first = Vec::new();
        run_with(
            &args,
            &mut first,
            tmp.path(),
            &|k| env.get(k).cloned(),
            false,
        )
        .expect("first runs");

        let mut second = Vec::new();
        run_with(
            &args,
            &mut second,
            tmp.path(),
            &|k| env.get(k).cloned(),
            false,
        )
        .expect("second runs");
        let text = String::from_utf8(second).expect("utf8");

        assert_eq!(
            text.lines().count(),
            1,
            "exactly one line for no change: {text}"
        );
        assert!(
            text.starts_with("status --diff: no change since "),
            "got {text}"
        );
    }

    #[test]
    fn diff_reports_only_the_section_that_changed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let env = env_for_session(state.root(), "sess-diff-changed");
        let args = StatusArgs {
            decisions: 10,
            brief: false,
            diff: true,
            breakdown: None,
        };

        let mut first = Vec::new();
        run_with(
            &args,
            &mut first,
            tmp.path(),
            &|k| env.get(k).cloned(),
            false,
        )
        .expect("first runs");

        crate::commands::ctx::handoff::store(
            &state,
            tmp.path(),
            "sess-diff-changed",
            &Handoff {
                task: "Wire the webhook".to_string(),
                next_step: "Write the test".to_string(),
                ..Handoff::default()
            },
        )
        .expect("store handoff");

        let mut second = Vec::new();
        run_with(
            &args,
            &mut second,
            tmp.path(),
            &|k| env.get(k).cloned(),
            false,
        )
        .expect("second runs");
        let text = String::from_utf8(second).expect("utf8");

        assert!(
            text.starts_with("status --diff: 1 of "),
            "exactly one section changed: {text}"
        );
        assert!(text.contains("Wire the webhook"), "changed body: {text}");
        assert!(
            !text.contains("no supervised sessions"),
            "unrelated sections must not print: {text}"
        );
    }

    #[test]
    fn status_and_diff_report_model_identity_drift_with_turn_distance() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::create_dir_all(&repo).expect("repo");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);

        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("state");
        let session = "abcdef12-3456-4789-8abc-def012345678";
        let record = crate::commands::ctx::sessions::Record::new(
            session,
            "claude",
            &repo,
            crate::commands::ctx::sessions::Verb::Exec,
        );
        let _guard = crate::commands::ctx::sessions::SessionGuard::register(&state, record);

        let transcript_dir = home
            .join(".claude/projects")
            .join(crate::commands::ctx::adapters::claude::project_slug(&repo));
        std::fs::create_dir_all(&transcript_dir).expect("transcript dir");
        let transcript = transcript_dir.join(format!("{session}.jsonl"));
        let first = concat!(
            r#"{"type":"user","message":{"content":"one"}}"#,
            "\n",
            r#"{"type":"assistant","message":{"model":"claude-opus-5","content":[{"type":"text","text":"[zirv] one"}],"usage":{"input_tokens":1}}}"#,
            "\n",
        );
        std::fs::write(&transcript, first).expect("first turn");

        let mut env = env_for_session(state.root(), session);
        env.insert("ZIRV_CTX_AGENT".to_string(), "claude".to_string());
        let args = StatusArgs {
            decisions: 10,
            brief: false,
            diff: true,
            breakdown: None,
        };
        let mut first_out = Vec::new();
        run_with(
            &args,
            &mut first_out,
            &repo,
            &|key| env.get(key).cloned(),
            false,
        )
        .expect("initial snapshot");

        let second = concat!(
            r#"{"type":"user","message":{"content":"two"}}"#,
            "\n",
            r#"{"type":"assistant","message":{"model":"claude-sonnet-5","content":[{"type":"text","text":"[zirv] two"}],"usage":{"input_tokens":2}}}"#,
            "\n",
        );
        std::fs::write(&transcript, format!("{first}{second}")).expect("second turn");

        let mut status_out = Vec::new();
        run_with(
            &StatusArgs {
                decisions: 10,
                brief: false,
                diff: false,
                breakdown: None,
            },
            &mut status_out,
            &repo,
            &|key| env.get(key).cloned(),
            false,
        )
        .expect("status");
        let status_text = String::from_utf8(status_out).expect("utf8");
        assert!(
            status_text.contains(
                "model changed mid-session 0 turns ago: `claude-opus-5` -> `claude-sonnet-5`"
            ),
            "got {status_text}"
        );

        let mut diff_out = Vec::new();
        run_with(
            &args,
            &mut diff_out,
            &repo,
            &|key| env.get(key).cloned(),
            false,
        )
        .expect("diff");
        let text = String::from_utf8(diff_out).expect("utf8");
        assert!(
            text.contains(
                "model changed mid-session 0 turns ago: `claude-opus-5` -> `claude-sonnet-5`"
            ),
            "got {text}"
        );
        assert!(
            text.contains("sessions:"),
            "the sessions section changed: {text}"
        );
    }

    #[test]
    fn model_identity_drift_text_cannot_forge_terminal_rows() {
        let text = model_change_status_text(&crate::commands::ctx::event::ModelChange {
            from: "claude-opus-5\nforged\u{1b}[31m".to_string(),
            to: "claude-sonnet-5\nforged\u{1b}[2J".to_string(),
            turns_ago: 2,
            limit_pressure: true,
        });

        assert_eq!(
            text,
            "model changed mid-session 2 turns ago: `claude-opus-5 forged [31m` -> \
             `claude-sonnet-5 forged [2J`"
        );
        assert!(
            !text.chars().any(char::is_control),
            "no transcript-derived control character may reach status output: {text:?}"
        );
    }

    #[test]
    fn diff_snapshots_are_isolated_per_session() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        // `sessions::short_id` keeps only the first 8 alphanumeric
        // characters, so these must differ within that window -- unlike
        // most other fixtures in this file, a shared "sess-diff-" prefix
        // would collide here.
        let env_a = env_for_session(state.root(), "alpha-diff-session");
        let env_b = env_for_session(state.root(), "beta-diff-session");
        let args = StatusArgs {
            decisions: 10,
            brief: false,
            diff: true,
            breakdown: None,
        };

        let mut out_a = Vec::new();
        run_with(
            &args,
            &mut out_a,
            tmp.path(),
            &|k| env_a.get(k).cloned(),
            false,
        )
        .expect("a runs");

        let mut out_b = Vec::new();
        run_with(
            &args,
            &mut out_b,
            tmp.path(),
            &|k| env_b.get(k).cloned(),
            false,
        )
        .expect("b runs");
        let text_b = String::from_utf8(out_b).expect("utf8");

        assert!(
            text_b.starts_with(
                "status --diff: no snapshot for this session yet; full report follows\n"
            ),
            "session b must not see session a's snapshot: {text_b}"
        );
    }

    #[test]
    fn diff_brief_and_full_snapshots_do_not_cross_match() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let env = env_for_session(state.root(), "sess-diff-brief-full");

        let mut full_out = Vec::new();
        run_with(
            &StatusArgs {
                decisions: 10,
                brief: false,
                diff: true,
                breakdown: None,
            },
            &mut full_out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            false,
        )
        .expect("full runs");

        let mut brief_out = Vec::new();
        run_with(
            &StatusArgs {
                decisions: 10,
                brief: true,
                diff: true,
                breakdown: None,
            },
            &mut brief_out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            false,
        )
        .expect("brief runs");
        let brief_text = String::from_utf8(brief_out).expect("utf8");

        assert!(
            brief_text.starts_with(
                "status --diff: no snapshot for this session yet; full report follows\n"
            ),
            "a --brief --diff snapshot must not match an earlier full --diff snapshot: {brief_text}"
        );
    }

    #[test]
    fn diff_with_no_session_identity_shows_full_report_and_writes_no_snapshot() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state_root = tmp.path().join("state");
        let env = env_for(&state_root);

        let mut out = Vec::new();
        let code = run_with(
            &StatusArgs {
                decisions: 10,
                brief: false,
                diff: true,
                breakdown: None,
            },
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            false,
        )
        .expect("runs");
        assert_eq!(code, 0);

        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.starts_with(
                "status --diff: no session identity (ZIRV_CTX_SESSION unset); showing the \
                 full report\n"
            ),
            "got {text}"
        );
        assert!(text.contains("no supervised sessions"), "got {text}");

        let state = StateDir::from_root(state_root);
        assert!(
            !state.status_snapshots().exists(),
            "no snapshot directory must be created when there is no session identity"
        );
    }
}
