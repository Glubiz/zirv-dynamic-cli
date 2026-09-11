use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::credential::CredentialRef;
use super::{
    AccountId, BillingClass, BillingPoolId, EndpointId, ProviderId, RouteId, Support, provider,
};
use crate::commands::ctx::CtxResult;

pub const NATIVE_CONFIG_FILE: &str = "native.toml";
pub const NATIVE_SCHEMA: u32 = 1;

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NativeConfig {
    pub schema: u32,
    #[serde(rename = "endpoint")]
    pub endpoints: BTreeMap<EndpointId, EndpointConfig>,
    #[serde(rename = "account")]
    pub accounts: BTreeMap<AccountId, AccountConfig>,
    #[serde(rename = "route")]
    pub routes: BTreeMap<RouteId, RouteConfig>,
    pub roles: BTreeMap<String, RouteId>,
    pub policy: NativePolicy,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EndpointConfig {
    pub provider: ProviderId,
    pub base_url: Option<String>,
    pub vendor: Option<String>,
}

impl Default for EndpointConfig {
    fn default() -> Self {
        Self {
            provider: ProviderId::new("missing").expect("static provider id"),
            base_url: None,
            vendor: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AccountConfig {
    pub provider: ProviderId,
    pub credential: Option<CredentialRef>,
    pub billing: BillingClass,
    pub pool: Option<BillingPoolId>,
}

impl Default for AccountConfig {
    fn default() -> Self {
        Self {
            provider: ProviderId::new("missing").expect("static provider id"),
            credential: None,
            billing: BillingClass::Api,
            pool: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RouteConfig {
    pub account: AccountId,
    pub endpoint: Option<EndpointId>,
    pub model: String,
}

impl Default for RouteConfig {
    fn default() -> Self {
        Self {
            account: AccountId::new("missing").expect("static account id"),
            endpoint: None,
            model: String::new(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NativePolicy {
    pub allowed_routes: Option<BTreeSet<RouteId>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedEndpoint {
    pub id: EndpointId,
    pub provider: ProviderId,
    pub base_url: String,
    pub vendor: String,
    pub implicit: bool,
}

impl NativeConfig {
    pub fn operator_path(home: &Path) -> PathBuf {
        home.join(crate::utils::SCRIPT_DIR_NAME)
            .join(NATIVE_CONFIG_FILE)
    }

    pub fn repo_path(repo: &Path) -> PathBuf {
        repo.join(crate::utils::SCRIPT_DIR_NAME)
            .join(NATIVE_CONFIG_FILE)
    }

    pub fn load(home: &Path, repo: &Path) -> CtxResult<Option<Self>> {
        let operator_path = Self::operator_path(home);
        let text = match std::fs::read_to_string(&operator_path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(format!("{}: {error}", operator_path.display()).into());
            }
        };
        let mut config: Self = toml::from_str(&text)
            .map_err(|error| format!("{}: {error}", operator_path.display()))?;
        validate_schema(config.schema, &operator_path)?;

        if !crate::utils::repo_is_home(repo) {
            let repo_path = Self::repo_path(repo);
            match std::fs::read_to_string(&repo_path) {
                Ok(repo_text) => {
                    let table: toml::Table = toml::from_str(&repo_text)
                        .map_err(|error| format!("{}: {error}", repo_path.display()))?;
                    reject_untrusted_keys(&table, &repo_path)?;
                    let repo_cfg: RepoConfig = toml::from_str(&repo_text)
                        .map_err(|error| format!("{}: {error}", repo_path.display()))?;
                    validate_schema(repo_cfg.schema, &repo_path)?;
                    if let Some(repo_allowed) = repo_cfg.policy.allowed_routes {
                        let operator_allowed = config
                            .policy
                            .allowed_routes
                            .clone()
                            .unwrap_or_else(|| config.routes.keys().cloned().collect());
                        config.policy.allowed_routes = Some(
                            operator_allowed
                                .intersection(&repo_allowed)
                                .cloned()
                                .collect(),
                        );
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(format!("{}: {error}", repo_path.display()).into()),
            }
        }

        if config.policy.allowed_routes.is_none() {
            config.policy.allowed_routes = Some(config.routes.keys().cloned().collect());
        }
        config.validate(&operator_path)?;
        Ok(Some(config))
    }

    pub fn allowed_routes(&self) -> &BTreeSet<RouteId> {
        self.policy
            .allowed_routes
            .as_ref()
            .expect("validated config always resolves allowed_routes")
    }

    pub fn account_pool(&self, id: &AccountId) -> BillingPoolId {
        self.accounts
            .get(id)
            .and_then(|account| account.pool.clone())
            .unwrap_or_else(|| BillingPoolId::new(id.as_ref()).expect("account ids are pool ids"))
    }

    pub fn effective_endpoints(&self) -> BTreeMap<EndpointId, ResolvedEndpoint> {
        let mut endpoints = BTreeMap::new();
        for spec in super::providers() {
            if let (Some(base_url), Some(vendor)) = (spec.default_base_url, spec.vendor) {
                let id = EndpointId::new(spec.id).expect("static provider id");
                endpoints.insert(
                    id.clone(),
                    ResolvedEndpoint {
                        id,
                        provider: ProviderId::new(spec.id).expect("static provider id"),
                        base_url: base_url.to_string(),
                        vendor: vendor.to_string(),
                        implicit: true,
                    },
                );
            }
        }
        for (id, endpoint) in &self.endpoints {
            let Some(spec) = provider(endpoint.provider.as_ref()) else {
                continue;
            };
            let Some(base_url) = endpoint
                .base_url
                .clone()
                .or_else(|| spec.default_base_url.map(str::to_string))
            else {
                continue;
            };
            let Some(vendor) = endpoint
                .vendor
                .clone()
                .or_else(|| spec.vendor.map(str::to_string))
            else {
                continue;
            };
            endpoints.insert(
                id.clone(),
                ResolvedEndpoint {
                    id: id.clone(),
                    provider: endpoint.provider.clone(),
                    base_url,
                    vendor,
                    implicit: false,
                },
            );
        }
        endpoints
    }

    pub fn route_endpoint(&self, route: &RouteConfig) -> Option<EndpointId> {
        route.endpoint.clone().or_else(|| {
            self.accounts
                .get(&route.account)
                .map(|account| EndpointId::new(account.provider.as_ref()).expect("provider id"))
        })
    }

    fn validate(&self, path: &Path) -> CtxResult<()> {
        for (id, endpoint) in &self.endpoints {
            let key = format!("endpoint.{id}");
            let spec = provider(endpoint.provider.as_ref()).ok_or_else(|| {
                format!(
                    "{}: `{key}.provider` names unknown provider `{}`",
                    path.display(),
                    endpoint.provider
                )
            })?;
            if let Some(base_url) = endpoint.base_url.as_deref() {
                super::super::config::validate_endpoint_base_url(
                    &format!("{key}.base_url"),
                    base_url,
                )
                .map_err(|error| format!("{}: {error}", path.display()))?;
            }
            if spec.id == "openai-compatible" {
                if endpoint.base_url.is_none() {
                    return Err(format!(
                        "{}: `{key}.base_url` is required for openai-compatible",
                        path.display()
                    )
                    .into());
                }
                let vendor = endpoint.vendor.as_deref().ok_or_else(|| {
                    format!(
                        "{}: `{key}.vendor` is required for openai-compatible",
                        path.display()
                    )
                })?;
                ProviderId::new(vendor)
                    .map_err(|error| format!("{}: `{key}.vendor`: {error}", path.display()))?;
            } else if endpoint.vendor.is_some() {
                return Err(format!(
                    "{}: `{key}.vendor` is forbidden because provider `{}` fixes vendor `{}`",
                    path.display(),
                    spec.id,
                    spec.vendor.unwrap_or("")
                )
                .into());
            }
        }

        for (id, account) in &self.accounts {
            let Some(spec) = provider(account.provider.as_ref()) else {
                return Err(format!(
                    "{}: `account.{id}.provider` names unknown provider `{}`",
                    path.display(),
                    account.provider
                )
                .into());
            };
            if spec.id != "openai-compatible" && account.credential.is_none() {
                return Err(format!(
                    "{}: `account.{id}.credential` is required for provider `{}`",
                    path.display(),
                    account.provider
                )
                .into());
            }
        }

        let endpoints = self.effective_endpoints();
        for (id, route) in &self.routes {
            if route.model.trim().is_empty() {
                return Err(format!("{}: `route.{id}.model` is required", path.display()).into());
            }
            let account = self.accounts.get(&route.account).ok_or_else(|| {
                format!(
                    "{}: `route.{id}.account` references undeclared account `{}`",
                    path.display(),
                    route.account
                )
            })?;
            let spec = provider(account.provider.as_ref()).expect("account provider validated");
            if let Support::Planned(tracking) = spec.support {
                return Err(format!(
                    "{}: `route.{id}` uses provider `{}` which is not yet supported natively; tracked as {tracking}",
                    path.display(), spec.id
                ).into());
            }
            if route.endpoint.is_none() && spec.default_base_url.is_none() {
                return Err(format!(
                    "{}: `route.{id}.endpoint` is required because provider `{}` has no default endpoint",
                    path.display(), account.provider
                )
                .into());
            }
            let endpoint_id = self.route_endpoint(route).ok_or_else(|| {
                format!(
                    "{}: `route.{id}.endpoint` is required because provider `{}` has no default endpoint",
                    path.display(), account.provider
                )
            })?;
            let endpoint = endpoints.get(&endpoint_id).ok_or_else(|| {
                format!(
                    "{}: `route.{id}.endpoint` references undeclared endpoint `{endpoint_id}`",
                    path.display()
                )
            })?;
            if endpoint.provider != account.provider {
                return Err(format!(
                    "{}: `route.{id}.endpoint` provider `{}` does not match account provider `{}`",
                    path.display(),
                    endpoint.provider,
                    account.provider
                )
                .into());
            }
            super::inventory::resolve_model(id, &endpoint_id, &endpoint.vendor, &route.model)
                .map_err(|error| format!("{}: `route.{id}.model`: {error}", path.display()))?;
        }

        for (role, route) in &self.roles {
            if !self.routes.contains_key(route) {
                return Err(format!(
                    "{}: `roles.{role}` references undeclared route `{route}`",
                    path.display()
                )
                .into());
            }
            if !self.allowed_routes().contains(route) {
                return Err(format!(
                    "{}: `roles.{role}` references route `{route}` outside effective `policy.allowed_routes`",
                    path.display()
                ).into());
            }
        }
        Ok(())
    }
}

fn validate_schema(schema: u32, path: &Path) -> CtxResult<()> {
    if schema == 0 {
        return Err(format!("{}: `schema` is required", path.display()).into());
    }
    if schema > NATIVE_SCHEMA {
        return Err(format!(
            "{}: native.toml schema {schema} is newer than this zirv (supports {NATIVE_SCHEMA}); upgrade zirv",
            path.display()
        ).into());
    }
    if schema != NATIVE_SCHEMA {
        return Err(format!(
            "{}: unsupported native.toml schema {schema}",
            path.display()
        )
        .into());
    }
    Ok(())
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RepoConfig {
    schema: u32,
    policy: NativePolicy,
}

fn reject_untrusted_keys(table: &toml::Table, path: &Path) -> CtxResult<()> {
    for (key, value) in table {
        match key.as_str() {
            "schema" => {}
            "policy" => {
                if let Some(policy) = value.as_table() {
                    for policy_key in policy.keys() {
                        if policy_key != "allowed_routes" {
                            return Err(repo_forbidden(path, &format!("policy.{policy_key}")));
                        }
                    }
                }
            }
            "endpoint" | "account" | "route" | "roles" => {
                let dotted = value
                    .as_table()
                    .and_then(|nested| nested.keys().next())
                    .map_or_else(|| key.clone(), |nested| format!("{key}.{nested}"));
                return Err(repo_forbidden(path, &dotted));
            }
            other => return Err(repo_forbidden(path, other)),
        }
    }
    Ok(())
}

fn repo_forbidden(path: &Path, key: &str) -> Box<dyn std::error::Error> {
    format!(
        "{}: `{key}` may not be set by a repository config; set it in ~/.zirv/native.toml",
        path.display()
    )
    .into()
}

#[cfg(test)]
mod tests {
    use super::super::super::testenv::{HomeGuard, repo};
    use super::*;

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn load_is_none_until_the_operator_opts_in() {
        let home = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(home.path());
        let repo = repo();
        assert_eq!(NativeConfig::load(home.path(), repo.path()).unwrap(), None);
    }

    #[test]
    fn newer_schema_and_unknown_keys_are_actionable() {
        let home = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(home.path());
        let repo = repo();
        let path = NativeConfig::operator_path(home.path());
        write(&path, "schema = 2\n");
        let error = NativeConfig::load(home.path(), repo.path()).unwrap_err();
        assert!(
            error.to_string().contains("schema 2 is newer"),
            "got {error}"
        );
        write(&path, "schema = 1\nunknown = true\n");
        let error = NativeConfig::load(home.path(), repo.path()).unwrap_err();
        assert!(error.to_string().contains("unknown"), "got {error}");
        assert!(error.to_string().contains(&path.display().to_string()));
    }

    #[test]
    fn repository_accounts_are_hard_refused_by_dotted_key() {
        let home = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(home.path());
        let repo = repo();
        write(&NativeConfig::operator_path(home.path()), "schema = 1\n");
        write(
            &NativeConfig::repo_path(repo.path()),
            "schema = 1\n[account.work]\nprovider = 'anthropic'\n",
        );
        let error = NativeConfig::load(home.path(), repo.path()).unwrap_err();
        assert!(error.to_string().contains("`account.work`"), "got {error}");
        assert!(error.to_string().contains("~/.zirv/native.toml"));
    }

    #[test]
    fn repository_allowed_routes_intersects_and_never_widens() {
        let home = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(home.path());
        let repo = repo();
        write(
            &NativeConfig::operator_path(home.path()),
            "schema=1\n[account.work]\nprovider='anthropic'\ncredential='env:KEY'\n[route.a]\naccount='work'\nmodel='haiku'\n[route.b]\naccount='work'\nmodel='sonnet'\n[policy]\nallowed_routes=['a']\n",
        );
        write(
            &NativeConfig::repo_path(repo.path()),
            "schema=1\n[policy]\nallowed_routes=['a','b','undeclared']\n",
        );
        let cfg = NativeConfig::load(home.path(), repo.path())
            .unwrap()
            .unwrap();
        assert_eq!(
            cfg.allowed_routes()
                .iter()
                .map(AsRef::as_ref)
                .collect::<Vec<_>>(),
            ["a"]
        );
    }

    #[test]
    fn narrowing_cannot_leave_a_bound_role_on_a_disallowed_route() {
        let home = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(home.path());
        let repo = repo();
        write(
            &NativeConfig::operator_path(home.path()),
            "schema=1\n[account.work]\nprovider='anthropic'\ncredential='env:KEY'\n[route.a]\naccount='work'\nmodel='haiku'\n[route.b]\naccount='work'\nmodel='sonnet'\n[roles]\nworker='b'\n",
        );
        write(
            &NativeConfig::repo_path(repo.path()),
            "schema=1\n[policy]\nallowed_routes=['a']\n",
        );
        let error = NativeConfig::load(home.path(), repo.path()).unwrap_err();
        assert!(error.to_string().contains("roles.worker"), "got {error}");
        assert!(
            error.to_string().contains("outside effective"),
            "got {error}"
        );
    }

    #[test]
    fn planned_provider_route_names_its_roadmap_step() {
        let home = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(home.path());
        let repo = repo();
        write(
            &NativeConfig::operator_path(home.path()),
            "schema=1\n[account.work]\nprovider='google-vertex'\ncredential='env:KEY'\n[route.work]\naccount='work'\nmodel='gemini-2.5-pro'\n",
        );
        let error = NativeConfig::load(home.path(), repo.path()).unwrap_err();
        assert!(error.to_string().contains("N12 (#481)"), "got {error}");
    }

    #[test]
    fn every_route_requires_a_nonempty_model() {
        let home = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(home.path());
        let repo = repo();
        write(
            &NativeConfig::operator_path(home.path()),
            "schema=1\n[endpoint.local]\nprovider='openai-compatible'\nbase_url='http://127.0.0.1:11434'\nvendor='ollama'\n[account.local]\nprovider='openai-compatible'\n[route.local]\naccount='local'\nendpoint='local'\n",
        );
        let error = NativeConfig::load(home.path(), repo.path()).unwrap_err();
        assert!(
            error.to_string().contains("`route.local.model`"),
            "got {error}"
        );
    }
}
