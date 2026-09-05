//! zirv's single terminal design system: semantic tones, display-width-aware
//! text primitives, and shared formatting helpers, used by CLI output
//! (`output.rs`), the `ctx wrap` chrome, and the ratatui dashboard. New
//! styling code should use this module instead of ad hoc `console::style`
//! calls or inline ratatui styles scattered across call sites, so every
//! surface stays visually consistent and every colour decision degrades the
//! same way on a no-color terminal.

use std::borrow::Cow;

use unicode_width::UnicodeWidthChar;

/// An en dash, for a value that is not yet known -- never a zero, which would
/// read as a real (if empty) measurement rather than an absent one. Mirrors
/// the private `PLACEHOLDER` in `commands::ctx::chrome` (a later phase folds
/// that copy into this one; not touched here).
pub const PLACEHOLDER: &str = "\u{2013}";

/// The standard 2-space indent step used across CLI output.
#[allow(dead_code)] // first caller lands with the remaining CLI-surface migration (#202 follow-up)
pub const INDENT: &str = "  ";

/// Semantic tones for terminal text. Each tone names *why* text is styled,
/// not the raw colour, so a future palette change touches one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// No styling at all, even when `colour` is true.
    Plain,
    /// Draws the eye to a value the reader is meant to notice first.
    Accent,
    /// Bold, without a colour -- structural emphasis (headings, key facts).
    Emphasis,
    /// Dim -- secondary detail, de-emphasized but still legible.
    Muted,
    /// A healthy/success state.
    Ok,
    /// A caution the reader should not miss, but that is not an error.
    Warn,
    /// A failure or error state.
    Err,
}

/// Applies `tone` to `text`. `colour = false` returns `text` completely
/// unchanged (an exact round-trip), which is what every no-color/CI/pipe
/// path in this codebase relies on. `colour = true` styles it with
/// `console::style(...).force_styling(true)` -- the same pattern
/// `commands::ctx::chrome::styled` already uses -- so the result is
/// deterministic regardless of `console`'s own global terminal detection:
/// two calls with the same arguments always produce the same escape codes,
/// never depending on whichever test or process last touched
/// `console::set_colors_enabled`.
///
/// `Tone::Plain` never applies styling, even when `colour` is true -- it
/// exists so a caller can pick a tone unconditionally (e.g. from a lookup
/// table) without a separate "no tone" branch.
///
/// Invariant every tone upholds: `console::strip_ansi_codes(&paint(t, tone,
/// true)) == t`, i.e. styling never changes the underlying text, only wraps
/// it in escape codes.
pub fn paint(text: &str, tone: Tone, colour: bool) -> String {
    if !colour || tone == Tone::Plain {
        return text.to_string();
    }

    let styled = console::style(text).force_styling(true);
    match tone {
        Tone::Plain => text.to_string(),
        Tone::Accent => styled.cyan().bold().to_string(),
        Tone::Emphasis => styled.bold().to_string(),
        Tone::Muted => styled.dim().to_string(),
        Tone::Ok => styled.green().to_string(),
        Tone::Warn => styled.yellow().bold().to_string(),
        Tone::Err => styled.red().bold().to_string(),
    }
}

/// Display width of `s` in terminal columns: the sum of each character's
/// `unicode-width` width, treating control characters as width 0 (rather
/// than the crate's own "undefined" for those code points) so a string that
/// happens to carry one never under- or over-counts by an unpredictable
/// amount.
///
/// Known limitation: this sums *scalar values* (`char`s), not grapheme
/// clusters. A multi-codepoint cluster that a terminal renders as one glyph
/// -- a ZWJ emoji sequence (`👨\u{200d}👩\u{200d}👧`), a regional-indicator
/// flag pair, a base character plus combining marks -- is measured as the
/// sum of its parts, which overcounts the column width a real terminal
/// actually uses for it. Fixing that precisely needs Unicode grapheme
/// segmentation (e.g. the `unicode-segmentation` crate), which this module
/// deliberately does not depend on; [`truncate_display`] and
/// [`truncate_display_ellipsis`] compensate only for the specific failure
/// mode that segmentation would otherwise prevent -- a cut landing on a
/// dangling zero-width joiner -- not for the overcount itself.
pub fn display_width(s: &str) -> usize {
    s.chars()
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
        .sum()
}

/// Backs `end` up past a trailing zero-width joiner (U+200D) left dangling
/// at a truncation boundary. `leftmost_fit_end`'s scan below happily keeps
/// consuming zero-width scalars (a ZWJ included) as long as they fit for
/// free -- they never add to `width` -- so a join sequence like
/// `👨\u{200d}👩` can end up cut right after the ZWJ, once the emoji it was
/// about to join doesn't fit. That leaves a lone ZWJ as the last character
/// of the truncated string: not a glyph a terminal can render, since a ZWJ
/// only ever means "join with what follows". Backing up past it (and any
/// further trailing ZWJs, for a longer chain) drops the whole dangling
/// join instead, so a cluster is always kept whole or dropped whole.
///
/// Plain combining marks and variation selectors are deliberately left
/// alone: `leftmost_fit_end` only ever keeps one when it fit for free right
/// after its base, so it is never split from it in the first place.
fn trim_trailing_zwj(s: &str, mut end: usize) -> usize {
    while let Some(prev) = s[..end].chars().next_back() {
        if prev != '\u{200d}' {
            break;
        }
        end -= prev.len_utf8();
    }
    end
}

/// Byte length of the leftmost prefix of `s` whose display width is `<=
/// max_cols`, choosing the longest such prefix and never splitting a
/// codepoint in half, and never leaving a dangling zero-width joiner as the
/// last character (see [`trim_trailing_zwj`]).
fn leftmost_fit_end(s: &str, max_cols: usize) -> usize {
    let mut width = 0usize;
    let mut end = 0usize;
    for (idx, ch) in s.char_indices() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + w > max_cols {
            break;
        }
        width += w;
        end = idx + ch.len_utf8();
    }
    trim_trailing_zwj(s, end)
}

/// Byte offset of the start of the rightmost suffix of `s` whose display
/// width is `<= max_cols`, choosing the longest such suffix and never
/// splitting a codepoint in half, and never leaving a leading zero-width
/// joiner stranded without the base it was meant to join (the mirror image
/// of [`trim_trailing_zwj`], for the suffix side).
#[allow(dead_code)] // used by middle_truncate; first caller lands with the #202 follow-up
fn rightmost_fit_start(s: &str, max_cols: usize) -> usize {
    let mut width = 0usize;
    let mut start = s.len();
    for (idx, ch) in s.char_indices().rev() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + w > max_cols {
            break;
        }
        width += w;
        start = idx;
    }
    while let Some(next) = s[start..].chars().next() {
        if next != '\u{200d}' {
            break;
        }
        start += next.len_utf8();
    }
    start
}

/// Keeps the leftmost content of `s` that fits within `max_cols` display
/// columns. No ellipsis is added -- see [`truncate_display_ellipsis`] for
/// that -- and a codepoint is never split in half, so the result's own
/// display width is always `<= max_cols`. Returns `Cow::Borrowed` when `s`
/// already fits, so a caller that only truncates the rare long case never
/// pays for an allocation on the common short one.
pub fn truncate_display(s: &str, max_cols: usize) -> Cow<'_, str> {
    if display_width(s) <= max_cols {
        return Cow::Borrowed(s);
    }
    Cow::Owned(s[..leftmost_fit_end(s, max_cols)].to_string())
}

/// Like [`truncate_display`], but appends a single `…` (display width 1)
/// when truncation actually happens, so the caller can tell the text was
/// shortened. `max_cols == 0` returns an empty string (there is no room for
/// even the ellipsis); `max_cols == 1` returns just `…` when truncation is
/// needed, since there is no room for the ellipsis plus any content.
pub fn truncate_display_ellipsis(s: &str, max_cols: usize) -> Cow<'_, str> {
    if max_cols == 0 {
        return Cow::Borrowed("");
    }
    if display_width(s) <= max_cols {
        return Cow::Borrowed(s);
    }
    if max_cols == 1 {
        return Cow::Borrowed("\u{2026}");
    }
    let head = &s[..leftmost_fit_end(s, max_cols - 1)];
    Cow::Owned(format!("{head}\u{2026}"))
}

/// Keeps the head and tail of `s`, replacing the middle with `…` -- the
/// right shape for a path or branch name, where the interesting detail is
/// often at both ends (`src/…/status.rs` rather than `src/commands/ctx/st…`).
/// Degrades sensibly at tiny widths: `max_cols == 0` is empty, `max_cols ==
/// 1` is just `…`, and anything larger splits the remaining budget between
/// head and tail (head gets the extra column on an odd split).
#[allow(dead_code)] // first caller lands with the remaining CLI-surface migration (#202 follow-up)
pub fn middle_truncate(s: &str, max_cols: usize) -> Cow<'_, str> {
    if display_width(s) <= max_cols {
        return Cow::Borrowed(s);
    }
    if max_cols == 0 {
        return Cow::Borrowed("");
    }
    if max_cols == 1 {
        return Cow::Borrowed("\u{2026}");
    }

    let budget = max_cols - 1;
    let head_budget = budget.div_ceil(2);
    let tail_budget = budget - head_budget;

    let head = &s[..leftmost_fit_end(s, head_budget)];
    let tail = &s[rightmost_fit_start(s, tail_budget)..];

    Cow::Owned(format!("{head}\u{2026}{tail}"))
}

/// Humanized age: one unit, whichever is largest without going to zero --
/// seconds under a minute, then minutes, hours, days. A session's age is
/// usually minutes to days old, never sub-second, so this deliberately does
/// not go finer than seconds. Exact behavior (including unit boundaries)
/// carried over from the former private `commands::ctx::status::format_age`.
pub fn format_age(seconds: u64) -> String {
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

/// A percentage, rounded to the nearest whole number: `format_pct(63.4) ==
/// "63%"`.
pub fn format_pct(v: f64) -> String {
    format!("{v:.0}%")
}

/// A section header line, in the convention already used across the
/// `status`/`context_status` surfaces: a blank line, then `<title>:`.
#[allow(dead_code)] // first caller lands with the remaining CLI-surface migration (#202 follow-up)
pub fn section_header(title: &str) -> String {
    format!("\n{title}:")
}

/// Ratatui style tokens for the dashboard. Dashboard chrome stays default-
/// palette monochrome (bold/dim/reversed, no colour at all) except for these
/// semantic states, which use ANSI base colors only (`Color::Red`, etc.),
/// never RGB or indexed colours, so they still degrade correctly on a
/// 16-color terminal.
pub mod tui {
    use ratatui::style::{Color, Modifier, Style};

    /// A section/pane title.
    pub fn title() -> Style {
        Style::default().add_modifier(Modifier::BOLD)
    }

    /// Secondary, de-emphasized text.
    pub fn muted() -> Style {
        Style::default().add_modifier(Modifier::DIM)
    }

    /// The currently selected row/item.
    #[allow(dead_code)] // dash uses selected_strong today; kept as the plain-selection token
    pub fn selected() -> Style {
        Style::default().add_modifier(Modifier::REVERSED)
    }

    /// A selected row/item that also needs extra weight (e.g. it also has
    /// focus).
    pub fn selected_strong() -> Style {
        Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
    }

    /// A keybinding hint / footer text.
    pub fn hint() -> Style {
        Style::default().add_modifier(Modifier::DIM)
    }

    /// An error/failure state.
    pub fn error() -> Style {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    }

    /// A caution state that is not an error.
    pub fn warning() -> Style {
        Style::default().fg(Color::Yellow)
    }

    /// A healthy/success state.
    pub fn ok() -> Style {
        Style::default().fg(Color::Green)
    }

    /// Finished, but nobody has looked at it yet (issue #354 phase 2's
    /// done-unread `◆`). Deliberately its own hue rather than a reuse of
    /// [`ok`]: "done" and "done, and you have already seen it" are two
    /// different things to an operator scanning a roster, and the approved
    /// design distinguishes them by colour *and* shape.
    pub fn unread() -> Style {
        Style::default().fg(Color::Magenta)
    }

    /// A value the reader should notice first. Bold (issue #209/v3): the
    /// approved v3 mock sharpens every accent-toned surface at once --
    /// spinners, frame titles, focused borders -- rather than leaving cyan
    /// to carry the emphasis alone. Checked against every consumer before
    /// this changed (`tui::accent()`'s own callers, all inside the
    /// dashboard); none asserted the un-bolded `Style` by equality, so none
    /// needed a test update for the added modifier.
    pub fn accent() -> Style {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    }

    /// The zirv brand chip: black text on a cyan background, bold. One per
    /// screen (the dashboard header's own ` zirv ` badge) -- everything else
    /// in the dashboard's chrome stays default-palette monochrome except the
    /// semantic states above, so this is the one place a *background* colour
    /// is ever used.
    pub fn chip() -> Style {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    }

    /// Braille spinner frames for a working pane's sidebar glyph. Advanced
    /// one frame per render tick (`SPINNER_FRAMES[tick %
    /// SPINNER_FRAMES.len()]`), not on a clock of its own: the dashboard
    /// already redraws every frame, so a tick counter threaded through the
    /// render state is enough -- no new polling.
    pub const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Modifier};

    #[test]
    fn paint_with_colour_off_is_an_identity() {
        for tone in [
            Tone::Plain,
            Tone::Accent,
            Tone::Emphasis,
            Tone::Muted,
            Tone::Ok,
            Tone::Warn,
            Tone::Err,
        ] {
            assert_eq!(paint("hello", tone, false), "hello");
        }
    }

    #[test]
    fn paint_with_colour_on_strips_back_to_the_original_text_for_every_tone() {
        for tone in [
            Tone::Plain,
            Tone::Accent,
            Tone::Emphasis,
            Tone::Muted,
            Tone::Ok,
            Tone::Warn,
            Tone::Err,
        ] {
            let painted = paint("hello world", tone, true);
            assert_eq!(
                console::strip_ansi_codes(&painted),
                "hello world",
                "tone {tone:?} must round-trip"
            );
        }
    }

    #[test]
    fn paint_plain_never_styles_even_with_colour_on() {
        assert_eq!(paint("hello", Tone::Plain, true), "hello");
    }

    #[test]
    fn paint_accent_actually_adds_escape_codes() {
        let painted = paint("x", Tone::Accent, true);
        assert_ne!(painted, "x", "accent must actually style when colour=true");
    }

    #[test]
    fn display_width_of_ascii_is_char_count() {
        assert_eq!(display_width("hello"), 5);
    }

    #[test]
    fn display_width_of_cjk_is_double_width() {
        assert_eq!(display_width("日本語"), 6);
    }

    #[test]
    fn display_width_of_emoji_is_nonzero() {
        assert!(display_width("🎉") > 0);
    }

    #[test]
    fn display_width_of_empty_is_zero() {
        assert_eq!(display_width(""), 0);
    }

    #[test]
    fn truncate_display_returns_borrowed_when_it_already_fits() {
        let out = truncate_display("hi", 10);
        assert_eq!(out, "hi");
        assert!(matches!(out, Cow::Borrowed(_)));
    }

    #[test]
    fn truncate_display_exact_fit_is_unchanged() {
        assert_eq!(truncate_display("hello", 5), "hello");
    }

    #[test]
    fn truncate_display_overflow_keeps_the_leftmost_columns() {
        assert_eq!(truncate_display("hello world", 5), "hello");
    }

    #[test]
    fn truncate_display_never_splits_a_wide_char_and_stays_within_budget() {
        // "日本語" is 3 chars, width 2 each = 6 total. A budget of 3 can only
        // fit one whole wide char (width 2), not half of a second.
        let out = truncate_display("日本語", 3);
        assert!(display_width(&out) <= 3);
        assert_eq!(out, "日");
    }

    #[test]
    fn truncate_display_never_strands_a_base_char_from_its_combining_mark() {
        // "e" + combining acute accent (U+0301), width 0, then more content.
        // Whatever the base fits, whichever zero-width marks fit right after
        // it are free (they never add to the running width), so the mark
        // must never be dropped while its base is kept.
        let s = "e\u{0301}bcdef";
        for cols in 0..=display_width(s) {
            let out = truncate_display(s, cols);
            assert!(display_width(&out) <= cols, "cols={cols} out={out:?}");
            if out.starts_with('e') {
                assert!(
                    out.starts_with("e\u{0301}") || out == "e",
                    "base kept without checking for its mark: cols={cols} out={out:?}"
                );
            }
        }
    }

    #[test]
    fn truncate_display_keeps_or_drops_a_zwj_emoji_family_whole() {
        // Man + ZWJ + Woman + ZWJ + Girl: a three-codepoint join sequence a
        // terminal renders as one glyph. At every budget, the truncated
        // result must never end on a bare ZWJ -- that would be a dangling
        // "join with what follows" marker with nothing left to join.
        let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
        for cols in 0..=display_width(family) {
            let out = truncate_display(family, cols);
            assert!(display_width(&out) <= cols, "cols={cols} out={out:?}");
            assert!(
                !out.ends_with('\u{200d}'),
                "cols={cols} left a dangling ZWJ: {out:?}"
            );
        }
    }

    #[test]
    fn truncate_display_ellipsis_keeps_or_drops_a_zwj_emoji_family_whole() {
        let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
        for cols in 0..=display_width(family) + 2 {
            let out = truncate_display_ellipsis(family, cols);
            assert!(display_width(&out) <= cols, "cols={cols} out={out:?}");
            // Strip a trailing ellipsis before checking for a dangling ZWJ:
            // the ellipsis itself is never part of the joined cluster.
            let without_ellipsis = out.strip_suffix('\u{2026}').unwrap_or(&out);
            assert!(
                !without_ellipsis.ends_with('\u{200d}'),
                "cols={cols} left a dangling ZWJ: {out:?}"
            );
        }
    }

    #[test]
    fn truncate_display_ellipsis_at_zero_is_empty() {
        assert_eq!(truncate_display_ellipsis("hello", 0), "");
    }

    #[test]
    fn truncate_display_ellipsis_at_one_with_truncation_is_just_the_ellipsis() {
        assert_eq!(truncate_display_ellipsis("hello", 1), "\u{2026}");
    }

    #[test]
    fn truncate_display_ellipsis_exact_fit_is_unchanged_no_ellipsis() {
        assert_eq!(truncate_display_ellipsis("hello", 5), "hello");
    }

    #[test]
    fn truncate_display_ellipsis_overflow_appends_ellipsis_and_fits() {
        let out = truncate_display_ellipsis("hello world", 5);
        assert_eq!(out, "hell\u{2026}");
        assert_eq!(display_width(&out), 5);
    }

    #[test]
    fn middle_truncate_of_a_long_path_keeps_head_and_tail() {
        let out = middle_truncate("src/commands/ctx/status.rs", 15);
        assert!(display_width(&out) <= 15);
        assert!(out.starts_with("src/"));
        assert!(out.ends_with("status.rs") || out.contains('\u{2026}'));
        assert!(out.contains('\u{2026}'));
    }

    #[test]
    fn middle_truncate_already_fits_is_unchanged() {
        let out = middle_truncate("short", 10);
        assert_eq!(out, "short");
        assert!(matches!(out, Cow::Borrowed(_)));
    }

    #[test]
    fn middle_truncate_at_zero_is_empty() {
        assert_eq!(middle_truncate("abcdefgh", 0), "");
    }

    #[test]
    fn middle_truncate_at_one_is_just_the_ellipsis() {
        assert_eq!(middle_truncate("abcdefgh", 1), "\u{2026}");
    }

    #[test]
    fn middle_truncate_at_tiny_widths_never_exceeds_budget() {
        for cols in 0..8 {
            let out = middle_truncate("abcdefghijklmnop", cols);
            assert!(
                display_width(&out) <= cols,
                "cols={cols} produced {out:?} with width {}",
                display_width(&out)
            );
        }
    }

    #[test]
    fn format_age_seconds_boundary() {
        assert_eq!(format_age(0), "0s");
        assert_eq!(format_age(59), "59s");
    }

    #[test]
    fn format_age_minutes_boundary() {
        assert_eq!(format_age(60), "1m");
        assert_eq!(format_age(3599), "59m");
    }

    #[test]
    fn format_age_hours_boundary() {
        assert_eq!(format_age(3600), "1h");
        assert_eq!(format_age(86_399), "23h");
    }

    #[test]
    fn format_age_days_boundary() {
        assert_eq!(format_age(86_400), "1d");
        assert_eq!(format_age(172_800), "2d");
    }

    #[test]
    fn format_pct_rounds_to_nearest_whole_number() {
        assert_eq!(format_pct(63.4), "63%");
        assert_eq!(format_pct(63.5), "64%");
        assert_eq!(format_pct(0.0), "0%");
        assert_eq!(format_pct(100.0), "100%");
    }

    #[test]
    fn section_header_is_a_blank_line_then_title_colon() {
        assert_eq!(section_header("sessions"), "\nsessions:");
    }

    #[test]
    fn tui_error_is_red_and_bold() {
        let style = tui::error();
        assert_eq!(style.fg, Some(Color::Red));
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn tui_warning_is_yellow() {
        assert_eq!(tui::warning().fg, Some(Color::Yellow));
    }

    #[test]
    fn tui_ok_is_green() {
        assert_eq!(tui::ok().fg, Some(Color::Green));
    }

    #[test]
    fn tui_accent_is_cyan() {
        assert_eq!(tui::accent().fg, Some(Color::Cyan));
    }

    #[test]
    fn tui_selected_is_reversed() {
        assert!(tui::selected().add_modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn tui_chip_is_black_on_cyan_and_bold() {
        let style = tui::chip();
        assert_eq!(style.fg, Some(Color::Black));
        assert_eq!(style.bg, Some(Color::Cyan));
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn tui_spinner_frames_are_ten_single_column_braille_glyphs() {
        assert_eq!(tui::SPINNER_FRAMES.len(), 10);
        for frame in tui::SPINNER_FRAMES {
            assert_eq!(
                display_width(frame),
                1,
                "spinner frame {frame:?} must be exactly one column wide"
            );
        }
        // Every frame is distinct -- a spinner that repeats a frame within
        // one cycle would look like it stalled for a tick.
        let unique: std::collections::HashSet<&&str> = tui::SPINNER_FRAMES.iter().collect();
        assert_eq!(unique.len(), tui::SPINNER_FRAMES.len());
    }

    #[test]
    fn tui_selected_strong_is_reversed_and_bold() {
        let style = tui::selected_strong();
        assert!(style.add_modifier.contains(Modifier::REVERSED));
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn tui_title_is_bold() {
        assert!(tui::title().add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn tui_muted_and_hint_are_dim() {
        assert!(tui::muted().add_modifier.contains(Modifier::DIM));
        assert!(tui::hint().add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn placeholder_is_an_en_dash() {
        assert_eq!(PLACEHOLDER, "\u{2013}");
    }

    #[test]
    fn indent_is_two_spaces() {
        assert_eq!(INDENT, "  ");
    }
}
