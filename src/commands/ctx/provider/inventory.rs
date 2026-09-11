use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use super::capability::{ModelCapabilities, declared};
use super::config::{NativeConfig, ResolvedEndpoint};
use super::credential::{Credential, CredentialRef, CredentialStore, resolve};
use super::probe::{Probe, ProbeResult};
use super::{
    AccountId, BillingClass, BillingPoolId, EndpointId, ModelId, Protocol, ProviderId, RouteId,
    Support, provider, providers,
};
use crate::commands::ctx::config::EnvLookup;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RouteState {
    #[allow(dead_code)] // Load-time validation advances every emitted N02 route to Configured.
    Recognized,
    Configured,
    Credentialed,
    Reachable,
    Authenticated,
    Validated,
}

#[derive(Clone, Debug, Serialize)]
pub struct RouteReport {
    pub route: RouteId,
    pub account: AccountId,
    pub pool: BillingPoolId,
    pub endpoint: EndpointId,
    pub provider: ProviderId,
    pub protocol: Protocol,
    pub model: ModelId,
    pub billing: BillingClass,
    pub state: RouteState,
    pub capabilities: ModelCapabilities,
    pub allowed: bool,
    pub problems: Vec<String>,
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct EndpointReport {
    pub id: EndpointId,
    pub provider: ProviderId,
    pub base_url: String,
    pub vendor: String,
    pub implicit: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct AccountReport {
    pub id: AccountId,
    pub provider: ProviderId,
    pub billing: BillingClass,
}

#[derive(Clone, Debug, Serialize)]
pub struct PoolReport {
    pub pool: BillingPoolId,
    pub accounts: Vec<AccountReport>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProviderReport {
    pub id: String,
    pub support: Support,
    pub configured: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct AccessRow {
    pub role: String,
    pub route: Option<RouteId>,
    pub state: Option<RouteState>,
    pub state_text: String,
    pub allowed: bool,
    pub problem: Option<String>,
}

pub type AccessMatrix = Vec<AccessRow>;

#[derive(Clone, Debug, Serialize)]
pub struct Inventory {
    pub providers: Vec<ProviderReport>,
    pub endpoints: Vec<EndpointReport>,
    pub pools: Vec<PoolReport>,
    pub routes: Vec<RouteReport>,
    pub access: AccessMatrix,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelResolutionError {
    Ambiguous { candidates: Vec<String> },
    UnknownModel { vendor: String, known: Vec<String> },
}

impl std::fmt::Display for ModelResolutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ambiguous { candidates } => write!(
                f,
                "model is ambiguous; use an exact id or alias (candidates: {})",
                candidates.join(", ")
            ),
            Self::UnknownModel { vendor, known } => write!(
                f,
                "unknown model for vendor `{vendor}` (known ids: {})",
                known.join(", ")
            ),
        }
    }
}

impl std::error::Error for ModelResolutionError {}

pub(crate) fn resolve_model(
    vendor_slug: &str,
    requested: &str,
) -> Result<(ModelId, Option<String>), ModelResolutionError> {
    let normalized = crate::commands::ctx::catalogue::normalize_id(requested);
    let needle = normalized.to_ascii_lowercase();
    let Some(vendor) = crate::commands::ctx::catalogue::vendor(vendor_slug) else {
        return Ok((
            ModelId {
                vendor: vendor_slug.to_string(),
                id: normalized.into_owned(),
            },
            Some("not in the catalogue; declared by the operator".into()),
        ));
    };
    if vendor.rungs.is_empty() {
        return Ok((
            ModelId {
                vendor: vendor_slug.to_string(),
                id: normalized.into_owned(),
            },
            Some("not in the catalogue; declared by the operator".into()),
        ));
    }
    if let Some(rung) = vendor.rungs.iter().find(|rung| {
        rung.id.eq_ignore_ascii_case(&needle) || rung.alias.eq_ignore_ascii_case(&needle)
    }) {
        return Ok((
            ModelId {
                vendor: vendor_slug.to_string(),
                id: rung.id.to_string(),
            },
            None,
        ));
    }
    let candidates: Vec<String> = vendor
        .rungs
        .iter()
        .filter(|rung| {
            rung.id.to_ascii_lowercase().contains(&needle)
                || rung.alias.to_ascii_lowercase().contains(&needle)
                || needle.contains(&rung.id.to_ascii_lowercase())
                || needle.contains(&rung.alias.to_ascii_lowercase())
        })
        .map(|rung| rung.id.to_string())
        .collect();
    if !candidates.is_empty() {
        return Err(ModelResolutionError::Ambiguous { candidates });
    }
    Err(ModelResolutionError::UnknownModel {
        vendor: vendor_slug.to_string(),
        known: vendor
            .rungs
            .iter()
            .map(|rung| rung.id.to_string())
            .collect(),
    })
}

impl Inventory {
    pub fn build(
        cfg: &NativeConfig,
        env: EnvLookup<'_>,
        store: &dyn CredentialStore,
        now: u64,
        probe: Option<&dyn Probe>,
    ) -> Self {
        let endpoints = cfg.effective_endpoints();
        let mut routes = Vec::new();
        for (route_id, route) in &cfg.routes {
            let Some(account) = cfg.accounts.get(&route.account) else {
                continue;
            };
            let Some(endpoint_id) = cfg.route_endpoint(route) else {
                continue;
            };
            let Some(endpoint) = endpoints.get(&endpoint_id) else {
                continue;
            };
            let Some(spec) = provider(account.provider.as_ref()) else {
                continue;
            };
            let Ok((model, catalogue_note)) = resolve_model(&endpoint.vendor, &route.model) else {
                continue;
            };
            let mut report = RouteReport {
                route: route_id.clone(),
                account: route.account.clone(),
                pool: cfg.account_pool(&route.account),
                endpoint: endpoint_id,
                provider: account.provider.clone(),
                protocol: spec.protocol,
                capabilities: declared(spec.protocol, &model),
                model,
                billing: account.billing,
                state: RouteState::Configured,
                allowed: cfg.allowed_routes().contains(route_id),
                problems: Vec::new(),
                notes: catalogue_note.into_iter().collect(),
            };
            if account.billing == BillingClass::Subscription {
                report.problems.push(format!(
                    "account `{}` is subscription-billed; the native `{}` route needs an API credential. The harness backend remains the way to spend that subscription.",
                    route.account, account.provider
                ));
                routes.push(report);
                continue;
            }

            let credential = resolve_account_credential(account, spec, env, store, now);
            let credential = match credential {
                Ok(credential) => {
                    report.state = RouteState::Credentialed;
                    credential
                }
                Err(problem) => {
                    report.problems.push(problem);
                    routes.push(report);
                    continue;
                }
            };

            if let Some(probe) = probe {
                apply_probe(&mut report, endpoint, spec, credential.as_ref(), probe);
            }
            routes.push(report);
        }

        let mut grouped: BTreeMap<BillingPoolId, Vec<AccountReport>> = BTreeMap::new();
        for (id, account) in &cfg.accounts {
            grouped
                .entry(cfg.account_pool(id))
                .or_default()
                .push(AccountReport {
                    id: id.clone(),
                    provider: account.provider.clone(),
                    billing: account.billing,
                });
        }
        let pools = grouped
            .into_iter()
            .map(|(pool, accounts)| PoolReport { pool, accounts })
            .collect();
        let provider_reports = providers()
            .iter()
            .map(|spec| ProviderReport {
                id: spec.id.to_string(),
                support: spec.support,
                configured: cfg
                    .accounts
                    .values()
                    .any(|account| account.provider.as_ref() == spec.id),
            })
            .collect();
        let endpoint_reports = endpoints.values().map(endpoint_report).collect();
        let access = access_matrix(cfg, &routes);
        Self {
            providers: provider_reports,
            endpoints: endpoint_reports,
            pools,
            routes,
            access,
        }
    }
}

fn endpoint_report(endpoint: &ResolvedEndpoint) -> EndpointReport {
    EndpointReport {
        id: endpoint.id.clone(),
        provider: endpoint.provider.clone(),
        base_url: endpoint.base_url.clone(),
        vendor: endpoint.vendor.clone(),
        implicit: endpoint.implicit,
    }
}

fn resolve_account_credential(
    account: &super::config::AccountConfig,
    spec: &super::ProviderSpec,
    env: EnvLookup<'_>,
    store: &dyn CredentialStore,
    now: u64,
) -> Result<Option<Credential>, String> {
    if let Some(reference) = &account.credential {
        return resolve(reference, env, store, now)
            .map(Some)
            .map_err(|error| error.to_string());
    }
    if spec.default_credential_env.is_empty() {
        return Ok(None);
    }
    let mut last_error = None;
    for name in spec.default_credential_env {
        let reference = CredentialRef::Env((*name).to_string());
        match resolve(&reference, env, store, now) {
            Ok(credential) => return Ok(Some(credential)),
            Err(error) => last_error = Some(error.to_string()),
        }
    }
    Err(last_error.unwrap_or_else(|| "credential is missing".into()))
}

fn apply_probe(
    report: &mut RouteReport,
    endpoint: &ResolvedEndpoint,
    spec: &super::ProviderSpec,
    credential: Option<&Credential>,
    probe: &dyn Probe,
) {
    match probe.models(&endpoint.base_url, spec, credential) {
        ProbeResult::Unreachable(problem) => report.problems.push(problem),
        ProbeResult::Http {
            status: 200,
            model_ids,
        } => {
            report.state = RouteState::Authenticated;
            if !model_ids
                .iter()
                .any(|id| id.eq_ignore_ascii_case(&report.model.id))
            {
                report.notes.push(
                    "model not listed for this account (entitlement restriction or alias mismatch)"
                        .into(),
                );
            }
        }
        ProbeResult::Http {
            status: 401 | 403, ..
        } => {
            report.state = RouteState::Reachable;
            report
                .problems
                .push("credential rejected (expired or invalid)".into());
        }
        ProbeResult::Http { status: 404, .. }
            if spec.protocol == Protocol::OpenAiChatCompatible =>
        {
            report.state = RouteState::Reachable;
            report.notes.push("models listing unsupported".into());
        }
        ProbeResult::Http { status, .. } => {
            report.state = RouteState::Reachable;
            report
                .problems
                .push(format!("models probe returned HTTP {status}"));
        }
    }
}

fn access_matrix(cfg: &NativeConfig, reports: &[RouteReport]) -> AccessMatrix {
    let mut roles: BTreeSet<String> = [
        "orchestrator",
        "sub-orchestrator",
        "worker",
        "reviewer",
        "distiller",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    roles.extend(cfg.roles.keys().cloned());
    roles
        .into_iter()
        .map(|role| {
            let Some(route_id) = cfg.roles.get(&role) else {
                return AccessRow {
                    role,
                    route: None,
                    state: None,
                    state_text: "unconfigured".into(),
                    allowed: false,
                    problem: Some("no native route configured".into()),
                };
            };
            let report = reports.iter().find(|report| &report.route == route_id);
            match report {
                Some(report) => AccessRow {
                    role,
                    route: Some(route_id.clone()),
                    state: Some(report.state),
                    state_text: state_text(report.state).into(),
                    allowed: report.allowed,
                    problem: report.problems.first().cloned(),
                },
                None => AccessRow {
                    role,
                    route: Some(route_id.clone()),
                    state: None,
                    state_text: "unconfigured".into(),
                    allowed: false,
                    problem: Some("no native route configured".into()),
                },
            }
        })
        .collect()
}

pub fn state_text(state: RouteState) -> &'static str {
    match state {
        RouteState::Recognized => "recognized (configuration unproven)",
        RouteState::Configured => "configured (credential/access unproven)",
        RouteState::Credentialed => "credentialed (access unproven -- run --live)",
        RouteState::Reachable => "reachable (authentication unproven)",
        RouteState::Authenticated => "authenticated",
        RouteState::Validated => "validated",
    }
}

#[cfg(test)]
pub fn has_verified_capability(capabilities: &ModelCapabilities) -> bool {
    [
        capabilities.tools,
        capabilities.streaming,
        capabilities.vision,
        capabilities.structured_output,
        capabilities.prompt_caching,
        capabilities.reasoning_controls,
        capabilities.continuation,
    ]
    .into_iter()
    .any(super::capability::Capability::is_verified)
}

#[cfg(test)]
mod tests {
    use super::super::config::NativeConfig;
    use super::super::credential::FakeStore;
    use super::super::probe::FakeProbe;
    use super::*;

    fn config(text: &str) -> NativeConfig {
        let mut cfg: NativeConfig = toml::from_str(text).unwrap();
        cfg.policy.allowed_routes = Some(cfg.routes.keys().cloned().collect());
        cfg
    }

    #[test]
    fn exact_resolution_never_uses_a_fuzzy_strongest_match() {
        let exact = resolve_model("anthropic", "sonnet").unwrap().0;
        assert_eq!(exact.id, "claude-sonnet-5");
        let ambiguous = resolve_model("openai", "gpt-5.6").unwrap_err();
        assert!(matches!(ambiguous, ModelResolutionError::Ambiguous { .. }));
        assert!(ambiguous.to_string().contains("gpt-5.6-sol"));
        assert!(ambiguous.to_string().contains("gpt-5.6-terra"));
        let unknown = resolve_model("anthropic", "unknown-alias").unwrap_err();
        assert!(unknown.to_string().contains("claude-sonnet-5"));
    }

    #[test]
    fn pools_group_accounts_not_routes() {
        let cfg = config(
            "schema=1\n[account.one]\nprovider='anthropic'\n[account.two]\nprovider='anthropic'\n[route.a]\naccount='one'\nmodel='haiku'\n[route.b]\naccount='one'\nmodel='sonnet'\n[route.c]\naccount='two'\nmodel='opus'\n",
        );
        let inventory = Inventory::build(
            &cfg,
            &|_| Some("key".into()),
            &FakeStore::default(),
            0,
            None,
        );
        assert_eq!(inventory.pools.len(), 2);
        assert_eq!(
            inventory
                .pools
                .iter()
                .map(|pool| pool.accounts.len())
                .sum::<usize>(),
            2
        );
        assert_eq!(inventory.routes.len(), 3);
    }

    #[test]
    fn shared_pool_is_explicitly_allowed() {
        let cfg = config(
            "schema=1\n[account.one]\nprovider='anthropic'\npool='shared'\n[account.two]\nprovider='anthropic'\npool='shared'\n[route.a]\naccount='one'\nmodel='haiku'\n[route.b]\naccount='two'\nmodel='sonnet'\n",
        );
        let inventory = Inventory::build(
            &cfg,
            &|_| Some("key".into()),
            &FakeStore::default(),
            0,
            None,
        );
        assert_eq!(inventory.pools.len(), 1);
        assert_eq!(inventory.pools[0].accounts.len(), 2);
    }

    #[test]
    fn subscription_route_stops_at_configured_without_harming_api_route() {
        let cfg = config(
            "schema=1\n[account.plan]\nprovider='anthropic'\nbilling='subscription'\n[account.api]\nprovider='anthropic'\n[route.plan]\naccount='plan'\nmodel='haiku'\n[route.api]\naccount='api'\nmodel='sonnet'\n",
        );
        let inventory = Inventory::build(
            &cfg,
            &|_| Some("key".into()),
            &FakeStore::default(),
            0,
            None,
        );
        let plan = inventory
            .routes
            .iter()
            .find(|route| route.route.as_ref() == "plan")
            .unwrap();
        assert_eq!(plan.state, RouteState::Configured);
        assert!(
            plan.problems[0]
                .contains("The harness backend remains the way to spend that subscription.")
        );
        let api = inventory
            .routes
            .iter()
            .find(|route| route.route.as_ref() == "api")
            .unwrap();
        assert_eq!(api.state, RouteState::Credentialed);
        assert!(api.problems.is_empty());
    }

    #[test]
    fn live_probe_states_distinguish_network_and_authentication() {
        let cfg = config(
            "schema=1\n[account.work]\nprovider='anthropic'\n[route.work]\naccount='work'\nmodel='sonnet'\n",
        );
        let build = |result| {
            Inventory::build(
                &cfg,
                &|_| Some("key".into()),
                &FakeStore::default(),
                0,
                Some(&FakeProbe::new(result)),
            )
        };
        let listed = build(ProbeResult::Http {
            status: 200,
            model_ids: vec!["claude-sonnet-5".into()],
        });
        assert_eq!(listed.routes[0].state, RouteState::Authenticated);
        assert!(listed.routes[0].notes.is_empty());
        let absent = build(ProbeResult::Http {
            status: 200,
            model_ids: vec![],
        });
        assert!(absent.routes[0].notes[0].contains("model not listed"));
        let rejected = build(ProbeResult::Http {
            status: 401,
            model_ids: vec![],
        });
        assert_eq!(rejected.routes[0].state, RouteState::Reachable);
        assert!(rejected.routes[0].problems[0].contains("credential rejected"));
        let down = build(ProbeResult::Unreachable("connection refused".into()));
        assert_eq!(down.routes[0].state, RouteState::Credentialed);
    }

    #[test]
    fn inventory_never_claims_validated_in_n02() {
        let cfg = config(
            "schema=1\n[account.work]\nprovider='anthropic'\n[route.work]\naccount='work'\nmodel='sonnet'\n",
        );
        let inventory = Inventory::build(
            &cfg,
            &|_| Some("key".into()),
            &FakeStore::default(),
            0,
            None,
        );
        assert!(
            inventory
                .routes
                .iter()
                .all(|route| route.state != RouteState::Validated)
        );
        assert!(
            inventory
                .routes
                .iter()
                .all(|route| !has_verified_capability(&route.capabilities))
        );
    }
}
