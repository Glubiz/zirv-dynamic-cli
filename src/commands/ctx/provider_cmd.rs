use std::io::{IsTerminal, Read, Write};
use std::path::Path;

use clap::{Args, Subcommand};

use super::config::{EnvLookup, env_from_process};
use super::provider::config::{NATIVE_SCHEMA, NativeConfig};
use super::provider::credential::{CredentialStore, OsStore, refuse_harness_login};
use super::provider::inventory::{Inventory, state_text};
use super::provider::probe::{HttpProbe, Probe};
use super::{CtxResult, state};

#[derive(Debug, Args)]
pub struct ProviderArgs {
    #[command(subcommand)]
    pub command: ProviderVerb,
}

#[derive(Debug, Subcommand)]
pub enum ProviderVerb {
    /// Create a commented ~/.zirv/native.toml template.
    Init,
    /// List native providers, endpoints, accounts, routes and capabilities.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Validate role access, optionally probing provider model-list endpoints.
    Check {
        #[arg(long)]
        live: bool,
        #[arg(long)]
        role: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Manage credentials held by the operating-system protected store.
    Credential(CredentialArgs),
}

#[derive(Debug, Args)]
pub struct CredentialArgs {
    #[command(subcommand)]
    pub command: CredentialVerb,
}

#[derive(Debug, Subcommand)]
pub enum CredentialVerb {
    /// Read a secret from stdin/a hidden prompt and store it for ACCOUNT.
    Set { account: super::provider::AccountId },
}

const TEMPLATE: &str = r#"schema = 1

#[endpoint.local]
#provider = "openai-compatible"
#base_url = "http://127.0.0.1:11434"
#vendor = "ollama"

#[account.work]
#provider = "anthropic"
#credential = "env:ANTHROPIC_API_KEY_WORK"
#billing = "api"
#pool = "work"

#[route.work-sonnet]
#account = "work"
#endpoint = "anthropic"
#model = "claude-sonnet-5"

#[roles]
#orchestrator = "work-sonnet"

#[policy]
#allowed_routes = ["work-sonnet"]
"#;

pub fn run(args: &ProviderArgs, w: &mut dyn Write) -> CtxResult<i32> {
    let home = crate::utils::home_dir()?;
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    let store = OsStore::default();
    let probe = HttpProbe::default();
    run_with(
        args,
        w,
        &home,
        &repo,
        &env,
        &store,
        &probe,
        now_secs(),
        || read_secret_for_store(std::io::stdin().is_terminal()),
    )
}

#[allow(clippy::too_many_arguments)]
fn run_with(
    args: &ProviderArgs,
    w: &mut dyn Write,
    home: &Path,
    repo: &Path,
    env: EnvLookup<'_>,
    store: &dyn CredentialStore,
    probe: &dyn Probe,
    now: u64,
    read_secret_fn: impl FnOnce() -> CtxResult<String>,
) -> CtxResult<i32> {
    if matches!(args.command, ProviderVerb::Init) {
        return init(home, w);
    }
    let Some(cfg) = NativeConfig::load(home, repo)? else {
        writeln!(
            w,
            "native provider routes are not configured -- run `zirv ctx provider init`"
        )?;
        return Ok(0);
    };
    match &args.command {
        ProviderVerb::Init => unreachable!(),
        ProviderVerb::List { json } => {
            let inventory = Inventory::build(&cfg, env, store, now, None);
            print_inventory(&inventory, *json, w)?;
            Ok(0)
        }
        ProviderVerb::Check { live, role, json } => {
            let live_probe = live.then_some(probe);
            let inventory = Inventory::build(&cfg, env, store, now, live_probe);
            print_check(&inventory, role.as_deref(), *json, w)
        }
        ProviderVerb::Credential(CredentialArgs {
            command: CredentialVerb::Set { account },
        }) => {
            let account_cfg = cfg
                .accounts
                .get(account)
                .ok_or_else(|| format!("account `{account}` is not declared in native.toml"))?;
            let reference = account_cfg.credential.as_ref().ok_or_else(|| {
                format!("account `{account}` has no credential ref; set it to store:<item>")
            })?;
            refuse_harness_login(reference)?;
            let item = reference.store_item().ok_or_else(|| {
                format!(
                    "account `{account}` credential is `{reference}`; credential set requires store:<item>"
                )
            })?;
            let secret = read_secret_fn()?;
            if secret.trim().is_empty() {
                return Err("credential is empty".into());
            }
            store.set(item, secret.trim()).map_err(|error| {
                format!("could not set credential {reference} for account `{account}`: {error}")
            })?;
            writeln!(w, "credential stored for account `{account}`")?;
            Ok(0)
        }
    }
}

fn init(home: &Path, w: &mut dyn Write) -> CtxResult<i32> {
    let path = NativeConfig::operator_path(home);
    if path.exists() {
        writeln!(w, "{} already exists", path.display())?;
        return Ok(0);
    }
    if let Some(parent) = path.parent() {
        state::create_private_dir_all(parent)?;
    }
    state::write_private(&path, TEMPLATE)?;
    writeln!(
        w,
        "created {} (native.toml schema {NATIVE_SCHEMA})",
        path.display()
    )?;
    Ok(0)
}

fn print_inventory(inventory: &Inventory, json: bool, w: &mut dyn Write) -> CtxResult<()> {
    if json {
        writeln!(w, "{}", serde_json::to_string_pretty(inventory)?)?;
        return Ok(());
    }
    writeln!(w, "PROVIDER\tSUPPORT\tCONFIGURED")?;
    for provider in &inventory.providers {
        writeln!(
            w,
            "{}\t{:?}\t{}",
            provider.id, provider.support, provider.configured
        )?;
    }
    writeln!(w, "\nENDPOINT\tPROVIDER\tVENDOR\tURL")?;
    for endpoint in &inventory.endpoints {
        writeln!(
            w,
            "{}\t{}\t{}\t{}",
            endpoint.id, endpoint.provider, endpoint.vendor, endpoint.base_url
        )?;
    }
    writeln!(w, "\nPOOL\tACCOUNTS")?;
    for pool in &inventory.pools {
        let accounts = pool
            .accounts
            .iter()
            .map(|account| account.id.as_ref())
            .collect::<Vec<_>>()
            .join(",");
        writeln!(w, "{}\t{accounts}", pool.pool)?;
    }
    writeln!(w, "\nROUTE\tMODEL\tSTATE\tCAPABILITIES\tALLOWED\tPROBLEM")?;
    for route in &inventory.routes {
        writeln!(
            w,
            "{}\t{}/{}\t{}\t{}\t{}\t{}",
            route.route,
            route.model.vendor,
            route.model.id,
            state_text(route.state),
            capability_summary(&route.capabilities),
            route.allowed,
            route.problems.first().map(String::as_str).unwrap_or("")
        )?;
    }
    Ok(())
}

fn capability_summary(capabilities: &super::provider::capability::ModelCapabilities) -> String {
    let fields = [
        ("tools", capabilities.tools),
        ("stream", capabilities.streaming),
        ("vision", capabilities.vision),
        ("structured", capabilities.structured_output),
        ("cache", capabilities.prompt_caching),
        ("reasoning", capabilities.reasoning_controls),
        ("continuation", capabilities.continuation),
    ];
    fields
        .into_iter()
        .filter_map(|(name, capability)| match capability {
            super::provider::capability::Capability::Declared { declared: true } => Some(name),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn print_check(
    inventory: &Inventory,
    role: Option<&str>,
    json: bool,
    w: &mut dyn Write,
) -> CtxResult<i32> {
    let rows: Vec<_> = inventory
        .access
        .iter()
        .filter(|row| role.is_none_or(|role| row.role == role))
        .collect();
    if json {
        writeln!(w, "{}", serde_json::to_string_pretty(&rows)?)?;
    } else {
        writeln!(w, "ROLE\tROUTE\tSTATE\tALLOWED\tPROBLEM")?;
        for row in &rows {
            writeln!(
                w,
                "{}\t{}\t{}\t{}\t{}",
                row.role,
                row.route
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "-".into()),
                row.state_text,
                row.allowed,
                row.problem.as_deref().unwrap_or("")
            )?;
        }
    }
    let failed = rows
        .iter()
        .any(|row| row.route.is_some() && (!row.allowed || row.problem.is_some()));
    if !json {
        writeln!(
            w,
            "verdict: {}{}",
            if failed { "problems found" } else { "ok" },
            if inventory
                .routes
                .iter()
                .all(|route| route.state != super::provider::inventory::RouteState::Validated)
            {
                " (validated is not yet possible in N02)"
            } else {
                ""
            }
        )?;
    }
    Ok(i32::from(failed))
}

fn read_secret(interactive: bool) -> CtxResult<String> {
    if interactive {
        return dialoguer::Password::new()
            .with_prompt("API credential")
            .interact()
            .map_err(Into::into);
    }
    let mut secret = String::new();
    std::io::stdin().read_to_string(&mut secret)?;
    Ok(secret)
}

fn read_secret_for_store(interactive: bool) -> CtxResult<String> {
    #[cfg(target_os = "macos")]
    if interactive {
        // OsStore deliberately invokes `security ... -w` without a value on
        // an interactive Mac, so Apple's tool owns the hidden prompt. This
        // non-secret marker satisfies the shared validation path and is
        // ignored by that argv builder; it is never stored or printed.
        return Ok("security-prompts-interactively".into());
    }
    read_secret(interactive)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::super::provider::credential::FakeStore;
    use super::super::provider::probe::{FakeProbe, ProbeResult};
    use super::super::testenv::{HomeGuard, repo};
    use super::*;

    fn write_config(home: &Path, text: &str) {
        let path = NativeConfig::operator_path(home);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn invoke(
        home: &Path,
        repo: &Path,
        command: ProviderVerb,
        env: EnvLookup<'_>,
    ) -> (i32, String) {
        let mut output = Vec::new();
        let code = run_with(
            &ProviderArgs { command },
            &mut output,
            home,
            repo,
            env,
            &FakeStore::default(),
            &FakeProbe::new(ProbeResult::Unreachable("offline".into())),
            0,
            || Ok("input-secret".into()),
        )
        .unwrap();
        (code, String::from_utf8(output).unwrap())
    }

    #[test]
    fn init_is_create_only_and_absent_list_is_an_opt_in_message() {
        let home = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(home.path());
        let repo = repo();
        let (_, absent) = invoke(
            home.path(),
            repo.path(),
            ProviderVerb::List { json: false },
            &|_| None,
        );
        assert!(absent.contains("provider init"));
        let (_, created) = invoke(home.path(), repo.path(), ProviderVerb::Init, &|_| None);
        assert!(created.contains("created"));
        let path = NativeConfig::operator_path(home.path());
        let original = std::fs::read_to_string(&path).unwrap();
        let (_, exists) = invoke(home.path(), repo.path(), ProviderVerb::Init, &|_| None);
        assert!(exists.contains("already exists"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    }

    #[test]
    fn list_and_check_json_never_serialize_resolved_secrets() {
        let home = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(home.path());
        let repo = repo();
        write_config(
            home.path(),
            "schema=1\n[account.work]\nprovider='anthropic'\ncredential='env:WORK_KEY'\n[route.work]\naccount='work'\nmodel='sonnet'\n[roles]\nworker='work'\n",
        );
        let env = |name: &str| (name == "WORK_KEY").then(|| "sk-test-secret-123".into());
        for command in [
            ProviderVerb::List { json: true },
            ProviderVerb::Check {
                live: false,
                role: None,
                json: true,
            },
        ] {
            let (_, output) = invoke(home.path(), repo.path(), command, &env);
            assert!(!output.contains("sk-test-secret-123"), "leaked: {output}");
        }
    }

    #[test]
    fn check_returns_one_for_a_configured_role_problem() {
        let home = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(home.path());
        let repo = repo();
        write_config(
            home.path(),
            "schema=1\n[account.work]\nprovider='anthropic'\ncredential='env:MISSING_KEY'\n[route.work]\naccount='work'\nmodel='sonnet'\n[roles]\nworker='work'\n",
        );
        let (code, output) = invoke(
            home.path(),
            repo.path(),
            ProviderVerb::Check {
                live: false,
                role: Some("worker".into()),
                json: false,
            },
            &|_| None,
        );
        assert_eq!(code, 1);
        assert!(
            output.ends_with("verdict: problems found (validated is not yet possible in N02)\n")
        );
    }

    #[test]
    fn credential_set_reads_the_secret_out_of_band_and_writes_only_store_refs() {
        let home = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(home.path());
        let repo = repo();
        write_config(
            home.path(),
            "schema=1\n[account.work]\nprovider='anthropic'\ncredential='store:work'\n",
        );
        let store = FakeStore::default();
        let mut output = Vec::new();
        let code = run_with(
            &ProviderArgs {
                command: ProviderVerb::Credential(CredentialArgs {
                    command: CredentialVerb::Set {
                        account: "work".parse().unwrap(),
                    },
                }),
            },
            &mut output,
            home.path(),
            repo.path(),
            &|_| None,
            &store,
            &FakeProbe::new(ProbeResult::Unreachable("offline".into())),
            0,
            || Ok("sk-test-secret-123".into()),
        )
        .unwrap();
        assert_eq!(code, 0);
        assert_eq!(
            store.get("work").unwrap().as_deref(),
            Some("sk-test-secret-123")
        );
        assert!(
            !String::from_utf8(output)
                .unwrap()
                .contains("sk-test-secret-123")
        );
    }

    #[test]
    fn credential_set_refuses_harness_login_stores_before_reading_or_writing() {
        let home = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(home.path());
        let repo = repo();
        write_config(
            home.path(),
            "schema=1\n[account.work]\nprovider='anthropic'\ncredential='store:Claude Code-credentials'\n",
        );
        let store = FakeStore::default();
        let mut output = Vec::new();
        let error = run_with(
            &ProviderArgs {
                command: ProviderVerb::Credential(CredentialArgs {
                    command: CredentialVerb::Set {
                        account: "work".parse().unwrap(),
                    },
                }),
            },
            &mut output,
            home.path(),
            repo.path(),
            &|_| None,
            &store,
            &FakeProbe::new(ProbeResult::Unreachable("offline".into())),
            0,
            || panic!("refusal must happen before reading a secret"),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("harness login tokens are subscription entitlements"),
            "got {error}"
        );
        assert_eq!(store.get("Claude Code-credentials").unwrap(), None);
    }
}
