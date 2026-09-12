//! Native coding tool service (issue #474, roadmap N05).
//!
//! Provider output supplies only a stable tool name and JSON arguments. This
//! module validates that payload against a closed typed registry, converts it
//! to an N04 [`ExecutionAction`], obtains effect-time authorization, and only
//! then reaches filesystem/process code. Large results stream into the
//! existing output store and return opaque retrieval ids. Every invocation
//! returns a bounded receipt with an explicit retry/reconciliation contract.

mod files;
mod process;

use std::collections::BTreeMap;
use std::fmt::Display;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use self::files::{
    ApplyPatchArgs, DirectoryArgs, FileOutcome, GlobArgs, ReadFileArgs, SearchArgs, WriteFileArgs,
};
use self::process::{
    ProcessHandleArgs, ProcessLimits, ProcessManager, ProcessStartArgs, ProcessWaitArgs,
    ProcessWriteArgs,
};
use super::enforcement::{
    ApprovalGrant, ApprovalRequest, Authorization, BrokerError, ExecutionAction, ExecutionBroker,
    ProcessInvocation,
};
use super::journal::{
    ContentRef, EventScope, ExecutionId, ExecutionState, Journal, JournalSessionId, ToolCallId,
};
use crate::commands::ctx::config::CtxConfig;
use crate::commands::ctx::output::{self, CompactionScope, StreamingCapture};
use crate::commands::ctx::state::{self, StateDir};

pub const MAX_TOOL_ARGUMENT_BYTES: usize = 1024 * 1024;
pub const DEFAULT_MAX_PROCESSES: usize = 16;

pub const FILE_READ: &str = "file_read";
pub const DIRECTORY_LIST: &str = "directory_list";
pub const GLOB_SEARCH: &str = "glob_search";
pub const TEXT_SEARCH: &str = "text_search";
pub const FILE_WRITE: &str = "file_write";
pub const APPLY_PATCH: &str = "apply_patch";
pub const PROCESS_START: &str = "process_start";
pub const PROCESS_POLL: &str = "process_poll";
pub const PROCESS_WAIT: &str = "process_wait";
pub const PROCESS_WRITE: &str = "process_write";
pub const PROCESS_TERMINATE: &str = "process_terminate";
pub const OUTPUT_READ: &str = "output_read";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolExecutionMode {
    Immediate,
    BackgroundProcess,
    ProcessControl,
    Retrieval,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceClaimKind {
    ReadRoot,
    WorktreeWrite,
    OutsideWrite,
    GitMetadata,
    Network,
    OutputStore,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CancellationContract {
    BeforeEffect,
    AtomicCommit,
    ProcessTree,
    NotApplicable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryPolicy {
    Safe,
    Reconcile,
    NeverAfterStart,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub capabilities: Vec<String>,
    pub execution_mode: ToolExecutionMode,
    pub resource_claims: Vec<ResourceClaimKind>,
    pub cancellation: CancellationContract,
    pub retry: RetryPolicy,
    pub errors: Vec<ToolErrorCode>,
}

#[derive(Clone, Debug, Default)]
pub struct ToolRegistry {
    definitions: BTreeMap<String, ToolDefinition>,
}

impl ToolRegistry {
    pub fn native() -> Self {
        let mut registry = Self::default();
        for definition in native_definitions() {
            let previous = registry
                .definitions
                .insert(definition.name.clone(), definition);
            debug_assert!(previous.is_none(), "native tool names must be unique");
        }
        registry
    }

    pub fn get(&self, name: &str) -> Option<&ToolDefinition> {
        self.definitions.get(name)
    }

    pub fn definitions(&self) -> impl Iterator<Item = &ToolDefinition> {
        self.definitions.values()
    }

    fn parse(&self, name: &str, arguments: Value) -> Result<ParsedTool, ToolError> {
        if self.get(name).is_none() {
            return Err(ToolError::new(
                ToolErrorCode::UnknownTool,
                format!("unknown native tool {name:?}"),
            ));
        }
        let bytes = serde_json::to_vec(&arguments)
            .map_err(ToolError::external)?
            .len();
        if bytes > MAX_TOOL_ARGUMENT_BYTES {
            return Err(ToolError::new(
                ToolErrorCode::InvalidArguments,
                format!("tool arguments are {bytes} bytes; limit is {MAX_TOOL_ARGUMENT_BYTES}"),
            ));
        }
        if !arguments.is_object() {
            return Err(ToolError::new(
                ToolErrorCode::InvalidArguments,
                "tool arguments must be one complete JSON object",
            ));
        }
        macro_rules! parse {
            ($kind:ident, $ty:ty) => {
                serde_json::from_value::<$ty>(arguments)
                    .map(ParsedTool::$kind)
                    .map_err(|error| {
                        ToolError::new(ToolErrorCode::InvalidArguments, error.to_string())
                    })
            };
        }
        let parsed = match name {
            FILE_READ => parse!(ReadFile, ReadFileArgs),
            DIRECTORY_LIST => parse!(DirectoryList, DirectoryArgs),
            GLOB_SEARCH => parse!(Glob, GlobArgs),
            TEXT_SEARCH => parse!(Search, SearchArgs),
            FILE_WRITE => parse!(WriteFile, WriteFileArgs),
            APPLY_PATCH => parse!(ApplyPatch, ApplyPatchArgs),
            PROCESS_START => parse!(ProcessStart, ProcessStartArgs),
            PROCESS_POLL => parse!(ProcessPoll, ProcessHandleArgs),
            PROCESS_WAIT => parse!(ProcessWait, ProcessWaitArgs),
            PROCESS_WRITE => parse!(ProcessWrite, ProcessWriteArgs),
            PROCESS_TERMINATE => parse!(ProcessTerminate, ProcessHandleArgs),
            OUTPUT_READ => parse!(OutputRead, OutputReadArgs),
            _ => unreachable!("registry membership and parser match stay in lockstep"),
        }?;
        parsed.validate()?;
        Ok(parsed)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OutputReadArgs {
    id: String,
    #[serde(default)]
    range: Option<String>,
    #[serde(default)]
    bytes: Option<String>,
}

#[derive(Debug)]
enum ParsedTool {
    ReadFile(ReadFileArgs),
    DirectoryList(DirectoryArgs),
    Glob(GlobArgs),
    Search(SearchArgs),
    WriteFile(WriteFileArgs),
    ApplyPatch(ApplyPatchArgs),
    ProcessStart(ProcessStartArgs),
    ProcessPoll(ProcessHandleArgs),
    ProcessWait(ProcessWaitArgs),
    ProcessWrite(ProcessWriteArgs),
    ProcessTerminate(ProcessHandleArgs),
    OutputRead(OutputReadArgs),
}

impl ParsedTool {
    fn validate(&self) -> Result<(), ToolError> {
        let non_empty_path = |path: &Path, field: &str| {
            if path.as_os_str().is_empty() {
                Err(ToolError::new(
                    ToolErrorCode::InvalidArguments,
                    format!("{field} must not be empty"),
                ))
            } else {
                Ok(())
            }
        };
        match self {
            Self::ReadFile(args) => non_empty_path(&args.path, "path"),
            Self::DirectoryList(args) => {
                non_empty_path(&args.path, "path")?;
                positive(args.max_results, "max_results")
            }
            Self::Glob(args) => {
                non_empty_path(&args.root, "root")?;
                non_empty(&args.pattern, "pattern")?;
                positive(args.max_results, "max_results")
            }
            Self::Search(args) => {
                non_empty_path(&args.root, "root")?;
                non_empty(&args.query, "query")?;
                positive(args.max_results, "max_results")
            }
            Self::WriteFile(args) => {
                non_empty_path(&args.path, "path")?;
                valid_idempotency(&args.idempotency_key)
            }
            Self::ApplyPatch(args) => {
                non_empty_path(&args.path, "path")?;
                non_empty(&args.expected_sha256, "expected_sha256")?;
                if args.operations.is_empty() {
                    return Err(ToolError::new(
                        ToolErrorCode::InvalidArguments,
                        "operations must not be empty",
                    ));
                }
                valid_idempotency(&args.idempotency_key)
            }
            Self::ProcessStart(args) => {
                non_empty(&args.program, "program")?;
                non_empty_path(&args.cwd, "cwd")?;
                if args
                    .shell_script
                    .as_ref()
                    .is_some_and(|script| script.is_empty())
                {
                    return Err(ToolError::new(
                        ToolErrorCode::InvalidArguments,
                        "shell_script must not be empty when supplied",
                    ));
                }
                if args.timeout_ms == Some(0) {
                    return Err(ToolError::new(
                        ToolErrorCode::InvalidArguments,
                        "timeout_ms must be positive",
                    ));
                }
                valid_idempotency(&args.idempotency_key)
            }
            Self::ProcessPoll(args) | Self::ProcessTerminate(args) => {
                non_empty(&args.handle, "handle")
            }
            Self::ProcessWait(args) => {
                non_empty(&args.handle, "handle")?;
                if args.wait_ms > 60_000 {
                    return Err(ToolError::new(
                        ToolErrorCode::InvalidArguments,
                        "wait_ms must not exceed 60000",
                    ));
                }
                Ok(())
            }
            Self::ProcessWrite(args) => non_empty(&args.handle, "handle"),
            Self::OutputRead(args) => non_empty(&args.id, "id"),
        }
    }

    fn action(&self) -> ExecutionAction {
        match self {
            Self::ReadFile(args) => ExecutionAction::ReadFile {
                path: args.path.clone(),
            },
            Self::DirectoryList(args) => ExecutionAction::ReadFile {
                path: args.path.clone(),
            },
            Self::Glob(args) => ExecutionAction::ReadFile {
                path: args.root.clone(),
            },
            Self::Search(args) => ExecutionAction::ReadFile {
                path: args.root.clone(),
            },
            Self::WriteFile(args) => ExecutionAction::WriteFile {
                path: args.path.clone(),
            },
            Self::ApplyPatch(args) => ExecutionAction::WriteFile {
                path: args.path.clone(),
            },
            Self::ProcessStart(args) => {
                let invocation = match &args.shell_script {
                    Some(script) => ProcessInvocation::Shell {
                        program: args.program.clone(),
                        args: args.args.clone(),
                        script: script.clone(),
                        cwd: args.cwd.clone(),
                        environment: args.environment.clone(),
                    },
                    None => ProcessInvocation::Argv {
                        program: args.program.clone(),
                        args: args.args.clone(),
                        cwd: args.cwd.clone(),
                        environment: args.environment.clone(),
                    },
                };
                ExecutionAction::Process {
                    invocation,
                    effects: args.effects(),
                }
            }
            Self::ProcessPoll(args) => process_control(&args.handle, PROCESS_POLL),
            Self::ProcessWait(args) => process_control(&args.handle, PROCESS_WAIT),
            Self::ProcessWrite(args) => process_control(&args.handle, PROCESS_WRITE),
            Self::ProcessTerminate(args) => process_control(&args.handle, PROCESS_TERMINATE),
            Self::OutputRead(args) => ExecutionAction::OutputRead {
                id: args.id.clone(),
            },
        }
    }

    fn retry_policy(&self) -> RetryPolicy {
        match self {
            Self::ReadFile(_)
            | Self::DirectoryList(_)
            | Self::Glob(_)
            | Self::Search(_)
            | Self::ProcessPoll(_)
            | Self::ProcessWait(_)
            | Self::OutputRead(_) => RetryPolicy::Safe,
            Self::WriteFile(_)
            | Self::ApplyPatch(_)
            | Self::ProcessWrite(_)
            | Self::ProcessTerminate(_) => RetryPolicy::Reconcile,
            Self::ProcessStart(args)
                if args.network || args.outside_write || args.git_push_or_destructive =>
            {
                RetryPolicy::NeverAfterStart
            }
            Self::ProcessStart(_) => RetryPolicy::Reconcile,
        }
    }
}

fn non_empty(value: &str, field: &str) -> Result<(), ToolError> {
    if value.is_empty() {
        Err(ToolError::new(
            ToolErrorCode::InvalidArguments,
            format!("{field} must not be empty"),
        ))
    } else {
        Ok(())
    }
}

fn positive(value: usize, field: &str) -> Result<(), ToolError> {
    if value == 0 {
        Err(ToolError::new(
            ToolErrorCode::InvalidArguments,
            format!("{field} must be positive"),
        ))
    } else {
        Ok(())
    }
}

fn valid_idempotency(value: &str) -> Result<(), ToolError> {
    if value.is_empty() || value.len() > 256 || value.contains('\0') {
        Err(ToolError::new(
            ToolErrorCode::InvalidArguments,
            "idempotency_key must contain 1..=256 non-NUL bytes",
        ))
    } else {
        Ok(())
    }
}

fn process_control(handle: &str, operation: &str) -> ExecutionAction {
    ExecutionAction::ProcessControl {
        handle: handle.to_string(),
        operation: operation.to_string(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolReceiptState {
    Completed,
    Failed,
    OutcomeUnknown,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ToolReceipt {
    pub receipt_id: String,
    pub tool: String,
    pub state: ToolReceiptState,
    pub retry: RetryPolicy,
    pub result: Option<Value>,
    pub error: Option<ToolError>,
    pub policy_fingerprint: Option<String>,
    pub approved_by: Option<String>,
    pub started_at_ms: u64,
    pub completed_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolErrorCode {
    UnknownTool,
    InvalidArguments,
    AuthorizationDenied,
    ApprovalRequired,
    IsolationUnavailable,
    PreconditionFailed,
    UnsupportedContent,
    ResourceBusy,
    UnknownProcess,
    ProcessClosed,
    OutputLimit,
    Io,
    Journal,
    Internal,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ToolError {
    pub code: ToolErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval: Option<ApprovalRequest>,
    pub outcome_unknown: bool,
}

impl ToolError {
    pub(super) fn new(code: ToolErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            approval: None,
            outcome_unknown: false,
        }
    }

    pub(super) fn io(error: std::io::Error) -> Self {
        Self::new(ToolErrorCode::Io, error.to_string())
    }

    pub(super) fn external(error: impl Display) -> Self {
        Self::new(ToolErrorCode::Internal, error.to_string())
    }

    fn unknown_outcome(message: impl Into<String>) -> Self {
        Self {
            code: ToolErrorCode::Journal,
            message: message.into(),
            approval: None,
            outcome_unknown: true,
        }
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ToolError {}

impl From<BrokerError> for ToolError {
    fn from(error: BrokerError) -> Self {
        let code = match &error {
            BrokerError::ApprovalRequired(_) | BrokerError::ApprovalUnavailable(_) => {
                ToolErrorCode::ApprovalRequired
            }
            BrokerError::IsolationUnavailable(_) => ToolErrorCode::IsolationUnavailable,
            BrokerError::WriterPermit(_) => ToolErrorCode::ResourceBusy,
            BrokerError::Denied(_)
            | BrokerError::ProtectedPath(_)
            | BrokerError::Scope(_)
            | BrokerError::Identity(_)
            | BrokerError::StaleGeneration { .. }
            | BrokerError::InvalidApproval(_) => ToolErrorCode::AuthorizationDenied,
            BrokerError::InvalidAction(_) => ToolErrorCode::InvalidArguments,
            BrokerError::PolicyUnavailable(_) | BrokerError::Internal(_) => ToolErrorCode::Internal,
        };
        let approval = match &error {
            BrokerError::ApprovalRequired(request)
            | BrokerError::ApprovalUnavailable(request)
            | BrokerError::InvalidApproval(request) => Some((**request).clone()),
            _ => None,
        };
        Self {
            code,
            message: error.to_string(),
            approval,
            outcome_unknown: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ToolLimits {
    pub max_inline_bytes: usize,
    pub max_output_read_bytes: usize,
    pub max_processes: usize,
    process: ProcessLimits,
}

impl ToolLimits {
    pub fn from_config(config: &CtxConfig) -> Self {
        let max_inline_bytes = config.search.max_output_bytes;
        Self {
            max_inline_bytes,
            max_output_read_bytes: config.search.max_output_bytes,
            max_processes: DEFAULT_MAX_PROCESSES,
            process: ProcessLimits {
                max_inline_bytes,
                max_processes: DEFAULT_MAX_PROCESSES,
                max_summary_bytes: config.output.max_summary_bytes,
                max_heavy_operations: config.supervise.max_heavy_operations,
                heavy_patterns: config.supervise.heavy_command_patterns.clone(),
                output_filter: config.output.filter.clone(),
                extra_verbatim: config.output.verbatim.clone(),
                compact_search: config.output.compact_search,
            },
        }
    }

    #[cfg(test)]
    fn testing() -> Self {
        Self {
            max_inline_bytes: 1024,
            max_output_read_bytes: 2048,
            max_processes: 4,
            process: ProcessLimits {
                max_inline_bytes: 1024,
                max_processes: 4,
                max_summary_bytes: 4096,
                max_heavy_operations: 1,
                heavy_patterns: Vec::new(),
                output_filter: Vec::new(),
                extra_verbatim: Vec::new(),
                compact_search: true,
            },
        }
    }
}

pub struct JournalExecution<'a> {
    pub journal: &'a mut Journal,
    pub session: JournalSessionId,
    pub generation: u64,
    pub scope: EventScope,
    pub tool_call: ToolCallId,
    pub execution: ExecutionId,
}

pub struct NativeToolClient {
    registry: ToolRegistry,
    broker: ExecutionBroker,
    state: StateDir,
    repo: PathBuf,
    limits: ToolLimits,
    processes: ProcessManager,
}

impl std::fmt::Debug for NativeToolClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeToolClient")
            .field("registry", &self.registry)
            .field("broker", &self.broker)
            .field("repo", &self.repo)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl NativeToolClient {
    pub fn new(
        broker: ExecutionBroker,
        state: StateDir,
        repo: PathBuf,
        limits: ToolLimits,
    ) -> Self {
        let processes = ProcessManager::new(state.clone(), repo.clone(), limits.process.clone());
        Self {
            registry: ToolRegistry::native(),
            broker,
            state,
            repo,
            limits,
            processes,
        }
    }

    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    pub fn execute(
        &mut self,
        name: &str,
        arguments: Value,
        grant: Option<&ApprovalGrant>,
        mut journal: Option<JournalExecution<'_>>,
    ) -> ToolReceipt {
        let started_at_ms = now_ms();
        let parsed = match self.registry.parse(name, arguments) {
            Ok(parsed) => parsed,
            Err(error) => return failed_receipt(name, RetryPolicy::Safe, error, started_at_ms),
        };
        let retry = parsed.retry_policy();
        let action = parsed.action();
        let authorization = match self.broker.authorize(&action, grant) {
            Ok(authorization) => authorization,
            Err(error) => return failed_receipt(name, retry, error.into(), started_at_ms),
        };

        if let Some(record) = journal.as_mut()
            && let Err(error) = journal_start(record)
        {
            return failed_receipt(
                name,
                retry,
                ToolError::new(ToolErrorCode::Journal, error.to_string()),
                started_at_ms,
            );
        }

        let result = self.dispatch(parsed, &action, &authorization);
        let completed_at_ms = now_ms();
        let mut receipt = match result {
            Ok(result) => ToolReceipt {
                receipt_id: uuid::Uuid::new_v4().simple().to_string(),
                tool: name.to_string(),
                state: ToolReceiptState::Completed,
                retry,
                result: Some(result),
                error: None,
                policy_fingerprint: Some(authorization.policy_fingerprint().to_string()),
                approved_by: authorization.approved_by().map(str::to_string),
                started_at_ms,
                completed_at_ms,
            },
            Err(error) => ToolReceipt {
                receipt_id: uuid::Uuid::new_v4().simple().to_string(),
                tool: name.to_string(),
                state: ToolReceiptState::Failed,
                retry,
                result: None,
                error: Some(error),
                policy_fingerprint: Some(authorization.policy_fingerprint().to_string()),
                approved_by: authorization.approved_by().map(str::to_string),
                started_at_ms,
                completed_at_ms,
            },
        };
        if let Some(record) = journal.as_mut()
            && let Err(error) = journal_finish(record, &receipt)
        {
            receipt.state = ToolReceiptState::OutcomeUnknown;
            receipt.retry = RetryPolicy::NeverAfterStart;
            receipt.result = None;
            receipt.error = Some(ToolError::unknown_outcome(format!(
                "tool effect finished but its durable receipt failed: {error}"
            )));
        }
        receipt
    }

    fn dispatch(
        &mut self,
        parsed: ParsedTool,
        action: &ExecutionAction,
        authorization: &Authorization,
    ) -> Result<Value, ToolError> {
        match parsed {
            ParsedTool::ReadFile(args) => {
                let path = authorized_path(authorization)?;
                self.finish_file(files::read_file(path, &args, self.limits.max_inline_bytes)?)
            }
            ParsedTool::DirectoryList(args) => {
                let path = authorized_path(authorization)?;
                self.finish_file(files::list_directory(path, &args)?)
            }
            ParsedTool::Glob(args) => {
                let path = authorized_path(authorization)?;
                self.finish_file(files::glob(path, &args)?)
            }
            ParsedTool::Search(args) => {
                let path = authorized_path(authorization)?;
                self.finish_file(files::search(path, &args)?)
            }
            ParsedTool::WriteFile(args) => {
                let path = authorized_path(authorization)?;
                self.finish_file(files::write_file(path, &args)?)
            }
            ParsedTool::ApplyPatch(args) => {
                let path = authorized_path(authorization)?;
                self.finish_file(files::apply_patch(path, &args)?)
            }
            ParsedTool::ProcessStart(args) => {
                let launch = self
                    .broker
                    .prepare_process(action, authorization)
                    .map_err(ToolError::from)?;
                let snapshot = self.processes.start(launch, &args)?;
                ProcessManager::output_json(snapshot)
            }
            ParsedTool::ProcessPoll(args) => {
                ProcessManager::output_json(self.processes.poll(&args.handle)?)
            }
            ParsedTool::ProcessWait(args) => {
                ProcessManager::output_json(self.processes.wait(&args.handle, args.wait_ms)?)
            }
            ParsedTool::ProcessWrite(args) => ProcessManager::output_json(
                self.processes
                    .write_input(&args.handle, &args.input, args.close)?,
            ),
            ParsedTool::ProcessTerminate(args) => {
                ProcessManager::output_json(self.processes.terminate(&args.handle)?)
            }
            ParsedTool::OutputRead(args) => {
                let text = output::show_captured(
                    &self.state,
                    &self.repo,
                    args.id,
                    args.range,
                    args.bytes,
                    self.limits.max_output_read_bytes,
                )
                .map_err(ToolError::external)?;
                Ok(json!({ "content": text }))
            }
        }
    }

    fn finish_file(&self, mut outcome: FileOutcome) -> Result<Value, ToolError> {
        if let Some(capture) = outcome.capture.take() {
            let stored = persist_capture(
                &self.state,
                &self.repo,
                capture,
                self.limits.process.max_summary_bytes,
                &self.limits.process.output_filter,
            )?;
            let object = outcome.data.as_object_mut().ok_or_else(|| {
                ToolError::new(ToolErrorCode::Internal, "file result is not an object")
            })?;
            object.insert("output_id".into(), Value::String(stored.id));
            object.insert(
                "summary".into(),
                stored.summary.map(Value::String).unwrap_or(Value::Null),
            );
        }
        Ok(outcome.data)
    }
}

#[derive(Debug)]
pub(super) struct CapturePayload {
    bytes: Vec<u8>,
    command: Vec<String>,
    scope: CompactionScope,
}

impl CapturePayload {
    pub(super) fn new(bytes: Vec<u8>, command: Vec<String>, scope: CompactionScope) -> Self {
        Self {
            bytes,
            command,
            scope,
        }
    }
}

fn persist_capture(
    state: &StateDir,
    repo: &Path,
    capture: CapturePayload,
    max_summary_bytes: usize,
    filter: &[crate::commands::ctx::config::OutputFilterRule],
) -> Result<output::CapturedOutput, ToolError> {
    let mut stored = StreamingCapture::start(state, repo).map_err(ToolError::external)?;
    if let Err(error) = stored.append(&capture.bytes) {
        stored.abort();
        return Err(ToolError::external(error));
    }
    stored
        .finish(
            &capture.command,
            Some(0),
            max_summary_bytes,
            capture.scope,
            filter,
        )
        .map_err(ToolError::external)
}

fn authorized_path(authorization: &Authorization) -> Result<&Path, ToolError> {
    authorization
        .resolved_paths()
        .first()
        .map(PathBuf::as_path)
        .ok_or_else(|| {
            ToolError::new(
                ToolErrorCode::Internal,
                "filesystem authorization did not resolve a target path",
            )
        })
}

fn journal_start(record: &mut JournalExecution<'_>) -> Result<(), super::journal::JournalError> {
    let committed_at = state::now_secs();
    let at_ms = Some(now_ms());
    record.journal.prepare_execution(
        &record.session,
        record.generation,
        &record.scope,
        record.execution.clone(),
        record.tool_call.clone(),
        at_ms,
        committed_at,
    )?;
    record.journal.transition_execution(
        &record.session,
        record.generation,
        &record.scope,
        &record.execution,
        ExecutionState::Started,
        None,
        None,
        at_ms,
        committed_at,
    )?;
    Ok(())
}

fn journal_finish(
    record: &mut JournalExecution<'_>,
    receipt: &ToolReceipt,
) -> Result<(), super::journal::JournalError> {
    let (state, detail) = match receipt.state {
        ToolReceiptState::Completed => (ExecutionState::Completed, None),
        ToolReceiptState::Failed => (
            ExecutionState::Failed,
            receipt.error.as_ref().map(|error| error.message.clone()),
        ),
        ToolReceiptState::OutcomeUnknown => (ExecutionState::OutcomeUnknown, None),
    };
    let text = serde_json::to_string(receipt)?;
    record.journal.transition_execution(
        &record.session,
        record.generation,
        &record.scope,
        &record.execution,
        state,
        Some(ContentRef::Inline { text }),
        detail,
        Some(now_ms()),
        state::now_secs(),
    )?;
    Ok(())
}

fn failed_receipt(
    name: &str,
    retry: RetryPolicy,
    error: ToolError,
    started_at_ms: u64,
) -> ToolReceipt {
    let state = if error.outcome_unknown {
        ToolReceiptState::OutcomeUnknown
    } else {
        ToolReceiptState::Failed
    };
    ToolReceipt {
        receipt_id: uuid::Uuid::new_v4().simple().to_string(),
        tool: name.to_string(),
        state,
        retry,
        result: None,
        error: Some(error),
        policy_fingerprint: None,
        approved_by: None,
        started_at_ms,
        completed_at_ms: now_ms(),
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn definition(
    name: &str,
    description: &str,
    input_schema: Value,
    capabilities: &[&str],
    execution_mode: ToolExecutionMode,
    resource_claims: &[ResourceClaimKind],
    cancellation: CancellationContract,
    retry: RetryPolicy,
) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: description.into(),
        input_schema,
        capabilities: capabilities.iter().map(|value| (*value).into()).collect(),
        execution_mode,
        resource_claims: resource_claims.to_vec(),
        cancellation,
        retry,
        errors: vec![
            ToolErrorCode::InvalidArguments,
            ToolErrorCode::AuthorizationDenied,
            ToolErrorCode::ApprovalRequired,
            ToolErrorCode::PreconditionFailed,
            ToolErrorCode::Io,
            ToolErrorCode::Internal,
        ],
    }
}

fn object_schema(required: &[&str], properties: Value) -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": required,
        "properties": properties,
    })
}

fn native_definitions() -> Vec<ToolDefinition> {
    let read_caps = ["tool_access"];
    let write_caps = ["tool_access", "repo_fs_write"];
    let process_caps = [
        "tool_access",
        "shell_exec",
        "repo_fs_write",
        "outside_repo_fs_write",
        "network",
        "git_push_destructive",
    ];
    vec![
        definition(
            FILE_READ,
            "Read a bounded text range or inspect binary/image metadata.",
            object_schema(
                &["path"],
                json!({
                    "path": {"type":"string"},
                    "start_line": {"type":"integer","minimum":1},
                    "end_line": {"type":"integer","minimum":1}
                }),
            ),
            &read_caps,
            ToolExecutionMode::Immediate,
            &[ResourceClaimKind::ReadRoot, ResourceClaimKind::OutputStore],
            CancellationContract::BeforeEffect,
            RetryPolicy::Safe,
        ),
        definition(
            DIRECTORY_LIST,
            "List a directory without following symlinked directories.",
            object_schema(
                &["path"],
                json!({
                    "path":{"type":"string"},
                    "recursive":{"type":"boolean"},
                    "max_results":{"type":"integer","minimum":1}
                }),
            ),
            &read_caps,
            ToolExecutionMode::Immediate,
            &[ResourceClaimKind::ReadRoot, ResourceClaimKind::OutputStore],
            CancellationContract::BeforeEffect,
            RetryPolicy::Safe,
        ),
        definition(
            GLOB_SEARCH,
            "Find paths with a relative *, ?, or ** glob.",
            object_schema(
                &["root", "pattern"],
                json!({
                    "root":{"type":"string"},
                    "pattern":{"type":"string","minLength":1},
                    "max_results":{"type":"integer","minimum":1}
                }),
            ),
            &read_caps,
            ToolExecutionMode::Immediate,
            &[ResourceClaimKind::ReadRoot, ResourceClaimKind::OutputStore],
            CancellationContract::BeforeEffect,
            RetryPolicy::Safe,
        ),
        definition(
            TEXT_SEARCH,
            "Search text files with fixed text or a regular expression.",
            object_schema(
                &["root", "query"],
                json!({
                    "root":{"type":"string"},
                    "query":{"type":"string","minLength":1},
                    "regex":{"type":"boolean"},
                    "case_sensitive":{"type":"boolean"},
                    "include":{"type":"string"},
                    "max_results":{"type":"integer","minimum":1}
                }),
            ),
            &read_caps,
            ToolExecutionMode::Immediate,
            &[ResourceClaimKind::ReadRoot, ResourceClaimKind::OutputStore],
            CancellationContract::BeforeEffect,
            RetryPolicy::Safe,
        ),
        definition(
            FILE_WRITE,
            "Atomically create or replace a text file with a hash precondition.",
            object_schema(
                &["path", "content", "idempotency_key"],
                json!({
                    "path":{"type":"string"},
                    "content":{"type":"string"},
                    "expected_sha256":{"type":"string"},
                    "create_only":{"type":"boolean"},
                    "idempotency_key":{"type":"string","minLength":1,"maxLength":256}
                }),
            ),
            &write_caps,
            ToolExecutionMode::Immediate,
            &[ResourceClaimKind::WorktreeWrite],
            CancellationContract::AtomicCommit,
            RetryPolicy::Reconcile,
        ),
        definition(
            APPLY_PATCH,
            "Apply exact-content replacements after a full-file hash check.",
            object_schema(
                &["path", "expected_sha256", "operations", "idempotency_key"],
                json!({
                    "path":{"type":"string"},
                    "expected_sha256":{"type":"string","minLength":1},
                    "operations":{"type":"array","minItems":1,"items":{
                        "type":"object","additionalProperties":false,
                        "required":["expected","replacement"],
                        "properties":{
                            "expected":{"type":"string","minLength":1},
                            "replacement":{"type":"string"},
                            "expected_occurrences":{"type":"integer","minimum":1}
                        }
                    }},
                    "idempotency_key":{"type":"string","minLength":1,"maxLength":256}
                }),
            ),
            &write_caps,
            ToolExecutionMode::Immediate,
            &[ResourceClaimKind::WorktreeWrite],
            CancellationContract::AtomicCommit,
            RetryPolicy::Reconcile,
        ),
        definition(
            PROCESS_START,
            "Start an argv or explicitly typed shell process in the platform sandbox.",
            object_schema(
                &["program", "cwd", "idempotency_key"],
                json!({
                    "program":{"type":"string","minLength":1},
                    "args":{"type":"array","items":{"type":"string"}},
                    "shell_script":{"type":"string"},
                    "cwd":{"type":"string"},
                    "environment":{"type":"object","additionalProperties":{"type":"string"}},
                    "read_only":{"type":"boolean"},
                    "network":{"type":"boolean"},
                    "outside_write":{"type":"boolean"},
                    "git_metadata_write":{"type":"boolean"},
                    "git_push_or_destructive":{"type":"boolean"},
                    "interactive":{"type":"boolean"},
                    "timeout_ms":{"type":"integer","minimum":1},
                    "idempotency_key":{"type":"string","minLength":1,"maxLength":256}
                }),
            ),
            &process_caps,
            ToolExecutionMode::BackgroundProcess,
            &[
                ResourceClaimKind::ReadRoot,
                ResourceClaimKind::WorktreeWrite,
                ResourceClaimKind::OutsideWrite,
                ResourceClaimKind::GitMetadata,
                ResourceClaimKind::Network,
                ResourceClaimKind::OutputStore,
            ],
            CancellationContract::ProcessTree,
            RetryPolicy::NeverAfterStart,
        ),
        control_definition(
            PROCESS_POLL,
            "Poll a process and return only new bounded output.",
        ),
        definition(
            PROCESS_WAIT,
            "Wait up to 60 seconds for a process while keeping output responsive.",
            object_schema(
                &["handle"],
                json!({
                    "handle":{"type":"string","minLength":1},
                    "wait_ms":{"type":"integer","minimum":0,"maximum":60000}
                }),
            ),
            &read_caps,
            ToolExecutionMode::ProcessControl,
            &[ResourceClaimKind::OutputStore],
            CancellationContract::ProcessTree,
            RetryPolicy::Safe,
        ),
        definition(
            PROCESS_WRITE,
            "Write input to a running pipe or PTY and optionally close input.",
            object_schema(
                &["handle", "input"],
                json!({
                    "handle":{"type":"string","minLength":1},
                    "input":{"type":"string"},
                    "close":{"type":"boolean"}
                }),
            ),
            &read_caps,
            ToolExecutionMode::ProcessControl,
            &[ResourceClaimKind::OutputStore],
            CancellationContract::ProcessTree,
            RetryPolicy::Reconcile,
        ),
        control_definition(PROCESS_TERMINATE, "Terminate and reap a process tree."),
        definition(
            OUTPUT_READ,
            "Retrieve a bounded line or byte range from a stored full output.",
            object_schema(
                &["id"],
                json!({
                    "id":{"type":"string","minLength":1},
                    "range":{"type":"string"},
                    "bytes":{"type":"string"}
                }),
            ),
            &read_caps,
            ToolExecutionMode::Retrieval,
            &[ResourceClaimKind::OutputStore],
            CancellationContract::NotApplicable,
            RetryPolicy::Safe,
        ),
    ]
}

fn control_definition(name: &str, description: &str) -> ToolDefinition {
    definition(
        name,
        description,
        object_schema(
            &["handle"],
            json!({"handle":{"type":"string","minLength":1}}),
        ),
        &["tool_access"],
        ToolExecutionMode::ProcessControl,
        &[ResourceClaimKind::OutputStore],
        CancellationContract::ProcessTree,
        if name == PROCESS_POLL {
            RetryPolicy::Safe
        } else {
            RetryPolicy::Reconcile
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_names_are_unique_and_schemas_are_closed_objects() {
        let registry = ToolRegistry::native();
        let names: Vec<&str> = registry
            .definitions()
            .map(|definition| definition.name.as_str())
            .collect();
        assert_eq!(names.len(), 12);
        assert_eq!(
            names
                .iter()
                .copied()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            12
        );
        for definition in registry.definitions() {
            assert_eq!(definition.input_schema["type"], "object");
            assert_eq!(definition.input_schema["additionalProperties"], false);
            assert!(!definition.capabilities.is_empty());
        }
    }

    #[test]
    fn malformed_or_incomplete_arguments_never_become_a_typed_tool() {
        let registry = ToolRegistry::native();
        let missing = registry
            .parse(FILE_READ, json!({}))
            .expect_err("path is required");
        assert_eq!(missing.code, ToolErrorCode::InvalidArguments);
        let unknown = registry
            .parse("shell_magic", json!({}))
            .expect_err("closed registry");
        assert_eq!(unknown.code, ToolErrorCode::UnknownTool);
        let extra = registry
            .parse(FILE_READ, json!({"path":"a", "surprise":true}))
            .expect_err("unknown fields are rejected");
        assert_eq!(extra.code, ToolErrorCode::InvalidArguments);
    }

    #[test]
    fn process_environment_and_argv_are_part_of_the_authorized_action() {
        let parsed = ToolRegistry::native()
            .parse(
                PROCESS_START,
                json!({
                    "program":"printf",
                    "args":["%s", "hello world"],
                    "cwd":".",
                    "environment":{"LANG":"C"},
                    "read_only":true,
                    "idempotency_key":"run-1"
                }),
            )
            .expect("parse");
        let ExecutionAction::Process {
            invocation,
            effects,
        } = parsed.action()
        else {
            panic!("process action");
        };
        let ProcessInvocation::Argv {
            program,
            args,
            environment,
            ..
        } = invocation
        else {
            panic!("argv");
        };
        assert_eq!(program, "printf");
        assert_eq!(args, ["%s", "hello world"]);
        assert_eq!(environment.get("LANG").map(String::as_str), Some("C"));
        assert!(!effects.repo_write);
    }

    #[test]
    fn limits_are_derived_from_operator_config() {
        let limits = ToolLimits::testing();
        assert_eq!(limits.max_processes, 4);
        assert_eq!(limits.process.max_inline_bytes, 1024);
    }
}
