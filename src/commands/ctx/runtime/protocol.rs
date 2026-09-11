//! Versioned in-process wire protocol over [`RuntimeBackend`] (issue #470,
//! aligned with issue #353's own contract): unknown fields are ignored by
//! every `#[derive(Deserialize)]` here (the default, since nothing in this
//! file sets `deny_unknown_fields`), every enum vocabulary carries an
//! `unknown` fallback (`#[serde(other)]`), every command/reply carries a
//! caller-chosen request id so a reply can be matched back to its command,
//! errors are structured (`ErrorCode`, not a free-text guess), and every
//! event carries a monotonically increasing revision plus the generation it
//! was recorded against.
//!
//! No daemon and no socket in this issue: [`dispatch`] calls straight into
//! an in-process `&mut dyn RuntimeBackend`. The envelopes and versioning
//! exist so a later transport (a socket, a subprocess) can carry the exact
//! same JSON without this shape changing.

use serde::{Deserialize, Serialize};

use super::{RuntimeBackend, RuntimeCapabilities, SessionHandle, SessionSpec};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum RuntimeCommand {
    Capabilities,
    Start {
        spec: SessionSpec,
    },
    Submit {
        session: SessionHandle,
        input: String,
    },
    Steer {
        session: SessionHandle,
        input: String,
    },
    Interrupt {
        session: SessionHandle,
    },
    Resume {
        session: SessionHandle,
        input: Option<String>,
    },
    Subscribe {
        session: SessionHandle,
        after_revision: u64,
    },
    /// Forward-compat fallback: a command tag this build has never heard of,
    /// or one whose payload could not be matched to any known variant.
    /// [`dispatch`] refuses it with [`ErrorCode::UnknownCommand`] rather than
    /// guessing which known command was meant.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandEnvelope {
    pub version: u32,
    pub id: String,
    #[serde(flatten)]
    pub command: RuntimeCommand,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum RuntimeEvent {
    Started {
        session: SessionHandle,
    },
    TurnStarted,
    AssistantText {
        text: String,
    },
    ToolCall {
        name: String,
    },
    ToolResult {
        is_error: bool,
    },
    TurnCompleted {
        final_text: Option<String>,
    },
    Interrupted,
    Failed {
        class: String,
        message: String,
    },
    Ended {
        exit_code: Option<i32>,
    },
    /// Forward-compat fallback: an event tag this build has never heard of.
    #[serde(other)]
    Unknown,
}

/// One event on the wire: `revision` is monotonically increasing PER
/// SESSION and never reused (a caller resumes a subscription with
/// `after_revision`, never an offset it has to compute itself), and
/// `generation` pins the seat generation the event was recorded against, so
/// a subscriber can tell an event belonging to a superseded handle apart
/// from a current one even if both share `session`.
///
/// `session` is renamed to `session_id` on the wire only (`#[serde(rename)]`,
/// Rust field name unchanged): `RuntimeEvent::Started` flattens its own
/// `session: SessionHandle` field into this same JSON object, and two
/// flattened fields sharing one wire key -- even at different types -- is a
/// hard `#[serde(flatten)]` conflict, not a stylistic choice.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub version: u32,
    pub revision: u64,
    #[serde(rename = "session_id")]
    pub session: String,
    pub generation: u64,
    #[serde(flatten)]
    pub event: RuntimeEvent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum RuntimeReply {
    Capabilities { capabilities: RuntimeCapabilities },
    Session { session: SessionHandle },
    Ack,
    Events { events: Vec<EventEnvelope> },
    Error { code: ErrorCode, message: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplyEnvelope {
    pub version: u32,
    pub id: String,
    #[serde(flatten)]
    pub reply: RuntimeReply,
}

/// Structured failure classes for a [`RuntimeReply::Error`], so a caller
/// branches on `code` rather than pattern-matching `message` text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    VersionMismatch,
    UnknownCommand,
    Unsupported,
    UnknownSession,
    Busy,
    StaleGeneration,
    /// Any backend-reported error this protocol layer does not have a more
    /// specific code for -- a plain I/O failure, a spawn failure, and so on.
    Backend,
    /// Forward-compat fallback: an error code this build has never heard of
    /// (only reachable by deserializing a reply this build did not itself
    /// produce).
    #[serde(other)]
    Unknown,
}

/// The one place a [`RuntimeCommand`] is turned into a call against a real
/// backend and a [`RuntimeReply`]. Never panics: every arm returns a value,
/// and a backend error is downcast to a [`super::RuntimeError`] when
/// possible (structured `ErrorCode`) or reported as `ErrorCode::Backend`
/// otherwise.
pub fn dispatch(backend: &mut dyn RuntimeBackend, cmd: &CommandEnvelope) -> ReplyEnvelope {
    let id = cmd.id.clone();
    if cmd.version != PROTOCOL_VERSION {
        return ReplyEnvelope {
            version: PROTOCOL_VERSION,
            id,
            reply: RuntimeReply::Error {
                code: ErrorCode::VersionMismatch,
                message: format!(
                    "protocol version {} does not match {PROTOCOL_VERSION}",
                    cmd.version
                ),
            },
        };
    }
    let reply = match &cmd.command {
        RuntimeCommand::Capabilities => RuntimeReply::Capabilities {
            capabilities: backend.capabilities(),
        },
        RuntimeCommand::Start { spec } => match backend.start(spec) {
            Ok(session) => RuntimeReply::Session { session },
            Err(error) => error_reply(error.as_ref()),
        },
        RuntimeCommand::Submit { session, input } => match backend.submit(session, input) {
            Ok(()) => RuntimeReply::Ack,
            Err(error) => error_reply(error.as_ref()),
        },
        RuntimeCommand::Steer { session, input } => match backend.steer(session, input) {
            Ok(()) => RuntimeReply::Ack,
            Err(error) => error_reply(error.as_ref()),
        },
        RuntimeCommand::Interrupt { session } => match backend.interrupt(session) {
            Ok(()) => RuntimeReply::Ack,
            Err(error) => error_reply(error.as_ref()),
        },
        RuntimeCommand::Resume { session, input } => {
            match backend.resume(session, input.as_deref()) {
                Ok(session) => RuntimeReply::Session { session },
                Err(error) => error_reply(error.as_ref()),
            }
        }
        RuntimeCommand::Subscribe {
            session,
            after_revision,
        } => match backend.subscribe(session, *after_revision) {
            Ok(events) => RuntimeReply::Events { events },
            Err(error) => error_reply(error.as_ref()),
        },
        RuntimeCommand::Unknown => RuntimeReply::Error {
            code: ErrorCode::UnknownCommand,
            message: "unrecognized command".to_string(),
        },
    };
    ReplyEnvelope {
        version: PROTOCOL_VERSION,
        id,
        reply,
    }
}

fn error_reply(error: &(dyn std::error::Error + 'static)) -> RuntimeReply {
    let code = match error.downcast_ref::<super::RuntimeError>() {
        Some(super::RuntimeError::Unsupported(_)) => ErrorCode::Unsupported,
        Some(super::RuntimeError::UnknownSession(_)) => ErrorCode::UnknownSession,
        Some(super::RuntimeError::Busy(_)) => ErrorCode::Busy,
        Some(super::RuntimeError::StaleGeneration { .. }) => ErrorCode::StaleGeneration,
        None => ErrorCode::Backend,
    };
    RuntimeReply::Error {
        code,
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::runtime::RuntimeKind;

    /// One frozen JSON fixture per command/reply/event variant, generated
    /// once from this same module's types (see the now-removed fixture
    /// generator this test's own history carried) and committed as data
    /// under `tests/fixtures/runtime/v1/`. Round-tripping every one proves
    /// the wire shape is stable: a fixture that deserializes and
    /// re-serializes to different bytes means this build would talk a
    /// different protocol than whatever wrote the fixture.
    fn fixtures_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/runtime/v1")
    }

    fn assert_round_trips<T>(name: &str)
    where
        T: for<'de> Deserialize<'de> + Serialize,
    {
        let path = fixtures_dir().join(name);
        let original = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read fixture {path:?}: {error}"));
        let value: T = serde_json::from_str(&original)
            .unwrap_or_else(|error| panic!("deserialize fixture {path:?}: {error}"));
        let mut re_serialized =
            serde_json::to_string_pretty(&value).expect("re-serialize fixture value");
        re_serialized.push('\n');
        assert_eq!(
            original, re_serialized,
            "fixture {path:?} is not byte-identical after a round trip"
        );
    }

    #[test]
    fn every_command_fixture_round_trips() {
        for name in [
            "command-capabilities.json",
            "command-start.json",
            "command-submit.json",
            "command-steer.json",
            "command-interrupt.json",
            "command-resume.json",
            "command-subscribe.json",
        ] {
            assert_round_trips::<CommandEnvelope>(name);
        }
    }

    #[test]
    fn every_reply_fixture_round_trips() {
        for name in [
            "reply-capabilities.json",
            "reply-session.json",
            "reply-ack.json",
            "reply-events.json",
            "reply-error.json",
        ] {
            assert_round_trips::<ReplyEnvelope>(name);
        }
    }

    #[test]
    fn every_event_fixture_round_trips() {
        for name in [
            "event-started.json",
            "event-turn_started.json",
            "event-assistant_text.json",
            "event-tool_call.json",
            "event-tool_result.json",
            "event-turn_completed.json",
            "event-interrupted.json",
            "event-failed.json",
            "event-ended.json",
        ] {
            assert_round_trips::<EventEnvelope>(name);
        }
    }

    /// A command whose `command` tag is unrecognized -- and which carries
    /// extra fields no known variant has -- deserializes to `Unknown`
    /// rather than failing to parse, and `dispatch` reports it as
    /// `UnknownCommand` rather than guessing which real command was meant.
    #[test]
    fn an_unknown_command_tag_with_extra_fields_dispatches_to_unknown_command() {
        let json = r#"{
            "version": 1,
            "id": "req-1",
            "command": "levitate",
            "extra_field_nobody_asked_for": "surprise"
        }"#;
        let envelope: CommandEnvelope = serde_json::from_str(json).expect("deserialize");
        assert_eq!(envelope.command, RuntimeCommand::Unknown);

        #[derive(Debug)]
        struct NeverCalled;
        impl RuntimeBackend for NeverCalled {
            fn kind(&self) -> RuntimeKind {
                unreachable!()
            }
            fn capabilities(&self) -> RuntimeCapabilities {
                unreachable!()
            }
            fn start(
                &mut self,
                _spec: &SessionSpec,
            ) -> crate::commands::ctx::CtxResult<SessionHandle> {
                unreachable!()
            }
            fn submit(
                &mut self,
                _session: &SessionHandle,
                _input: &str,
            ) -> crate::commands::ctx::CtxResult<()> {
                unreachable!()
            }
            fn steer(
                &mut self,
                _session: &SessionHandle,
                _input: &str,
            ) -> crate::commands::ctx::CtxResult<()> {
                unreachable!()
            }
            fn interrupt(
                &mut self,
                _session: &SessionHandle,
            ) -> crate::commands::ctx::CtxResult<()> {
                unreachable!()
            }
            fn resume(
                &mut self,
                _session: &SessionHandle,
                _input: Option<&str>,
            ) -> crate::commands::ctx::CtxResult<SessionHandle> {
                unreachable!()
            }
            fn subscribe(
                &mut self,
                _session: &SessionHandle,
                _after_revision: u64,
            ) -> crate::commands::ctx::CtxResult<Vec<EventEnvelope>> {
                unreachable!()
            }
        }
        let mut backend = NeverCalled;
        let reply = dispatch(&mut backend, &envelope);
        match reply.reply {
            RuntimeReply::Error { code, .. } => assert_eq!(code, ErrorCode::UnknownCommand),
            other => panic!("expected an UnknownCommand error, got {other:?}"),
        }
    }

    /// An event whose `event` tag is unrecognized deserializes to `Unknown`
    /// rather than failing to parse -- the same forward-compat rule as
    /// commands, proven independently since events and commands are
    /// deserialized by separate derived impls.
    #[test]
    fn an_unknown_event_tag_deserializes_to_unknown() {
        let json = r#"{
            "version": 1,
            "revision": 1,
            "session_id": "session-1",
            "generation": 1,
            "event": "levitated",
            "whatever": true
        }"#;
        let envelope: EventEnvelope = serde_json::from_str(json).expect("deserialize");
        assert_eq!(envelope.event, RuntimeEvent::Unknown);
    }

    #[test]
    fn a_version_mismatch_is_refused_before_the_backend_is_ever_called() {
        #[derive(Debug)]
        struct Panics;
        impl RuntimeBackend for Panics {
            fn kind(&self) -> RuntimeKind {
                RuntimeKind::Harness
            }
            fn capabilities(&self) -> RuntimeCapabilities {
                panic!("must not be called on a version mismatch")
            }
            fn start(
                &mut self,
                _spec: &SessionSpec,
            ) -> crate::commands::ctx::CtxResult<SessionHandle> {
                panic!("must not be called on a version mismatch")
            }
            fn submit(
                &mut self,
                _session: &SessionHandle,
                _input: &str,
            ) -> crate::commands::ctx::CtxResult<()> {
                panic!("must not be called on a version mismatch")
            }
            fn steer(
                &mut self,
                _session: &SessionHandle,
                _input: &str,
            ) -> crate::commands::ctx::CtxResult<()> {
                panic!("must not be called on a version mismatch")
            }
            fn interrupt(
                &mut self,
                _session: &SessionHandle,
            ) -> crate::commands::ctx::CtxResult<()> {
                panic!("must not be called on a version mismatch")
            }
            fn resume(
                &mut self,
                _session: &SessionHandle,
                _input: Option<&str>,
            ) -> crate::commands::ctx::CtxResult<SessionHandle> {
                panic!("must not be called on a version mismatch")
            }
            fn subscribe(
                &mut self,
                _session: &SessionHandle,
                _after_revision: u64,
            ) -> crate::commands::ctx::CtxResult<Vec<EventEnvelope>> {
                panic!("must not be called on a version mismatch")
            }
        }
        let envelope = CommandEnvelope {
            version: 99,
            id: "req-2".to_string(),
            command: RuntimeCommand::Capabilities,
        };
        let mut backend = Panics;
        let reply = dispatch(&mut backend, &envelope);
        match reply.reply {
            RuntimeReply::Error { code, .. } => assert_eq!(code, ErrorCode::VersionMismatch),
            other => panic!("expected a VersionMismatch error, got {other:?}"),
        }
    }
}
