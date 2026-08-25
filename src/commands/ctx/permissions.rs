//! `zirv ctx permissions audit` (issue #132): a transcript-backed audit of
//! recent escalated/denied command-permission requests, grouped by a
//! normalized command family, with a reusability verdict ("will the saved
//! approval match the next equivalent command?") and a recommendation.
//!
//! Two extractors, agent-neutral output:
//! - **codex**: `response_item` / `custom_tool_call` records named `exec`
//!   whose payload carries `sandbox_permissions: "require_escalated"` (the
//!   shape documented in issue #132's own reproduction section, over
//!   `~/.codex/sessions/**/*.jsonl`).
//! - **claude**: a headless (`--permission-mode dontAsk`) launch's
//!   `PreToolUse` denial, grounded in this machine's own real transcripts
//!   under `~/.claude/projects/<slug>/*.jsonl` -- an assistant `tool_use`
//!   entry correlated by `id`/`tool_use_id` with a later user `tool_result`
//!   entry carrying `"is_error":true` and text starting "Permission to use
//!   <Tool> has been denied because Claude Code is running in don't ask
//!   mode." (verified by inspecting real session files, not guessed).
//!
//! Both extractors reuse `safety.rs`'s own wrapper-normalization helpers
//! (`pipeline_stages`, `unwrap_env_prefix`, `unwrap_shell_wrapper`,
//! `strip_program_dir`, `sql_program_name`) so a `/bin/zsh -lc '...'` or
//! `env -u FOO zirv ...` wrapper collapses to the same family as the bare
//! command underneath it -- the exact normalization gap issue #132 names.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;

use clap::ValueEnum;
use serde::Serialize;
use serde_json::Value;

use super::CtxResult;
use super::safety::{
    collapse_whitespace, pipeline_stages, sql_program_name, strip_program_dir, unwrap_env_prefix,
    unwrap_shell_wrapper,
};

/// Which agent's transcripts to audit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AuditAgent {
    Codex,
    Claude,
}

impl AuditAgent {
    pub fn label(self) -> &'static str {
        match self {
            AuditAgent::Codex => "codex",
            AuditAgent::Claude => "claude",
        }
    }
}

/// One escalated/denied permission request extracted from one transcript.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PermissionRequest {
    pub session: String,
    /// The raw command/tool invocation as it appeared in the transcript.
    pub raw: String,
    /// The normalized command family this request is grouped under.
    pub family: String,
    /// Why the harness required a human/policy decision.
    pub cause: String,
    /// What happened to the request: `"escalated"` (codex; the transcript
    /// does not itself record the operator's answer) or `"denied"` (claude,
    /// a headless `dontAsk` refusal).
    pub result: String,
    /// Whether a saved approval for this exact family would plausibly match
    /// the NEXT equivalent invocation, or whether it would collapse to a
    /// one-off (a long literal payload) or a downstream-only capability (a
    /// pipe into `jq`/`grep`/`awk`/`sed` whose own argument is what varies).
    pub reusable: bool,
}

/// One command family's aggregated requests, ready to render or recommend
/// a policy change from.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FamilyGroup {
    pub family: String,
    pub count: usize,
    /// Reusable only when every member request in this family is reusable.
    pub reusable: bool,
    pub sample: String,
    pub cause: String,
    pub recommendation: String,
}

/// The whole audit, agent-neutral.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AuditReport {
    pub agent: &'static str,
    pub sessions_scanned: usize,
    pub total_requests: usize,
    pub groups: Vec<FamilyGroup>,
}

/// Above this many total requests, `optimize`'s friction pass surfaces the
/// audit as a finding (issue #132's "surface the same summary in
/// setup/optimization output when approval noise exceeds a documented
/// threshold"). Chosen well below the 33-in-one-session count the issue's
/// own production evidence reports, so the finding fires long before a
/// session reaches that volume.
pub const NOISE_FINDING_THRESHOLD: usize = 5;

// ---------------------------------------------------------------------
// Family normalization + reusability, shared by both extractors
// ---------------------------------------------------------------------

/// Depth (program + how many leading non-flag tokens) that names a stable
/// family for a multi-word CLI. Everything else collapses to just its
/// program name -- `curl https://a` and `curl https://b` are one family.
fn family_depth(program: &str) -> usize {
    match program {
        "zirv" => 3,
        "git" | "gh" | "cargo" | "npm" | "docker" | "kubectl" | "npx" | "pnpm" | "yarn" => 2,
        _ => 1,
    }
}

/// Peels `env`/shell-wrapper layers off `segment` (bounded to 4 layers,
/// matching `safety.rs`'s own structural-candidate depth), returning the
/// innermost command text -- `env -u FOO zirv ...` or `/bin/zsh -lc '...'`
/// collapse to the wrapped command underneath. Shared by [`family_of`] (the
/// leading command) and [`is_reusable`] (2026-08-25 review: a decorated
/// pipe target like `... | env NO_COLOR=1 jq .` must still be recognised as
/// `jq`, not missed because the filter check only looked at the bare last
/// stage).
fn unwrap_wrappers(segment: &str) -> String {
    let mut current = segment.to_string();
    for _ in 0..4 {
        if let Some(inner) = unwrap_env_prefix(&current) {
            current = inner;
            continue;
        }
        if let Some(inner) = unwrap_shell_wrapper(&current) {
            current = inner;
            continue;
        }
        break;
    }
    current
}

/// Normalizes a raw command/tool invocation to a stable family key: unwraps
/// `env`/shell wrappers, drops to the first pipeline stage (the real
/// command; a trailing `| jq ...` is a downstream formatting stage, not a
/// different family), then keeps the program name plus as many leading
/// non-flag tokens as [`family_depth`] says are part of this program's own
/// stable invocation shape (`gh issue`, `zirv setup apply`, `cargo`).
pub(crate) fn family_of(raw: &str) -> String {
    let first_stage = pipeline_stages(raw).into_iter().next().unwrap_or_default();
    let current = unwrap_wrappers(&first_stage);
    let collapsed = collapse_whitespace(&strip_program_dir(&current));
    let tokens: Vec<&str> = collapsed.split(' ').filter(|t| !t.is_empty()).collect();
    let Some(first) = tokens.first() else {
        return "(empty)".to_string();
    };
    let program = sql_program_name(first);
    let depth = family_depth(&program);
    let mut parts = vec![program];
    for token in tokens.iter().skip(1).take(depth.saturating_sub(1)) {
        if token.starts_with('-') {
            break;
        }
        parts.push((*token).to_string());
    }
    parts.join(" ")
}

/// The longest single- or double-quoted span in `raw`, by character count
/// (quotes excluded). A long quoted span is the signature of a one-off
/// literal payload -- an issue/PR body, a commit message -- that will never
/// recur verbatim, so a saved approval keyed on the exact command text
/// cannot match the next equivalent invocation.
fn longest_quoted_span(raw: &str) -> usize {
    let mut longest = 0usize;
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '"' && c != '\'' {
            continue;
        }
        let quote = c;
        let mut len = 0usize;
        let mut escaped = false;
        for inner in chars.by_ref() {
            if escaped {
                escaped = false;
                len += 1;
                continue;
            }
            if inner == '\\' {
                escaped = true;
                continue;
            }
            if inner == quote {
                break;
            }
            len += 1;
        }
        longest = longest.max(len);
    }
    longest
}

/// Above this many characters, a quoted span reads as a one-off literal
/// payload rather than a short flag value (a branch name, a query string).
const LONG_LITERAL_THRESHOLD: usize = 80;

/// Whether a saved approval for `raw` would plausibly match the next
/// equivalent invocation: false when the command embeds a long literal
/// payload, or pipes into a text filter whose own argument is the varying
/// part of the command (issue #132: "piped read-only API queries produced
/// approvals for downstream `jq` expressions instead of one reusable
/// read-only API capability").
pub(crate) fn is_reusable(raw: &str) -> bool {
    if raw.trim().is_empty() {
        return true;
    }
    if longest_quoted_span(raw) > LONG_LITERAL_THRESHOLD {
        return false;
    }
    let stages = pipeline_stages(raw);
    if stages.len() > 1
        && let Some(last) = stages.last()
    {
        // 2026-08-25 review: unwrap the last stage's own env/shell wrapper
        // before matching the filter program name, so `| env NO_COLOR=1 jq
        // .` is still recognised as a `jq` pipe target and not missed
        // because the wrapper sat in front of it.
        let unwrapped = unwrap_wrappers(last);
        let collapsed = collapse_whitespace(&strip_program_dir(&unwrapped));
        let first = collapsed.split(' ').next().unwrap_or("");
        if matches!(
            sql_program_name(first).as_str(),
            "jq" | "grep" | "awk" | "sed"
        ) {
            return false;
        }
    }
    true
}

/// Command families issue #132 requires stay prompting no matter how
/// reusable the saved approval would otherwise be: "keep global writes,
/// destructive operations, secrets, and publish/release actions prompting
/// unless explicitly enabled by the operator." `git push --force origin
/// main` is a perfectly REUSABLE family by [`is_reusable`]'s own definition
/// (no long literal, no filter pipe) -- that is exactly why reusability
/// alone cannot gate the recommendation; a protected family must never be
/// recommended a standing allow regardless.
///
/// `sample` (not just `family`) is what a caller must classify against:
/// `family_of` strips flags, so `git push --force origin main` and `git
/// push origin feature` both normalize to the family `"git push"` --
/// distinguishing them needs the actual command text, which is why this
/// function takes both.
///
/// Pure: token/substring matching only, no fs/clock/env, the same
/// discipline `safety.rs`'s own classifiers hold to. This is a
/// recommendation heuristic for `zirv ctx permissions audit`'s report, not
/// a hard safety gate -- `safety.rs`'s `SHIPPED_POSTURE_DENY`/`_ASK` remain
/// the actual enforcement boundary a launch is projected against.
pub(crate) fn is_protected_family(family: &str, sample: &str) -> bool {
    let tokens: Vec<String> = sample
        .split(|c: char| c.is_whitespace() || matches!(c, '\'' | '"' | '`'))
        .filter(|t| !t.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    let has = |word: &str| tokens.iter().any(|t| t == word);
    let has_bundled_short_force = tokens.iter().any(|t| {
        t.starts_with('-')
            && !t.starts_with("--")
            && t.len() > 1
            && t[1..].chars().all(|c| c.is_ascii_alphabetic())
            && t.contains('f')
    });
    let lower_sample = sample.to_ascii_lowercase();
    let writes_into_a_global_install_path = matches!(
        family,
        "cp" | "copy" | "xcopy" | "copy-item" | "mv" | "move" | "install"
    ) && (lower_sample.contains("program files")
        || lower_sample.contains("/usr/local/")
        || lower_sample.contains("/usr/bin/")
        || lower_sample.contains("/usr/sbin/")
        || lower_sample.contains("c:\\windows\\"));

    let family_protected = match family {
        // Destructive git: recoverable in principle (a reflog, a rebuild)
        // but exactly the family issue #132 names as one that must keep
        // prompting rather than being silently granted a standing allow.
        "git push" => {
            has("--force")
                || has("--force-with-lease")
                || has("-f")
                || has("--delete")
                || has("-d")
        }
        "git reset" => has("--hard"),
        "git clean" => has("--force") || has_bundled_short_force,
        "git branch" => has("-d") || has("--delete"),
        // History rewrites: always protected, no flag needed.
        "git rebase" | "git filter-branch" => true,
        // A tag alone is harmless; a tag pushed to a remote is a release.
        "git tag" => has("push"),
        // Publish/release: irreversible by nature.
        "cargo publish" | "npm publish" | "gh release" | "cargo install" => true,
        // Credential/secret surfaces.
        "gh auth" | "git credential" | "ssh-keygen" | "security" => true,
        // Global binary/config installs.
        "choco" | "winget" => true,
        "npm install" | "npm" => has("-g") || has("--global"),
        _ => false,
    };

    family_protected
        || writes_into_a_global_install_path
        || has("token")
        || has("keychain")
        || has("keyring")
}

fn recommendation_for(family: &str, protected: bool, reusable: bool) -> String {
    if protected {
        return format!(
            "'{family}' is a protected family: stays prompting unless the operator explicitly \
             enables it (destructive git, a global binary/config install, a credential/secret \
             command, or a publish/release action -- issue #132). Do not grant a standing allow, \
             regardless of reusability."
        );
    }
    if reusable {
        format!(
            "Grant '{family}' a standing allow in the operator's [safety]/[policy] \
             layer -- every observed invocation normalizes to this one family."
        )
    } else {
        format!(
            "Approve the '{family}' CAPABILITY once, not the literal command: the \
             saved approval must normalize away the varying payload (a long \
             quoted body, or a downstream filter expression) or it will keep \
             re-prompting on the next equivalent invocation."
        )
    }
}

// ---------------------------------------------------------------------
// Codex extractor
// ---------------------------------------------------------------------

/// Whether `payload` (a `custom_tool_call` record) requested escalated
/// sandbox permissions, either directly or nested inside a JSON-encoded
/// `input` string.
fn escalation_requested(payload: &Value) -> bool {
    if payload.get("sandbox_permissions").and_then(Value::as_str) == Some("require_escalated") {
        return true;
    }
    if let Some(input) = payload.get("input").and_then(Value::as_str)
        && let Ok(inner) = serde_json::from_str::<Value>(input)
        && inner.get("sandbox_permissions").and_then(Value::as_str) == Some("require_escalated")
    {
        return true;
    }
    false
}

fn command_string(node: &Value) -> Option<String> {
    if let Some(s) = node.get("command").and_then(Value::as_str) {
        return Some(s.to_string());
    }
    if let Some(arr) = node.get("command").and_then(Value::as_array) {
        let parts: Vec<&str> = arr.iter().filter_map(Value::as_str).collect();
        if !parts.is_empty() {
            return Some(parts.join(" "));
        }
    }
    None
}

fn codex_command_of(payload: &Value) -> Option<String> {
    if let Some(command) = command_string(payload) {
        return Some(command);
    }
    let input = payload.get("input").and_then(Value::as_str)?;
    let inner: Value = serde_json::from_str(input).ok()?;
    command_string(&inner)
}

/// Extracts every `require_escalated` exec request from one codex rollout
/// JSONL transcript (see this module's own doc comment for the shape).
pub fn extract_codex_requests(text: &str, session: &str) -> Vec<PermissionRequest> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) != Some("response_item") {
            continue;
        }
        let Some(payload) = v.get("payload") else {
            continue;
        };
        if payload.get("type").and_then(Value::as_str) != Some("custom_tool_call") {
            continue;
        }
        if payload.get("name").and_then(Value::as_str) != Some("exec") {
            continue;
        }
        if !escalation_requested(payload) {
            continue;
        }
        let Some(command) = codex_command_of(payload) else {
            continue;
        };
        let family = family_of(&command);
        let reusable = is_reusable(&command);
        out.push(PermissionRequest {
            session: session.to_string(),
            raw: command,
            family,
            cause: "sandbox_permissions: require_escalated".to_string(),
            result: "escalated".to_string(),
            reusable,
        });
    }
    out
}

// ---------------------------------------------------------------------
// Claude extractor
// ---------------------------------------------------------------------

#[derive(Default, Clone)]
struct ClaudeToolUse {
    name: String,
    command: Option<String>,
}

fn tool_result_text(entry: &Value) -> String {
    match entry.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|t| t.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// Extracts every headless `dontAsk` permission denial from one claude
/// transcript JSONL file (see this module's own doc comment for the shape,
/// grounded in real session files on this machine).
pub fn extract_claude_requests(text: &str, session: &str) -> Vec<PermissionRequest> {
    let mut uses: HashMap<String, ClaudeToolUse> = HashMap::new();
    let mut out = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(content) = v.pointer("/message/content").and_then(Value::as_array) else {
            continue;
        };
        for entry in content {
            match entry.get("type").and_then(Value::as_str) {
                Some("tool_use") => {
                    let Some(id) = entry.get("id").and_then(Value::as_str) else {
                        continue;
                    };
                    let name = entry
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let command = entry
                        .pointer("/input/command")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .or_else(|| {
                            entry
                                .pointer("/input/file_path")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        });
                    uses.insert(id.to_string(), ClaudeToolUse { name, command });
                }
                Some("tool_result") => {
                    if entry.get("is_error").and_then(Value::as_bool) != Some(true) {
                        continue;
                    }
                    let text_val = tool_result_text(entry);
                    if !text_val.contains("Permission to use") {
                        continue;
                    }
                    let Some(tool_use_id) = entry.get("tool_use_id").and_then(Value::as_str) else {
                        continue;
                    };
                    let Some(used) = uses.get(tool_use_id) else {
                        continue;
                    };
                    let raw = used.command.clone().unwrap_or_else(|| used.name.clone());
                    let is_bash = used.name.eq_ignore_ascii_case("Bash");
                    let family = if is_bash {
                        family_of(&raw)
                    } else {
                        used.name.clone()
                    };
                    out.push(PermissionRequest {
                        session: session.to_string(),
                        raw,
                        family,
                        cause: "permission-mode dontAsk denial".to_string(),
                        result: "denied".to_string(),
                        reusable: !is_bash
                            || is_reusable(&used.command.clone().unwrap_or_default()),
                    });
                }
                _ => {}
            }
        }
    }
    out
}

// ---------------------------------------------------------------------
// Grouping + report
// ---------------------------------------------------------------------

pub(crate) fn group_requests(requests: &[PermissionRequest]) -> Vec<FamilyGroup> {
    let mut order: Vec<String> = Vec::new();
    let mut by_family: HashMap<String, Vec<&PermissionRequest>> = HashMap::new();
    for request in requests {
        by_family
            .entry(request.family.clone())
            .or_insert_with(|| {
                order.push(request.family.clone());
                Vec::new()
            })
            .push(request);
    }
    let mut groups: Vec<FamilyGroup> = order
        .into_iter()
        .map(|family| {
            let members = &by_family[&family];
            let reusable = members.iter().all(|m| m.reusable);
            // Any member's raw command classifying as protected protects
            // the whole group -- a family that is SOMETIMES a destructive
            // shape (`git push origin x` alongside `git push --force origin
            // main`) must not be recommended a standing allow just because
            // its first-observed member looked routine.
            let protected = members
                .iter()
                .any(|m| is_protected_family(&family, &m.raw));
            FamilyGroup {
                count: members.len(),
                reusable,
                sample: members[0].raw.clone(),
                cause: members[0].cause.clone(),
                recommendation: recommendation_for(&family, protected, reusable),
                family,
            }
        })
        .collect();
    groups.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.family.cmp(&b.family)));
    groups
}

/// Runs the extractor for `agent` over every transcript file in `files`,
/// tagging each request with its file stem as the session id.
pub fn audit_report(agent: AuditAgent, files: &[PathBuf]) -> AuditReport {
    let mut requests = Vec::new();
    let mut scanned = 0usize;
    for path in files {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        scanned += 1;
        let session = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();
        let mut found = match agent {
            AuditAgent::Codex => extract_codex_requests(&text, &session),
            AuditAgent::Claude => extract_claude_requests(&text, &session),
        };
        requests.append(&mut found);
    }
    AuditReport {
        agent: agent.label(),
        sessions_scanned: scanned,
        total_requests: requests.len(),
        groups: group_requests(&requests),
    }
}

pub fn render_report(report: &AuditReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# Permission audit -- {} ({} sessions scanned, {} requests)\n\n",
        report.agent, report.sessions_scanned, report.total_requests
    ));
    if report.groups.is_empty() {
        out.push_str("No escalated or denied permission requests found.\n");
        return out;
    }
    for group in &report.groups {
        out.push_str(&format!(
            "## {} -- {} request(s), {}\n",
            group.family,
            group.count,
            if group.reusable {
                "reusable"
            } else {
                "NOT reusable"
            }
        ));
        out.push_str(&format!("- cause: {}\n", group.cause));
        out.push_str(&format!("- sample: `{}`\n", group.sample));
        out.push_str(&format!("- recommendation: {}\n\n", group.recommendation));
    }
    out
}

// ---------------------------------------------------------------------
// CLI: `zirv ctx permissions audit`
// ---------------------------------------------------------------------

#[derive(Debug, clap::Args)]
pub struct PermissionsArgs {
    #[command(subcommand)]
    pub verb: PermissionsVerb,
}

#[derive(Debug, clap::Subcommand)]
pub enum PermissionsVerb {
    /// Audit recent transcripts for escalated/denied permission requests,
    /// grouped by normalized command family.
    Audit(AuditArgs),
}

#[derive(Debug, clap::Args)]
pub struct AuditArgs {
    /// Which agent's transcripts to read.
    #[arg(long, value_enum, default_value = "codex")]
    pub agent: AuditAgent,
    /// How many of the most recently modified transcripts to sample.
    #[arg(long, default_value_t = 5)]
    pub sessions: usize,
    /// Print the report as JSON instead of the human-readable form.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

fn transcripts_root(agent: AuditAgent) -> Option<PathBuf> {
    match agent {
        AuditAgent::Codex => crate::utils::home_dir()
            .ok()
            .map(|h| h.join(".codex").join("sessions")),
        AuditAgent::Claude => super::window::projects_root().ok(),
    }
}

pub fn run_audit<W: Write>(args: &AuditArgs, w: &mut W) -> CtxResult<i32> {
    let files = transcripts_root(args.agent)
        .map(|root| super::optimize::newest_transcripts(&root, args.sessions))
        .unwrap_or_default();
    let report = audit_report(args.agent, &files);
    if args.json {
        writeln!(
            w,
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_default()
        )?;
    } else {
        write!(w, "{}", render_report(&report))?;
    }
    Ok(0)
}

pub fn run<W: Write>(args: &PermissionsArgs, w: &mut W) -> CtxResult<i32> {
    match &args.verb {
        PermissionsVerb::Audit(a) => run_audit(a, w),
    }
}

/// Shared with `optimize.rs`'s friction pass (issue #132: "surface the same
/// summary in setup/optimization output when approval noise exceeds a
/// documented threshold"): builds the audit for `agent` over `files` and
/// returns it only when the noise is worth interrupting the operator about.
pub fn noisy_audit(agent: AuditAgent, files: &[PathBuf]) -> Option<AuditReport> {
    let report = audit_report(agent, files);
    (report.total_requests > NOISE_FINDING_THRESHOLD).then_some(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------------
    // family_of / is_reusable
    // -------------------------------------------------------------

    #[test]
    fn family_of_collapses_wrappers_to_the_underlying_command() {
        assert_eq!(family_of("gh issue create --title x"), "gh issue");
        assert_eq!(family_of("/bin/zsh -lc 'gh issue view 42'"), "gh issue");
        assert_eq!(
            family_of("env -u FOO zirv setup profile --repo ."),
            "zirv setup profile"
        );
        assert_eq!(
            family_of("cargo nextest run --no-fail-fast"),
            "cargo nextest"
        );
        assert_eq!(family_of("git fetch origin"), "git fetch");
        assert_eq!(family_of("curl https://example.com/health"), "curl");
    }

    #[test]
    fn family_of_treats_a_pipe_target_as_downstream_not_a_new_family() {
        assert_eq!(
            family_of("gh issue list --limit 5 | jq '.[].title'"),
            "gh issue"
        );
    }

    #[test]
    fn a_short_flag_value_is_reusable() {
        assert!(is_reusable("gh issue create --title x --label bug"));
        assert!(is_reusable("cargo nextest run --no-fail-fast"));
        assert!(is_reusable("zirv setup apply --repo ."));
    }

    #[test]
    fn a_long_literal_body_is_not_reusable() {
        let body = "x".repeat(200);
        let command = format!("gh issue create --title y --body \"{body}\"");
        assert!(!is_reusable(&command));
    }

    #[test]
    fn a_pipe_into_a_text_filter_is_not_reusable() {
        assert!(!is_reusable("gh issue list --limit 5 | jq '.[].title'"));
        assert!(!is_reusable("gh pr view 12 --json body | jq -r .body"));
    }

    /// 2026-08-25 review: `is_reusable` used to check only the BARE last
    /// pipe stage, so an `env`-decorated filter target slipped past the
    /// `jq`/`grep`/`awk`/`sed` check entirely and was misjudged reusable.
    #[test]
    fn a_decorated_pipe_filter_is_still_recognised_as_not_reusable() {
        assert!(!is_reusable(
            "gh issue list --limit 5 | env NO_COLOR=1 jq ."
        ));
    }

    // -------------------------------------------------------------
    // codex extractor
    // -------------------------------------------------------------

    #[test]
    fn codex_extracts_only_require_escalated_exec_requests() {
        let lines = [
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "custom_tool_call",
                    "name": "exec",
                    "call_id": "call_1",
                    "sandbox_permissions": "require_escalated",
                    "command": "gh issue create --title x --body-file /tmp/body.md"
                }
            })
            .to_string(),
            // Not escalated -- must be skipped.
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "custom_tool_call",
                    "name": "exec",
                    "call_id": "call_2",
                    "command": "cargo build"
                }
            })
            .to_string(),
            // Not an exec tool call -- must be skipped.
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "custom_tool_call",
                    "name": "apply_patch",
                    "sandbox_permissions": "require_escalated",
                    "command": "echo hi"
                }
            })
            .to_string(),
        ];
        let text = lines.join("\n");
        let requests = extract_codex_requests(&text, "session-a");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].family, "gh issue");
        assert_eq!(requests[0].result, "escalated");
        // A `--body-file` request has no long inline literal and no filter
        // pipe, so it IS reusable -- the not-reusable shapes are covered by
        // the fixture-driven tests below.
        assert!(requests[0].reusable);
    }

    #[test]
    fn codex_reads_a_command_array_and_a_json_encoded_input_string() {
        let array_line = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "custom_tool_call",
                "name": "exec",
                "sandbox_permissions": "require_escalated",
                "command": ["bash", "-lc", "git push --force origin main"]
            }
        })
        .to_string();
        let requests = extract_codex_requests(&array_line, "s");
        assert_eq!(requests.len(), 1);
        assert!(requests[0].raw.contains("git push"));

        let encoded_input = serde_json::json!({
            "sandbox_permissions": "require_escalated",
            "command": "zirv ctx optimize --sessions 3"
        })
        .to_string();
        let input_line = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "custom_tool_call",
                "name": "exec",
                "input": encoded_input
            }
        })
        .to_string();
        let requests = extract_codex_requests(&input_line, "s");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].family, "zirv ctx optimize");
    }

    // -------------------------------------------------------------
    // claude extractor
    // -------------------------------------------------------------

    fn claude_denial_pair(id: &str, name: &str, command: Option<&str>) -> String {
        let input = match command {
            Some(c) => serde_json::json!({ "command": c }),
            None => serde_json::json!({}),
        };
        let use_line = serde_json::json!({
            "message": {
                "content": [{"type": "tool_use", "id": id, "name": name, "input": input}]
            }
        })
        .to_string();
        let result_line = serde_json::json!({
            "message": {
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": id,
                    "is_error": true,
                    "content": [{"type": "text", "text": format!(
                        "Permission to use {name} has been denied because Claude Code is running in don't ask mode."
                    )}]
                }]
            }
        })
        .to_string();
        format!("{use_line}\n{result_line}")
    }

    #[test]
    fn claude_correlates_a_denial_with_its_tool_use_by_id() {
        let text = claude_denial_pair("toolu_1", "Bash", Some("zirv ctx status"));
        let requests = extract_claude_requests(&text, "session-b");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].family, "zirv ctx status");
        assert_eq!(requests[0].result, "denied");
        assert!(requests[0].cause.contains("dontAsk"));
    }

    #[test]
    fn claude_ignores_a_successful_tool_result() {
        let use_line = serde_json::json!({
            "message": {"content": [{"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "echo hi"}}]}
        }).to_string();
        let ok_result = serde_json::json!({
            "message": {"content": [{"type": "tool_result", "tool_use_id": "t1", "content": "hi"}]}
        })
        .to_string();
        let text = format!("{use_line}\n{ok_result}");
        assert!(extract_claude_requests(&text, "s").is_empty());
    }

    #[test]
    fn claude_ignores_an_error_that_is_not_a_permission_denial() {
        let use_line = serde_json::json!({
            "message": {"content": [{"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "false"}}]}
        }).to_string();
        let err_result = serde_json::json!({
            "message": {"content": [{"type": "tool_result", "tool_use_id": "t1", "is_error": true, "content": [{"type": "text", "text": "command exited 1"}]}]}
        }).to_string();
        let text = format!("{use_line}\n{err_result}");
        assert!(extract_claude_requests(&text, "s").is_empty());
    }

    #[test]
    fn claude_non_bash_tools_report_the_tool_name_as_family() {
        let text = claude_denial_pair("toolu_2", "Read", None);
        let requests = extract_claude_requests(&text, "s");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].family, "Read");
        // Non-Bash denials have no command-shaped payload to judge for
        // reusability, so they default to reusable (the whole TOOL, not one
        // invocation of it, is what a policy change would grant).
        assert!(requests[0].reusable);
    }

    // -------------------------------------------------------------
    // grouping + report
    // -------------------------------------------------------------

    #[test]
    fn group_requests_sorts_by_count_then_family_and_marks_mixed_reusability() {
        let requests = vec![
            PermissionRequest {
                session: "s".into(),
                raw: "cargo nextest run".into(),
                family: "cargo nextest".into(),
                cause: "c".into(),
                result: "escalated".into(),
                reusable: true,
            },
            PermissionRequest {
                session: "s".into(),
                raw: "gh issue create --body x".into(),
                family: "gh issue".into(),
                cause: "c".into(),
                result: "escalated".into(),
                reusable: true,
            },
            PermissionRequest {
                session: "s".into(),
                raw: "gh issue create --body y".into(),
                family: "gh issue".into(),
                cause: "c".into(),
                result: "escalated".into(),
                reusable: false,
            },
        ];
        let groups = group_requests(&requests);
        assert_eq!(groups[0].family, "gh issue");
        assert_eq!(groups[0].count, 2);
        assert!(!groups[0].reusable, "mixed reusability must fold to false");
        assert_eq!(groups[1].family, "cargo nextest");
        assert_eq!(groups[1].count, 1);
        assert!(groups[1].reusable);
    }

    #[test]
    fn render_report_names_every_group_and_its_recommendation() {
        let report = AuditReport {
            agent: "codex",
            sessions_scanned: 2,
            total_requests: 1,
            groups: vec![FamilyGroup {
                family: "gh issue".into(),
                count: 1,
                reusable: true,
                sample: "gh issue view 1".into(),
                cause: "sandbox_permissions: require_escalated".into(),
                recommendation: "Grant 'gh issue' a standing allow".into(),
            }],
        };
        let text = render_report(&report);
        assert!(text.contains("gh issue"));
        assert!(text.contains("reusable"));
        assert!(text.contains("Grant 'gh issue'"));
    }

    #[test]
    fn render_report_says_so_when_nothing_was_found() {
        let report = AuditReport {
            agent: "claude",
            sessions_scanned: 3,
            total_requests: 0,
            groups: vec![],
        };
        assert!(render_report(&report).contains("No escalated or denied"));
    }

    #[test]
    fn noisy_audit_is_none_below_the_threshold_and_some_above_it() {
        // Below threshold: no transcripts at all -> zero requests.
        assert!(noisy_audit(AuditAgent::Codex, &[]).is_none());
    }

    // -------------------------------------------------------------
    // Regression fixtures (issue #132 acceptance criterion: "Add
    // regression fixtures covering all command shapes listed above and
    // assert that routine approved families stop re-prompting while
    // protected families still prompt.")
    // -------------------------------------------------------------

    const CODEX_FIXTURE: &str =
        include_str!("../../../tests/fixtures/codex-rollout-permission-requests.jsonl");
    const CLAUDE_FIXTURE: &str =
        include_str!("../../../tests/fixtures/claude-transcript-permission-requests.jsonl");

    #[test]
    fn codex_fixture_covers_reusable_and_one_off_command_shapes() {
        let requests = extract_codex_requests(CODEX_FIXTURE, "fixture-session");
        // 11 escalated `exec` requests in the fixture; the successful
        // `custom_tool_call_output` line and the non-`exec` `apply_patch`
        // line must both be skipped.
        assert_eq!(requests.len(), 11, "{requests:#?}");

        let groups = group_requests(&requests);
        let by_family: HashMap<&str, &FamilyGroup> =
            groups.iter().map(|g| (g.family.as_str(), g)).collect();

        // Routine, operator-approvable families: reusable, and NOT flagged
        // protected -- recommendation grants a standing allow.
        assert!(
            by_family["zirv setup apply"].reusable,
            "{:?}",
            by_family["zirv setup apply"]
        );
        assert!(by_family["git fetch"].reusable);
        assert!(by_family["zirv commit"].reusable);
        // The bare invocation (ctc_4) and its `env -u ...`-wrapped twin
        // (ctc_5) must normalize to the SAME family, not two separate
        // one-offs -- the exact gap issue #132 names ("environment-clean
        // wrappers ... were saved as exact invocations rather than the
        // underlying zirv capability").
        assert_eq!(by_family["zirv setup apply"].count, 2);

        // The pipe-into-`jq` requests and the long inline issue body all
        // collapse to the SAME "gh issue" family as the reusable
        // `--body-file` request -- exactly the noise issue #132 reports:
        // one family, mixed reusability, so the group as a whole must not
        // be reported reusable.
        assert_eq!(by_family["gh issue"].count, 4);
        assert!(
            !by_family["gh issue"].reusable,
            "{:?}",
            by_family["gh issue"]
        );

        // ctc_3, the `&&`-chained five-gate validation batch, asserted on
        // its own family and recommendation, not folded into an aggregate
        // count: it normalizes to "cargo build" (the first invocation in
        // the chain), is reusable (no long literal, no filter pipe), and
        // -- being neither destructive/global-write/credential/publish --
        // is recommended a standing allow, not left prompting.
        let cargo_build = by_family["cargo build"];
        assert_eq!(cargo_build.count, 1);
        assert!(cargo_build.reusable, "{cargo_build:?}");
        assert!(
            cargo_build.recommendation.contains("Grant 'cargo build'"),
            "{:?}",
            cargo_build
        );
        assert!(
            !cargo_build.recommendation.contains("protected family"),
            "an ordinary validation batch must not be flagged protected: {:?}",
            cargo_build
        );

        // A global machine-wide install (`choco install ...`) is REUSABLE
        // by `is_reusable`'s own definition (no long literal, no pipe), but
        // must still be recommended `ask`, never a standing allow -- issue
        // #132's "keep global writes ... prompting unless the operator
        // explicitly enables it."
        let choco = by_family["choco"];
        assert!(choco.reusable, "{choco:?}");
        assert!(
            choco.recommendation.contains("protected family"),
            "{:?}",
            choco
        );
        assert!(
            !choco.recommendation.contains("standing allow in"),
            "a protected family must never be told to grant a standing allow: {:?}",
            choco
        );

        // Publish/release: also reusable by the same definition, also must
        // stay protected regardless.
        let publish = by_family["cargo publish"];
        assert!(publish.reusable, "{publish:?}");
        assert!(
            publish.recommendation.contains("protected family"),
            "{:?}",
            publish
        );
    }

    #[test]
    fn codex_fixture_marks_the_long_body_and_jq_pipe_families_not_reusable() {
        let requests = extract_codex_requests(CODEX_FIXTURE, "fixture-session");
        let long_body = requests
            .iter()
            .find(|r| r.raw.contains("much longer inline issue body"))
            .expect("the long-body request");
        assert!(!long_body.reusable, "{long_body:?}");

        let jq_pipe = requests
            .iter()
            .filter(|r| r.raw.contains("| jq"))
            .collect::<Vec<_>>();
        assert_eq!(jq_pipe.len(), 2);
        assert!(jq_pipe.iter().all(|r| !r.reusable), "{jq_pipe:#?}");
    }

    #[test]
    fn claude_fixture_covers_bash_and_non_bash_denials() {
        let requests = extract_claude_requests(CLAUDE_FIXTURE, "fixture-session");
        // 4 permission denials in the fixture: zirv ctx status, the long-body
        // gh issue create, the Read denial, and the force-push. The
        // successful nextest run and the ordinary (non-permission) failure
        // must both be excluded.
        assert_eq!(requests.len(), 4, "{requests:#?}");

        let by_family: HashMap<&str, &PermissionRequest> =
            requests.iter().map(|r| (r.family.as_str(), r)).collect();
        assert!(by_family["zirv ctx status"].reusable);
        assert!(!by_family["gh issue"].reusable);
        assert!(by_family["Read"].reusable);
        assert_eq!(by_family["git push"].result, "denied");
    }

    /// Force-push is a genuinely dangerous, non-Bash-tool-only concern.
    /// `is_reusable("git push --force origin main")` is TRUE (no long
    /// literal, no filter pipe) -- exactly why reusability alone must never
    /// drive the recommendation: this test proves the "never softened"
    /// claim by actually reading the group's recommendation, not just its
    /// `result`/`reusable` fields, which a reusable-but-protected family
    /// would otherwise pass unnoticed.
    #[test]
    fn claude_fixture_force_push_stays_protected_despite_being_reusable() {
        let requests = extract_claude_requests(CLAUDE_FIXTURE, "fixture-session");
        let groups = group_requests(&requests);
        let git_push = groups
            .iter()
            .find(|g| g.family == "git push")
            .expect("the force-push group");
        assert!(
            git_push.reusable,
            "sanity: --force has no long literal or pipe, so is_reusable says true: {git_push:?}"
        );
        assert!(
            git_push.recommendation.contains("protected family"),
            "{git_push:?}"
        );
        assert!(
            git_push
                .recommendation
                .contains("stays prompting unless the operator explicitly enables it"),
            "{git_push:?}"
        );
        assert!(
            !git_push.recommendation.contains("standing allow in"),
            "a protected family must never be told to grant a standing allow: {git_push:?}"
        );
    }
}
