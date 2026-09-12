//! Provider-neutral direct-model transport contract (native roadmap N07+).
//!
//! A provider adapter transports one already-compiled request. It does not
//! own the agent loop, execute tools, choose a fallback route, or persist the
//! conversation. Those remain Zirv runtime responsibilities.

#![allow(dead_code)] // N09 wires this provider-neutral contract into the runtime loop.

use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

use super::config::NativeConfig;
use super::credential::{Credential, CredentialRef, CredentialStore, resolve};
use super::inventory::resolve_model;
use super::{
    AccountId, BillingClass, BillingPoolId, EndpointId, ModelId, Protocol, ProviderId, RouteId,
    provider,
};
use crate::commands::ctx::config::EnvLookup;
use crate::commands::ctx::runtime::context::{
    CompiledNativeContext, MessageRole as ContextMessageRole,
};
use crate::commands::ctx::runtime::tools::ToolDefinition;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderTarget {
    pub route: RouteId,
    pub provider: ProviderId,
    pub endpoint: EndpointId,
    pub account: AccountId,
    pub billing_pool: BillingPoolId,
    pub protocol: Protocol,
    pub base_url: String,
    pub model: ModelId,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheMode {
    #[default]
    Disabled,
    Ephemeral5m,
    Ephemeral1h,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingDisplay {
    Summarized,
    Omitted,
    Updates,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ThinkingConfig {
    #[default]
    Default,
    Disabled,
    Adaptive {
        #[serde(default)]
        display: Option<ThinkingDisplay>,
    },
    Enabled {
        budget_tokens: u64,
        #[serde(default)]
        display: Option<ThinkingDisplay>,
        #[serde(default)]
        interleaved: bool,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProviderRequest {
    pub model: String,
    pub system: Vec<String>,
    pub messages: Vec<ProviderMessage>,
    pub tools: Vec<ToolDefinition>,
    pub max_output_tokens: u64,
    pub stop_sequences: Vec<String>,
    pub thinking: ThinkingConfig,
    pub effort: Option<Effort>,
    pub cache: CacheMode,
}

impl ProviderRequest {
    pub fn from_compiled(
        model: impl Into<String>,
        compiled: &CompiledNativeContext,
        max_output_tokens: u64,
    ) -> Self {
        let mut system = Vec::new();
        let mut user_content = Vec::new();
        for message in &compiled.messages {
            match message.role {
                ContextMessageRole::Instruction => system.push(message.content.clone()),
                ContextMessageRole::Data => user_content.push(ProviderContent::Text {
                    text: message.content.clone(),
                }),
            }
        }
        let messages = if user_content.is_empty() {
            Vec::new()
        } else {
            vec![ProviderMessage {
                role: ProviderMessageRole::User,
                content: user_content,
            }]
        };
        Self {
            model: model.into(),
            system,
            messages,
            tools: compiled.tools.clone(),
            max_output_tokens,
            stop_sequences: Vec::new(),
            thinking: ThinkingConfig::Default,
            effort: None,
            cache: CacheMode::Disabled,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderMessageRole {
    User,
    Assistant,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProviderMessage {
    pub role: ProviderMessageRole,
    pub content: Vec<ProviderContent>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderContent {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        signature: super::OpaqueProviderData,
    },
    RedactedThinking {
        data: super::OpaqueProviderData,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderUsage {
    pub input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub reasoning_tokens: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    EndTurn,
    MaxTokens,
    StopSequence,
    ToolUse,
    PauseTurn,
    Refusal,
    ContextWindowExceeded,
    Unknown(String),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProviderResponse {
    pub message_id: String,
    pub model: String,
    pub content: Vec<ProviderContent>,
    pub finish_reason: FinishReason,
    pub stop_sequence: Option<String>,
    pub stop_details: Option<super::OpaqueProviderData>,
    pub usage: ProviderUsage,
    pub request_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderStreamEvent {
    /// A syntactically valid provider event, used by the transport watchdog.
    /// Adapter consumers do not need to render or persist it.
    ProtocolActivity,
    MessageStarted {
        id: String,
        model: String,
    },
    TextDelta {
        index: usize,
        text: String,
    },
    ThinkingDelta {
        index: usize,
        text: String,
    },
    ToolInputDelta {
        index: usize,
        partial_json: String,
    },
    BlockCompleted {
        index: usize,
    },
    Ping,
}

pub trait EventSink {
    fn push(&mut self, event: ProviderStreamEvent);
}

impl EventSink for Vec<ProviderStreamEvent> {
    fn push(&mut self, event: ProviderStreamEvent) {
        Vec::push(self, event);
    }
}

pub trait Cancellation: std::fmt::Debug + Send + Sync {
    fn is_cancelled(&self) -> bool;
}

#[derive(Debug, Default)]
pub struct NeverCancelled;

impl Cancellation for NeverCancelled {
    fn is_cancelled(&self) -> bool {
        false
    }
}

#[derive(Debug, Default)]
pub struct CancellationFlag(AtomicBool);

impl CancellationFlag {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
}

impl Cancellation for CancellationFlag {
    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureClass {
    Cancelled,
    FirstEventTimeout,
    IdleTimeout,
    Authentication,
    Permission,
    Entitlement,
    ModelAccess,
    Configuration,
    ContextOverflow,
    RateLimited,
    Overloaded,
    Transport,
    Provider,
    InvalidStream,
    InvalidToolArguments,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureScopeKind {
    Request,
    Model,
    Account,
    BillingPool,
    Endpoint,
    Provider,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailureScope {
    pub kind: FailureScopeKind,
    pub id: Option<String>,
}

impl FailureScope {
    pub fn request() -> Self {
        Self {
            kind: FailureScopeKind::Request,
            id: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RetryHint {
    pub retryable: bool,
    pub after_ms: Option<u64>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ProviderFailure {
    pub class: FailureClass,
    pub scope: FailureScope,
    pub message: String,
    pub http_status: Option<u16>,
    pub provider_request_id: Option<String>,
    pub retry: RetryHint,
}

impl std::fmt::Debug for ProviderFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderFailure")
            .field("class", &self.class)
            .field("scope", &self.scope)
            .field("message", &self.message)
            .field("http_status", &self.http_status)
            .field("provider_request_id", &self.provider_request_id)
            .field("retry", &self.retry)
            .finish()
    }
}

impl std::fmt::Display for ProviderFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)?;
        if let Some(request_id) = &self.provider_request_id {
            write!(f, " (provider request {request_id})")?;
        }
        Ok(())
    }
}

impl std::error::Error for ProviderFailure {}

impl ProviderFailure {
    pub fn new(class: FailureClass, scope: FailureScope, message: impl Into<String>) -> Self {
        Self {
            class,
            scope,
            message: message.into(),
            http_status: None,
            provider_request_id: None,
            retry: RetryHint::default(),
        }
    }
}

pub trait ProviderAdapter: std::fmt::Debug + Send + Sync {
    fn protocol(&self) -> Protocol;
    fn target(&self) -> &ProviderTarget;
    fn stream(
        &self,
        request: &ProviderRequest,
        cancellation: &dyn Cancellation,
        sink: &mut dyn EventSink,
    ) -> Result<ProviderResponse, ProviderFailure>;
}

pub(crate) fn resolve_target(
    config: &NativeConfig,
    route_id: &RouteId,
    env: EnvLookup<'_>,
    store: &dyn CredentialStore,
    now: u64,
) -> Result<(ProviderTarget, Option<Credential>), ProviderFailure> {
    if !config.allowed_routes().contains(route_id) {
        return Err(ProviderFailure::new(
            FailureClass::Configuration,
            FailureScope::request(),
            format!("native route `{route_id}` is not allowed by effective policy"),
        ));
    }
    let route = config.routes.get(route_id).ok_or_else(|| {
        ProviderFailure::new(
            FailureClass::Configuration,
            FailureScope::request(),
            format!("unknown native route `{route_id}`"),
        )
    })?;
    let account = config.accounts.get(&route.account).ok_or_else(|| {
        ProviderFailure::new(
            FailureClass::Configuration,
            FailureScope::request(),
            format!(
                "route `{route_id}` references unknown account `{}`",
                route.account
            ),
        )
    })?;
    let spec = provider(account.provider.as_ref()).ok_or_else(|| {
        ProviderFailure::new(
            FailureClass::Configuration,
            FailureScope::request(),
            format!("unknown provider `{}`", account.provider),
        )
    })?;
    if account.billing == BillingClass::Subscription {
        return Err(ProviderFailure::new(
            FailureClass::Entitlement,
            FailureScope {
                kind: FailureScopeKind::Account,
                id: Some(route.account.to_string()),
            },
            format!(
                "account `{}` is subscription-billed; direct provider calls require API entitlement",
                route.account
            ),
        ));
    }
    let endpoint_id = config.route_endpoint(route).ok_or_else(|| {
        ProviderFailure::new(
            FailureClass::Configuration,
            FailureScope::request(),
            format!("route `{route_id}` has no endpoint"),
        )
    })?;
    let endpoints = config.effective_endpoints();
    let endpoint = endpoints.get(&endpoint_id).ok_or_else(|| {
        ProviderFailure::new(
            FailureClass::Configuration,
            FailureScope {
                kind: FailureScopeKind::Endpoint,
                id: Some(endpoint_id.to_string()),
            },
            format!("route `{route_id}` endpoint `{endpoint_id}` cannot be resolved"),
        )
    })?;
    let (model, _) = resolve_model(route_id, &endpoint_id, &endpoint.vendor, &route.model)
        .map_err(|error| {
            ProviderFailure::new(
                FailureClass::Configuration,
                FailureScope {
                    kind: FailureScopeKind::Model,
                    id: Some(route.model.clone()),
                },
                error.to_string(),
            )
        })?;

    let credential = if let Some(reference) = account.credential.as_ref() {
        Some(resolve(reference, env, store, now).map_err(|error| {
            ProviderFailure::new(
                FailureClass::Authentication,
                FailureScope {
                    kind: FailureScopeKind::Account,
                    id: Some(route.account.to_string()),
                },
                error.to_string(),
            )
        })?)
    } else {
        spec.default_credential_env.iter().find_map(|name| {
            let reference = CredentialRef::Env((*name).to_string());
            resolve(&reference, env, store, now).ok()
        })
    };

    Ok((
        ProviderTarget {
            route: route_id.clone(),
            provider: account.provider.clone(),
            endpoint: endpoint_id,
            account: route.account.clone(),
            billing_pool: config.account_pool(&route.account),
            protocol: spec.protocol,
            base_url: endpoint.base_url.clone(),
            model,
        },
        credential,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::provider::credential::FakeStore;

    #[test]
    fn cancellation_flag_is_monotonic() {
        let flag = CancellationFlag::default();
        assert!(!flag.is_cancelled());
        flag.cancel();
        assert!(flag.is_cancelled());
    }

    #[test]
    fn subscription_routes_fail_before_credentials_are_read() {
        let config: NativeConfig = toml::from_str(
            "schema=1\n[policy]\nallowed_routes=['work']\n[account.work]\nprovider='anthropic'\nbilling='subscription'\n\
             [route.work]\naccount='work'\nmodel='claude-sonnet-5'\n",
        )
        .unwrap();
        let route = RouteId::new("work").unwrap();
        let error = resolve_target(
            &config,
            &route,
            &|_| panic!("credential lookup must not run"),
            &FakeStore::default(),
            0,
        )
        .unwrap_err();
        assert_eq!(error.class, FailureClass::Entitlement);
        assert_eq!(error.scope.kind, FailureScopeKind::Account);
    }
}
