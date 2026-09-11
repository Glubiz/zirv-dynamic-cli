//! [`HarnessBackend`]: a [`RuntimeBackend`] facade over the EXISTING
//! harness-process supervision code -- `adapters::AgentAdapter` and
//! `supervise::spawn_tapped` -- not a new spawn path. Every spawn this
//! backend performs goes through the same chokepoint `exec.rs`'s own
//! supervision loop already uses.
//!
//! Scope decision: `submit`/`resume`-with-input/`subscribe` all need the
//! working directory a session was started in, and the trait's own
//! `SessionHandle` deliberately carries no `cwd` (see `runtime::mod`'s own
//! doc comment on why backend/role/route/model/surface stay separate,
//! narrow fields). This backend tracks `cwd` itself, keyed by session, in
//! the same per-session table that holds the live child -- populated by
//! `start`. A caller that asks this backend to act on a session it never
//! itself `start`ed (a resume against a fresh backend instance with an
//! empty table) gets `RuntimeError::UnknownSession` rather than a guessed
//! cwd: consistent with every other "never guess" resume rule in this
//! crate (see `AgentAdapter::resume_target`'s own doc comment).
//!
//! Generation: `TrackedSession` also carries this backend's own record of
//! the session's current generation (starts at 1 on `start`, bumped on
//! `resume`). `submit`/`steer`/`interrupt`/`subscribe` all refuse a handle
//! whose `generation` is below the tracked one with
//! `RuntimeError::StaleGeneration` before doing anything else -- a command
//! issued against a handle a `resume` has since superseded must never be
//! silently answered by the new generation.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Child;
use std::time::Duration;

use super::protocol::{EventEnvelope, PROTOCOL_VERSION, RuntimeEvent};
use super::{
    BackendConversationRef, RuntimeBackend, RuntimeCapabilities, RuntimeError, RuntimeKind,
    SessionHandle, SessionSpec,
};
use crate::commands::ctx::CtxResult;
use crate::commands::ctx::adapters::AgentAdapter;
use crate::commands::ctx::event::{NormalizedEvent, ProviderErrorClass, SessionId, SessionRef};
use crate::commands::ctx::supervise::{self, ChildGuard};
use crate::commands::ctx::{config, sessions, state::StateDir};

/// Everything this backend needs to remember about one session between
/// calls: the live child (if any is currently running), the cwd it was
/// launched in, and this backend's own record of its current generation --
/// none of which travel on [`SessionHandle`] itself.
#[derive(Debug)]
struct TrackedSession {
    cwd: PathBuf,
    child: Option<Child>,
    guard: Option<ChildGuard>,
    generation: u64,
}

/// A [`RuntimeBackend`] over the existing harness-process code. One
/// instance per adapter; sessions this instance has `start`ed (or resumed
/// with a live tracked entry) stay in `tracked` for the life of this
/// backend.
#[derive(Debug)]
pub(crate) struct HarnessBackend {
    adapter: Box<dyn AgentAdapter>,
    tracked: HashMap<String, TrackedSession>,
}

impl HarnessBackend {
    pub(crate) fn new(adapter: Box<dyn AgentAdapter>) -> Self {
        Self {
            adapter,
            tracked: HashMap::new(),
        }
    }

    /// Whether `logical_id`'s tracked child is still running. `false` for an
    /// untracked session, an entry with no child at all, and a child whose
    /// exit status could not be read (best-effort, like every other
    /// liveness check in this crate: a supervision failure must never be
    /// the reason a caller gets stuck permanently `Busy`).
    fn is_running(&mut self, logical_id: &str) -> bool {
        let Some(entry) = self.tracked.get_mut(logical_id) else {
            return false;
        };
        let Some(child) = entry.child.as_mut() else {
            return false;
        };
        matches!(child.try_wait(), Ok(None))
    }

    /// Refuses `session` when this backend's own tracked generation for it
    /// has since moved past the handle's own (a `resume` superseded it).
    /// `Ok(())` for an untracked session too -- that is an `UnknownSession`
    /// case, not a stale one, and an untracked session is left to the caller,
    /// which refuses it downstream with `UnknownSession`, except `steer`,
    /// which returns `Unsupported` before any lookup.
    fn ensure_current_generation(&self, session: &SessionHandle) -> CtxResult<()> {
        let Some(entry) = self.tracked.get(&session.logical_id) else {
            return Ok(());
        };
        if session.generation < entry.generation {
            return Err(RuntimeError::StaleGeneration {
                expected: entry.generation,
                got: session.generation,
            }
            .into());
        }
        Ok(())
    }
}

impl RuntimeBackend for HarnessBackend {
    fn kind(&self) -> RuntimeKind {
        RuntimeKind::Harness
    }

    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities {
            // Headless harness processes take exactly one prompt per
            // process: there is no verified mechanism to inject a second
            // instruction into an already-running one.
            steer: false,
            interrupt: true,
            resume: self.adapter.resume_args("probe").is_some(),
            events: self.adapter.capabilities().events,
            surfaces: vec![super::UiSurface::Headless],
        }
    }

    fn start(&mut self, spec: &SessionSpec) -> CtxResult<SessionHandle> {
        let session_id = SessionId::new_v4();
        let mut command = self
            .adapter
            .headless_cmd(&spec.prompt, &session_id, &spec.extra_args);
        command.current_dir(&spec.cwd);
        let (child, _tap, guard) = supervise::spawn_tapped(command, None)?;
        let logical_id = session_id.as_str().to_string();
        let short = sessions::short_id(&logical_id);
        self.tracked.insert(
            logical_id.clone(),
            TrackedSession {
                cwd: spec.cwd.clone(),
                child: Some(child),
                guard: Some(guard),
                generation: 1,
            },
        );
        Ok(SessionHandle {
            runtime: RuntimeKind::Harness,
            logical_id: logical_id.clone(),
            short,
            generation: 1,
            role: spec.role.clone(),
            surface: spec.surface,
            conversation: Some(BackendConversationRef {
                agent: self.adapter.name().to_string(),
                conversation: logical_id,
            }),
        })
    }

    fn submit(&mut self, session: &SessionHandle, input: &str) -> CtxResult<()> {
        self.ensure_current_generation(session)?;
        if self.is_running(&session.logical_id) {
            return Err(RuntimeError::Busy(session.logical_id.clone()).into());
        }
        let conversation = session
            .conversation
            .as_ref()
            .ok_or_else(|| RuntimeError::UnknownSession(session.logical_id.clone()))?;
        let Some(mut command) =
            self.adapter
                .headless_resume_cmd(Some(input), &conversation.conversation, &[])
        else {
            return Err(RuntimeError::Unsupported(format!(
                "{} cannot resume a headless conversation",
                self.adapter.name()
            ))
            .into());
        };
        let entry = self
            .tracked
            .get_mut(&session.logical_id)
            .ok_or_else(|| RuntimeError::UnknownSession(session.logical_id.clone()))?;
        command.current_dir(&entry.cwd);
        let (child, _tap, guard) = supervise::spawn_tapped(command, None)?;
        entry.child = Some(child);
        entry.guard = Some(guard);
        Ok(())
    }

    fn steer(&mut self, session: &SessionHandle, _input: &str) -> CtxResult<()> {
        self.ensure_current_generation(session)?;
        Err(RuntimeError::Unsupported(
            "headless harness processes take one prompt per process; steering mid-turn is not supported"
                .to_string(),
        )
        .into())
    }

    fn interrupt(&mut self, session: &SessionHandle) -> CtxResult<()> {
        self.ensure_current_generation(session)?;
        let entry = self
            .tracked
            .get_mut(&session.logical_id)
            .ok_or_else(|| RuntimeError::UnknownSession(session.logical_id.clone()))?;
        match entry.child.as_mut() {
            // The existing terminate() ladder (SIGTERM, then SIGKILL after
            // the same 5s grace `supervise_child`'s own timeout/stop arms
            // already use) -- never a new kill ladder.
            Some(child) => supervise::terminate(child, Duration::from_secs(5)),
            None => Ok(()),
        }
    }

    fn resume(&mut self, session: &SessionHandle, input: Option<&str>) -> CtxResult<SessionHandle> {
        let env = config::env_from_process();
        let state = StateDir::resolve(&env)?;
        let Some(conversation) = sessions::native_conversation(
            &state,
            &session.short,
            self.adapter.name(),
            &session.logical_id,
            RuntimeKind::Harness,
        ) else {
            return Err(RuntimeError::UnknownSession(session.logical_id.clone()).into());
        };
        // Bumped from THIS backend's own tracked record, never from
        // `session.generation`: a caller resuming with an already-stale
        // handle must still land on the true next generation, not one
        // derived from the stale value it handed in. An untracked session
        // (never `start`ed by this instance) has no tracked generation to
        // bump from and keeps the handle's own.
        let generation = match self.tracked.get_mut(&session.logical_id) {
            Some(entry) => {
                entry.generation += 1;
                entry.generation
            }
            None => session.generation,
        };
        let handle = SessionHandle {
            generation,
            conversation: Some(BackendConversationRef {
                agent: self.adapter.name().to_string(),
                conversation,
            }),
            ..session.clone()
        };
        if let Some(input) = input {
            self.submit(&handle, input)?;
        }
        Ok(handle)
    }

    fn subscribe(
        &mut self,
        session: &SessionHandle,
        after_revision: u64,
    ) -> CtxResult<Vec<EventEnvelope>> {
        self.ensure_current_generation(session)?;
        let entry = self
            .tracked
            .get(&session.logical_id)
            .ok_or_else(|| RuntimeError::UnknownSession(session.logical_id.clone()))?;
        let session_ref = SessionRef {
            id: SessionId::parse(&session.logical_id),
            cwd: entry.cwd.clone(),
        };
        let path = self.adapter.transcript_path(&session_ref);
        let jsonl = std::fs::read_to_string(&path).unwrap_or_default();
        let normalized = self.adapter.parse_events(&jsonl);
        let projected = project(&normalized);
        Ok(projected
            .into_iter()
            .enumerate()
            .map(|(index, event)| EventEnvelope {
                version: PROTOCOL_VERSION,
                revision: index as u64 + 1,
                session: session.logical_id.clone(),
                generation: session.generation,
                event,
            })
            .filter(|envelope| envelope.revision > after_revision)
            .collect())
    }
}

/// Pure projection from the harness's own normalized transcript events to
/// the wire protocol's event vocabulary. Everything `NormalizedEvent` emits
/// that has no wire counterpart (usage/timing/breakdown signals rot and
/// score consume directly) is skipped rather than forced into a lossy
/// mapping.
pub(crate) fn project(events: &[NormalizedEvent]) -> Vec<RuntimeEvent> {
    let mut out = Vec::new();
    for event in events {
        match event {
            NormalizedEvent::TurnStart { .. } => out.push(RuntimeEvent::TurnStarted),
            NormalizedEvent::AssistantFinal { text, .. } => out.push(RuntimeEvent::TurnCompleted {
                final_text: Some(text.clone()),
            }),
            NormalizedEvent::ToolCall { name, .. } => {
                out.push(RuntimeEvent::ToolCall { name: name.clone() })
            }
            NormalizedEvent::ToolResult { is_error } => out.push(RuntimeEvent::ToolResult {
                is_error: *is_error,
            }),
            NormalizedEvent::ProviderError { class, .. } => out.push(RuntimeEvent::Failed {
                class: provider_error_class_str(class),
                message: String::new(),
            }),
            _ => {}
        }
    }
    out
}

fn provider_error_class_str(class: &ProviderErrorClass) -> String {
    format!("{class:?}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::event::{Capabilities, StructuralContext};

    fn normalized_script() -> Vec<NormalizedEvent> {
        vec![
            NormalizedEvent::TurnStart { at_ms: None },
            NormalizedEvent::ToolCall {
                name: "Read".to_string(),
                input_hash: 0,
                at_ms: None,
            },
            NormalizedEvent::ToolResult { is_error: false },
            NormalizedEvent::UserText { byte_len: 12 },
            NormalizedEvent::AssistantFinal {
                text: "done".to_string(),
                input_tokens: 42,
                at_ms: None,
            },
            NormalizedEvent::ProviderError {
                class: ProviderErrorClass::RateLimit,
                at: None,
                id: None,
            },
        ]
    }

    #[test]
    fn project_maps_every_known_variant_and_skips_the_rest() {
        let projected = project(&normalized_script());
        assert_eq!(
            projected,
            vec![
                RuntimeEvent::TurnStarted,
                RuntimeEvent::ToolCall {
                    name: "Read".to_string()
                },
                RuntimeEvent::ToolResult { is_error: false },
                RuntimeEvent::TurnCompleted {
                    final_text: Some("done".to_string())
                },
                RuntimeEvent::Failed {
                    class: "RateLimit".to_string(),
                    message: String::new(),
                },
            ],
            "UserText has no wire counterpart and must be skipped, never forced into a lossy mapping"
        );
    }

    #[test]
    fn project_of_an_empty_slice_is_empty() {
        assert!(project(&[]).is_empty());
    }

    #[derive(Debug)]
    struct StubAdapter {
        transcript: PathBuf,
    }

    impl AgentAdapter for StubAdapter {
        fn name(&self) -> &'static str {
            "stub"
        }

        fn program(&self) -> &str {
            "stub"
        }

        fn provider(&self) -> &'static str {
            "stub"
        }

        fn ready(&self) -> CtxResult<()> {
            Ok(())
        }

        fn detect(&self, _command: &[String]) -> bool {
            false
        }

        fn headless_cmd(
            &self,
            _prompt: &str,
            _session: &SessionId,
            _extra: &[String],
        ) -> std::process::Command {
            // Claude-style: one JSON object per line. Written by the child
            // itself so `start -> wait for exit -> subscribe` exercises a
            // real spawn through `supervise::spawn_tapped`, not a stub.
            let mut cmd = std::process::Command::new("sh");
            cmd.arg("-c").arg(format!(
                "printf '%s\\n' '{{\"type\":\"assistant\",\"text\":\"hello from the stub\"}}' > {}",
                self.transcript.display()
            ));
            cmd
        }

        fn interactive_cmd(
            &self,
            _initial_prompt: Option<&str>,
            _extra: &[String],
        ) -> std::process::Command {
            std::process::Command::new("true")
        }

        fn distiller_cmd(&self, _model: &str) -> std::process::Command {
            std::process::Command::new("true")
        }

        fn read_only_args(&self) -> Vec<String> {
            Vec::new()
        }

        fn system_prompt_args(&self, _prompt: &str) -> Vec<String> {
            Vec::new()
        }

        fn transcript_path(&self, _session: &SessionRef) -> PathBuf {
            self.transcript.clone()
        }

        fn parse_events(&self, jsonl: &str) -> Vec<NormalizedEvent> {
            jsonl
                .lines()
                .filter(|line| line.contains("\"type\":\"assistant\""))
                .map(|_| NormalizedEvent::AssistantFinal {
                    text: "hello from the stub".to_string(),
                    input_tokens: 0,
                    at_ms: None,
                })
                .collect()
        }

        fn structural_context(&self, _jsonl: &str, _last_n: usize) -> StructuralContext {
            StructuralContext::default()
        }

        fn compact_command(&self) -> Option<&'static str> {
            None
        }

        fn quit_sequence(&self) -> &'static str {
            ""
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }

        fn register_turn_signal(
            &self,
            _session: &SessionRef,
            _socket: &std::path::Path,
        ) -> crate::commands::ctx::adapters::TurnSignalSetup {
            crate::commands::ctx::adapters::TurnSignalSetup {
                env: Vec::new(),
                instructions: String::new(),
            }
        }
    }

    /// `start -> wait for the child to exit -> subscribe` through a real
    /// `supervise::spawn_tapped` child (not a stubbed-out one), proving the
    /// whole facade -- not just `project`'s own pure mapping -- actually
    /// wires up end to end.
    #[test]
    #[cfg(unix)]
    fn start_wait_and_subscribe_yields_a_turn_completed_event() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let transcript = tmp.path().join("transcript.jsonl");
        let adapter = StubAdapter {
            transcript: transcript.clone(),
        };
        let mut backend = HarnessBackend::new(Box::new(adapter));
        let spec = SessionSpec {
            runtime: RuntimeKind::Harness,
            role: "worker".to_string(),
            agent: None,
            provider_route: None,
            model: None,
            surface: super::super::UiSurface::Headless,
            cwd: tmp.path().to_path_buf(),
            prompt: "hi".to_string(),
            extra_args: Vec::new(),
        };
        let handle = backend.start(&spec).expect("start");

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !transcript.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "the stub child never wrote its transcript"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        // Let the child fully exit so a concurrent `is_running` read (none
        // happens in this test, but a real caller's would) sees it clearly.
        if let Some(entry) = backend.tracked.get_mut(&handle.logical_id)
            && let Some(child) = entry.child.as_mut()
        {
            let _ = child.wait();
        }

        let events = backend.subscribe(&handle, 0).expect("subscribe");
        assert!(
            events.iter().any(|envelope| matches!(
                &envelope.event,
                RuntimeEvent::TurnCompleted { final_text: Some(text) }
                    if text == "hello from the stub"
            )),
            "expected a TurnCompleted event, got {events:?}"
        );
    }

    /// A session this backend `start`ed, then `resume`d with a marker
    /// `sessions::native_conversation` can find. `resume` reads the real
    /// process environment via `config::env_from_process` (not a
    /// test-injectable `EnvLookup` closure), so `ZIRV_CTX_STATE_DIR` has to
    /// be a genuine `std::env::set_var` here -- the same real-env pattern
    /// `agent.rs`'s own `FAKE_AGENT_*` fixtures already use.
    fn started_and_marked_for_resume(
        tmp: &std::path::Path,
    ) -> (HarnessBackend, SessionHandle, tempfile::TempDir) {
        let state_dir = tempfile::tempdir().expect("state tempdir");
        unsafe {
            std::env::set_var("ZIRV_CTX_STATE_DIR", state_dir.path());
        }
        let env = crate::commands::ctx::config::env_from_process();
        let state = crate::commands::ctx::state::StateDir::resolve(&env).expect("state dir");

        let adapter = StubAdapter {
            transcript: tmp.join("transcript.jsonl"),
        };
        let mut backend = HarnessBackend::new(Box::new(adapter));
        let spec = SessionSpec {
            runtime: RuntimeKind::Harness,
            role: "worker".to_string(),
            agent: None,
            provider_route: None,
            model: None,
            surface: super::super::UiSurface::Headless,
            cwd: tmp.to_path_buf(),
            prompt: "hi".to_string(),
            extra_args: Vec::new(),
        };
        let handle = backend.start(&spec).expect("start");
        crate::commands::ctx::sessions::record_native_conversation(
            &state,
            &handle.short,
            "stub",
            &handle.logical_id,
            "native-conv-after-resume",
        );
        (backend, handle, state_dir)
    }

    /// `resume` bumps THIS backend's own tracked generation (never derived
    /// from the caller's own, possibly-stale, handle) and keeps every other
    /// identity field -- `runtime::SessionHandle::attached`'s own "changes
    /// only surface" rule has a sibling here: `resume` changes only
    /// `generation` and `conversation`.
    #[test]
    #[cfg(unix)]
    fn resume_bumps_the_generation_and_keeps_identity() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (mut backend, original, _state_dir) = started_and_marked_for_resume(tmp.path());

        let resumed = backend.resume(&original, None).expect("resume");
        unsafe {
            std::env::remove_var("ZIRV_CTX_STATE_DIR");
        }

        assert_eq!(resumed.generation, 2);
        assert_eq!(resumed.logical_id, original.logical_id);
        assert_eq!(resumed.short, original.short);
        assert_eq!(resumed.role, original.role);
    }

    /// A handle a `resume` has since superseded is refused on `submit` and
    /// `subscribe` -- the enforcement `runtime::mod`'s own doc comment
    /// promises and `fake::FakeNativeBackend` already implements, now
    /// enforced by the harness facade too.
    #[test]
    #[cfg(unix)]
    fn a_pre_resume_handle_is_refused_with_stale_generation_on_submit_and_subscribe() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (mut backend, original, _state_dir) = started_and_marked_for_resume(tmp.path());

        backend.resume(&original, None).expect("resume");
        unsafe {
            std::env::remove_var("ZIRV_CTX_STATE_DIR");
        }

        let submit_error = backend
            .submit(&original, "too late")
            .expect_err("a pre-resume handle must be refused");
        assert_eq!(
            submit_error.downcast_ref::<RuntimeError>(),
            Some(&RuntimeError::StaleGeneration {
                expected: 2,
                got: 1
            })
        );

        let subscribe_error = backend
            .subscribe(&original, 0)
            .expect_err("a pre-resume handle must be refused");
        assert_eq!(
            subscribe_error.downcast_ref::<RuntimeError>(),
            Some(&RuntimeError::StaleGeneration {
                expected: 2,
                got: 1
            })
        );
    }
}
