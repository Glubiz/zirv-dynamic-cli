//! A deterministic, fully in-memory `Native` [`RuntimeBackend`] -- no clock,
//! no randomness, no filesystem, no process spawn, no environment reads.
//! Every id and every event is derived purely from an internal counter and
//! the calls actually made, so the same call sequence always produces
//! byte-identical output.
//!
//! Production-compiled (no `cfg(test)`) because later roadmap steps (issue
//! #469) and built-in checks need a real, selectable `Native` backend to run
//! against before a genuine one exists -- see [`FakeNativeBackend`]'s own
//! doc comment.

use std::collections::HashMap;

use super::protocol::{EventEnvelope, PROTOCOL_VERSION, RuntimeEvent};
use super::{
    BackendConversationRef, RuntimeBackend, RuntimeCapabilities, RuntimeError, RuntimeKind,
    SessionHandle, SessionSpec, UiSurface,
};
use crate::commands::ctx::CtxResult;

#[derive(Debug, Clone)]
struct FakeSession {
    short: String,
    generation: u64,
    role: String,
    surface: UiSurface,
    events: Vec<EventEnvelope>,
}

impl FakeSession {
    fn push(&mut self, logical_id: &str, event: RuntimeEvent) {
        let revision = self.events.len() as u64 + 1;
        self.events.push(EventEnvelope {
            version: PROTOCOL_VERSION,
            revision,
            session: logical_id.to_string(),
            generation: self.generation,
            event,
        });
    }
}

/// A deterministic stand-in for a real `Native` backend. Selected directly
/// as a `Box<dyn RuntimeBackend>` (never through [`super::select`], which
/// only ever resolves `Harness` today) by tests and by any later roadmap
/// step that wants something real to run a workflow against before issue
/// #469's genuine native backend lands.
#[derive(Debug, Default)]
pub(crate) struct FakeNativeBackend {
    next_id: u64,
    sessions: HashMap<String, FakeSession>,
}

impl FakeNativeBackend {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn mint_id(&mut self) -> String {
        self.next_id += 1;
        format!("fake-{}", self.next_id)
    }

    /// Looks up `session.logical_id`, refusing an unknown session and a
    /// handle whose generation has since gone stale (below the session's
    /// current one) -- the shared guard `submit`/`steer`/`interrupt` all
    /// apply. `resume` and `subscribe` do not: resuming a stale handle is
    /// exactly how a caller gets back on the current generation, and reading
    /// history through an old handle is harmless.
    fn resolve_current_mut(&mut self, session: &SessionHandle) -> CtxResult<&mut FakeSession> {
        let Some(entry) = self.sessions.get_mut(&session.logical_id) else {
            return Err(RuntimeError::UnknownSession(session.logical_id.clone()).into());
        };
        if session.generation < entry.generation {
            return Err(RuntimeError::StaleGeneration {
                expected: entry.generation,
                got: session.generation,
            }
            .into());
        }
        Ok(entry)
    }
}

impl RuntimeBackend for FakeNativeBackend {
    fn kind(&self) -> RuntimeKind {
        RuntimeKind::Native
    }

    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities {
            steer: true,
            interrupt: true,
            resume: true,
            events: true,
            surfaces: vec![
                UiSurface::Headless,
                UiSurface::Terminal,
                UiSurface::DashboardPane,
            ],
        }
    }

    fn start(&mut self, spec: &SessionSpec) -> CtxResult<SessionHandle> {
        let logical_id = self.mint_id();
        let short: String = logical_id.chars().take(8).collect();
        let mut session = FakeSession {
            short: short.clone(),
            generation: 1,
            role: spec.role.clone(),
            surface: spec.surface,
            events: Vec::new(),
        };
        let handle = SessionHandle {
            runtime: RuntimeKind::Native,
            logical_id: logical_id.clone(),
            short,
            generation: 1,
            role: spec.role.clone(),
            surface: spec.surface,
            conversation: None,
        };
        session.push(
            &logical_id,
            RuntimeEvent::Started {
                session: handle.clone(),
            },
        );
        session.push(&logical_id, RuntimeEvent::TurnStarted);
        session.push(
            &logical_id,
            RuntimeEvent::AssistantText {
                text: format!("ack: {}", spec.prompt),
            },
        );
        session.push(
            &logical_id,
            RuntimeEvent::ToolCall {
                name: "read_file".to_string(),
            },
        );
        session.push(&logical_id, RuntimeEvent::ToolResult { is_error: false });
        session.push(
            &logical_id,
            RuntimeEvent::TurnCompleted {
                final_text: Some(format!("done: {}", spec.prompt)),
            },
        );
        self.sessions.insert(logical_id, session);
        Ok(handle)
    }

    fn submit(&mut self, session: &SessionHandle, input: &str) -> CtxResult<()> {
        let logical_id = session.logical_id.clone();
        let entry = self.resolve_current_mut(session)?;
        entry.push(&logical_id, RuntimeEvent::TurnStarted);
        entry.push(
            &logical_id,
            RuntimeEvent::AssistantText {
                text: format!("ack: {input}"),
            },
        );
        entry.push(
            &logical_id,
            RuntimeEvent::TurnCompleted {
                final_text: Some(format!("done: {input}")),
            },
        );
        Ok(())
    }

    fn steer(&mut self, session: &SessionHandle, input: &str) -> CtxResult<()> {
        let logical_id = session.logical_id.clone();
        let entry = self.resolve_current_mut(session)?;
        entry.push(
            &logical_id,
            RuntimeEvent::AssistantText {
                text: format!("steer acknowledged: {input}"),
            },
        );
        Ok(())
    }

    fn interrupt(&mut self, session: &SessionHandle) -> CtxResult<()> {
        let logical_id = session.logical_id.clone();
        let entry = self.resolve_current_mut(session)?;
        entry.push(&logical_id, RuntimeEvent::Interrupted);
        Ok(())
    }

    fn resume(&mut self, session: &SessionHandle, input: Option<&str>) -> CtxResult<SessionHandle> {
        let logical_id = session.logical_id.clone();
        let Some(entry) = self.sessions.get_mut(&logical_id) else {
            return Err(RuntimeError::UnknownSession(logical_id).into());
        };
        entry.generation += 1;
        let handle = SessionHandle {
            runtime: RuntimeKind::Native,
            logical_id: logical_id.clone(),
            short: entry.short.clone(),
            generation: entry.generation,
            role: entry.role.clone(),
            surface: entry.surface,
            conversation: Some(BackendConversationRef {
                agent: "fake".to_string(),
                conversation: logical_id.clone(),
            }),
        };
        entry.push(
            &logical_id,
            RuntimeEvent::Started {
                session: handle.clone(),
            },
        );
        if let Some(input) = input {
            entry.push(&logical_id, RuntimeEvent::TurnStarted);
            entry.push(
                &logical_id,
                RuntimeEvent::AssistantText {
                    text: format!("ack: {input}"),
                },
            );
            entry.push(
                &logical_id,
                RuntimeEvent::TurnCompleted {
                    final_text: Some(format!("done: {input}")),
                },
            );
        }
        Ok(handle)
    }

    fn subscribe(
        &mut self,
        session: &SessionHandle,
        after_revision: u64,
    ) -> CtxResult<Vec<EventEnvelope>> {
        let Some(entry) = self.sessions.get(&session.logical_id) else {
            return Err(RuntimeError::UnknownSession(session.logical_id.clone()).into());
        };
        Ok(entry
            .events
            .iter()
            .filter(|event| event.revision > after_revision)
            .cloned()
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::runtime::protocol::{
        CommandEnvelope, ReplyEnvelope, RuntimeCommand, RuntimeReply, dispatch,
    };

    fn start_cmd(id: &str, prompt: &str) -> CommandEnvelope {
        CommandEnvelope {
            version: PROTOCOL_VERSION,
            id: id.to_string(),
            command: RuntimeCommand::Start {
                spec: SessionSpec {
                    runtime: RuntimeKind::Native,
                    role: "worker".to_string(),
                    agent: None,
                    provider_route: None,
                    model: None,
                    surface: UiSurface::Headless,
                    cwd: std::path::PathBuf::from("/repo"),
                    prompt: prompt.to_string(),
                    extra_args: Vec::new(),
                },
            },
        }
    }

    fn expect_session(reply: ReplyEnvelope) -> SessionHandle {
        match reply.reply {
            RuntimeReply::Session { session } => session,
            other => panic!("expected a Session reply, got {other:?}"),
        }
    }

    /// The full script -- start, submit, steer, interrupt, resume, subscribe
    /// -- driven entirely through `protocol::dispatch`, proving the fake is
    /// usable purely as a `RuntimeBackend` behind the wire protocol.
    #[test]
    fn the_full_script_runs_through_dispatch() {
        let mut backend = FakeNativeBackend::new();

        let started = expect_session(dispatch(&mut backend, &start_cmd("r1", "hello")));
        assert_eq!(started.runtime, RuntimeKind::Native);
        assert_eq!(started.generation, 1);
        assert_eq!(started.logical_id, "fake-1");

        let submit = CommandEnvelope {
            version: PROTOCOL_VERSION,
            id: "r2".to_string(),
            command: RuntimeCommand::Submit {
                session: started.clone(),
                input: "more".to_string(),
            },
        };
        match dispatch(&mut backend, &submit).reply {
            RuntimeReply::Ack => {}
            other => panic!("expected Ack, got {other:?}"),
        }

        let steer = CommandEnvelope {
            version: PROTOCOL_VERSION,
            id: "r3".to_string(),
            command: RuntimeCommand::Steer {
                session: started.clone(),
                input: "look here instead".to_string(),
            },
        };
        assert!(matches!(
            dispatch(&mut backend, &steer).reply,
            RuntimeReply::Ack
        ));

        let interrupt = CommandEnvelope {
            version: PROTOCOL_VERSION,
            id: "r4".to_string(),
            command: RuntimeCommand::Interrupt {
                session: started.clone(),
            },
        };
        assert!(matches!(
            dispatch(&mut backend, &interrupt).reply,
            RuntimeReply::Ack
        ));

        let resume = CommandEnvelope {
            version: PROTOCOL_VERSION,
            id: "r5".to_string(),
            command: RuntimeCommand::Resume {
                session: started.clone(),
                input: Some("again".to_string()),
            },
        };
        let resumed = expect_session(dispatch(&mut backend, &resume));
        assert_eq!(resumed.logical_id, started.logical_id);
        assert_eq!(resumed.short, started.short);
        assert_eq!(resumed.role, started.role);
        assert_eq!(resumed.generation, 2);
        assert_eq!(
            resumed.conversation,
            Some(BackendConversationRef {
                agent: "fake".to_string(),
                conversation: started.logical_id.clone(),
            })
        );

        let subscribe = CommandEnvelope {
            version: PROTOCOL_VERSION,
            id: "r6".to_string(),
            command: RuntimeCommand::Subscribe {
                session: resumed.clone(),
                after_revision: 0,
            },
        };
        let events = match dispatch(&mut backend, &subscribe).reply {
            RuntimeReply::Events { events } => events,
            other => panic!("expected Events, got {other:?}"),
        };
        // start (6) + submit (3) + steer (1) + interrupt (1) + resume with
        // input (1 Started + 3 submit-shaped) = 15.
        assert_eq!(events.len(), 15);
        for (index, event) in events.iter().enumerate() {
            assert_eq!(
                event.revision,
                index as u64 + 1,
                "revisions are 1-based and dense"
            );
        }
        assert!(
            events
                .iter()
                .all(|event| event.session == started.logical_id)
        );
    }

    #[test]
    fn a_stale_handle_is_refused_after_resume() {
        let mut backend = FakeNativeBackend::new();
        let original = backend
            .start(&SessionSpec {
                runtime: RuntimeKind::Native,
                role: "worker".to_string(),
                agent: None,
                provider_route: None,
                model: None,
                surface: UiSurface::Headless,
                cwd: std::path::PathBuf::from("/repo"),
                prompt: "hi".to_string(),
                extra_args: Vec::new(),
            })
            .expect("start");
        backend.resume(&original, None).expect("resume");

        let error = backend
            .submit(&original, "too late")
            .expect_err("stale handle must be refused");
        assert_eq!(
            error.downcast_ref::<RuntimeError>(),
            Some(&RuntimeError::StaleGeneration {
                expected: 2,
                got: 1
            })
        );
    }

    /// Attaching a different UI surface changes only `surface`; the session
    /// stays reachable under the same identity, and events recorded through
    /// the attached handle still carry the same `session`/`generation`.
    #[test]
    fn attaching_a_surface_preserves_identity_and_generation() {
        let mut backend = FakeNativeBackend::new();
        let original = backend
            .start(&SessionSpec {
                runtime: RuntimeKind::Native,
                role: "worker".to_string(),
                agent: None,
                provider_route: None,
                model: None,
                surface: UiSurface::Headless,
                cwd: std::path::PathBuf::from("/repo"),
                prompt: "hi".to_string(),
                extra_args: Vec::new(),
            })
            .expect("start");
        let attached = original.clone().attached(UiSurface::DashboardPane);
        assert_eq!(attached.logical_id, original.logical_id);
        assert_eq!(attached.short, original.short);
        assert_eq!(attached.generation, original.generation);
        assert_eq!(attached.role, original.role);

        backend
            .submit(&attached, "still me")
            .expect("submit via attached handle");
        let events = backend.subscribe(&attached, 0).expect("subscribe");
        assert!(
            events
                .iter()
                .all(|event| event.session == original.logical_id)
        );
        assert!(
            events
                .iter()
                .all(|event| event.generation == original.generation)
        );
    }

    #[test]
    fn subscribe_after_only_returns_newer_revisions() {
        let mut backend = FakeNativeBackend::new();
        let handle = backend
            .start(&SessionSpec {
                runtime: RuntimeKind::Native,
                role: "worker".to_string(),
                agent: None,
                provider_route: None,
                model: None,
                surface: UiSurface::Headless,
                cwd: std::path::PathBuf::from("/repo"),
                prompt: "hi".to_string(),
                extra_args: Vec::new(),
            })
            .expect("start");
        let all = backend.subscribe(&handle, 0).expect("subscribe from 0");
        assert_eq!(all.len(), 6);
        let tail = backend.subscribe(&handle, 4).expect("subscribe from 4");
        assert_eq!(tail.len(), 2);
        assert!(tail.iter().all(|event| event.revision > 4));
    }

    #[test]
    fn the_fake_is_selectable_as_a_runtime_backend_trait_object() {
        let backend: Box<dyn RuntimeBackend> = Box::new(FakeNativeBackend::new());
        assert_eq!(backend.kind(), RuntimeKind::Native);
    }
}
