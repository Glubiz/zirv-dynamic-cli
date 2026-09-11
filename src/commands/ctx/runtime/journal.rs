//! Authoritative native-conversation journal (issue #472, roadmap N03).
//!
//! This database owns native conversation facts only: acknowledged input,
//! committed model messages, provider attempts and usage, complete tool calls,
//! execution state, task receipts, checkpoints, and provider continuations.
//! The existing session registry, seat record, task log, mailbox, workflow
//! state, and policy compiler remain authoritative for their own domains; the
//! journal stores their stable identifiers and provenance rather than copying
//! or replacing those stores.
//!
//! One native runtime service is the writer for a session. SQLite serializes
//! transactions and WAL permits concurrent readers, but this module does not
//! invent a second ownership/lease system beside `seat.rs`; the persistent
//! runtime work in N20 binds this writer to that service. Every mutation needs
//! `&mut Journal`, allocates the next sequence in the same `IMMEDIATE`
//! transaction as the event, and fences on the persisted seat generation.
//!
//! Draft stream frames are deliberately outside the committed event log. A
//! completed-message/tool-call barrier moves them into one full event and
//! removes the drafts atomically. A truncated tool-argument stream therefore
//! cannot become executable. Tool executions record `Prepared` before any
//! effect and `Started` immediately before it; after a crash the runtime calls
//! [`Journal::reconcile_started_as_unknown`] and must reconcile or obtain an
//! idempotency guarantee before retrying an outcome-unknown effect.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::super::event::{NormalizedEvent, error_text_hash, input_hash};
use super::super::provider::{
    AccountId, BillingPoolId, EndpointId, ModelId, Protocol, ProviderId, RouteId,
};
use super::super::state::{self, StateDir};

pub const JOURNAL_SCHEMA_VERSION: i64 = 1;
pub const MAX_STREAM_FRAME_BYTES: usize = 64 * 1024;
pub const MAX_STREAM_DRAFT_BYTES: usize = 8 * 1024 * 1024;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

macro_rules! opaque_id {
    ($name:ident) => {
        #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> JournalResult<Self> {
                let value = value.into();
                if value.is_empty() || value.len() > 256 || value.contains('\0') {
                    return Err(JournalError::InvalidId {
                        kind: stringify!($name),
                        value,
                    });
                }
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

opaque_id!(JournalSessionId);
opaque_id!(SeatId);
opaque_id!(TaskId);
opaque_id!(TurnId);
opaque_id!(RequestAttemptId);
opaque_id!(MessageId);
opaque_id!(ToolCallId);
opaque_id!(ExecutionId);
opaque_id!(UsageId);
opaque_id!(CheckpointId);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SequenceId(pub u64);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteIdentity {
    pub route: RouteId,
    pub provider: ProviderId,
    pub endpoint: EndpointId,
    pub account: AccountId,
    pub billing_pool: BillingPoolId,
    pub protocol: Protocol,
    pub model: ModelId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionIdentity {
    pub session: JournalSessionId,
    pub seat: SeatId,
    pub generation: u64,
    pub task: Option<TaskId>,
    pub route: RouteIdentity,
    pub created_at: u64,
    pub completed_at: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventScope {
    pub turn: Option<TurnId>,
    pub attempt: Option<RequestAttemptId>,
    pub task: Option<TaskId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AssistantBlock {
    Text { text: String },
    Thinking { text: String },
    Refusal { text: String },
    ToolCall { tool_call: ToolCallId },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageRecord {
    pub id: UsageId,
    pub input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub output_tokens: u64,
    pub provider_request_id: Option<String>,
    pub estimated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyProvenance {
    pub fingerprint: String,
    pub source: String,
    pub decision: String,
    pub scope: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "storage", rename_all = "snake_case")]
pub enum ContentRef {
    Inline { text: String },
    Artifact {
        sha256: String,
        byte_len: u64,
        content_hash: u64,
        media_type: String,
    },
}

impl ContentRef {
    pub fn byte_len(&self) -> u64 {
        match self {
            Self::Inline { text } => text.len() as u64,
            Self::Artifact { byte_len, .. } => *byte_len,
        }
    }

    pub fn normalized_hash(&self) -> u64 {
        match self {
            Self::Inline { text } => input_hash(text),
            Self::Artifact { content_hash, .. } => *content_hash,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionState {
    Prepared,
    Started,
    Completed,
    Failed,
    Cancelled,
    OutcomeUnknown,
}

impl ExecutionState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskReceiptState {
    Accepted,
    Started,
    Blocked,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointKind {
    Recovery,
    Compaction,
    Handoff,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JournalEvent {
    InputAcknowledged {
        message_id: MessageId,
        text: String,
        steering: bool,
        at_ms: Option<u64>,
    },
    AssistantMessageCommitted {
        message_id: MessageId,
        blocks: Vec<AssistantBlock>,
        usage: Option<UsageId>,
        at_ms: Option<u64>,
    },
    UsageRecorded {
        usage: UsageRecord,
    },
    ToolCallPrepared {
        tool_call_id: ToolCallId,
        name: String,
        arguments: serde_json::Value,
        policy: PolicyProvenance,
        at_ms: Option<u64>,
    },
    ToolExecution {
        execution_id: ExecutionId,
        tool_call_id: ToolCallId,
        state: ExecutionState,
        result: Option<ContentRef>,
        detail: Option<String>,
        at_ms: Option<u64>,
    },
    TaskReceipt {
        task_id: TaskId,
        state: TaskReceiptState,
        receipt: serde_json::Value,
    },
    Checkpoint {
        checkpoint_id: CheckpointId,
        kind: CheckpointKind,
        portable_state: serde_json::Value,
    },
    GenerationAdvanced {
        previous: u64,
        current: u64,
    },
    SessionEnded {
        reason: String,
    },
}

impl JournalEvent {
    fn kind(&self) -> &'static str {
        match self {
            Self::InputAcknowledged { .. } => "input_acknowledged",
            Self::AssistantMessageCommitted { .. } => "assistant_message_committed",
            Self::UsageRecorded { .. } => "usage_recorded",
            Self::ToolCallPrepared { .. } => "tool_call_prepared",
            Self::ToolExecution { .. } => "tool_execution",
            Self::TaskReceipt { .. } => "task_receipt",
            Self::Checkpoint { .. } => "checkpoint",
            Self::GenerationAdvanced { .. } => "generation_advanced",
            Self::SessionEnded { .. } => "session_ended",
        }
    }

    fn indexed_ids(&self) -> IndexedIds<'_> {
        match self {
            Self::InputAcknowledged { message_id, .. }
            | Self::AssistantMessageCommitted { message_id, .. } => IndexedIds {
                message: Some(message_id),
                ..IndexedIds::default()
            },
            Self::UsageRecorded { usage } => IndexedIds {
                usage: Some(&usage.id),
                ..IndexedIds::default()
            },
            Self::ToolCallPrepared { tool_call_id, .. } => IndexedIds {
                tool_call: Some(tool_call_id),
                ..IndexedIds::default()
            },
            Self::ToolExecution {
                execution_id,
                tool_call_id,
                ..
            } => IndexedIds {
                tool_call: Some(tool_call_id),
                execution: Some(execution_id),
                ..IndexedIds::default()
            },
            Self::TaskReceipt { task_id, .. } => IndexedIds {
                task: Some(task_id),
                ..IndexedIds::default()
            },
            Self::Checkpoint { checkpoint_id, .. } => IndexedIds {
                checkpoint: Some(checkpoint_id),
                ..IndexedIds::default()
            },
            Self::GenerationAdvanced { .. } | Self::SessionEnded { .. } => {
                IndexedIds::default()
            }
        }
    }
}

#[derive(Default)]
struct IndexedIds<'a> {
    message: Option<&'a MessageId>,
    tool_call: Option<&'a ToolCallId>,
    execution: Option<&'a ExecutionId>,
    usage: Option<&'a UsageId>,
    task: Option<&'a TaskId>,
    checkpoint: Option<&'a CheckpointId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamFrameKind {
    AssistantText,
    AssistantThinking,
    AssistantRefusal,
    ToolArguments,
}

impl StreamFrameKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::AssistantText => "assistant_text",
            Self::AssistantThinking => "assistant_thinking",
            Self::AssistantRefusal => "assistant_refusal",
            Self::ToolArguments => "tool_arguments",
        }
    }

    fn parse(value: &str) -> JournalResult<Self> {
        match value {
            "assistant_text" => Ok(Self::AssistantText),
            "assistant_thinking" => Ok(Self::AssistantThinking),
            "assistant_refusal" => Ok(Self::AssistantRefusal),
            "tool_arguments" => Ok(Self::ToolArguments),
            other => Err(JournalError::Corrupt(format!(
                "unknown draft frame kind {other:?}"
            ))),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredEvent {
    pub sequence: SequenceId,
    pub generation: u64,
    pub scope: EventScope,
    pub committed_at: u64,
    pub event: JournalEvent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredMessage {
    pub sequence: SequenceId,
    pub message_id: MessageId,
    pub role: MessageRole,
    pub blocks: Vec<AssistantBlock>,
    pub text: Option<String>,
    pub steering: bool,
    pub usage: Option<UsageId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageRole {
    User,
    Assistant,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCallRecord {
    pub sequence: SequenceId,
    pub name: String,
    pub arguments: serde_json::Value,
    pub policy: PolicyProvenance,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionRecord {
    pub sequence: SequenceId,
    pub tool_call: ToolCallId,
    pub state: ExecutionState,
    pub result: Option<ContentRef>,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskReceiptRecord {
    pub sequence: SequenceId,
    pub state: TaskReceiptState,
    pub receipt: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckpointRecord {
    pub sequence: SequenceId,
    pub kind: CheckpointKind,
    pub portable_state: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversationState {
    pub identity: SessionIdentity,
    pub last_sequence: SequenceId,
    pub messages: Vec<StoredMessage>,
    pub usage: BTreeMap<UsageId, UsageRecord>,
    pub tool_calls: BTreeMap<ToolCallId, ToolCallRecord>,
    pub executions: BTreeMap<ExecutionId, ExecutionRecord>,
    pub task_receipts: BTreeMap<TaskId, Vec<TaskReceiptRecord>>,
    pub checkpoints: BTreeMap<CheckpointId, CheckpointRecord>,
    pub ended_reason: Option<String>,
}

impl ConversationState {
    fn reduce(identity: SessionIdentity, events: &[StoredEvent]) -> JournalResult<Self> {
        let mut state = Self {
            identity,
            last_sequence: SequenceId(0),
            messages: Vec::new(),
            usage: BTreeMap::new(),
            tool_calls: BTreeMap::new(),
            executions: BTreeMap::new(),
            task_receipts: BTreeMap::new(),
            checkpoints: BTreeMap::new(),
            ended_reason: None,
        };
        for stored in events {
            if stored.sequence <= state.last_sequence {
                return Err(JournalError::Corrupt(format!(
                    "event sequence {} follows {}",
                    stored.sequence.0, state.last_sequence.0
                )));
            }
            state.last_sequence = stored.sequence;
            match &stored.event {
                JournalEvent::InputAcknowledged {
                    message_id,
                    text,
                    steering,
                    ..
                } => state.messages.push(StoredMessage {
                    sequence: stored.sequence,
                    message_id: message_id.clone(),
                    role: MessageRole::User,
                    blocks: Vec::new(),
                    text: Some(text.clone()),
                    steering: *steering,
                    usage: None,
                }),
                JournalEvent::AssistantMessageCommitted {
                    message_id,
                    blocks,
                    usage,
                    ..
                } => state.messages.push(StoredMessage {
                    sequence: stored.sequence,
                    message_id: message_id.clone(),
                    role: MessageRole::Assistant,
                    blocks: blocks.clone(),
                    text: None,
                    steering: false,
                    usage: usage.clone(),
                }),
                JournalEvent::UsageRecorded { usage } => {
                    if state.usage.insert(usage.id.clone(), usage.clone()).is_some() {
                        return Err(JournalError::Corrupt(format!(
                            "duplicate usage id {}",
                            usage.id
                        )));
                    }
                }
                JournalEvent::ToolCallPrepared {
                    tool_call_id,
                    name,
                    arguments,
                    policy,
                    ..
                } => {
                    if state
                        .tool_calls
                        .insert(
                            tool_call_id.clone(),
                            ToolCallRecord {
                                sequence: stored.sequence,
                                name: name.clone(),
                                arguments: arguments.clone(),
                                policy: policy.clone(),
                            },
                        )
                        .is_some()
                    {
                        return Err(JournalError::Corrupt(format!(
                            "duplicate tool call id {tool_call_id}"
                        )));
                    }
                }
                JournalEvent::ToolExecution {
                    execution_id,
                    tool_call_id,
                    state: execution_state,
                    result,
                    detail,
                    ..
                } => {
                    if !state.tool_calls.contains_key(tool_call_id) {
                        return Err(JournalError::Corrupt(format!(
                            "execution {execution_id} references unknown tool call {tool_call_id}"
                        )));
                    }
                    if let Some(previous) = state.executions.get(execution_id) {
                        if previous.tool_call != *tool_call_id {
                            return Err(JournalError::Corrupt(format!(
                                "execution {execution_id} changed tool call from {} to {tool_call_id}",
                                previous.tool_call
                            )));
                        }
                        validate_execution_transition(
                            previous.state,
                            *execution_state,
                            result.as_ref(),
                        )
                        .map_err(|error| JournalError::Corrupt(error.to_string()))?;
                    } else if *execution_state != ExecutionState::Prepared || result.is_some() {
                        return Err(JournalError::Corrupt(format!(
                            "execution {execution_id} does not begin in prepared state"
                        )));
                    }
                    state.executions.insert(
                        execution_id.clone(),
                        ExecutionRecord {
                            sequence: stored.sequence,
                            tool_call: tool_call_id.clone(),
                            state: *execution_state,
                            result: result.clone(),
                            detail: detail.clone(),
                        },
                    );
                }
                JournalEvent::TaskReceipt {
                    task_id,
                    state: receipt_state,
                    receipt,
                } => state
                    .task_receipts
                    .entry(task_id.clone())
                    .or_default()
                    .push(TaskReceiptRecord {
                        sequence: stored.sequence,
                        state: *receipt_state,
                        receipt: receipt.clone(),
                    }),
                JournalEvent::Checkpoint {
                    checkpoint_id,
                    kind,
                    portable_state,
                } => {
                    state.checkpoints.insert(
                        checkpoint_id.clone(),
                        CheckpointRecord {
                            sequence: stored.sequence,
                            kind: *kind,
                            portable_state: portable_state.clone(),
                        },
                    );
                }
                JournalEvent::GenerationAdvanced { current, .. } => {
                    state.identity.generation = *current;
                }
                JournalEvent::SessionEnded { reason } => {
                    state.ended_reason = Some(reason.clone());
                }
            }
        }
        for message in &state.messages {
            if let Some(usage) = &message.usage
                && !state.usage.contains_key(usage)
            {
                return Err(JournalError::Corrupt(format!(
                    "message {} references unknown usage id {usage}",
                    message.message_id
                )));
            }
        }
        Ok(state)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContinuationIdentity {
    pub route: RouteId,
    pub provider: ProviderId,
    pub endpoint: EndpointId,
    pub account: AccountId,
    pub protocol: Protocol,
    pub model: ModelId,
}

impl From<&RouteIdentity> for ContinuationIdentity {
    fn from(route: &RouteIdentity) -> Self {
        Self {
            route: route.route.clone(),
            provider: route.provider.clone(),
            endpoint: route.endpoint.clone(),
            account: route.account.clone(),
            protocol: route.protocol,
            model: route.model.clone(),
        }
    }
}

#[derive(Debug)]
pub enum JournalError {
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
    Json(serde_json::Error),
    InvalidId { kind: &'static str, value: String },
    InvalidNumber { field: &'static str, value: u64 },
    UnsupportedSchema(i64),
    UnversionedSchema,
    Corrupt(String),
    UnknownSession(String),
    SessionExists(String),
    StaleGeneration { expected: u64, got: u64 },
    DuplicateId { kind: &'static str, value: String },
    UnknownToolCall(String),
    UnknownExecution(String),
    InvalidExecutionTransition {
        from: ExecutionState,
        to: ExecutionState,
    },
    InvalidToolArguments(String),
    InvalidStream(String),
    FrameTooLarge { bytes: usize, limit: usize },
    DraftTooLarge { bytes: usize, limit: usize },
    ContinuationMismatch,
}

impl std::fmt::Display for JournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sqlite(error) => write!(f, "native journal sqlite error: {error}"),
            Self::Io(error) => write!(f, "native journal I/O error: {error}"),
            Self::Json(error) => write!(f, "native journal JSON error: {error}"),
            Self::InvalidId { kind, value } => {
                write!(f, "invalid {kind} {value:?}; expected 1..=256 non-NUL bytes")
            }
            Self::InvalidNumber { field, value } => {
                write!(f, "{field} value {value} exceeds SQLite's signed integer range")
            }
            Self::UnsupportedSchema(version) => write!(
                f,
                "native journal schema {version} is newer than supported schema {JOURNAL_SCHEMA_VERSION}; upgrade zirv"
            ),
            Self::UnversionedSchema => write!(
                f,
                "native journal contains tables but has schema version 0; refusing to guess a migration"
            ),
            Self::Corrupt(detail) => write!(f, "native journal is corrupt: {detail}"),
            Self::UnknownSession(id) => write!(f, "unknown native journal session {id}"),
            Self::SessionExists(id) => write!(f, "native journal session already exists: {id}"),
            Self::StaleGeneration { expected, got } => {
                write!(f, "stale native journal generation: expected {expected}, got {got}")
            }
            Self::DuplicateId { kind, value } => write!(f, "duplicate {kind} id: {value}"),
            Self::UnknownToolCall(id) => write!(f, "unknown native tool call {id}"),
            Self::UnknownExecution(id) => write!(f, "unknown native tool execution {id}"),
            Self::InvalidExecutionTransition { from, to } => {
                write!(f, "invalid tool execution transition {from:?} -> {to:?}")
            }
            Self::InvalidToolArguments(detail) => {
                write!(f, "invalid complete tool arguments: {detail}")
            }
            Self::InvalidStream(detail) => write!(f, "invalid native stream: {detail}"),
            Self::FrameTooLarge { bytes, limit } => {
                write!(f, "stream frame is {bytes} bytes; limit is {limit}")
            }
            Self::DraftTooLarge { bytes, limit } => {
                write!(f, "stream draft is {bytes} bytes; limit is {limit}")
            }
            Self::ContinuationMismatch => write!(
                f,
                "provider continuation identity does not match its original route/protocol/model"
            ),
        }
    }
}

impl std::error::Error for JournalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for JournalError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

impl From<std::io::Error> for JournalError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for JournalError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

pub type JournalResult<T> = Result<T, JournalError>;

#[derive(Debug)]
pub struct Journal {
    path: PathBuf,
    conn: Connection,
}

impl Journal {
    pub fn open(state: &StateDir) -> JournalResult<Self> {
        Self::open_path(state.native_journal())
    }

    pub fn open_path(path: impl Into<PathBuf>) -> JournalResult<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            state::create_private_dir_all(parent)?;
        }
        let conn = Connection::open(&path)?;
        secure_database_file(&path)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        verify_integrity(&conn)?;
        migrate(&conn)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        Ok(Self { path, conn })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn create_session(&mut self, identity: &SessionIdentity) -> JournalResult<()> {
        let generation = sql_u64(identity.generation, "generation")?;
        let created_at = sql_u64(identity.created_at, "created_at")?;
        let completed_at = identity
            .completed_at
            .map(|value| sql_u64(value, "completed_at"))
            .transpose()?;
        let protocol = protocol_name(identity.route.protocol);
        let result = self.conn.execute(
            "INSERT INTO native_sessions (
                 session_id, seat_id, generation, task_id, route_id, provider_id,
                 endpoint_id, account_id, billing_pool_id, protocol, model_vendor,
                 model_id, created_at, updated_at, next_sequence, completed_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?13, 0, ?14)",
            params![
                identity.session.as_str(),
                identity.seat.as_str(),
                generation,
                identity.task.as_ref().map(TaskId::as_str),
                identity.route.route.as_ref(),
                identity.route.provider.as_ref(),
                identity.route.endpoint.as_ref(),
                identity.route.account.as_ref(),
                identity.route.billing_pool.as_ref(),
                protocol,
                identity.route.model.vendor,
                identity.route.model.id,
                created_at,
                completed_at,
            ],
        );
        match result {
            Ok(_) => Ok(()),
            Err(error) if is_constraint(&error) => {
                Err(JournalError::SessionExists(identity.session.to_string()))
            }
            Err(error) => Err(error.into()),
        }
    }

    pub fn session(&self, session: &JournalSessionId) -> JournalResult<SessionIdentity> {
        read_session(&self.conn, session)
    }

    pub fn acknowledge_input(
        &mut self,
        session: &JournalSessionId,
        generation: u64,
        scope: &EventScope,
        message_id: MessageId,
        text: String,
        steering: bool,
        at_ms: Option<u64>,
        committed_at: u64,
    ) -> JournalResult<SequenceId> {
        self.append(
            session,
            generation,
            scope,
            JournalEvent::InputAcknowledged {
                message_id,
                text,
                steering,
                at_ms,
            },
            committed_at,
        )
    }

    pub fn record_usage(
        &mut self,
        session: &JournalSessionId,
        generation: u64,
        scope: &EventScope,
        usage: UsageRecord,
        committed_at: u64,
    ) -> JournalResult<SequenceId> {
        self.append(
            session,
            generation,
            scope,
            JournalEvent::UsageRecorded { usage },
            committed_at,
        )
    }

    pub fn record_assistant_message(
        &mut self,
        session: &JournalSessionId,
        generation: u64,
        scope: &EventScope,
        message_id: MessageId,
        blocks: Vec<AssistantBlock>,
        usage: Option<UsageId>,
        at_ms: Option<u64>,
        committed_at: u64,
    ) -> JournalResult<SequenceId> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(usage) = usage.as_ref() {
            require_usage(&tx, session, usage)?;
        }
        let sequence = append_tx(
            &tx,
            session,
            generation,
            scope,
            &JournalEvent::AssistantMessageCommitted {
                message_id,
                blocks,
                usage,
                at_ms,
            },
            committed_at,
        )?;
        tx.commit()?;
        Ok(sequence)
    }

    pub fn prepare_tool_call(
        &mut self,
        session: &JournalSessionId,
        generation: u64,
        scope: &EventScope,
        tool_call_id: ToolCallId,
        name: String,
        arguments: serde_json::Value,
        policy: PolicyProvenance,
        at_ms: Option<u64>,
        committed_at: u64,
    ) -> JournalResult<SequenceId> {
        validate_tool_arguments(&arguments)?;
        self.append(
            session,
            generation,
            scope,
            JournalEvent::ToolCallPrepared {
                tool_call_id,
                name,
                arguments,
                policy,
                at_ms,
            },
            committed_at,
        )
    }

    pub fn prepare_execution(
        &mut self,
        session: &JournalSessionId,
        generation: u64,
        scope: &EventScope,
        execution_id: ExecutionId,
        tool_call_id: ToolCallId,
        at_ms: Option<u64>,
        committed_at: u64,
    ) -> JournalResult<SequenceId> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_tool_call(&tx, session, &tool_call_id)?;
        if latest_execution(&tx, session, &execution_id)?.is_some() {
            return Err(JournalError::DuplicateId {
                kind: "ExecutionId",
                value: execution_id.to_string(),
            });
        }
        let sequence = append_tx(
            &tx,
            session,
            generation,
            scope,
            &JournalEvent::ToolExecution {
                execution_id,
                tool_call_id,
                state: ExecutionState::Prepared,
                result: None,
                detail: None,
                at_ms,
            },
            committed_at,
        )?;
        tx.commit()?;
        Ok(sequence)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn transition_execution(
        &mut self,
        session: &JournalSessionId,
        generation: u64,
        scope: &EventScope,
        execution_id: &ExecutionId,
        to: ExecutionState,
        result: Option<ContentRef>,
        detail: Option<String>,
        at_ms: Option<u64>,
        committed_at: u64,
    ) -> JournalResult<SequenceId> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous = latest_execution(&tx, session, execution_id)?
            .ok_or_else(|| JournalError::UnknownExecution(execution_id.to_string()))?;
        validate_execution_transition(previous.state, to, result.as_ref())?;
        let sequence = append_tx(
            &tx,
            session,
            generation,
            scope,
            &JournalEvent::ToolExecution {
                execution_id: execution_id.clone(),
                tool_call_id: previous.tool_call,
                state: to,
                result,
                detail,
                at_ms,
            },
            committed_at,
        )?;
        tx.commit()?;
        Ok(sequence)
    }

    /// Converts every execution whose latest durable state is `Started` to
    /// `OutcomeUnknown` in one transaction. This is explicit recovery, never
    /// an automatic retry and never run merely because another reader opens
    /// the database while a live runtime is executing a tool.
    pub fn reconcile_started_as_unknown(
        &mut self,
        session: &JournalSessionId,
        generation: u64,
        at_ms: Option<u64>,
        committed_at: u64,
    ) -> JournalResult<Vec<ExecutionId>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_generation(&tx, session, generation)?;
        let started = latest_started_executions(&tx, session)?;
        for (execution_id, tool_call_id) in &started {
            append_tx(
                &tx,
                session,
                generation,
                &EventScope::default(),
                &JournalEvent::ToolExecution {
                    execution_id: execution_id.clone(),
                    tool_call_id: tool_call_id.clone(),
                    state: ExecutionState::OutcomeUnknown,
                    result: None,
                    detail: Some("runtime stopped after effect began; reconcile before retry".into()),
                    at_ms,
                },
                committed_at,
            )?;
        }
        tx.commit()?;
        Ok(started.into_iter().map(|(id, _)| id).collect())
    }

    pub fn record_task_receipt(
        &mut self,
        session: &JournalSessionId,
        generation: u64,
        scope: &EventScope,
        task_id: TaskId,
        receipt_state: TaskReceiptState,
        receipt: serde_json::Value,
        committed_at: u64,
    ) -> JournalResult<SequenceId> {
        self.append(
            session,
            generation,
            scope,
            JournalEvent::TaskReceipt {
                task_id,
                state: receipt_state,
                receipt,
            },
            committed_at,
        )
    }

    pub fn record_checkpoint(
        &mut self,
        session: &JournalSessionId,
        generation: u64,
        scope: &EventScope,
        checkpoint_id: CheckpointId,
        kind: CheckpointKind,
        portable_state: serde_json::Value,
        committed_at: u64,
    ) -> JournalResult<SequenceId> {
        self.append(
            session,
            generation,
            scope,
            JournalEvent::Checkpoint {
                checkpoint_id,
                kind,
                portable_state,
            },
            committed_at,
        )
    }

    pub fn advance_generation(
        &mut self,
        session: &JournalSessionId,
        expected: u64,
        current: u64,
        committed_at: u64,
    ) -> JournalResult<SequenceId> {
        if current <= expected {
            return Err(JournalError::Corrupt(format!(
                "generation must advance from {expected}, got {current}"
            )));
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_generation(&tx, session, expected)?;
        tx.execute(
            "UPDATE native_sessions SET generation = ?2, updated_at = ?3 WHERE session_id = ?1",
            params![
                session.as_str(),
                sql_u64(current, "generation")?,
                sql_u64(committed_at, "committed_at")?,
            ],
        )?;
        let sequence = append_tx(
            &tx,
            session,
            current,
            &EventScope::default(),
            &JournalEvent::GenerationAdvanced {
                previous: expected,
                current,
            },
            committed_at,
        )?;
        tx.commit()?;
        Ok(sequence)
    }

    pub fn complete_session(
        &mut self,
        session: &JournalSessionId,
        generation: u64,
        reason: String,
        committed_at: u64,
    ) -> JournalResult<SequenceId> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let sequence = append_tx(
            &tx,
            session,
            generation,
            &EventScope::default(),
            &JournalEvent::SessionEnded { reason },
            committed_at,
        )?;
        tx.execute(
            "UPDATE native_sessions SET completed_at = ?2, updated_at = ?2 WHERE session_id = ?1",
            params![session.as_str(), sql_u64(committed_at, "committed_at")?],
        )?;
        tx.commit()?;
        Ok(sequence)
    }

    pub fn append_stream_frame(
        &mut self,
        session: &JournalSessionId,
        generation: u64,
        stream_id: &str,
        kind: StreamFrameKind,
        bytes: &[u8],
        created_at: u64,
    ) -> JournalResult<u64> {
        if bytes.len() > MAX_STREAM_FRAME_BYTES {
            return Err(JournalError::FrameTooLarge {
                bytes: bytes.len(),
                limit: MAX_STREAM_FRAME_BYTES,
            });
        }
        if stream_id.is_empty() || stream_id.len() > 256 || stream_id.contains('\0') {
            return Err(JournalError::InvalidStream(
                "stream id must contain 1..=256 non-NUL bytes".into(),
            ));
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_generation(&tx, session, generation)?;
        let (next, current_bytes): (i64, i64) = tx.query_row(
            "SELECT COALESCE(MAX(frame_index), -1) + 1, COALESCE(SUM(length(data)), 0)
             FROM native_stream_frames WHERE session_id = ?1 AND stream_id = ?2",
            params![session.as_str(), stream_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let total = usize::try_from(current_bytes)
            .unwrap_or(usize::MAX)
            .saturating_add(bytes.len());
        if total > MAX_STREAM_DRAFT_BYTES {
            return Err(JournalError::DraftTooLarge {
                bytes: total,
                limit: MAX_STREAM_DRAFT_BYTES,
            });
        }
        tx.execute(
            "INSERT INTO native_stream_frames
                 (session_id, stream_id, frame_index, kind, data, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                session.as_str(),
                stream_id,
                next,
                kind.as_str(),
                bytes,
                sql_u64(created_at, "created_at")?,
            ],
        )?;
        tx.commit()?;
        u64::try_from(next).map_err(|_| JournalError::Corrupt("negative frame index".into()))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn commit_assistant_stream(
        &mut self,
        session: &JournalSessionId,
        generation: u64,
        scope: &EventScope,
        stream_id: &str,
        message_id: MessageId,
        usage: Option<UsageId>,
        at_ms: Option<u64>,
        committed_at: u64,
    ) -> JournalResult<SequenceId> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_generation(&tx, session, generation)?;
        if let Some(usage) = usage.as_ref() {
            require_usage(&tx, session, usage)?;
        }
        let frames = read_frames(&tx, session, stream_id)?;
        if frames.is_empty() {
            return Err(JournalError::InvalidStream("no frames to commit".into()));
        }
        let blocks = assistant_blocks_from_frames(&frames)?;
        let sequence = append_tx(
            &tx,
            session,
            generation,
            scope,
            &JournalEvent::AssistantMessageCommitted {
                message_id,
                blocks,
                usage,
                at_ms,
            },
            committed_at,
        )?;
        delete_frames(&tx, session, stream_id)?;
        tx.commit()?;
        Ok(sequence)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn commit_tool_call_stream(
        &mut self,
        session: &JournalSessionId,
        generation: u64,
        scope: &EventScope,
        stream_id: &str,
        tool_call_id: ToolCallId,
        name: String,
        policy: PolicyProvenance,
        at_ms: Option<u64>,
        committed_at: u64,
    ) -> JournalResult<SequenceId> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_generation(&tx, session, generation)?;
        let frames = read_frames(&tx, session, stream_id)?;
        if frames.is_empty() || frames.iter().any(|(kind, _)| *kind != StreamFrameKind::ToolArguments)
        {
            return Err(JournalError::InvalidStream(
                "tool call needs one or more tool_arguments frames only".into(),
            ));
        }
        let bytes: Vec<u8> = frames.into_iter().flat_map(|(_, bytes)| bytes).collect();
        let text = std::str::from_utf8(&bytes)
            .map_err(|error| JournalError::InvalidToolArguments(error.to_string()))?;
        let arguments: serde_json::Value = serde_json::from_str(text)
            .map_err(|error| JournalError::InvalidToolArguments(error.to_string()))?;
        validate_tool_arguments(&arguments)?;
        let sequence = append_tx(
            &tx,
            session,
            generation,
            scope,
            &JournalEvent::ToolCallPrepared {
                tool_call_id,
                name,
                arguments,
                policy,
                at_ms,
            },
            committed_at,
        )?;
        delete_frames(&tx, session, stream_id)?;
        tx.commit()?;
        Ok(sequence)
    }

    pub fn discard_stream(
        &mut self,
        session: &JournalSessionId,
        stream_id: &str,
    ) -> JournalResult<()> {
        self.conn.execute(
            "DELETE FROM native_stream_frames WHERE session_id = ?1 AND stream_id = ?2",
            params![session.as_str(), stream_id],
        )?;
        Ok(())
    }

    pub fn put_artifact(
        &mut self,
        media_type: &str,
        bytes: &[u8],
        created_at: u64,
    ) -> JournalResult<ContentRef> {
        let sha256 = hex_sha256(bytes);
        self.conn.execute(
            "INSERT OR IGNORE INTO native_artifacts
                 (sha256, media_type, byte_len, content, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                sha256,
                media_type,
                sql_usize(bytes.len(), "artifact byte length")?,
                bytes,
                sql_u64(created_at, "created_at")?,
            ],
        )?;
        Ok(ContentRef::Artifact {
            sha256,
            byte_len: bytes.len() as u64,
            content_hash: input_hash(&String::from_utf8_lossy(bytes)),
            media_type: media_type.to_string(),
        })
    }

    pub fn read_artifact(&self, sha256: &str) -> JournalResult<Option<Vec<u8>>> {
        self.conn
            .query_row(
                "SELECT content FROM native_artifacts WHERE sha256 = ?1",
                [sha256],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn store_continuation(
        &mut self,
        session: &JournalSessionId,
        attempt: &RequestAttemptId,
        identity: &ContinuationIdentity,
        envelope: &[u8],
        created_at: u64,
    ) -> JournalResult<()> {
        let session_identity = self.session(session)?;
        if ContinuationIdentity::from(&session_identity.route) != *identity {
            return Err(JournalError::ContinuationMismatch);
        }
        self.conn.execute(
            "INSERT INTO native_continuations (
                 session_id, attempt_id, route_id, provider_id, endpoint_id,
                 account_id, protocol, model_vendor, model_id, envelope, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(session_id, attempt_id) DO UPDATE SET
                 route_id = excluded.route_id,
                 provider_id = excluded.provider_id,
                 endpoint_id = excluded.endpoint_id,
                 account_id = excluded.account_id,
                 protocol = excluded.protocol,
                 model_vendor = excluded.model_vendor,
                 model_id = excluded.model_id,
                 envelope = excluded.envelope,
                 created_at = excluded.created_at",
            params![
                session.as_str(),
                attempt.as_str(),
                identity.route.as_ref(),
                identity.provider.as_ref(),
                identity.endpoint.as_ref(),
                identity.account.as_ref(),
                protocol_name(identity.protocol),
                identity.model.vendor,
                identity.model.id,
                envelope,
                sql_u64(created_at, "created_at")?,
            ],
        )?;
        Ok(())
    }

    pub fn load_continuation(
        &self,
        session: &JournalSessionId,
        attempt: &RequestAttemptId,
        identity: &ContinuationIdentity,
    ) -> JournalResult<Option<Vec<u8>>> {
        let row: Option<(String, String, String, String, String, String, String, Vec<u8>)> = self
            .conn
            .query_row(
                "SELECT route_id, provider_id, endpoint_id, account_id, protocol,
                        model_vendor, model_id, envelope
                 FROM native_continuations WHERE session_id = ?1 AND attempt_id = ?2",
                params![session.as_str(), attempt.as_str()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                    ))
                },
            )
            .optional()?;
        let Some((route, provider, endpoint, account, protocol, vendor, model, envelope)) = row
        else {
            return Ok(None);
        };
        let matches = route == identity.route.as_ref()
            && provider == identity.provider.as_ref()
            && endpoint == identity.endpoint.as_ref()
            && account == identity.account.as_ref()
            && protocol == protocol_name(identity.protocol)
            && vendor == identity.model.vendor
            && model == identity.model.id;
        if !matches {
            return Err(JournalError::ContinuationMismatch);
        }
        Ok(Some(envelope))
    }

    pub fn events(&self, session: &JournalSessionId) -> JournalResult<Vec<StoredEvent>> {
        // Establish that the session exists before an empty query could
        // otherwise make unknown and no-events sessions indistinguishable.
        let _ = self.session(session)?;
        read_events(&self.conn, session)
    }

    pub fn replay(&self, session: &JournalSessionId) -> JournalResult<ConversationState> {
        let identity = self.session(session)?;
        let events = read_events(&self.conn, session)?;
        ConversationState::reduce(identity, &events)
    }

    /// Rebuilds the lossy scoring projection from authoritative native facts.
    /// `rot.rs` remains untouched and pure; this is an adapter at the same
    /// boundary as every external harness transcript parser.
    pub fn normalized_events(
        &self,
        session: &JournalSessionId,
    ) -> JournalResult<Vec<NormalizedEvent>> {
        let state = self.replay(session)?;
        let events = read_events(&self.conn, session)?;
        let mut projected = vec![NormalizedEvent::ModelId {
            id: state.identity.route.model.id.clone(),
        }];
        for stored in events {
            match stored.event {
                JournalEvent::InputAcknowledged { text, at_ms, .. } => {
                    projected.push(NormalizedEvent::TurnStart { at_ms });
                    projected.push(NormalizedEvent::UserText {
                        byte_len: text.len() as u64,
                    });
                }
                JournalEvent::AssistantMessageCommitted {
                    blocks,
                    usage,
                    at_ms,
                    ..
                } => {
                    let mut text = String::new();
                    let mut thinking_bytes = 0u64;
                    for block in blocks {
                        match block {
                            AssistantBlock::Text { text: block }
                            | AssistantBlock::Refusal { text: block } => text.push_str(&block),
                            AssistantBlock::Thinking { text } => {
                                thinking_bytes = thinking_bytes.saturating_add(text.len() as u64);
                            }
                            AssistantBlock::ToolCall { .. } => {}
                        }
                    }
                    if !text.is_empty() {
                        projected.push(NormalizedEvent::AssistantFirstText { at_ms });
                    }
                    projected.push(NormalizedEvent::AssistantFinal {
                        text,
                        input_tokens: usage
                            .as_ref()
                            .and_then(|id| state.usage.get(id))
                            .map_or(0, |usage| usage.input_tokens),
                        at_ms,
                    });
                    if thinking_bytes > 0 {
                        projected.push(NormalizedEvent::AssistantThinking {
                            byte_len: thinking_bytes,
                        });
                    }
                }
                JournalEvent::ToolCallPrepared {
                    name,
                    arguments,
                    at_ms,
                    ..
                } => {
                    let arguments = serde_json::to_string(&arguments)?;
                    projected.push(NormalizedEvent::ToolCall {
                        name,
                        input_hash: input_hash(&arguments),
                        at_ms,
                    });
                }
                JournalEvent::ToolExecution {
                    state: execution_state,
                    result,
                    detail,
                    at_ms,
                    ..
                } if matches!(execution_state, ExecutionState::Completed | ExecutionState::Failed) => {
                    let is_error = execution_state == ExecutionState::Failed;
                    projected.push(NormalizedEvent::ToolResult { is_error });
                    if is_error && let Some(detail) = detail.as_deref() {
                        projected.push(NormalizedEvent::ToolErrorText {
                            hash: error_text_hash(detail),
                        });
                    }
                    if let Some(result) = result {
                        projected.push(NormalizedEvent::ToolResultSize {
                            byte_len: result.byte_len(),
                            content_hash: result.normalized_hash(),
                        });
                    }
                    projected.push(NormalizedEvent::ToolResultTimestamp { at_ms });
                }
                JournalEvent::Checkpoint {
                    kind: CheckpointKind::Compaction,
                    ..
                } => projected.push(NormalizedEvent::Compaction),
                JournalEvent::UsageRecorded { .. }
                | JournalEvent::ToolExecution { .. }
                | JournalEvent::TaskReceipt { .. }
                | JournalEvent::Checkpoint { .. }
                | JournalEvent::GenerationAdvanced { .. }
                | JournalEvent::SessionEnded { .. } => {}
            }
        }
        Ok(projected)
    }

    pub fn prune_completed_before(&mut self, cutoff: u64) -> JournalResult<usize> {
        let deleted = self.conn.execute(
            "DELETE FROM native_sessions WHERE completed_at IS NOT NULL AND completed_at < ?1",
            [sql_u64(cutoff, "retention cutoff")?],
        )?;
        Ok(deleted)
    }

    fn append(
        &mut self,
        session: &JournalSessionId,
        generation: u64,
        scope: &EventScope,
        event: JournalEvent,
        committed_at: u64,
    ) -> JournalResult<SequenceId> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let sequence = append_tx(
            &tx,
            session,
            generation,
            scope,
            &event,
            committed_at,
        )?;
        tx.commit()?;
        Ok(sequence)
    }
}

fn secure_database_file(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn verify_integrity(conn: &Connection) -> JournalResult<()> {
    let result: String = conn
        .query_row("PRAGMA quick_check(1)", [], |row| row.get(0))
        .map_err(|error| JournalError::Corrupt(error.to_string()))?;
    if result.eq_ignore_ascii_case("ok") {
        Ok(())
    } else {
        Err(JournalError::Corrupt(result))
    }
}

fn migrate(conn: &Connection) -> JournalResult<()> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version > JOURNAL_SCHEMA_VERSION {
        return Err(JournalError::UnsupportedSchema(version));
    }
    if version == JOURNAL_SCHEMA_VERSION {
        return Ok(());
    }
    let table_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_schema
         WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
        [],
        |row| row.get(0),
    )?;
    if table_count != 0 {
        return Err(JournalError::UnversionedSchema);
    }
    conn.execute_batch(
        "BEGIN IMMEDIATE;
         CREATE TABLE native_sessions (
             session_id TEXT PRIMARY KEY,
             seat_id TEXT NOT NULL,
             generation INTEGER NOT NULL CHECK (generation >= 0),
             task_id TEXT,
             route_id TEXT NOT NULL,
             provider_id TEXT NOT NULL,
             endpoint_id TEXT NOT NULL,
             account_id TEXT NOT NULL,
             billing_pool_id TEXT NOT NULL,
             protocol TEXT NOT NULL,
             model_vendor TEXT NOT NULL,
             model_id TEXT NOT NULL,
             created_at INTEGER NOT NULL,
             updated_at INTEGER NOT NULL,
             next_sequence INTEGER NOT NULL DEFAULT 0 CHECK (next_sequence >= 0),
             completed_at INTEGER
         );
         CREATE TABLE native_events (
             session_id TEXT NOT NULL REFERENCES native_sessions(session_id) ON DELETE CASCADE,
             sequence INTEGER NOT NULL CHECK (sequence > 0),
             generation INTEGER NOT NULL CHECK (generation >= 0),
             event_type TEXT NOT NULL,
             turn_id TEXT,
             attempt_id TEXT,
             message_id TEXT,
             tool_call_id TEXT,
             execution_id TEXT,
             usage_id TEXT,
             task_id TEXT,
             checkpoint_id TEXT,
             payload_json TEXT NOT NULL,
             committed_at INTEGER NOT NULL,
             PRIMARY KEY (session_id, sequence)
         );
         CREATE UNIQUE INDEX native_events_message_unique
             ON native_events(session_id, message_id)
             WHERE message_id IS NOT NULL;
         CREATE UNIQUE INDEX native_events_usage_unique
             ON native_events(session_id, usage_id)
             WHERE usage_id IS NOT NULL;
         CREATE UNIQUE INDEX native_events_tool_call_unique
             ON native_events(session_id, tool_call_id)
             WHERE event_type = 'tool_call_prepared';
         CREATE INDEX native_events_execution
             ON native_events(session_id, execution_id, sequence);
         CREATE INDEX native_events_task
             ON native_events(session_id, task_id, sequence);
         CREATE TABLE native_stream_frames (
             session_id TEXT NOT NULL REFERENCES native_sessions(session_id) ON DELETE CASCADE,
             stream_id TEXT NOT NULL,
             frame_index INTEGER NOT NULL CHECK (frame_index >= 0),
             kind TEXT NOT NULL,
             data BLOB NOT NULL,
             created_at INTEGER NOT NULL,
             PRIMARY KEY (session_id, stream_id, frame_index)
         );
         CREATE TABLE native_continuations (
             session_id TEXT NOT NULL REFERENCES native_sessions(session_id) ON DELETE CASCADE,
             attempt_id TEXT NOT NULL,
             route_id TEXT NOT NULL,
             provider_id TEXT NOT NULL,
             endpoint_id TEXT NOT NULL,
             account_id TEXT NOT NULL,
             protocol TEXT NOT NULL,
             model_vendor TEXT NOT NULL,
             model_id TEXT NOT NULL,
             envelope BLOB NOT NULL,
             created_at INTEGER NOT NULL,
             PRIMARY KEY (session_id, attempt_id)
         );
         CREATE TABLE native_artifacts (
             sha256 TEXT PRIMARY KEY,
             media_type TEXT NOT NULL,
             byte_len INTEGER NOT NULL CHECK (byte_len >= 0),
             content BLOB NOT NULL,
             created_at INTEGER NOT NULL
         );
         PRAGMA user_version = 1;
         COMMIT;",
    )?;
    Ok(())
}

fn append_tx(
    tx: &Transaction<'_>,
    session: &JournalSessionId,
    generation: u64,
    scope: &EventScope,
    event: &JournalEvent,
    committed_at: u64,
) -> JournalResult<SequenceId> {
    let (_, next) = ensure_generation(tx, session, generation)?;
    let sequence = next.checked_add(1).ok_or_else(|| {
        JournalError::Corrupt(format!("sequence exhausted for session {session}"))
    })?;
    let indexed = event.indexed_ids();
    let payload = serde_json::to_string(event)?;
    let result = tx.execute(
        "INSERT INTO native_events (
             session_id, sequence, generation, event_type, turn_id, attempt_id,
             message_id, tool_call_id, execution_id, usage_id, task_id,
             checkpoint_id, payload_json, committed_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            session.as_str(),
            sql_u64(sequence, "sequence")?,
            sql_u64(generation, "generation")?,
            event.kind(),
            scope.turn.as_ref().map(TurnId::as_str),
            scope.attempt.as_ref().map(RequestAttemptId::as_str),
            indexed.message.map(MessageId::as_str),
            indexed.tool_call.map(ToolCallId::as_str),
            indexed.execution.map(ExecutionId::as_str),
            indexed.usage.map(UsageId::as_str),
            indexed.task.map(TaskId::as_str).or_else(|| scope.task.as_ref().map(TaskId::as_str)),
            indexed.checkpoint.map(CheckpointId::as_str),
            payload,
            sql_u64(committed_at, "committed_at")?,
        ],
    );
    if let Err(error) = result {
        if is_constraint(&error) {
            let (kind, value) = duplicate_identity(event);
            return Err(JournalError::DuplicateId { kind, value });
        }
        return Err(error.into());
    }
    tx.execute(
        "UPDATE native_sessions SET next_sequence = ?2, updated_at = ?3 WHERE session_id = ?1",
        params![
            session.as_str(),
            sql_u64(sequence, "sequence")?,
            sql_u64(committed_at, "committed_at")?,
        ],
    )?;
    Ok(SequenceId(sequence))
}

fn duplicate_identity(event: &JournalEvent) -> (&'static str, String) {
    match event {
        JournalEvent::InputAcknowledged { message_id, .. }
        | JournalEvent::AssistantMessageCommitted { message_id, .. } => {
            ("MessageId", message_id.to_string())
        }
        JournalEvent::UsageRecorded { usage } => ("UsageId", usage.id.to_string()),
        JournalEvent::ToolCallPrepared { tool_call_id, .. } => {
            ("ToolCallId", tool_call_id.to_string())
        }
        _ => ("event", event.kind().to_string()),
    }
}

fn ensure_generation(
    tx: &Transaction<'_>,
    session: &JournalSessionId,
    generation: u64,
) -> JournalResult<(u64, u64)> {
    let row: Option<(i64, i64)> = tx
        .query_row(
            "SELECT generation, next_sequence FROM native_sessions WHERE session_id = ?1",
            [session.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((actual, next)) = row else {
        return Err(JournalError::UnknownSession(session.to_string()));
    };
    let actual = rust_u64(actual, "generation")?;
    if actual != generation {
        return Err(JournalError::StaleGeneration {
            expected: actual,
            got: generation,
        });
    }
    Ok((actual, rust_u64(next, "next_sequence")?))
}

fn require_tool_call(
    tx: &Transaction<'_>,
    session: &JournalSessionId,
    tool_call: &ToolCallId,
) -> JournalResult<()> {
    let exists: bool = tx.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM native_events
             WHERE session_id = ?1 AND event_type = 'tool_call_prepared' AND tool_call_id = ?2
         )",
        params![session.as_str(), tool_call.as_str()],
        |row| row.get(0),
    )?;
    if exists {
        Ok(())
    } else {
        Err(JournalError::UnknownToolCall(tool_call.to_string()))
    }
}

fn require_usage(
    tx: &Transaction<'_>,
    session: &JournalSessionId,
    usage: &UsageId,
) -> JournalResult<()> {
    let exists: bool = tx.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM native_events
             WHERE session_id = ?1 AND event_type = 'usage_recorded' AND usage_id = ?2
         )",
        params![session.as_str(), usage.as_str()],
        |row| row.get(0),
    )?;
    if exists {
        Ok(())
    } else {
        Err(JournalError::Corrupt(format!(
            "assistant message references unknown usage id {usage}"
        )))
    }
}

fn latest_execution(
    tx: &Transaction<'_>,
    session: &JournalSessionId,
    execution: &ExecutionId,
) -> JournalResult<Option<ExecutionRecord>> {
    let row: Option<(i64, String)> = tx
        .query_row(
            "SELECT sequence, payload_json FROM native_events
             WHERE session_id = ?1 AND event_type = 'tool_execution' AND execution_id = ?2
             ORDER BY sequence DESC LIMIT 1",
            params![session.as_str(), execution.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((sequence, payload)) = row else {
        return Ok(None);
    };
    let event: JournalEvent = serde_json::from_str(&payload).map_err(|error| {
        JournalError::Corrupt(format!("execution event at sequence {sequence}: {error}"))
    })?;
    let JournalEvent::ToolExecution {
        tool_call_id,
        state,
        result,
        detail,
        ..
    } = event
    else {
        return Err(JournalError::Corrupt(format!(
            "execution index points to non-execution event at sequence {sequence}"
        )));
    };
    Ok(Some(ExecutionRecord {
        sequence: SequenceId(rust_u64(sequence, "sequence")?),
        tool_call: tool_call_id,
        state,
        result,
        detail,
    }))
}

fn latest_started_executions(
    tx: &Transaction<'_>,
    session: &JournalSessionId,
) -> JournalResult<Vec<(ExecutionId, ToolCallId)>> {
    let mut stmt = tx.prepare(
        "SELECT e.execution_id, e.payload_json
         FROM native_events e
         WHERE e.session_id = ?1
           AND e.event_type = 'tool_execution'
           AND e.sequence = (
               SELECT MAX(newer.sequence) FROM native_events newer
               WHERE newer.session_id = e.session_id
                 AND newer.execution_id = e.execution_id
                 AND newer.event_type = 'tool_execution'
           )
         ORDER BY e.sequence",
    )?;
    let rows = stmt.query_map([session.as_str()], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut started = Vec::new();
    for row in rows {
        let (execution_id, payload) = row?;
        let event: JournalEvent = serde_json::from_str(&payload)?;
        if let JournalEvent::ToolExecution {
            tool_call_id,
            state: ExecutionState::Started,
            ..
        } = event
        {
            started.push((ExecutionId::new(execution_id)?, tool_call_id));
        }
    }
    Ok(started)
}

fn validate_execution_transition(
    from: ExecutionState,
    to: ExecutionState,
    result: Option<&ContentRef>,
) -> JournalResult<()> {
    let legal = matches!(
        (from, to),
        (ExecutionState::Prepared, ExecutionState::Started)
            | (ExecutionState::Prepared, ExecutionState::Cancelled)
            | (ExecutionState::Started, ExecutionState::Completed)
            | (ExecutionState::Started, ExecutionState::Failed)
            | (ExecutionState::Started, ExecutionState::Cancelled)
            | (ExecutionState::Started, ExecutionState::OutcomeUnknown)
            | (ExecutionState::OutcomeUnknown, ExecutionState::Completed)
            | (ExecutionState::OutcomeUnknown, ExecutionState::Failed)
            | (ExecutionState::OutcomeUnknown, ExecutionState::Cancelled)
    );
    if !legal {
        return Err(JournalError::InvalidExecutionTransition { from, to });
    }
    if matches!(to, ExecutionState::Completed | ExecutionState::Failed) && result.is_none() {
        return Err(JournalError::InvalidStream(format!(
            "{to:?} execution requires an authoritative result"
        )));
    }
    if !matches!(to, ExecutionState::Completed | ExecutionState::Failed) && result.is_some() {
        return Err(JournalError::InvalidStream(format!(
            "{to:?} execution cannot carry a result"
        )));
    }
    Ok(())
}

fn validate_tool_arguments(arguments: &serde_json::Value) -> JournalResult<()> {
    if arguments.is_object() {
        Ok(())
    } else {
        Err(JournalError::InvalidToolArguments(
            "top-level value must be a JSON object".into(),
        ))
    }
}

fn read_frames(
    tx: &Transaction<'_>,
    session: &JournalSessionId,
    stream_id: &str,
) -> JournalResult<Vec<(StreamFrameKind, Vec<u8>)>> {
    let mut stmt = tx.prepare(
        "SELECT kind, data FROM native_stream_frames
         WHERE session_id = ?1 AND stream_id = ?2 ORDER BY frame_index",
    )?;
    let rows = stmt.query_map(params![session.as_str(), stream_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
    })?;
    let mut frames = Vec::new();
    for row in rows {
        let (kind, bytes) = row?;
        frames.push((StreamFrameKind::parse(&kind)?, bytes));
    }
    Ok(frames)
}

fn delete_frames(
    tx: &Transaction<'_>,
    session: &JournalSessionId,
    stream_id: &str,
) -> JournalResult<()> {
    tx.execute(
        "DELETE FROM native_stream_frames WHERE session_id = ?1 AND stream_id = ?2",
        params![session.as_str(), stream_id],
    )?;
    Ok(())
}

fn assistant_blocks_from_frames(
    frames: &[(StreamFrameKind, Vec<u8>)],
) -> JournalResult<Vec<AssistantBlock>> {
    let mut grouped: Vec<(StreamFrameKind, Vec<u8>)> = Vec::new();
    for (kind, bytes) in frames {
        if *kind == StreamFrameKind::ToolArguments {
            return Err(JournalError::InvalidStream(
                "assistant stream contains tool argument frames".into(),
            ));
        }
        if let Some((last_kind, last_bytes)) = grouped.last_mut()
            && *last_kind == *kind
        {
            last_bytes.extend_from_slice(bytes);
        } else {
            grouped.push((*kind, bytes.clone()));
        }
    }
    grouped
        .into_iter()
        .map(|(kind, bytes)| {
            let text = String::from_utf8(bytes)
                .map_err(|error| JournalError::InvalidStream(error.to_string()))?;
            match kind {
                StreamFrameKind::AssistantText => Ok(AssistantBlock::Text { text }),
                StreamFrameKind::AssistantThinking => Ok(AssistantBlock::Thinking { text }),
                StreamFrameKind::AssistantRefusal => Ok(AssistantBlock::Refusal { text }),
                StreamFrameKind::ToolArguments => unreachable!("rejected above"),
            }
        })
        .collect()
}

fn read_session(conn: &Connection, session: &JournalSessionId) -> JournalResult<SessionIdentity> {
    let raw: Option<RawSession> = conn
        .query_row(
            "SELECT seat_id, generation, task_id, route_id, provider_id, endpoint_id,
                    account_id, billing_pool_id, protocol, model_vendor, model_id,
                    created_at, completed_at
             FROM native_sessions WHERE session_id = ?1",
            [session.as_str()],
            |row| {
                Ok(RawSession {
                    seat: row.get(0)?,
                    generation: row.get(1)?,
                    task: row.get(2)?,
                    route: row.get(3)?,
                    provider: row.get(4)?,
                    endpoint: row.get(5)?,
                    account: row.get(6)?,
                    billing_pool: row.get(7)?,
                    protocol: row.get(8)?,
                    model_vendor: row.get(9)?,
                    model_id: row.get(10)?,
                    created_at: row.get(11)?,
                    completed_at: row.get(12)?,
                })
            },
        )
        .optional()?;
    let Some(raw) = raw else {
        return Err(JournalError::UnknownSession(session.to_string()));
    };
    Ok(SessionIdentity {
        session: session.clone(),
        seat: SeatId::new(raw.seat)?,
        generation: rust_u64(raw.generation, "generation")?,
        task: raw.task.map(TaskId::new).transpose()?,
        route: RouteIdentity {
            route: raw.route.parse().map_err(JournalError::Corrupt)?,
            provider: raw.provider.parse().map_err(JournalError::Corrupt)?,
            endpoint: raw.endpoint.parse().map_err(JournalError::Corrupt)?,
            account: raw.account.parse().map_err(JournalError::Corrupt)?,
            billing_pool: raw.billing_pool.parse().map_err(JournalError::Corrupt)?,
            protocol: parse_protocol(&raw.protocol)?,
            model: ModelId {
                vendor: raw.model_vendor,
                id: raw.model_id,
            },
        },
        created_at: rust_u64(raw.created_at, "created_at")?,
        completed_at: raw
            .completed_at
            .map(|value| rust_u64(value, "completed_at"))
            .transpose()?,
    })
}

struct RawSession {
    seat: String,
    generation: i64,
    task: Option<String>,
    route: String,
    provider: String,
    endpoint: String,
    account: String,
    billing_pool: String,
    protocol: String,
    model_vendor: String,
    model_id: String,
    created_at: i64,
    completed_at: Option<i64>,
}

fn read_events(conn: &Connection, session: &JournalSessionId) -> JournalResult<Vec<StoredEvent>> {
    let mut stmt = conn.prepare(
        "SELECT sequence, generation, event_type, turn_id, attempt_id, task_id,
                payload_json, committed_at
         FROM native_events WHERE session_id = ?1 ORDER BY sequence",
    )?;
    let rows = stmt.query_map([session.as_str()], |row| {
        Ok(RawEvent {
            sequence: row.get(0)?,
            generation: row.get(1)?,
            event_type: row.get(2)?,
            turn: row.get(3)?,
            attempt: row.get(4)?,
            task: row.get(5)?,
            payload: row.get(6)?,
            committed_at: row.get(7)?,
        })
    })?;
    let mut events = Vec::new();
    for row in rows {
        let raw = row?;
        let sequence = rust_u64(raw.sequence, "sequence")?;
        let event: JournalEvent = serde_json::from_str(&raw.payload).map_err(|error| {
            JournalError::Corrupt(format!("event payload at sequence {sequence}: {error}"))
        })?;
        if event.kind() != raw.event_type {
            return Err(JournalError::Corrupt(format!(
                "event type {:?} disagrees with payload {} at sequence {sequence}",
                raw.event_type,
                event.kind()
            )));
        }
        events.push(StoredEvent {
            sequence: SequenceId(sequence),
            generation: rust_u64(raw.generation, "generation")?,
            scope: EventScope {
                turn: raw.turn.map(TurnId::new).transpose()?,
                attempt: raw.attempt.map(RequestAttemptId::new).transpose()?,
                task: raw.task.map(TaskId::new).transpose()?,
            },
            committed_at: rust_u64(raw.committed_at, "committed_at")?,
            event,
        });
    }
    Ok(events)
}

struct RawEvent {
    sequence: i64,
    generation: i64,
    event_type: String,
    turn: Option<String>,
    attempt: Option<String>,
    task: Option<String>,
    payload: String,
    committed_at: i64,
}

fn protocol_name(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::AnthropicMessages => "anthropic_messages",
        Protocol::OpenAiResponses => "openai_responses",
        Protocol::OpenAiChatCompatible => "openai_chat_compatible",
        Protocol::GoogleGenerativeAi => "google_generative_ai",
        Protocol::GoogleVertex => "google_vertex",
        Protocol::AwsBedrock => "aws_bedrock",
    }
}

fn parse_protocol(value: &str) -> JournalResult<Protocol> {
    match value {
        "anthropic_messages" => Ok(Protocol::AnthropicMessages),
        "openai_responses" => Ok(Protocol::OpenAiResponses),
        "openai_chat_compatible" => Ok(Protocol::OpenAiChatCompatible),
        "google_generative_ai" => Ok(Protocol::GoogleGenerativeAi),
        "google_vertex" => Ok(Protocol::GoogleVertex),
        "aws_bedrock" => Ok(Protocol::AwsBedrock),
        other => Err(JournalError::Corrupt(format!(
            "unknown provider protocol {other:?}"
        ))),
    }
}

fn sql_u64(value: u64, field: &'static str) -> JournalResult<i64> {
    i64::try_from(value).map_err(|_| JournalError::InvalidNumber { field, value })
}

fn sql_usize(value: usize, field: &'static str) -> JournalResult<i64> {
    let value = u64::try_from(value).unwrap_or(u64::MAX);
    sql_u64(value, field)
}

fn rust_u64(value: i64, field: &'static str) -> JournalResult<u64> {
    u64::try_from(value)
        .map_err(|_| JournalError::Corrupt(format!("negative {field} value {value}")))
}

fn is_constraint(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(code, _)
            if code.code == rusqlite::ffi::ErrorCode::ConstraintViolation
    )
}

fn hex_sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn jid(value: &str) -> JournalSessionId {
        JournalSessionId::new(value).unwrap()
    }

    fn turn(value: &str) -> TurnId {
        TurnId::new(value).unwrap()
    }

    fn message(value: &str) -> MessageId {
        MessageId::new(value).unwrap()
    }

    fn tool(value: &str) -> ToolCallId {
        ToolCallId::new(value).unwrap()
    }

    fn execution(value: &str) -> ExecutionId {
        ExecutionId::new(value).unwrap()
    }

    fn usage(value: &str) -> UsageId {
        UsageId::new(value).unwrap()
    }

    fn attempt(value: &str) -> RequestAttemptId {
        RequestAttemptId::new(value).unwrap()
    }

    fn identity(session: &str) -> SessionIdentity {
        SessionIdentity {
            session: jid(session),
            seat: SeatId::new(format!("seat-{session}")).unwrap(),
            generation: 7,
            task: Some(TaskId::new("task-1").unwrap()),
            route: RouteIdentity {
                route: RouteId::new("work-openai").unwrap(),
                provider: ProviderId::new("openai").unwrap(),
                endpoint: EndpointId::new("openai").unwrap(),
                account: AccountId::new("work").unwrap(),
                billing_pool: BillingPoolId::new("work-pool").unwrap(),
                protocol: Protocol::OpenAiResponses,
                model: ModelId {
                    vendor: "openai".into(),
                    id: "gpt-5.6-sol".into(),
                },
            },
            created_at: 1,
            completed_at: None,
        }
    }

    fn policy() -> PolicyProvenance {
        PolicyProvenance {
            fingerprint: "sha256:policy".into(),
            source: "operator+repository".into(),
            decision: "allow".into(),
            scope: "workspace-write".into(),
        }
    }

    fn scope() -> EventScope {
        EventScope {
            turn: Some(turn("turn-1")),
            attempt: Some(attempt("attempt-1")),
            task: Some(TaskId::new("task-1").unwrap()),
        }
    }

    fn journal() -> (tempfile::TempDir, Journal) {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open_path(dir.path().join("journal.sqlite")).unwrap();
        (dir, journal)
    }

    #[test]
    fn schema_is_versioned_and_unknown_newer_versions_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.sqlite");
        let journal = Journal::open_path(&path).unwrap();
        let version: i64 = journal
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, JOURNAL_SCHEMA_VERSION);
        drop(journal);

        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", JOURNAL_SCHEMA_VERSION + 1)
            .unwrap();
        drop(conn);
        let error = Journal::open_path(&path).unwrap_err();
        assert!(matches!(
            error,
            JournalError::UnsupportedSchema(version) if version == JOURNAL_SCHEMA_VERSION + 1
        ));
    }

    #[test]
    fn unversioned_tables_are_not_guessed_into_the_native_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.sqlite");
        let conn = Connection::open(&path).unwrap();
        conn.execute("CREATE TABLE mystery (value TEXT)", []).unwrap();
        drop(conn);
        assert!(matches!(
            Journal::open_path(path).unwrap_err(),
            JournalError::UnversionedSchema
        ));
    }

    #[test]
    fn corrupt_database_is_reported_instead_of_recreated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.sqlite");
        std::fs::write(&path, b"not a sqlite database").unwrap();
        assert!(matches!(
            Journal::open_path(path).unwrap_err(),
            JournalError::Corrupt(_)
        ));
    }

    #[test]
    fn replay_reconstructs_conversation_execution_usage_and_generation() {
        let (_dir, mut journal) = journal();
        let session = jid("session-1");
        journal.create_session(&identity(session.as_str())).unwrap();
        let scope = scope();

        journal
            .record_usage(
                &session,
                7,
                &scope,
                UsageRecord {
                    id: usage("usage-1"),
                    input_tokens: 123,
                    cache_creation_input_tokens: 4,
                    cache_read_input_tokens: 80,
                    output_tokens: 20,
                    provider_request_id: Some("req-provider-1".into()),
                    estimated: false,
                },
                2,
            )
            .unwrap();
        journal
            .acknowledge_input(
                &session,
                7,
                &scope,
                message("user-1"),
                "inspect this".into(),
                false,
                Some(2_000),
                2,
            )
            .unwrap();
        journal
            .record_assistant_message(
                &session,
                7,
                &scope,
                message("assistant-1"),
                vec![
                    AssistantBlock::Thinking {
                        text: "private working state".into(),
                    },
                    AssistantBlock::Text {
                        text: "I will inspect it.".into(),
                    },
                    AssistantBlock::ToolCall {
                        tool_call: tool("call-1"),
                    },
                ],
                Some(usage("usage-1")),
                Some(2_100),
                3,
            )
            .unwrap();
        journal
            .prepare_tool_call(
                &session,
                7,
                &scope,
                tool("call-1"),
                "read".into(),
                json!({"path": "src/main.rs"}),
                policy(),
                Some(2_200),
                4,
            )
            .unwrap();
        journal
            .prepare_execution(
                &session,
                7,
                &scope,
                execution("exec-1"),
                tool("call-1"),
                Some(2_300),
                5,
            )
            .unwrap();
        journal
            .transition_execution(
                &session,
                7,
                &scope,
                &execution("exec-1"),
                ExecutionState::Started,
                None,
                None,
                Some(2_400),
                6,
            )
            .unwrap();
        journal
            .transition_execution(
                &session,
                7,
                &scope,
                &execution("exec-1"),
                ExecutionState::Completed,
                Some(ContentRef::Inline {
                    text: "fn main() {}".into(),
                }),
                None,
                Some(2_500),
                7,
            )
            .unwrap();
        journal
            .record_task_receipt(
                &session,
                7,
                &scope,
                TaskId::new("task-1").unwrap(),
                TaskReceiptState::Completed,
                json!({"result": "artifact-1"}),
                8,
            )
            .unwrap();
        journal
            .record_checkpoint(
                &session,
                7,
                &scope,
                CheckpointId::new("checkpoint-1").unwrap(),
                CheckpointKind::Compaction,
                json!({"summary": "portable"}),
                9,
            )
            .unwrap();
        journal.advance_generation(&session, 7, 8, 10).unwrap();

        let first = journal.replay(&session).unwrap();
        let second = journal.replay(&session).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.identity.generation, 8);
        assert_eq!(first.last_sequence, SequenceId(10));
        assert_eq!(first.messages.len(), 2);
        assert_eq!(first.usage[&usage("usage-1")].input_tokens, 123);
        assert_eq!(first.tool_calls[&tool("call-1")].name, "read");
        assert_eq!(
            first.executions[&execution("exec-1")].state,
            ExecutionState::Completed
        );
        assert_eq!(first.task_receipts[&TaskId::new("task-1").unwrap()].len(), 1);
        assert_eq!(
            first.checkpoints[&CheckpointId::new("checkpoint-1").unwrap()].kind,
            CheckpointKind::Compaction
        );

        let events = journal.events(&session).unwrap();
        assert_eq!(
            events.iter().map(|event| event.sequence.0).collect::<Vec<_>>(),
            (1..=10).collect::<Vec<_>>()
        );
        let normalized = journal.normalized_events(&session).unwrap();
        assert!(matches!(
            normalized.first(),
            Some(NormalizedEvent::ModelId { id }) if id == "gpt-5.6-sol"
        ));
        assert!(normalized.iter().any(|event| matches!(
            event,
            NormalizedEvent::AssistantFinal { input_tokens: 123, .. }
        )));
        assert!(normalized.iter().any(|event| matches!(
            event,
            NormalizedEvent::ToolResult { is_error: false }
        )));
        assert!(normalized
            .iter()
            .any(|event| matches!(event, NormalizedEvent::Compaction)));
    }

    #[test]
    fn stale_generation_cannot_append_or_advance() {
        let (_dir, mut journal) = journal();
        let session = jid("session-stale");
        journal.create_session(&identity(session.as_str())).unwrap();
        let error = journal
            .acknowledge_input(
                &session,
                6,
                &EventScope::default(),
                message("message-1"),
                "stale".into(),
                false,
                None,
                2,
            )
            .unwrap_err();
        assert!(matches!(
            error,
            JournalError::StaleGeneration {
                expected: 7,
                got: 6
            }
        ));
        assert!(journal.events(&session).unwrap().is_empty());
    }

    #[test]
    fn duplicate_message_and_tool_ids_do_not_create_phantom_events() {
        let (_dir, mut journal) = journal();
        let session = jid("session-duplicates");
        journal.create_session(&identity(session.as_str())).unwrap();
        journal
            .acknowledge_input(
                &session,
                7,
                &EventScope::default(),
                message("same-message"),
                "one".into(),
                false,
                None,
                2,
            )
            .unwrap();
        assert!(matches!(
            journal
                .record_assistant_message(
                    &session,
                    7,
                    &EventScope::default(),
                    message("same-message"),
                    Vec::new(),
                    None,
                    None,
                    3,
                )
                .unwrap_err(),
            JournalError::DuplicateId { .. }
        ));
        assert_eq!(journal.events(&session).unwrap().len(), 1);
    }

    #[test]
    fn execution_intent_precedes_effect_and_crash_recovery_never_retries_blindly() {
        let (_dir, mut journal) = journal();
        let session = jid("session-crash");
        journal.create_session(&identity(session.as_str())).unwrap();
        journal
            .prepare_tool_call(
                &session,
                7,
                &EventScope::default(),
                tool("call-1"),
                "shell".into(),
                json!({"command": "git status"}),
                policy(),
                None,
                2,
            )
            .unwrap();
        journal
            .prepare_execution(
                &session,
                7,
                &EventScope::default(),
                execution("prepared"),
                tool("call-1"),
                None,
                3,
            )
            .unwrap();
        journal
            .prepare_execution(
                &session,
                7,
                &EventScope::default(),
                execution("started"),
                tool("call-1"),
                None,
                4,
            )
            .unwrap();
        journal
            .transition_execution(
                &session,
                7,
                &EventScope::default(),
                &execution("started"),
                ExecutionState::Started,
                None,
                None,
                None,
                5,
            )
            .unwrap();

        let reconciled = journal
            .reconcile_started_as_unknown(&session, 7, None, 6)
            .unwrap();
        assert_eq!(reconciled, vec![execution("started")]);
        let state = journal.replay(&session).unwrap();
        assert_eq!(
            state.executions[&execution("prepared")].state,
            ExecutionState::Prepared
        );
        assert_eq!(
            state.executions[&execution("started")].state,
            ExecutionState::OutcomeUnknown
        );
        assert!(journal
            .transition_execution(
                &session,
                7,
                &EventScope::default(),
                &execution("started"),
                ExecutionState::Started,
                None,
                None,
                None,
                7,
            )
            .is_err());
        journal
            .transition_execution(
                &session,
                7,
                &EventScope::default(),
                &execution("started"),
                ExecutionState::Completed,
                Some(ContentRef::Inline {
                    text: "reconciled result".into(),
                }),
                Some("provider idempotency receipt matched".into()),
                None,
                8,
            )
            .unwrap();
    }

    #[test]
    fn terminal_execution_requires_an_authoritative_result() {
        let (_dir, mut journal) = journal();
        let session = jid("session-result");
        journal.create_session(&identity(session.as_str())).unwrap();
        journal
            .prepare_tool_call(
                &session,
                7,
                &EventScope::default(),
                tool("call-1"),
                "read".into(),
                json!({}),
                policy(),
                None,
                2,
            )
            .unwrap();
        journal
            .prepare_execution(
                &session,
                7,
                &EventScope::default(),
                execution("exec-1"),
                tool("call-1"),
                None,
                3,
            )
            .unwrap();
        journal
            .transition_execution(
                &session,
                7,
                &EventScope::default(),
                &execution("exec-1"),
                ExecutionState::Started,
                None,
                None,
                None,
                4,
            )
            .unwrap();
        assert!(journal
            .transition_execution(
                &session,
                7,
                &EventScope::default(),
                &execution("exec-1"),
                ExecutionState::Completed,
                None,
                None,
                None,
                5,
            )
            .is_err());
        assert_eq!(
            journal.replay(&session).unwrap().executions[&execution("exec-1")].state,
            ExecutionState::Started
        );
    }

    #[test]
    fn incomplete_tool_arguments_remain_a_non_executable_draft() {
        let (_dir, mut journal) = journal();
        let session = jid("session-tool-stream");
        journal.create_session(&identity(session.as_str())).unwrap();
        journal
            .append_stream_frame(
                &session,
                7,
                "tool-stream",
                StreamFrameKind::ToolArguments,
                br#"{"path":"src/"#,
                2,
            )
            .unwrap();
        let error = journal
            .commit_tool_call_stream(
                &session,
                7,
                &EventScope::default(),
                "tool-stream",
                tool("call-1"),
                "read".into(),
                policy(),
                None,
                3,
            )
            .unwrap_err();
        assert!(matches!(error, JournalError::InvalidToolArguments(_)));
        assert!(journal.events(&session).unwrap().is_empty());

        journal
            .append_stream_frame(
                &session,
                7,
                "tool-stream",
                StreamFrameKind::ToolArguments,
                br#"main.rs"}"#,
                4,
            )
            .unwrap();
        journal
            .commit_tool_call_stream(
                &session,
                7,
                &EventScope::default(),
                "tool-stream",
                tool("call-1"),
                "read".into(),
                policy(),
                None,
                5,
            )
            .unwrap();
        assert_eq!(
            journal.replay(&session).unwrap().tool_calls[&tool("call-1")].arguments,
            json!({"path": "src/main.rs"})
        );
    }

    #[test]
    fn assistant_frames_are_transient_until_the_completed_message_barrier() {
        let (_dir, mut journal) = journal();
        let session = jid("session-assistant-stream");
        journal.create_session(&identity(session.as_str())).unwrap();
        for (kind, bytes) in [
            (StreamFrameKind::AssistantThinking, b"inspect ".as_slice()),
            (StreamFrameKind::AssistantThinking, b"first".as_slice()),
            (StreamFrameKind::AssistantText, b"Done".as_slice()),
        ] {
            journal
                .append_stream_frame(&session, 7, "message-stream", kind, bytes, 2)
                .unwrap();
        }
        assert!(journal.replay(&session).unwrap().messages.is_empty());
        journal
            .commit_assistant_stream(
                &session,
                7,
                &EventScope::default(),
                "message-stream",
                message("assistant-1"),
                None,
                None,
                3,
            )
            .unwrap();
        assert_eq!(
            journal.replay(&session).unwrap().messages[0].blocks,
            vec![
                AssistantBlock::Thinking {
                    text: "inspect first".into()
                },
                AssistantBlock::Text {
                    text: "Done".into()
                }
            ]
        );
        assert!(journal
            .commit_assistant_stream(
                &session,
                7,
                &EventScope::default(),
                "message-stream",
                message("assistant-2"),
                None,
                None,
                4,
            )
            .is_err());
    }

    #[test]
    fn stream_frames_and_total_drafts_are_bounded() {
        let (_dir, mut journal) = journal();
        let session = jid("session-frame-bounds");
        journal.create_session(&identity(session.as_str())).unwrap();
        let oversized = vec![0; MAX_STREAM_FRAME_BYTES + 1];
        assert!(matches!(
            journal
                .append_stream_frame(
                    &session,
                    7,
                    "stream",
                    StreamFrameKind::AssistantText,
                    &oversized,
                    2,
                )
                .unwrap_err(),
            JournalError::FrameTooLarge { .. }
        ));
    }

    #[test]
    fn provider_continuations_are_lossless_identity_bound_and_not_portable_state() {
        let (_dir, mut journal) = journal();
        let session = jid("session-continuation");
        let session_identity = identity(session.as_str());
        let continuation_identity = ContinuationIdentity::from(&session_identity.route);
        journal.create_session(&session_identity).unwrap();
        let opaque = b"\0provider\xffenvelope\n";
        journal
            .store_continuation(
                &session,
                &attempt("attempt-1"),
                &continuation_identity,
                opaque,
                2,
            )
            .unwrap();
        assert_eq!(
            journal
                .load_continuation(&session, &attempt("attempt-1"), &continuation_identity)
                .unwrap(),
            Some(opaque.to_vec())
        );
        assert!(journal.events(&session).unwrap().is_empty());
        assert!(journal.replay(&session).unwrap().messages.is_empty());

        let mut wrong_model = continuation_identity.clone();
        wrong_model.model.id = "gpt-other".into();
        assert!(matches!(
            journal
                .load_continuation(&session, &attempt("attempt-1"), &wrong_model)
                .unwrap_err(),
            JournalError::ContinuationMismatch
        ));
        assert!(matches!(
            journal
                .store_continuation(
                    &session,
                    &attempt("attempt-2"),
                    &wrong_model,
                    b"wrong",
                    3,
                )
                .unwrap_err(),
            JournalError::ContinuationMismatch
        ));
    }

    #[test]
    fn content_addressed_artifacts_round_trip_without_duplication() {
        let (_dir, mut journal) = journal();
        let first = journal.put_artifact("text/plain", b"result", 1).unwrap();
        let second = journal.put_artifact("text/plain", b"result", 2).unwrap();
        assert_eq!(first, second);
        let ContentRef::Artifact { sha256, .. } = first else {
            panic!("artifact reference");
        };
        assert_eq!(journal.read_artifact(&sha256).unwrap(), Some(b"result".to_vec()));
        let count: i64 = journal
            .conn
            .query_row("SELECT COUNT(*) FROM native_artifacts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn committed_wal_rows_are_visible_to_a_concurrent_reader() {
        let (dir, mut writer) = journal();
        let session = jid("session-reader");
        writer.create_session(&identity(session.as_str())).unwrap();
        let reader = Journal::open_path(dir.path().join("journal.sqlite")).unwrap();
        writer
            .acknowledge_input(
                &session,
                7,
                &EventScope::default(),
                message("message-1"),
                "committed".into(),
                false,
                None,
                2,
            )
            .unwrap();
        assert_eq!(reader.replay(&session).unwrap().messages.len(), 1);
    }

    #[test]
    fn malformed_tail_is_reported_with_its_sequence_and_never_partially_replayed() {
        let (_dir, mut journal) = journal();
        let session = jid("session-corrupt-tail");
        journal.create_session(&identity(session.as_str())).unwrap();
        for number in 1..=2 {
            journal
                .acknowledge_input(
                    &session,
                    7,
                    &EventScope::default(),
                    message(&format!("message-{number}")),
                    format!("message {number}"),
                    false,
                    None,
                    number + 1,
                )
                .unwrap();
        }
        journal
            .conn
            .execute(
                "UPDATE native_events SET payload_json = '{truncated' WHERE session_id = ?1 AND sequence = 2",
                [session.as_str()],
            )
            .unwrap();
        let error = journal.replay(&session).unwrap_err();
        assert!(error.to_string().contains("sequence 2"));
    }

    #[test]
    fn retention_deletes_only_completed_sessions_and_cascades_private_state() {
        let (_dir, mut journal) = journal();
        let old = jid("old-session");
        let live = jid("live-session");
        journal.create_session(&identity(old.as_str())).unwrap();
        journal.create_session(&identity(live.as_str())).unwrap();
        journal
            .acknowledge_input(
                &old,
                7,
                &EventScope::default(),
                message("old-message"),
                "old".into(),
                false,
                None,
                2,
            )
            .unwrap();
        journal.complete_session(&old, 7, "done".into(), 10).unwrap();
        assert_eq!(journal.prune_completed_before(11).unwrap(), 1);
        assert!(matches!(
            journal.replay(&old).unwrap_err(),
            JournalError::UnknownSession(_)
        ));
        assert_eq!(journal.session(&live).unwrap().generation, 7);
    }

    #[cfg(unix)]
    #[test]
    fn journal_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let (_dir, journal) = journal();
        let mode = std::fs::metadata(journal.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
