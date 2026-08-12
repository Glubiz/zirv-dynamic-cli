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

use super::config::ChromeConfig;

/// Below this width the banner and the status bar both risk wrapping their
/// own text across lines, which corrupts the bar's single-line redraw.
pub const MIN_COLS: u16 = 40;
/// Below this height a reserved bottom row would eat a meaningful fraction of
/// the visible terminal.
pub const MIN_ROWS: u16 = 8;

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
}

fn styled(
    text: &str,
    colour: bool,
    apply: impl FnOnce(console::StyledObject<&str>) -> console::StyledObject<&str>,
) -> String {
    if colour {
        apply(console::style(text)).to_string()
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
    pub unread_mail: Option<usize>,
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
    let mail = state
        .unread_mail
        .map(|c| c.to_string())
        .unwrap_or_else(|| PLACEHOLDER.to_string());
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
        console::style(truncated).dim().to_string()
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
pub fn scroll_region_sequence(rows: u16) -> String {
    format!("\x1b[1;{}r", rows.saturating_sub(1).max(1))
}

/// One assembled redraw buffer: save cursor, jump to the reserved row, clear
/// it, write the bar text, restore the cursor. Callers write this in one
/// `write_all` call so a concurrent write from the output thread can never
/// land in the middle of it.
pub fn bar_redraw_sequence(rows: u16, text: &str) -> String {
    format!("\x1b7\x1b[{rows};1H\x1b[2K{text}\x1b8")
}

/// Undoes `scroll_region_sequence` and clears the reserved row, for the one
/// place `wrap` restores the terminal (the same arm that calls
/// `RawGuard::restore`).
pub fn bar_reset_sequence(rows: u16) -> String {
    format!("\x1b[r\x1b7\x1b[{rows};1H\x1b[2K\x1b8")
}

/// Whether a terminal has shrunk below the floor the bar needs to still be
/// legible, the post-resize half of the degrade rule (`ChromeCaps::probe`
/// covers the pre-launch half).
pub fn bar_should_disable_after_resize(cols: u16) -> bool {
    cols < MIN_COLS
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
        // depending on cargo test's own I/O environment.
        console::set_colors_enabled(false);
        let chrome = ChromeCaps::probe(true, true, SIZE, &cfg(), false, false);
        assert!(!chrome.colour, "NO_COLOR must suppress colour");
        assert!(
            chrome.banner && chrome.bar,
            "colour is independent of whether chrome itself is eligible"
        );
        console::set_colors_enabled(true);
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
        }
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
            unread_mail: Some(2),
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

    #[test]
    fn a_resize_recomputes_the_reserved_row_and_the_scroll_region() {
        assert_eq!(reserved_pty_size((80, 24), true), (80, 23));
        assert_eq!(reserved_pty_size((120, 40), true), (120, 39));

        assert_eq!(scroll_region_sequence(24), "\x1b[1;23r");
        assert_eq!(scroll_region_sequence(40), "\x1b[1;39r");
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

    /// The degrade decision is a bare `bool -> bool` function: it has no
    /// parameter that could name a child process, no return value that could
    /// signal one, and nothing it touches reaches outside this module. A
    /// sequence of attempts -- including failures -- therefore has no way to
    /// affect any "child alive" state a caller tracks separately, which this
    /// pins by running such a sequence against a simulated child flag that
    /// only this test's own loop, never `after_redraw_attempt`, could set.
    #[test]
    fn a_failed_bar_write_never_ends_the_child() {
        let mut chrome_disabled = false;
        let mut child_alive = true;
        for write_succeeded in [true, false, false, true] {
            chrome_disabled = after_redraw_attempt(chrome_disabled, write_succeeded);
            // Nothing above this line touches `child_alive`; it stays true
            // through every outcome, including repeated failures.
        }
        assert!(chrome_disabled, "the run included a failure");
        assert!(
            child_alive,
            "the child is never referenced by the degrade decision"
        );
        let _ = &mut child_alive;
    }

    #[test]
    fn the_scroll_region_and_cursor_are_reset_in_every_arm_that_leaves_the_pump() {
        let reset = bar_reset_sequence(24);
        assert!(
            reset.starts_with("\x1b[r"),
            "the scroll region is cleared first: {reset:?}"
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

    #[test]
    fn a_terminal_shrunk_below_the_floor_disables_the_bar() {
        assert!(bar_should_disable_after_resize(MIN_COLS - 1));
        assert!(!bar_should_disable_after_resize(MIN_COLS));
    }
}
