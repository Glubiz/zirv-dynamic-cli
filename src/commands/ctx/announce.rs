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
    /// (`"distilled"` or `"structural"`, `handoff::distill_or_structural`'s
    /// own vocabulary) and where the handoff was stored.
    Restart { style: String, stored: String },
    /// Unread mail is waiting in the mailbox (the T8 advisory `wrap`'s pump
    /// used to build by hand).
    MailWaiting { count: usize },
    /// Mail was folded into a composed prompt at session start.
    MailDelivered { count: usize },
    /// The pacing gate is waiting on a usage window, naming which one and
    /// when it is expected to reset.
    PacingWait {
        window: String,
        reset_at: Option<u64>,
    },
    /// Supervision degraded to permanent passthrough, naming what caused it.
    Degraded { cause: String },
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
    /// typed into, and never receives bodies -- a human has to go read it.
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
            Event::PacingWait { window, reset_at } => match reset_at {
                Some(at) => format!("pacing: waiting on the {window} window, resets at unix {at}"),
                None => format!("pacing: waiting on the {window} window, reset time unknown"),
            },
            Event::Degraded { cause } => format!("supervision degraded: {cause}"),
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

    /// Framed exactly like the advisories this channel replaces (`\r\n{line}\r`,
    /// via `writeln!`'s own trailing newline), so it interleaves safely with a
    /// raw-mode child's own cursor movement. A no-op when disabled: callers
    /// call this unconditionally on every event, and quiet is enforced here
    /// once rather than at every call site.
    pub fn emit_to<W: Write>(&self, w: &mut W, event: &Event) {
        if !self.enabled {
            return;
        }
        let _ = writeln!(w, "\r\n{}\r", self.render(event));
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
            Event::PacingWait {
                window: "five_hour".to_string(),
                reset_at: None,
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
        ];
        for event in sample {
            let line = event.line();
            assert!(
                !line.contains('\u{1b}'),
                "an event's own line must carry no escape sequences at all: {line:?}"
            );
        }
    }

    #[test]
    fn a_silent_announcer_matches_a_disabled_one() {
        let mut buf = Vec::new();
        Announcer::silent().emit_to(&mut buf, &Event::MailWaiting { count: 1 });
        assert!(buf.is_empty());
    }
}
