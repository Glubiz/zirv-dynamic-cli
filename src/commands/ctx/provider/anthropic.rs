//! Direct Anthropic Messages transport (issue #476, roadmap N07).
//!
//! This is raw HTTPS/SSE, not Claude Code or an agent SDK. Zirv owns request
//! construction, stream accumulation, cancellation, typed failures, and the
//! opaque thinking blocks required for safe continuation.

#![allow(dead_code)] // N09 wires direct providers into the persistent runtime loop.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::adapter::{
    CacheMode, Cancellation, Effort, EventSink, FailureClass, FailureScope, FailureScopeKind,
    FinishReason, ProviderAdapter, ProviderContent, ProviderFailure, ProviderMessageRole,
    ProviderRequest, ProviderResponse, ProviderStreamEvent, ProviderTarget, ProviderUsage,
    RetryHint, ThinkingConfig, ThinkingDisplay, resolve_target,
};
use super::config::NativeConfig;
use super::credential::{Credential, CredentialStore};
use super::probe::is_plaintext_non_loopback;
use super::{OpaqueProviderData, Protocol, RouteId};
use crate::commands::ctx::config::EnvLookup;

const API_VERSION: &str = "2023-06-01";
const MAX_ERROR_BODY_BYTES: u64 = 1024 * 1024;
const MAX_SSE_LINE_BYTES: usize = 1024 * 1024;
const CANCELLATION_POLL: Duration = Duration::from_millis(25);
const STREAM_EVENT_QUEUE: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnthropicTimeouts {
    pub connect: Duration,
    pub first_event: Duration,
    pub idle: Duration,
}

impl Default for AnthropicTimeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(10),
            first_event: Duration::from_secs(60),
            idle: Duration::from_secs(120),
        }
    }
}

#[derive(Clone)]
pub struct AnthropicMessagesAdapter {
    target: ProviderTarget,
    credential: Credential,
    timeouts: AnthropicTimeouts,
}

impl std::fmt::Debug for AnthropicMessagesAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicMessagesAdapter")
            .field("target", &self.target)
            .field("credential", &"[redacted]")
            .field("timeouts", &self.timeouts)
            .finish()
    }
}

impl AnthropicMessagesAdapter {
    pub fn from_config(
        config: &NativeConfig,
        route: &RouteId,
        env: EnvLookup<'_>,
        store: &dyn CredentialStore,
        now: u64,
        timeouts: AnthropicTimeouts,
    ) -> Result<Self, ProviderFailure> {
        let (target, credential) = resolve_target(config, route, env, store, now)?;
        if target.protocol != Protocol::AnthropicMessages {
            return Err(ProviderFailure::new(
                FailureClass::Configuration,
                FailureScope::request(),
                format!(
                    "route `{route}` uses {:?}, not the Anthropic Messages protocol",
                    target.protocol
                ),
            ));
        }
        let credential = credential.ok_or_else(|| {
            ProviderFailure::new(
                FailureClass::Authentication,
                FailureScope {
                    kind: FailureScopeKind::Account,
                    id: Some(target.account.to_string()),
                },
                format!(
                    "Anthropic account `{}` has no API credential",
                    target.account
                ),
            )
        })?;
        Self::new(target, credential, timeouts)
    }

    pub fn new(
        target: ProviderTarget,
        credential: Credential,
        timeouts: AnthropicTimeouts,
    ) -> Result<Self, ProviderFailure> {
        if target.protocol != Protocol::AnthropicMessages {
            return Err(ProviderFailure::new(
                FailureClass::Configuration,
                FailureScope::request(),
                "Anthropic adapter requires an anthropic-messages target",
            ));
        }
        if is_plaintext_non_loopback(&target.base_url) {
            return Err(ProviderFailure::new(
                FailureClass::Configuration,
                FailureScope {
                    kind: FailureScopeKind::Endpoint,
                    id: Some(target.endpoint.to_string()),
                },
                "Anthropic credentials cannot be sent over plaintext HTTP to a non-loopback host",
            ));
        }
        if [timeouts.connect, timeouts.first_event, timeouts.idle].contains(&Duration::ZERO) {
            return Err(ProviderFailure::new(
                FailureClass::Configuration,
                FailureScope::request(),
                "Anthropic timeouts must be greater than zero",
            ));
        }
        Ok(Self {
            target,
            credential,
            timeouts,
        })
    }

    fn messages_url(&self) -> String {
        format!("{}/v1/messages", self.target.base_url.trim_end_matches('/'))
    }

    fn encode_request(&self, request: &ProviderRequest) -> Result<EncodedRequest, ProviderFailure> {
        validate_request(request, &self.target)?;
        let mut body = serde_json::Map::new();
        body.insert("model".into(), Value::String(request.model.clone()));
        body.insert("max_tokens".into(), json!(request.max_output_tokens));
        body.insert("stream".into(), Value::Bool(true));

        if !request.system.is_empty() {
            body.insert(
                "system".into(),
                Value::Array(
                    request
                        .system
                        .iter()
                        .map(|text| json!({"type":"text", "text":text}))
                        .collect(),
                ),
            );
        }
        body.insert(
            "messages".into(),
            Value::Array(
                request
                    .messages
                    .iter()
                    .map(|message| {
                        json!({
                            "role": match message.role {
                                ProviderMessageRole::User => "user",
                                ProviderMessageRole::Assistant => "assistant",
                            },
                            "content": message.content,
                        })
                    })
                    .collect(),
            ),
        );
        if !request.tools.is_empty() {
            body.insert(
                "tools".into(),
                Value::Array(
                    request
                        .tools
                        .iter()
                        .map(|tool| {
                            json!({
                                "name": tool.name,
                                "description": tool.description,
                                "input_schema": tool.input_schema,
                            })
                        })
                        .collect(),
                ),
            );
        }
        if !request.stop_sequences.is_empty() {
            body.insert("stop_sequences".into(), json!(request.stop_sequences));
        }
        match request.cache {
            CacheMode::Disabled => {}
            CacheMode::Ephemeral5m => {
                body.insert("cache_control".into(), json!({"type":"ephemeral"}));
            }
            CacheMode::Ephemeral1h => {
                body.insert(
                    "cache_control".into(),
                    json!({"type":"ephemeral", "ttl":"1h"}),
                );
            }
        }
        match request.thinking {
            ThinkingConfig::Default => {}
            ThinkingConfig::Disabled => {
                body.insert("thinking".into(), json!({"type":"disabled"}));
            }
            ThinkingConfig::Adaptive { display } => {
                let mut value = json!({"type":"adaptive"});
                if let Some(display) = display {
                    value["display"] = json!(display_name(display));
                }
                body.insert("thinking".into(), value);
            }
            ThinkingConfig::Enabled {
                budget_tokens,
                display,
                ..
            } => {
                let mut value = json!({"type":"enabled", "budget_tokens":budget_tokens});
                if let Some(display) = display {
                    value["display"] = json!(display_name(display));
                }
                body.insert("thinking".into(), value);
            }
        }
        if let Some(effort) = request.effort {
            body.insert(
                "output_config".into(),
                json!({"effort":effort_name(effort)}),
            );
        }

        let mut beta_headers = Vec::new();
        if matches!(
            request.thinking,
            ThinkingConfig::Enabled {
                interleaved: true,
                ..
            }
        ) {
            beta_headers.push("interleaved-thinking-2025-05-14");
        }
        if matches!(
            request.thinking,
            ThinkingConfig::Adaptive {
                display: Some(ThinkingDisplay::Updates)
            } | ThinkingConfig::Enabled {
                display: Some(ThinkingDisplay::Updates),
                ..
            }
        ) {
            beta_headers.push("thinking-display-updates-2026-08-18");
        }

        Ok(EncodedRequest {
            body: Value::Object(body),
            beta_headers,
        })
    }

    fn perform_blocking(
        &self,
        encoded: &EncodedRequest,
        cancellation: &dyn Cancellation,
        sink: &mut dyn EventSink,
    ) -> Result<ProviderResponse, ProviderFailure> {
        if cancellation.is_cancelled() {
            return Err(cancelled());
        }
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(self.timeouts.connect))
            .timeout_recv_response(Some(self.timeouts.first_event))
            .timeout_recv_body(Some(self.timeouts.idle))
            .build()
            .into();
        let payload = serde_json::to_string(&encoded.body).map_err(|error| {
            ProviderFailure::new(
                FailureClass::Configuration,
                FailureScope::request(),
                format!("failed to encode Anthropic request: {error}"),
            )
        })?;
        let mut http = agent
            .post(self.messages_url())
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .header("anthropic-version", API_VERSION)
            .header("x-api-key", self.credential.secret.expose())
            .header("user-agent", format!("zirv/{}", env!("CARGO_PKG_VERSION")));
        if !encoded.beta_headers.is_empty() {
            http = http.header("anthropic-beta", encoded.beta_headers.join(","));
        }
        let mut response = http
            .send(payload)
            .map_err(|error| classify_transport_error(error, false, &self.target))?;
        let status = response.status().as_u16();
        let request_id = response
            .headers()
            .get("request-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(parse_retry_after_ms);
        if status >= 400 {
            let body = response
                .body_mut()
                .with_config()
                .limit(MAX_ERROR_BODY_BYTES)
                .read_to_string()
                .unwrap_or_else(|_| "Anthropic returned an unreadable error body".into());
            return Err(classify_http_error(
                status,
                &body,
                request_id,
                retry_after,
                &self.target,
            ));
        }
        let reader = BufReader::new(response.into_body().into_reader());
        parse_sse(reader, request_id, cancellation, sink, &self.target)
    }

    fn perform(
        &self,
        encoded: &EncodedRequest,
        cancellation: &dyn Cancellation,
        sink: &mut dyn EventSink,
    ) -> Result<ProviderResponse, ProviderFailure> {
        if cancellation.is_cancelled() {
            return Err(cancelled());
        }
        let adapter = self.clone();
        let encoded = encoded.clone();
        let worker_cancelled = Arc::new(AtomicBool::new(false));
        let worker_flag = WorkerCancellation(Arc::clone(&worker_cancelled));
        let (sender, receiver) = mpsc::sync_channel(STREAM_EVENT_QUEUE);
        std::thread::Builder::new()
            .name("zirv-anthropic-stream".into())
            .spawn(move || {
                let mut stream_sink = ChannelSink(sender.clone());
                let result = adapter.perform_blocking(&encoded, &worker_flag, &mut stream_sink);
                let _ = sender.send(TransportUpdate::Done(result));
            })
            .map_err(|error| {
                transport_failure(
                    format!("failed to start Anthropic transport worker: {error}"),
                    &self.target,
                )
            })?;

        let mut saw_event = false;
        let mut deadline = Instant::now() + self.timeouts.first_event;
        loop {
            if cancellation.is_cancelled() {
                worker_cancelled.store(true, Ordering::Release);
                return Err(cancelled());
            }
            let now = Instant::now();
            if now >= deadline {
                worker_cancelled.store(true, Ordering::Release);
                return Err(timeout_failure(saw_event, &self.target));
            }
            let wait = deadline
                .saturating_duration_since(now)
                .min(CANCELLATION_POLL);
            match receiver.recv_timeout(wait) {
                Ok(TransportUpdate::Activity) => {
                    saw_event = true;
                    deadline = Instant::now() + self.timeouts.idle;
                }
                Ok(TransportUpdate::Event(event)) => {
                    saw_event = true;
                    deadline = Instant::now() + self.timeouts.idle;
                    sink.push(event);
                }
                Ok(TransportUpdate::Done(result)) => return result,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(transport_failure(
                        "Anthropic transport worker stopped unexpectedly".into(),
                        &self.target,
                    ));
                }
            }
        }
    }
}

impl ProviderAdapter for AnthropicMessagesAdapter {
    fn protocol(&self) -> Protocol {
        Protocol::AnthropicMessages
    }

    fn target(&self) -> &ProviderTarget {
        &self.target
    }

    fn stream(
        &self,
        request: &ProviderRequest,
        cancellation: &dyn Cancellation,
        sink: &mut dyn EventSink,
    ) -> Result<ProviderResponse, ProviderFailure> {
        let encoded = self.encode_request(request)?;
        self.perform(&encoded, cancellation, sink)
    }
}

#[derive(Clone)]
struct EncodedRequest {
    body: Value,
    beta_headers: Vec<&'static str>,
}

enum TransportUpdate {
    Activity,
    Event(ProviderStreamEvent),
    Done(Result<ProviderResponse, ProviderFailure>),
}

struct ChannelSink(SyncSender<TransportUpdate>);

impl EventSink for ChannelSink {
    fn push(&mut self, event: ProviderStreamEvent) {
        let update = if event == ProviderStreamEvent::ProtocolActivity {
            TransportUpdate::Activity
        } else {
            TransportUpdate::Event(event)
        };
        let _ = self.0.send(update);
    }
}

#[derive(Debug)]
struct WorkerCancellation(Arc<AtomicBool>);

impl Cancellation for WorkerCancellation {
    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

fn display_name(display: ThinkingDisplay) -> &'static str {
    match display {
        ThinkingDisplay::Summarized => "summarized",
        ThinkingDisplay::Omitted => "omitted",
        ThinkingDisplay::Updates => "updates",
    }
}

fn effort_name(effort: Effort) -> &'static str {
    match effort {
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
        Effort::Xhigh => "xhigh",
        Effort::Max => "max",
    }
}

fn validate_request(
    request: &ProviderRequest,
    target: &ProviderTarget,
) -> Result<(), ProviderFailure> {
    let configuration = |message: String| {
        ProviderFailure::new(
            FailureClass::Configuration,
            FailureScope::request(),
            message,
        )
    };
    if request.model != target.model.id {
        return Err(configuration(format!(
            "request model `{}` does not exactly match route model `{}`",
            request.model, target.model.id
        )));
    }
    if request.max_output_tokens == 0 {
        return Err(configuration(
            "max_output_tokens must be greater than zero".into(),
        ));
    }
    if request.messages.is_empty() {
        return Err(configuration(
            "Anthropic Messages requests require at least one message".into(),
        ));
    }
    for tool in &request.tools {
        if !tool.input_schema.is_object() {
            return Err(configuration(format!(
                "tool `{}` input_schema must be a JSON object",
                tool.name
            )));
        }
    }
    validate_content_relationships(request)?;
    validate_thinking_controls(request)?;
    validate_effort(request)?;
    Ok(())
}

fn validate_content_relationships(request: &ProviderRequest) -> Result<(), ProviderFailure> {
    let mut pending = BTreeSet::new();
    for message in &request.messages {
        let has_results = message
            .content
            .iter()
            .any(|block| matches!(block, ProviderContent::ToolResult { .. }));
        if !pending.is_empty() && !has_results {
            return Err(ProviderFailure::new(
                FailureClass::Configuration,
                FailureScope::request(),
                "assistant tool_use blocks must be followed immediately by their tool_result blocks",
            ));
        }
        if has_results
            && (message.role != ProviderMessageRole::User
                || message
                    .content
                    .iter()
                    .any(|block| !matches!(block, ProviderContent::ToolResult { .. })))
        {
            return Err(ProviderFailure::new(
                FailureClass::Configuration,
                FailureScope::request(),
                "tool-result continuation messages must be user messages containing only tool_result blocks",
            ));
        }
        for block in &message.content {
            match block {
                ProviderContent::ToolUse { id, input, .. } => {
                    if message.role != ProviderMessageRole::Assistant
                        || id.is_empty()
                        || !input.is_object()
                    {
                        return Err(ProviderFailure::new(
                            FailureClass::InvalidToolArguments,
                            FailureScope::request(),
                            "assistant tool_use input must be one complete JSON object",
                        ));
                    }
                    if !pending.insert(id.clone()) {
                        return Err(ProviderFailure::new(
                            FailureClass::Configuration,
                            FailureScope::request(),
                            format!("duplicate unresolved tool_use id `{id}`"),
                        ));
                    }
                }
                ProviderContent::ToolResult { tool_use_id, .. } => {
                    if !pending.remove(tool_use_id) {
                        return Err(ProviderFailure::new(
                            FailureClass::Configuration,
                            FailureScope::request(),
                            format!("tool_result references unknown tool_use id `{tool_use_id}`"),
                        ));
                    }
                }
                ProviderContent::Thinking { signature, .. } => {
                    if message.role != ProviderMessageRole::Assistant
                        || signature.expose().as_str().is_none_or(str::is_empty)
                    {
                        return Err(ProviderFailure::new(
                            FailureClass::Configuration,
                            FailureScope::request(),
                            "thinking blocks must retain their non-empty provider signature",
                        ));
                    }
                }
                ProviderContent::RedactedThinking { data }
                    if message.role != ProviderMessageRole::Assistant
                        || data.expose().is_null() =>
                {
                    return Err(ProviderFailure::new(
                        FailureClass::Configuration,
                        FailureScope::request(),
                        "redacted thinking blocks must retain opaque provider data on an assistant message",
                    ));
                }
                ProviderContent::Text { .. } | ProviderContent::RedactedThinking { .. } => {}
            }
        }
        if has_results && !pending.is_empty() {
            return Err(ProviderFailure::new(
                FailureClass::Configuration,
                FailureScope::request(),
                "tool_result blocks must resolve every tool_use from the preceding assistant message",
            ));
        }
    }
    if !pending.is_empty() {
        return Err(ProviderFailure::new(
            FailureClass::Configuration,
            FailureScope::request(),
            "assistant tool_use blocks must be followed by matching tool_result blocks",
        ));
    }
    Ok(())
}

fn validate_thinking_controls(request: &ProviderRequest) -> Result<(), ProviderFailure> {
    let model = request.model.to_ascii_lowercase();
    let is_v5 = model_family(&model, "5");
    let is_47_or_48 = model_family(&model, "4-7") || model_family(&model, "4-8");
    let is_45_or_older =
        model_family(&model, "4-5") || model_family(&model, "4-1") || model.ends_with("-4");
    let always_on = is_v5 && (model.contains("fable") || model.contains("mythos"));
    match request.thinking {
        ThinkingConfig::Default => {}
        ThinkingConfig::Disabled if always_on => {
            return Err(config_error(format!(
                "model `{}` does not support disabling thinking",
                request.model
            )));
        }
        ThinkingConfig::Disabled
            if is_v5
                && model.contains("opus")
                && matches!(request.effort, Some(Effort::Xhigh | Effort::Max)) =>
        {
            return Err(config_error(
                "Claude Opus 5 cannot disable thinking at xhigh or max effort".into(),
            ));
        }
        ThinkingConfig::Adaptive { .. } if is_45_or_older => {
            return Err(config_error(format!(
                "model `{}` supports manual extended thinking, not adaptive thinking",
                request.model
            )));
        }
        ThinkingConfig::Enabled {
            budget_tokens,
            interleaved,
            ..
        } => {
            if is_v5 || is_47_or_48 {
                return Err(config_error(format!(
                    "model `{}` supports adaptive thinking and rejects manual budget_tokens",
                    request.model
                )));
            }
            if budget_tokens < 1024 {
                return Err(config_error(
                    "manual thinking budget_tokens must be at least 1024".into(),
                ));
            }
            if !interleaved && budget_tokens >= request.max_output_tokens {
                return Err(config_error(
                    "manual thinking budget_tokens must be lower than max_output_tokens unless interleaved thinking is enabled".into(),
                ));
            }
            if interleaved && model.contains("haiku-4-5") {
                return Err(config_error(
                    "Claude Haiku 4.5 does not support interleaved thinking".into(),
                ));
            }
            if interleaved && model.contains("opus-4-6") {
                return Err(config_error(
                    "Claude Opus 4.6 supports interleaving only in adaptive mode".into(),
                ));
            }
        }
        ThinkingConfig::Disabled | ThinkingConfig::Adaptive { .. } => {}
    }
    Ok(())
}

fn validate_effort(request: &ProviderRequest) -> Result<(), ProviderFailure> {
    let Some(effort) = request.effort else {
        return Ok(());
    };
    let model = request.model.to_ascii_lowercase();
    if effort == Effort::Xhigh
        && !((model_family(&model, "5") && !model.contains("haiku"))
            || model.contains("opus-4-7")
            || model.contains("opus-4-8"))
    {
        return Err(config_error(format!(
            "model `{}` does not declare xhigh effort support",
            request.model
        )));
    }
    if effort == Effort::Max
        && !((model_family(&model, "5") && !model.contains("haiku"))
            || model.contains("opus-4-6")
            || model.contains("opus-4-7")
            || model.contains("opus-4-8")
            || model.contains("sonnet-4-6"))
    {
        return Err(config_error(format!(
            "model `{}` does not declare max effort support",
            request.model
        )));
    }
    Ok(())
}

fn model_family(model: &str, version: &str) -> bool {
    ["fable", "mythos", "opus", "sonnet", "haiku"]
        .iter()
        .any(|family| model.contains(&format!("{family}-{version}")))
}

fn config_error(message: String) -> ProviderFailure {
    ProviderFailure::new(
        FailureClass::Configuration,
        FailureScope::request(),
        message,
    )
}

#[derive(Debug)]
enum BlockState {
    Text(String),
    Thinking {
        text: String,
        signature: Option<OpaqueProviderData>,
    },
    RedactedThinking(OpaqueProviderData),
    ToolUse {
        id: String,
        name: String,
        initial_input: Value,
        partial_json: String,
    },
    Ignored,
}

#[derive(Default)]
struct Accumulator {
    message_id: Option<String>,
    model: Option<String>,
    blocks: BTreeMap<usize, BlockState>,
    completed: BTreeMap<usize, ProviderContent>,
    finish_reason: Option<FinishReason>,
    stop_sequence: Option<String>,
    stop_details: Option<OpaqueProviderData>,
    usage: ProviderUsage,
    saw_stop: bool,
    saw_event: bool,
}

fn parse_sse<R: BufRead>(
    mut reader: R,
    request_id: Option<String>,
    cancellation: &dyn Cancellation,
    sink: &mut dyn EventSink,
    target: &ProviderTarget,
) -> Result<ProviderResponse, ProviderFailure> {
    let mut accumulator = Accumulator::default();
    let mut event_name = String::new();
    let mut data = String::new();
    loop {
        if cancellation.is_cancelled() {
            return Err(cancelled());
        }
        let mut line = String::new();
        let read = (&mut reader)
            .take((MAX_SSE_LINE_BYTES + 1) as u64)
            .read_line(&mut line)
            .map_err(|error| {
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) {
                    timeout_failure(accumulator.saw_event, target)
                } else if error.kind() == std::io::ErrorKind::InvalidData {
                    invalid_stream("Anthropic SSE contains invalid UTF-8".into())
                } else {
                    transport_failure(format!("Anthropic stream read failed: {error}"), target)
                }
            })?;
        if read == 0 {
            if !event_name.is_empty() || !data.is_empty() {
                process_sse_event(&event_name, &data, &mut accumulator, sink, target)?;
            }
            break;
        }
        if line.len() > MAX_SSE_LINE_BYTES {
            return Err(ProviderFailure::new(
                FailureClass::InvalidStream,
                FailureScope::request(),
                format!("Anthropic SSE line exceeds {MAX_SSE_LINE_BYTES} bytes"),
            ));
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            if !event_name.is_empty() || !data.is_empty() {
                process_sse_event(&event_name, &data, &mut accumulator, sink, target)?;
                event_name.clear();
                data.clear();
            }
            continue;
        }
        if let Some(value) = trimmed.strip_prefix("event:") {
            event_name = value.trim_start().to_string();
        } else if let Some(value) = trimmed.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value.trim_start());
        }
    }

    if !accumulator.saw_stop {
        return Err(ProviderFailure::new(
            FailureClass::InvalidStream,
            FailureScope::request(),
            "Anthropic stream ended before message_stop",
        ));
    }
    if !accumulator.blocks.is_empty() {
        return Err(ProviderFailure::new(
            FailureClass::InvalidStream,
            FailureScope::request(),
            "Anthropic stream ended with an incomplete content block",
        ));
    }
    let message_id = accumulator.message_id.ok_or_else(|| {
        ProviderFailure::new(
            FailureClass::InvalidStream,
            FailureScope::request(),
            "Anthropic stream had no message_start id",
        )
    })?;
    let model = accumulator.model.ok_or_else(|| {
        ProviderFailure::new(
            FailureClass::InvalidStream,
            FailureScope::request(),
            "Anthropic stream had no serving model",
        )
    })?;
    let finish_reason = accumulator.finish_reason.ok_or_else(|| {
        ProviderFailure::new(
            FailureClass::InvalidStream,
            FailureScope::request(),
            "Anthropic stream had no stop_reason",
        )
    })?;
    Ok(ProviderResponse {
        message_id,
        model,
        content: accumulator.completed.into_values().collect(),
        finish_reason,
        stop_sequence: accumulator.stop_sequence,
        stop_details: accumulator.stop_details,
        usage: accumulator.usage,
        request_id,
    })
}

fn process_sse_event(
    event_name: &str,
    data: &str,
    accumulator: &mut Accumulator,
    sink: &mut dyn EventSink,
    target: &ProviderTarget,
) -> Result<(), ProviderFailure> {
    let value: Value = serde_json::from_str(data).map_err(|error| {
        ProviderFailure::new(
            FailureClass::InvalidStream,
            FailureScope::request(),
            format!("invalid Anthropic SSE JSON for `{event_name}`: {error}"),
        )
    })?;
    accumulator.saw_event = true;
    sink.push(ProviderStreamEvent::ProtocolActivity);
    match event_name {
        "message_start" => {
            let message = value.get("message").unwrap_or(&Value::Null);
            let id = required_string(message, "id", "message_start")?;
            let model = required_string(message, "model", "message_start")?;
            accumulator.message_id = Some(id.clone());
            accumulator.model = Some(model.clone());
            merge_usage(&mut accumulator.usage, message.get("usage"));
            sink.push(ProviderStreamEvent::MessageStarted { id, model });
        }
        "content_block_start" => {
            let index = required_index(&value)?;
            if accumulator.blocks.contains_key(&index) || accumulator.completed.contains_key(&index)
            {
                return Err(invalid_stream(format!(
                    "duplicate Anthropic content block index {index}"
                )));
            }
            let block = value.get("content_block").unwrap_or(&Value::Null);
            let kind = required_string(block, "type", "content_block_start")?;
            let state = match kind.as_str() {
                "text" => BlockState::Text(
                    block
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                ),
                "thinking" => BlockState::Thinking {
                    text: block
                        .get("thinking")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    signature: block.get("signature").and_then(|signature| {
                        if signature.as_str().is_some_and(str::is_empty) {
                            None
                        } else {
                            Some(OpaqueProviderData::new(signature.clone()))
                        }
                    }),
                },
                "redacted_thinking" => BlockState::RedactedThinking(OpaqueProviderData::new(
                    block.get("data").cloned().unwrap_or(Value::Null),
                )),
                "tool_use" => BlockState::ToolUse {
                    id: required_string(block, "id", "tool_use")?,
                    name: required_string(block, "name", "tool_use")?,
                    initial_input: block.get("input").cloned().unwrap_or_else(|| json!({})),
                    partial_json: String::new(),
                },
                _ => BlockState::Ignored,
            };
            accumulator.blocks.insert(index, state);
        }
        "content_block_delta" => {
            let index = required_index(&value)?;
            let state = accumulator.blocks.get_mut(&index).ok_or_else(|| {
                invalid_stream(format!("delta for unopened Anthropic block {index}"))
            })?;
            let delta = value.get("delta").unwrap_or(&Value::Null);
            match delta.get("type").and_then(Value::as_str).unwrap_or("") {
                "text_delta" => {
                    let text = required_string(delta, "text", "text_delta")?;
                    let BlockState::Text(buffer) = state else {
                        return Err(invalid_stream("text delta on non-text block".into()));
                    };
                    buffer.push_str(&text);
                    sink.push(ProviderStreamEvent::TextDelta { index, text });
                }
                "thinking_delta" => {
                    let text = required_string(delta, "thinking", "thinking_delta")?;
                    let BlockState::Thinking { text: buffer, .. } = state else {
                        return Err(invalid_stream(
                            "thinking delta on non-thinking block".into(),
                        ));
                    };
                    buffer.push_str(&text);
                    sink.push(ProviderStreamEvent::ThinkingDelta { index, text });
                }
                "signature_delta" => {
                    let signature = delta.get("signature").cloned().ok_or_else(|| {
                        invalid_stream("signature_delta missing signature".into())
                    })?;
                    let BlockState::Thinking {
                        signature: stored, ..
                    } = state
                    else {
                        return Err(invalid_stream(
                            "signature delta on non-thinking block".into(),
                        ));
                    };
                    *stored = Some(OpaqueProviderData::new(signature));
                }
                "input_json_delta" => {
                    let partial_json = required_string(delta, "partial_json", "input_json_delta")?;
                    let BlockState::ToolUse {
                        partial_json: buffer,
                        ..
                    } = state
                    else {
                        return Err(invalid_stream("tool input delta on non-tool block".into()));
                    };
                    buffer.push_str(&partial_json);
                    sink.push(ProviderStreamEvent::ToolInputDelta {
                        index,
                        partial_json,
                    });
                }
                _ => {}
            }
        }
        "content_block_stop" => {
            let index = required_index(&value)?;
            let state = accumulator.blocks.remove(&index).ok_or_else(|| {
                invalid_stream(format!("stop for unopened Anthropic block {index}"))
            })?;
            let completed = finish_block(state)?;
            if let Some(block) = completed {
                accumulator.completed.insert(index, block);
            }
            sink.push(ProviderStreamEvent::BlockCompleted { index });
        }
        "message_delta" => {
            let delta = value.get("delta").unwrap_or(&Value::Null);
            if let Some(reason) = delta.get("stop_reason").and_then(Value::as_str) {
                accumulator.finish_reason = Some(parse_finish_reason(reason));
            }
            accumulator.stop_sequence = delta
                .get("stop_sequence")
                .and_then(Value::as_str)
                .map(str::to_string);
            accumulator.stop_details = delta
                .get("stop_details")
                .filter(|details| !details.is_null())
                .cloned()
                .map(OpaqueProviderData::new);
            merge_usage(&mut accumulator.usage, value.get("usage"));
        }
        "message_stop" => accumulator.saw_stop = true,
        "ping" => sink.push(ProviderStreamEvent::Ping),
        "error" => return Err(classify_stream_error(&value, target)),
        _ => {}
    }
    Ok(())
}

fn finish_block(state: BlockState) -> Result<Option<ProviderContent>, ProviderFailure> {
    match state {
        BlockState::Text(text) => Ok(Some(ProviderContent::Text { text })),
        BlockState::Thinking { text, signature } => {
            let signature = signature.ok_or_else(|| {
                invalid_stream("Anthropic thinking block ended without a signature".into())
            })?;
            Ok(Some(ProviderContent::Thinking {
                thinking: text,
                signature,
            }))
        }
        BlockState::RedactedThinking(data) => Ok(Some(ProviderContent::RedactedThinking { data })),
        BlockState::ToolUse {
            id,
            name,
            initial_input,
            partial_json,
        } => {
            let input = if partial_json.is_empty() {
                initial_input
            } else {
                serde_json::from_str(&partial_json).map_err(|error| {
                    ProviderFailure::new(
                        FailureClass::InvalidToolArguments,
                        FailureScope::request(),
                        format!("Anthropic tool `{name}` returned incomplete JSON: {error}"),
                    )
                })?
            };
            if !input.is_object() {
                return Err(ProviderFailure::new(
                    FailureClass::InvalidToolArguments,
                    FailureScope::request(),
                    format!("Anthropic tool `{name}` input is not a JSON object"),
                ));
            }
            Ok(Some(ProviderContent::ToolUse { id, name, input }))
        }
        BlockState::Ignored => Ok(None),
    }
}

fn merge_usage(usage: &mut ProviderUsage, value: Option<&Value>) {
    let Some(value) = value else {
        return;
    };
    if let Some(tokens) = value.get("input_tokens").and_then(Value::as_u64) {
        usage.input_tokens = tokens;
    }
    if let Some(tokens) = value
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64)
    {
        usage.cache_creation_input_tokens = tokens;
    }
    if let Some(tokens) = value.get("cache_read_input_tokens").and_then(Value::as_u64) {
        usage.cache_read_input_tokens = tokens;
    }
    if let Some(tokens) = value.get("output_tokens").and_then(Value::as_u64) {
        usage.output_tokens = tokens;
    }
    if let Some(tokens) = value
        .get("output_tokens_details")
        .and_then(|details| details.get("thinking_tokens"))
        .and_then(Value::as_u64)
    {
        usage.reasoning_tokens = Some(tokens);
    }
}

fn parse_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "end_turn" => FinishReason::EndTurn,
        "max_tokens" => FinishReason::MaxTokens,
        "stop_sequence" => FinishReason::StopSequence,
        "tool_use" => FinishReason::ToolUse,
        "pause_turn" => FinishReason::PauseTurn,
        "refusal" => FinishReason::Refusal,
        "model_context_window_exceeded" => FinishReason::ContextWindowExceeded,
        other => FinishReason::Unknown(other.to_string()),
    }
}

fn required_index(value: &Value) -> Result<usize, ProviderFailure> {
    value
        .get("index")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| invalid_stream("Anthropic content event has no valid index".into()))
}

fn required_string(value: &Value, field: &str, context: &str) -> Result<String, ProviderFailure> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| invalid_stream(format!("Anthropic {context} has no string `{field}`")))
}

fn invalid_stream(message: String) -> ProviderFailure {
    ProviderFailure::new(
        FailureClass::InvalidStream,
        FailureScope::request(),
        message,
    )
}

fn cancelled() -> ProviderFailure {
    ProviderFailure::new(
        FailureClass::Cancelled,
        FailureScope::request(),
        "Anthropic request cancelled",
    )
}

fn timeout_failure(saw_event: bool, target: &ProviderTarget) -> ProviderFailure {
    let (class, message) = if saw_event {
        (FailureClass::IdleTimeout, "Anthropic stream became idle")
    } else {
        (
            FailureClass::FirstEventTimeout,
            "Anthropic did not produce a first event before the timeout",
        )
    };
    let mut failure = ProviderFailure::new(
        class,
        target_scope(target, FailureScopeKind::Endpoint),
        message,
    );
    failure.retry.retryable = true;
    failure
}

fn classify_transport_error(
    error: ureq::Error,
    saw_event: bool,
    target: &ProviderTarget,
) -> ProviderFailure {
    if matches!(error, ureq::Error::Timeout(_)) {
        return timeout_failure(saw_event, target);
    }
    transport_failure(format!("Anthropic transport failed: {error}"), target)
}

fn transport_failure(message: String, target: &ProviderTarget) -> ProviderFailure {
    let mut failure = ProviderFailure::new(
        FailureClass::Transport,
        target_scope(target, FailureScopeKind::Endpoint),
        message,
    );
    failure.retry.retryable = true;
    failure
}

fn classify_http_error(
    status: u16,
    body: &str,
    header_request_id: Option<String>,
    retry_after_ms: Option<u64>,
    target: &ProviderTarget,
) -> ProviderFailure {
    let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let error_type = parsed
        .pointer("/error/type")
        .and_then(Value::as_str)
        .unwrap_or("");
    let message = parsed
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("Anthropic request failed")
        .to_string();
    let request_id = parsed
        .get("request_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or(header_request_id);
    let lower = message.to_ascii_lowercase();
    let (class, scope_kind, retryable) = match status {
        401 => (
            FailureClass::Authentication,
            FailureScopeKind::Account,
            false,
        ),
        402 => (FailureClass::Entitlement, FailureScopeKind::Account, false),
        403 => (FailureClass::Permission, FailureScopeKind::Account, false),
        404 => (FailureClass::ModelAccess, FailureScopeKind::Model, false),
        413 => (
            FailureClass::ContextOverflow,
            FailureScopeKind::Request,
            false,
        ),
        429 => (
            FailureClass::RateLimited,
            FailureScopeKind::BillingPool,
            true,
        ),
        408 => (FailureClass::Transport, FailureScopeKind::Endpoint, true),
        529 => (FailureClass::Overloaded, FailureScopeKind::Provider, true),
        500..=599 => (FailureClass::Provider, FailureScopeKind::Endpoint, true),
        _ if error_type == "authentication_error" => (
            FailureClass::Authentication,
            FailureScopeKind::Account,
            false,
        ),
        _ if error_type == "permission_error" => {
            (FailureClass::Permission, FailureScopeKind::Account, false)
        }
        _ if error_type == "not_found_error" => {
            (FailureClass::ModelAccess, FailureScopeKind::Model, false)
        }
        _ if lower.contains("context") && (lower.contains("window") || lower.contains("token")) => {
            (
                FailureClass::ContextOverflow,
                FailureScopeKind::Request,
                false,
            )
        }
        _ => (
            FailureClass::Configuration,
            FailureScopeKind::Request,
            false,
        ),
    };
    let mut failure = ProviderFailure::new(class, target_scope(target, scope_kind), message);
    failure.http_status = Some(status);
    failure.provider_request_id = request_id;
    failure.retry = RetryHint {
        retryable,
        after_ms: retry_after_ms,
    };
    failure
}

fn classify_stream_error(value: &Value, target: &ProviderTarget) -> ProviderFailure {
    let error_type = value
        .pointer("/error/type")
        .and_then(Value::as_str)
        .unwrap_or("");
    let message = value
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("Anthropic stream failed")
        .to_string();
    let (class, scope_kind, retryable) = match error_type {
        "overloaded_error" => (FailureClass::Overloaded, FailureScopeKind::Provider, true),
        "rate_limit_error" => (
            FailureClass::RateLimited,
            FailureScopeKind::BillingPool,
            true,
        ),
        "authentication_error" => (
            FailureClass::Authentication,
            FailureScopeKind::Account,
            false,
        ),
        "permission_error" => (FailureClass::Permission, FailureScopeKind::Account, false),
        "not_found_error" => (FailureClass::ModelAccess, FailureScopeKind::Model, false),
        _ => (FailureClass::Provider, FailureScopeKind::Request, false),
    };
    let mut failure = ProviderFailure::new(class, target_scope(target, scope_kind), message);
    failure.provider_request_id = value
        .get("request_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    failure.retry.retryable = retryable;
    failure
}

fn target_scope(target: &ProviderTarget, kind: FailureScopeKind) -> FailureScope {
    let id = match kind {
        FailureScopeKind::Request => None,
        FailureScopeKind::Model => Some(target.model.id.clone()),
        FailureScopeKind::Account => Some(target.account.to_string()),
        FailureScopeKind::BillingPool => Some(target.billing_pool.to_string()),
        FailureScopeKind::Endpoint => Some(target.endpoint.to_string()),
        FailureScopeKind::Provider => Some(target.provider.to_string()),
    };
    FailureScope { kind, id }
}

fn parse_retry_after_ms(value: &str) -> Option<u64> {
    value
        .trim()
        .parse::<u64>()
        .ok()
        .and_then(|seconds| seconds.checked_mul(1000))
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;

    use super::*;
    use crate::commands::ctx::provider::credential::Secret;
    use crate::commands::ctx::provider::{
        AccountId, BillingPoolId, EndpointId, ModelId, ProviderId,
    };
    use crate::commands::ctx::runtime::tools::ToolRegistry;

    const STREAM: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/provider/anthropic/v1/stream-multi-tool-thinking.sse"
    ));
    const MALFORMED_TOOL: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/provider/anthropic/v1/stream-malformed-tool.sse"
    ));

    fn target(base_url: String) -> ProviderTarget {
        ProviderTarget {
            route: RouteId::new("work-sonnet").unwrap(),
            provider: ProviderId::new("anthropic").unwrap(),
            endpoint: EndpointId::new("anthropic").unwrap(),
            account: AccountId::new("work").unwrap(),
            billing_pool: BillingPoolId::new("work").unwrap(),
            protocol: Protocol::AnthropicMessages,
            base_url,
            model: ModelId {
                vendor: "anthropic".into(),
                id: "claude-sonnet-5".into(),
            },
        }
    }

    fn credential() -> Credential {
        Credential {
            secret: Secret::new("test-secret-never-log".into()),
            expires_at: None,
        }
    }

    fn request() -> ProviderRequest {
        ProviderRequest {
            model: "claude-sonnet-5".into(),
            system: vec!["system method".into()],
            messages: vec![super::super::adapter::ProviderMessage {
                role: ProviderMessageRole::User,
                content: vec![ProviderContent::Text {
                    text: "do work".into(),
                }],
            }],
            tools: ToolRegistry::native()
                .definitions()
                .take(2)
                .cloned()
                .collect(),
            max_output_tokens: 4096,
            stop_sequences: Vec::new(),
            thinking: ThinkingConfig::Adaptive {
                display: Some(ThinkingDisplay::Omitted),
            },
            effort: Some(Effort::Medium),
            cache: CacheMode::Ephemeral5m,
        }
    }

    #[test]
    fn stream_reassembles_thinking_signatures_usage_and_multiple_tools() {
        let target = target("https://api.anthropic.com".into());
        let mut events = Vec::new();
        let response = parse_sse(
            BufReader::new(STREAM.as_bytes()),
            Some("req_header".into()),
            &super::super::adapter::NeverCancelled,
            &mut events,
            &target,
        )
        .unwrap();
        assert_eq!(response.message_id, "msg_fixture");
        assert_eq!(response.model, "claude-sonnet-5");
        assert_eq!(response.finish_reason, FinishReason::ToolUse);
        assert_eq!(response.request_id.as_deref(), Some("req_header"));
        assert_eq!(response.usage.input_tokens, 100);
        assert_eq!(response.usage.cache_creation_input_tokens, 20);
        assert_eq!(response.usage.cache_read_input_tokens, 80);
        assert_eq!(response.usage.output_tokens, 31);
        assert_eq!(response.usage.reasoning_tokens, Some(11));
        assert_eq!(response.content.len(), 4);
        assert!(matches!(
            &response.content[0],
            ProviderContent::Thinking { thinking, signature }
                if thinking.is_empty() && signature.expose() == "sig-opaque"
        ));
        assert!(matches!(
            &response.content[1],
            ProviderContent::RedactedThinking { data }
                if data.expose() == "redacted-opaque"
        ));
        assert!(matches!(
            &response.content[2],
            ProviderContent::ToolUse { id, input, .. }
                if id == "toolu_1" && input["path"] == "src/lib.rs"
        ));
        assert!(matches!(
            &response.content[3],
            ProviderContent::ToolUse { id, input, .. }
                if id == "toolu_2" && input["path"] == "Cargo.toml"
        ));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ProviderStreamEvent::ToolInputDelta { index: 2, .. }))
        );
        assert!(!format!("{response:?}").contains("sig-opaque"));
        assert!(!format!("{response:?}").contains("redacted-opaque"));
    }

    #[test]
    fn malformed_streamed_tool_json_never_becomes_a_tool_call() {
        let target = target("https://api.anthropic.com".into());
        let mut events = Vec::new();
        let error = parse_sse(
            BufReader::new(MALFORMED_TOOL.as_bytes()),
            None,
            &super::super::adapter::NeverCancelled,
            &mut events,
            &target,
        )
        .unwrap_err();
        assert_eq!(error.class, FailureClass::InvalidToolArguments);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, ProviderStreamEvent::BlockCompleted { .. }))
        );
    }

    #[test]
    fn cancelled_before_transport_is_typed() {
        let flag = super::super::adapter::CancellationFlag::default();
        flag.cancel();
        let adapter = AnthropicMessagesAdapter::new(
            target("http://127.0.0.1:9".into()),
            credential(),
            AnthropicTimeouts::default(),
        )
        .unwrap();
        let error = adapter
            .stream(&request(), &flag, &mut Vec::new())
            .unwrap_err();
        assert_eq!(error.class, FailureClass::Cancelled);
    }

    #[test]
    fn first_event_idle_and_in_flight_cancellation_are_enforced() {
        let (url, _) = delayed_stream_server("", STREAM, Duration::from_millis(100));
        let adapter = AnthropicMessagesAdapter::new(
            target(url),
            credential(),
            AnthropicTimeouts {
                connect: Duration::from_secs(1),
                first_event: Duration::from_millis(20),
                idle: Duration::from_secs(1),
            },
        )
        .unwrap();
        let error = adapter
            .stream(
                &request(),
                &super::super::adapter::NeverCancelled,
                &mut Vec::new(),
            )
            .unwrap_err();
        assert_eq!(error.class, FailureClass::FirstEventTimeout);

        let split = STREAM.find("event: content_block_start").unwrap();
        let (url, _) = delayed_stream_server(
            &STREAM[..split],
            &STREAM[split..],
            Duration::from_millis(100),
        );
        let adapter = AnthropicMessagesAdapter::new(
            target(url),
            credential(),
            AnthropicTimeouts {
                connect: Duration::from_secs(1),
                first_event: Duration::from_secs(1),
                idle: Duration::from_millis(20),
            },
        )
        .unwrap();
        let error = adapter
            .stream(
                &request(),
                &super::super::adapter::NeverCancelled,
                &mut Vec::new(),
            )
            .unwrap_err();
        assert_eq!(error.class, FailureClass::IdleTimeout);

        let (url, _) = delayed_stream_server("", STREAM, Duration::from_millis(100));
        let adapter = AnthropicMessagesAdapter::new(
            target(url),
            credential(),
            AnthropicTimeouts {
                connect: Duration::from_secs(1),
                first_event: Duration::from_secs(1),
                idle: Duration::from_secs(1),
            },
        )
        .unwrap();
        let flag = Arc::new(super::super::adapter::CancellationFlag::default());
        let canceller = Arc::clone(&flag);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            canceller.cancel();
        });
        let error = adapter
            .stream(&request(), flag.as_ref(), &mut Vec::new())
            .unwrap_err();
        assert_eq!(error.class, FailureClass::Cancelled);
    }

    #[test]
    fn status_failures_have_stable_class_scope_request_id_and_retry_hint() {
        let target = target("https://api.anthropic.com".into());
        for (status, body, class, scope, retryable) in [
            (
                401,
                r#"{"type":"error","error":{"type":"authentication_error","message":"bad key"},"request_id":"req_auth"}"#,
                FailureClass::Authentication,
                FailureScopeKind::Account,
                false,
            ),
            (
                403,
                r#"{"type":"error","error":{"type":"permission_error","message":"access denied"},"request_id":"req_access"}"#,
                FailureClass::Permission,
                FailureScopeKind::Account,
                false,
            ),
            (
                429,
                r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"},"request_id":"req_rate"}"#,
                FailureClass::RateLimited,
                FailureScopeKind::BillingPool,
                true,
            ),
            (
                529,
                r#"{"type":"error","error":{"type":"overloaded_error","message":"overloaded"},"request_id":"req_over"}"#,
                FailureClass::Overloaded,
                FailureScopeKind::Provider,
                true,
            ),
            (
                500,
                r#"{"type":"error","error":{"type":"api_error","message":"internal"},"request_id":"req_500"}"#,
                FailureClass::Provider,
                FailureScopeKind::Endpoint,
                true,
            ),
            (
                400,
                r#"{"type":"error","error":{"type":"invalid_request_error","message":"context window exceeded"},"request_id":"req_ctx"}"#,
                FailureClass::ContextOverflow,
                FailureScopeKind::Request,
                false,
            ),
        ] {
            let failure =
                classify_http_error(status, body, Some("header".into()), Some(2_000), &target);
            assert_eq!(failure.class, class);
            assert_eq!(failure.scope.kind, scope);
            assert_eq!(failure.retry.retryable, retryable);
            assert!(failure.provider_request_id.unwrap().starts_with("req_"));
        }
    }

    #[test]
    fn model_specific_thinking_and_effort_combinations_fail_locally() {
        let target = target("https://api.anthropic.com".into());
        let mut manual = request();
        manual.thinking = ThinkingConfig::Enabled {
            budget_tokens: 2048,
            display: Some(ThinkingDisplay::Summarized),
            interleaved: true,
        };
        assert_eq!(
            validate_request(&manual, &target).unwrap_err().class,
            FailureClass::Configuration
        );

        let mut unsupported_effort = request();
        unsupported_effort.model = "claude-haiku-5".into();
        let mut haiku_target = target;
        haiku_target.model.id = "claude-haiku-5".into();
        unsupported_effort.effort = Some(Effort::Xhigh);
        assert_eq!(
            validate_request(&unsupported_effort, &haiku_target)
                .unwrap_err()
                .class,
            FailureClass::Configuration
        );
    }

    #[test]
    fn continuation_requires_unchanged_thinking_and_exact_tool_relationships() {
        let target = target("https://api.anthropic.com".into());
        let mut continued = request();
        continued
            .messages
            .push(super::super::adapter::ProviderMessage {
                role: ProviderMessageRole::Assistant,
                content: vec![
                    ProviderContent::Thinking {
                        thinking: String::new(),
                        signature: OpaqueProviderData::new(json!("sig")),
                    },
                    ProviderContent::ToolUse {
                        id: "toolu_1".into(),
                        name: "file_read".into(),
                        input: json!({"path":"README.md"}),
                    },
                ],
            });
        continued
            .messages
            .push(super::super::adapter::ProviderMessage {
                role: ProviderMessageRole::User,
                content: vec![ProviderContent::ToolResult {
                    tool_use_id: "toolu_1".into(),
                    content: "result".into(),
                    is_error: false,
                }],
            });
        validate_request(&continued, &target).unwrap();

        let ProviderContent::Thinking { signature, .. } = &mut continued.messages[1].content[0]
        else {
            unreachable!();
        };
        *signature = OpaqueProviderData::new(Value::String(String::new()));
        assert_eq!(
            validate_request(&continued, &target).unwrap_err().class,
            FailureClass::Configuration
        );
    }

    #[test]
    fn consecutive_tool_results_preserve_each_tool_use_relationship() {
        let target = target("https://api.anthropic.com".into());
        let mut continued = request();
        continued
            .messages
            .push(super::super::adapter::ProviderMessage {
                role: ProviderMessageRole::Assistant,
                content: ["toolu_1", "toolu_2"]
                    .into_iter()
                    .map(|id| ProviderContent::ToolUse {
                        id: id.into(),
                        name: "file_read".into(),
                        input: json!({"path":"README.md"}),
                    })
                    .collect(),
            });
        continued
            .messages
            .push(super::super::adapter::ProviderMessage {
                role: ProviderMessageRole::User,
                content: ["toolu_1", "toolu_2"]
                    .into_iter()
                    .map(|tool_use_id| ProviderContent::ToolResult {
                        tool_use_id: tool_use_id.into(),
                        content: "result".into(),
                        is_error: false,
                    })
                    .collect(),
            });
        validate_request(&continued, &target).unwrap();

        let ProviderContent::ToolResult { tool_use_id, .. } = &mut continued.messages[2].content[1]
        else {
            unreachable!();
        };
        *tool_use_id = "toolu_1".into();
        assert_eq!(
            validate_request(&continued, &target).unwrap_err().class,
            FailureClass::Configuration
        );
    }

    #[test]
    fn refusal_context_and_timeout_outcomes_are_typed() {
        assert_eq!(parse_finish_reason("refusal"), FinishReason::Refusal);
        assert_eq!(
            parse_finish_reason("model_context_window_exceeded"),
            FinishReason::ContextWindowExceeded
        );
        let target = target("https://api.anthropic.com".into());
        let first = timeout_failure(false, &target);
        assert_eq!(first.class, FailureClass::FirstEventTimeout);
        assert!(first.retry.retryable);
        let idle = timeout_failure(true, &target);
        assert_eq!(idle.class, FailureClass::IdleTimeout);
        assert_eq!(idle.scope.kind, FailureScopeKind::Endpoint);
    }

    #[test]
    fn direct_http_request_uses_messages_api_and_excludes_tool_executor_metadata() {
        let (url, captured) = one_shot_server(200, STREAM, &[]);
        let adapter =
            AnthropicMessagesAdapter::new(target(url), credential(), AnthropicTimeouts::default())
                .unwrap();
        let response = adapter
            .stream(
                &request(),
                &super::super::adapter::NeverCancelled,
                &mut Vec::new(),
            )
            .unwrap();
        assert_eq!(response.finish_reason, FinishReason::ToolUse);
        let request = captured.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(request.starts_with("POST /v1/messages HTTP/1.1"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("x-api-key: test-secret-never-log")
        );
        assert!(request.contains("\"stream\":true"));
        assert!(request.contains("\"output_config\":{\"effort\":\"medium\"}"));
        assert!(request.contains("\"cache_control\":{\"type\":\"ephemeral\"}"));
        assert!(!request.contains("resource_claims"));
        assert!(!request.contains("execution_mode"));
        assert!(!request.contains("Claude Code"));
    }

    #[test]
    fn http_retry_after_and_midstream_overload_are_typed() {
        let error_body = r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow"},"request_id":"req_429"}"#;
        let (url, _) = one_shot_server(429, error_body, &[("Retry-After", "3")]);
        let adapter =
            AnthropicMessagesAdapter::new(target(url), credential(), AnthropicTimeouts::default())
                .unwrap();
        let error = adapter
            .stream(
                &request(),
                &super::super::adapter::NeverCancelled,
                &mut Vec::new(),
            )
            .unwrap_err();
        assert_eq!(error.class, FailureClass::RateLimited);
        assert_eq!(error.retry.after_ms, Some(3_000));

        let stream_error = concat!(
            "event: error\n",
            "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"busy\"},\"request_id\":\"req_stream\"}\n\n"
        );
        let error = parse_sse(
            BufReader::new(stream_error.as_bytes()),
            None,
            &super::super::adapter::NeverCancelled,
            &mut Vec::new(),
            adapter.target(),
        )
        .unwrap_err();
        assert_eq!(error.class, FailureClass::Overloaded);
        assert!(error.retry.retryable);
    }

    fn one_shot_server(
        status: u16,
        body: &'static str,
        headers: &'static [(&'static str, &'static str)],
    ) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            let _ = sender.send(request);
            let reason = if status == 200 { "OK" } else { "Error" };
            write!(
                stream,
                "HTTP/1.1 {status} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
                if status == 200 {
                    "text/event-stream"
                } else {
                    "application/json"
                },
                body.len()
            )
            .unwrap();
            for (name, value) in headers {
                write!(stream, "{name}: {value}\r\n").unwrap();
            }
            write!(stream, "\r\n{body}").unwrap();
        });
        (format!("http://{address}"), receiver)
    }

    fn delayed_stream_server(
        prefix: &'static str,
        suffix: &'static str,
        delay: Duration,
    ) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            let _ = sender.send(request);
            let length = prefix.len() + suffix.len();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n{prefix}"
            )
            .unwrap();
            stream.flush().unwrap();
            std::thread::sleep(delay);
            let _ = stream.write_all(suffix.as_bytes());
        });
        (format!("http://{address}"), receiver)
    }

    fn read_http_request(stream: &mut TcpStream) -> String {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 4096];
        let mut expected = None;
        loop {
            let count = stream.read(&mut buffer).unwrap();
            if count == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..count]);
            if expected.is_none()
                && let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n")
            {
                let headers = String::from_utf8_lossy(&bytes[..end]);
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                expected = Some(end + 4 + length);
            }
            if expected.is_some_and(|expected| bytes.len() >= expected) {
                break;
            }
        }
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    #[ignore = "live Anthropic contract; set ANTHROPIC_API_KEY and ZIRV_ANTHROPIC_LIVE_MODEL"]
    fn live_anthropic_messages_contract() {
        let key = std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY");
        let model = std::env::var("ZIRV_ANTHROPIC_LIVE_MODEL")
            .expect("ZIRV_ANTHROPIC_LIVE_MODEL must name an entitled exact model id");
        let mut target = target("https://api.anthropic.com".into());
        target.model.id = model.clone();
        let adapter = AnthropicMessagesAdapter::new(
            target,
            Credential {
                secret: Secret::new(key),
                expires_at: None,
            },
            AnthropicTimeouts::default(),
        )
        .unwrap();
        let mut request = request();
        request.model = model;
        request.messages = vec![super::super::adapter::ProviderMessage {
            role: ProviderMessageRole::User,
            content: vec![ProviderContent::Text {
                text: "Reply with exactly: zirv-live-ok".into(),
            }],
        }];
        request.tools.clear();
        request.thinking = ThinkingConfig::Default;
        request.effort = None;
        request.cache = CacheMode::Disabled;
        let response = adapter
            .stream(
                &request,
                &super::super::adapter::NeverCancelled,
                &mut Vec::new(),
            )
            .unwrap();
        assert!(!response.message_id.is_empty());
        assert!(response.usage.output_tokens > 0);
    }
}
