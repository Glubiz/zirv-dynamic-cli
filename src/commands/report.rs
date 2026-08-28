//! `zirv report bug|feature` files feedback against Zirv's own GitHub
//! repository without sending the operator through a browser workflow.
//!
//! The destination and endpoint are fixed product constants, not repository
//! configuration. Credentials resolve from the installed GitHub CLI first,
//! then `GH_TOKEN`, `GITHUB_TOKEN`, and finally operator-owned
//! `~/.zirv/.settings.toml`; a checkout's `.zirv/.settings.toml` is never
//! consulted. Tests inject both credential lookup and transport, so the
//! suite never touches a real credential or the network.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};

type ReportResult<T> = Result<T, Box<dyn std::error::Error>>;
type EnvLookup<'a> = &'a dyn Fn(&str) -> Option<String>;

const GITHUB_API_URL: &str = "https://api.github.com/repos/Glubiz/zirv-dynamic-cli/issues";
const GITHUB_REPOSITORY: &str = "Glubiz/zirv-dynamic-cli";
const HTTP_TIMEOUT_SECS: u64 = 15;
const HTTP_CONNECT_TIMEOUT_SECS: u64 = 5;

#[derive(Debug, Parser)]
#[command(
    name = "zirv report",
    about = "File a Zirv bug or feature request on GitHub.",
    disable_help_subcommand = true
)]
pub struct ReportCli {
    #[command(subcommand)]
    verb: ReportVerb,
}

#[derive(Debug, Subcommand)]
enum ReportVerb {
    /// Report something that is broken or behaving incorrectly.
    Bug(ReportArgs),
    /// Request a new Zirv capability or behavior.
    Feature(ReportArgs),
}

#[derive(Debug, Args, Clone)]
pub struct ReportArgs {
    /// Short issue title.
    title: String,
    /// Markdown issue body supplied inline.
    #[arg(long, conflicts_with = "body_file")]
    body: Option<String>,
    /// Read the Markdown issue body from this file.
    #[arg(long, value_name = "PATH", conflicts_with = "body")]
    body_file: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct IssueRequest {
    title: String,
    body: String,
    labels: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct IssueResponse {
    html_url: String,
}

static HTTP_AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();

fn http_agent() -> &'static ureq::Agent {
    HTTP_AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(HTTP_TIMEOUT_SECS)))
            .timeout_connect(Some(Duration::from_secs(HTTP_CONNECT_TIMEOUT_SECS)))
            .build()
            .into()
    })
}

fn create_issue(token: &str, request: &IssueRequest) -> ReportResult<String> {
    let payload = serde_json::to_string(request)?;
    let mut response = http_agent()
        .post(GITHUB_API_URL)
        .header("Accept", "application/vnd.github+json")
        .header("Content-Type", "application/json")
        .header("Authorization", &format!("Bearer {token}"))
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", &format!("zirv/{}", env!("CARGO_PKG_VERSION")))
        .send(payload)
        .map_err(|error| format!("GitHub rejected the issue request: {error}"))?;
    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|error| format!("GitHub returned an unreadable issue response: {error}"))?;
    let created: IssueResponse = serde_json::from_str(&body)
        .map_err(|error| format!("GitHub returned an unreadable issue response: {error}"))?;
    if created.html_url.trim().is_empty() {
        return Err("GitHub created an issue but returned no issue URL".into());
    }
    Ok(created.html_url)
}

/// Reads the token GitHub CLI has selected for github.com. The command and
/// every argument are fixed; no shell parses them and no repository value
/// can alter the invocation.
fn gh_auth_token() -> Option<String> {
    let output = Command::new("gh")
        .args(["auth", "token", "--hostname", "github.com"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty())
}

fn clean_token(token: Option<String>) -> Option<String> {
    token
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty())
}

fn resolve_token(
    home: Option<&Path>,
    env: EnvLookup<'_>,
    cli_token: &dyn Fn() -> Option<String>,
) -> ReportResult<String> {
    if let Some(token) = clean_token(cli_token()) {
        return Ok(token);
    }
    if let Some(token) = clean_token(env("GH_TOKEN")) {
        return Ok(token);
    }
    if let Some(token) = clean_token(env("GITHUB_TOKEN")) {
        return Ok(token);
    }
    if let Some(home) = home
        && let Some(token) = crate::settings::operator_github_token(home)?
    {
        return Ok(token);
    }
    Err(format!(
        "no GitHub credentials found for {GITHUB_REPOSITORY}; run `gh auth login --hostname \
         github.com`, set GH_TOKEN or GITHUB_TOKEN, or add `[github] token = \"...\"` to \
         ~/.zirv/.settings.toml"
    )
    .into())
}

fn safe_context_value(value: Option<String>) -> Option<String> {
    let value = value?
        .replace(['\r', '\n'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let value = crate::utils::truncate_bytes(value, Some(128));
    (!value.is_empty()).then_some(value)
}

fn environment_context(env: EnvLookup<'_>) -> String {
    let mut lines = vec![
        "---".to_string(),
        "Environment".to_string(),
        format!("- Zirv: {}", env!("CARGO_PKG_VERSION")),
        format!("- OS: {}", std::env::consts::OS),
        format!("- Architecture: {}", std::env::consts::ARCH),
    ];
    if let Some(harness) = safe_context_value(env(crate::commands::ctx::adapters::AGENT_ENV)) {
        lines.push(format!("- Harness: {harness}"));
    }
    if let Some(model) = safe_context_value(env(crate::commands::ctx::adapters::SEAT_MODEL_ENV)) {
        lines.push(format!("- Model: {model}"));
    }
    lines.join("\n")
}

fn issue_body(body: Option<String>, env: EnvLookup<'_>) -> String {
    let body = body.unwrap_or_default();
    let body = body.trim_end();
    if body.is_empty() {
        environment_context(env)
    } else {
        format!("{body}\n\n{}", environment_context(env))
    }
}

fn supplied_body(args: &ReportArgs) -> ReportResult<Option<String>> {
    match (&args.body, &args.body_file) {
        (Some(body), None) => Ok(Some(body.clone())),
        (None, Some(path)) => std::fs::read_to_string(path).map(Some).map_err(|error| {
            format!("could not read report body {}: {error}", path.display()).into()
        }),
        (None, None) => Ok(None),
        (Some(_), Some(_)) => Err("pass only one of --body or --body-file".into()),
    }
}

fn request_for(verb: &ReportVerb, env: EnvLookup<'_>) -> ReportResult<IssueRequest> {
    let (args, label) = match verb {
        ReportVerb::Bug(args) => (args, "bug"),
        ReportVerb::Feature(args) => (args, "enhancement"),
    };
    let title = args.title.trim();
    if title.is_empty() {
        return Err("report title must not be empty".into());
    }
    Ok(IssueRequest {
        title: title.to_string(),
        body: issue_body(supplied_body(args)?, env),
        labels: vec![label.to_string()],
    })
}

fn run_with<W: Write>(
    cli: &ReportCli,
    writer: &mut W,
    home: Option<&Path>,
    env: EnvLookup<'_>,
    cli_token: &dyn Fn() -> Option<String>,
    issue_creator: &dyn Fn(&str, &IssueRequest) -> ReportResult<String>,
) -> ReportResult<i32> {
    let request = request_for(&cli.verb, env)?;
    let token = resolve_token(home, env, cli_token)?;
    let url = issue_creator(&token, &request)?;
    writeln!(writer, "{url}")?;
    Ok(0)
}

/// `args[0]` is the literal `report` command as it appeared in argv. It is
/// discarded in favor of a stable synthetic program name, so raw dispatch
/// remains case-insensitive while clap's usage text stays deterministic.
pub fn dispatch(args: &[String]) -> i32 {
    let argv = std::iter::once("zirv report".to_string()).chain(args.iter().skip(1).cloned());
    let cli = match ReportCli::try_parse_from(argv) {
        Ok(cli) => cli,
        Err(error) => {
            let _ = error.print();
            return match error.kind() {
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion => 0,
                _ => 2,
            };
        }
    };
    let home = crate::utils::home_dir().ok();
    let env = |key: &str| std::env::var(key).ok();
    match run_with(
        &cli,
        &mut std::io::stdout(),
        home.as_deref(),
        &env,
        &gh_auth_token,
        &create_issue,
    ) {
        Ok(code) => code,
        Err(error) => {
            crate::output::error(error);
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    fn cli(args: &[&str]) -> ReportCli {
        ReportCli::try_parse_from(args).expect("parse")
    }

    fn env_map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn bug_report_appends_environment_and_uses_bug_label() {
        let vars = env_map(&[
            (crate::commands::ctx::adapters::AGENT_ENV, "claude"),
            (crate::commands::ctx::adapters::SEAT_MODEL_ENV, "sonnet"),
            (
                crate::commands::ctx::adapters::SESSION_ENV,
                "secret-session-id",
            ),
        ]);
        let parsed = cli(&["zirv report", "bug", "Something broke", "--body", "Details"]);
        let request = request_for(&parsed.verb, &|key| vars.get(key).cloned()).expect("request");

        assert_eq!(request.title, "Something broke");
        assert_eq!(request.labels, ["bug"]);
        assert!(request.body.starts_with("Details\n\n---\nEnvironment"));
        assert!(
            request
                .body
                .contains(&format!("- Zirv: {}", env!("CARGO_PKG_VERSION")))
        );
        assert!(
            request
                .body
                .contains(&format!("- OS: {}", std::env::consts::OS))
        );
        assert!(
            request
                .body
                .contains(&format!("- Architecture: {}", std::env::consts::ARCH))
        );
        assert!(request.body.contains("- Harness: claude"));
        assert!(request.body.contains("- Model: sonnet"));
        assert!(!request.body.contains("secret-session-id"));
    }

    #[test]
    fn feature_report_reads_body_file_and_uses_enhancement_label() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("body.md");
        std::fs::write(&path, "Requested behavior\n").expect("write");
        let parsed = cli(&[
            "zirv report",
            "feature",
            "Add a thing",
            "--body-file",
            path.to_str().expect("utf8"),
        ]);
        let request = request_for(&parsed.verb, &|_| None).expect("request");

        assert_eq!(request.labels, ["enhancement"]);
        assert!(request.body.starts_with("Requested behavior\n\n---"));
    }

    #[test]
    fn body_sources_are_mutually_exclusive() {
        let error = ReportCli::try_parse_from([
            "zirv report",
            "bug",
            "title",
            "--body",
            "inline",
            "--body-file",
            "body.md",
        ])
        .expect_err("conflict");
        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn credential_order_is_gh_then_environment_then_operator_settings() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/.settings.toml"),
            "[github]\ntoken = \"settings-token\"\n",
        )
        .expect("settings");
        let vars = env_map(&[("GH_TOKEN", "gh-env"), ("GITHUB_TOKEN", "github-env")]);

        assert_eq!(
            resolve_token(Some(home.path()), &|key| vars.get(key).cloned(), &|| Some(
                "gh-cli".to_string()
            ))
            .expect("token"),
            "gh-cli"
        );
        assert_eq!(
            resolve_token(Some(home.path()), &|key| vars.get(key).cloned(), &|| None)
                .expect("token"),
            "gh-env"
        );
        assert_eq!(
            resolve_token(
                Some(home.path()),
                &|key| (key == "GITHUB_TOKEN").then(|| "github-env".to_string()),
                &|| None
            )
            .expect("token"),
            "github-env"
        );
        assert_eq!(
            resolve_token(Some(home.path()), &|_| None, &|| None).expect("token"),
            "settings-token"
        );
    }

    #[test]
    fn missing_credentials_error_names_every_setup_path() {
        let home = tempfile::tempdir().expect("tempdir");
        let error = resolve_token(Some(home.path()), &|_| None, &|| None)
            .expect_err("credentials must be required")
            .to_string();
        for expected in ["gh auth login", "GH_TOKEN", "GITHUB_TOKEN", "[github]"] {
            assert!(error.contains(expected), "missing {expected}: {error}");
        }
    }

    #[test]
    fn success_prints_only_the_created_issue_url() {
        let parsed = cli(&["zirv report", "bug", "Broken"]);
        let captured = RefCell::new(None);
        let mut output = Vec::new();
        let code = run_with(
            &parsed,
            &mut output,
            None,
            &|_| None,
            &|| Some("token".to_string()),
            &|token, request| {
                assert_eq!(token, "token");
                captured.replace(Some(request.clone()));
                Ok("https://github.com/Glubiz/zirv-dynamic-cli/issues/999".to_string())
            },
        )
        .expect("run");

        assert_eq!(code, 0);
        assert_eq!(
            String::from_utf8(output).expect("utf8"),
            "https://github.com/Glubiz/zirv-dynamic-cli/issues/999\n"
        );
        assert_eq!(captured.borrow().as_ref().expect("request").title, "Broken");
    }

    #[test]
    fn context_values_are_single_line_and_bounded() {
        let long = format!("claude\r\n{}", "x".repeat(300));
        let body = environment_context(&|key| {
            (key == crate::commands::ctx::adapters::AGENT_ENV).then(|| long.clone())
        });
        let harness = body
            .lines()
            .find(|line| line.starts_with("- Harness:"))
            .expect("harness");
        assert!(!harness.contains('\r'));
        assert!(harness.len() <= "- Harness: ".len() + 128);
    }

    #[test]
    fn fixed_destination_is_the_zirv_repository() {
        assert_eq!(GITHUB_REPOSITORY, "Glubiz/zirv-dynamic-cli");
        assert_eq!(
            GITHUB_API_URL,
            "https://api.github.com/repos/Glubiz/zirv-dynamic-cli/issues"
        );
    }
}
