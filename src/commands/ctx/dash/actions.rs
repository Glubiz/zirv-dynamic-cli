//! Issue #354 phase 4: the dashboard's ONE action-descriptor table.
//!
//! Before this module the same set of actions was written down four times --
//! `ui::HELP_BINDINGS` (the help overlay), `ui::header_hints` (the header's
//! context cluster), `dash::menu_entries` (the phase-3 context menu and its
//! disable reasons) and, implicitly, `filter_key` itself. Four tables that
//! had to be kept in step by hand is how a drawn chord ends up naming an
//! action the keyboard does not have (finding F09) and how the help screen
//! ends up describing a dashboard that no longer exists (finding F08).
//!
//! [`ACTIONS`] is now the single source of truth. It feeds:
//!
//! 1. the help overlay (`^A ?` / `^A h`) -- grouped by [`ActionSection`];
//! 2. the header hint cluster -- [`header_ids`] picks its at-most-four
//!    entries out of the table by context;
//! 3. the phase-3 context menu -- every descriptor carrying a
//!    [`MenuAction`], in table order, with its own disable reason;
//! 4. the `^A p` palette -- every descriptor, fuzzy-searchable.
//!
//! The module is pure: no frame, no filesystem, no clock, no config. A
//! descriptor's `checks` are the exact `(KeyEvent, DashAction)` pairs
//! `filter_key` must produce for that chord, which is what the sync tests at
//! the bottom walk -- `filter_key` stays the one and only key -> action
//! mapping, and this table only *references* it.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::DashAction;
use super::ui::MenuAction;

/// Why a context-menu entry cannot be used, named once so the menu, the
/// palette and their tests can never drift.
pub const MENU_NOT_ATTACHED: &str = "not a pane this dashboard owns";
pub const MENU_ENDED: &str = "this pane has ended";
pub const MENU_STILL_RUNNING: &str = "still running";
pub const MENU_NO_REQUEST: &str = "no spawn request kept";
pub const MENU_NO_CWD: &str = "no cwd known";
pub const MENU_EXITED_CLEAN: &str = "exited cleanly";
pub const MENU_NOT_RETAINED: &str = "only a finished row can be dismissed";
/// Issue #354 phase 5: the sidebar's summary line is a real action target --
/// `inspect` opens the dashboard-level inspector over it -- but it is not a
/// session, so every per-session action is listed there inert with this
/// reason rather than hidden.
pub const MENU_SUMMARY_LINE: &str = "the dashboard, not a session";

/// A stable identity for one row of [`ACTIONS`].
///
/// Deliberately its own enum rather than [`DashAction`]: several descriptors
/// stand for a *pair* of actions (`^A ↑/↓` is `SelectUp` and `SelectDown`),
/// several stand for a parameterised one (`^A 1-9` is `Switch(n)`), and
/// several -- the row actions the context menu adds -- have no global chord
/// at all. `checks` is where a descriptor names the real `DashAction`s.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActionId {
    NextPane,
    SelectRow,
    FoldGroup,
    JumpPane,
    ScrollPage,
    ScrollEnds,
    ContextActions,
    Inspect,
    Focus,
    Nudge,
    Mail,
    Handover,
    Stop,
    Restore,
    OpenWorktree,
    Evidence,
    Retry,
    Dismiss,
    Spawn,
    Memory,
    Errors,
    Zoom,
    SelectMode,
    Palette,
    Help,
    Quit,
    LiteralPrefix,
    Wheel,
    EscEnter,
}

/// Which block of the help/palette listing a descriptor belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionSection {
    /// Moving the roster cursor, the focus and the scrollback.
    Navigate,
    /// Everything that acts on the selected row -- which is also, in this
    /// exact order, the context menu.
    Session,
    /// The dashboard itself.
    Dashboard,
    /// Reaches the dashboard with no prefix at all.
    Pointer,
    /// Not a binding: the one rule that holds in every dialog.
    Note,
}

impl ActionSection {
    /// The heading the help/palette listing draws above the section.
    pub fn title(self) -> &'static str {
        match self {
            ActionSection::Navigate => "navigate",
            ActionSection::Session => "the selected row",
            ActionSection::Dashboard => "dashboard",
            ActionSection::Pointer => "no prefix",
            ActionSection::Note => "every dialog",
        }
    }

    /// Display order of the sections themselves.
    pub const ORDER: &'static [ActionSection] = &[
        ActionSection::Navigate,
        ActionSection::Session,
        ActionSection::Dashboard,
        ActionSection::Pointer,
        ActionSection::Note,
    ];
}

/// Whether an action can be used right now, and -- when it cannot -- the
/// short reason the menu and the palette both render beside it.
///
/// `Hidden` is not "disabled with no reason": it is for an action that has no
/// meaning at all in this context (a row action with no row selected), which
/// is listed nowhere rather than listed inert. The context menu never sees a
/// `Hidden`, because it always has a target row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    Enabled,
    Disabled(&'static str),
    Hidden,
}

impl Availability {
    pub fn is_enabled(self) -> bool {
        matches!(self, Availability::Enabled)
    }

    /// The reason an entry is inert, or `None` when it is usable (or hidden,
    /// which is not something an operator is ever shown).
    pub fn reason(self) -> Option<&'static str> {
        match self {
            Availability::Disabled(reason) => Some(reason),
            _ => None,
        }
    }
}

/// Everything an [`ActionDescriptor`]'s availability is decided from: the
/// selected (or, for the context menu, the targeted) row's own facts. Plain
/// booleans rather than the roster row itself, so this module stays pure and
/// the whole availability matrix is testable as a function of nine bits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ActionContext {
    /// There is a row to act on at all.
    pub selected: bool,
    /// This dashboard owns a live `Pane` for it.
    pub attached: bool,
    /// It is a live session (attached, or a view-only registry row).
    pub alive: bool,
    /// It is an ended pane -- a retained completed worker, or one reaped and
    /// still on screen.
    pub ended: bool,
    /// Its glyph is `▲`: something is waiting on the operator there.
    pub needs_action: bool,
    /// It is one of the retained ended rows, so it can be dropped.
    pub retained: bool,
    /// The dashboard still holds the spawn request that created it.
    pub has_request: bool,
    /// Its recorded exit code was zero (or none was recorded).
    pub clean_exit: bool,
    /// Its checkout is known.
    pub has_cwd: bool,
    /// Issue #354 phase 5: the target is the sidebar's summary line -- the
    /// dashboard itself. `selected` is false there (there is no session row),
    /// but `inspect` still has something to open, so this is its own bit
    /// rather than an overload of `selected`.
    pub summary: bool,
}

fn always(_: &ActionContext) -> Availability {
    Availability::Enabled
}

/// Needs a target of any kind -- a session row, or the summary line. The
/// actions that work on both (`inspect`, and the menu that offers it).
fn needs_target(ctx: &ActionContext) -> Availability {
    if ctx.selected || ctx.summary {
        Availability::Enabled
    } else {
        Availability::Hidden
    }
}

/// Needs an actual session row, and nothing more.
fn needs_row(ctx: &ActionContext) -> Availability {
    match session_gate(ctx) {
        Some(refusal) => refusal,
        None => Availability::Enabled,
    }
}

/// The one rule every per-session action starts with: the summary line names
/// no session (inert, with a reason -- an operator must be able to see that
/// `nudge` exists and why it does not apply here), and nothing selected at
/// all means the action does not apply and is not listed.
fn session_gate(ctx: &ActionContext) -> Option<Availability> {
    if ctx.summary {
        Some(Availability::Disabled(MENU_SUMMARY_LINE))
    } else if ctx.selected {
        None
    } else {
        Some(Availability::Hidden)
    }
}

/// Needs a live pane this dashboard owns: focus, handover, stop.
fn needs_attached(ctx: &ActionContext) -> Availability {
    if let Some(refusal) = session_gate(ctx) {
        refusal
    } else if ctx.attached && !ctx.ended {
        Availability::Enabled
    } else if ctx.ended {
        Availability::Disabled(MENU_ENDED)
    } else {
        Availability::Disabled(MENU_NOT_ATTACHED)
    }
}

/// A nudge reaches a view-only registry session too (`sessions::
/// run_nudge_with`'s headless marker path), so "alive" is the real gate here,
/// not "attached".
fn needs_alive(ctx: &ActionContext) -> Availability {
    if let Some(refusal) = session_gate(ctx) {
        refusal
    } else if ctx.alive {
        Availability::Enabled
    } else {
        Availability::Disabled(MENU_ENDED)
    }
}

fn needs_request(ctx: &ActionContext) -> Availability {
    if let Some(refusal) = session_gate(ctx) {
        refusal
    } else if !ctx.ended {
        Availability::Disabled(MENU_STILL_RUNNING)
    } else if !ctx.has_request {
        Availability::Disabled(MENU_NO_REQUEST)
    } else {
        Availability::Enabled
    }
}

/// Exactly `restore`, named for the failure case: a clean exit has nothing to
/// retry.
fn needs_failed_request(ctx: &ActionContext) -> Availability {
    match needs_request(ctx) {
        Availability::Enabled if ctx.clean_exit => Availability::Disabled(MENU_EXITED_CLEAN),
        other => other,
    }
}

fn needs_cwd(ctx: &ActionContext) -> Availability {
    if let Some(refusal) = session_gate(ctx) {
        refusal
    } else if ctx.has_cwd {
        Availability::Enabled
    } else {
        Availability::Disabled(MENU_NO_CWD)
    }
}

fn needs_retained(ctx: &ActionContext) -> Availability {
    if let Some(refusal) = session_gate(ctx) {
        refusal
    } else if ctx.retained {
        Availability::Enabled
    } else {
        Availability::Disabled(MENU_NOT_RETAINED)
    }
}

/// One action the dashboard offers, wherever it is offered.
///
/// `chord` is the chord exactly as it is drawn everywhere -- the header
/// cluster, the help listing and the palette all print this same string, so a
/// chord shown in one place can never disagree with the same chord shown in
/// another. It is empty for a row action that only the context menu and the
/// palette can reach.
pub struct ActionDescriptor {
    pub id: ActionId,
    pub chord: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub section: ActionSection,
    pub availability: fn(&ActionContext) -> Availability,
    /// The context-menu entry this descriptor is, when it is one. The menu's
    /// order IS this table's order, filtered to `Some`.
    pub menu: Option<MenuAction>,
    /// The `filter_key` outcomes this chord must produce, in the armed state.
    /// Empty for a descriptor that is not one keystroke (the mouse wheel, the
    /// Esc/Enter note, every menu-only row action).
    pub checks: &'static [(KeyEvent, DashAction)],
}

impl ActionDescriptor {
    /// The `DashAction` this descriptor's chord dispatches to, or `None` for
    /// one that is not a global chord at all. The FIRST check: a descriptor
    /// standing for a pair (`^A ↑/↓`) runs the first of the pair when it is
    /// invoked from the palette, which is the one an operator picking
    /// "select row" out of a list means.
    pub fn dash_action(&self) -> Option<DashAction> {
        self.checks.first().map(|(_, action)| action.clone())
    }

    /// Whether this descriptor documents something rather than binding it:
    /// nothing dispatches it and no menu entry runs it. Review of cc92a56
    /// (finding 1): these used to be listed in the palette as ordinary
    /// enabled rows, so Enter on one closed the palette and did nothing at
    /// all. They are [`PaletteRow::Note`]s now.
    pub fn informational(&self) -> bool {
        self.checks.is_empty() && self.menu.is_none()
    }

    /// What the fuzzy filter matches against: label, description and chord,
    /// lowercased once. Unicode-safe -- `char` iteration, never byte slicing.
    pub fn haystack(&self) -> String {
        let mut hay =
            String::with_capacity(self.label.len() + self.description.len() + self.chord.len() + 2);
        for part in [self.label, self.description, self.chord] {
            if !hay.is_empty() {
                hay.push(' ');
            }
            hay.extend(part.chars().flat_map(char::to_lowercase));
        }
        hay
    }
}

const fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

const fn ch(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}

/// THE table. Order inside a section is display order; the `Session`
/// section's order is additionally the context menu's own approved order
/// (inspect, focus, nudge, mail, handover, stop, restore, open worktree,
/// evidence, retry, dismiss) -- `^A c` itself leads the section because it is
/// what opens that menu.
///
/// A `static`, not a function rebuilding a `Vec`: the header cluster reaches
/// this on every frame, which during the adaptive poll's hot window is up to
/// ~100/s. `KeyEvent::new` is `const fn` in crossterm 0.29, so the whole
/// table is built once at compile time.
pub static ACTIONS: &[ActionDescriptor] = &[
    ActionDescriptor {
        id: ActionId::NextPane,
        chord: "^A Tab",
        label: "next pane",
        description: "focus the next pane",
        section: ActionSection::Navigate,
        availability: always,
        menu: None,
        checks: &[(key(KeyCode::Tab), DashAction::NextPane)],
    },
    ActionDescriptor {
        id: ActionId::SelectRow,
        chord: "^A \u{2191}/\u{2193}",
        label: "select row",
        description: "move the roster cursor",
        section: ActionSection::Navigate,
        availability: always,
        menu: None,
        checks: &[
            (key(KeyCode::Up), DashAction::SelectUp),
            (key(KeyCode::Down), DashAction::SelectDown),
        ],
    },
    ActionDescriptor {
        id: ActionId::FoldGroup,
        chord: "^A \u{2190}/\u{2192}",
        label: "fold group",
        description: "collapse or expand a work group",
        section: ActionSection::Navigate,
        availability: always,
        menu: None,
        checks: &[
            (key(KeyCode::Left), DashAction::CollapseGroup),
            (key(KeyCode::Right), DashAction::ExpandGroup),
        ],
    },
    ActionDescriptor {
        id: ActionId::JumpPane,
        chord: "^A 1-9",
        label: "jump to pane",
        description: "focus pane 1 through 9",
        section: ActionSection::Navigate,
        availability: always,
        menu: None,
        checks: &[
            (ch('1'), DashAction::Switch(0)),
            (ch('9'), DashAction::Switch(8)),
        ],
    },
    ActionDescriptor {
        id: ActionId::ScrollPage,
        chord: "^A PgUp/PgDn",
        label: "scroll page",
        description: "page the focused pane",
        section: ActionSection::Navigate,
        availability: always,
        menu: None,
        checks: &[
            (key(KeyCode::PageUp), DashAction::ScrollPageUp),
            (key(KeyCode::PageDown), DashAction::ScrollPageDown),
        ],
    },
    ActionDescriptor {
        id: ActionId::ScrollEnds,
        chord: "^A Home/End",
        label: "scroll ends",
        description: "oldest row, or back to live",
        section: ActionSection::Navigate,
        availability: always,
        menu: None,
        checks: &[
            (key(KeyCode::Home), DashAction::ScrollTop),
            (key(KeyCode::End), DashAction::ScrollLive),
        ],
    },
    ActionDescriptor {
        id: ActionId::ContextActions,
        chord: "^A c",
        label: "actions",
        description: "action menu (or right click)",
        section: ActionSection::Session,
        availability: needs_target,
        menu: None,
        checks: &[(ch('c'), DashAction::ContextActions)],
    },
    ActionDescriptor {
        id: ActionId::Inspect,
        chord: "^A i",
        label: "inspect",
        // Issue #354 phase 5: "this row" includes the summary line, where it
        // opens the dashboard-level inspector instead.
        description: "evidence for this row",
        section: ActionSection::Session,
        availability: needs_target,
        menu: Some(MenuAction::Inspect),
        checks: &[(ch('i'), DashAction::Inspect)],
    },
    ActionDescriptor {
        id: ActionId::Focus,
        chord: "",
        label: "focus",
        description: "give this row the keyboard",
        section: ActionSection::Session,
        availability: needs_attached,
        menu: Some(MenuAction::Focus),
        checks: &[],
    },
    ActionDescriptor {
        id: ActionId::Nudge,
        chord: "^A n",
        label: "nudge",
        description: "send this session a line",
        section: ActionSection::Session,
        availability: needs_alive,
        menu: Some(MenuAction::Nudge),
        checks: &[(ch('n'), DashAction::Nudge)],
    },
    ActionDescriptor {
        id: ActionId::Mail,
        chord: "^A m",
        label: "mail",
        description: "read and compose mail",
        section: ActionSection::Session,
        availability: always,
        menu: Some(MenuAction::Mail),
        checks: &[(ch('m'), DashAction::Mail)],
    },
    ActionDescriptor {
        id: ActionId::Handover,
        chord: "^A o",
        label: "handover",
        description: "swap model or harness",
        section: ActionSection::Session,
        availability: needs_attached,
        menu: Some(MenuAction::Handover),
        checks: &[(ch('o'), DashAction::Handover)],
    },
    ActionDescriptor {
        id: ActionId::Stop,
        chord: "",
        label: "stop",
        description: "ask this harness to quit",
        section: ActionSection::Session,
        availability: needs_attached,
        menu: Some(MenuAction::Stop),
        checks: &[],
    },
    ActionDescriptor {
        id: ActionId::Restore,
        chord: "^A r",
        label: "restore",
        description: "relaunch an ended row",
        section: ActionSection::Session,
        availability: needs_request,
        menu: Some(MenuAction::Restore),
        checks: &[(ch('r'), DashAction::RestoreRow)],
    },
    ActionDescriptor {
        id: ActionId::OpenWorktree,
        chord: "",
        label: "open worktree",
        description: "show this row's checkout",
        section: ActionSection::Session,
        availability: needs_cwd,
        menu: Some(MenuAction::OpenWorktree),
        checks: &[],
    },
    ActionDescriptor {
        id: ActionId::Evidence,
        chord: "",
        label: "evidence",
        description: "inspect, at the evidence",
        section: ActionSection::Session,
        availability: needs_row,
        menu: Some(MenuAction::Evidence),
        checks: &[],
    },
    ActionDescriptor {
        id: ActionId::Retry,
        chord: "",
        label: "retry",
        description: "relaunch a row that failed",
        section: ActionSection::Session,
        availability: needs_failed_request,
        menu: Some(MenuAction::Retry),
        checks: &[],
    },
    ActionDescriptor {
        id: ActionId::Dismiss,
        chord: "",
        label: "dismiss",
        description: "drop a finished row",
        section: ActionSection::Session,
        availability: needs_retained,
        menu: Some(MenuAction::Dismiss),
        checks: &[],
    },
    ActionDescriptor {
        id: ActionId::Spawn,
        chord: "^A s",
        label: "spawn",
        description: "start a new worker here",
        section: ActionSection::Dashboard,
        availability: always,
        menu: None,
        checks: &[(ch('s'), DashAction::Spawn)],
    },
    ActionDescriptor {
        id: ActionId::Memory,
        chord: "^A M",
        label: "memory",
        description: "browse the memory bank",
        section: ActionSection::Dashboard,
        availability: always,
        menu: None,
        checks: &[(ch('M'), DashAction::Memory)],
    },
    ActionDescriptor {
        id: ActionId::Errors,
        chord: "^A e",
        label: "errors",
        description: "the recent error buffer",
        section: ActionSection::Dashboard,
        availability: always,
        menu: None,
        checks: &[(ch('e'), DashAction::ShowErrors)],
    },
    ActionDescriptor {
        id: ActionId::Zoom,
        chord: "^A z",
        label: "zoom",
        description: "hide the chrome",
        section: ActionSection::Dashboard,
        availability: always,
        menu: None,
        checks: &[(ch('z'), DashAction::Zoom)],
    },
    ActionDescriptor {
        id: ActionId::SelectMode,
        chord: "^A v",
        label: "select mode",
        description: "toggle text selection",
        section: ActionSection::Dashboard,
        availability: always,
        menu: None,
        checks: &[(ch('v'), DashAction::ToggleSelectMode)],
    },
    ActionDescriptor {
        id: ActionId::Palette,
        chord: "^A p",
        label: "palette",
        description: "search and run any action",
        section: ActionSection::Dashboard,
        availability: always,
        menu: None,
        checks: &[(ch('p'), DashAction::Palette)],
    },
    ActionDescriptor {
        id: ActionId::Help,
        chord: "^A ?",
        label: "help",
        description: "this key reference",
        section: ActionSection::Dashboard,
        availability: always,
        menu: None,
        // A real terminal delivers SHIFT alongside both '?' (shift-slash on
        // most layouts) and 'H' itself; `filter_key` matches on `key.code`
        // alone, so all five must (and do) land on the same action.
        checks: &[
            (ch('?'), DashAction::Help),
            (
                KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT),
                DashAction::Help,
            ),
            (ch('h'), DashAction::Help),
            (ch('H'), DashAction::Help),
            (
                KeyEvent::new(KeyCode::Char('H'), KeyModifiers::SHIFT),
                DashAction::Help,
            ),
        ],
    },
    ActionDescriptor {
        id: ActionId::Quit,
        chord: "^A q",
        label: "quit",
        description: "close the dashboard",
        section: ActionSection::Dashboard,
        availability: always,
        menu: None,
        checks: &[(ch('q'), DashAction::Quit)],
    },
    ActionDescriptor {
        id: ActionId::LiteralPrefix,
        chord: "^A ^A",
        label: "literal Ctrl+A",
        description: "send the child a Ctrl+A",
        section: ActionSection::Dashboard,
        availability: always,
        menu: None,
        checks: &[(
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
            DashAction::LiteralPrefix,
        )],
    },
    ActionDescriptor {
        id: ActionId::Wheel,
        chord: "wheel",
        label: "scroll",
        description: "scroll the focused pane",
        section: ActionSection::Pointer,
        availability: always,
        menu: None,
        checks: &[],
    },
    ActionDescriptor {
        id: ActionId::EscEnter,
        chord: "esc / \u{23ce}",
        label: "close / confirm",
        description: "esc closes, enter confirms",
        section: ActionSection::Note,
        availability: always,
        menu: None,
        checks: &[],
    },
];

/// Pure: the descriptor for one id, or `None` for an id no table row claims
/// (which the exhaustiveness test below makes impossible in practice).
pub fn descriptor(id: ActionId) -> Option<&'static ActionDescriptor> {
    ACTIONS.iter().find(|d| d.id == id)
}

/// Pure: the context menu's entries, in table order -- every descriptor that
/// carries a [`MenuAction`], each paired with the availability this context
/// gives it. Every entry is ALWAYS present: an operator who cannot see that
/// `restore` exists cannot learn why it is unavailable.
pub fn menu_actions(ctx: &ActionContext) -> Vec<(MenuAction, Availability)> {
    ACTIONS
        .iter()
        .filter_map(|d| d.menu.map(|m| (m, (d.availability)(ctx))))
        .collect()
}

/// Pure: the ids the header's right-hand cluster offers for `ctx`, in display
/// order, at most four (the approved design's own cap).
///
/// The order is the approved one -- a row that needs action, then an ended
/// row, then any other live pane, then the two hints that always exist -- and
/// every candidate is filtered through its own descriptor's availability, so
/// a chord the header draws always does something. An action that cannot be
/// used is *named and explained* in the context menu instead, never offered
/// inert here.
pub fn header_ids(ctx: &ActionContext) -> Vec<ActionId> {
    let wanted: &[ActionId] = if ctx.summary {
        // Issue #354 phase 5: the cursor is on the summary line -- the two
        // things that still apply there, then the two that always do.
        &[
            ActionId::Inspect,
            ActionId::ContextActions,
            ActionId::Mail,
            ActionId::Help,
        ]
    } else if ctx.needs_action {
        &[
            ActionId::Inspect,
            ActionId::ContextActions,
            ActionId::Nudge,
            ActionId::Help,
        ]
    } else if ctx.ended {
        &[
            ActionId::Inspect,
            ActionId::Restore,
            ActionId::ContextActions,
            ActionId::Help,
        ]
    } else if ctx.alive {
        &[
            ActionId::ContextActions,
            ActionId::Nudge,
            ActionId::Mail,
            ActionId::Help,
        ]
    } else {
        &[ActionId::Errors, ActionId::Help]
    };
    wanted
        .iter()
        .copied()
        .filter(|id| descriptor(*id).is_some_and(|d| (d.availability)(ctx).is_enabled()))
        .take(4)
        .collect()
}

/// Pure: does `query` appear in `haystack` as a case-insensitive subsequence?
///
/// Both sides are walked by `char`, never by byte index: a query typed in any
/// script must not be able to slice a UTF-8 boundary, and the release profile
/// is `panic = "abort"`. An empty query matches everything, which is what
/// makes "the palette with nothing typed lists the whole table" the same code
/// path as every other query. Whitespace in the query is skipped, so
/// `"open worktree"` and `"openwork"` both find the same row.
pub fn fuzzy_match(query: &str, haystack: &str) -> bool {
    let mut hay = haystack.chars().flat_map(char::to_lowercase);
    'outer: for needle in query.chars().flat_map(char::to_lowercase) {
        if needle.is_whitespace() {
            continue;
        }
        for c in hay.by_ref() {
            if c == needle {
                continue 'outer;
            }
        }
        return false;
    }
    true
}

/// One drawn line of the palette (or of the help listing, which is the same
/// list in read-only mode).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteRow {
    /// A section heading. Never activatable, and skipped by the caret.
    Section(&'static str),
    /// Review of cc92a56 (finding 1): a descriptor that documents something
    /// rather than binding it -- the mouse wheel, and the one Esc/Enter rule
    /// every dialog holds. Both are worth listing (that is the whole point of
    /// the help screen) and neither can be run, so they are their own kind:
    /// drawn dim, never taking the caret, and never claiming to be "disabled"
    /// -- there is nothing to enable.
    Note {
        label: &'static str,
        chord: &'static str,
    },
    Action {
        id: ActionId,
        label: &'static str,
        chord: &'static str,
        /// `Some(reason)` renders the row dim and refuses to run it.
        disabled: Option<&'static str>,
    },
}

impl PaletteRow {
    /// Whether the caret may rest on this row and Enter may run it.
    pub fn activatable(&self) -> bool {
        matches!(self, PaletteRow::Action { disabled: None, .. })
    }

    /// Whether the caret may rest on this row at all -- a disabled action
    /// still takes the caret (so its reason can be read), a heading does not.
    pub fn selectable(&self) -> bool {
        matches!(self, PaletteRow::Action { .. })
    }
}

/// Pure: the palette's rows for `ctx` and `query`.
///
/// An empty query lists everything, grouped by [`ActionSection`] with a
/// heading per section; any other query drops the headings and lists the
/// matches flat, in table order. `Hidden` descriptors never appear either
/// way -- a row action with no row selected is not something to explain, it
/// is something that does not apply.
pub fn palette_rows(ctx: &ActionContext, query: &str) -> Vec<PaletteRow> {
    let grouped = query.trim().is_empty();
    let mut rows = Vec::new();
    let visible = |d: &'static ActionDescriptor| -> Option<PaletteRow> {
        let availability = (d.availability)(ctx);
        if availability == Availability::Hidden {
            return None;
        }
        if !grouped && !fuzzy_match(query, &d.haystack()) {
            return None;
        }
        if d.informational() {
            return Some(PaletteRow::Note {
                label: d.label,
                chord: d.chord,
            });
        }
        Some(PaletteRow::Action {
            id: d.id,
            label: d.label,
            chord: d.chord,
            disabled: availability.reason(),
        })
    };
    if grouped {
        for section in ActionSection::ORDER {
            let mut block: Vec<PaletteRow> = ACTIONS
                .iter()
                .filter(|d| d.section == *section)
                .filter_map(visible)
                .collect();
            if block.is_empty() {
                continue;
            }
            rows.push(PaletteRow::Section(section.title()));
            rows.append(&mut block);
        }
    } else {
        rows.extend(ACTIONS.iter().filter_map(visible));
    }
    rows
}

/// Pure: the next row the caret may rest on, walking `delta` (`+1`/`-1`) from
/// `cursor` and skipping section headings. Stays put when there is nothing
/// selectable in that direction, and lands on the first selectable row when
/// `cursor` itself is a heading.
pub fn palette_step(rows: &[PaletteRow], cursor: usize, delta: isize) -> usize {
    if rows.is_empty() {
        return 0;
    }
    let last = rows.len() - 1;
    let mut index = cursor.min(last);
    loop {
        let next = if delta >= 0 {
            if index >= last {
                break;
            }
            index + 1
        } else {
            if index == 0 {
                break;
            }
            index - 1
        };
        index = next;
        if rows[index].selectable() {
            return index;
        }
    }
    // Nothing selectable that way: keep the caret where it was, unless it is
    // parked on a heading, in which case take the first selectable row.
    if rows
        .get(cursor.min(last))
        .is_some_and(PaletteRow::selectable)
    {
        cursor.min(last)
    } else {
        palette_first(rows)
    }
}

/// Pure: the first row the caret may rest on, or `0` when none can.
pub fn palette_first(rows: &[PaletteRow]) -> usize {
    rows.iter().position(PaletteRow::selectable).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::super::{InputVerdict, filter_key};
    use super::*;

    fn live_row() -> ActionContext {
        ActionContext {
            selected: true,
            attached: true,
            alive: true,
            ..ActionContext::default()
        }
    }

    /// The table's own ground truth: every `checks` entry must produce
    /// exactly the `filter_key` outcome it claims, in the armed state, so no
    /// surface fed by this table can ever drift from what `Ctrl+A <key>`
    /// actually does.
    #[test]
    fn action_table_chords_match_the_real_filter_key_dispatch() {
        for descriptor in ACTIONS {
            for (event, expected) in descriptor.checks {
                let (_, verdict) = filter_key(true, *event);
                assert_eq!(
                    verdict,
                    InputVerdict::Dash(expected.clone()),
                    "{} ({}): filter_key({event:?}) disagreed",
                    descriptor.label,
                    descriptor.chord
                );
            }
        }
    }

    /// One flag per [`DashAction`] variant, all false until the test below
    /// sees that action in some descriptor's `checks`. The match against it
    /// has no wildcard arm, so adding a `DashAction` variant without giving
    /// it a flag here -- and a table row that sets it -- is a compile error,
    /// not a silently-undocumented action.
    #[derive(Default)]
    struct DashActionCoverage {
        switch: bool,
        next_pane: bool,
        select_up: bool,
        select_down: bool,
        spawn: bool,
        nudge: bool,
        mail: bool,
        memory: bool,
        handover: bool,
        show_errors: bool,
        zoom: bool,
        quit: bool,
        scroll_page_up: bool,
        scroll_page_down: bool,
        scroll_top: bool,
        scroll_live: bool,
        literal_prefix: bool,
        help: bool,
        palette: bool,
        toggle_select_mode: bool,
        context_actions: bool,
        collapse_group: bool,
        expand_group: bool,
        inspect: bool,
        restore_row: bool,
    }

    /// Completeness, not just correctness: the test above proves every
    /// claimed chord is right, this one proves no `DashAction` variant is
    /// missing a descriptor at all.
    #[test]
    fn the_action_table_covers_every_dash_action() {
        let mut cov = DashActionCoverage::default();
        for descriptor in ACTIONS {
            for (_, action) in descriptor.checks {
                match action {
                    DashAction::ContextMenu(_) => {
                        panic!("pointer targets are covered by route_mouse tests")
                    }
                    DashAction::ContextActions => cov.context_actions = true,
                    DashAction::CollapseGroup => cov.collapse_group = true,
                    DashAction::ExpandGroup => cov.expand_group = true,
                    DashAction::Switch(_) => cov.switch = true,
                    DashAction::NextPane => cov.next_pane = true,
                    DashAction::SelectUp => cov.select_up = true,
                    DashAction::SelectDown => cov.select_down = true,
                    DashAction::Spawn => cov.spawn = true,
                    DashAction::Nudge => cov.nudge = true,
                    DashAction::Mail => cov.mail = true,
                    DashAction::Memory => cov.memory = true,
                    DashAction::Handover => cov.handover = true,
                    DashAction::ShowErrors => cov.show_errors = true,
                    DashAction::Zoom => cov.zoom = true,
                    DashAction::Quit => cov.quit = true,
                    DashAction::ScrollPageUp => cov.scroll_page_up = true,
                    DashAction::ScrollPageDown => cov.scroll_page_down = true,
                    DashAction::ScrollTop => cov.scroll_top = true,
                    DashAction::ScrollLive => cov.scroll_live = true,
                    DashAction::LiteralPrefix => cov.literal_prefix = true,
                    DashAction::Help => cov.help = true,
                    DashAction::Palette => cov.palette = true,
                    DashAction::ToggleSelectMode => cov.toggle_select_mode = true,
                    DashAction::Inspect => cov.inspect = true,
                    DashAction::RestoreRow => cov.restore_row = true,
                }
            }
        }
        assert!(
            cov.switch
                && cov.next_pane
                && cov.select_up
                && cov.select_down
                && cov.spawn
                && cov.nudge
                && cov.mail
                && cov.memory
                && cov.handover
                && cov.show_errors
                && cov.zoom
                && cov.quit
                && cov.scroll_page_up
                && cov.scroll_page_down
                && cov.scroll_top
                && cov.scroll_live
                && cov.literal_prefix
                && cov.help
                && cov.palette
                && cov.toggle_select_mode
                && cov.context_actions
                && cov.collapse_group
                && cov.expand_group
                && cov.inspect
                && cov.restore_row,
            "the action table is missing a row for at least one DashAction variant"
        );
    }

    /// Nothing may be defined twice: one row per id, one row per chord, one
    /// row per menu action, and one row per label (the palette lists labels,
    /// and two identical ones are two rows an operator cannot tell apart).
    #[test]
    fn the_action_table_defines_each_id_chord_label_and_menu_entry_once() {
        for (i, a) in ACTIONS.iter().enumerate() {
            for b in &ACTIONS[i + 1..] {
                assert_ne!(a.id, b.id, "duplicate id {:?}", a.id);
                assert_ne!(a.label, b.label, "duplicate label {:?}", a.label);
                if !a.chord.is_empty() {
                    assert_ne!(a.chord, b.chord, "duplicate chord {:?}", a.chord);
                }
                if let (Some(x), Some(y)) = (a.menu, b.menu) {
                    assert_ne!(x, y, "duplicate menu action {x:?}");
                }
            }
        }
    }

    /// Every `MenuAction` the context menu can render has exactly one
    /// descriptor, and they come back in the approved menu order.
    #[test]
    fn the_menu_order_is_the_tables_own_session_order() {
        let ctx = live_row();
        let order: Vec<MenuAction> = menu_actions(&ctx).into_iter().map(|(m, _)| m).collect();
        assert_eq!(
            order,
            vec![
                MenuAction::Inspect,
                MenuAction::Focus,
                MenuAction::Nudge,
                MenuAction::Mail,
                MenuAction::Handover,
                MenuAction::Stop,
                MenuAction::Restore,
                MenuAction::OpenWorktree,
                MenuAction::Evidence,
                MenuAction::Retry,
                MenuAction::Dismiss,
            ]
        );
    }

    /// The availability matrix the phase-3 menu hard-coded, now read off the
    /// table: a live attached worker, an ended one with no kept request, and
    /// an ended one that failed with its request still held.
    #[test]
    fn availability_reproduces_the_phase_three_disable_reasons() {
        let live = live_row();
        let reason = |ctx: &ActionContext, m: MenuAction| {
            menu_actions(ctx)
                .into_iter()
                .find(|(a, _)| *a == m)
                .and_then(|(_, av)| av.reason())
        };
        assert_eq!(reason(&live, MenuAction::Inspect), None);
        assert_eq!(reason(&live, MenuAction::Focus), None);
        assert_eq!(reason(&live, MenuAction::Restore), Some(MENU_STILL_RUNNING));
        assert_eq!(reason(&live, MenuAction::Retry), Some(MENU_STILL_RUNNING));
        assert_eq!(reason(&live, MenuAction::OpenWorktree), Some(MENU_NO_CWD));
        assert_eq!(reason(&live, MenuAction::Dismiss), Some(MENU_NOT_RETAINED));

        let view_only = ActionContext {
            attached: false,
            ..live
        };
        assert_eq!(
            reason(&view_only, MenuAction::Focus),
            Some(MENU_NOT_ATTACHED)
        );
        assert_eq!(reason(&view_only, MenuAction::Nudge), None);

        let ended = ActionContext {
            selected: true,
            attached: true,
            alive: false,
            ended: true,
            retained: true,
            has_request: false,
            clean_exit: true,
            has_cwd: true,
            needs_action: false,
            summary: false,
        };
        assert_eq!(reason(&ended, MenuAction::Focus), Some(MENU_ENDED));
        assert_eq!(reason(&ended, MenuAction::Nudge), Some(MENU_ENDED));
        assert_eq!(reason(&ended, MenuAction::Restore), Some(MENU_NO_REQUEST));
        assert_eq!(reason(&ended, MenuAction::Dismiss), None);
        assert_eq!(reason(&ended, MenuAction::OpenWorktree), None);

        let failed = ActionContext {
            has_request: true,
            clean_exit: false,
            ..ended
        };
        assert_eq!(reason(&failed, MenuAction::Restore), None);
        assert_eq!(reason(&failed, MenuAction::Retry), None);
        let clean = ActionContext {
            clean_exit: true,
            ..failed
        };
        assert_eq!(reason(&clean, MenuAction::Retry), Some(MENU_EXITED_CLEAN));
    }

    /// The header cluster comes out of the same table, capped at four, and
    /// never offers an action its own descriptor says is unavailable.
    #[test]
    fn header_ids_are_chosen_from_the_table_and_never_offer_a_dead_chord() {
        let alive = live_row();
        assert_eq!(
            header_ids(&alive),
            vec![
                ActionId::ContextActions,
                ActionId::Nudge,
                ActionId::Mail,
                ActionId::Help
            ]
        );
        let needs_action = ActionContext {
            needs_action: true,
            ..alive
        };
        assert_eq!(
            header_ids(&needs_action),
            vec![
                ActionId::Inspect,
                ActionId::ContextActions,
                ActionId::Nudge,
                ActionId::Help
            ]
        );
        let ended = ActionContext {
            selected: true,
            alive: false,
            ended: true,
            ..ActionContext::default()
        };
        assert_eq!(
            header_ids(&ended),
            vec![ActionId::Inspect, ActionId::ContextActions, ActionId::Help]
        );
        let restorable = ActionContext {
            has_request: true,
            ..ended
        };
        assert_eq!(
            header_ids(&restorable),
            vec![
                ActionId::Inspect,
                ActionId::Restore,
                ActionId::ContextActions,
                ActionId::Help
            ]
        );
        assert_eq!(
            header_ids(&ActionContext::default()),
            vec![ActionId::Errors, ActionId::Help]
        );
        for ctx in [alive, needs_action, ended, restorable] {
            assert!(header_ids(&ctx).len() <= 4);
        }
    }

    #[test]
    fn fuzzy_match_is_a_case_insensitive_unicode_safe_subsequence() {
        assert!(fuzzy_match("", "anything"));
        assert!(fuzzy_match("nudge", "nudge send this session a line ^A n"));
        assert!(fuzzy_match("NUDGE", "nudge send this session a line"));
        // A subsequence, not a substring.
        assert!(fuzzy_match("hndv", "handover swap model or harness"));
        assert!(fuzzy_match("open work", "open worktree show this checkout"));
        assert!(!fuzzy_match("zzz", "handover swap model or harness"));
        // Multi-byte on both sides: no byte indexing anywhere, and the
        // lowercase mapping is Unicode's, not ASCII's.
        assert!(fuzzy_match("\u{c5}", "\u{e5}rhus"));
        assert!(fuzzy_match("\u{2191}", "^A \u{2191}/\u{2193} select row"));
        assert!(!fuzzy_match("\u{4e2d}\u{6587}", "select row"));
    }

    #[test]
    fn an_empty_query_lists_every_section_and_a_query_flattens_the_matches() {
        let ctx = live_row();
        let all = palette_rows(&ctx, "");
        assert!(all.contains(&PaletteRow::Section("navigate")));
        assert!(all.contains(&PaletteRow::Section("the selected row")));
        // Everything the table has, minus nothing (a live selected row hides
        // no descriptor) -- except the informational notes, which are listed
        // but never take the caret.
        let notes = ACTIONS.iter().filter(|d| d.informational()).count();
        assert_eq!(
            all.iter()
                .filter(|r| matches!(r, PaletteRow::Note { .. }))
                .count(),
            notes
        );
        assert_eq!(
            all.iter().filter(|r| r.selectable()).count(),
            ACTIONS.len() - notes,
            "an empty query must list the whole table"
        );

        let filtered = palette_rows(&ctx, "nudge");
        assert!(!filtered.iter().any(|r| matches!(r, PaletteRow::Section(_))));
        assert!(filtered.iter().any(|r| matches!(
            r,
            PaletteRow::Action {
                id: ActionId::Nudge,
                ..
            }
        )));
        assert!(palette_rows(&ctx, "qqqqzzz").is_empty());
    }

    /// A row action with no row selected is not listed at all; a row action
    /// that cannot be used on THIS row is listed with its reason and is not
    /// activatable.
    #[test]
    fn hidden_rows_vanish_and_disabled_rows_are_listed_but_inert() {
        let none = ActionContext::default();
        let rows = palette_rows(&none, "");
        assert!(!rows.iter().any(|r| matches!(
            r,
            PaletteRow::Action {
                id: ActionId::Focus,
                ..
            }
        )));
        // The dashboard-wide actions are all still there.
        assert!(rows.iter().any(|r| matches!(
            r,
            PaletteRow::Action {
                id: ActionId::Quit,
                ..
            }
        )));

        let live = live_row();
        let restore = palette_rows(&live, "restore")
            .into_iter()
            .find(|r| {
                matches!(
                    r,
                    PaletteRow::Action {
                        id: ActionId::Restore,
                        ..
                    }
                )
            })
            .expect("restore row");
        assert_eq!(
            restore,
            PaletteRow::Action {
                id: ActionId::Restore,
                label: "restore",
                chord: "^A r",
                disabled: Some(MENU_STILL_RUNNING),
            }
        );
        assert!(!restore.activatable());
        assert!(restore.selectable());
    }

    /// The caret never rests on a section heading, and never runs off either
    /// end of the list.
    #[test]
    fn the_palette_caret_skips_headings_and_clamps_at_both_ends() {
        let rows = palette_rows(&live_row(), "");
        let first = palette_first(&rows);
        assert!(rows[first].selectable());
        assert_eq!(palette_step(&rows, first, -1), first);
        let mut cursor = first;
        for _ in 0..rows.len() * 2 {
            cursor = palette_step(&rows, cursor, 1);
            assert!(rows[cursor].selectable(), "caret landed on a heading");
        }
        // Walking all the way down and back up stays selectable throughout.
        for _ in 0..rows.len() * 2 {
            cursor = palette_step(&rows, cursor, -1);
            assert!(rows[cursor].selectable());
        }
        assert_eq!(cursor, first);
        // A caret parked on a heading is pulled back onto a real row.
        assert!(rows[palette_step(&rows, 0, -1)].selectable());
        assert!(palette_step(&[], 0, 1) == 0);
    }

    /// Review of cc92a56 (finding 1): every row the palette lets the caret
    /// rest on and Enter activate must have something to run. The wheel and
    /// the Esc/Enter note are listed as notes instead -- visible, dim, inert,
    /// and never claiming to be "disabled".
    #[test]
    fn every_activatable_palette_row_has_a_runnable_action() {
        for ctx in [live_row(), ActionContext::default(), summary_line()] {
            for row in palette_rows(&ctx, "") {
                let PaletteRow::Action { id, .. } = row else {
                    continue;
                };
                if !row.activatable() {
                    continue;
                }
                let d = descriptor(id).expect("descriptor");
                assert!(
                    d.dash_action().is_some() || d.menu.is_some(),
                    "{} is activatable in the palette but nothing runs it",
                    d.label
                );
            }
        }
        let notes: Vec<&ActionDescriptor> = ACTIONS.iter().filter(|d| d.informational()).collect();
        assert_eq!(notes.len(), 2, "only the wheel and the Esc/Enter note");
        for d in notes {
            assert!(matches!(d.id, ActionId::Wheel | ActionId::EscEnter));
        }
        let rows = palette_rows(&live_row(), "");
        let wheel = rows
            .iter()
            .find(|r| {
                matches!(
                    r,
                    PaletteRow::Note {
                        label: "scroll",
                        ..
                    }
                )
            })
            .expect("the wheel is still listed");
        assert!(!wheel.activatable() && !wheel.selectable());
    }

    fn summary_line() -> ActionContext {
        ActionContext {
            summary: true,
            ..ActionContext::default()
        }
    }

    /// Issue #354 phase 5: the summary line is a valid target for `inspect`
    /// (and for the menu that offers it); every per-session action is listed
    /// there with a reason rather than hidden or, worse, enabled.
    #[test]
    fn the_summary_line_offers_inspect_and_explains_every_session_action() {
        let ctx = summary_line();
        assert_eq!(
            (descriptor(ActionId::Inspect).unwrap().availability)(&ctx),
            Availability::Enabled
        );
        assert_eq!(
            (descriptor(ActionId::ContextActions).unwrap().availability)(&ctx),
            Availability::Enabled
        );
        for id in [
            ActionId::Focus,
            ActionId::Nudge,
            ActionId::Handover,
            ActionId::Stop,
            ActionId::Restore,
            ActionId::OpenWorktree,
            ActionId::Evidence,
            ActionId::Retry,
            ActionId::Dismiss,
        ] {
            assert_eq!(
                (descriptor(id).unwrap().availability)(&ctx),
                Availability::Disabled(MENU_SUMMARY_LINE),
                "{id:?} must be inert on the summary line, with a reason"
            );
        }
        // The menu still lists every entry, and the header offers inspect.
        assert_eq!(menu_actions(&ctx).len(), 11);
        assert_eq!(
            header_ids(&ctx),
            vec![
                ActionId::Inspect,
                ActionId::ContextActions,
                ActionId::Mail,
                ActionId::Help
            ]
        );
        // Nothing selected at all is still "does not apply", not "inert".
        assert_eq!(
            (descriptor(ActionId::Focus).unwrap().availability)(&ActionContext::default()),
            Availability::Hidden
        );
    }

    /// Every descriptor that has a chord can be run from the palette, and
    /// every one that does not is a context-menu row action instead -- there
    /// is no third kind, and nothing in the table is unreachable.
    #[test]
    fn every_descriptor_is_reachable_by_a_chord_or_a_menu_entry() {
        for descriptor in ACTIONS {
            let runnable = descriptor.dash_action().is_some() || descriptor.menu.is_some();
            let informational = matches!(descriptor.id, ActionId::Wheel | ActionId::EscEnter);
            assert!(
                runnable || informational,
                "{} is in the table but nothing can run it",
                descriptor.label
            );
            if !descriptor.chord.is_empty() && !informational {
                assert!(
                    !descriptor.checks.is_empty(),
                    "{} draws a chord with no filter_key check behind it",
                    descriptor.label
                );
            }
        }
    }
}
