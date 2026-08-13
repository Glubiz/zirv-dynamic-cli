//! Terminal "chrome": whether a session gets the launch banner, the reserved
//! status bar (T12b) and colour, plus the pure renderers for the banner and
//! for a styled copy of the aggregated no-adapter error. Every function here
//! is pure -- no I/O, no terminal handle, no clock -- so the eligibility
//! rules and the exact text are unit-testable without a real pty. Callers
//! (`chat.rs`, `wrap.rs`) do the actual terminal probing (`term::window_size`,
//! `term::enable_vt_output`, an `isatty` check on stdout) and pass the
//! results in.
//!
//! Chrome degrades in one direction only: a probe failure, a too-small
//! terminal, `--simple`, or `--no-supervise` turns every piece of chrome off.
//! Nothing here ever upgrades a session mid-run, and nothing here ever
//! touches the wrapped child: nobody but `wrap`'s own pump reads or writes
//! the child's pty.

use super::config::{ChromeConfig, DashConfig};

/// Below this width the banner and the status bar both risk wrapping their
/// own text across lines, which corrupts the bar's single-line redraw.
pub const MIN_COLS: u16 = 40;
/// Below this height a reserved bottom row would eat a meaningful fraction of
/// the visible terminal.
pub const MIN_ROWS: u16 = 8;

/// The dashboard's own, taller floor: a sidebar plus a usable pane grid needs
/// more room than the banner/status-bar chrome above. Measured by the vt100
/// spike (`docs/superpowers/notes/2026-08-13-vt100-spike.md`): no rendering
/// problems were observed at these sizes, so the defaults stand as the
/// minimums.
pub const MIN_DASH_COLS: u16 = 80;
pub const MIN_DASH_ROWS: u16 = 20;

/// Whether `zirv chat` should open the dashboard rather than falling through
/// to the plain `wrap` passthrough session. Pure: every input is caller-
/// supplied, so this is unit-testable without a real terminal.
///
/// `simple` and `!cfg.enabled` both turn eligibility off outright, matching
/// `ChromeCaps::probe`'s own one-way degrade rule -- `--simple` promises a
/// plain session, and an operator who disabled the dashboard gets exactly
/// that, unconditionally. Both `stdout_tty` and `stdin_tty` have to be real
/// terminals (not just stdout, the way `ChromeCaps::probe` only checks
/// stdout): the dashboard reads keystrokes from stdin to drive pane
/// selection and overlays, so a piped stdin can never make a usable session
/// even if stdout happens to be a terminal.
pub fn dash_eligible(
    stdout_tty: bool,
    stdin_tty: bool,
    vt_ok: bool,
    size: (u16, u16),
    cfg: &DashConfig,
    simple: bool,
) -> bool {
    if simple || !cfg.enabled || !stdout_tty || !stdin_tty || !vt_ok {
        return false;
    }
    let (cols, rows) = size;
    cols >= MIN_DASH_COLS && rows >= MIN_DASH_ROWS
}

/// What this session gets, decided once at launch. `colour` is ANDed with
/// `vt_ok`: on Windows, ANSI colour codes are only interpreted once VT
/// processing is on, so colour without VT would print raw escape bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Chrome {
    pub banner: bool,
    pub bar: bool,
    pub colour: bool,
}

pub struct ChromeCaps;

impl ChromeCaps {
    /// Pure decision from the caller-supplied probe results: no terminal
    /// handle is opened here, and nothing is re-probed. `simple` and
    /// `no_supervise` both promise a plain passthrough session, so both turn
    /// every piece of chrome off exactly like a non-terminal or an
    /// undersized one does.
    pub fn probe(
        stdout_is_tty: bool,
        vt_ok: bool,
        size: (u16, u16),
        cfg: &ChromeConfig,
        simple: bool,
        no_supervise: bool,
    ) -> Chrome {
        let (cols, rows) = size;
        let big_enough = cols >= MIN_COLS && rows >= MIN_ROWS;
        let eligible = stdout_is_tty && big_enough && !simple && !no_supervise;
        if !eligible {
            return Chrome::default();
        }

        let colour = vt_ok && console::colors_enabled();
        Chrome {
            banner: cfg.banner,
            // The bar draws with cursor-addressing escapes (see T12b), which
            // need VT even to be legible, let alone useful; without it the
            // bar is turned off outright rather than shipped broken.
            bar: cfg.bar && vt_ok,
            colour,
        }
    }
}

/// Which rule picked the harness this session launched, for the banner. A
/// finer grain than `adapters::DefaultOrigin`: that enum only distinguishes
/// "configured" from "first enabled and ready", and has no case at all for an
/// explicit `--agent`, which never reaches `resolve_default` in the first
/// place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarnessRule {
    /// `--agent <name>` on the command line.
    Explicit,
    /// `agent = "<name>"` in `ctx.toml`.
    Configured,
    /// No explicit or configured choice; the first registry entry that was
    /// both gate-enabled and `ready()`.
    FirstEnabledReady,
}

impl HarnessRule {
    pub fn describe(&self) -> &'static str {
        match self {
            HarnessRule::Explicit => "requested with --agent",
            HarnessRule::Configured => "configured as the default agent",
            HarnessRule::FirstEnabledReady => "the first enabled, ready harness",
        }
    }
}

/// Everything the banner needs to render, gathered by the caller (`chat.rs`)
/// from the resolved adapter, the agent gate and the resume lookup. Kept
/// separate from any live terminal or config type so `banner` stays pure.
#[derive(Debug, Clone, PartialEq)]
pub struct BannerFacts {
    pub harness: String,
    pub rule: HarnessRule,
    pub session: String,
    /// Every known harness, in registry order, alongside whether the gate
    /// currently enables it.
    pub harnesses: Vec<(String, bool)>,
    /// `Some` names where the resumed handoff came from (a short human
    /// description, not a path) when `--resume` folded one into this launch.
    pub resuming: Option<String>,
    /// `cfg.chat.model`/`ZIRV_CTX_CHAT_MODEL` when set: the model the
    /// orchestrator session was launched with, shown so an operator who
    /// configured it can see at a glance that it actually took effect.
    /// `None` renders no line at all, matching the resuming field's own
    /// "absent means nothing to say" convention.
    pub model: Option<String>,
}

/// `colour` here is already the final decision (`Chrome::colour`, or an
/// explicit test value): forcing styling on rather than letting
/// `StyledObject`'s own `Display` impl re-check the process-global
/// `console::colors_enabled()` flag is what keeps this deterministic. Without
/// `force_styling`, a caller that computed `colour = true` still rendered
/// plain text whenever the global flag happened to be off (a no-color CI
/// environment, or -- in tests -- a leftover from whichever test last
/// touched `console::set_colors_enabled` and ran first).
fn styled(
    text: &str,
    colour: bool,
    apply: impl FnOnce(console::StyledObject<&str>) -> console::StyledObject<&str>,
) -> String {
    if colour {
        apply(console::style(text).force_styling(true)).to_string()
    } else {
        text.to_string()
    }
}

/// Pure text builder for the launch banner. `colour` and `vt` both have to be
/// true for any styling to appear; without either one the banner is plain
/// text, which is always legible and always a valid fallback.
pub fn banner(facts: &BannerFacts, colour: bool, vt: bool) -> String {
    let colour = colour && vt;
    let mut lines = Vec::new();

    lines.push(format!(
        "{} {} ({})",
        styled("zirv chat", colour, |s| s.cyan().bold()),
        styled(&facts.harness, colour, |s| s.bold()),
        facts.rule.describe()
    ));
    lines.push(format!("session {}", facts.session));

    let harnesses = facts
        .harnesses
        .iter()
        .map(|(name, enabled)| {
            if *enabled {
                name.clone()
            } else {
                format!("{name} (disabled)")
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    lines.push(format!("harnesses: {harnesses}"));

    if let Some(resuming) = &facts.resuming {
        lines.push(format!("resuming: {resuming}"));
    }

    if let Some(model) = &facts.model {
        lines.push(format!("model: {model}"));
    }

    lines.join("\n")
}

/// Decorates the aggregated no-adapter error (one line per candidate, each
/// naming why it was skipped) without changing its text: the first line gets
/// emphasis, every following line is indented and dimmed. `colour` false
/// returns the input unchanged plus the same indentation, so the shape is
/// identical either way and only the escape codes differ.
pub fn style_no_adapter_error(raw: &str, colour: bool) -> String {
    let mut out = String::new();
    for (index, line) in raw.lines().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        if index == 0 {
            out.push_str(&styled(line, colour, |s| s.red().bold()));
        } else {
            out.push_str("  ");
            out.push_str(&styled(line, colour, |s| s.dim()));
        }
    }
    out
}

// T12b: the reserved bottom status bar.

use super::rot::Verdict;

/// A placeholder for a value that is not yet known -- an en dash, never a
/// zero: a session with no usage reading yet must not look like it is at 0%.
const PLACEHOLDER: &str = "\u{2013}";

/// Everything the bar draws. Every field that can be unknown is an `Option`
/// so `status_bar` can render the placeholder instead of a misleading zero.
#[derive(Debug, Clone, PartialEq)]
pub struct BarState {
    pub harness: String,
    pub score: Option<u32>,
    pub verdict: Option<Verdict>,
    /// The worse of the two usage windows, 0.0..=100.0.
    pub usage_percent: Option<f64>,
    /// N7: `(broadcast, direct-to-this-session)` unread counts, split so a
    /// message addressed to this session specifically is not lost inside an
    /// undifferentiated total. `None` under the same conditions the bar's
    /// caller already treats mail as unknown (disabled, or unreadable).
    pub unread_mail: Option<(usize, usize)>,
    pub degraded: bool,
}

/// Pure renderer: one line, right-truncated to `cols`, no cursor-addressing
/// escapes of its own (the caller wraps it in the redraw sequence). `colour`
/// dims the whole line rather than picking out individual fields, since this
/// is status chrome, not something that needs field-level emphasis.
pub fn status_bar(state: &BarState, cols: u16, colour: bool) -> String {
    let score_verdict = match (state.score, state.verdict) {
        (Some(score), Some(verdict)) => format!("{score} {}", verdict.as_str()),
        _ => PLACEHOLDER.to_string(),
    };
    let usage = state
        .usage_percent
        .map(|p| format!("{p:.0}%"))
        .unwrap_or_else(|| PLACEHOLDER.to_string());
    // N7: a direct count of zero renders as plain `mail N` -- identical to
    // the bar's pre-N7 wording -- and only grows a `+direct` suffix once
    // something is actually addressed to this session specifically.
    let mail = match state.unread_mail {
        None => PLACEHOLDER.to_string(),
        Some((broadcast, 0)) => broadcast.to_string(),
        Some((broadcast, direct)) => format!("{broadcast}+{direct}"),
    };
    let supervision = if state.degraded {
        "degraded"
    } else {
        "supervised"
    };

    let text = format!(
        "{} | score {score_verdict} | usage {usage} | mail {mail} | {supervision}",
        state.harness
    );
    let truncated = right_truncate(&text, cols as usize);
    if colour {
        // `force_styling`: same reasoning as `styled` above -- `colour` is
        // already the final decision, so this must not be re-gated by the
        // process-global `console::colors_enabled()` flag a second time.
        console::style(truncated)
            .dim()
            .force_styling(true)
            .to_string()
    } else {
        truncated
    }
}

/// Keeps the leftmost `cols` characters (not bytes: a multi-byte placeholder
/// character must not be split), dropping whatever would overflow on the
/// right. Never produces more than `cols` characters.
fn right_truncate(s: &str, cols: usize) -> String {
    if s.chars().count() <= cols {
        return s.to_string();
    }
    s.chars().take(cols).collect()
}

/// The pty size to open (or resize to) when the bar reserves the bottom row:
/// one row shorter than the terminal, floored at 1 so a pathologically short
/// terminal never asks for a zero-row pty. Returns `size` unchanged when the
/// bar is off.
pub fn reserved_pty_size(size: (u16, u16), bar_on: bool) -> (u16, u16) {
    if !bar_on {
        return size;
    }
    (size.0, size.1.saturating_sub(1).max(1))
}

/// The real terminal's scroll region confining the child (and any
/// `announce`-channel line printed above the bar) to every row except the
/// reserved one. `rows` is the terminal's own row count, matching
/// `reserved_pty_size`'s reservation of exactly one row.
///
/// Wrapped in a cursor save/restore (`ESC7` ... `ESC8`): `CSI r` (DECSTBM) is
/// specified to home the cursor to the scroll region's own top-left the
/// moment it takes effect, in every VT100-descended terminal including every
/// real one this ships on. Writing it bare would yank the cursor back to row
/// 1 out from under whatever the session had already printed there (the
/// launch banner, in particular) -- B2. The save has to come *before* the
/// region write, and the restore after, so what gets saved is the real
/// pre-existing position rather than the row the region write just homed to.
pub fn scroll_region_sequence(rows: u16) -> String {
    format!("\x1b7\x1b[1;{}r\x1b8", rows.saturating_sub(1).max(1))
}

/// One assembled redraw buffer: save cursor, jump to the reserved row, clear
/// it, write the bar text, restore the cursor. Callers write this in one
/// `write_all` call so a concurrent write from the output thread can never
/// land in the middle of it.
pub fn bar_redraw_sequence(rows: u16, text: &str) -> String {
    format!("\x1b7\x1b[{rows};1H\x1b[2K{text}\x1b8")
}

/// Undoes `scroll_region_sequence` and clears the reserved row, for every
/// place `wrap` gives up the reserved row: the final cleanup alongside
/// `RawGuard::restore`, and (B1) the moment the bar degrades mid-session.
///
/// Same save-first ordering as `scroll_region_sequence` and for the same
/// reason: `CSI r` homes the cursor the instant it runs, so the position has
/// to be captured *before* that happens, or what gets restored is the homed
/// position rather than wherever the session's own output actually was.
pub fn bar_reset_sequence(rows: u16) -> String {
    format!("\x1b7\x1b[r\x1b[{rows};1H\x1b[2K\x1b8")
}

/// Whether a terminal has shrunk below the floor the bar needs to still be
/// legible in either dimension -- the post-resize half of the degrade rule
/// (`ChromeCaps::probe` covers the pre-launch half, and already checks both).
pub fn bar_should_disable(size: (u16, u16)) -> bool {
    size.0 < MIN_COLS || size.1 < MIN_ROWS
}

/// One redraw attempt's effect on the permanent degrade flag: a one-way
/// switch, exactly like `wrap::note_failure`. Once disabled by any probe,
/// lock or write failure, every later attempt (success or not) leaves it
/// disabled; the child is never referenced by this decision either way, so
/// nothing here can end it.
pub fn after_redraw_attempt(chrome_disabled: bool, write_succeeded: bool) -> bool {
    chrome_disabled || !write_succeeded
}

/// Whether the bar is due for a redraw: only when the rendered text has
/// actually changed since the last draw. Paired with an external 1s
/// wall-clock throttle in `wrap`'s pump (not modeled here, since it needs a
/// real clock); this half is the content comparison, always correct
/// regardless of how the throttle is timed.
pub fn bar_text_changed(last: Option<&str>, next: &str) -> bool {
    last != Some(next)
}

/// B1: what one resize tick does to the bar and the child pty, decided
/// without touching a pty handle so `wrap`'s pump can apply it verbatim and
/// the decision itself stays unit-testable. `bar_was_active` is the bar's
/// state going *into* this resize (`chrome.bar && !disabled`): once it is
/// `false` -- never eligible, or already degraded -- every later resize is
/// ordinary full-size forwarding forever, exactly like a bar-less session;
/// nothing here ever re-enables a bar that degraded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResizeDecision {
    /// The pty size to apply this tick.
    pub pty_size: (u16, u16),
    /// Whether the bar's own scroll region needs to be (re)written this tick
    /// (only true while the bar is still active and stays that way).
    pub set_scroll_region: bool,
    /// Whether this tick is the one that disables the bar (the terminal
    /// shrank below the floor in either dimension).
    pub disables_bar: bool,
}

pub fn resize_decision(bar_was_active: bool, new_size: (u16, u16)) -> ResizeDecision {
    if !bar_was_active {
        return ResizeDecision {
            pty_size: new_size,
            set_scroll_region: false,
            disables_bar: false,
        };
    }
    if bar_should_disable(new_size) {
        // B1: the whole point is that this is *not* `reserved_pty_size` --
        // a session that just lost its bar must reach full size immediately,
        // not stay pinned at the last reserved height.
        ResizeDecision {
            pty_size: new_size,
            set_scroll_region: false,
            disables_bar: true,
        }
    } else {
        ResizeDecision {
            pty_size: reserved_pty_size(new_size, true),
            set_scroll_region: true,
            disables_bar: false,
        }
    }
}

/// B1: whether the bar's one-time degrade cleanup (clear the reserved row,
/// stop pinning the pty size) still needs to run. `true` exactly once, on
/// the first check after the bar shows disabled; every later check (with
/// `recovered` now `true`) is a no-op, and a bar that was never eligible
/// (`chrome_bar` false) never needed any cleanup in the first place.
pub fn bar_needs_recovery(chrome_bar: bool, disabled: bool, recovered: bool) -> bool {
    chrome_bar && disabled && !recovered
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ChromeConfig {
        ChromeConfig::default()
    }

    const SIZE: (u16, u16) = (100, 30);

    #[test]
    fn chrome_is_off_when_stdout_is_not_a_terminal() {
        let chrome = ChromeCaps::probe(false, true, SIZE, &cfg(), false, false);
        assert_eq!(chrome, Chrome::default());
    }

    #[test]
    fn chrome_is_off_for_a_simple_or_unsupervised_run() {
        let simple = ChromeCaps::probe(true, true, SIZE, &cfg(), true, false);
        assert_eq!(simple, Chrome::default());

        let no_supervise = ChromeCaps::probe(true, true, SIZE, &cfg(), false, true);
        assert_eq!(no_supervise, Chrome::default());
    }

    #[test]
    fn chrome_is_off_below_the_minimum_terminal_size() {
        let too_narrow = ChromeCaps::probe(true, true, (MIN_COLS - 1, 30), &cfg(), false, false);
        assert_eq!(too_narrow, Chrome::default());

        let too_short = ChromeCaps::probe(true, true, (100, MIN_ROWS - 1), &cfg(), false, false);
        assert_eq!(too_short, Chrome::default());

        let exactly_at_the_floor =
            ChromeCaps::probe(true, true, (MIN_COLS, MIN_ROWS), &cfg(), false, false);
        assert!(exactly_at_the_floor.banner, "the floor itself is eligible");
    }

    #[test]
    fn chrome_renders_without_colour_when_no_color_is_set() {
        // console's colour detection is a process-wide, once-computed flag;
        // `set_colors_enabled` is the crate's own supported way to simulate
        // what a NO_COLOR-influenced first read would have produced, without
        // depending on cargo test's own I/O environment. Restored to
        // whatever it actually was, not hardcoded back to `true`: this test
        // otherwise leaks a forced-on flag process-wide, which is exactly
        // what made `the_banner_is_plain_text_when_the_terminal_cannot_do_vt`
        // pass only when it happened to run after this one (S6).
        let original = console::colors_enabled();
        console::set_colors_enabled(false);
        let chrome = ChromeCaps::probe(true, true, SIZE, &cfg(), false, false);
        console::set_colors_enabled(original);

        assert!(!chrome.colour, "NO_COLOR must suppress colour");
        assert!(
            chrome.banner && chrome.bar,
            "colour is independent of whether chrome itself is eligible"
        );
    }

    #[test]
    fn a_terminal_that_cannot_do_vt_still_gets_a_plain_banner_and_no_bar() {
        let chrome = ChromeCaps::probe(true, false, SIZE, &cfg(), false, false);
        assert!(chrome.banner, "the banner still shows, just plain");
        assert!(!chrome.colour, "no VT means no escape codes at all");
        assert!(!chrome.bar, "the bar needs VT to draw at all");
    }

    fn facts() -> BannerFacts {
        BannerFacts {
            harness: "claude".to_string(),
            rule: HarnessRule::Configured,
            session: "abc12345".to_string(),
            harnesses: vec![("claude".to_string(), true), ("codex".to_string(), false)],
            resuming: None,
            model: None,
        }
    }

    #[test]
    fn the_banner_shows_the_model_when_configured() {
        let without = banner(&facts(), false, false);
        assert!(
            !without.to_lowercase().contains("model"),
            "no line at all when unset: {without}"
        );

        let mut with_model = facts();
        with_model.model = Some("fable".to_string());
        let text = banner(&with_model, false, false);
        assert!(text.contains("model: fable"), "got {text}");
    }

    #[test]
    fn the_banner_names_the_harness_and_the_rule_that_chose_it() {
        let text = banner(&facts(), false, false);
        assert!(text.contains("claude"), "got {text}");
        assert!(
            text.contains("configured as the default agent"),
            "got {text}"
        );
    }

    #[test]
    fn the_banner_lists_every_enabled_harness_and_marks_the_disabled_ones() {
        let text = banner(&facts(), false, false);
        assert!(text.contains("claude"), "got {text}");
        assert!(text.contains("codex (disabled)"), "got {text}");
        assert!(
            !text.contains("claude (disabled)"),
            "the enabled one is not marked: {text}"
        );
    }

    #[test]
    fn the_banner_says_when_a_stored_handoff_is_being_resumed() {
        let without = banner(&facts(), false, false);
        assert!(!without.to_lowercase().contains("resuming"));

        let mut with_resume = facts();
        with_resume.resuming = Some("the last stored handoff".to_string());
        let text = banner(&with_resume, false, false);
        assert!(text.to_lowercase().contains("resuming"), "got {text}");
        assert!(text.contains("the last stored handoff"), "got {text}");
    }

    #[test]
    fn the_banner_is_plain_text_when_the_terminal_cannot_do_vt() {
        let with_vt = banner(&facts(), true, true);
        let without_vt = banner(&facts(), true, false);
        assert_ne!(
            with_vt, without_vt,
            "colour requested with vt must differ from colour requested without it"
        );
        assert_eq!(
            without_vt,
            banner(&facts(), false, false),
            "no vt must render identically to no colour at all"
        );
        assert!(
            console::strip_ansi_codes(&with_vt) == without_vt,
            "the same content survives either way"
        );
    }

    #[test]
    fn the_no_adapter_error_keeps_every_per_adapter_reason_when_styling_is_stripped() {
        let raw = "no agent is both enabled and ready:\nclaude: disabled by .settings.toml\ncodex: not implemented yet (see issue #11)";
        let styled = style_no_adapter_error(raw, true);

        assert_ne!(styled, raw, "styling must actually add something");
        let stripped = console::strip_ansi_codes(&styled).to_string();
        assert!(
            stripped.contains("claude: disabled by .settings.toml"),
            "got {stripped}"
        );
        assert!(
            stripped.contains("codex: not implemented yet (see issue #11)"),
            "got {stripped}"
        );
        assert!(
            stripped.contains("no agent is both enabled and ready:"),
            "got {stripped}"
        );
    }

    #[test]
    fn an_unstyled_no_adapter_error_is_unchanged_content_with_indentation() {
        let raw = "no agent is both enabled and ready:\nclaude: disabled";
        let plain = style_no_adapter_error(raw, false);
        assert_eq!(
            plain,
            "no agent is both enabled and ready:\n  claude: disabled"
        );
    }

    // T12b: the reserved bottom status bar.

    fn bar_state() -> BarState {
        BarState {
            harness: "claude".to_string(),
            score: Some(42),
            verdict: Some(Verdict::Advise),
            usage_percent: Some(63.4),
            unread_mail: Some((2, 0)),
            degraded: false,
        }
    }

    #[test]
    fn the_bar_renders_harness_score_verdict_usage_mail_and_supervision_state() {
        let line = status_bar(&bar_state(), 200, false);
        assert!(line.contains("claude"), "got {line}");
        assert!(line.contains("42"), "got {line}");
        assert!(line.contains("advise"), "got {line}");
        assert!(line.contains("63%"), "got {line}");
        assert!(line.contains("mail 2"), "got {line}");
        assert!(line.contains("supervised"), "got {line}");
    }

    #[test]
    fn the_bar_is_truncated_from_the_right_and_never_exceeds_the_terminal_width() {
        let full = status_bar(&bar_state(), 200, false);
        assert!(
            full.chars().count() > 20,
            "sanity: the untruncated line is long"
        );

        let truncated = status_bar(&bar_state(), 12, false);
        assert_eq!(truncated.chars().count(), 12);
        assert!(
            full.starts_with(&truncated),
            "truncation drops the right side, not the left: {truncated:?} vs {full:?}"
        );
    }

    #[test]
    fn a_degraded_session_says_so_in_the_bar() {
        let mut degraded = bar_state();
        degraded.degraded = true;
        let line = status_bar(&degraded, 200, false);
        assert!(line.contains("degraded"), "got {line}");
        assert!(!line.contains("supervised"), "got {line}");
    }

    #[test]
    fn unknown_usage_or_unread_mail_renders_as_a_placeholder_not_a_zero() {
        let mut unknown = bar_state();
        unknown.usage_percent = None;
        unknown.unread_mail = None;
        unknown.score = None;
        unknown.verdict = None;
        let line = status_bar(&unknown, 200, false);

        assert!(!line.contains('0'), "no invented zero anywhere: {line}");
        assert_eq!(
            line.matches('\u{2013}').count(),
            3,
            "one placeholder each for score/verdict, usage and mail: {line}"
        );
    }

    /// N7: a message addressed to this session specifically must not
    /// disappear into an undifferentiated total.
    #[test]
    fn the_status_bar_shows_session_addressed_mail_separately_from_broadcast() {
        let mut both = bar_state();
        both.unread_mail = Some((2, 1));
        let line = status_bar(&both, 200, false);
        assert!(line.contains("mail 2+1"), "got {line}");

        let mut broadcast_only = bar_state();
        broadcast_only.unread_mail = Some((2, 0));
        let line = status_bar(&broadcast_only, 200, false);
        assert!(
            line.contains("mail 2") && !line.contains('+'),
            "no direct mail means the plain count, unchanged from before N7: {line}"
        );

        let mut direct_only = bar_state();
        direct_only.unread_mail = Some((0, 3));
        let line = status_bar(&direct_only, 200, false);
        assert!(line.contains("mail 0+3"), "got {line}");
    }

    #[test]
    fn the_child_pty_is_one_row_shorter_than_the_terminal_when_the_bar_is_on() {
        assert_eq!(reserved_pty_size((100, 30), true), (100, 29));
    }

    #[test]
    fn the_child_pty_is_full_height_when_the_bar_is_off() {
        assert_eq!(reserved_pty_size((100, 30), false), (100, 30));
    }

    #[test]
    fn a_pathologically_short_terminal_never_reserves_down_to_zero_rows() {
        assert_eq!(reserved_pty_size((100, 1), true), (100, 1));
        assert_eq!(reserved_pty_size((100, 0), true), (100, 1));
    }

    /// The pure sizing math a resize recomputes: the reserved row and the
    /// scroll region's own bottom line move together, one row short of the
    /// terminal, whatever the terminal's actual size is.
    #[test]
    fn the_reserved_row_and_the_scroll_region_bottom_move_together_on_any_size() {
        assert_eq!(reserved_pty_size((80, 24), true), (80, 23));
        assert_eq!(reserved_pty_size((120, 40), true), (120, 39));

        assert!(scroll_region_sequence(24).contains("\x1b[1;23r"));
        assert!(scroll_region_sequence(40).contains("\x1b[1;39r"));
    }

    /// B1: this is the actual resize decision `wrap`'s pump applies verbatim
    /// -- while the bar is still active and the new size still clears both
    /// floors, it keeps the reserved row and rewrites the scroll region.
    #[test]
    fn a_resize_while_the_bar_is_active_recomputes_the_reserved_row_and_the_scroll_region() {
        let decision = resize_decision(true, (80, 24));
        assert_eq!(decision.pty_size, (80, 23), "still reserving one row");
        assert!(decision.set_scroll_region, "the region moves with it");
        assert!(!decision.disables_bar);
    }

    /// B1 (blocking): shrinking below either floor must disable the bar
    /// *and* hand the child the full new size immediately -- not the last
    /// reserved size, which is the bug the review's `frozen` guard produced
    /// (the pty stayed pinned forever, even once the terminal widened back
    /// out).
    #[test]
    fn a_resize_below_minimum_disables_the_bar_and_restores_full_size_forwarding() {
        let too_narrow = resize_decision(true, (MIN_COLS - 1, 30));
        assert!(too_narrow.disables_bar, "cols below the floor disables it");
        assert_eq!(
            too_narrow.pty_size,
            (MIN_COLS - 1, 30),
            "the child gets the full size right away, not a stale reserved one"
        );
        assert!(!too_narrow.set_scroll_region);

        let too_short = resize_decision(true, (80, MIN_ROWS - 1));
        assert!(
            too_short.disables_bar,
            "rows below the floor disables it too"
        );
        assert_eq!(too_short.pty_size, (80, MIN_ROWS - 1));
    }

    /// B1: once the bar is no longer active (already degraded, or never
    /// eligible in the first place), every later resize -- including
    /// widening back out past the floor -- is ordinary full-size forwarding.
    /// Nothing re-enables a degraded bar; the child simply keeps tracking
    /// the terminal from here on, exactly like a bar-less session.
    #[test]
    fn a_later_widen_after_degrade_reaches_the_child() {
        let widened = resize_decision(false, (200, 60));
        assert_eq!(widened.pty_size, (200, 60));
        assert!(!widened.set_scroll_region);
        assert!(!widened.disables_bar, "a degraded bar never re-enables");

        // And a bar-less session (never eligible) behaves identically.
        let never_eligible = resize_decision(false, (30, 5));
        assert_eq!(never_eligible.pty_size, (30, 5));
    }

    #[test]
    fn a_failed_bar_write_disables_the_chrome_for_the_rest_of_the_session() {
        assert!(
            !after_redraw_attempt(false, true),
            "a success stays enabled"
        );
        assert!(after_redraw_attempt(false, false), "a failure disables it");
        assert!(
            after_redraw_attempt(true, true),
            "once disabled, a later success does not re-enable it"
        );
        assert!(
            after_redraw_attempt(true, false),
            "once disabled, stays disabled"
        );
    }

    /// B1: the one-time recovery (clear the row, hand back full size) fires
    /// exactly once per degrade, regardless of which caller noticed the
    /// disable first (a resize below the floor, or a redraw/lock failure
    /// with no resize event at all).
    #[test]
    fn bar_recovery_fires_exactly_once_after_a_degrade() {
        assert!(
            !bar_needs_recovery(false, true, false),
            "never eligible: there is nothing to recover"
        );
        assert!(
            bar_needs_recovery(true, true, false),
            "eligible, just disabled, not yet cleaned up"
        );
        assert!(
            !bar_needs_recovery(true, true, true),
            "already recovered once: never again"
        );
        assert!(
            !bar_needs_recovery(true, false, false),
            "still active: nothing to recover"
        );
    }

    /// B2 (blocking): `CSI r` (DECSTBM) homes the cursor the instant it
    /// takes effect in every VT100-descended terminal. Every place this
    /// module writes a region set or clear has to save the cursor *before*
    /// that happens and restore it *after*, or the write yanks the cursor
    /// back to row 1 over whatever the session had already printed there
    /// (the launch banner, in particular).
    #[test]
    fn every_scroll_region_write_saves_and_restores_the_cursor() {
        for sequence in [scroll_region_sequence(24), bar_reset_sequence(24)] {
            assert!(
                sequence.starts_with("\x1b7"),
                "the cursor is saved before the region op: {sequence:?}"
            );
            assert!(
                sequence.ends_with("\x1b8"),
                "and restored after it: {sequence:?}"
            );
            let save_at = sequence.find("\x1b7").expect("save present");
            let region_at = sequence
                .find("\x1b[r")
                .or_else(|| sequence.find("\x1b[1;"))
                .expect("a region op (either the plain reset or the explicit set) is present");
            assert!(
                save_at < region_at,
                "save must precede the homing region op: {sequence:?}"
            );
        }
    }

    /// The reset sequence itself: region cleared, reserved row addressed and
    /// blanked, all inside the save/restore pair B2 requires.
    #[test]
    fn bar_reset_sequence_clears_the_region_and_the_reserved_row() {
        let reset = bar_reset_sequence(24);
        assert!(
            reset.contains("\x1b[r"),
            "the scroll region is cleared: {reset:?}"
        );
        assert!(
            reset.contains("\x1b[24;1H"),
            "moves to the reserved row: {reset:?}"
        );
        assert!(reset.contains("\x1b[2K"), "clears that row: {reset:?}");
    }

    #[test]
    fn the_redraw_buffer_is_one_assembled_write_that_saves_and_restores_the_cursor() {
        let seq = bar_redraw_sequence(24, "claude | score 42 advise");
        assert!(seq.starts_with("\x1b7"), "saves the cursor first: {seq:?}");
        assert!(seq.ends_with("\x1b8"), "restores the cursor last: {seq:?}");
        assert!(seq.contains("\x1b[24;1H"), "got {seq:?}");
        assert!(seq.contains("\x1b[2K"), "got {seq:?}");
        assert!(seq.contains("claude | score 42 advise"), "got {seq:?}");
    }

    #[test]
    fn the_bar_only_redraws_when_its_text_actually_changed() {
        assert!(bar_text_changed(None, "a"));
        assert!(!bar_text_changed(Some("a"), "a"));
        assert!(bar_text_changed(Some("a"), "b"));
    }

    /// B1: both floors, not just columns -- `chrome::MIN_ROWS` is enforced
    /// after a resize exactly like `MIN_COLS` is, matching what
    /// `ChromeCaps::probe` already enforces before launch.
    #[test]
    fn a_terminal_shrunk_below_either_floor_disables_the_bar() {
        assert!(bar_should_disable((MIN_COLS - 1, 30)));
        assert!(bar_should_disable((80, MIN_ROWS - 1)));
        assert!(!bar_should_disable((MIN_COLS, MIN_ROWS)));
    }

    // Task 6: dashboard eligibility.

    fn dash_cfg() -> DashConfig {
        DashConfig::default()
    }

    const DASH_SIZE: (u16, u16) = (100, 30);

    #[test]
    fn dash_is_eligible_with_every_axis_satisfied() {
        assert!(dash_eligible(
            true,
            true,
            true,
            DASH_SIZE,
            &dash_cfg(),
            false
        ));
    }

    #[test]
    fn dash_is_ineligible_when_either_stream_is_not_a_terminal() {
        assert!(!dash_eligible(
            false,
            true,
            true,
            DASH_SIZE,
            &dash_cfg(),
            false
        ));
        assert!(!dash_eligible(
            true,
            false,
            true,
            DASH_SIZE,
            &dash_cfg(),
            false
        ));
    }

    #[test]
    fn dash_is_ineligible_without_vt() {
        assert!(!dash_eligible(
            true,
            true,
            false,
            DASH_SIZE,
            &dash_cfg(),
            false
        ));
    }

    #[test]
    fn dash_is_ineligible_when_disabled_or_simple() {
        let mut disabled = dash_cfg();
        disabled.enabled = false;
        assert!(!dash_eligible(
            true, true, true, DASH_SIZE, &disabled, false
        ));

        assert!(!dash_eligible(
            true,
            true,
            true,
            DASH_SIZE,
            &dash_cfg(),
            true
        ));
    }

    #[test]
    fn dash_is_eligible_exactly_at_the_minimum_size() {
        assert!(dash_eligible(
            true,
            true,
            true,
            (MIN_DASH_COLS, MIN_DASH_ROWS),
            &dash_cfg(),
            false
        ));
    }

    #[test]
    fn dash_is_ineligible_one_below_the_minimum_size_in_either_dimension() {
        assert!(!dash_eligible(
            true,
            true,
            true,
            (MIN_DASH_COLS - 1, MIN_DASH_ROWS),
            &dash_cfg(),
            false
        ));
        assert!(!dash_eligible(
            true,
            true,
            true,
            (MIN_DASH_COLS, MIN_DASH_ROWS - 1),
            &dash_cfg(),
            false
        ));
    }
}
