//! `zirv chat`'s session multiplexer: a dashboard process owning N
//! interactive ConPTY harness sessions, each rendered through its own
//! embedded `vt100` screen model.
//!
//! Module shell only -- the event loop (`run_dashboard`, the prefix-key
//! filter, zoom, quit) arrives in the plan's Task 5; this task wires in the
//! pane primitive alone (`pane::Pane`, the supervised ConPTY child behind a
//! `vt100::Screen`) so Task 4's pure renderers and Task 5's event loop have
//! something to render and drive.

pub mod pane;

// Nothing in the binary constructs a `Pane` yet -- Task 4's renderers take a
// `&vt100::Screen`/`&PaneState` and Task 5's event loop is what builds a
// `PaneSpec` and calls `Pane::spawn`. Re-exported now so those tasks import
// from `dash::` rather than reaching into `dash::pane::` directly.
#[allow(unused_imports)]
pub(crate) use pane::{Pane, PaneSpec, PaneState};
