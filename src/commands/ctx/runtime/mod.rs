//! Shared runtime contracts (issue #470): the seam between zirv's own
//! supervision code and whichever backend actually drives an agent
//! conversation. Today the only real backend is [`harness::HarnessBackend`],
//! a facade over the EXISTING harness-process supervision code
//! (`adapters::AgentAdapter`, `supervise::spawn_tapped`) -- this issue adds
//! no new spawn path, only the contract every future backend (issue #469's
//! native runtime, steps N02-N09) will also implement.
//!
//! [`RuntimeKind`] (which backend), `role`/`provider_route`/`model`
//! (`SessionSpec`'s own fields), and [`UiSurface`] (which surface is
//! attached) are deliberately kept as separate fields throughout this
//! module rather than folded into one label: a session's backend, the seat
//! role it was spawned under, the account route it spends, the model it
//! runs, and which UI is currently looking at it are five independent axes
//! that a future caller needs to reason about independently (e.g. attaching
//! a dashboard pane to a session that keeps running headless underneath it
//! changes only `surface`).
//!
//! `protocol.rs` is the versioned wire shape over this trait; `fake.rs` is a
//! deterministic in-memory backend later roadmap steps and built-in checks
//! can run against without spawning anything real; `harness.rs` is the one
//! production backend this issue ships.
//!
//! Nothing in the binary calls into this module outside its own tests yet:
//! wiring a live caller through it (`zirv ctx exec`/`dash`, and the native
//! backend itself) is later roadmap work (issue #469, steps N02-N09).
//! `#![allow(dead_code)]` covers it until then, the same reasoning
//! `dash::pane`'s and `transcript_source`'s own module doc comments already
//! document: a real, fully-tested API with no in-tree caller yet is not the
//! same thing as code that should be deleted.
#![allow(dead_code)]

pub mod fake;
pub mod harness;
pub mod protocol;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use self::protocol::EventEnvelope;
use super::CtxResult;
use super::adapters::AgentAdapter;
use super::provider::RouteId;

/// Which backend drives a session's own conversation. `Unknown` is the
/// forward-compat fallback for a value a future build wrote that this one
/// has never heard of -- never a guess at `Harness`, which would silently
/// let a native-only session be treated as one a harness process can be
/// spawned/resumed for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeKind {
    #[default]
    Harness,
    Native,
    #[serde(other)]
    Unknown,
}

impl RuntimeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            RuntimeKind::Harness => "harness",
            RuntimeKind::Native => "native",
            RuntimeKind::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for RuntimeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for RuntimeKind {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "harness" => RuntimeKind::Harness,
            "native" => RuntimeKind::Native,
            _ => RuntimeKind::Unknown,
        })
    }
}

/// Which UI is currently attached to a session, independent of which
/// backend runs it -- a headless launch, an interactive terminal, or a
/// dashboard pane can all sit in front of either an `Harness` or `Native`
/// session. `Unknown` is the same forward-compat fallback `RuntimeKind`
/// documents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UiSurface {
    #[default]
    Headless,
    Terminal,
    DashboardPane,
    #[serde(other)]
    Unknown,
}

impl UiSurface {
    pub fn as_str(&self) -> &'static str {
        match self {
            UiSurface::Headless => "headless",
            UiSurface::Terminal => "terminal",
            UiSurface::DashboardPane => "dashboardpane",
            UiSurface::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for UiSurface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for UiSurface {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "headless" => UiSurface::Headless,
            "terminal" => UiSurface::Terminal,
            "dashboardpane" => UiSurface::DashboardPane,
            _ => UiSurface::Unknown,
        })
    }
}

/// Everything a [`RuntimeBackend::start`] needs to launch one session.
/// Backend, seat role, provider route, model and UI surface are separate
/// fields on purpose -- see this module's own doc comment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSpec {
    #[serde(default)]
    pub runtime: RuntimeKind,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub provider_route: Option<RouteId>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub surface: UiSurface,
    pub cwd: PathBuf,
    pub prompt: String,
    #[serde(default)]
    pub extra_args: Vec<String>,
}

/// An opaque, per-backend continuation reference -- e.g. the harness's own
/// conversation id (`sessions::native_conversation`'s value) for
/// [`harness::HarnessBackend`]. Never interpreted outside the backend that
/// produced it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendConversationRef {
    pub agent: String,
    pub conversation: String,
}

/// A live session, as a caller needs to keep referring to it across calls.
/// `logical_id` is the zirv session uuid (`event::SessionId`'s own string
/// form); `short` is the existing short id; `generation` is the seat
/// generation (`seat::Seat::generation`) this handle was minted at -- a
/// `resume` bumps it, and a command issued against a handle whose
/// generation has since gone stale must be refused rather than silently
/// answered by the new one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHandle {
    pub runtime: RuntimeKind,
    pub logical_id: String,
    pub short: String,
    pub generation: u64,
    pub role: String,
    pub surface: UiSurface,
    #[serde(default)]
    pub conversation: Option<BackendConversationRef>,
}

impl SessionHandle {
    /// Changes ONLY `surface` -- attaching a different UI to an already-live
    /// session must never touch its identity, generation, role or backend
    /// conversation reference.
    pub fn attached(mut self, surface: UiSurface) -> Self {
        self.surface = surface;
        self
    }
}

/// Which rot/dashboard-relevant operations a backend actually supports, so a
/// caller can degrade gracefully instead of calling and catching
/// `RuntimeError::Unsupported`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RuntimeCapabilities {
    pub steer: bool,
    pub interrupt: bool,
    pub resume: bool,
    pub events: bool,
    #[serde(default)]
    pub surfaces: Vec<UiSurface>,
}

/// The seam every conversation-driving backend implements: today only
/// [`harness::HarnessBackend`] (a facade over the existing harness-process
/// code) and, for tests and later roadmap steps, [`fake::FakeNativeBackend`].
pub trait RuntimeBackend: std::fmt::Debug {
    fn kind(&self) -> RuntimeKind;
    fn capabilities(&self) -> RuntimeCapabilities;
    fn start(&mut self, spec: &SessionSpec) -> CtxResult<SessionHandle>;
    fn submit(&mut self, session: &SessionHandle, input: &str) -> CtxResult<()>;
    fn steer(&mut self, session: &SessionHandle, input: &str) -> CtxResult<()>;
    fn interrupt(&mut self, session: &SessionHandle) -> CtxResult<()>;
    fn resume(&mut self, session: &SessionHandle, input: Option<&str>) -> CtxResult<SessionHandle>;
    fn subscribe(
        &mut self,
        session: &SessionHandle,
        after_revision: u64,
    ) -> CtxResult<Vec<EventEnvelope>>;
}

/// The runtime-contract-specific failures a [`RuntimeBackend`] reports, on
/// top of whatever a concrete backend's own I/O already returns as a plain
/// boxed error. [`protocol::dispatch`] downcasts for exactly these four to
/// pick a structured [`protocol::ErrorCode`]; anything else maps to
/// `ErrorCode::Backend`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeError {
    /// This backend/operation combination is not implemented -- e.g.
    /// `HarnessBackend::steer`, or `select(RuntimeKind::Native, ..)` before
    /// issue #469's later roadmap steps land.
    Unsupported(String),
    /// `session` names no session this backend instance currently tracks.
    UnknownSession(String),
    /// A turn is already in flight for this session; the caller must wait
    /// for it to finish (or `interrupt`) before submitting another.
    Busy(String),
    /// `session`'s generation no longer matches the backend's own record for
    /// it -- a command issued against a handle a `resume` has since
    /// superseded.
    StaleGeneration { expected: u64, got: u64 },
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RuntimeError::Unsupported(what) => write!(f, "unsupported: {what}"),
            RuntimeError::UnknownSession(id) => write!(f, "unknown session: {id}"),
            RuntimeError::Busy(id) => write!(f, "session busy: {id}"),
            RuntimeError::StaleGeneration { expected, got } => {
                write!(f, "stale generation: expected {expected}, got {got}")
            }
        }
    }
}

impl std::error::Error for RuntimeError {}

/// Picks the backend for `kind`. `Harness` needs a real adapter (this is
/// where the existing harness code is actually reached); `Native` and
/// `Unknown` both fail closed today -- issue #469's later roadmap steps
/// (N02-N09) are what will make `Native` succeed.
///
/// [`fake::FakeNativeBackend`] is deliberately not reachable through this
/// function: it exists for tests and for later roadmap steps that want a
/// deterministic stand-in, and both select it directly as a
/// `Box<dyn RuntimeBackend>`.
pub fn select(
    kind: RuntimeKind,
    adapter: Option<Box<dyn AgentAdapter>>,
) -> CtxResult<Box<dyn RuntimeBackend>> {
    match kind {
        RuntimeKind::Harness => {
            let adapter = adapter.ok_or_else(|| {
                Box::new(RuntimeError::Unsupported(
                    "harness runtime requires an adapter".to_string(),
                )) as Box<dyn std::error::Error>
            })?;
            Ok(Box::new(harness::HarnessBackend::new(adapter)))
        }
        RuntimeKind::Native => Err(RuntimeError::Unsupported(
            "native runtime is not available yet (roadmap #469, steps N02-N09)".to_string(),
        )
        .into()),
        RuntimeKind::Unknown => {
            Err(RuntimeError::Unsupported("unknown runtime kind".to_string()).into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_kind_round_trips_through_display_and_from_str() {
        for kind in [RuntimeKind::Harness, RuntimeKind::Native] {
            let parsed: RuntimeKind = kind.to_string().parse().expect("infallible");
            assert_eq!(parsed, kind);
        }
        assert_eq!("garbage".parse::<RuntimeKind>(), Ok(RuntimeKind::Unknown));
    }

    #[test]
    fn ui_surface_round_trips_through_display_and_from_str() {
        for surface in [
            UiSurface::Headless,
            UiSurface::Terminal,
            UiSurface::DashboardPane,
        ] {
            let parsed: UiSurface = surface.to_string().parse().expect("infallible");
            assert_eq!(parsed, surface);
        }
        assert_eq!("garbage".parse::<UiSurface>(), Ok(UiSurface::Unknown));
    }

    #[test]
    fn attached_changes_only_the_surface() {
        let handle = SessionHandle {
            runtime: RuntimeKind::Harness,
            logical_id: "session-1".to_string(),
            short: "abcd1234".to_string(),
            generation: 3,
            role: "orchestrator".to_string(),
            surface: UiSurface::Headless,
            conversation: Some(BackendConversationRef {
                agent: "claude".to_string(),
                conversation: "conv-1".to_string(),
            }),
        };
        let attached = handle.clone().attached(UiSurface::DashboardPane);
        assert_eq!(attached.surface, UiSurface::DashboardPane);
        assert_eq!(attached.logical_id, handle.logical_id);
        assert_eq!(attached.short, handle.short);
        assert_eq!(attached.generation, handle.generation);
        assert_eq!(attached.role, handle.role);
        assert_eq!(attached.conversation, handle.conversation);
    }

    #[test]
    fn select_harness_without_an_adapter_is_a_clear_error() {
        let error = select(RuntimeKind::Harness, None).expect_err("no adapter");
        assert!(error.to_string().contains("requires an adapter"));
    }

    #[test]
    fn select_native_names_the_roadmap_issue() {
        let error = select(RuntimeKind::Native, None).expect_err("native not ready");
        assert!(error.to_string().contains("#469"));
    }

    #[test]
    fn select_unknown_is_an_error() {
        assert!(select(RuntimeKind::Unknown, None).is_err());
    }
}
