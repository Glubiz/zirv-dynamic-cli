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
use crate::style::{self, Tone};

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
    /// The old, full-sentence form: used only by the legacy (no-VT) banner
    /// tier, which never changed shape from the pre-#202 banner.
    pub fn describe(&self) -> &'static str {
        match self {
            HarnessRule::Explicit => "requested with --agent",
            HarnessRule::Configured => "configured as the default agent",
            HarnessRule::FirstEnabledReady => "the first enabled, ready harness",
        }
    }

    /// A single word, for the box/compact banner tiers, which have no room
    /// for `describe`'s full sentence.
    fn short_word(&self) -> &'static str {
        match self {
            HarnessRule::Explicit => "explicit",
            HarnessRule::Configured => "configured",
            HarnessRule::FirstEnabledReady => "auto",
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

/// One coloured piece of a banner content line or a status-bar assembly.
/// `Tone` covers everything `style::paint` already knows how to do;
/// `CyanPlain`/`CyanDim`/`Chip` are the handful of treatments this module
/// needs that have no `Tone` of their own (the resume glyph and the compact
/// tier's rule are plain cyan, the banner box border is dimmed cyan so it
/// recedes behind its content, and the status bar's brand chip is a filled
/// background, not a foreground colour at all).
#[derive(Debug, Clone, Copy)]
enum Paint {
    Tone(Tone),
    /// Plain cyan -- no bold, no dim.
    CyanPlain,
    /// Cyan, dimmed -- the banner box border's own colour.
    CyanDim,
    /// Black text on a cyan background, bold -- the status bar's leading
    /// brand chip.
    Chip,
}

/// An ordered list of `(text, paint)` pieces -- one banner content line, or
/// one status-bar segment/assembly. Named so a `Segments` (and,
/// where two of them travel together, a tuple of two) never counts as
/// clippy's own "very complex type".
type Segments = Vec<(String, Paint)>;

/// `colour` here is already the final decision (`Chrome::colour`, or an
/// explicit test value): forcing styling on rather than letting
/// `StyledObject`'s own `Display` impl re-check the process-global
/// `console::colors_enabled()` flag is what keeps this deterministic. Without
/// `force_styling`, a caller that computed `colour = true` still rendered
/// plain text whenever the global flag happened to be off (a no-color CI
/// environment, or -- in tests -- a leftover from whichever test last
/// touched `console::set_colors_enabled` and ran first). `Tone` delegates to
/// `style::paint`, which already has this exact rule built in.
fn paint_seg(text: &str, paint: Paint, colour: bool) -> String {
    if let Paint::Tone(tone) = paint {
        return style::paint(text, tone, colour);
    }
    if !colour {
        return text.to_string();
    }
    match paint {
        Paint::CyanPlain => console::style(text).force_styling(true).cyan().to_string(),
        Paint::CyanDim => console::style(text)
            .force_styling(true)
            .cyan()
            .dim()
            .to_string(),
        Paint::Chip => console::style(text)
            .force_styling(true)
            .black()
            .on_cyan()
            .bold()
            .to_string(),
        Paint::Tone(_) => unreachable!("handled above"),
    }
}

/// Colours one banner content line or one status-bar assembly, built from an
/// ordered list of `(text, paint)` pairs. Every width decision -- whether it
/// fits, where to cut it if not -- is made from the pairs' concatenated
/// *plain* text, never from an already-styled/ANSI-bearing string: escape
/// codes inserted by colouring an earlier piece must never skew where a
/// wide/CJK/emoji character later on gets cut, and a cut mid-escape-sequence
/// would corrupt the terminal's own state.
///
/// `pad` right-pads with plain spaces to exactly `avail` columns -- the
/// banner box needs this so its right border always lines up; the status bar
/// never pads. `ellipsis` appends a single `…` when truncation actually
/// happens (the banner's contract); the status bar's own last-resort hard
/// truncate passes `false` so its result stays an exact prefix of the
/// untruncated text with nothing appended, matching `style::truncate_display`
/// rather than `style::truncate_display_ellipsis`.
fn render_line(
    segments: &[(String, Paint)],
    avail: usize,
    colour: bool,
    pad: bool,
    ellipsis: bool,
) -> String {
    let plain: String = segments.iter().map(|(t, _)| t.as_str()).collect();
    let plain_width = style::display_width(&plain);

    if plain_width <= avail {
        let mut out = String::new();
        for (text, paint) in segments {
            out.push_str(&paint_seg(text, *paint, colour));
        }
        if pad && plain_width < avail {
            out.push_str(&" ".repeat(avail - plain_width));
        }
        return out;
    }

    let ellipsis_cost = if ellipsis && avail > 0 { 1 } else { 0 };
    let content_budget = avail.saturating_sub(ellipsis_cost);
    let mut out = String::new();
    let mut remaining = content_budget;
    let mut kept_width = 0usize;
    for (text, paint) in segments {
        if remaining == 0 {
            break;
        }
        let w = style::display_width(text);
        if w <= remaining {
            out.push_str(&paint_seg(text, *paint, colour));
            remaining -= w;
            kept_width += w;
        } else {
            let piece = style::truncate_display(text, remaining);
            kept_width += style::display_width(&piece);
            out.push_str(&paint_seg(&piece, *paint, colour));
            remaining = 0;
        }
    }
    if ellipsis && avail > 0 {
        out.push('\u{2026}');
        kept_width += 1;
    }
    if pad && kept_width < avail {
        out.push_str(&" ".repeat(avail - kept_width));
    }
    out
}

/// Below this width the banner box's own frame (the title alone costs 9
/// columns: `╭─ zirv ╮`) stops being worth drawing; narrower terminals get
/// the compact, borderless tier instead.
const BANNER_BOX_MIN_COLS: u16 = 56;

/// A hard floor under the box's own width, independent of content: shorter
/// than this and the title `╭─ zirv ╮` itself would not fit.
const BANNER_BOX_MIN_WIDTH: usize = 9;

/// Pure text builder for the launch banner. Degrades in one direction only:
///
/// * `vt == false` -- a terminal that cannot do VT processing at all -- gets
///   the legacy, pre-#202 plain-line form regardless of `colour` or `cols`;
///   colour would render as raw escape bytes without VT, so it is never
///   attempted.
/// * `vt == true` and `cols` is at least [`BANNER_BOX_MIN_COLS`] gets the
///   rounded box.
/// * `vt == true` and `cols` is narrower than that (or unknown -- `None`,
///   meaning the terminal size probe itself failed) gets the compact,
///   borderless tier: a real width is required to compute a box that always
///   joins its own corners, so an unknown width is treated the same as a
///   too-narrow one rather than guessed at.
/// * `colour == false` at any tier renders the same text with zero ANSI --
///   every tier's own rendering already goes through `paint_seg`/
///   `render_line`, which round-trip exactly under `colour == false`.
pub fn banner(facts: &BannerFacts, colour: bool, vt: bool, cols: Option<u16>) -> String {
    if !vt {
        return banner_legacy(facts);
    }
    match cols {
        Some(c) if c >= BANNER_BOX_MIN_COLS => banner_box(facts, colour, c),
        _ => banner_compact(facts, colour, cols),
    }
}

/// The pre-#202 banner text, unstyled: plain lines, no box, no glyphs. This
/// is what every terminal that cannot do VT processing still gets -- always
/// legible, always a valid fallback -- and it is deliberately left bit-for-
/// bit identical to the original `banner` so no behavior changes for that
/// tier.
fn banner_legacy(facts: &BannerFacts) -> String {
    let mut lines = Vec::new();

    lines.push(format!(
        "zirv chat {} ({})",
        facts.harness,
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

/// The banner's per-harness roster segment, shared by the box and compact
/// tiers: `{name} ●` for an enabled harness, `{name} ○ disabled` for a
/// disabled one, one space between entries.
fn harness_roster_segments(harnesses: &[(String, bool)]) -> Segments {
    let mut segments = Vec::new();
    for (index, (name, enabled)) in harnesses.iter().enumerate() {
        if index > 0 {
            segments.push((" ".to_string(), Paint::Tone(Tone::Plain)));
        }
        segments.push((name.clone(), Paint::Tone(Tone::Plain)));
        segments.push((" ".to_string(), Paint::Tone(Tone::Plain)));
        if *enabled {
            segments.push(("●".to_string(), Paint::Tone(Tone::Ok)));
        } else {
            segments.push(("○".to_string(), Paint::Tone(Tone::Muted)));
            segments.push((" disabled".to_string(), Paint::Tone(Tone::Muted)));
        }
    }
    segments
}

/// The rounded-box banner tier: the same frame grammar Claude Code's own
/// welcome box uses. Every content line is truncated (with an ellipsis) to
/// fit inside the box, and the box's own width is computed from the widest
/// line's *display* width so the border always joins its own corners, even
/// with a CJK or emoji harness/session name.
fn banner_box(facts: &BannerFacts, colour: bool, cols: u16) -> String {
    let mut line1: Segments =
        vec![(facts.harness.clone(), Paint::Tone(Tone::Emphasis))];
    if let Some(model) = &facts.model {
        line1.push((" · ".to_string(), Paint::Tone(Tone::Muted)));
        line1.push((format!("model {model}"), Paint::Tone(Tone::Muted)));
    }
    line1.push((" · ".to_string(), Paint::Tone(Tone::Muted)));
    line1.push((
        format!("rule {}", facts.rule.short_word()),
        Paint::Tone(Tone::Muted),
    ));

    let mut line2: Segments = vec![
        ("session ".to_string(), Paint::Tone(Tone::Muted)),
        (facts.session.clone(), Paint::Tone(Tone::Muted)),
        (" · ".to_string(), Paint::Tone(Tone::Muted)),
        ("harnesses ".to_string(), Paint::Tone(Tone::Muted)),
    ];
    line2.extend(harness_roster_segments(&facts.harnesses));

    let line3 = facts.resuming.as_ref().map(|resuming| {
        vec![
            ("↺".to_string(), Paint::CyanPlain),
            (" resuming · ".to_string(), Paint::Tone(Tone::Muted)),
            (resuming.clone(), Paint::Tone(Tone::Muted)),
        ]
    });

    let mut content_lines: Vec<Segments> = vec![line1, line2];
    if let Some(line3) = line3 {
        content_lines.push(line3);
    }

    let plain_width_of = |segs: &[(String, Paint)]| -> usize {
        style::display_width(&segs.iter().map(|(t, _)| t.as_str()).collect::<String>())
    };
    let widest = content_lines
        .iter()
        .map(|segs| plain_width_of(segs))
        .max()
        .unwrap_or(0);
    let box_width = (cols as usize).min(widest + 4).max(BANNER_BOX_MIN_WIDTH);
    let avail = box_width.saturating_sub(4);

    let mut out = String::new();
    out.push_str(&banner_box_top(box_width, colour));
    for segs in &content_lines {
        out.push('\n');
        out.push_str(&paint_seg("│  ", Paint::CyanDim, colour));
        out.push_str(&render_line(segs, avail, colour, true, true));
        out.push_str(&paint_seg("│", Paint::CyanDim, colour));
    }
    out.push('\n');
    out.push_str(&banner_box_bottom(box_width, colour));
    out
}

fn banner_box_top(box_width: usize, colour: bool) -> String {
    let inner = box_width.saturating_sub(BANNER_BOX_MIN_WIDTH);
    format!(
        "{}{}{}",
        paint_seg("╭─ ", Paint::CyanDim, colour),
        paint_seg("zirv", Paint::Tone(Tone::Accent), colour),
        paint_seg(&format!(" {}╮", "─".repeat(inner)), Paint::CyanDim, colour),
    )
}

fn banner_box_bottom(box_width: usize, colour: bool) -> String {
    let inner = box_width.saturating_sub(2);
    paint_seg(&format!("╰{}╯", "─".repeat(inner)), Paint::CyanDim, colour)
}

/// The narrow-terminal (or unknown-width) banner tier: two lines, no box, no
/// padding to a fixed width -- there is no border to keep joined, so this
/// only truncates against a *known* `cols`, and renders full text when the
/// width itself is unknown (`None`) rather than guessing.
fn banner_compact(facts: &BannerFacts, colour: bool, cols: Option<u16>) -> String {
    let mut line1: Segments = vec![
        ("▎ ".to_string(), Paint::CyanPlain),
        ("zirv ".to_string(), Paint::Tone(Tone::Accent)),
        (facts.harness.clone(), Paint::Tone(Tone::Emphasis)),
    ];
    if let Some(model) = &facts.model {
        line1.push((" · ".to_string(), Paint::Tone(Tone::Muted)));
        line1.push((model.clone(), Paint::Tone(Tone::Muted)));
    }
    line1.push((" · ".to_string(), Paint::Tone(Tone::Muted)));
    line1.push((
        facts.rule.short_word().to_string(),
        Paint::Tone(Tone::Muted),
    ));

    let mut line2: Segments = vec![
        ("▎ ".to_string(), Paint::CyanPlain),
        ("session ".to_string(), Paint::Tone(Tone::Muted)),
        (facts.session.clone(), Paint::Tone(Tone::Muted)),
        (" · ".to_string(), Paint::Tone(Tone::Muted)),
    ];
    line2.extend(harness_roster_segments(&facts.harnesses));

    let plain_width_of = |segs: &[(String, Paint)]| -> usize {
        style::display_width(&segs.iter().map(|(t, _)| t.as_str()).collect::<String>())
    };
    let avail1 = cols
        .map(|c| c as usize)
        .unwrap_or_else(|| plain_width_of(&line1));
    let avail2 = cols
        .map(|c| c as usize)
        .unwrap_or_else(|| plain_width_of(&line2));

    let l1 = render_line(&line1, avail1, colour, false, true);
    let l2 = render_line(&line2, avail2, colour, false, true);
    format!("{l1}\n{l2}")
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
            out.push_str(&style::paint(line, Tone::Err, colour));
        } else {
            out.push_str("  ");
            out.push_str(&style::paint(line, Tone::Muted, colour));
        }
    }
    out
}

// T12b: the reserved bottom status bar.

use super::rot::Verdict;

/// A placeholder for a value that is not yet known -- an en dash, never a
/// zero: a session with no usage reading yet must not look like it is at 0%.
///
/// `pub(crate)` so the dashboard header (`dash::ui::header_line`) can reuse
/// the exact same glyph for its own per-harness usage segment rather than
/// retyping the escape.
pub(crate) const PLACEHOLDER: &str = "\u{2013}";

/// Everything the bar draws. Every field that can be unknown is an `Option`
/// so `status_bar` can render the placeholder instead of a misleading zero.
#[derive(Debug, Clone, PartialEq)]
pub struct BarState {
    pub harness: String,
    pub score: Option<u32>,
    pub verdict: Option<Verdict>,
    /// Both subscription windows, 0.0..=100.0 each, already filtered through
    /// `window::available` by the caller so a value here is honest -- never a
    /// reading whose window has provably reset. `None` in either slot renders
    /// as no segment for that window, not a fabricated zero; both `None`
    /// renders the same placeholder the bar has always shown for "unknown".
    pub usage_five_hour: Option<f64>,
    pub usage_seven_day: Option<f64>,
    /// N7: `(broadcast, direct-to-this-session)` unread counts, split so a
    /// message addressed to this session specifically is not lost inside an
    /// undifferentiated total. `None` under the same conditions the bar's
    /// caller already treats mail as unknown (disabled, or unreadable).
    pub unread_mail: Option<(usize, usize)>,
    pub degraded: bool,
}

/// The colour band a verdict's glyph and word render in: green for the
/// healthy end, yellow for the advise/warming middle, red+bold for the
/// compact/restart end -- the same three-band shape `rot`'s own thresholds
/// already draw, just carried into colour.
fn verdict_paint(verdict: Verdict) -> Paint {
    match verdict {
        Verdict::Healthy => Paint::Tone(Tone::Ok),
        Verdict::Advise => Paint::Tone(Tone::Warn),
        Verdict::Compact | Verdict::Restart => Paint::Tone(Tone::Err),
    }
}

/// Three plain spaces: the separator between two top-level bar segments
/// (chip/harness/verdict/usage/mail/supervision). A single space separates
/// pieces *within* one segment.
const BAR_SEGMENT_GAP: &str = "   ";

/// Pure renderer: one line, never wider than `cols`, no cursor-addressing
/// escapes of its own (the caller wraps it in the redraw sequence).
///
/// Six segments, in priority order: the `zirv` brand chip, the harness name,
/// the rot verdict (glyph, word and score), the usage windows, unread mail,
/// and supervision state. When the assembled line would exceed `cols`,
/// segments drop out from *least* to *most* important -- usage, then the
/// verdict's score number (the verdict word itself stays), then the harness
/// name, then the chip -- since those are progressively more disposable than
/// knowing whether the session is rotting, has mail, or is even supervised
/// at all, which are never dropped outright. If even that irreducible core
/// still overflows, it is hard-truncated (no ellipsis, an exact prefix) as
/// the last resort, exactly like the bar's original right-truncation.
pub fn status_bar(state: &BarState, cols: u16, colour: bool) -> String {
    let cols = cols as usize;
    let gap = || (BAR_SEGMENT_GAP.to_string(), Paint::Tone(Tone::Plain));

    let chip: Segments = vec![(
        if colour {
            " zirv ".to_string()
        } else {
            "zirv".to_string()
        },
        Paint::Chip,
    )];

    let harness: Segments = vec![(state.harness.clone(), Paint::Tone(Tone::Plain))];

    let (verdict_full, verdict_reduced): (Segments, Segments) =
        match (state.verdict, state.score) {
            (Some(verdict), Some(score)) => {
                let paint = verdict_paint(verdict);
                let full = vec![
                    ("✻ ".to_string(), paint),
                    (verdict.as_str().to_string(), paint),
                    (" ".to_string(), Paint::Tone(Tone::Plain)),
                    (score.to_string(), Paint::Tone(Tone::Muted)),
                ];
                let reduced = vec![
                    ("✻ ".to_string(), paint),
                    (verdict.as_str().to_string(), paint),
                ];
                (full, reduced)
            }
            _ => {
                let unknown = vec![
                    ("✻ ".to_string(), Paint::Tone(Tone::Muted)),
                    (PLACEHOLDER.to_string(), Paint::Tone(Tone::Muted)),
                ];
                (unknown.clone(), unknown)
            }
        };

    // Both windows are honest lower bounds by the time they reach here --
    // `window::available` already dropped whichever had provably reset --
    // but the segment always shows both slots so the layout never shifts:
    // an absent half renders `PLACEHOLDER`, never a fabricated 0%.
    let usage: Segments = {
        let five = state
            .usage_five_hour
            .map(style::format_pct)
            .unwrap_or_else(|| PLACEHOLDER.to_string());
        let seven = state
            .usage_seven_day
            .map(style::format_pct)
            .unwrap_or_else(|| PLACEHOLDER.to_string());
        vec![(format!("◔ {five}·{seven}"), Paint::Tone(Tone::Muted))]
    };

    // N7: a direct count of zero renders as plain `✉ N` -- identical to the
    // bar's pre-N7 wording -- and only grows a `+direct` suffix once
    // something is actually addressed to this session specifically. `None`
    // and an explicit `(0, 0)` both mean "nothing unread", so both get the
    // same never-a-zero placeholder rather than a literal `✉ 0`.
    let mail: Segments = match state.unread_mail {
        None | Some((0, 0)) => vec![
            ("✉ ".to_string(), Paint::Tone(Tone::Muted)),
            (PLACEHOLDER.to_string(), Paint::Tone(Tone::Muted)),
        ],
        Some((broadcast, 0)) => vec![
            ("✉ ".to_string(), Paint::Tone(Tone::Warn)),
            (broadcast.to_string(), Paint::Tone(Tone::Warn)),
        ],
        Some((broadcast, direct)) => vec![
            ("✉ ".to_string(), Paint::Tone(Tone::Warn)),
            (format!("{broadcast}+{direct}"), Paint::Tone(Tone::Warn)),
        ],
    };

    let supervision: Segments = if state.degraded {
        vec![("▲ degraded".to_string(), Paint::Tone(Tone::Err))]
    } else {
        vec![
            ("● ".to_string(), Paint::Tone(Tone::Ok)),
            ("supervised".to_string(), Paint::Tone(Tone::Plain)),
        ]
    };

    let assemble = |include_chip: bool,
                    include_harness: bool,
                    include_score: bool,
                    include_usage: bool|
     -> Segments {
        let mut present: Vec<&[(String, Paint)]> = Vec::new();
        if include_chip {
            present.push(&chip);
        }
        if include_harness {
            present.push(&harness);
        }
        present.push(if include_score {
            &verdict_full
        } else {
            &verdict_reduced
        });
        if include_usage {
            present.push(&usage);
        }
        present.push(&mail);
        present.push(&supervision);

        let mut combined = Vec::new();
        for (index, segs) in present.into_iter().enumerate() {
            if index > 0 {
                combined.push(gap());
            }
            combined.extend(segs.iter().cloned());
        }
        combined
    };
    let plain_width = |segs: &[(String, Paint)]| -> usize {
        style::display_width(&segs.iter().map(|(t, _)| t.as_str()).collect::<String>())
    };

    // Drop order: usage, then the verdict's score number, then the harness
    // name, then the chip -- the verdict word, mail and supervision are
    // never dropped outright; `render_line`'s own hard-truncate is the only
    // thing that can still cut into them, and only once every droppable
    // segment is already gone.
    let mut include_chip = true;
    let mut include_harness = true;
    let mut include_score = true;
    let mut include_usage = true;
    let mut combined = assemble(include_chip, include_harness, include_score, include_usage);

    if plain_width(&combined) > cols {
        include_usage = false;
        combined = assemble(include_chip, include_harness, include_score, include_usage);
    }
    if plain_width(&combined) > cols {
        include_score = false;
        combined = assemble(include_chip, include_harness, include_score, include_usage);
    }
    if plain_width(&combined) > cols {
        include_harness = false;
        combined = assemble(include_chip, include_harness, include_score, include_usage);
    }
    if plain_width(&combined) > cols {
        include_chip = false;
        combined = assemble(include_chip, include_harness, include_score, include_usage);
    }

    render_line(&combined, cols, colour, false, false)
}

/// Keeps the leftmost `cols` display columns (not bytes, not even
/// characters: a wide/CJK/emoji character must not be split in half),
/// dropping whatever would overflow on the right. Never produces a result
/// wider than `cols` columns. A thin wrapper over `style::truncate_display`;
/// kept under its own name because `dash::ui`'s header/sidebar renderers
/// still call it under this name and are owned by another change.
pub(crate) fn right_truncate(s: &str, cols: usize) -> String {
    style::truncate_display(s, cols).into_owned()
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

    // The legacy (no-VT) tier: bit-for-bit the pre-#202 banner, so these
    // keep asserting on its full-sentence rule descriptions and `key: value`
    // line shape. `vt = false` selects this tier regardless of `cols`.

    #[test]
    fn the_banner_shows_the_model_when_configured() {
        let without = banner(&facts(), false, false, None);
        assert!(
            !without.to_lowercase().contains("model"),
            "no line at all when unset: {without}"
        );

        let mut with_model = facts();
        with_model.model = Some("fable".to_string());
        let text = banner(&with_model, false, false, None);
        assert!(text.contains("model: fable"), "got {text}");
    }

    #[test]
    fn the_banner_names_the_harness_and_the_rule_that_chose_it() {
        let text = banner(&facts(), false, false, None);
        assert!(text.contains("claude"), "got {text}");
        assert!(
            text.contains("configured as the default agent"),
            "got {text}"
        );
    }

    #[test]
    fn the_banner_lists_every_enabled_harness_and_marks_the_disabled_ones() {
        let text = banner(&facts(), false, false, None);
        assert!(text.contains("claude"), "got {text}");
        assert!(text.contains("codex (disabled)"), "got {text}");
        assert!(
            !text.contains("claude (disabled)"),
            "the enabled one is not marked: {text}"
        );
    }

    #[test]
    fn the_banner_says_when_a_stored_handoff_is_being_resumed() {
        let without = banner(&facts(), false, false, None);
        assert!(!without.to_lowercase().contains("resuming"));

        let mut with_resume = facts();
        with_resume.resuming = Some("the last stored handoff".to_string());
        let text = banner(&with_resume, false, false, None);
        assert!(text.to_lowercase().contains("resuming"), "got {text}");
        assert!(text.contains("the last stored handoff"), "got {text}");
    }

    #[test]
    fn vt_false_always_selects_the_legacy_tier_regardless_of_colour_or_cols() {
        let plain = banner(&facts(), false, false, None);
        let colour_requested = banner(&facts(), true, false, Some(100));
        assert_eq!(
            plain, colour_requested,
            "no vt must render identically to no colour at all, whatever cols is"
        );
        assert!(
            !plain.contains('\u{1b}'),
            "no vt means no escape codes at all: {plain}"
        );
    }

    // The box tier (vt = true, cols >= BANNER_BOX_MIN_COLS): the rounded
    // frame, real glyphs, and the short one-word rule label.

    #[test]
    fn the_box_tier_names_the_harness_model_and_short_rule_word() {
        let mut with_model = facts();
        with_model.model = Some("sonnet".to_string());
        let text = banner(&with_model, false, true, Some(80));
        assert!(text.contains("claude"), "got {text}");
        assert!(text.contains("model sonnet"), "got {text}");
        assert!(text.contains("rule configured"), "got {text}");
        assert!(
            text.starts_with("\u{256d}"),
            "opens with the box's top-left corner: {text}"
        );
    }

    #[test]
    fn the_box_tier_omits_the_model_segment_when_unset() {
        let text = banner(&facts(), false, true, Some(80));
        assert!(!text.contains("model "), "got {text}");
    }

    #[test]
    fn the_box_tier_lists_the_harness_roster_with_ready_and_disabled_glyphs() {
        let text = banner(&facts(), false, true, Some(80));
        assert!(text.contains("claude \u{25cf}"), "got {text}");
        assert!(text.contains("codex \u{25cb} disabled"), "got {text}");
    }

    #[test]
    fn the_box_tier_shows_a_resume_line_only_when_resuming() {
        let without = banner(&facts(), false, true, Some(80));
        assert!(!without.contains('\u{21ba}'), "got {without}");

        let mut with_resume = facts();
        with_resume.resuming = Some("handoff from c357a8fb".to_string());
        let text = banner(&with_resume, false, true, Some(80));
        assert!(text.contains('\u{21ba}'), "got {text}");
        assert!(text.contains("handoff from c357a8fb"), "got {text}");
    }

    #[test]
    fn the_box_tiers_border_always_joins_its_own_corners() {
        for name in [
            "claude",
            "a-very-long-harness-name-indeed",
            "日本語エージェント",
        ] {
            let mut long = facts();
            long.harness = name.to_string();
            for cols in [BANNER_BOX_MIN_COLS, 80, 200] {
                let text = banner(&long, false, true, Some(cols));
                for line in text.lines() {
                    assert_eq!(
                        style::display_width(line),
                        style::display_width(text.lines().next().unwrap()),
                        "every line has the same display width as the top border: {text}"
                    );
                }
                assert!(
                    text.lines()
                        .all(|l| style::display_width(l) <= cols as usize),
                    "no line exceeds the terminal width: {text}"
                );
            }
        }
    }

    #[test]
    fn the_box_tier_round_trips_exactly_under_colour() {
        let coloured = banner(&facts(), true, true, Some(80));
        let plain = banner(&facts(), false, true, Some(80));
        assert_eq!(
            console::strip_ansi_codes(&coloured),
            plain,
            "colour must never change the underlying text"
        );
        assert_ne!(coloured, plain, "colour must actually add something");
    }

    // The compact tier (vt = true, cols < BANNER_BOX_MIN_COLS, or cols
    // unknown): two lines, no box, no padding.

    #[test]
    fn the_compact_tier_is_selected_below_the_box_width_floor_and_when_cols_is_unknown() {
        let narrow = banner(&facts(), false, true, Some(BANNER_BOX_MIN_COLS - 1));
        assert!(narrow.starts_with('\u{258e}'), "got {narrow}");
        assert!(!narrow.contains('\u{256d}'), "no box corner: {narrow}");

        let unknown = banner(&facts(), false, true, None);
        assert!(unknown.starts_with('\u{258e}'), "got {unknown}");
        assert!(!unknown.contains('\u{256d}'), "no box corner: {unknown}");
    }

    #[test]
    fn the_compact_tier_names_the_harness_and_the_roster() {
        // Wide enough (unlike the box tier's floor of 56) for the whole
        // roster line to fit without its own ellipsis truncation kicking in.
        let text = banner(&facts(), false, true, Some(50));
        assert!(text.contains("zirv"), "got {text}");
        assert!(text.contains("claude"), "got {text}");
        assert!(text.contains("session abc12345"), "got {text}");
        assert!(text.contains("codex \u{25cb} disabled"), "got {text}");
    }

    #[test]
    fn the_compact_tier_round_trips_exactly_under_colour() {
        let coloured = banner(&facts(), true, true, Some(50));
        let plain = banner(&facts(), false, true, Some(50));
        assert_eq!(console::strip_ansi_codes(&coloured), plain);
        assert_ne!(coloured, plain, "colour must actually add something");
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
            usage_five_hour: Some(63.4),
            usage_seven_day: Some(51.0),
            unread_mail: Some((2, 0)),
            degraded: false,
        }
    }

    #[test]
    fn the_bar_renders_harness_score_verdict_usage_mail_and_supervision_state() {
        let line = status_bar(&bar_state(), 200, false);
        assert!(line.contains("claude"), "got {line}");
        assert!(line.contains("\u{273b} advise 42"), "got {line}");
        assert!(line.contains("\u{25d4} 63%\u{b7}51%"), "got {line}");
        assert!(line.contains("\u{2709} 2"), "got {line}");
        assert!(line.contains("\u{25cf} supervised"), "got {line}");
    }

    #[test]
    fn only_the_five_hour_window_is_known_the_other_half_is_a_placeholder() {
        let mut state = bar_state();
        state.usage_seven_day = None;
        let line = status_bar(&state, 200, false);
        assert!(line.contains("\u{25d4} 63%\u{b7}\u{2013}"), "got {line}");
    }

    #[test]
    fn only_the_seven_day_window_is_known_the_other_half_is_a_placeholder() {
        let mut state = bar_state();
        state.usage_five_hour = None;
        let line = status_bar(&state, 200, false);
        assert!(line.contains("\u{25d4} \u{2013}\u{b7}51%"), "got {line}");
    }

    #[test]
    fn the_bar_never_exceeds_the_terminal_width_at_any_size() {
        let full = status_bar(&bar_state(), 200, false);
        assert!(
            style::display_width(&full) > 20,
            "sanity: the untruncated line is long"
        );

        for cols in [1u16, 5, 12, 20, 30, 50, 80, 200] {
            let line = status_bar(&bar_state(), cols, false);
            assert!(
                style::display_width(&line) <= cols as usize,
                "cols={cols} produced {line:?} with width {}",
                style::display_width(&line)
            );
        }
    }

    /// Priority truncation: as the width tightens, the least important
    /// segments drop first (usage, then the score number, then the harness
    /// name, then the chip), while the verdict word, mail and supervision
    /// stay legible as long as possible.
    #[test]
    fn narrow_widths_drop_segments_in_priority_order() {
        let full = status_bar(&bar_state(), 200, true);
        assert!(full.contains("zirv"), "the full line carries the chip");

        // Wide enough for everything but the usage window (full is 60
        // columns; dropping usage alone brings it to 48).
        let no_usage = status_bar(&bar_state(), 50, false);
        assert!(
            !no_usage.contains('\u{25d4}'),
            "usage should be the first thing dropped: {no_usage}"
        );
        assert!(
            no_usage.contains("42") && no_usage.contains("claude") && no_usage.contains("zirv"),
            "the score, harness and chip should still be present here: {no_usage}"
        );

        // Every droppable segment gone (usage, score, harness and chip all
        // dropped, at 36/45/48/60 columns respectively) but the protected
        // core (verdict word, mail, supervision -- 29 columns) still fits
        // whole.
        let core_only = status_bar(&bar_state(), 30, false);
        assert!(!core_only.contains('\u{25d4}'), "got {core_only}");
        assert!(!core_only.contains("zirv"), "the chip is gone: {core_only}");
        assert!(
            !core_only.contains("claude"),
            "the harness name is gone: {core_only}"
        );
        assert!(
            core_only.contains("\u{273b} advise"),
            "the verdict word survives: {core_only}"
        );
        assert!(core_only.contains("supervised"), "got {core_only}");
        assert!(
            style::display_width(&core_only) <= 30,
            "still within budget: {core_only}"
        );
    }

    #[test]
    fn even_the_protected_core_is_hard_truncated_as_a_last_resort() {
        // The protected core (verdict word + mail + supervision) alone is
        // 29 columns; a terminal narrower than that still must not overflow.
        let line = status_bar(&bar_state(), 10, false);
        assert_eq!(style::display_width(&line), 10);
        assert!(
            "\u{273b} advise   \u{2709} 2   \u{25cf} supervised".starts_with(&line),
            "the hard truncate is an exact prefix of the protected core, no ellipsis: {line:?}"
        );
    }

    #[test]
    fn a_degraded_session_says_so_in_the_bar() {
        let mut degraded = bar_state();
        degraded.degraded = true;
        let line = status_bar(&degraded, 200, false);
        assert!(line.contains("\u{25b2} degraded"), "got {line}");
        assert!(!line.contains("supervised"), "got {line}");
    }

    #[test]
    fn unknown_score_verdict_usage_and_mail_render_as_placeholders_not_zeros() {
        let mut unknown = bar_state();
        unknown.usage_five_hour = None;
        unknown.usage_seven_day = None;
        unknown.unread_mail = None;
        unknown.score = None;
        unknown.verdict = None;
        let line = status_bar(&unknown, 200, false);

        assert!(!line.contains('0'), "no invented zero anywhere: {line}");
        // One placeholder for the verdict, two for the usage window's own
        // two slots, one for mail: the field structure changed (usage now
        // always shows both halves) but the never-a-zero contract has not.
        assert_eq!(
            line.matches('\u{2013}').count(),
            4,
            "verdict, both usage halves, and mail: {line}"
        );
    }

    // The chip is the one segment whose *text*, not just its styling,
    // depends on `colour` (` zirv ` padded to fill a background vs. the
    // bare word `zirv` with no background to fill), so it is deliberately
    // excluded from the strip-round-trip check below; a narrow `cols` that
    // drops the chip entirely isolates every other segment, which must
    // still round-trip exactly.
    #[test]
    fn the_verdict_mail_and_supervision_segments_round_trip_exactly_under_colour() {
        for state in [bar_state(), {
            let mut d = bar_state();
            d.degraded = true;
            d
        }] {
            let coloured = status_bar(&state, 30, true);
            let plain = status_bar(&state, 30, false);
            assert!(
                !plain.contains("zirv"),
                "sanity: the chip is dropped at this width"
            );
            assert_eq!(console::strip_ansi_codes(&coloured), plain);
            assert_ne!(coloured, plain, "colour must actually add something");
        }
    }

    #[test]
    fn colour_off_renders_the_chip_as_a_bare_word_with_no_padding() {
        let line = status_bar(&bar_state(), 200, false);
        assert!(
            line.starts_with("zirv"),
            "the bare word, no leading padding space: {line}"
        );
        assert!(
            line.starts_with("zirv   claude"),
            "exactly one 3-space segment gap follows it: {line}"
        );
    }

    #[test]
    fn colour_on_renders_the_chip_as_a_padded_background_word() {
        let coloured = status_bar(&bar_state(), 200, true);
        let plain = console::strip_ansi_codes(&coloured).to_string();
        // The chip's own trailing space plus the 3-space segment gap: four
        // spaces between it and the harness name, not three.
        assert!(
            plain.starts_with(" zirv    claude"),
            "the chip gains a leading/trailing padding space to fill its \
             background: {plain}"
        );
    }

    /// N7: a message addressed to this session specifically must not
    /// disappear into an undifferentiated total.
    #[test]
    fn the_status_bar_shows_session_addressed_mail_separately_from_broadcast() {
        let mut both = bar_state();
        both.unread_mail = Some((2, 1));
        let line = status_bar(&both, 200, false);
        assert!(line.contains("\u{2709} 2+1"), "got {line}");

        let mut broadcast_only = bar_state();
        broadcast_only.unread_mail = Some((2, 0));
        let line = status_bar(&broadcast_only, 200, false);
        assert!(
            line.contains("\u{2709} 2") && !line.contains('+'),
            "no direct mail means the plain count, unchanged from before N7: {line}"
        );

        let mut direct_only = bar_state();
        direct_only.unread_mail = Some((0, 3));
        let line = status_bar(&direct_only, 200, false);
        assert!(line.contains("\u{2709} 0+3"), "got {line}");

        let mut none_unread = bar_state();
        none_unread.unread_mail = Some((0, 0));
        let line = status_bar(&none_unread, 200, false);
        assert!(
            line.contains("\u{2709} \u{2013}"),
            "an explicit (0, 0) is still nothing unread, not a literal zero: {line}"
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
