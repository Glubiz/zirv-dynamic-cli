//! The `zirv ▸` announcement channel: short, timestamped lines on stderr that
//! narrate what a supervised session is doing -- prompt composition, a rot
//! verdict crossing a band, a compaction or restart, a pacing wait, mail
//! delivered or waiting, supervision degrading, a delegated run starting or
//! finishing. It replaces the ad-hoc `writeln!(stderr, "\r\n{}\r", ...)`
//! lines `wrap.rs`, `pace.rs` and friends used to build by hand (the rot
//! advisory, the T8 mail advisory) with one shared format and one shared
//! opt-out.
//!
//! `Event::line` is pure: it never touches a clock injected from outside
//! (there is nowhere in any of these call sites to thread one through) but
//! it also never reads any terminal state, never applies colour, and never
//! emits a cursor-addressing escape sequence -- the reserved bottom row
//! (T12b) is never at risk from anything printed here. Colour, when it
//! applies at all, is added by `Announcer::render`, kept separate so the
//! event's own text can be asserted on exactly regardless of it.
//!
//! Opt-outs -- `--quiet` on `chat`/`agent` (folded into `ZIRV_CTX_QUIET` by
//! those verbs), `ZIRV_CTX_QUIET` itself, and `[chrome] events = false` --
//! all collapse to one boolean, `cfg.chrome.events`, which is what
//! `Announcer::enabled` is built from. `output::error` and `output::warn`
//! are a different channel entirely: neither takes an `Announcer` nor
//! consults `cfg.chrome`, so nothing here can suppress them.

use std::io::Write;

use super::rot::Verdict;

/// One thing worth narrating to the operator. Each variant carries exactly
/// the data its `text()` needs; nothing here is optional-and-usually-None.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A system prompt was composed at session start, naming the layers that
    /// went into it (`ComposedPrompt::describe`'s own text).
    InjectionComposed { layers: String },
    /// Composition was skipped, naming why (`--simple`, disabled, or the
    /// agent has no verified mechanism).
    InjectionSkipped { reason: String },
    /// The rot verdict crossed into a new band. Callers are responsible for
    /// only constructing this on an actual change; see `verdict_change`.
    VerdictChanged {
        from: Verdict,
        to: Verdict,
        score: u32,
    },
    /// A compaction was injected into the wrapped agent, and whether the
    /// transcript confirmed it actually happened.
    Compact { verified: bool },
    /// A rotted session was restarted, naming the handoff style
    /// (`"distilled"`, `"structural"`, or `"no data"` for an adapter with no
    /// verified event parsing at all, `handoff::distill_or_structural`'s own
    /// vocabulary) and where the handoff was stored.
    Restart { style: String, stored: String },
    /// Unread mail is waiting in the mailbox (the T8 advisory `wrap`'s pump
    /// used to build by hand).
    MailWaiting { count: usize },
    /// Mail was folded into a composed prompt at session start.
    MailDelivered { count: usize },
    /// Low 8: mail was pending but this launch has no channel to carry it
    /// at all -- `exec.rs`'s own `mail_deliverable == false` case (an
    /// explicit `--` command for an adapter with no system-prompt
    /// mechanism, which has neither `composed` nor task-prompt text to
    /// append to), the headless-side counterpart of the narration
    /// `dash/mod.rs`'s worker-pane spawn already gives (`push_error`) when
    /// its own, narrower "unsafe on this launch" case holds mail back.
    /// Left unread, same as there -- still visible to `zirv ctx inbox` and
    /// to any other session that can actually deliver it.
    MailWithheld { count: usize },
    /// The pacing gate is waiting on a usage window, naming which one and
    /// when it is expected to reset.
    PacingWait {
        window: String,
        reset_at: Option<u64>,
    },
    /// T8 (fail-SAFE, not open): the pacing gate has no usable data at all
    /// for this provider -- no binding collector reading (ever recorded, or
    /// gone stale below the ceiling with nothing fresher), and no configured
    /// estimator to fall back on -- so instead of the old behavior (skip the
    /// gate outright, proceed at full speed forever) it applies a bounded
    /// `delay_secs` safety delay every cycle until real data shows up --
    /// pacing is now degraded, not off, and this event says so.
    /// See `zirv ctx status`'s `poll::usage_source_hint` for the reason and
    /// remedy (file absent, macOS Keychain access needed, or the statusline
    /// tee never wired).
    PacingBlind { provider: String, delay_secs: u64 },
    /// Item 4: the pacing gate is inside the soft-throttle band, delaying
    /// cycles rather than hard-pausing (`PaceDecision::Slow`). Unlike
    /// `PacingWait`, this is a recurring per-cycle delay rather than a wait
    /// to an absolute deadline, so `delay_secs` is a snapshot of the delay
    /// at the moment this throttle episode latched, not a live countdown.
    /// Emitted once per latched episode, the same discipline `PacingWait`
    /// already follows.
    PacingThrottled {
        window: String,
        delay_secs: u64,
        percent: f64,
    },
    /// Supervision degraded to permanent passthrough, naming what caused it.
    Degraded { cause: String },
    /// The ctx configuration could not be parsed, so the workflow subsystem's
    /// two gates over repository-provided input (`workflow.repo_checks_enabled`
    /// and `workflow.repo_skills_enabled`) both closed. A repository checkout
    /// controls a file in that layered config, so an unreadable config must
    /// never be a way to *widen* what the checkout may contribute -- and the
    /// operator has to be told, because zirv is now running with less of the
    /// repository's own input than the repository asked for.
    WorkflowGatesClosed { reason: String },
    /// The active workflow step's skill context could not be rendered, so the
    /// composed prompt is missing its workflow layer. A repository skill
    /// manifest that will not load is the usual cause, and the whole layer
    /// used to disappear in silence (`.ok().flatten()`) -- leaving a session
    /// running with no methodology and no way to notice.
    WorkflowLayerSkipped { reason: String },
    /// Context health is slipping (the rot advisory `wrap`'s pump used to
    /// build by hand as `advisory_line`).
    RotAdvisory { score: u32, tokens: u64 },
    /// A delegated headless run (`zirv ctx agent`, or an `agent:` script
    /// step) started on another harness.
    DelegatedStart { agent: String },
    /// A delegated run finished, naming the human-readable meaning of its
    /// exit (`exec::describe_exit`'s own text for the two supervisor exit
    /// codes, or a plain "exited with code N" otherwise).
    DelegatedFinish { agent: String, meaning: String },
    /// Item 6 audit: `wrap`'s pump loop ends the wrapped session, propagating
    /// the wrapped agent's own exit code, the moment the child exits --
    /// whether that is the agent quitting cleanly or crashing. Previously
    /// silent: nothing printed when this happened, so a maintainer watching
    /// the session saw it end with no explanation at all, indistinguishable
    /// from a bug in `wrap` itself.
    SessionEnded { agent: String, code: i32 },
    /// An interactive session is launching with a model chosen by
    /// configuration rather than by the operator's own command line
    /// (`chat.model`/`ZIRV_CTX_CHAT_MODEL`).
    ///
    /// `chat.model` is one of the few keys a **repo** `ctx.toml` may set, and
    /// the exemption was granted on the strength of the choice being visible
    /// on screen. It was not: the only disclosure was `chrome::banner`, and
    /// `chrome.banner` is *not* `REPO_FORBIDDEN`, so a checked-out repo could
    /// set `[chrome] banner = false` alongside `[chat] model = ...` and pick
    /// the model with nothing shown anywhere (the `wrap` fallback shows it
    /// nowhere else at all). `chrome.events` **is** `REPO_FORBIDDEN`, so
    /// announcing it here is a disclosure a repo cannot silence -- only the
    /// operator can, with `--quiet`/`ZIRV_CTX_QUIET`, which is exactly the
    /// trust asymmetry the rest of this codebase already holds.
    ChatModel { model: String },
    /// A `zirv ctx nudge` wake-up marker was claimed. `from` names the
    /// *sending* session's short id, read out of the marker file itself
    /// (C4): every emitter used to pass its own short id here, so the line
    /// always read "nudged by <myself>".
    Nudge {
        from: String,
        disposition: NudgeDisposition,
    },
    /// macOS only: about to shell out to `security find-generic-password` for
    /// the claude OAuth token, which -- unlike a plain file read -- can pop a
    /// GUI "zirv wants to access key 'Claude Code-credentials'" dialog, since
    /// zirv is not in that keychain item's ACL. Emitted once per process
    /// (`poll`'s own one-time latch, the same discipline `pace_no_source_
    /// announced` already follows for its own once-per-run line) right before
    /// the first such attempt, so an operator on a headless/SSH session -- who
    /// cannot click the dialog -- learns *why* usage data is stalling instead
    /// of just seeing a bounded timeout expire in silence, and one running a
    /// real terminal learns that "Always Allow" makes the prompt a one-time
    /// cost rather than a recurring interruption.
    ///
    /// `poll::anthropic_token_from_keychain` (the only production
    /// constructor) is itself `#[cfg(target_os = "macos")]`, so on every
    /// other target this variant is genuinely unreachable outside its own
    /// test below -- `#[allow(dead_code)]` documents that as deliberate
    /// rather than an oversight, the same way `poll.rs`'s own macOS-only
    /// items are marked.
    #[allow(dead_code)]
    MacosKeychainPromptExpected,
    /// The shipped-default "sandboxed, no prompts" launch posture
    /// (2026-08-22, `adapters::policy_launch_args`/`AgentAdapter::default_
    /// sandbox_args`) applied -- or explicitly not applied -- to this
    /// launch, so the behaviour change is visible on the one channel every
    /// other launch-time decision already narrates through, not silent.
    /// `detail` is pre-rendered by the caller: the joined argv when the
    /// posture (or an explicit `[policy]` restriction) applied, or a short
    /// reason when it did not (`--sandbox.enabled = false`, or the
    /// operator's own flags already pinned the same concern).
    SandboxPosture { detail: String },
    /// A ctx config layer (`~/.zirv/ctx.toml` or `<repo>/.zirv/ctx.toml`)
    /// failed to *parse* as TOML and was skipped rather than aborting the
    /// whole load (`config::CtxConfig::load`, `config::UnparsableLayer`) --
    /// distinct from a `REPO_FORBIDDEN` rejection, which still fails the load
    /// outright. Never silent: a stray keystroke in an untrusted repo file
    /// must not become a silently-skipped layer with no operator-visible
    /// sign of it. `detail` is pre-rendered by the caller (`config::
    /// announce_unparsable_layers_once`), naming every unparsable layer's
    /// path, line:col and parse message -- more than one layer can fail at
    /// once (home and repo both malformed), but this announces once per
    /// process regardless, the same one-time-latch discipline `poll.rs`'s
    /// `announce_keychain_prompt_once` uses.
    ConfigUnparsable { detail: String },
    /// Issue #89: this session's resolved distiller or workflow reviewer is
    /// an adapter whose own report-only sandbox pin
    /// (`AgentAdapter::read_only_args`) has a known, recorded gap on the
    /// operator's currently-installed binary
    /// (`AgentAdapter::sandbox_residual_note`) -- e.g. codex's `--sandbox
    /// read-only` not being paired with `--ignore-rules
    /// --ignore-user-config` on an older codex-cli, so the child still
    /// reads the repo's `.rules` execpolicy files and the operator's own
    /// `~/.codex/config.toml`. Fired at most once per process
    /// (`adapters::announce_sandbox_residual_once`); `note` is the
    /// adapter's own one-line explanation, pre-rendered by the caller.
    SandboxResidual { note: String },
    /// Issue #87: a durable memory harvest ran at a session boundary
    /// (restart or clean exit, `memory::harvest_durable` -- the single
    /// choke point every one of the four call sites in `exec.rs`/`wrap.rs`
    /// funnels through) and finished. `count` is the number of entries
    /// actually accepted and written into the shared scope
    /// (`write_durable`'s own return), which can legitimately be zero when
    /// the model proposed nothing durable or every candidate was filtered
    /// out -- still worth a one-line signal, so an operator watching the
    /// `zirv ▸` channel can see that memory harvesting ran at all, not just
    /// silently infer it from a diff in `.zirv/memory/` days later.
    MemoryHarvested { count: usize },
    /// Issue #84: the orchestrator seat's model or harness was swapped in
    /// place via `zirv ctx handover`, carrying a handoff packet across the
    /// swap while the session kept its registry short id. Both models are
    /// named, matching the decision-log entry's own contract.
    Handover {
        from_agent: String,
        from_model: String,
        to_agent: String,
        to_model: String,
        stored: String,
    },
    /// A `zirv ctx handover` request was refused rather than acted on --
    /// most commonly "mid-turn, and no `--force` was given" (see
    /// `wrap::may_inject`, the same quiesce check every other injection
    /// already gates on).
    HandoverRefused { reason: String },
}

/// What the nudged session is actually going to do about it -- the three
/// dispositions are genuinely different promises to the operator, and
/// collapsing them into one boolean (C4) told interactive sessions their
/// nudge "will be picked up as mail", which is exactly what never happens
/// there: `wrap`/`chat` are advised only and never receive message bodies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NudgeDisposition {
    /// `exec`: stopping the child and relaunching with the guidance folded
    /// into the fresh prompt.
    Relaunching,
    /// `loop`: the payload is ordinary mail and this cycle is already
    /// running, so the next cycle's own listing picks it up.
    NextCycle,
    /// `wrap`/`chat`: an interactive session is never restarted and never
    /// receives bodies -- at most a one-line advisory is typed in at a
    /// verified-idle boundary; the body waits in the inbox.
    Advisory,
}

impl Event {
    /// `[HH:MM:SS] zirv ▸ <text>`, in UTC: every other clock read in this
    /// module (`state::now_secs`) is already a bare unix timestamp with no
    /// timezone handling, and adding a timezone dependency just to print a
    /// clock face on an advisory line is not worth it.
    pub fn line(&self) -> String {
        format!("[{}] zirv \u{25b8} {}", Self::clock(), self.text())
    }

    fn clock() -> String {
        let secs_of_day = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
            % 86_400;
        format!(
            "{:02}:{:02}:{:02}",
            secs_of_day / 3600,
            (secs_of_day / 60) % 60,
            secs_of_day % 60
        )
    }

    fn text(&self) -> String {
        match self {
            Event::InjectionComposed { layers } => format!("system prompt composed ({layers})"),
            Event::InjectionSkipped { reason } => format!("system prompt skipped: {reason}"),
            Event::SandboxPosture { detail } => format!("sandbox posture: {detail}"),
            Event::ConfigUnparsable { detail } => format!(
                "config layer(s) could not be parsed and were skipped, defaults used instead: \
                 {detail}"
            ),
            Event::SandboxResidual { note } => format!("sandbox residual: {note}"),
            Event::VerdictChanged { from, to, score } => {
                format!(
                    "context health {} -> {} (score {score})",
                    from.as_str(),
                    to.as_str()
                )
            }
            Event::Compact { verified } => format!(
                "compaction injected, {}",
                if *verified {
                    "verified"
                } else {
                    "not verified"
                }
            ),
            Event::Restart { style, stored } => {
                format!("session restarted with a {style} handoff, stored at {stored}")
            }
            Event::MailWaiting { count } => {
                let plural = if *count == 1 { "" } else { "s" };
                format!(
                    "{count} new mail message{plural} waiting; run `zirv ctx inbox` to read them"
                )
            }
            Event::MailDelivered { count } => {
                let plural = if *count == 1 { "" } else { "s" };
                format!("delivered {count} mail message{plural} into the session prompt")
            }
            Event::MailWithheld { count } => {
                let plural = if *count == 1 { "" } else { "s" };
                format!(
                    "{count} mail message{plural} cannot reach this launch (no delivery channel \
                     for this adapter/command shape); left unread"
                )
            }
            Event::PacingWait { window, reset_at } => match reset_at {
                Some(at) => format!("pacing: waiting on the {window} window, resets at unix {at}"),
                None => format!("pacing: waiting on the {window} window, reset time unknown"),
            },
            Event::PacingBlind {
                provider,
                delay_secs,
            } => format!(
                "pacing degraded: {provider} has no usage source; applying a {delay_secs}s \
                 safety delay per cycle -- run `zirv ctx status` for the reason and remedy"
            ),
            Event::PacingThrottled {
                window,
                delay_secs,
                percent,
            } => format!(
                "pacing: throttling the {window} window at {percent:.1}%, ~{delay_secs}s before the next run"
            ),
            Event::Degraded { cause } => format!("supervision degraded: {cause}"),
            Event::WorkflowGatesClosed { reason } => format!(
                "ctx config unreadable, so repo-provided workflow checks and skills are disabled: \
                 {reason}"
            ),
            Event::WorkflowLayerSkipped { reason } => {
                format!("workflow step context skipped: {reason}")
            }
            Event::RotAdvisory { score, tokens } => format!(
                "context health is slipping (score {score}, {tokens} tokens in context); a \
                 /compact soon will keep instruction-following sharp"
            ),
            Event::DelegatedStart { agent } => format!("delegating to {agent}"),
            Event::DelegatedFinish { agent, meaning } => format!("{agent} finished: {meaning}"),
            Event::SessionEnded { agent, code } => {
                format!("{agent} session ended (exit code {code})")
            }
            Event::ChatModel { model } => format!("chat model '{model}' (from config)"),
            Event::Nudge { from, disposition } => match disposition {
                NudgeDisposition::Relaunching => {
                    format!("nudged by {from}; relaunching with the guidance")
                }
                NudgeDisposition::NextCycle => {
                    format!("nudged by {from}; will be picked up as mail on the next cycle")
                }
                NudgeDisposition::Advisory => {
                    format!("nudged by {from}; run `zirv ctx inbox` to read it")
                }
            },
            Event::Handover {
                from_agent,
                from_model,
                to_agent,
                to_model,
                stored,
            } => format!(
                "handover: {from_agent} ({from_model}) -> {to_agent} ({to_model}), same session \
                 id; handoff stored at {stored}"
            ),
            Event::HandoverRefused { reason } => format!("handover refused: {reason}"),
            Event::MacosKeychainPromptExpected => {
                "macOS may prompt for Keychain access to read Claude Code's usage token \
                 ('Claude Code-credentials'); choose 'Always Allow' to make that a one-time cost \
                 -- on a headless/SSH session with nobody to answer it, this attempt will time \
                 out and usage stays unknown rather than hang"
                    .to_string()
            }
            Event::MemoryHarvested { count } => {
                let noun = if *count == 1 { "entry" } else { "entries" };
                format!("memory harvest wrote {count} durable {noun}")
            }
        }
    }
}

/// `Some` only when `next` is a different band than `prev`: the one place
/// that decides "only on change" for a verdict announcement, kept pure and
/// separate from `wrap`'s pump so it is testable without a live session.
pub fn verdict_change(prev: Verdict, next: Verdict, score: u32) -> Option<Event> {
    (prev != next).then_some(Event::VerdictChanged {
        from: prev,
        to: next,
        score,
    })
}

/// Whether the channel is on, and whether it may colour the `zirv ▸` marker.
/// Built once per session from `cfg.chrome.events` (already folding in
/// `--quiet`, `ZIRV_CTX_QUIET` and `[chrome] events`) and
/// `console::colors_enabled_stderr()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Announcer {
    pub enabled: bool,
    pub colour: bool,
}

impl Announcer {
    pub fn new(enabled: bool, colour: bool) -> Self {
        Self { enabled, colour }
    }

    /// A permanently silent announcer, for a caller that has no config to
    /// hand (e.g. a `--no-supervise` run) but still needs something to call
    /// `.emit` on unconditionally rather than threading an `Option` through.
    pub fn silent() -> Self {
        Self {
            enabled: false,
            colour: false,
        }
    }

    /// The line as it would actually be printed: `event.line()` unchanged
    /// when `colour` is off, or with the `zirv ▸` marker coloured when it is
    /// on. Only the marker is styled -- the event's own text is never
    /// touched -- so `console::strip_ansi_codes` on the result always
    /// recovers `event.line()` exactly.
    pub fn render(&self, event: &Event) -> String {
        let line = event.line();
        if !self.colour {
            return line;
        }
        let marker = "zirv \u{25b8}";
        let styled = console::style(marker).cyan().dim().to_string();
        line.replacen(marker, &styled, 1)
    }

    /// `emit_to`, but reporting whether the line actually reached `w`:
    /// `false` when the channel is disabled (`--quiet`, `ZIRV_CTX_QUIET`,
    /// `[chrome] events = false`) or when the write itself failed.
    ///
    /// Almost every caller wants `emit`/`emit_to`: a narration line that does
    /// not land is not worth a single branch at the call site, and swallowing
    /// it is the whole point of the shared opt-out. The exception is a caller
    /// that *records having said something* -- `wrap`'s mail watch marks a
    /// message as announced so it does not re-announce on every poll. For an
    /// adapter with no turn-signal mechanism `may_inject` never becomes true,
    /// so this channel is the only surface that advisory has: a swallowed
    /// emit recorded as an announcement means the operator is never told at
    /// all, by either route. Such a caller has to know, so it can leave the
    /// message unannounced and retry at the next poll.
    pub fn try_emit_to<W: Write>(&self, w: &mut W, event: &Event) -> bool {
        if !self.enabled {
            return false;
        }
        writeln!(w, "\r\n{}\r", self.render(event)).is_ok()
    }

    pub fn try_emit(&self, event: &Event) -> bool {
        self.try_emit_to(&mut std::io::stderr(), event)
    }

    /// Framed exactly like the advisories this channel replaces (`\r\n{line}\r`,
    /// via `writeln!`'s own trailing newline), so it interleaves safely with a
    /// raw-mode child's own cursor movement. A no-op when disabled: callers
    /// call this unconditionally on every event, and quiet is enforced here
    /// once rather than at every call site.
    pub fn emit_to<W: Write>(&self, w: &mut W, event: &Event) {
        let _ = self.try_emit_to(w, event);
    }

    pub fn emit(&self, event: &Event) {
        self.emit_to(&mut std::io::stderr(), event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_announcement_carries_a_timestamp_and_the_zirv_prefix() {
        let events = [
            Event::InjectionComposed {
                layers: "default+repo".to_string(),
            },
            Event::Degraded {
                cause: "socket unavailable".to_string(),
            },
        ];
        for event in events {
            let line = event.line();
            assert!(line.starts_with('['), "got {line}");
            let closing = line.find(']').expect("a closing bracket");
            let stamp = &line[1..closing];
            assert_eq!(stamp.len(), 8, "HH:MM:SS is eight characters: {stamp}");
            assert_eq!(stamp.matches(':').count(), 2, "got {stamp}");
            assert!(line[closing..].contains("zirv \u{25b8}"), "got {line}");
        }
    }

    #[test]
    fn an_announcement_is_one_line_and_survives_colour_being_stripped() {
        let event = Event::RotAdvisory {
            score: 65,
            tokens: 120_000,
        };
        let announcer = Announcer::new(true, true);
        let rendered = announcer.render(&event);

        assert_eq!(rendered.lines().count(), 1, "got {rendered:?}");
        assert_eq!(
            console::strip_ansi_codes(&rendered).to_string(),
            event.line(),
            "stripping colour must recover the exact plain content"
        );
    }

    #[test]
    fn quiet_suppresses_announcements_without_suppressing_errors() {
        let event = Event::Degraded {
            cause: "test".to_string(),
        };
        let mut buf = Vec::new();
        Announcer::new(false, false).emit_to(&mut buf, &event);
        assert!(
            buf.is_empty(),
            "a disabled announcer must write nothing at all"
        );

        let mut buf = Vec::new();
        Announcer::new(true, false).emit_to(&mut buf, &event);
        assert!(
            !buf.is_empty(),
            "an enabled announcer still writes when colour is off"
        );

        // `output::error`/`output::warn` are a structurally separate channel:
        // neither function takes an `Announcer` or a `cfg.chrome` value, so
        // there is no path by which this module's quiet flag reaches them.
        // (Their own `eprintln!`-based bodies are exercised by output's own
        // tests, not here.)
    }

    #[test]
    fn the_prompt_injection_announcement_names_the_layers_that_were_composed() {
        let event = Event::InjectionComposed {
            layers: "v3 layers: default+adapter+harness".to_string(),
        };
        assert!(
            event.line().contains("default+adapter+harness"),
            "got {}",
            event.line()
        );
    }

    #[test]
    fn a_skipped_injection_announcement_says_why_it_was_skipped() {
        let event = Event::InjectionSkipped {
            reason: "no prompt composed (simple run or prompt disabled)".to_string(),
        };
        assert!(
            event.line().contains("simple run or prompt disabled"),
            "got {}",
            event.line()
        );
    }

    /// Issue #89: the sandbox-residual announcement must name the resolved
    /// distiller/reviewer's own recorded gap plainly, not just gesture at
    /// "something is degraded".
    #[test]
    fn the_sandbox_residual_announcement_names_the_gap() {
        let event = Event::SandboxResidual {
            note: "codex's report-only sandbox (--sandbox read-only) could not add \
                   --ignore-rules --ignore-user-config on this installed codex-cli"
                .to_string(),
        };
        assert!(
            event.line().contains("sandbox residual:"),
            "got {}",
            event.line()
        );
        assert!(
            event.line().contains("--ignore-rules"),
            "got {}",
            event.line()
        );
    }

    /// Issue #87: a durable memory harvest must announce a one-line summary
    /// on the `zirv ▸` channel, singular/plural and zero-count included --
    /// a harvest that ran and accepted nothing is still worth reporting, not
    /// silently indistinguishable from a harvest that never ran.
    #[test]
    fn the_memory_harvested_announcement_names_the_count() {
        let none = Event::MemoryHarvested { count: 0 };
        assert!(
            none.line().contains("wrote 0 durable entries"),
            "got {}",
            none.line()
        );

        let one = Event::MemoryHarvested { count: 1 };
        assert!(
            one.line().contains("wrote 1 durable entry"),
            "got {}",
            one.line()
        );
        assert!(
            !one.line().contains("entries"),
            "singular must not also say entries: {}",
            one.line()
        );

        let many = Event::MemoryHarvested { count: 3 };
        assert!(
            many.line().contains("wrote 3 durable entries"),
            "got {}",
            many.line()
        );
    }

    /// Bug B (harness/model parity, 2026-08-22): the shipped-default
    /// sandbox posture must be visible on the same channel every other
    /// launch-time decision already narrates through -- both the applied
    /// case (the exact argv) and the not-applied case (a short reason).
    #[test]
    fn the_sandbox_posture_announcement_names_the_applied_argv() {
        let event = Event::SandboxPosture {
            detail: "--sandbox workspace-write --ask-for-approval never".to_string(),
        };
        assert!(
            event
                .line()
                .contains("sandbox posture: --sandbox workspace-write --ask-for-approval never"),
            "got {}",
            event.line()
        );
    }

    #[test]
    fn the_sandbox_posture_announcement_names_why_it_was_not_applied() {
        let event = Event::SandboxPosture {
            detail: "not applied ([sandbox] enabled = false)".to_string(),
        };
        assert!(
            event
                .line()
                .contains("sandbox posture: not applied ([sandbox] enabled = false)"),
            "got {}",
            event.line()
        );
    }

    /// The disclosure a repo cannot silence: `chrome.events` is
    /// `REPO_FORBIDDEN`, `chrome.banner` is not.
    #[test]
    fn the_chat_model_announcement_names_the_model_and_where_it_came_from() {
        let event = Event::ChatModel {
            model: "fable".to_string(),
        };
        assert!(
            event.line().contains("chat model 'fable' (from config)"),
            "got {}",
            event.line()
        );
    }

    /// A stray keystroke in an untrusted repo `ctx.toml` must never be a
    /// silent full-permissive fallback: the announcement names the file(s)
    /// and the parse message, and never claims the config load failed
    /// outright (it degraded to defaults, it did not abort).
    #[test]
    fn a_config_unparsable_announcement_names_the_layer_and_the_parse_message() {
        let event = Event::ConfigUnparsable {
            detail: ".zirv/ctx.toml: TOML parse error at line 1, column 2: key with no value, \
                      expected `=`"
                .to_string(),
        };
        let line = event.line();
        assert!(line.contains(".zirv/ctx.toml"), "got {line}");
        assert!(line.contains("line 1, column 2"), "got {line}");
        assert!(line.contains("skipped"), "got {line}");
        assert!(
            line.contains("defaults used"),
            "must read as a degrade, not a hard failure: {line}"
        );
    }

    #[test]
    fn a_score_announcement_is_made_only_when_the_verdict_band_changes() {
        assert_eq!(verdict_change(Verdict::Healthy, Verdict::Healthy, 10), None);
        let changed = verdict_change(Verdict::Advise, Verdict::Compact, 65)
            .expect("a real change produces an event");
        assert!(changed.line().contains("advise"), "got {}", changed.line());
        assert!(changed.line().contains("compact"), "got {}", changed.line());
        assert!(changed.line().contains("65"), "got {}", changed.line());
    }

    #[test]
    fn a_compact_announcement_reports_whether_verification_succeeded() {
        let verified = Event::Compact { verified: true };
        assert!(
            verified.line().contains("verified"),
            "got {}",
            verified.line()
        );
        assert!(
            !verified.line().contains("not verified"),
            "got {}",
            verified.line()
        );

        let not_verified = Event::Compact { verified: false };
        assert!(
            not_verified.line().contains("not verified"),
            "got {}",
            not_verified.line()
        );
    }

    #[test]
    fn a_restart_announcement_names_the_handoff_style_and_where_it_was_stored() {
        let event = Event::Restart {
            style: "distilled".to_string(),
            stored: "/state/handoffs/abc.md".to_string(),
        };
        let line = event.line();
        assert!(line.contains("distilled"), "got {line}");
        assert!(line.contains("/state/handoffs/abc.md"), "got {line}");
    }

    #[test]
    fn a_pacing_wait_announcement_names_the_window_and_when_it_resets() {
        let known = Event::PacingWait {
            window: "five_hour".to_string(),
            reset_at: Some(1_785_507_915),
        };
        let line = known.line();
        assert!(line.contains("five_hour"), "got {line}");
        assert!(line.contains("1785507915"), "got {line}");

        let unknown = Event::PacingWait {
            window: "seven_day".to_string(),
            reset_at: None,
        };
        assert!(unknown.line().contains("unknown"), "got {}", unknown.line());
    }

    /// T8: names the provider and the delay, and points at the remedy --
    /// this must never read as "pacing is off"/"skipped" any more, since it
    /// is a fail-SAFE degrade, not a fail-open skip.
    #[test]
    fn a_pacing_blind_announcement_names_the_provider_delay_and_remedy() {
        let event = Event::PacingBlind {
            provider: "openai".to_string(),
            delay_secs: 60,
        };
        let line = event.line();
        assert!(line.contains("openai"), "got {line}");
        assert!(line.contains("no usage source"), "got {line}");
        assert!(line.contains("60s"), "got {line}");
        assert!(line.contains("zirv ctx status"), "got {line}");
        assert!(
            !line.to_lowercase().contains("pacing off"),
            "must not read as fully off: {line}"
        );
    }

    /// Item 4: `Slow` used to be invisible on the `zirv ▸` channel -- only
    /// `PacingWait` (the hard pause) ever announced anything, so a
    /// potentially hours-long soft throttle produced no visible line at all.
    #[test]
    fn a_pacing_throttled_announcement_names_the_window_delay_and_percent() {
        let event = Event::PacingThrottled {
            window: "five_hour".to_string(),
            delay_secs: 473,
            percent: 90.0,
        };
        let line = event.line();
        assert!(line.contains("five_hour"), "got {line}");
        assert!(line.contains("473"), "got {line}");
        assert!(line.contains("90.0"), "got {line}");
    }

    #[test]
    fn a_mail_delivery_announcement_counts_the_notes_it_added() {
        let one = Event::MailDelivered { count: 1 };
        assert!(one.line().contains("1 mail message "), "got {}", one.line());
        let many = Event::MailDelivered { count: 3 };
        assert!(
            many.line().contains("3 mail messages "),
            "got {}",
            many.line()
        );
    }

    /// Low 8: `exec.rs`'s own visibility gap -- when `mail_deliverable` is
    /// false (an explicit `--` command for an adapter with no system-prompt
    /// mechanism), mail is left unread with no announcement at all, so an
    /// operator watching the `zirv ▸` channel could not tell "nothing
    /// pending" from "something pending but silently withheld".
    #[test]
    fn a_mail_withheld_announcement_counts_and_explains_itself() {
        let one = Event::MailWithheld { count: 1 };
        let line = one.line();
        assert!(line.contains("1 mail message "), "got {line}");
        assert!(line.contains("cannot reach"), "got {line}");
        assert!(line.contains("unread"), "got {line}");

        let many = Event::MailWithheld { count: 2 };
        assert!(
            many.line().contains("2 mail messages "),
            "got {}",
            many.line()
        );
    }

    #[test]
    fn a_degraded_supervision_announcement_names_the_failure_that_caused_it() {
        let event = Event::Degraded {
            cause: "compaction not verified".to_string(),
        };
        assert!(
            event.line().contains("compaction not verified"),
            "got {}",
            event.line()
        );
    }

    #[test]
    fn a_delegated_run_announces_its_start_and_the_meaning_of_its_exit() {
        let start = Event::DelegatedStart {
            agent: "codex".to_string(),
        };
        assert!(start.line().contains("codex"), "got {}", start.line());

        let finish = Event::DelegatedFinish {
            agent: "codex".to_string(),
            meaning: "the session kept rotting and the restart budget ran out".to_string(),
        };
        let line = finish.line();
        assert!(line.contains("codex"), "got {line}");
        assert!(line.contains("restart budget ran out"), "got {line}");
    }

    /// Item 6 (audit): `wrap`'s pump loop used to end the wrapped session
    /// with no narration at all -- the child's own exit code and nothing
    /// else. This is the announcement that fixes it, naming both the agent
    /// and the exit code so it reads as an event, not a silent stop.
    #[test]
    fn a_session_ended_announcement_names_the_agent_and_the_exit_code() {
        let clean = Event::SessionEnded {
            agent: "claude".to_string(),
            code: 0,
        };
        let line = clean.line();
        assert!(line.contains("claude"), "got {line}");
        assert!(line.contains('0'), "got {line}");

        let crashed = Event::SessionEnded {
            agent: "codex".to_string(),
            code: 134,
        };
        let line = crashed.line();
        assert!(line.contains("codex"), "got {line}");
        assert!(line.contains("134"), "got {line}");
    }

    /// C4: all three dispositions are distinct promises, and every one of
    /// them names the *sender*. Collapsing the last two into one boolean is
    /// what told an interactive session its nudge would "be picked up as
    /// mail" -- the one thing that never happens there.
    #[test]
    fn a_nudge_announcement_names_the_sender_and_what_will_happen_next() {
        let relaunching = Event::Nudge {
            from: "aaaa1111".to_string(),
            disposition: NudgeDisposition::Relaunching,
        };
        let line = relaunching.line();
        assert!(line.contains("aaaa1111"), "got {line}");
        assert!(line.contains("relaunching"), "got {line}");

        let next_cycle = Event::Nudge {
            from: "aaaa1111".to_string(),
            disposition: NudgeDisposition::NextCycle,
        };
        let line = next_cycle.line();
        assert!(line.contains("aaaa1111"), "got {line}");
        assert!(
            !line.contains("relaunching"),
            "a loop cycle must not claim it relaunched: {line}"
        );
        assert!(
            line.contains("next cycle"),
            "a loop says when it will pick the payload up: {line}"
        );

        let advisory = Event::Nudge {
            from: "aaaa1111".to_string(),
            disposition: NudgeDisposition::Advisory,
        };
        let line = advisory.line();
        assert!(line.contains("aaaa1111"), "got {line}");
        assert!(
            !line.contains("relaunching"),
            "an interactive session is never relaunched: {line}"
        );
        assert!(
            line.contains("zirv ctx inbox"),
            "an interactive session never receives bodies, so it must point at              inbox rather than promise delivery: {line}"
        );
        assert!(
            !line.contains("picked up as mail"),
            "the old wording promised a delivery that never happens here: {line}"
        );
    }

    /// The macOS Keychain-prompt heads-up: names both halves of the promise
    /// (approve once with "Always Allow", or a headless run times out rather
    /// than hangs) so an operator reading only this one line still knows what
    /// to do either way.
    #[test]
    fn a_macos_keychain_prompt_announcement_explains_both_outcomes() {
        let line = Event::MacosKeychainPromptExpected.line();
        assert!(line.contains("Claude Code-credentials"), "got {line}");
        assert!(line.contains("Always Allow"), "got {line}");
        assert!(line.contains("time out"), "got {line}");
    }

    #[test]
    fn announcements_never_touch_the_reserved_bar_row() {
        let sample = [
            Event::InjectionComposed {
                layers: "default".to_string(),
            },
            Event::InjectionSkipped {
                reason: "simple".to_string(),
            },
            Event::VerdictChanged {
                from: Verdict::Healthy,
                to: Verdict::Advise,
                score: 45,
            },
            Event::Compact { verified: true },
            Event::Restart {
                style: "structural".to_string(),
                stored: "x.md".to_string(),
            },
            Event::MailWaiting { count: 2 },
            Event::MailDelivered { count: 2 },
            Event::MailWithheld { count: 2 },
            Event::PacingWait {
                window: "five_hour".to_string(),
                reset_at: None,
            },
            Event::PacingBlind {
                provider: "openai".to_string(),
                delay_secs: 60,
            },
            Event::PacingThrottled {
                window: "five_hour".to_string(),
                delay_secs: 100,
                percent: 85.0,
            },
            Event::Degraded {
                cause: "x".to_string(),
            },
            Event::RotAdvisory {
                score: 50,
                tokens: 1,
            },
            Event::DelegatedStart {
                agent: "claude".to_string(),
            },
            Event::DelegatedFinish {
                agent: "claude".to_string(),
                meaning: "exited with code 0".to_string(),
            },
            Event::SessionEnded {
                agent: "claude".to_string(),
                code: 0,
            },
            Event::Nudge {
                from: "aaaa1111".to_string(),
                disposition: NudgeDisposition::Relaunching,
            },
            Event::MacosKeychainPromptExpected,
            Event::ConfigUnparsable {
                detail: ".zirv/ctx.toml: TOML parse error at line 1, column 2: bad".to_string(),
            },
        ];
        for event in sample {
            let line = event.line();
            assert!(
                !line.contains('\u{1b}'),
                "an event's own line must carry no escape sequences at all: {line:?}"
            );
        }
    }

    /// A writer that always fails, standing in for a stderr that has gone
    /// away (a closed pipe, a full device).
    struct BrokenWriter;

    impl Write for BrokenWriter {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("stderr is gone"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("stderr is gone"))
        }
    }

    /// The reason `try_emit_to` exists: a caller that *records having said
    /// something* (wrap's mail watch) must be able to tell a line that landed
    /// from one this channel swallowed, or it marks an advisory as delivered
    /// that nobody ever saw.
    #[test]
    fn an_announcement_that_never_surfaced_is_reported_rather_than_swallowed() {
        let event = Event::MailWaiting { count: 1 };

        let mut buf = Vec::new();
        assert!(
            !Announcer::new(false, false).try_emit_to(&mut buf, &event),
            "a disabled channel surfaced nothing"
        );
        assert!(buf.is_empty());

        assert!(
            !Announcer::silent().try_emit_to(&mut BrokenWriter, &event),
            "silent is disabled too"
        );
        assert!(
            !Announcer::new(true, false).try_emit_to(&mut BrokenWriter, &event),
            "an enabled channel whose write failed surfaced nothing either"
        );

        let mut buf = Vec::new();
        assert!(
            Announcer::new(true, false).try_emit_to(&mut buf, &event),
            "an enabled channel with a working writer reports success"
        );
        assert!(!buf.is_empty());
    }

    /// `emit_to` keeps its infallible shape for every other call site, and
    /// still writes exactly what `try_emit_to` does.
    #[test]
    fn the_infallible_emit_still_writes_the_same_bytes() {
        let event = Event::MailWaiting { count: 2 };
        let announcer = Announcer::new(true, false);

        let mut infallible = Vec::new();
        announcer.emit_to(&mut infallible, &event);
        let mut reporting = Vec::new();
        assert!(announcer.try_emit_to(&mut reporting, &event));

        assert_eq!(infallible, reporting);
        // And a broken writer is still not a panic on the infallible path.
        announcer.emit_to(&mut BrokenWriter, &event);
    }

    #[test]
    fn a_silent_announcer_matches_a_disabled_one() {
        let mut buf = Vec::new();
        Announcer::silent().emit_to(&mut buf, &Event::MailWaiting { count: 1 });
        assert!(buf.is_empty());
    }
}
