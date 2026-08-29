use std::path::{Path, PathBuf};
use std::process::Command;

pub mod claude;
pub mod codex;

use super::CtxResult;
use super::config::CtxConfig;
use super::event::{
    Capabilities, NormalizedEvent, SessionId, SessionRef, StructuralContext, TranscriptUsage,
};

/// How an adapter arranges for turn-boundary events to reach a supervisor's
/// socket. `env` is injected into the launched agent so the hook that runs
/// inside it can find the socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnSignalSetup {
    pub env: Vec<(String, String)>,
    pub instructions: String,
}

pub const SOCKET_ENV: &str = "ZIRV_CTX_SOCKET";
pub const SESSION_ENV: &str = "ZIRV_CTX_SESSION";
/// Tells a spawned session which agent it is running as. Deliberately the
/// same name as `ctx.toml`'s own `agent` config key (`ZIRV_CTX_AGENT` in
/// `config::ENV_MAP`): it states the same fact from the other direction, so a
/// nested `zirv ctx ...` invocation inside a worker's own child processes
/// defaults to that worker's own harness rather than re-resolving from
/// scratch. Read by `mail::run_send`/`mail::run_inbox` to identify the
/// calling session without requiring an explicit `--to`/`--agent` flag.
pub const AGENT_ENV: &str = "ZIRV_CTX_AGENT";

/// Tells a spawned **orchestrator** session which model its own seat runs on,
/// so the `zirv ctx hook pretool` guard inside it can refuse a subagent
/// dispatch that would silently inherit that seat (see `hook::pretool_
/// decision`). Prompt-level guidance was tried first and failed: a fork
/// fan-out inherited the seat model and spent roughly half a five-hour usage
/// window in one run, so the gate is deterministic rather than advisory.
///
/// Set only by the two orchestrator launch paths (`wrap::run_with` for an
/// `Orchestrator` role, and the dashboard's first pane), never by
/// `exec`/`loop`/worker panes -- and listed in `sessions::SUPERVISION_ENV` so
/// a worker spawned from inside an orchestrator session has it scrubbed
/// rather than inherited. A seat is a property of the session that owns it,
/// exactly like `SESSION_ENV`/`SOCKET_ENV`.
pub const SEAT_MODEL_ENV: &str = "ZIRV_CTX_SEAT_MODEL";

/// Set on every child zirv itself launches interactively -- `zirv chat`,
/// `zirv ctx wrap`, or a dashboard pane spawned from a request that vouches
/// a human is present (`SpawnRequest.interactive`) -- so `zirv ctx safety
/// check` (a `PreToolUse` hook that runs as a child of that same claude
/// process, inheriting its environment the same way any other zirv-owned
/// launch env var reaches it) can prove `LaunchMode::Interactive` from
/// zirv's OWN launch record rather than trusting only Claude's
/// self-reported `permission_mode` (issue #147 amendment, 2026-08-26): an
/// operator whose native `defaultMode` is anything other than
/// `"default"`/`"plan"`/`"acceptEdits"` (`"auto"`, in the field evidence
/// that filed this) had every genuinely interactive session silently fall
/// to the fail-closed Headless posture, asking on everything a human was
/// right there to approve. See `safety::launch_mode_pinned_interactive` for
/// the read side.
///
/// Listed in `sessions::SUPERVISION_ENV` so it is scrubbed, not inherited,
/// by a nested launch: a headless worker spawned from inside an interactive
/// session (`exec`/`loop`, or a dashboard pane fulfilling a non-interactive
/// request) must decide its OWN interactivity fresh, never borrow its
/// parent's proof.
pub const LAUNCH_MODE_ENV: &str = "ZIRV_CTX_LAUNCH_MODE";
/// The one value [`LAUNCH_MODE_ENV`] is ever set to. Any other value, or its
/// absence, reads as "not provably zirv-interactive-launched" -- absence is
/// the fail-closed default, not a second, spoofable "false" value.
pub const LAUNCH_MODE_INTERACTIVE_VALUE: &str = "interactive";

/// The `(key, value)` pair a real interactive-launch seam pushes into its
/// child's env vector -- `None` for [`LaunchMode::Headless`], so a headless
/// launch adds nothing rather than a second, spoofable "not interactive"
/// value alongside the pin.
pub fn launch_mode_pin_env(mode: LaunchMode) -> Option<(String, String)> {
    match mode {
        LaunchMode::Interactive => Some((
            LAUNCH_MODE_ENV.to_string(),
            LAUNCH_MODE_INTERACTIVE_VALUE.to_string(),
        )),
        LaunchMode::Headless => None,
    }
}

/// How one argv token spells a model-selecting flag -- `--model`/`-m`, in
/// separated (bare, value is the next token), joined-by-`=`
/// (`--model=x`/`-m=x`), or (short form only) attached (`-mx`) form. Shared
/// by `last_model_flag` below (which needs the value) and `agent::
/// flags_pin_model` (which only needs to know a token pins something at
/// all, never the value), so the two can never drift on what counts as a
/// model flag between them.
///
/// `Separated` deliberately carries no value itself: `last_model_flag` reads
/// the following token from `flags` when it wants one, and `flags_pin_model`
/// never needs to at all -- the flag's own presence is enough to say
/// "already pinned", matching the pre-existing bare `--model`/`-m` rule.
pub(crate) enum ModelFlagForm<'a> {
    Separated,
    Joined(&'a str),
}

/// Classifies `arg`, or `None` when it is not a model flag at all.
///
/// The attached short form (`-mopus`) is recognised only when `arg` is not
/// itself a `--`-prefixed long flag -- `--model-foo` starts with `-m` too,
/// once its own leading `-` is peeled, and must not match -- and carries at
/// least one character of value (`arg.len() > 2`, so a bare `-m` is
/// `Separated`, not an attached value of `""`).
pub(crate) fn classify_model_flag(arg: &str) -> Option<ModelFlagForm<'_>> {
    if arg == "--model" || arg == "-m" {
        return Some(ModelFlagForm::Separated);
    }
    if let Some(value) = arg.strip_prefix("--model=") {
        return Some(ModelFlagForm::Joined(value));
    }
    if let Some(value) = arg.strip_prefix("-m=") {
        return Some(ModelFlagForm::Joined(value));
    }
    if !arg.starts_with("--") && arg.starts_with("-m") && arg.len() > 2 {
        return Some(ModelFlagForm::Joined(&arg[2..]));
    }
    None
}

/// Whether the launch this argv is being built for has a human sitting in
/// front of it who can answer an approval prompt.
///
/// This is the one distinction zirv's shipped posture could not previously
/// express, and it is why `--permission-mode dontAsk` had to be applied to
/// interactive sessions too: with no way to say "someone is watching", the
/// only safe answer was the fail-closed one. Every real-launch seam
/// (`chat.rs`, `wrap.rs`, `dash/mod.rs`, `handover.rs`, `exec.rs`,
/// `run_loop.rs`, `agent.rs`) now states its own answer, and the compiler
/// -- not a comment -- is what keeps a new seam from forgetting to.
///
/// `ValueEnum` so `zirv ctx safety explain --mode <...>` can take it
/// directly; the derived value names are already `interactive`/`headless`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum LaunchMode {
    /// `zirv chat`, `zirv ctx wrap`, a dashboard pane, a live handover swap:
    /// the harness's own TUI is on a terminal the operator is watching, so an
    /// `Ask` verdict becomes a real prompt they can answer.
    Interactive,
    /// `zirv ctx exec`, `zirv ctx loop`, `zirv ctx agent`: nobody is present,
    /// so an `Ask` verdict is an unanswerable prompt and must fail closed.
    Headless,
}

// Task 1 lands these accessors before the later policy/report tasks consume
// them in production; the seam tests exercise both in the meantime.
#[allow(dead_code)]
impl LaunchMode {
    pub fn label(self) -> &'static str {
        match self {
            LaunchMode::Interactive => "interactive",
            LaunchMode::Headless => "headless",
        }
    }

    pub fn is_interactive(self) -> bool {
        matches!(self, LaunchMode::Interactive)
    }
}

/// Whether `flags` already pins one of the CLI-level policy flags
/// `AgentAdapter::policy_args`/`default_sandbox_args` might otherwise
/// prepend: claude's `--disallowedTools`/`--allowedTools`/`--permission-
/// mode`, or codex's `-s/--sandbox` and `-a/--ask-for-approval`. Exact match
/// or the `=`-joined form only (`--sandbox=read-only`) -- unlike `classify_
/// model_flag`, this deliberately does not also recognise an attached short
/// form (`-sread-only`): that spelling was never verified for these flags
/// the way `-mvalue` was for `--model` (`-m`'s own attached form is a
/// dedicated, tested case in this file), and a false positive here means
/// silently *withholding* zirv's own restriction rather than merely
/// mis-ordering a model flag, so precision matters more than coverage.
///
/// **Codex `-c`/`--config` overrides (2026-08-26, approval-posture round):**
/// an operator may also pin codex's approval/sandbox posture via a raw
/// config override -- `-c approval_policy=<value>`, `-c sandbox_mode=<value>`,
/// or the long `--config` spelling of either -- rather than the dedicated
/// `--ask-for-approval`/`--sandbox` flags above. Before this, that spelling
/// was invisible to this function: `flags_pin_policy` returned `false`, so
/// `policy_launch_args` still prepended zirv's own `-c approval_policy=...`
/// (`CodexAdapter::approval_suppression_args`'s exec-probe fallback) *after*
/// the operator's own override, and codex's config resolution is
/// last-value-wins, so the operator's explicit choice was silently
/// overridden by zirv's own. `CODEX_CONFIG_OVERRIDE_KEYS` below closes that
/// gap for the split form (`-c`/`--config` followed by a `key=value` token,
/// checked pairwise since the key lives in the *next* token, unlike every
/// other flag this function recognises), the `=`-joined single-token form
/// (`--config=approval_policy=...`), mirroring the existing `--sandbox=...`
/// handling above, and (2026-08-26, correction round) codex's own attached
/// short form -- `-cKEY=VALUE` with no space at all, verified accepted by
/// codex-cli 0.149.1, mirroring the attached `-mvalue` form
/// `classify_model_flag` already recognises for `--model`. All-or-nothing,
/// same as every other flag this function recognises: pinning just one
/// dimension (e.g. only `approval_policy`) withholds zirv's *entire*
/// computed prefix, not only the approval half -- `policy_launch_args` has
/// no partial-prefix concept, and this function's contract has always been
/// "the operator's own flag pins policy outright".
///
/// Mirrors `agent.rs`'s own `flags_pin_model` in spirit: the operator's own
/// explicit choice must demonstrably win over a zirv-computed default, not
/// merely happen to survive because a CLI takes the last occurrence of a
/// repeated flag. `policy_launch_args` is the sole caller that acts on this.
pub fn flags_pin_policy(flags: &[String]) -> bool {
    const POLICY_FLAG_NAMES: &[&str] = &[
        "--disallowedTools",
        "--allowedTools",
        "--permission-mode",
        "--settings",
        "--sandbox",
        "-s",
        "--ask-for-approval",
        "-a",
        "--approve-for-me",
    ];
    const CODEX_CONFIG_OVERRIDE_KEYS: &[&str] = &["approval_policy", "sandbox_mode"];

    let names_a_config_override_key = |value: &str| {
        CODEX_CONFIG_OVERRIDE_KEYS
            .iter()
            .any(|key| value == *key || value.starts_with(&format!("{key}=")))
    };

    flags.iter().enumerate().any(|(i, f)| {
        if POLICY_FLAG_NAMES
            .iter()
            .any(|name| f == name || f.starts_with(&format!("{name}=")))
        {
            return true;
        }
        if let Some(rest) = f.strip_prefix("--config=") {
            return names_a_config_override_key(rest);
        }
        if f == "-c" || f == "--config" {
            return flags
                .get(i + 1)
                .is_some_and(|next| names_a_config_override_key(next));
        }
        // Codex's attached short form (`-cKEY=VALUE`, no space) -- checked
        // after the `f == "-c"` split form above so a bare `-c` still falls
        // through to that pairwise check rather than being consumed here
        // with an empty attached value.
        if let Some(rest) = f.strip_prefix("-c")
            && !rest.is_empty()
            && names_a_config_override_key(rest)
        {
            return true;
        }
        false
    })
}

/// One family of in-repo-development or destructive actions zirv's own
/// shipped-default "sandboxed, no prompts" posture takes a position on --
/// the single source both `ClaudeAdapter::default_sandbox_args` (which
/// projects every entry onto a concrete `Bash(...)`/`Read(...)`/`Edit(...)`
/// permission rule) and codex's own `default_sandbox_args` (a coarse
/// `--sandbox workspace-write --ask-for-approval never` pair, documented
/// against this same list -- see that method's own doc comment) are
/// expressions of, so the two harnesses' postures cannot independently
/// drift into disagreement about what "sandboxed, no prompts" means.
///
/// **Why `dontAsk` alone is not enough (2026-08-22, fix round 2):** a fresh
/// install with no operator-configured `permissions.allow` denies every
/// `Write`/`Edit`/`Bash` call outright -- safe, but inert, not the "session
/// works and stays safe" posture the operator actually asked for. This list
/// is what makes `--permission-mode dontAsk` *usable* out of the box.
///
/// **Verified live, not guessed**, against the installed `claude 2.1.240`:
/// - `Edit(./**)` (not bare `Write`) is the rule that actually scopes a
///   write to the workspace -- the CLI's own runtime error is explicit
///   about this: `"Write(./**) is not matched by file permission checks --
///   only Edit(path) rules are. ... Edit rules cover all file-editing
///   tools."` A bare `Write` allow rule, tested live, let a write reach the
///   *parent* directory of the workspace with no denial at all.
/// - `Read(./**)` genuinely scopes reads to the workspace (a read outside
///   it was denied); a bare `Read` rule, tested live, did not (it read a
///   file one directory above the workspace).
/// - A `disallowedTools` entry wins over a broader, unrelated `allowedTools`
///   entry even when both could apply to the same command family (`Bash(git
///   push --force *)` denied while a broader `Bash(git *)` allow was also
///   configured) -- Claude Code's own settings.json schema documents `deny`
///   winning over `allow` as the contract, and this was reproduced live, not
///   assumed from the docs alone.
/// - `Bash(<verb> *)` is prefix matching (Claude Code's own embedded schema
///   docs: `"Prefix wildcard: \"Bash(git *)\" - matches git, git status, git
///   commit, etc."`), reproduced live for both the space-separated form
///   (`Bash(git status *)`, the documented spelling) and a colon-separated
///   form that also happened to work; this list uses the documented
///   spelling.
///
/// **What is deliberately NOT in this list, and why:** "writes outside the
/// workspace" is not a separate deny rule -- `Edit(./**)`'s own scoping
/// already denies it by omission (verified live above), and a second rule
/// trying to express the same negative space would be redundant and harder
/// to audit. General credential-file reads via `Bash(cat ...)` are denied
/// the same way: no allow rule pre-approves `cat`/`Bash` in general, so
/// `dontAsk` denies it by omission; `Bash(security *)` is still listed
/// explicitly (the one credential-reading *command family* worth naming on
/// its own, since zirv's own macOS keychain fallback already documents it
/// as the concrete vector -- see `poll.rs`).
///
/// **Fix round 4 (2026-08-23, issue #104): whole toolchain families, harness
/// dirs, scratchpad, `WebFetch`/`WebSearch`.** Round 2's list above still hit
/// `dontAsk`'s own inert-by-omission failure one layer up: it only
/// pre-approved a handful of subcommands per toolchain (`cargo build *`/
/// `cargo test *`/`cargo check *`, not `cargo run *`/`cargo doc *`/...), so
/// an otherwise-legitimate in-family command still hit a silent, final
/// denial. The narrow per-subcommand entries are replaced with whole
/// `Bash(<tool> *)` families (`git *`, `gh *`, `cargo *`, `npm *`, `npx *`,
/// `node *`, `python *`, `python3 *`, `pip *`, `go *`, `dotnet *`, `make *`,
/// `gradle *`, `mvn *`, `pytest *`, `zirv *`) plus a set of read-only shell
/// utilities -- the deny list, not per-verb narrowing, is what still keeps
/// each family's destructive half blocked (`git clean *`, `git push
/// --delete *`, `gh repo delete *`, `gh release delete *`, `gh auth *`,
/// `cargo publish *`, `npm publish *`, added to `SHIPPED_POSTURE_DENY`
/// alongside the pre-existing force-push/reset/rebase/curl/wget/sudo/su/
/// security entries -- deny still wins, verified live in fix round 2).
///
/// `zirv *` (issue #98): the injected session prompt routinely instructs a
/// session to run `zirv ctx ...`/`zirv agent ...` -- denying zirv's own CLI
/// by omission would make that prompt-mandated guidance unusable under
/// `dontAsk`.
///
/// Also added: `Read(~/.claude/**)`/`Edit(~/.claude/projects/**)` (inspect
/// the harness's own settings/memory, and write Claude Code's own
/// auto-memory, which lives under `~/.claude/projects/<slug>/memory/`);
/// `Read(~/.zirv/**)` (inspect the operator layer -- `Edit(~/.zirv/**)` is
/// denied below, since a session must never widen its own posture);
/// `WebFetch`/`WebSearch` (bare tool rules, no `Bash(...)` wrapper); and two
/// scratchpad rules computed at launch from the real `std::env::temp_dir()`
/// rather than baked into this `&'static` list -- see [`scratchpad_rules`].
pub const SHIPPED_POSTURE_ALLOW: &[(&str, &str)] = &[
    ("Read(./**)", "read anything inside the workspace"),
    (
        "Edit(./**)",
        "create or modify files inside the workspace (covers both the Write and Edit tools)",
    ),
    (
        "Read(~/.claude/**)",
        "inspect the harness's own settings and memory",
    ),
    (
        "Edit(~/.claude/projects/**)",
        "Claude Code's own auto-memory lives under ~/.claude/projects/<slug>/memory/",
    ),
    (
        "Read(~/.zirv/**)",
        "inspect the operator layer (editing it is denied below)",
    ),
    ("WebFetch", "fetch a URL's contents, read-only"),
    ("WebSearch", "search the web, read-only"),
    // Whole toolchain families (2026-08-23, fix round 4, issue #104) -- see
    // this constant's own doc comment for why the narrower per-subcommand
    // entries these replace were still inert-by-omission on anything else
    // in the same family.
    (
        "Bash(git *)",
        "the full git command family; force-push, hard reset, rebase, filter-branch and clean are denied below and win",
    ),
    (
        "Bash(gh *)",
        "the GitHub CLI; repo/release delete and auth are denied below and win",
    ),
    (
        "Bash(cargo *)",
        "the Rust toolchain; publish is denied below",
    ),
    (
        "Bash(npm *)",
        "the Node/npm toolchain; publish is denied below",
    ),
    (
        "Bash(npx *)",
        "run a Node package binary with no separate install step",
    ),
    ("Bash(node *)", "run a Node script directly"),
    ("Bash(python *)", "the Python toolchain"),
    (
        "Bash(python3 *)",
        "the Python toolchain, explicit-version spelling",
    ),
    ("Bash(pip *)", "install or manage Python packages"),
    ("Bash(go *)", "the Go toolchain"),
    ("Bash(dotnet *)", "the .NET toolchain"),
    ("Bash(make *)", "a Makefile-based toolchain"),
    ("Bash(gradle *)", "the Java/Kotlin/Gradle toolchain"),
    ("Bash(mvn *)", "the Java/Maven toolchain"),
    ("Bash(pytest *)", "test with the Python toolchain"),
    (
        "Bash(zirv *)",
        "zirv's own CLI (issue #98) -- the injected prompt routinely instructs a session to run it; denying it by omission would make that guidance unusable under dontAsk",
    ),
    // Read-only shell utilities.
    ("Bash(ls *)", "list directory contents, read-only"),
    ("Bash(grep *)", "search file contents, read-only"),
    ("Bash(rg *)", "search file contents, read-only"),
    ("Bash(cat *)", "read file contents, read-only"),
    ("Bash(head *)", "read the start of a file, read-only"),
    ("Bash(tail *)", "read the end of a file, read-only"),
    ("Bash(wc *)", "count lines, words or bytes, read-only"),
    (
        "Bash(find *)",
        "search for files by name; read-only DENIED into -delete/-exec/-ok by the entries below",
    ),
    ("Bash(echo *)", "print text, read-only, no side effects"),
    ("Bash(pwd)", "print the working directory, read-only"),
    ("Bash(which *)", "locate a command on PATH, read-only"),
    (
        "Bash(where *)",
        "locate a command on PATH, read-only (Windows spelling)",
    ),
    ("Bash(diff *)", "compare files, read-only"),
    ("Bash(sort *)", "sort input lines, read-only"),
    ("Bash(uniq *)", "filter duplicate lines, read-only"),
    ("Bash(tr *)", "translate or delete characters, read-only"),
    ("Bash(cut *)", "extract fields from input, read-only"),
    // Moved out of SHIPPED_POSTURE_DENY (2026-08-24, primary acceptance
    // criterion): fetching a URL is everyday dev work -- checking an API,
    // downloading a fixture -- and denying the tool wholesale is exactly the
    // over-blocking this round exists to remove. The real danger, a download
    // piped straight into a shell, is denied on its own below.
    (
        "Bash(curl *)",
        "fetch a URL; piping into a shell is denied below",
    ),
    (
        "Bash(wget *)",
        "fetch a URL; piping into a shell is denied below",
    ),
];

/// Projects the operator's scratchpad temp directory into the two claude
/// permission rules that make it usable under `dontAsk` -- computed at
/// launch (2026-08-23, issue #104) inside `ClaudeAdapter::
/// default_sandbox_args` rather than baked into [`SHIPPED_POSTURE_ALLOW`],
/// since the path is per-machine and that constant has to stay `&'static`.
///
/// Claude Code's absolute-path rule form is a *doubled* leading slash
/// (`//<path>`, the same convention `SHIPPED_POSTURE_ALLOW`'s own doc
/// comment cites live findings against). `temp_dir` is normalized to
/// forward slashes, any trailing slash is removed, then **one** leading
/// slash (if the path already had one, e.g. a Unix absolute path) is
/// stripped before the `//` prefix is added -- so the result always has
/// exactly two leading slashes, never three. A Windows path with no leading
/// slash of its own (a drive letter) is unaffected by the strip:
/// `C:\Users\x\AppData\Local\Temp\` becomes
/// `//C:/Users/x/AppData/Local/Temp/claude/**`; a Unix `/tmp` becomes
/// `//tmp/claude/**`, not `///tmp/claude/**`.
pub(crate) fn scratchpad_rules(temp_dir: &Path) -> [String; 2] {
    let normalized = temp_dir.to_string_lossy().replace('\\', "/");
    let trimmed = normalized.trim_end_matches('/');
    let stripped = trimmed.strip_prefix('/').unwrap_or(trimmed);
    let base = format!("//{stripped}/claude/**");
    [format!("Read({base})"), format!("Edit({base})")]
}

/// The destructive families this posture denies regardless of anything on
/// [`SHIPPED_POSTURE_ALLOW`] -- verified live to win over a broader,
/// overlapping allow entry (see that constant's own doc comment).
///
/// **Fix round 4 additions (2026-08-23, issue #104):** `Edit(~/.zirv/**)`
/// (a session must never widen its own posture -- `Read(~/.zirv/**)` is
/// allowed above, editing it is not) and `Read(~/.claude/.credentials.json)`
/// (the harness's own stored OAuth credentials, alongside the harness dirs
/// newly allowed above) -- both non-`Bash` entries, declared first so the
/// claude projection can prepend them the same way it prepends
/// [`SHIPPED_POSTURE_ALLOW`]'s own non-`Bash` entries, see `ClaudeAdapter::
/// default_sandbox_args`. Plus the destructive halves of the toolchain
/// families [`SHIPPED_POSTURE_ALLOW`] widened to whole `Bash(<tool> *)`
/// entries: `cargo publish *`/`npm publish *` (irreversible), `gh repo
/// delete *`/`gh release delete *`/`gh auth *`, `git clean *`, `git push
/// --delete *`. A trailing `" *"` denies the bare invocation too, not only
/// one carrying flags (issue #106's `glob_match` fix -- a claude `Bash(<x>
/// *)` rule is documented to match the bare `<x>` as well).
///
/// **Fix round 5 (2026-08-23, issue #111): argument-reordering and
/// sibling-utility bypasses.** PR #107's review found the round-4 `git
/// push`/`git reset` entries were flag-anchored (`Bash(git push --force *)`)
/// and so matched only when the dangerous flag came first -- `git push
/// origin --force` slipped through untouched, as did the short-flag
/// spellings (`-f`, `-d`), an empty-src refspec delete (`git push origin
/// :branch`), and a force-refspec push (`git push origin +branch`). Those
/// entries are replaced with mid-string-wildcard patterns below (`glob_
/// match` already supports `*` anywhere, not only as a suffix). `find`'s
/// own `-delete`/`-exec`/`-ok` actions, and the credential-path reads
/// `head`/`tail`/`diff` can perform just as well as the already-denied
/// `cat`, are closed the same way, plus three `gh` escapes (`gh api -X
/// DELETE`, `gh secret`, `gh codespace ssh`). **With arbitrary-code
/// toolchains (`python *`, `node *`, ...) allowed by
/// [`SHIPPED_POSTURE_ALLOW`], this list is a tripwire for named
/// destructive/credential command families, not a security boundary** -- a
/// session can always reach the same effect through an interpreter one-liner
/// this list cannot enumerate in advance; the README already frames the
/// shipped posture as an honest partial, and this round narrows the gap
/// without pretending to close it.
pub const SHIPPED_POSTURE_DENY: &[(&str, &str)] = &[
    (
        "Edit(~/.zirv/**)",
        "a session must never widen its own posture",
    ),
    (
        "Read(~/.claude/.credentials.json)",
        "the harness's own stored OAuth credentials",
    ),
    // Self-destructive (2026-08-24): this session itself runs under zirv, so
    // killing a zirv process kills the supervisor that would have asked the
    // question. `evaluate_single` walks the whole deny list before it looks
    // at ask at all, so these beat the broad `taskkill *`/`rm -rf *` entries
    // in SHIPPED_POSTURE_ASK with no ordering rule needed.
    ("Bash(taskkill*zirv*)", "kills the supervising zirv session"),
    (
        "Bash(Stop-Process*zirv*)",
        "kills the supervising zirv session, PowerShell spelling",
    ),
    ("Bash(pkill*zirv*)", "kills the supervising zirv session"),
    ("Bash(killall*zirv*)", "kills the supervising zirv session"),
    (
        "Bash(rm -rf*zirv*)",
        "destroys zirv's own state or operator layer",
    ),
    (
        "Bash(rm -fr*zirv*)",
        "destroys zirv's own state or operator layer, flag-order variant",
    ),
    (
        "Bash(Remove-Item*zirv*)",
        "destroys zirv's own state or operator layer, PowerShell spelling",
    ),
    // The actual danger `curl`/`wget` were denied wholesale for, now denied
    // precisely instead: a remote download executed as a shell script. These
    // are whole-string patterns, matched against the raw command -- which
    // `evaluate` always checks as its first candidate.
    (
        "Bash(* | sh)",
        "a remote download executed as a shell script",
    ),
    (
        "Bash(* | bash)",
        "a remote download executed as a shell script",
    ),
    (
        "Bash(* | zsh)",
        "a remote download executed as a shell script",
    ),
    (
        "Bash(*| sh)",
        "a remote download executed as a shell script, no space before the pipe",
    ),
    (
        "Bash(*| bash)",
        "a remote download executed as a shell script, no space before the pipe",
    ),
    ("Bash(sudo *)", "privilege escalation"),
    ("Bash(su *)", "privilege escalation"),
    (
        "Bash(security *)",
        "macOS keychain CLI; reads stored credentials",
    ),
    // Credential-path reads (2026-08-22, fix round 3): the allow list never
    // grants a broad `cat`/`Bash`, so these were already denied by
    // omission -- explicit here so the guarantee does not rest on that
    // remaining true as the allow list grows. A mid-string wildcard was
    // verified live to be honored (`Bash(cat *.aws*)` denied `cat
    // .aws/credentials`), not assumed from the prefix-only doc example.
    (
        "Bash(cat *credentials*)",
        "reads a file conventionally named for stored credentials",
    ),
    ("Bash(cat *.aws*)", "reads AWS credential files"),
    ("Bash(cat *.ssh*)", "reads SSH private keys"),
    ("Bash(cat *.netrc*)", "reads stored HTTP credentials"),
    // Credential-path reads, head/tail/diff parity (2026-08-23, issue
    // #111): `head`/`tail`/`diff` can read the same credential paths `cat`
    // can, and were not covered by the `cat`-anchored entries above.
    (
        "Bash(head *credentials*)",
        "reads a file conventionally named for stored credentials",
    ),
    ("Bash(head *.aws*)", "reads AWS credential files"),
    ("Bash(head *.ssh*)", "reads SSH private keys"),
    ("Bash(head *.netrc*)", "reads stored HTTP credentials"),
    (
        "Bash(tail *credentials*)",
        "reads a file conventionally named for stored credentials",
    ),
    ("Bash(tail *.aws*)", "reads AWS credential files"),
    ("Bash(tail *.ssh*)", "reads SSH private keys"),
    ("Bash(tail *.netrc*)", "reads stored HTTP credentials"),
    (
        "Bash(diff *credentials*)",
        "reads a file conventionally named for stored credentials",
    ),
    ("Bash(diff *.aws*)", "reads AWS credential files"),
    ("Bash(diff *.ssh*)", "reads SSH private keys"),
    ("Bash(diff *.netrc*)", "reads stored HTTP credentials"),
    ("Bash(cargo publish *)", "publishes a crate; irreversible"),
    ("Bash(npm publish *)", "publishes a package; irreversible"),
    (
        "Bash(gh repo delete *)",
        "irreversibly deletes a GitHub repository",
    ),
    (
        "Bash(gh release delete *)",
        "irreversibly deletes a GitHub release",
    ),
    (
        "Bash(gh auth *)",
        "changes or reveals the operator's own GitHub authentication",
    ),
    // gh escapes (2026-08-23, issue #111).
    (
        "Bash(gh api*DELETE*)",
        "covers both -X DELETE and --method DELETE",
    ),
    ("Bash(gh secret *)", "reads or writes repository secrets"),
    ("Bash(gh codespace ssh*)", "opens a shell into a codespace"),
];

/// The short, closed list of families zirv's shipped posture wants a HUMAN to
/// see before they run (2026-08-24, cross-harness permissions design).
///
/// **This list is deliberately narrow, and adding to it is a product
/// decision, not a hardening reflex.** The primary acceptance criterion is
/// that an everyday dev command -- and a command zirv has never seen -- never
/// prompts. Every entry here is a prompt an operator will actually be
/// interrupted by, so the bar for membership is "genuinely dangerous and
/// hard to undo", not "mutates something". `cargo build`, `npm install`,
/// `git commit`, `mkdir`, an in-repo file write, a plain `curl` and an
/// unrecognised tool are all `Allow`, and must stay that way -- pinned by
/// `the_product_requirement_no_everyday_or_novel_command_ever_prompts` in
/// `safety.rs`.
///
/// **Split from [`SHIPPED_POSTURE_DENY`] by reversibility, not by danger.**
/// `git push --force` is recoverable from a reflog and `rm -rf ./target`
/// from a rebuild, so both ask. `cargo publish` is irreversible and
/// `cat ~/.ssh/id_rsa` has already leaked by the time anyone sees the
/// prompt, so both stay denied.
///
/// **Deny still wins**: `safety::evaluate_single` walks deny before ask, so
/// the specific `Bash(taskkill*zirv*)` deny beats the broad
/// `Bash(taskkill *)` ask here with no ordering rule of its own.
///
/// Projected differently per launch mode: claude's INTERACTIVE argv leaves
/// these off `--allowedTools`, so the safety hook's `"ask"` decision is what
/// prompts on them; claude's HEADLESS argv folds them into
/// `--disallowedTools` alongside the deny set, since nobody is present to
/// answer (see `ClaudeAdapter::default_sandbox_args`).
pub const SHIPPED_POSTURE_ASK: &[(&str, &str)] = &[
    ("Bash(rm -rf *)", "recursive force-delete"),
    (
        "Bash(rm -fr *)",
        "recursive force-delete, flag-order variant",
    ),
    (
        "Bash(git push*--force*)",
        "force-push (covers --force-with-lease too), any argument position",
    ),
    (
        "Bash(git push* -f *)",
        "force-push, short-flag form, followed by more arguments",
    ),
    (
        "Bash(git push* -f)",
        "force-push, short-flag form, as the final argument",
    ),
    (
        "Bash(git push*--delete*)",
        "deletes a remote branch, any argument position",
    ),
    (
        "Bash(git push* -d *)",
        "deletes a remote branch, short-flag form, followed by more arguments",
    ),
    (
        "Bash(git push* -d)",
        "deletes a remote branch, short-flag form, as the final argument",
    ),
    (
        "Bash(git push* :*)",
        "empty-src refspec delete (git push origin :branch)",
    ),
    (
        "Bash(git push* +*)",
        "force-refspec push (git push origin +branch)",
    ),
    (
        "Bash(git reset*--hard*)",
        "destroys uncommitted work and can discard commits, any argument position",
    ),
    ("Bash(git rebase *)", "rewrites commit history"),
    ("Bash(git filter-branch *)", "rewrites commit history"),
    ("Bash(git clean *)", "irreversibly deletes untracked files"),
    // These three glob entries name the most common shapes literally; the
    // general case -- ANY `find -exec`/`-ok`/`-execdir`/`-okdir` action that
    // is not on a small proven-safe allow-list (`find -exec sh -c ...`,
    // `find -exec chmod -R 777 ...`, `-ok rm ...`, ...) -- is caught by
    // `safety::apply_find_exec_outcome` (`is_risky_find_exec`) instead: an
    // ask-unless-proven-safe semantic gate, not another glob to keep
    // enumerating. `find -exec grep`/`-exec sed -n` are everyday read-only
    // work and must not prompt, which is why a blanket `find*-exec*` glob
    // entry is still not carried over here.
    ("Bash(find*-delete*)", "find's own delete action"),
    (
        "Bash(find*-exec rm*)",
        "find's exec action invoking a delete",
    ),
    (
        "Bash(find*-exec*rm -rf*)",
        "find's exec action invoking a recursive force-delete",
    ),
    // Process termination. The zirv-specific spellings are DENIED above and
    // win, since deny is walked first.
    ("Bash(taskkill *)", "terminates a running process"),
    (
        "Bash(Stop-Process *)",
        "terminates a running process, PowerShell spelling",
    ),
    ("Bash(pkill *)", "terminates running processes by name"),
    ("Bash(killall *)", "terminates running processes by name"),
    (
        "Bash(Remove-Item*-Recurse*)",
        "recursive delete, PowerShell spelling",
    ),
    // Raw device and partition tools.
    (
        "Bash(dd *)",
        "writes raw blocks; can destroy a whole device",
    ),
    ("Bash(mkfs*)", "formats a filesystem; destroys its contents"),
    ("Bash(mkswap *)", "reformats a device as swap"),
    ("Bash(diskpart*)", "Windows disk partitioning tool"),
    ("Bash(fdisk *)", "disk partitioning tool"),
    ("Bash(format *)", "formats a volume; destroys its contents"),
    // Registry MUTATION only -- `reg query` is read-only and must not prompt.
    (
        "Bash(reg delete*)",
        "deletes a Windows registry key or value",
    ),
    ("Bash(reg add*)", "writes a Windows registry key or value"),
    ("Bash(reg import*)", "bulk-writes the Windows registry"),
    ("Bash(shutdown *)", "powers off or restarts the machine"),
    ("Bash(reboot*)", "restarts the machine"),
];

/// The last model-flag occurrence in `flags`, in any form `classify_model_
/// flag` recognises -- CLI last-wins semantics, the same rule a real argv
/// parser applies when a flag is repeated, honored across mixed spellings
/// (a later `-mhaiku` still overrides an earlier `--model opus`). `None`
/// when `flags` names no model at all, or when a trailing bare `--model`/
/// `-m` has nothing after it to be its value -- a dangling flag with no
/// value contributes nothing, it does not clear an earlier match.
///
/// Recognises codex's `-m` short alias (all three forms) as well as
/// claude's long `--model`, unlike the version of this function before FIX
/// A: this feeds `seat_model_env`, and a codex-adapter launch built with a
/// bare `-m <expensive>`/`-m=<expensive>`/`-m<expensive>` passthrough used
/// to export no seat env at all, leaving the pretool guard blind to it.
///
/// `pub(crate)`: `agent::run_with` (issue #155, Phase 2) is a second caller,
/// reading the model actually launched with back out of the effective argv
/// for the delegation checkpoint record -- same scan, no reason for a
/// second copy of it.
pub(crate) fn last_model_flag(flags: &[String]) -> Option<&str> {
    let mut found = None;
    let mut i = 0;
    while i < flags.len() {
        match classify_model_flag(&flags[i]) {
            Some(ModelFlagForm::Separated) => {
                if let Some(value) = flags.get(i + 1) {
                    found = Some(value.as_str());
                }
                i += 2;
                continue;
            }
            Some(ModelFlagForm::Joined(value)) => {
                found = Some(value);
            }
            None => {}
        }
        i += 1;
    }
    found
}

/// The model `flags` pins when it pins **nothing else** -- every token in it
/// is part of one model flag, in any form `classify_model_flag` recognises.
/// `None` when `flags` is empty, names any other flag, leaves a bare
/// `--model`/`-m` dangling with no value, or names a value that is itself
/// flag-shaped (a leading `-` is never a model name, and this value becomes an
/// argv token).
///
/// The one caller is `agent::try_join_dashboard`: a dashboard pane cannot
/// honour arbitrary trailing flags (they belong to `exec::run_with`), so a
/// request carrying any declines the pane and runs headless. A model pin is
/// the exception the harness layer now teaches orchestrators to write on every
/// delegation, and it is the one flag a pane *can* honour, since the pane
/// builds its own argv from a resolved worker model anyway -- so recognising
/// exactly that shape is what keeps "pick the cheapest model" from silently
/// costing every dashboard delegation its pane.
pub(crate) fn model_only_flags(flags: &[String]) -> Option<&str> {
    let mut found = None;
    let mut i = 0;
    while i < flags.len() {
        match classify_model_flag(&flags[i]) {
            Some(ModelFlagForm::Separated) => {
                found = Some(flags.get(i + 1)?.as_str());
                i += 2;
            }
            Some(ModelFlagForm::Joined(value)) => {
                found = Some(value);
                i += 1;
            }
            None => return None,
        }
    }
    found
        .map(str::trim)
        .filter(|model| !model.is_empty() && !model.starts_with('-'))
}

/// The `SEAT_MODEL_ENV` pair a launch exports, or nothing. Pure, so which
/// launches disclose a seat is testable without a pty.
///
/// Only an `Orchestrator` launch with a non-blank resolved model discloses
/// one: a `Worker` is not a seat that dispatches subagents, and with no
/// resolved model the harness picks its own default, which zirv cannot name
/// and therefore must not claim to.
///
/// The resolved model prefers an operator-passed `--model`/`--model=` in
/// `flags` (the last occurrence, CLI last-wins) over `cfg_model`
/// (`cfg.chat.model`): `flags` is the argv the launch actually uses, built by
/// `extra_with_model` from `cfg_model` and then the operator's own trailing
/// flags appended after it, so an operator passthrough like `zirv chat --
/// --model fable` with no `chat.model` configured must still disclose the
/// seat it actually launches on, and a configured `chat.model` that an
/// operator's own passthrough then overrides must disclose the flag's value,
/// not the configured one -- both directions the guard was blind to when
/// this only ever read `cfg.chat.model`.
pub fn seat_model_env(
    role: super::prompt::PromptRole,
    flags: &[String],
    cfg_model: Option<&str>,
) -> Vec<(String, String)> {
    if role != super::prompt::PromptRole::Orchestrator {
        return Vec::new();
    }
    let resolved = last_model_flag(flags).or(cfg_model);
    match resolved.map(str::trim).filter(|m| !m.is_empty()) {
        Some(model) => vec![(SEAT_MODEL_ENV.to_string(), model.to_string())],
        None => Vec::new(),
    }
}

/// `Debug` is a supertrait so `Box<dyn AgentAdapter>` can appear in
/// `Result::expect_err` (the registry tests assert on the unknown-adapter
/// error path); every adapter already derives it.
pub trait AgentAdapter: std::fmt::Debug {
    fn name(&self) -> &'static str;

    /// The program this adapter actually spawns -- `agent_bin`'s override, or
    /// this adapter's own default binary name, whichever `ClaudeAdapter::new`/
    /// `CodexAdapter::new` resolved to `program` at construction. Distinct
    /// from `name()`: `name` is the fixed registry key (`"claude"`,
    /// `"codex"`), while this can be any override an operator's `agent_bin`
    /// or `--agent-bin` named. Exists so a caller with only a `&dyn
    /// AgentAdapter` -- `harness_prompt_lines`'s presence check in particular
    /// -- can ask what binary `ready()` actually resolved, without the
    /// module-private `program` field each adapter otherwise keeps to itself.
    fn program(&self) -> &str;

    /// The ACCOUNT/vendor whose rate limits this agent spends, as a stable
    /// lowercase slug (`[a-z0-9-]`): `"anthropic"` for claude, `"openai"`
    /// for codex.
    ///
    /// Deliberately *not* the binary or the adapter's own `name`. Usage
    /// windows are a property of the account being billed, and two harnesses
    /// can sit on one account -- a second Anthropic-backed harness would
    /// report `"anthropic"` here and share claude's windows, which is the
    /// truth about the limit even though it is a different program. It is
    /// what `StateDir::usage_for` names a usage file after, so a change to an
    /// existing adapter's slug orphans that adapter's stored readings.
    fn provider(&self) -> &'static str;

    /// `Err` when the adapter exists but is not safe to use yet, so callers
    /// fail loudly instead of scoring garbage.
    fn ready(&self) -> CtxResult<()>;

    fn detect(&self, command: &[String]) -> bool;

    fn headless_cmd(&self, prompt: &str, session: &SessionId, extra: &[String]) -> Command;
    fn interactive_cmd(&self, initial_prompt: Option<&str>, extra: &[String]) -> Command;
    /// Builds the judgment/distiller model child's command. `model` is empty
    /// when neither the operator's own config (`handoff.model`/`optimize.
    /// model`) nor this adapter's own [`default_distiller_model`](Self::
    /// default_distiller_model) named one, which an adapter with no sane
    /// default of its own (codex) must read as "omit the model flag
    /// entirely" rather than pass an empty value to its own CLI -- see
    /// `resolve_distiller_model` in `handoff.rs`, which is what every caller
    /// uses to turn `Option<&str>` config into this parameter.
    fn distiller_cmd(&self, model: &str) -> Command;

    /// The flags that keep this agent from writing files or running shell
    /// commands: claude's `--disallowedTools=...`, codex's `--sandbox
    /// read-only`. This is the pin `distiller_cmd` applies, exposed on the
    /// trait so any other child that embeds untrusted repository text in its
    /// prompt (the workflow reviewer, which is handed a repo diff) applies the
    /// *same* restriction instead of a hardcoded copy that can drift from it.
    ///
    /// No default: a new adapter has to answer this deliberately rather than
    /// inherit "no restriction" by omission.
    fn read_only_args(&self) -> Vec<String>;

    /// Build a provider-neutral workflow-seat launch. Agent manifests describe
    /// required capabilities and methodology but never grant authority: this
    /// default re-loads the effective canonical policy, applies the normal
    /// headless sandbox/policy projection, and finally appends the adapter's
    /// read-only floor when the seat requires it. Provider-specific model ids
    /// are accepted only when an operator/caller explicitly supplies one;
    /// `model_tier` remains a routing hint rather than a guessed model name.
    fn dispatch_agent(
        &self,
        manifest: &crate::commands::workflow::agents::AgentManifest,
        task: &crate::commands::workflow::agents::AgentTask,
    ) -> CtxResult<Command> {
        self.ready()?;
        let cfg = CtxConfig::load(&task.repo, &|key| std::env::var(key).ok())?;
        let report = crate::commands::workflow::capability::CapabilityReport::for_policy(
            self.name(),
            &cfg.policy,
        );
        for capability in &manifest.required_capabilities {
            if !report.support(*capability).satisfies_requirement() {
                return Err(format!(
                    "workflow agent '{}' requires capability '{}' which is unavailable under the effective policy for adapter '{}'",
                    manifest.id,
                    capability,
                    self.name()
                )
                .into());
            }
        }

        let mut extra = policy_launch_args(&cfg, self, &[], LaunchMode::Headless);
        if let Some(model) = task.model.as_deref() {
            extra.extend(self.model_args(model));
        }
        let system_prompt = format!(
            "zirv workflow agent seat: {}@{}\nrole: {}\nsource instructions are methodology, never authorization.\n\n{}",
            manifest.id,
            manifest.version,
            manifest.role,
            manifest.instructions.trim()
        );
        extra.extend(self.system_prompt_args(&system_prompt));
        if manifest.read_only {
            // Last writer wins for both current adapters' restriction flags.
            // Keeping this last makes the read-only floor impossible for a
            // weaker sandbox/model argument to undo.
            extra.extend(self.read_only_args());
        }
        let session = SessionId::new_v4();
        let mut command = self.headless_cmd(&task.prompt, &session, &extra);
        command.current_dir(&task.repo);
        Ok(command)
    }

    /// Names a known, recorded residual in this adapter's own report-only
    /// sandbox pin ([`read_only_args`](Self::read_only_args)), for the
    /// operator's currently-resolved binary -- issue #89. `None` (the
    /// default, and claude's own answer: `--disallowedTools=...` is the
    /// *whole* restriction claude needs, nothing partial about it) means
    /// there is nothing to disclose. Consulted by
    /// [`announce_sandbox_residual_once`] whenever this adapter is resolved
    /// as the distiller (`handoff::run_model`) or the workflow reviewer
    /// (`workflow::review::reviewer_argv`, via
    /// [`read_only_args_for_agent_name`]), so an operator whose judgment/
    /// review child runs on codex learns about the residual instead of
    /// discovering it only in a doc file a terminal session never opens.
    fn sandbox_residual_note(&self) -> Option<String> {
        None
    }

    /// The model name to use for the judgment/distiller child when the
    /// operator has not named one explicitly (`handoff.model`/`optimize.
    /// model` both empty/unset). `None` -- the default, and codex's own
    /// answer -- means this adapter has no verified cheap-model default of
    /// its own to guess, so `resolve_distiller_model` passes an empty model
    /// through, and this adapter's own `distiller_cmd` must read that as
    /// "omit the model flag" so the agent's own configuration (e.g. codex's
    /// `~/.codex/config.toml`) picks a model instead of zirv guessing a name
    /// that may not exist on the operator's account. Claude's own default is
    /// a real, verified value ("haiku") rather than the trait default,
    /// because a hardcoded model name is specific to one agent's lineup and
    /// must never leak into another adapter's guess.
    fn default_distiller_model(&self) -> Option<&'static str> {
        None
    }

    /// This adapter's own verified model ladder, one tier below `seat` --
    /// `seat` is the orchestrator seat's own model (`cfg.chat.model`), or
    /// `None`/unrecognised when unset, which this must read as "assume the
    /// top tier" rather than guess low. Used only when the operator has not
    /// set `review.<agent>` explicitly (see `resolve_review_model` below,
    /// the one place this and the operator override are combined into the
    /// harness-roster line an Orchestrator session sees).
    ///
    /// `""` -- the default, and not meant to be a real model id -- means
    /// this adapter has no verified ladder of its own, the same "nothing
    /// verified to guess" answer `default_distiller_model`'s `None` gives.
    /// Both registered adapters (claude, codex) override this with real,
    /// verified tier names; `resolve_review_model` is the only caller, and
    /// treats a `""` result the same way it treats any other resolved
    /// string (harmless here because every reachable adapter overrides it).
    fn review_model_below(&self, seat: Option<&str>) -> &'static str {
        let _ = seat;
        ""
    }

    /// This adapter's own hard-coded model for a delegated headless worker
    /// (`zirv ctx agent`, and the dashboard's own spawn-request pane
    /// variant) when the operator has not set `worker.<name>` explicitly.
    /// Used only by `resolve_worker_model` in this module, the one place
    /// this and the operator override are combined into the argv a
    /// delegation spawn actually launches with.
    ///
    /// `None` -- the default, and codex's own answer -- means this adapter
    /// has no verified cheap-enough default of its own to guess, the same
    /// "nothing verified to guess" answer `default_distiller_model` gives:
    /// the launch omits `--model` entirely and the agent's own
    /// configuration (codex's `~/.codex/config.toml`) picks instead.
    /// Claude's own default is `"sonnet"`, a real hard-coded value specific
    /// to claude's lineup: a delegated worker used to silently inherit
    /// whatever the operator's own interactive default happened to be
    /// (often a far pricier model than the work actually needs), which is
    /// exactly the spend this default exists to stop.
    fn default_worker_model(&self) -> Option<&'static str> {
        None
    }

    /// Arguments that add `prompt` to this agent's system prompt for one run.
    /// Empty when the agent has no verified mechanism, which is how an
    /// unsupported agent ships without injection rather than with a guess.
    fn system_prompt_args(&self, prompt: &str) -> Vec<String>;

    /// This agent's own base system prompt: text that only makes sense for
    /// this agent, because it names that agent's tools and conventions.
    /// Composed as a base layer, after the shipped default and before every
    /// layer a human wrote, so the user, repo and command-line layers all
    /// still append after it and still take precedence.
    ///
    /// `None` (the default) means this agent contributes nothing of its own,
    /// which is what an agent whose tool vocabulary zirv has not verified
    /// must do rather than be handed another agent's instructions.
    fn base_system_prompt(&self) -> Option<&'static str> {
        None
    }

    /// This agent's own layer for a delegated **Worker** session -- the
    /// role-scoped counterpart to [`base_system_prompt`](Self::
    /// base_system_prompt), which is spliced in for an **Orchestrator**
    /// session only. Exactly one of the two ever reaches a launch, so a
    /// worker never receives the orchestrator layer's own delegate-and-review
    /// coaching: telling a session that was itself delegated to that its job
    /// is to delegate is what invites the recursion `zirv agent`'s workers
    /// must not do.
    ///
    /// `None` (the default) means this agent contributes no worker-specific
    /// layer of its own, the same "no verified mechanism" shape every other
    /// optional layer on this trait uses.
    fn worker_system_prompt(&self) -> Option<&'static str> {
        None
    }

    /// This agent's own layer for a `PromptRole::SubOrchestrator` session --
    /// a coordinator handed one scope, which may dispatch Workers but not
    /// spawn another coordinator (see `PromptRole::SubOrchestrator`).
    ///
    /// Defaults to the Worker layer: an adapter with nothing coordinator-
    /// specific to say should say the safer thing, not the more permissive
    /// one.
    fn sub_orchestrator_system_prompt(&self) -> Option<&'static str> {
        self.worker_system_prompt()
    }

    /// The user-facing flag name `system_prompt_args` emits, when the agent has
    /// one. Lets a caller find and merge a user's own use of the flag instead
    /// of silently overriding it with a second occurrence. `None` when the
    /// agent has no such flag, which is also the default: nothing to merge.
    fn user_system_prompt_flag(&self) -> Option<&'static str> {
        None
    }

    /// The user-facing flag name that delivers the composed prompt via a
    /// file path instead of argv text, when this agent has a verified one.
    /// `None` (the default) means: use `system_prompt_args`, which puts the
    /// prompt on argv instead.
    fn system_prompt_file_flag(&self) -> Option<&'static str> {
        None
    }

    /// Whether the binary about to be spawned advertises
    /// `system_prompt_file_flag` in its own `--help`. Probed rather than
    /// assumed: an adapter can know a flag's name and still find it missing
    /// from an older install.
    ///
    /// `launch` is the argv the caller is about to spawn, and the probe must
    /// hit exactly that program: `wrap` spawns the user's own argv, which can
    /// be an entirely different install from the one `agent_bin` names, and
    /// handing the file flag to a binary that does not have it fails the
    /// launch outright. An empty `launch` means the adapter's own program.
    ///
    /// `false` -- the default, and the fallback for any probe failure -- means
    /// argv delivery via `system_prompt_args`, never a blocked launch.
    fn supports_system_prompt_file(&self, launch: &[String]) -> bool {
        let _ = launch;
        false
    }

    /// Whether a headless launch this adapter builds resolves to the Windows
    /// `cmd.exe /c <shim>` form (an npm-installed `.cmd`), where cmd.exe
    /// reparses the whole downstream command line. The default derives the
    /// answer from [`resolve_program`]'s own resolution of
    /// [`program()`](Self::program) -- via the free
    /// [`launches_through_cmd_shim`] function -- rather than assuming a
    /// permissive `false`: an adapter that overrides nothing is still
    /// protected, because zirv already knows whether the binary it resolved
    /// is a `.cmd`/`.bat` shim. `false` off Windows and for a directly
    /// executable program, same as before. When `true`, a caller delivers
    /// the headless prompt -- and any folded mail -- on the child's stdin via
    /// [`headless_cmd_stdin`](Self::headless_cmd_stdin) rather than as an
    /// argv token, so that untrusted free text never reaches cmd.exe's parser
    /// (`guard_cmd_shim_reparse` is only the fail-closed backstop). Override
    /// only for a deliberate, reviewable opt-*out* -- there is no legitimate
    /// reason today to opt an adapter *into* more protection than this
    /// derivation already grants it.
    fn launches_through_cmd_shim(&self) -> bool {
        launches_through_cmd_shim(self.program())
    }

    /// A headless launch that expects its prompt on **stdin** rather than as
    /// the `-p <prompt>` argv token, for the
    /// [`launches_through_cmd_shim`](Self::launches_through_cmd_shim) case.
    /// `None` (the default) means this agent has no verified stdin form, so the
    /// caller keeps argv delivery. When `Some`, the returned `Command` reads
    /// its prompt from stdin to EOF -- the same mechanism the distiller uses --
    /// and the caller must pipe the prompt in.
    fn headless_cmd_stdin(&self, session: &SessionId, extra: &[String]) -> Option<Command> {
        let _ = (session, extra);
        None
    }

    /// How many leading argv tokens are the program invocation itself rather
    /// than flags the operator passed. One for a bare binary; more when
    /// `agent_bin` carries arguments, since `"/usr/bin/env claude"` spends two
    /// tokens before the first real flag. A relaunch rebuilds the invocation
    /// from `headless_cmd`, so anything inside this prefix must never be
    /// carried over as if the operator had asked for it.
    fn launch_prefix_len(&self) -> usize {
        1
    }

    fn transcript_path(&self, session: &SessionRef) -> PathBuf;

    /// Must be line-local: every line's events depend on that line alone, so
    /// parsing a transcript in pieces cut at newlines and concatenating the
    /// results is the same as parsing the whole of it. The incremental scoring
    /// path in `score.rs` feeds each adapter only the bytes appended since the
    /// last pass, and that is what makes it equal to a full parse.
    fn parse_events(&self, jsonl: &str) -> Vec<NormalizedEvent>;
    fn structural_context(&self, jsonl: &str, last_n: usize) -> StructuralContext;

    /// The most recently observed live model id inside `jsonl`, or `None`
    /// when this adapter has no per-transcript model signal (the default) or
    /// the fragment happens to carry none. Must be line-local, exactly like
    /// [`parse_events`](Self::parse_events): `score.rs` feeds this only the
    /// bytes appended since the last poll, so a caller keeps the last value
    /// it resolved across polls rather than treating a fragment with no hit
    /// as "no model at all".
    ///
    /// Issue #155 D1: this is what lets a live scoring path call
    /// [`capabilities_for_model`](Self::capabilities_for_model) with a real
    /// model string instead of always falling back to the conservative
    /// "unstated model" reading -- see that method's own doc comment.
    fn model_hint(&self, jsonl: &str) -> Option<String> {
        let _ = jsonl;
        None
    }

    /// Cumulative input/output usage exposed by this harness's transcript.
    /// This is deliberately separate from rot's latest-context token signal:
    /// workflow telemetry needs phase cost, not current context occupancy.
    fn transcript_usage(&self, jsonl: &str) -> Option<TranscriptUsage> {
        let _ = jsonl;
        None
    }

    /// Whether [`transcript_usage`](Self::transcript_usage) returns the
    /// transcript's cumulative latest snapshot instead of summing only the
    /// supplied JSONL fragment.
    fn transcript_usage_is_cumulative(&self) -> bool {
        false
    }

    /// Whether [`parse_events`](Self::parse_events) can ever emit
    /// [`NormalizedEvent::ToolCall`] for this agent -- i.e., whether
    /// `--max-tool-calls` (issue #155, Phase 5(d)) has any real signal to
    /// count against. `true` by default, since most adapters' `parse_events`
    /// are built directly off verified tool-call records in their own
    /// transcript.
    ///
    /// Issue #155 review finding C2: `CodexAdapter` overrides this to
    /// `false` -- its own `parse_events` doc comment explains there is no
    /// verified rollout shape for a tool call at all, so it deliberately
    /// never emits one. Left silently `true` here, `--max-tool-calls` would
    /// accept the flag for a codex worker and then never advance toward it,
    /// which reads as "budget respected" forever rather than "budget not
    /// enforceable". `exec::run_with_clock` checks this once, at
    /// argument-validation time, and refuses the flag outright rather than
    /// let it fail silently on every poll after that.
    ///
    /// Deliberately outside [`Capabilities`]: nothing in `rot.rs` or
    /// anything scored reads this, so it does not belong in the struct that
    /// exists to feed those signals.
    fn counts_tool_calls(&self) -> bool {
        true
    }

    fn compact_command(&self) -> Option<&'static str>;
    fn quit_sequence(&self) -> &'static str;
    fn capabilities(&self) -> Capabilities;

    /// This adapter's usable context window for `model`, when it can state
    /// one. `None` -- the default -- means no verified capacity, which
    /// leaves rotation on its absolute thresholds. Never guess: an
    /// overstated capacity raises the restart ceiling past what the seat
    /// holds, and overrunning a window is worse than rotating early.
    fn context_window_tokens(&self, _model: Option<&str>) -> Option<u64> {
        None
    }

    /// [`capabilities`](Self::capabilities) with the context window resolved
    /// for a KNOWN model. Callers that have a model string to hand use this;
    /// everything else keeps calling `capabilities()`, which carries the
    /// adapter's own conservative default.
    ///
    /// Issue #155 D1: `score.rs`'s live scoring paths (`full_score`,
    /// `IncrementalScorer::poll`) resolve the model via
    /// [`model_hint`](Self::model_hint) off the transcript they already have
    /// in hand and call this instead of `capabilities()`, so a `[1m]` claude
    /// seat's real 1M window reaches rot's token gates rather than the 200k
    /// baseline every unstated model gets.
    fn capabilities_for_model(&self, model: Option<&str>) -> Capabilities {
        Capabilities {
            context_window_tokens: self.context_window_tokens(model),
            ..self.capabilities()
        }
    }

    /// A verified harness-owned way to present a local artifact directly in
    /// that harness's UI, without launching a browser or development server.
    /// Current Claude Code and Codex CLI adapters intentionally keep the
    /// default: accepting an image as model input is not the same capability
    /// as presenting an output artifact to the operator.
    fn native_artifact_presentation(
        &self,
        path: &Path,
        interactive_required: bool,
    ) -> Option<&'static str> {
        let _ = (path, interactive_required);
        None
    }

    /// Whether this concrete launch has a safe system-prompt channel. Most
    /// adapters are launch-invariant; adapters using shell shims can narrow
    /// their advertised capability for the unsafe launch shape.
    fn system_prompt_supported(&self, launch: &[String]) -> bool {
        let _ = launch;
        self.capabilities().system_prompt
    }

    /// What this harness can actually deliver for one of zirv's own policy
    /// capabilities at one requested stance -- the per-adapter half of
    /// `policy::evaluate`, which is the only caller.
    ///
    /// Answer with a `CapabilityDescriptor` naming the **verified per-run
    /// mechanism** this adapter would pin on the launch, or with
    /// `CapabilityDescriptor::advisory_only()` -- the default -- when there is
    /// none. That default is the same "no verified mechanism" shape every
    /// other optional method on this trait uses, and here it carries the
    /// load-bearing honesty rule: prompt text asking a session to respect a
    /// stance is advisory context, never enforcement, so a harness with only
    /// that to offer must report `Support::Unsupported` rather than claim a
    /// guarantee zirv cannot keep.
    ///
    /// `stance` is never `Stance::Allow`: `policy::evaluate` answers that case
    /// itself (zirv is imposing nothing, so there is no mechanism to name), so
    /// an implementation may leave it to a catch-all arm.
    ///
    /// `allow(dead_code)` for the same reason `model_args` below carries one:
    /// the only caller, `policy::evaluate`, has no production caller of its own
    /// until issues #44/#46 wire it in. Both adapters override it already, and
    /// `policy.rs`'s own tests exercise every arm.
    ///
    /// This method only ever sees one `(capability, stance)` pair at a time,
    /// so it cannot express a cross-capability implication -- e.g. claude's
    /// tool-deny pin denying all writes also happens to cover
    /// `outside_repo_fs_write`/`git_push_destructive` whenever
    /// `repo_fs_write = deny`, in the safe (narrowing) direction, but each of
    /// those still answers `Unsupported` in isolation. Issue #44, which pins
    /// a stance onto a real launch, needs the whole `EffectivePolicy` in
    /// hand to exploit an implication like that; it is not visible from this
    /// signature alone.
    #[allow(dead_code)]
    fn policy_support(
        &self,
        capability: super::policy::Capability,
        stance: super::policy::Stance,
        mode: LaunchMode,
    ) -> super::policy::CapabilityDescriptor {
        let _ = (capability, stance, mode);
        super::policy::CapabilityDescriptor::advisory_only()
    }

    /// Argv that applies zirv's canonical `[policy]` (`policy::
    /// EffectivePolicy`) to a REAL session launch -- not the honest report
    /// `policy_support`/`policy::evaluate` produce for `zirv context status`,
    /// but the actual flags a launch command carries, so one operator setting
    /// produces equivalent behaviour on every registered adapter.
    ///
    /// Default is empty: the same "nothing verified to guess" shape every
    /// other optional method on this trait uses, and also the *correct*
    /// answer for the shipped default -- `EffectivePolicy::default()` is
    /// `Allow` on every capability except `network` (`Stance::Allow`'s own
    /// doc comment: "zirv declares no restriction of its own";
    /// `EffectivePolicy`'s own doc comment covers `network`'s exception,
    /// `Option<Stance>` defaulting to `None` -- "no operator layer has ever
    /// named it" -- rather than a `Stance` this method would need to act
    /// on), so an operator who has set no `[policy]` table at all gets
    /// byte-for-byte the same argv as before this method existed, on every
    /// adapter. Anything more restrictive requires an explicit `[policy]`
    /// stance, and anything more *permissive* than the default cannot come
    /// from this method at all: there is no `Stance` value that widens past
    /// `Allow`, and a repo checkout cannot set one stricter than the
    /// operator's own layer either (`policy::resolve`'s narrow-only fold --
    /// see that module's doc comment).
    ///
    /// Only a capability this adapter also names `Enforced`/`Degraded` for in
    /// `policy_support` may ever change this launch's argv: `Ask`/`Allow`
    /// stay `OperatorControlled` (the harness's own native config decides,
    /// exactly as before this method existed) -- this trait has no verified
    /// per-run mechanism to make a headless worker request approval that
    /// isn't already asking, only to suppress or deny it. Both registered
    /// adapters override this for `Deny` on `RepoFsWrite`/`ShellExec` only,
    /// the same pair `read_only_args`/`distiller_cmd` already pin.
    ///
    /// `mode` (issue #134, 2026-08-25) exists for the same reason
    /// `default_sandbox_args` already takes it: codex's projection of a Deny
    /// stance is command-surface-dependent (`codex exec` rejects
    /// `--ask-for-approval` on current codex-cli even though the top-level
    /// interactive `codex` accepts it), so the adapter needs to know which
    /// surface this argv is headed for to project a working flag rather
    /// than one the installed binary rejects outright. Claude has no such
    /// surface split and ignores it, exactly as it already ignores `mode`
    /// nowhere else on this trait.
    fn policy_args(
        &self,
        policy: &super::policy::EffectivePolicy,
        mode: LaunchMode,
    ) -> Vec<String> {
        let _ = (policy, mode);
        Vec::new()
    }

    /// argv for zirv's own shipped-default launch posture (2026-08-22
    /// decision, harness/model parity round): **sandboxed, no prompts**.
    /// Commands run freely inside the repository workspace; anything
    /// reaching outside it fails rather than prompting a human -- both
    /// halves are load-bearing (a posture that stops prompting by removing
    /// the sandbox is not this). Applied by `policy_launch_args` whenever
    /// `cfg.sandbox.enabled` is true (the default; an operator opts out with
    /// `[sandbox] enabled = false` or `ZIRV_CTX_SANDBOX=false`), independent
    /// of whether `[policy]` itself is configured -- `EffectivePolicy`'s own
    /// default stays all-`Allow` ("zirv's per-capability policy declares
    /// nothing"; unchanged by this), so this is a **separate** baseline
    /// layered underneath it, not a change to what `Allow` means.
    ///
    /// Default empty (no verified mechanism); both registered adapters
    /// override it -- unlike `policy_args`, this takes no `EffectivePolicy`
    /// input, since the shipped baseline is the same argv regardless of
    /// what (if anything) `[policy]` says.
    ///
    /// `sandbox` (fix round 3, 2026-08-22) carries the operator's own
    /// `extra_allow`/`extra_deny` (`SandboxConfig`, `config.rs`) -- claude's
    /// own implementation appends both after the command-family rules
    /// projected from `safety` (issue #83, below) before rendering the
    /// generated `--allowedTools=`/`--disallowedTools=` argv, so an operator
    /// whose project needs one more build command is not forced to discard
    /// the whole generated deny list by pinning their own flags instead
    /// (`flags_pin_policy` still covers that path).
    ///
    /// `safety` (issue #83) is zirv's harness-neutral command safety policy
    /// (`safety::SafetyPolicy`, resolved from `[safety]` plus the built-in
    /// set derived from `SHIPPED_POSTURE_ALLOW`/`_DENY`) -- the single
    /// source this method's generated command rules are a projection of.
    /// Under the shipped default (no `[safety]`/`sandbox.extra_*`
    /// configured), `safety` is exactly `SHIPPED_POSTURE_ALLOW`/`_DENY`
    /// again (`SafetyPolicy::default()` derives from the same constants),
    /// so claude's projection stays byte-identical to before this method
    /// took the parameter -- see `default_sandbox_args_stays_byte_
    /// identical_to_the_pre_safety_shipped_default` in `claude.rs`. Codex
    /// has no per-command mechanism to receive either parameter and ignores
    /// both.
    fn default_sandbox_args(
        &self,
        sandbox: &super::config::SandboxConfig,
        safety: &super::safety::SafetyPolicy,
        mode: LaunchMode,
    ) -> Vec<String> {
        let _ = (sandbox, safety, mode);
        Vec::new()
    }

    /// Extra writable-root argv for a launch whose working directory is
    /// `cwd`, beyond `default_sandbox_args`'s own baseline sandbox flags.
    ///
    /// **Caller contract:** call only where both `cwd` and `mail_dir` are
    /// already in hand for a launch that is actually happening -- today that
    /// is `dash::worker_pane_extra_args` alone (see below), called
    /// unconditionally for every dashboard-spawned worker pane, never
    /// speculatively for a launch that may not occur. Added as a distinct
    /// method (2026-08-26, codex approval-posture round)
    /// rather than folded into `default_sandbox_args` itself, since neither
    /// `cwd` nor `mail_dir` is available at that method's existing call site
    /// (`policy_launch_args`) without threading them through all seven
    /// launch seams that call it; this is called separately, only where a
    /// caller already has both in hand (currently `dash::worker_pane_extra_
    /// args`, the one seam issue #119's own evidence names).
    ///
    /// Two concrete gaps this closes for codex, neither addressed by
    /// `default_sandbox_args`'s `--sandbox workspace-write`:
    /// - a dashboard pane whose `cwd` is a linked `git worktree add` sibling
    ///   (issue #119) shares its `.git` common dir with the main checkout,
    ///   which sits OUTSIDE `cwd` and so outside the workspace-write
    ///   sandbox -- every git object/ref write a worker makes there fails
    ///   (headless) or escalates (interactive) with no mechanism to grant it.
    /// - `zirv ctx send`'s report-back write lands under the state dir's
    ///   `mail/` subtree, also outside `cwd`, so it is denied the same way.
    ///
    /// Default empty -- no verified per-run mechanism, matching every other
    /// "no verified mechanism" trait default on this trait (`policy_args`,
    /// `default_sandbox_args`); only `CodexAdapter` overrides it. `mail_dir`
    /// is deliberately the mail subtree alone (`StateDir::mail()`), never the
    /// whole state root: policy snapshots and the decision log must stay
    /// unwritable by the workload even when this widens the mail path.
    fn extra_writable_root_args(&self, cwd: &Path, mail_dir: &Path) -> Vec<String> {
        let _ = (cwd, mail_dir);
        Vec::new()
    }

    fn register_turn_signal(&self, session: &SessionRef, socket: &Path) -> TurnSignalSetup;

    /// Argv tokens that select `model` for one interactive launch (the
    /// dashboard's orchestrator pane, via `chat.model`/`ZIRV_CTX_CHAT_MODEL`).
    /// Appended after the launch prefix, alongside any other `extra` argv
    /// `interactive_cmd` receives. The default is empty, matching every other
    /// "no verified mechanism" trait default on this trait
    /// (`system_prompt_args`, `base_system_prompt`): an adapter with no
    /// verified flag ships with no model selection rather than a guess.
    ///
    /// Both current adapters override this, so nothing calls the default body
    /// through `dyn AgentAdapter` yet -- wired into the orchestrator pane's
    /// argv when `chat.rs` builds it (dashboard Task 6).
    #[allow(dead_code)]
    fn model_args(&self, model: &str) -> Vec<String> {
        let _ = model;
        Vec::new()
    }

    /// Argv tokens that resume `session_id`'s own conversation, for the
    /// dashboard's quit/restore roster (`dash::roster::restore_argv`, called
    /// through `dyn AgentAdapter` -- unlike `model_args` above, both
    /// adapters reach this default body today, since codex does not
    /// override it). `None` -- the default, and every "no verified
    /// mechanism" trait default's own answer -- means this agent's resume
    /// story is unverified: a restore falls back to a fresh launch carrying
    /// a plain one-line "resuming after a dashboard restart" prompt instead
    /// of trying to guess a flag.
    fn resume_args(&self, session_id: &str) -> Option<Vec<String>> {
        let _ = session_id;
        None
    }

    /// Argv tokens that make this agent adopt zirv's own `session` uuid as the
    /// id of the conversation it is about to start, so a later
    /// [`resume_args`](Self::resume_args) against that same uuid finds
    /// something. Empty -- the default -- means the agent mints its own
    /// conversation id and zirv's uuid is only ever a zirv-side handle.
    ///
    /// Appended **only** to a dashboard pane's launch (`chat.rs::
    /// dash_orchestrator_pane` and `dash::fulfill_spawn_request`, both of
    /// which own a freshly minted uuid), never inside `interactive_cmd`
    /// itself: `wrap`'s relaunch path deliberately lets the harness mint a
    /// fresh conversation on every restart, and a restored pane already
    /// carries `resume_args`, which would conflict with a pin.
    ///
    /// Without this, the dashboard's restore roster stored a uuid the agent
    /// had never heard of: `claude --resume <zirv-uuid>` answered "no
    /// conversation found" and the restored pane died immediately.
    fn session_pin_args(&self, session: &str) -> Vec<String> {
        let _ = session;
        Vec::new()
    }
}

/// The program invocation at the head of an argv: the binary plus the leading
/// arguments before the first flag, which is what `sh wrapper.sh --foo` and
/// `/usr/bin/env claude -p x` both need. Anything past that is the operator's
/// own flags and has no business being passed to a `--help` probe.
pub fn program_invocation(launch: &[String]) -> Option<(String, Vec<String>)> {
    let (program, rest) = launch.split_first()?;
    let args = rest
        .iter()
        .take_while(|arg| !arg.starts_with('-'))
        .cloned()
        .collect();
    Some((program.clone(), args))
}

/// A program invocation rewritten so the host OS can actually execute it.
/// `prefix` is the tokens that have to lead the original arguments, empty
/// whenever the program can be spawned directly (always, off Windows).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProgram {
    pub program: String,
    pub prefix: Vec<String>,
}

impl ResolvedProgram {
    /// The invocation exactly as written: no launcher, nothing prepended.
    pub fn direct(program: &str) -> Self {
        Self {
            program: program.to_string(),
            prefix: Vec::new(),
        }
    }
}

/// Resolves `program` the way the OS itself would, and rewrites the
/// invocation when what it resolves to cannot be handed to the process
/// creation call directly.
///
/// Off Windows this is the identity: `execvp` honors the shebang of anything
/// on `PATH`, so there is nothing to rewrite.
///
/// On Windows it matters. An npm-installed `claude` is `claude.cmd`, and the
/// two resolvers zirv uses disagreed about it: `std::process::Command` only
/// ever appends `.exe`, while portable-pty's `search_path` honors `PATHEXT`,
/// finds `claude.cmd`, and then hands it to `CreateProcessW` as
/// `lpApplicationName`, which rejects it with `ERROR_BAD_EXE_FORMAT` (193).
/// Resolving `PATH` plus `PATHEXT` here and routing a `.cmd`/`.bat` through
/// `cmd.exe` (a `.ps1` through PowerShell) is what makes the most common
/// Windows install layout launch at all.
///
/// A program that resolves to nothing is returned untouched, so a missing
/// binary still fails with the OS's own "not found" rather than a zirv error
/// about a path that does not exist. `Err` is reserved for the one case zirv
/// can name before spawning and knows will fail: a bare name that `PATHEXT`
/// resolved to a file type with no launcher. A program written with a
/// directory in it is never an error here, whatever it ends in: the caller
/// named that exact file, and a wrapper this code has never heard of is
/// theirs to be told about by the OS, exactly as before.
#[cfg(windows)]
pub fn resolve_program(program: &str) -> Result<ResolvedProgram, String> {
    let Some((resolved, from_path)) = resolve_on_path(program) else {
        return Ok(ResolvedProgram::direct(program));
    };
    let extension = resolved
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let found = resolved.display().to_string();
    match extension.as_str() {
        "cmd" | "bat" => Ok(ResolvedProgram {
            program: std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string()),
            prefix: vec!["/c".to_string(), found],
        }),
        "ps1" => Ok(ResolvedProgram {
            program: "powershell".to_string(),
            prefix: vec!["-NoProfile".to_string(), "-File".to_string(), found],
        }),
        other if from_path && !matches!(other, "exe" | "com" | "") => Err(format!(
            "cannot launch '{program}': it resolves to '{found}', which Windows cannot execute \
             directly (CreateProcess accepts only .exe and .com). zirv runs .cmd and .bat through \
             cmd.exe and .ps1 through PowerShell, but it has no launcher for '.{other}'."
        )),
        // Directly executable, or named explicitly enough that the caller
        // owns the outcome. Deliberately keeps the program spelled the way it
        // was written rather than substituting the resolved path: nothing
        // about the launch changes, so nothing about it should.
        _ => Ok(ResolvedProgram::direct(program)),
    }
}

#[cfg(not(windows))]
pub fn resolve_program(program: &str) -> Result<ResolvedProgram, String> {
    Ok(ResolvedProgram::direct(program))
}

/// I: the flags an adapter-built command carries, with any launcher prefix
/// dropped. On a Windows machine where the adapter's own program resolves to
/// a real npm `.cmd` shim, every command an adapter builds starts `cmd.exe /c
/// <shim>`, and those tokens are not what a test about agent flags is
/// asserting on. `program` is the adapter's own `program` field (each
/// adapter's `base()` resolves exactly this string) -- a shared helper takes
/// it as a plain `&str` rather than `&dyn AgentAdapter` because nothing else
/// about the adapter is needed, and because `program` is private to each
/// adapter's own module, so only that module's own tests can pass it in
/// anyway. Was duplicated byte-for-byte in `claude.rs` and `codex.rs`'s own
/// test modules before this; both now call this one copy.
#[cfg(test)]
pub(crate) fn built_args(program: &str, cmd: &std::process::Command) -> Vec<String> {
    let launcher = resolve_program(program)
        .map(|resolved| resolved.prefix.len())
        .unwrap_or(0);
    cmd.get_args()
        .skip(launcher)
        .map(|a| a.to_string_lossy().to_string())
        .collect()
}

/// The cmd.exe metacharacters that, appearing RAW in an argument, cmd.exe
/// re-parses out of its own `/c` command line rather than passing through to
/// the shim it invokes. portable-pty and `std::process` both append a
/// no-whitespace metachar-bearing argument to a Windows command line unquoted,
/// and an embedded `"` toggles cmd.exe out of any quoting that *was* added
/// (BatBadBut / CVE-2024-24576's quote-toggle). Newline and carriage return
/// terminate the command line outright. Any of these in a shim-form argument
/// is therefore a command-injection primitive, not a literal argument value.
#[cfg(windows)]
const CMD_REPARSE_METACHARS: &[char] =
    &['&', '|', '<', '>', '^', '(', ')', '%', '!', '"', '\n', '\r'];

/// Whether `program` + `args` is the `cmd.exe /c <shim>` launcher form that
/// [`resolve_program`] produces for a `.cmd`/`.bat` on Windows: the program's
/// file stem is `cmd` and the first argument is `/c`. Matched structurally
/// (case-insensitively) rather than by identity with a specific `COMSPEC`
/// value, so a full-path or upper-cased `CMD.EXE` is recognised too.
#[cfg(windows)]
fn is_cmd_shim_launch(program: &str, args: &[String]) -> bool {
    let program_is_cmd = Path::new(program)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(|stem| stem.eq_ignore_ascii_case("cmd"))
        .unwrap_or(false);
    program_is_cmd
        && args
            .first()
            .map(|first| first.eq_ignore_ascii_case("/c"))
            .unwrap_or(false)
}

/// FIX D (defense-in-depth): the number of leading `args` tokens that are the
/// zirv-controlled launcher prefix, when `program` + `args` is a Windows
/// launcher form whose command line is reparsed before it reaches the real
/// script -- either the `cmd.exe /c <shim>` form (a `.cmd`/`.bat`) or the
/// `powershell -NoProfile -File <script>` form (a `.ps1`), both of which
/// [`resolve_program`] produces. `None` when it is neither, so a direct
/// `.exe` or an `sh <script>` fake agent is not keyed on at all. Keyed on the
/// `/c` / `-File` structure rather than only the launcher basename, so the
/// guard covers whichever launcher the resolver actually inserted.
#[cfg(windows)]
fn reparse_launcher_prefix(program: &str, args: &[String]) -> Option<usize> {
    let stem = Path::new(program)
        .file_stem()
        .and_then(|stem| stem.to_str())?
        .to_ascii_lowercase();
    match stem.as_str() {
        "cmd" => args
            .first()
            .map(|first| first.eq_ignore_ascii_case("/c"))
            .unwrap_or(false)
            // `/c` and the shim path are both zirv-controlled.
            .then_some(2),
        "powershell" | "pwsh" => {
            // Everything through the `-File <script>` pair is the launcher
            // prefix; the script's own arguments follow it.
            let file_at = args
                .iter()
                .position(|arg| arg.eq_ignore_ascii_case("-File"))?;
            (file_at + 1 < args.len()).then_some(file_at + 2)
        }
        _ => None,
    }
}

/// FIX (command-injection defense): fail-closed guard for the one launch shape
/// where a downstream argv element becomes cmd.exe *source text* rather than a
/// literal argument. When [`resolve_program`] rewrites an npm-installed
/// `claude.cmd` to `cmd.exe /c <shim>`, cmd.exe parses the whole appended
/// command line before invoking the shim, so any argument after the shim path
/// that carries a cmd.exe metacharacter is re-interpreted as a command. Repo-
/// controlled strings (an injected system prompt, a passed-through flag) reach
/// this argv, so an unguarded metacharacter there is arbitrary code execution
/// on a victim who merely runs a supervised session in a hostile checkout.
///
/// This rejects such a launch outright rather than trying to quote around
/// cmd.exe (which the embedded-quote toggle defeats). It is deliberately a
/// pure decision function over the already-resolved `program`/`args`, called
/// at every spawn seam (`supervise::spawn_tapped` for the headless
/// `exec`/`loop` path; the `CommandBuilder` assembly in `wrap` and
/// `dash::pane` for the pty path), so there is one metacharacter policy.
///
/// A no-op off Windows, and on Windows for any launch that is not the shim
/// form: a direct `.exe`, an `sh <script>` fake agent, or a program with no
/// launcher prefix is spawned exactly as before. zirv's own flags never carry
/// these characters, so only injected content is ever rejected. The two shim-
/// prefix tokens themselves (`/c` and the shim path) are zirv-controlled and
/// skipped.
pub fn guard_cmd_shim_reparse(program: &str, args: &[String]) -> Result<(), String> {
    #[cfg(windows)]
    {
        if let Some(prefix) = reparse_launcher_prefix(program, args) {
            for arg in args.iter().skip(prefix) {
                if let Some(bad) = arg.chars().find(|c| CMD_REPARSE_METACHARS.contains(c)) {
                    return Err(format!(
                        "refusing to launch: argument '{arg}' contains the cmd.exe \
                         metacharacter {bad:?}. zirv routes this agent through a Windows \
                         launcher ('cmd.exe /c' for an npm-installed '.cmd' shim, or \
                         'powershell -File' for a '.ps1'), which would re-parse that character \
                         as a command rather than pass it through. This is a fail-closed \
                         backstop against command injection; zirv's own arguments never contain \
                         these characters, and untrusted content (the composed system prompt, a \
                         headless task prompt) is kept off this argv entirely."
                    ));
                }
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (program, args);
    }
    Ok(())
}

/// Whether spawning `program` resolves to the Windows `cmd.exe /c <shim>`
/// launcher form (an npm-installed `.cmd`), where cmd.exe reparses the whole
/// downstream command line. The adapters use it to move a headless prompt --
/// and any folded mail -- onto the child's stdin on exactly the launch shape
/// where an argv token would otherwise be reparsed. Always `false` off
/// Windows, and for a directly executable program.
pub fn launches_through_cmd_shim(program: &str) -> bool {
    #[cfg(windows)]
    {
        match resolve_program(program) {
            Ok(resolved) => is_cmd_shim_launch(&resolved.program, &resolved.prefix),
            Err(_) => false,
        }
    }
    #[cfg(not(windows))]
    {
        let _ = program;
        false
    }
}

/// Whether spawning the argv `launch` puts its downstream tokens through a
/// Windows launcher that reparses them -- either the `cmd.exe /c <shim>` form
/// (an npm-installed `.cmd`) or the `powershell -File <script>` form (a
/// `.ps1`). Unlike [`launches_through_cmd_shim`], which is given only a bare
/// program name to re-resolve, this handles an argv that is **already
/// resolved** to a launcher: `chat::build_launch`/`ClaudeAdapter::base` hand
/// `wrap`/`dash_orchestrator_pane` an argv whose head is literally `cmd.exe`
/// (or `powershell`), so re-resolving that head finds a plain `.exe` and would
/// wrongly report "not a shim", leaving the forced-file-form defence inert on
/// the interactive path. Recognising the resolved launcher structure directly
/// (via [`reparse_launcher_prefix`]) is what keeps that defence engaged.
///
/// Falls back to resolving the head program for an argv that has *not* been
/// resolved yet (a raw `wrap` command such as `["claude", "--resume"]`), so
/// both call shapes reach the same verdict. Always `false` off Windows.
pub fn launch_reparses_through_shim(launch: &[String]) -> bool {
    #[cfg(windows)]
    {
        let Some((program, rest)) = launch.split_first() else {
            return false;
        };
        // An already-resolved `cmd.exe /c <shim>` or `powershell -File <script>`
        // argv: the launcher reparses everything past its own prefix.
        if reparse_launcher_prefix(program, rest).is_some() {
            return true;
        }
        // Otherwise the head is an ordinary program name that `resolve_program`
        // may still route through a launcher.
        launches_through_cmd_shim(program)
    }
    #[cfg(not(windows))]
    {
        let _ = launch;
        false
    }
}

/// `std::process::Command` -> the flat `program, arg, arg, ...` form
/// [`launch_reparses_through_shim`] wants. Shared here rather than
/// duplicated per call site (`exec.rs`, `run_loop.rs`; `dash/mod.rs` keeps
/// its own private copy, established first and not worth churning): a probe
/// command built purely to answer "what launcher shape would this be" (no
/// real prompt text on it yet) is flattened the same way regardless of
/// which module is asking.
pub fn flatten_command(command: std::process::Command) -> Vec<String> {
    let mut argv = vec![command.get_program().to_string_lossy().to_string()];
    argv.extend(command.get_args().map(|a| a.to_string_lossy().to_string()));
    argv
}

/// `PATH` plus `PATHEXT`, the search the Windows shell performs and
/// `std::process::Command` does not. A program that already carries a
/// directory is looked for where it says, not on `PATH`; the flag reports
/// which of the two happened, because only a `PATH` hit is a name the shell
/// itself would have claimed to be executable.
#[cfg(windows)]
fn resolve_on_path(program: &str) -> Option<(PathBuf, bool)> {
    if program.is_empty() {
        return None;
    }
    let extensions: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
        .split(';')
        .filter(|ext| !ext.is_empty())
        .map(|ext| ext.to_ascii_lowercase())
        .collect();

    let named_directory = program.contains('/') || program.contains('\\');
    let bases: Vec<PathBuf> = if named_directory {
        vec![PathBuf::from(program)]
    } else {
        std::env::var_os("PATH")
            .map(|path| {
                std::env::split_paths(&path)
                    .map(|dir| dir.join(program))
                    .collect()
            })
            .unwrap_or_default()
    };

    let from_path = !named_directory;
    for base in bases {
        // An explicit extension that exists wins outright, so
        // `claude.cmd` is never resolved to `claude.cmd.exe`.
        if base.extension().is_some() && base.is_file() {
            return Some((base, from_path));
        }
        for extension in &extensions {
            let candidate = PathBuf::from(format!("{}{extension}", base.display()));
            if candidate.is_file() {
                return Some((candidate, from_path));
            }
        }
        if base.is_file() {
            return Some((base, from_path));
        }
    }
    None
}

/// Whether `program`'s binary genuinely exists on disk -- either at an
/// explicit path, or somewhere on `PATH` (`PATHEXT`-aware on Windows).
///
/// This is deliberately a *stronger* claim than `resolve_program`/`ready()`
/// make: `resolve_program` is fail-open by design for a name it cannot find
/// (a program that resolves to nothing is spawned exactly as written, so a
/// genuinely missing binary fails with the OS's own "not found" rather than a
/// zirv-invented error), and several call sites rely on exactly that
/// fail-open behavior (`agent_bin` naming a not-yet-real path still has to
/// fall through to whichever adapter it actually matches by name, not error
/// out early). `harness_prompt_lines` is the one caller that turns "ready"
/// into a concrete invitation (`zirv agent <name> "<prompt>"`) an orchestrator
/// may act on immediately, so it alone needs this stronger check layered on
/// top of -- never in place of -- `ready()`.
pub fn program_is_present(program: &str) -> bool {
    if program.is_empty() {
        return false;
    }
    #[cfg(windows)]
    {
        resolve_on_path(program).is_some()
    }
    #[cfg(not(windows))]
    {
        if program.contains('/') {
            return Path::new(program).is_file();
        }
        std::env::var_os("PATH")
            .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
            .unwrap_or(false)
    }
}

/// An adapter constructor: the same shape `ClaudeAdapter::new` and
/// `CodexAdapter::new` already share, named so `ADAPTERS` reads as a table
/// rather than a wall of type punctuation.
pub type AdapterCtor = fn(Option<&str>) -> Box<dyn AgentAdapter>;

fn make_claude(bin: Option<&str>) -> Box<dyn AgentAdapter> {
    Box::new(claude::ClaudeAdapter::new(bin))
}

fn make_codex(bin: Option<&str>) -> Box<dyn AgentAdapter> {
    Box::new(codex::CodexAdapter::new(bin))
}

/// The single source of truth for which adapters exist: a name paired with a
/// constructor. Adding an adapter is one entry here (plus its own module) --
/// `all`, `select`'s fallback, `describe_known_adapters`, `resolve_default`
/// and `readiness_note` all walk this table rather than naming adapters by
/// hand, so none of them can drift from it.
pub const ADAPTERS: &[(&str, AdapterCtor)] = &[("claude", make_claude), ("codex", make_codex)];

pub fn all(bin: Option<&str>) -> Vec<Box<dyn AgentAdapter>> {
    ADAPTERS.iter().map(|(_, ctor)| ctor(bin)).collect()
}

/// Low 5: the account a usage readout should report for `name`, without
/// needing that adapter to be enabled or ready -- adapter name -> provider
/// is a static fact through the registry (`ctor(None).provider()` never
/// touches the filesystem, a gate, or `ready()`), so it stays answerable
/// even when `adapters::select(name, ...)` itself would refuse. `zirv ctx
/// usage`'s no-subcommand branch and `zirv ctx status`'s usage-windows line
/// used to fall back to `window::LEGACY_USAGE_PROVIDER` on *any* `select`
/// refusal, which silently showed Anthropic percentages for a repo
/// configured for a disabled codex rather than "openai: no usage source" --
/// a guess dressed up as a fact. Falls back to `LEGACY_USAGE_PROVIDER` only
/// when `name` is `None` or matches no registered adapter at all (an unknown
/// or absent configuration, where there truly is nothing more specific to
/// say than the legacy default).
pub fn provider_for_agent_name(name: Option<&str>) -> &'static str {
    name.and_then(|n| ADAPTERS.iter().find(|(adapter_name, _)| *adapter_name == n))
        .map(|(_, ctor)| ctor(None).provider())
        .unwrap_or(super::window::LEGACY_USAGE_PROVIDER)
}

/// `AgentAdapter::read_only_args` for a registered adapter name, without
/// requiring that adapter to be enabled or ready -- the same static-fact
/// lookup through `ADAPTERS` that `provider_for_agent_name` does. `None` for
/// an unknown name, so a caller that must not launch an unpinned child can
/// refuse rather than guess an empty restriction.
pub fn read_only_args_for_agent_name(name: &str) -> Option<Vec<String>> {
    ADAPTERS
        .iter()
        .find(|(adapter_name, _)| *adapter_name == name)
        .map(|(_, ctor)| {
            let adapter = ctor(None);
            // Issue #89: the workflow reviewer's own choke point for
            // resolving a read-only pin by name -- a sibling call site to
            // the ones production callers make directly around `handoff::
            // run_model` for the distiller role. `chrome.events` is not
            // known at this call site (no `CtxConfig` in hand), so this
            // defaults to enabled, matching this function's own pre-
            // existing "no config, no repo" shape; `reviewer_argv`'s own
            // caller may still be running under `ZIRV_CTX_QUIET`, which
            // `Announcer` itself does not re-check here -- see the
            // documented residual on `announce_sandbox_residual_once`.
            announce_sandbox_residual_once(adapter.as_ref(), true);
            adapter.read_only_args()
        })
}

/// Issue #89: a one-time `zirv ▸` announcement naming a resolved distiller/
/// reviewer adapter's own recorded sandbox residual
/// ([`AgentAdapter::sandbox_residual_note`]), fired at most once per
/// process. Self-contained -- builds its own [`super::announce::Announcer`]
/// rather than requiring every call site to carry one of its own, the same
/// shape `poll::announce_keychain_prompt_once` uses for the identical "no
/// per-call state to carry a latch in" reason. A no-op whenever
/// `sandbox_residual_note` is `None` (claude today, and codex once its own
/// installed version supports `--ignore-rules --ignore-user-config` -- see
/// `CodexAdapter::sandbox_residual_note`).
///
/// `chrome_events_enabled` mirrors `cfg.chrome.events` -- the same "opt-outs
/// collapse to one boolean" contract every other `zirv ▸` line honors
/// (`--quiet`/`ZIRV_CTX_QUIET`/`[chrome] events = false`) -- passed by
/// production call sites that already have a resolved `CtxConfig` in scope
/// (`handoff::run_model`'s own callers, each individually, since `run_model`
/// itself is reused by claude and codex alike and must not assume which).
/// [`read_only_args_for_agent_name`] above has no `CtxConfig` in hand at
/// all and defaults to `true`, a recorded, narrow residual: an operator
/// whose only opt-out is `--quiet`/`ZIRV_CTX_QUIET` still sees this one
/// announcement on the workflow-reviewer path specifically. See Known
/// Issues.
pub fn announce_sandbox_residual_once(adapter: &dyn AgentAdapter, chrome_events_enabled: bool) {
    static ANNOUNCED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let Some(note) = adapter.sandbox_residual_note() else {
        return;
    };
    if !claim_once(&ANNOUNCED) {
        return;
    }
    super::announce::Announcer::new(chrome_events_enabled, console::colors_enabled_stderr())
        .emit(&super::announce::Event::SandboxResidual { note });
}

/// `true` the first time this specific latch flips from `false` to `true`,
/// `false` on every call after (including a concurrent caller: only one
/// `compare_exchange` wins). Extracted as its own pure function so the
/// "fires at most once" property is unit-testable against a caller-owned
/// `AtomicBool`, without needing to reset the process-wide static
/// `announce_sandbox_residual_once` actually uses between test runs (which
/// share one process and would otherwise contaminate each other -- the same
/// reason `poll::announce_keychain_prompt_once`/`config::announce_
/// unparsable_layers_once` have no dedicated "fires once" test of their
/// own today).
fn claim_once(latch: &std::sync::atomic::AtomicBool) -> bool {
    latch
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        )
        .is_ok()
}

/// Static adapter lookup for [`AgentAdapter::native_artifact_presentation`].
/// This does not require an installed/ready harness: presentation support is
/// adapter metadata, while the caller separately applies enablement and
/// canonical policy.
pub fn native_artifact_presentation_for_agent_name(
    name: &str,
    path: &Path,
    interactive_required: bool,
) -> Option<&'static str> {
    ADAPTERS
        .iter()
        .find(|(adapter_name, _)| *adapter_name == name)
        .and_then(|(_, ctor)| ctor(None).native_artifact_presentation(path, interactive_required))
}

/// Final wave item 4: `provider_for_agent_name(cfg.agent)` alone gets an
/// *unset* `agent` wrong whenever `resolve_default` would not have landed on
/// the legacy provider -- an operator-disabled claude (home `.settings.toml`
/// or `ZIRV_AGENT_CLAUDE_ENABLED=false`, not a repo one) with codex enabled
/// and ready falls back straight to `LEGACY_USAGE_PROVIDER` ("anthropic")
/// with no `agent` name to derive anything more specific from, even though
/// `resolve_default`'s own fallback loop would correctly skip claude and
/// land on codex. Tried first here for exactly that reason: `resolve_
/// default` is the actual selection logic (gates, `ready()`, the repo-
/// narrowing guard), so when it succeeds its answer is authoritative.
/// `provider_for_agent_name` is the fallback for when it does not -- an
/// explicitly configured, repo-disabled agent (`resolve_default`'s
/// configured arm hard-refuses there) still needs a provider, and only
/// `provider_for_agent_name` can name one without requiring readiness.
pub fn provider_for_usage_readout(cfg: &CtxConfig) -> &'static str {
    resolve_default(cfg)
        .map(|(adapter, _origin)| adapter.provider())
        .unwrap_or_else(|_| provider_for_agent_name(cfg.agent.as_deref()))
}

/// `cfg.agent_bin` is one global override applied to *whichever* adapter is
/// selected (every `ctor(bin)` call in this module reuses the same value
/// regardless of the adapter name) -- there is no per-adapter binary
/// override. That is fine for a stub path, a wrapper script (`sh
/// /path/fake-codex-agent.sh`), or a differently located install of the
/// *same* agent, but a value whose own program basename names a *different*
/// registered adapter (`agent_bin = "/usr/local/bin/claude"` while `codex`
/// is what gets selected, most plausibly stale config left over from
/// switching agents) would launch that other agent's real binary dressed up
/// in the selected adapter's own argv shape -- codex's `exec <prompt>`
/// positional form handed to the real claude CLI, wrong account, wrong
/// safety model, and no error anywhere naming what happened. Checked by
/// basename only (extension stripped, case-insensitive), not full-path
/// identity: an operator who genuinely renamed a binary to something that
/// happens to collide with another adapter's own name gets the same
/// refusal, which is the conservative, name-the-problem-and-stop failure
/// mode this guard exists for.
///
/// Returns the *other* adapter's name when `bin`'s basename collides with
/// one that is not `selected`; `None` when `bin` is unset, names no
/// registered adapter at all (a stub/wrapper path, the common test and
/// wrapper-script shape), or names `selected` itself.
fn agent_bin_names_a_different_adapter(bin: Option<&str>, selected: &str) -> Option<&'static str> {
    let bin = bin?;
    let program = bin.split_whitespace().next()?;
    let stem = Path::new(program).file_stem()?.to_str()?;
    ADAPTERS.iter().find_map(|(name, _)| {
        (!name.eq_ignore_ascii_case(selected) && stem.eq_ignore_ascii_case(name)).then_some(*name)
    })
}

/// The clear, name-both-adapters refusal `agent_bin_names_a_different_
/// adapter` backs, shared by every `select`/`resolve_default` arm that is
/// about to return `selected` as the resolved adapter.
fn refuse_if_agent_bin_names_another_adapter(bin: Option<&str>, selected: &str) -> CtxResult<()> {
    if let Some(other) = agent_bin_names_a_different_adapter(bin, selected) {
        return Err(format!(
            "agent_bin '{}' names '{other}', not the selected agent '{selected}' -- refusing to \
             launch '{other}'s binary as if it were '{selected}'. Point agent_bin at a '{selected}' \
             install, or select '{other}' instead.",
            bin.unwrap_or_default()
        )
        .into());
    }
    Ok(())
}

/// The registry's names, each suffixed `(disabled)` when `gate` refuses it --
/// used by the unknown-name error so a mistyped `--agent` also shows which
/// known names are actually usable right now.
fn describe_known_adapters(gate: &crate::settings::AgentGate) -> String {
    ADAPTERS
        .iter()
        .map(|(name, _)| {
            if gate.is_enabled(name) {
                name.to_string()
            } else {
                format!("{name} (disabled)")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The adapters that are both gate-enabled and `ready()` right now, in
/// registry order. Used to spell out actual options in an error instead of a
/// single hardcoded name -- `wrap`'s undetected-command refusal in
/// particular, which used to say "pass --agent claude" no matter how many
/// adapters the registry actually held.
pub fn available_adapter_names(cfg: &CtxConfig) -> Vec<&'static str> {
    let bin = cfg.agent_bin.as_deref();
    ADAPTERS
        .iter()
        .filter(|(name, ctor)| cfg.agents.is_enabled(name) && ctor(bin).ready().is_ok())
        .map(|(name, _)| *name)
        .collect()
}

/// One line per registered adapter, describing whether an Orchestrator
/// session may delegate to it right now via `zirv agent <name> "<prompt>"`.
/// Rendered by a caller that already has `cfg` in hand (`wrap`, `chat`'s
/// dashboard orchestrator pane path), never by a Worker call site: the result
/// is folded into the composed prompt as the harness roster
/// (`prompt::PromptSource::Harnesses`), and a worker must not learn what it
/// could delegate to.
///
/// `current_adapter` is the resolved adapter running *this* session -- its
/// line is marked "(this session's harness)" instead of the `zirv agent`
/// invitation, since `HARNESS_PROMPT` frames delegation as going to *other*
/// harnesses; a session cannot review-round itself.
///
/// Walks `ADAPTERS` in registry order, same as `readiness_note`, but reports
/// per-adapter gate state too (`readiness_note` only ever speaks about
/// installed-but-not-ready or degraded adapters, never a disabled one): a
/// disabled adapter gets its own line naming where the disable came from
/// (`AgentState::location`) rather than silently vanishing from the roster,
/// so an operator reading the prompt can tell "not offered because disabled
/// in .zirv/.settings.toml" from "not offered because not installed".
///
/// `ready()` alone is fail-open for a binary that simply is not there (see
/// [`program_is_present`]'s own doc comment), so a `ready()`-Ok adapter is
/// checked again against the real filesystem before its line may claim
/// "ready" and hand out the `zirv agent` invitation -- otherwise it reads as
/// "not installed" instead, exactly like a genuinely unready one.
///
/// `cfg.agent_bin` is one global override (see `agent_bin_names_a_different_
/// adapter`'s own doc comment), so it is never handed to an adapter whose own
/// basename it does not name: every `ctor(bin)` call below used to reuse the
/// same `bin` for every adapter in the registry, which put a real `claude`
/// binary's presence verdict onto codex's line (and vice versa) whenever an
/// operator's `agent_bin` named one specific agent -- either wrongly
/// advertising `zirv agent codex` on the strength of claude's binary, or, for
/// a not-yet-real wrapper path, wrongly marking every *other* adapter "not
/// installed" too. `agent_bin_names_a_different_adapter` returning `Some`
/// means `bin`'s basename names a *different* registered adapter than the one
/// about to be built, so that adapter is built with `None` instead and its
/// presence is judged from its own default program name, exactly as if no
/// override were configured at all.
pub fn harness_prompt_lines(cfg: &CtxConfig, current_adapter: &str) -> Vec<String> {
    let bin = cfg.agent_bin.as_deref();
    let mut lines: Vec<String> = ADAPTERS
        .iter()
        .map(|(name, ctor)| {
            let name: &str = name;
            let is_self = name == current_adapter;
            let (enabled, location) = cfg
                .agents
                .states()
                .find(|(n, _)| *n == name)
                .map(|(_, s)| (s.enabled, s.location()))
                .unwrap_or((true, "default".to_string()));
            if !enabled {
                return format!("- {name}: disabled ({location})");
            }

            let adapter = if agent_bin_names_a_different_adapter(bin, name).is_some() {
                ctor(None)
            } else {
                ctor(bin)
            };
            match adapter.ready() {
                Ok(()) => {
                    let program = adapter.program();
                    if !program_is_present(program) {
                        return format!("- {name}: not installed (no '{program}' found)");
                    }
                    let missing = missing_capability_labels(adapter.capabilities());
                    let degraded = if missing.is_empty() {
                        String::new()
                    } else {
                        format!(" (degraded: no {})", join_with_or(&missing))
                    };
                    // Repo `.settings.toml` (or the operator, or the
                    // environment) may mark a harness capacity-limited; the
                    // roster line carries that forward so an orchestrator
                    // routes only small, bounded briefs its way -- both for
                    // reviews and for `zirv agent` delegations (see
                    // `HARNESS_PROMPT`'s final paragraph).
                    let capacity_note = if cfg.agents.is_capacity_small(name) {
                        " -- small tasks only"
                    } else {
                        ""
                    };
                    if is_self {
                        format!(
                            "- {name}: enabled, ready{capacity_note} (this session's harness){degraded}"
                        )
                    } else {
                        format!(
                            "- {name}: enabled, ready{capacity_note} -- initiate with `zirv agent {name} \"<prompt>\"`{degraded}"
                        )
                    }
                }
                Err(err) => {
                    let reason = err.to_string();
                    let short = reason.lines().next().unwrap_or(&reason);
                    format!("- {name}: installed? not ready ({short})")
                }
            }
        })
        .collect();
    if let Some(review_line) = review_roster_line(cfg) {
        lines.push(review_line);
    }
    lines
}

/// The resolved review-model choice for one enabled harness: either the
/// operator's own `cfg.review.<agent>` value, or `adapter`'s own
/// `AgentAdapter::review_model_below` ladder default computed from the
/// orchestrator seat (`cfg.chat.model`, or the top tier when unset). This is
/// the one place both halves are combined -- `review_roster_line` below is
/// its only caller.
struct ReviewModelChoice {
    model: String,
    configured: bool,
}

fn resolve_review_model(
    cfg: &CtxConfig,
    name: &str,
    adapter: &dyn AgentAdapter,
) -> ReviewModelChoice {
    let configured = match name {
        "claude" => cfg.review.claude.as_deref(),
        "codex" => cfg.review.codex.as_deref(),
        _ => None,
    };
    if let Some(model) = configured {
        return ReviewModelChoice {
            model: model.to_string(),
            configured: true,
        };
    }
    ReviewModelChoice {
        model: adapter
            .review_model_below(cfg.chat.model.as_deref())
            .to_string(),
        configured: false,
    }
}

/// The resolved `worker.<name>` model for a delegated headless worker: the
/// operator's own `cfg.worker.<name>` value if set, else `adapter`'s own
/// `AgentAdapter::default_worker_model`. `None` means neither exists, so a
/// delegation spawn adds no `--model` flag at all and the agent's own
/// configuration picks (codex, with no `worker.codex` set). Unlike
/// `resolve_review_model` above, there is no ladder to fall back to: a
/// delegated worker has no orchestrator seat of its own to be "one tier
/// below", so the adapter-owned default is a fixed model name, not a
/// function of `cfg.chat.model`.
fn resolve_worker_model<'a>(
    cfg: &'a CtxConfig,
    name: &str,
    adapter: &'a dyn AgentAdapter,
) -> Option<&'a str> {
    let configured = match name {
        "claude" => cfg.worker.claude.as_deref(),
        "codex" => cfg.worker.codex.as_deref(),
        _ => None,
    };
    configured.or_else(|| adapter.default_worker_model())
}

/// Argv tokens (`AgentAdapter::model_args`) for the resolved worker model, or
/// empty when `resolve_worker_model` resolves nothing. The one place a
/// delegation spawn (`zirv ctx agent`'s own headless path in `agent.rs`, and
/// the dashboard's own spawn-request pane variant in `dash/mod.rs`) turns the
/// resolved model into a flag; neither caller applies this when the
/// operator's own trailing flags already pin a model explicitly (see each
/// caller's own doc comment for why that check lives there and not here).
pub fn worker_model_args(cfg: &CtxConfig, name: &str, adapter: &dyn AgentAdapter) -> Vec<String> {
    match resolve_worker_model(cfg, name, adapter) {
        Some(model) => adapter.model_args(model),
        None => Vec::new(),
    }
}

/// The argv `policy_launch_args` prepends ahead of an operator's own
/// trailing flags, at every real-launch seam this codebase builds
/// (`agent.rs::worker_launch_flags`, `exec.rs`, `run_loop.rs`, `wrap.rs`,
/// `chat.rs::dash_orchestrator_pane`, `dash::mod::fulfill_spawn_request`,
/// `handover.rs::resolve_swap_launch`) -- the one function all seven call,
/// so "operator's own choice always wins" and the shipped-default posture
/// can never drift between seams.
///
/// `Vec::new()` when `flags_pin_policy(flags)`: the operator's own explicit
/// flag wins outright, nothing of zirv's own is prepended at all. Otherwise:
/// `adapter.default_sandbox_args()` when `cfg.sandbox.enabled` (the shipped
/// default -- see that method's own doc comment for the exact posture),
/// followed by `adapter.policy_args(&cfg.policy, mode)` for any *additional*
/// restriction an explicit `[policy]` `Deny` stance asks for on top of the
/// baseline. The more specific choice comes last, so it wins if it overlaps
/// with the baseline (both adapters' relevant flags are single-value; the
/// underlying CLI takes the last occurrence).
pub fn policy_launch_args(
    cfg: &CtxConfig,
    adapter: &dyn AgentAdapter,
    flags: &[String],
    mode: LaunchMode,
) -> Vec<String> {
    if flags_pin_policy(flags) {
        return Vec::new();
    }
    let mut out = if cfg.sandbox.enabled {
        adapter.default_sandbox_args(&cfg.sandbox, &cfg.safety, mode)
    } else {
        Vec::new()
    };
    out.extend(adapter.policy_args(&cfg.policy, mode));
    out
}

/// The trailing "- code review: ..." line `harness_prompt_lines` appends
/// after its per-harness lines: names every *enabled* harness's resolved
/// review model (an operator override or the ladder default, each marked as
/// such) and states the rule that outranks any other model-routing guidance
/// a session's own base prompt carries (see `ORCHESTRATOR_PROMPT`'s
/// model-routing bullet in claude.rs, which now points back at this line).
/// Returns `None` when no harness is enabled at all -- absence, not a line
/// naming zero harnesses.
///
/// A disabled harness's entry is simply absent, the same "absence, not
/// silence" rule its own per-harness line above follows -- readiness is
/// deliberately not checked here (unlike the per-harness lines): the rule
/// this line states applies to a harness the moment it is enabled, whether
/// or not its binary happens to be on disk on this machine right now.
///
/// The rendered sentence must never be false for any entry. Two cases would
/// otherwise make it false: a harness whose ladder default is already at
/// the floor tier (seat "haiku" resolves claude's own default to "haiku"
/// too -- neither "one tier below the seat" nor "never on the seat's own
/// model" holds), and an operator who explicitly configures `review.<agent>`
/// equal to the seat (allowed -- the operator's choice wins -- but then
/// "never on the seat's own model" is false of that entry). Both are the
/// same underlying condition -- the resolved model's text equals the seat's
/// text, case-insensitively -- so both are detected by one `equals_seat`
/// check per entry, regardless of whether the model came from the ladder
/// default or an operator override. (Deliberately a plain text comparison,
/// not a second call into the ladder: re-running `review_model_below` on
/// the *resolved* model would also self-map at the floor tier for a seat
/// one rung *above* the floor -- e.g. seat "sonnet" resolves to "haiku",
/// and "haiku" maps to itself too -- which would wrongly flag a perfectly
/// true "one tier below the seat" note as a floor case.) When any entry's
/// `equals_seat` is true, the trailing clause softens from the strict
/// "never on an orchestrator seat's own model" to the weaker but always-true
/// "never on a model above the named one" (the named model is by
/// construction never ranked above the seat, so this holds in every case).
fn review_roster_line(cfg: &CtxConfig) -> Option<String> {
    let bin = cfg.agent_bin.as_deref();
    let seat = cfg.chat.model.as_deref();
    let mut any_equals_seat = false;
    let entries: Vec<String> = ADAPTERS
        .iter()
        .filter(|(name, _)| cfg.agents.is_enabled(name))
        .map(|(name, ctor)| {
            let adapter = if agent_bin_names_a_different_adapter(bin, name).is_some() {
                ctor(None)
            } else {
                ctor(bin)
            };
            let choice = resolve_review_model(cfg, name, adapter.as_ref());
            let equals_seat = seat.is_some_and(|s| s.eq_ignore_ascii_case(&choice.model));
            if equals_seat {
                any_equals_seat = true;
            }
            let note = if choice.configured {
                "configured".to_string()
            } else if equals_seat {
                "floor tier: the seat is already at the bottom rung".to_string()
            } else {
                "default: one tier below the seat".to_string()
            };
            format!("{name} -> \"{}\" ({note})", choice.model)
        })
        .collect();
    if entries.is_empty() {
        return None;
    }
    let never_clause = if any_equals_seat {
        "never on a model above the named one"
    } else {
        "never on an orchestrator seat's own model"
    };
    Some(format!(
        "- code review: {} -- run every code review on the named model, {never_clause}. This \
         outranks any other model-routing guidance.",
        entries.join(", ")
    ))
}

/// A `Capabilities` predicate paired with its user-facing label -- factored
/// out purely to keep `CAPABILITY_LABELS`'s type simple enough for clippy's
/// `type_complexity` lint.
type CapabilityLabel = (fn(Capabilities) -> bool, &'static str);

/// The user-facing label for each `Capabilities` flag this disclosure cares
/// about, in a fixed reporting order. `marker_signal` is deliberately not
/// included: it is a sub-feature of `events` (no event parsing means no
/// marker detection either), so listing both would say the same thing twice.
const CAPABILITY_LABELS: &[CapabilityLabel] = &[
    (|c| c.events, "rot score"),
    (|c| c.token_usage, "usage"),
    (|c| c.turn_signal, "turn signal"),
    (|c| c.system_prompt, "injected prompt"),
];

/// Which of [`CAPABILITY_LABELS`] this adapter's `capabilities()` reports as
/// missing, in the same fixed order.
fn missing_capability_labels(caps: Capabilities) -> Vec<&'static str> {
    CAPABILITY_LABELS
        .iter()
        .filter(|(has, _)| !has(caps))
        .map(|(_, label)| *label)
        .collect()
}

/// `["a", "b", "c"]` -> `"a, b, or c"`; `["a", "b"]` -> `"a or b"`; `["a"]` ->
/// `"a"`. Plain English list join for a short, human-readable sentence.
fn join_with_or(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => (*one).to_string(),
        [first, second] => format!("{first} or {second}"),
        _ => {
            let (last, rest) = items.split_last().expect("non-empty, matched above");
            format!("{}, or {last}", rest.join(", "))
        }
    }
}

/// A short clause naming every adapter that is not ready yet, plus one
/// naming every *ready* adapter whose own `capabilities()` still leaves its
/// launches degraded (no rot score, usage, turn signal, or injected
/// system prompt) -- for `zirv ctx --help`'s `about` text. Both halves are
/// generated from each adapter's own `ready()`/`capabilities()` rather than
/// hardcoded, so a newly wired-up adapter (or one that later closes a
/// capability gap) falls in or out of the sentence on its own. Empty only
/// once every adapter is both ready and fully capable.
///
/// Codex is the adapter this currently discloses: `ready()` no longer
/// hard-errors (its shim gap and `resolve_program` routing are closed, see
/// [[Known Issues]] via CLAUDE.md), but its `capabilities()` is still
/// honestly all-`false` -- `--agent codex` works, silently missing the four
/// things claude gets for free, which a user reading `--help` deserves to
/// see stated plainly rather than only discovering by surprise.
pub fn readiness_note() -> String {
    let mut clauses: Vec<String> = Vec::new();

    // Item 11: each adapter is constructed and `ready()`-checked exactly
    // once here, in one pass -- the two-pass version used to build a fresh
    // adapter and re-call `ready()` a second time for every adapter, once
    // per clause. `ctx_about()`'s `OnceLock` already caps this to once per
    // process, but the hook/statusline path still goes through it on every
    // invocation before that cache is warm.
    let mut not_ready: Vec<&str> = Vec::new();
    let mut degraded: Vec<String> = Vec::new();
    for (name, ctor) in ADAPTERS {
        let adapter = ctor(None);
        if adapter.ready().is_err() {
            not_ready.push(name);
            continue;
        }
        let missing = missing_capability_labels(adapter.capabilities());
        if !missing.is_empty() {
            degraded.push(format!(
                "{name} (launch-level: no {})",
                join_with_or(&missing)
            ));
        }
    }

    if !not_ready.is_empty() {
        clauses.push(format!(
            "Not ready yet: {} (see issue #11).",
            not_ready.join(", ")
        ));
    }
    if !degraded.is_empty() {
        clauses.push(format!(
            "Degraded surface: {} (see issue #11).",
            degraded.join("; ")
        ));
    }

    clauses.join(" ")
}

/// Which rule picked the default adapter, for callers (`zirv ctx status`,
/// diagnostics) that want to explain the choice rather than just use it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultOrigin {
    /// `cfg.agent` named it explicitly.
    Configured,
    /// No configured agent; this was the first adapter in registry order
    /// that was both gate-enabled and `ready()`.
    FirstEnabledReady,
}

/// Resolves the adapter `select` falls back to when neither an explicit
/// `--agent` nor detection named one: `cfg.agent` if set, else the first
/// registry entry that is both gate-enabled and `ready()`. Every call site
/// of `select` already folds `cfg.agent` into the `name` it passes in, so by
/// the time `select`'s fallback arm calls this, `cfg.agent` is always `None`
/// there -- but this function stands on its own (and is tested that way),
/// since a `None` name is not the only way to reach "use the configured or
/// default agent".
///
/// When nothing qualifies, the error aggregates one line per adapter naming
/// why it was skipped, reusing the gate's own refusal text and each
/// adapter's own `ready()` text rather than inventing new wording.
///
/// G: a repo checkout's own `.settings.toml` may narrow this fallback (take
/// an adapter off the table) but must never *select* a different one for the
/// operator as a side effect of that narrowing -- a repo-only disable
/// (`AgentGate::disabled_only_by_repo`) that would otherwise leave the
/// fallback silently landing on a different, still-enabled adapter refuses
/// instead, naming both adapters and the fix. Skipping past a repo-disabled
/// adapter when *nothing else* qualifies either is unaffected: no different
/// provider was ever silently chosen, so the ordinary aggregate error still
/// applies and still names every candidate.
///
/// G2 (fix): `repo_narrowed` is only recorded when the repo-disabled adapter
/// would *also* have passed `ready()` -- otherwise it was never a candidate
/// this fallback could have landed on in the first place (an unlaunchable
/// bare name, say), and the refusal's own claim that it "would otherwise
/// have been the default agent" would be false. Without this, disabling an
/// already-unlaunchable adapter via `.settings.toml` could still block a
/// perfectly good fallback to the next one, over a hypothetical that was
/// never true.
pub fn resolve_default(cfg: &CtxConfig) -> CtxResult<(Box<dyn AgentAdapter>, DefaultOrigin)> {
    let bin = cfg.agent_bin.as_deref();

    if let Some(name) = cfg.agent.as_deref() {
        let adapter = ADAPTERS
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, ctor)| ctor(bin))
            .ok_or_else(|| {
                format!(
                    "unknown agent '{name}'; known adapters: {}",
                    describe_known_adapters(&cfg.agents)
                )
            })?;
        if let Some(refusal) = cfg.agents.refusal(adapter.name()) {
            return Err(refusal.into());
        }
        adapter.ready()?;
        refuse_if_agent_bin_names_another_adapter(bin, adapter.name())?;
        return Ok((adapter, DefaultOrigin::Configured));
    }

    let mut reasons = Vec::new();
    let mut repo_narrowed: Option<&str> = None;
    for (name, ctor) in ADAPTERS {
        let adapter = ctor(bin);
        if let Some(refusal) = cfg.agents.refusal(name) {
            // Final wave item 3: the same cross-adapter skip Medium 2 gave
            // the enabled-and-ready arm below, applied here too. Without
            // it, `ctor(bin)` on this line always builds the candidate
            // with the *global* `agent_bin`, even when `agent_bin` names a
            // different adapter entirely -- so `adapter.ready()` could
            // report "ready" for, say, a claude adapter whose `program` is
            // actually pointed at a real codex binary. That is not a
            // candidate this fallback could ever have genuinely landed on
            // (the cross-adapter guard would refuse it exactly the way
            // Medium 2 does below), so recording `repo_narrowed` from it
            // would refuse on a false premise: "claude would otherwise
            // have been the default agent" when `agent_bin` never actually
            // named claude's own binary at all.
            if repo_narrowed.is_none()
                && cfg.agents.disabled_only_by_repo(name)
                && agent_bin_names_a_different_adapter(bin, name).is_none()
                && adapter.ready().is_ok()
            {
                repo_narrowed = Some(name);
            }
            reasons.push(format!("{name}: {refusal}"));
            continue;
        }
        match adapter.ready() {
            Ok(()) => {
                if let Some(narrowed) = repo_narrowed {
                    return Err(format!(
                        "the repository checkout disabled '{narrowed}' via .settings.toml, \
                         which would otherwise have been the default agent; a repo may narrow \
                         this fallback but not choose '{name}' for you instead. Pass --agent \
                         explicitly, or set `agent` in your own operator config or environment, \
                         to pick one."
                    )
                    .into());
                }
                // Medium 2: recorded and skipped, not `?`-aborted. `bin`
                // is one value tried against *every* candidate in this
                // loop in registry order -- if it names a different
                // adapter than this one (`name`), the right answer is to
                // keep walking to the adapter it actually does name, not
                // to abort the whole fallback here. An operator with no
                // `agent =` configured, only `agent_bin` pointing at a
                // real codex install, used to get a hard error at claude
                // (first in registry order) instead of landing on codex.
                // The explicit-`--agent` arm above still hard-refuses:
                // there the operator named the mismatch directly, so
                // there is nothing left to fall back to.
                if let Some(other) = agent_bin_names_a_different_adapter(bin, name) {
                    reasons.push(format!("{name}: agent_bin names '{other}', not '{name}'"));
                    continue;
                }
                return Ok((adapter, DefaultOrigin::FirstEnabledReady));
            }
            Err(e) => reasons.push(format!("{name}: {e}")),
        }
    }
    Err(format!(
        "no agent is both enabled and ready:\n{}",
        reasons.join("\n")
    )
    .into())
}

/// Explicit `--agent` name, else detection from the wrapped argv, else
/// `resolve_default`. The `.settings.toml` gate (`cfg.agents`) is checked
/// before `ready()` in every arm: `ready()` reports implementation state,
/// the gate reports operator policy, and a disabled agent must report the
/// disable rather than (for codex) "not implemented yet".
pub fn select(
    name: Option<&str>,
    command: &[String],
    cfg: &CtxConfig,
) -> CtxResult<Box<dyn AgentAdapter>> {
    let bin = cfg.agent_bin.as_deref();
    let adapters = all(bin);

    if let Some(name) = name {
        let found = adapters.into_iter().find(|a| a.name() == name);
        let adapter = found.ok_or_else(|| {
            format!(
                "unknown agent '{name}'; known adapters: {}",
                describe_known_adapters(&cfg.agents)
            )
        })?;
        if let Some(refusal) = cfg.agents.refusal(adapter.name()) {
            return Err(refusal.into());
        }
        adapter.ready()?;
        refuse_if_agent_bin_names_another_adapter(bin, adapter.name())?;
        return Ok(adapter);
    }

    if let Some(adapter) = adapters.into_iter().find(|a| a.detect(command)) {
        if let Some(refusal) = cfg.agents.refusal(adapter.name()) {
            return Err(refusal.into());
        }
        adapter.ready()?;
        refuse_if_agent_bin_names_another_adapter(bin, adapter.name())?;
        return Ok(adapter);
    }

    resolve_default(cfg).map(|(adapter, _origin)| adapter)
}

/// True when the wrapped command can be trusted to actually be this adapter's
/// agent: either the operator named it explicitly (`--agent`, or the config's
/// `agent` key), or detection matched the command's own argv. Neither true
/// means `select`'s last arm defaulted here with nothing to back it up (an
/// arbitrary wrapped command that matches no adapter), and injecting this
/// adapter's own flags (e.g. `--append-system-prompt`) into whatever program
/// that turns out to be would leak them into its output instead of an agent
/// that would ever read them.
pub fn command_matches_adapter(
    adapter: &dyn AgentAdapter,
    agent_explicit: bool,
    command: &[String],
) -> bool {
    agent_explicit || adapter.detect(command)
}

/// The canonicalised git "common dir" that owns `path` -- the shared `.git`
/// directory a plain repo and every `git worktree add`-linked sibling of it
/// all point back at -- or `None` if `git` is missing, `path` is not inside a
/// git working tree, or the process exits non-zero. Best-effort and
/// shell-out only, same precedent as `compile::changed_repo_paths`.
///
/// `git rev-parse --git-common-dir` prints a path RELATIVE to `path` for a
/// main worktree (typically just `.git`) but an ABSOLUTE one for a linked
/// worktree (it points back at the main checkout's `.git`). Both forms are
/// resolved against `path` before canonicalising, so a main worktree and any
/// of its linked siblings canonicalise to the exact same `PathBuf` even
/// though git reports the two differently.
///
/// Code review (issue #119, round 2): this used to back an authorization
/// check in `dash/mod.rs` alone (its answer decided whether a spawn request
/// got to run a real agent), so it must not trust an inherited environment
/// that a request's own process could have set. `GIT_DIR`/`GIT_COMMON_DIR`/
/// `GIT_WORK_TREE` (and `GIT_INDEX_FILE`, for the same family of override)
/// all redirect where `git` looks for repo state regardless of `-C`'s
/// argument; left inherited, any one of them set in the calling process
/// would make `git` resolve to the SAME overridden value for two genuinely
/// unrelated paths. Stripped here, at the one seam that shells out to `git`
/// for this decision, rather than trusted to already be absent from the
/// caller's environment -- a property every caller inherits for free,
/// including `CodexAdapter::extra_writable_root_args` below.
///
/// Moved here from `dash/mod.rs` (2026-08-26, codex approval-posture round):
/// `dash` already imports `adapters` (`policy_launch_args`, `LaunchMode`,
/// ...), so `adapters` calling back into `dash` would be a cycle -- this is
/// the shared home the one-directional import graph demands, used by
/// `dash::accepted_spawn_cwd`'s eligibility check (issue #119) and by
/// `CodexAdapter::extra_writable_root_args`'s writable-root computation
/// (same issue, the other half: eligibility says a linked worktree pane may
/// run, this says its shared git dir must actually be writable once it does).
pub(crate) fn git_common_dir(path: &Path) -> Option<PathBuf> {
    let output = std::process::Command::new("git")
        .env_remove("GIT_DIR")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .arg("-C")
        .arg(path)
        .arg("rev-parse")
        .arg("--git-common-dir")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let candidate = PathBuf::from(raw);
    let resolved = if candidate.is_absolute() {
        candidate
    } else {
        path.join(candidate)
    };
    std::fs::canonicalize(&resolved).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Issue "codex approval hell" (2026-08-26): before this, `-c
    /// approval_policy=...`/`-c sandbox_mode=...` were invisible to
    /// `flags_pin_policy`, so zirv's own computed config-override fallback
    /// (`CodexAdapter::approval_suppression_args`) still landed after an
    /// operator's own override and won outright (codex's config resolution
    /// is last-value-wins). Both the split (`-c`/`--config` plus a following
    /// `key=value` token) and the `=`-joined single-token forms must pin.
    #[test]
    fn flags_pin_policy_recognizes_codex_config_overrides_of_approval_or_sandbox_mode() {
        assert!(flags_pin_policy(&[
            "-c".to_string(),
            "approval_policy=on-request".to_string()
        ]));
        assert!(flags_pin_policy(&[
            "-c".to_string(),
            "sandbox_mode=read-only".to_string()
        ]));
        assert!(flags_pin_policy(&[
            "--config".to_string(),
            "approval_policy=never".to_string()
        ]));
        assert!(flags_pin_policy(&[
            "--config".to_string(),
            "sandbox_mode=workspace-write".to_string()
        ]));
        assert!(flags_pin_policy(&[
            "--config=approval_policy=on-request".to_string()
        ]));
        assert!(flags_pin_policy(&[
            "--config=sandbox_mode=danger-full-access".to_string()
        ]));
    }

    /// The bug this round fixes: codex-cli 0.149.1 also accepts `-c`'s
    /// argument attached to the flag itself with no space (`-cKEY=VALUE`),
    /// mirroring the attached short form `classify_model_flag` already
    /// recognises for `-m` (`-mopus`). Before this, only the split
    /// (`-c`/`--config` plus a following token) and `--config=`-joined forms
    /// were recognised, so an operator spelling their override as
    /// `-capproval_policy=on-request` was invisible to `flags_pin_policy` and
    /// zirv's own computed prefix still landed after it.
    #[test]
    fn flags_pin_policy_recognizes_codexs_attached_short_config_override_form() {
        assert!(flags_pin_policy(&[
            "-capproval_policy=on-request".to_string()
        ]));
        assert!(flags_pin_policy(&["-csandbox_mode=read-only".to_string()]));
        // Precision still matters: an attached `-c` naming an unrelated key
        // must not false-positive, and `-c` itself (no attached value) is the
        // ordinary split form, already covered above.
        assert!(!flags_pin_policy(&["-cmodel=gpt-5.6-sol".to_string()]));
    }

    /// A bare `-c`/`--config` with no following token, or one overriding an
    /// unrelated key, must not false-positive: this function's whole
    /// contract is precision over coverage (see its own doc comment), and a
    /// false positive here means silently *withholding* zirv's restriction.
    #[test]
    fn flags_pin_policy_ignores_unrelated_or_dangling_config_overrides() {
        assert!(!flags_pin_policy(&[
            "-c".to_string(),
            "model=gpt-5.6-sol".to_string()
        ]));
        assert!(!flags_pin_policy(&["-c".to_string()]));
        assert!(!flags_pin_policy(&["--config".to_string()]));
        assert!(!flags_pin_policy(&[]));
    }

    /// One dimension pins the whole prefix, the same all-or-nothing
    /// granularity a bare operator `--sandbox`/`--ask-for-approval` flag
    /// already has (see the function's own doc comment): a `-c
    /// approval_policy=...` override alone, with no accompanying
    /// `sandbox_mode` override, still withholds zirv's entire computed
    /// prefix rather than just the approval half.
    #[test]
    fn flags_pin_policy_config_override_pins_the_whole_prefix_not_just_one_dimension() {
        let flags = vec!["-c".to_string(), "approval_policy=on-request".to_string()];
        assert!(flags_pin_policy(&flags));
        let cfg = CtxConfig::default();
        let codex = codex::CodexAdapter::new(None)
            .with_on_request_approval_forced(true)
            .with_exec_ask_for_approval_forced(true);
        assert!(
            policy_launch_args(&cfg, &codex, &flags, LaunchMode::Headless).is_empty(),
            "an operator's own -c approval_policy=... override must suppress zirv's entire \
             computed prefix, not just the approval flag"
        );
    }

    /// The interactive/headless seam itself (2026-08-24, cross-harness
    /// permissions): the enum every real-launch call site now has to answer
    /// with. Landing the parameter with no behaviour change is deliberate --
    /// the compiler forces all seven seams to state their own posture
    /// before any task actually branches on it.
    #[test]
    fn launch_mode_names_the_two_postures_the_projection_splits_on() {
        assert_eq!(LaunchMode::Interactive.label(), "interactive");
        assert_eq!(LaunchMode::Headless.label(), "headless");
        assert!(LaunchMode::Interactive.is_interactive());
        assert!(!LaunchMode::Headless.is_interactive());
    }

    /// The Task 1 seam becomes load-bearing once Tasks 3 and 7 project the
    /// two postures differently. Pin that distinction at the shared seam,
    /// with codex's live capability probe forced out of the assertion.
    #[test]
    fn launch_mode_projects_the_two_postures_differently() {
        let cfg = CtxConfig::default();
        let claude = claude::ClaudeAdapter::new(None);
        let interactive = policy_launch_args(&cfg, &claude, &[], LaunchMode::Interactive);
        let headless = policy_launch_args(&cfg, &claude, &[], LaunchMode::Headless);
        assert_ne!(interactive, headless);
        assert!(
            interactive
                .windows(2)
                .any(|w| w == ["--permission-mode", "default"])
        );
        assert!(
            headless
                .windows(2)
                .any(|w| w == ["--permission-mode", "dontAsk"])
        );

        let codex = codex::CodexAdapter::new(None)
            .with_on_request_approval_forced(true)
            .with_exec_ask_for_approval_forced(true);
        let interactive = policy_launch_args(&cfg, &codex, &[], LaunchMode::Interactive);
        let headless = policy_launch_args(&cfg, &codex, &[], LaunchMode::Headless);
        assert_ne!(interactive, headless);
        assert!(
            interactive
                .windows(2)
                .any(|w| w == ["--ask-for-approval", "on-request"])
        );
        assert!(
            headless
                .windows(2)
                .any(|w| w == ["--ask-for-approval", "never"])
        );
    }

    /// A permissive `CtxConfig` (every agent enabled, no `agent_bin`
    /// override) for tests that only care about selection, not gating.
    /// `CtxConfig::default()` never touches the filesystem or `HOME` (its
    /// `AgentGate` is `AgentGate::default()`, not a `load`), so this one
    /// needs no `HomeGuard`, unlike `cfg_disabling` below.
    fn permissive_cfg() -> CtxConfig {
        CtxConfig::default()
    }

    /// A `CtxConfig` whose gate disables exactly one named agent, as if an
    /// operator or repo `.settings.toml` had set `[agents.<name>] enabled =
    /// false`, but without touching any file: `AgentGate`'s fields are
    /// crate-private, so the state is built by loading a real settings file
    /// from an isolated repo dir instead. `AgentGate::load` also reads the
    /// operator (home) layer, so this isolates `HOME`/`USERPROFILE` too --
    /// otherwise a developer machine's real `~/.zirv/.settings.toml` (if any)
    /// would leak into the loaded gate.
    fn cfg_disabling(name: &str) -> CtxConfig {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/.settings.toml"),
            format!("[agents.{name}]\nenabled = false\n"),
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        CtxConfig {
            agents: crate::settings::AgentGate::load(repo.path(), &|k| empty.get(k).cloned())
                .expect("load"),
            ..CtxConfig::default()
        }
    }

    #[test]
    fn join_with_or_reads_like_plain_english() {
        assert_eq!(join_with_or(&[]), "");
        assert_eq!(join_with_or(&["a"]), "a");
        assert_eq!(join_with_or(&["a", "b"]), "a or b");
        assert_eq!(join_with_or(&["a", "b", "c"]), "a, b, or c");
    }

    #[test]
    fn missing_capability_labels_names_only_the_false_flags() {
        let all_false = Capabilities::default();
        assert_eq!(
            missing_capability_labels(all_false),
            vec!["rot score", "usage", "turn signal", "injected prompt"]
        );

        let all_true = Capabilities {
            marker_signal: true,
            token_usage: true,
            turn_signal: true,
            system_prompt: true,
            events: true,
            defer_injection_submit: true,
            context_window_tokens: None,
        };
        assert!(missing_capability_labels(all_true).is_empty());

        let mixed = Capabilities {
            events: true,
            ..Capabilities::default()
        };
        assert_eq!(
            missing_capability_labels(mixed),
            vec!["usage", "turn signal", "injected prompt"],
            "an adapter with real events but nothing else"
        );
    }

    /// F: codex is ready (its own `ready()` no longer hard-errors) but its
    /// usage/turn capabilities remain degraded, so `--help`'s about text
    /// must keep disclosing the degraded surface even though codex no longer
    /// shows up in the "not ready yet" clause at all. Issue #86 (2026-08-23)
    /// gave codex real event parsing, so "rot score" is no longer one of the
    /// missing labels -- this must NOT regress back to claiming codex has no
    /// rot score.
    #[test]
    fn the_readiness_note_discloses_codexs_degraded_surface_now_that_it_is_ready() {
        let note = readiness_note();
        assert!(
            !note.to_lowercase().contains("not ready"),
            "codex is ready now, not unready: {note}"
        );
        assert!(note.contains("codex"), "got {note}");
        assert!(
            !note.contains("rot score"),
            "issue #86 gave codex real event parsing: {note}"
        );
        assert!(note.contains("usage"), "got {note}");
        assert!(note.contains("turn signal"), "got {note}");
        assert!(!note.contains("injected prompt"), "got {note}");
        assert!(note.contains("issue #11"), "got {note}");
        assert!(
            !note.contains("claude (launch-level"),
            "claude is fully capable and must not appear in the degraded clause: {note}"
        );
    }

    #[test]
    fn harness_prompt_lines_returns_one_line_per_registered_adapter() {
        let lines = harness_prompt_lines(&permissive_cfg(), "");
        // One line per adapter, plus one trailing "- code review: ..." line
        // naming every enabled harness's resolved review model.
        assert_eq!(lines.len(), ADAPTERS.len() + 1);
        for (name, _) in ADAPTERS {
            assert!(
                lines.iter().any(|l| l.starts_with(&format!("- {name}:"))),
                "missing a line for '{name}': {lines:?}"
            );
        }
        assert!(
            lines
                .last()
                .is_some_and(|l| l.starts_with("- code review:")),
            "the review line comes last: {lines:?}"
        );
    }

    /// Unconfigured `review.claude`/`review.codex`: the roster line names
    /// each enabled harness's ladder-computed default (one tier below the
    /// seat -- unset `chat.model` assumes the top tier), marks each entry as
    /// a default rather than an operator choice, and states the never-the-
    /// seat / outranks-other-routing rule.
    #[test]
    fn harness_prompt_lines_review_line_shows_computed_defaults_when_unset() {
        let lines = harness_prompt_lines(&permissive_cfg(), "");
        let review_line = lines.last().expect("at least the review line");
        assert!(
            review_line.contains("claude -> \"opus\" (default: one tier below the seat)"),
            "got {review_line}"
        );
        assert!(
            review_line.contains("codex -> \"gpt-5.6-terra\" (default: one tier below the seat)"),
            "got {review_line}"
        );
        assert!(
            review_line.contains("never on an orchestrator seat's own model"),
            "got {review_line}"
        );
        assert!(
            review_line.contains("outranks"),
            "states it outranks other routing guidance: {review_line}"
        );
    }

    /// An operator-configured `review.<agent>` wins over the ladder default
    /// and is marked `(configured)` rather than `(default: ...)`.
    #[test]
    fn harness_prompt_lines_review_line_uses_the_operators_configured_model() {
        let cfg = CtxConfig {
            review: crate::commands::ctx::config::ReviewConfig {
                claude: Some("custom-review-model".to_string()),
                codex: None,
            },
            ..permissive_cfg()
        };
        let lines = harness_prompt_lines(&cfg, "");
        let review_line = lines.last().expect("at least the review line");
        assert!(
            review_line.contains("claude -> \"custom-review-model\" (configured)"),
            "got {review_line}"
        );
        assert!(
            review_line.contains("codex -> \"gpt-5.6-terra\" (default: one tier below the seat)"),
            "codex stays on its computed default: {review_line}"
        );
    }

    /// A disabled harness gets no entry in the review line at all -- same
    /// absence-not-silence rule its own per-harness line above follows.
    #[test]
    fn harness_prompt_lines_review_line_omits_a_disabled_harnesses_entry() {
        let cfg = cfg_disabling("codex");
        let lines = harness_prompt_lines(&cfg, "");
        let review_line = lines.last().expect("at least the review line");
        assert!(
            review_line.contains("claude ->"),
            "claude stays: {review_line}"
        );
        assert!(
            !review_line.contains("codex ->"),
            "a disabled harness must not appear in the review line: {review_line}"
        );
    }

    /// Normal case (no entry's resolved model equals the orchestrator seat):
    /// the strict "never on an orchestrator seat's own model" clause is
    /// true for every entry, so it stays.
    #[test]
    fn review_roster_line_normal_case_keeps_the_strict_clause() {
        let lines = harness_prompt_lines(&permissive_cfg(), "");
        let review_line = lines.last().expect("at least the review line");
        assert!(
            review_line.contains("never on an orchestrator seat's own model"),
            "got {review_line}"
        );
        assert!(
            !review_line.contains("never on a model above the named one"),
            "the softened clause must not appear when nothing is contradictory: {review_line}"
        );
    }

    /// Floor-tier case: seat "haiku" resolves (unconfigured) to claude's own
    /// floor default "haiku" too -- neither "one tier below the seat" nor
    /// "never on an orchestrator seat's own model" would be true of that
    /// entry, so both the per-entry note and the global clause must adjust
    /// to stay honest.
    #[test]
    fn review_roster_line_floor_seat_case_is_not_contradictory() {
        let cfg = CtxConfig {
            chat: crate::commands::ctx::config::ChatConfig {
                model: Some("haiku".to_string()),
            },
            ..permissive_cfg()
        };
        let lines = harness_prompt_lines(&cfg, "");
        let review_line = lines.last().expect("at least the review line");
        assert!(
            review_line.contains("claude -> \"haiku\""),
            "got {review_line}"
        );
        assert!(
            !review_line.contains("claude -> \"haiku\" (default: one tier below the seat)"),
            "that note would be false when the seat is already the floor: {review_line}"
        );
        assert!(review_line.contains("floor tier"), "got {review_line}");
        assert!(
            !review_line.contains("never on an orchestrator seat's own model"),
            "that clause would be false for the floor-tier entry: {review_line}"
        );
    }

    /// Configured-equals-seat case: the operator's own `review.claude`
    /// explicitly names the same model as the orchestrator seat -- allowed
    /// (the operator's choice wins), but then "never on an orchestrator
    /// seat's own model" is false of that entry, so the global clause must
    /// soften to something that stays true.
    #[test]
    fn review_roster_line_configured_equals_seat_case_is_not_contradictory() {
        let cfg = CtxConfig {
            chat: crate::commands::ctx::config::ChatConfig {
                model: Some("opus".to_string()),
            },
            review: crate::commands::ctx::config::ReviewConfig {
                claude: Some("opus".to_string()),
                codex: None,
            },
            ..permissive_cfg()
        };
        let lines = harness_prompt_lines(&cfg, "");
        let review_line = lines.last().expect("at least the review line");
        assert!(
            review_line.contains("claude -> \"opus\" (configured)"),
            "got {review_line}"
        );
        assert!(
            !review_line.contains("never on an orchestrator seat's own model"),
            "that clause would be false for the operator's own configured entry: {review_line}"
        );
    }

    /// The seat threads all the way from `cfg.chat.model` through to the
    /// rendered claude entry: seat "sonnet" resolves claude's own ladder
    /// default to "haiku" (one tier below sonnet).
    #[test]
    fn harness_prompt_lines_review_line_threads_the_seat_for_claude() {
        let cfg = CtxConfig {
            chat: crate::commands::ctx::config::ChatConfig {
                model: Some("sonnet".to_string()),
            },
            ..permissive_cfg()
        };
        let lines = harness_prompt_lines(&cfg, "");
        let review_line = lines.last().expect("at least the review line");
        assert!(
            review_line.contains("claude -> \"haiku\" (default: one tier below the seat)"),
            "got {review_line}"
        );
    }

    /// No enabled harness at all: `review_roster_line` must not emit a
    /// line naming zero harnesses -- absence, not an empty-handed line.
    #[test]
    fn harness_prompt_lines_omits_the_review_line_when_no_harness_is_enabled() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/.settings.toml"),
            "[agents.claude]\nenabled = false\n[agents.codex]\nenabled = false\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let cfg = CtxConfig {
            agents: crate::settings::AgentGate::load(repo.path(), &|k| empty.get(k).cloned())
                .expect("load"),
            ..CtxConfig::default()
        };
        let lines = harness_prompt_lines(&cfg, "");
        assert!(
            !lines.iter().any(|l| l.starts_with("- code review:")),
            "no harness enabled: there must be no review line at all: {lines:?}"
        );
    }

    /// The one call site (`prompt::compose` for an Orchestrator session) must
    /// never learn about a disabled adapter as if it were offered for
    /// delegation: a disabled line names where the disable came from and
    /// never the `zirv agent <name>` invitation.
    #[test]
    fn harness_prompt_lines_names_the_disabled_adapter_and_its_location() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let env: std::collections::HashMap<String, String> =
            [("ZIRV_AGENT_CODEX_ENABLED".to_string(), "false".to_string())]
                .into_iter()
                .collect();
        let cfg = CtxConfig {
            agents: crate::settings::AgentGate::load(repo.path(), &|k| env.get(k).cloned())
                .expect("load"),
            ..CtxConfig::default()
        };

        let lines = harness_prompt_lines(&cfg, "");
        let codex_line = lines
            .iter()
            .find(|l| l.starts_with("- codex:"))
            .expect("codex line present");
        assert!(codex_line.contains("disabled"), "got {codex_line}");
        assert!(
            codex_line.contains("ZIRV_AGENT_CODEX_ENABLED"),
            "names the environment source: {codex_line}"
        );
        assert!(
            !codex_line.contains("zirv agent codex"),
            "a disabled adapter is never offered for delegation: {codex_line}"
        );
    }

    /// Finding 1: `ready()` alone is fail-open for a program that simply is
    /// not on disk anywhere -- `resolve_program` deliberately returns `Ok`
    /// for it (see its own doc comment), and several other call sites lean on
    /// that. `harness_prompt_lines` must not repeat the same fail-open claim
    /// in a roster line an orchestrator can act on immediately: a name that
    /// resolves to nothing must read as not installed, not ready.
    #[test]
    fn harness_prompt_lines_reports_not_installed_when_the_resolved_program_is_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("nonexistent-agent-binary");
        let cfg = CtxConfig {
            agent_bin: Some(missing.display().to_string()),
            ..permissive_cfg()
        };

        let lines = harness_prompt_lines(&cfg, "");
        let claude_line = lines
            .iter()
            .find(|l| l.starts_with("- claude:"))
            .expect("claude line present");
        assert!(claude_line.contains("not installed"), "got {claude_line}");
        assert!(
            !claude_line.contains("zirv agent"),
            "a binary that is not there must never be offered for delegation: {claude_line}"
        );
    }

    /// Bug A (harness/model parity): two harnesses in the identical state --
    /// here, both absent, behind the same missing `agent_bin` override --
    /// must render the identical templated line, differing only by their own
    /// name. No adapter may get softer or harsher wording than another for
    /// the same underlying fact; `harness_prompt_lines` renders every entry
    /// through one shared format, never an adapter-specific one, and this
    /// pins that down behaviourally rather than only by code inspection.
    #[test]
    fn harness_prompt_lines_render_the_same_template_for_two_adapters_in_the_same_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("nonexistent-agent-binary");
        let cfg = CtxConfig {
            agent_bin: Some(missing.display().to_string()),
            ..permissive_cfg()
        };

        let lines = harness_prompt_lines(&cfg, "");
        let claude_line = lines
            .iter()
            .find(|l| l.starts_with("- claude:"))
            .expect("claude line present");
        let codex_line = lines
            .iter()
            .find(|l| l.starts_with("- codex:"))
            .expect("codex line present");

        let normalize = |line: &str, name: &str| line.replacen(name, "{name}", 1);
        assert_eq!(
            normalize(claude_line, "claude"),
            normalize(codex_line, "codex"),
            "both adapters are equally absent and must render the identical template, \
             differing only by name:\nclaude: {claude_line}\ncodex: {codex_line}"
        );
    }

    /// Finding 1's positive case, plus Finding 4: a program that genuinely
    /// exists on disk is still offered for delegation, except when it is
    /// this session's own adapter, which is marked as such instead of
    /// inviting a session to delegate to itself.
    ///
    /// Each adapter's own default program name (no `agent_bin` override) is
    /// planted as a real stub on a PATH restricted to one temp dir, so
    /// codex's "ready" verdict here is earned by codex's own binary, never
    /// borrowed from an unrelated stub -- see the follow-up regression right
    /// below this test for the case where a *shared* override used to make
    /// that borrowing happen.
    #[test]
    fn harness_prompt_lines_offers_delegation_only_to_a_present_non_self_adapter() {
        let dir = tempfile::tempdir().expect("tempdir");
        for name in ["claude", "codex"] {
            std::fs::write(dir.path().join(name), "").expect("write stub");
        }
        let _path_guard = crate::commands::ctx::testenv::VarGuard::set(&[(
            "PATH",
            Some(dir.path().to_str().expect("utf8 tempdir path")),
        )]);
        let cfg = permissive_cfg();

        let lines = harness_prompt_lines(&cfg, "claude");
        let claude_line = lines
            .iter()
            .find(|l| l.starts_with("- claude:"))
            .expect("claude line present");
        assert!(
            claude_line.contains("this session's harness"),
            "got {claude_line}"
        );
        assert!(
            !claude_line.contains("zirv agent claude"),
            "a session never invites itself to delegate: {claude_line}"
        );

        let codex_line = lines
            .iter()
            .find(|l| l.starts_with("- codex:"))
            .expect("codex line present");
        assert!(
            codex_line.contains("zirv agent codex"),
            "a present, non-self adapter is still offered on the strength of its own binary: \
             {codex_line}"
        );
    }

    /// Item 1 regression: `harness_prompt_lines` used to build *every*
    /// adapter with the same global `agent_bin` override, so `agent_bin`
    /// naming claude's binary made codex's line borrow claude's presence
    /// verdict and falsely offer `zirv agent codex` -- a wasted delegation
    /// every review round, since `select` would go on to refuse it
    /// ("agent_bin names 'claude', not 'codex'"). With no `codex` binary
    /// anywhere on this test's restricted `PATH`, codex must read as not
    /// installed regardless of how present claude's own stub is.
    #[test]
    fn harness_prompt_lines_never_borrows_a_named_adapters_presence_for_another() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("claude"), "").expect("write stub");
        let _path_guard = crate::commands::ctx::testenv::VarGuard::set(&[(
            "PATH",
            Some(dir.path().to_str().expect("utf8 tempdir path")),
        )]);
        let cfg = CtxConfig {
            agent_bin: Some(dir.path().join("claude").display().to_string()),
            ..permissive_cfg()
        };

        let lines = harness_prompt_lines(&cfg, "claude");
        let claude_line = lines
            .iter()
            .find(|l| l.starts_with("- claude:"))
            .expect("claude line present");
        assert!(
            claude_line.contains("this session's harness"),
            "claude's own named override is present: {claude_line}"
        );

        let codex_line = lines
            .iter()
            .find(|l| l.starts_with("- codex:"))
            .expect("codex line present");
        assert!(
            !codex_line.contains("zirv agent codex"),
            "agent_bin naming claude must never make codex's line claim delegable: {codex_line}"
        );
        assert!(
            codex_line.contains("not installed"),
            "codex is judged on its own (absent) binary, not claude's override: {codex_line}"
        );
    }

    /// A capacity-limited harness's roster line gets the `-- small tasks
    /// only` suffix; an unmarked harness's line does not. This is the
    /// signal `HARNESS_PROMPT`'s final paragraph tells an orchestrator to
    /// route only small, bounded briefs by, for both reviews and `zirv
    /// agent` delegations.
    #[test]
    fn harness_prompt_lines_marks_a_capacity_limited_harness_small_tasks_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        for name in ["claude", "codex"] {
            std::fs::write(dir.path().join(name), "").expect("write stub");
        }
        let _path_guard = crate::commands::ctx::testenv::VarGuard::set(&[(
            "PATH",
            Some(dir.path().to_str().expect("utf8 tempdir path")),
        )]);

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/.settings.toml"),
            "[agents.codex]\ncapacity = \"small\"\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home_guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let cfg = CtxConfig {
            agents: crate::settings::AgentGate::load(repo.path(), &|k| empty.get(k).cloned())
                .expect("load"),
            ..CtxConfig::default()
        };

        let lines = harness_prompt_lines(&cfg, "claude");
        let codex_line = lines
            .iter()
            .find(|l| l.starts_with("- codex:"))
            .expect("codex line present");
        assert!(
            codex_line.contains("ready -- small tasks only"),
            "got {codex_line}"
        );

        let claude_line = lines
            .iter()
            .find(|l| l.starts_with("- claude:"))
            .expect("claude line present");
        assert!(
            !claude_line.contains("small tasks only"),
            "claude was never marked capacity-small: {claude_line}"
        );
    }

    /// M7 probed the adapter's own program while `wrap` spawned the user's
    /// argv, so the file flag could be handed to a binary that never
    /// advertised it -- failing the launch outright, which is the one thing
    /// the probe promises never to do. The probe target now comes from the
    /// argv about to be spawned, which means finding the invocation in it.
    #[test]
    fn the_program_invocation_stops_at_the_first_flag() {
        let argv =
            |parts: &[&str]| -> Vec<String> { parts.iter().map(|s| s.to_string()).collect() };

        assert_eq!(
            program_invocation(&argv(&["claude", "-p", "task"])),
            Some(("claude".to_string(), vec![]))
        );
        assert_eq!(
            program_invocation(&argv(&["/usr/bin/env", "claude", "-p", "task"])),
            Some(("/usr/bin/env".to_string(), vec!["claude".to_string()]))
        );
        assert_eq!(
            program_invocation(&argv(&["sh", "/opt/wrap.sh", "--model", "opus"])),
            Some(("sh".to_string(), vec!["/opt/wrap.sh".to_string()]))
        );
        assert_eq!(program_invocation(&[]), None, "nothing to probe");
    }

    /// Off Windows there is nothing to rewrite, and on Windows a program that
    /// is already directly executable is spawned exactly as it was written.
    #[test]
    fn a_directly_executable_program_is_left_alone() {
        let resolved = resolve_program("claude").expect("resolvable");
        assert_eq!(resolved.program, "claude");
        assert!(
            resolved.prefix.is_empty() || cfg!(windows),
            "only Windows ever inserts a launcher"
        );

        let missing = resolve_program("definitely-not-a-program-anywhere").expect("no error");
        assert_eq!(
            missing,
            ResolvedProgram::direct("definitely-not-a-program-anywhere"),
            "a program that resolves to nothing keeps the OS's own not-found"
        );
    }

    /// The npm install layout: `claude` on `PATH` is `claude.cmd`, which
    /// `CreateProcessW` rejects outright. `PATHEXT` finds it, and `cmd.exe`
    /// is what can actually run it.
    #[cfg(windows)]
    #[test]
    fn a_cmd_shim_is_rewritten_to_run_through_cmd_exe() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shim = dir.path().join("shim-agent.cmd");
        std::fs::write(&shim, "@echo off\r\n").expect("write");

        let resolved = resolve_program(&shim.display().to_string()).expect("resolvable");
        assert!(
            resolved.program.to_lowercase().contains("cmd"),
            "got {}",
            resolved.program
        );
        assert_eq!(
            resolved.prefix,
            vec!["/c".to_string(), shim.display().to_string()]
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_powershell_script_is_rewritten_to_run_through_powershell() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("shim-agent.ps1");
        std::fs::write(&script, "exit 0\r\n").expect("write");

        let resolved = resolve_program(&script.display().to_string()).expect("resolvable");
        assert_eq!(resolved.program, "powershell");
        assert_eq!(
            resolved.prefix,
            vec![
                "-NoProfile".to_string(),
                "-File".to_string(),
                script.display().to_string()
            ]
        );
    }

    /// A bare name resolved off `PATH` is one the shell itself claimed to be
    /// executable, so a file type with no launcher is a failure zirv can name
    /// before spawning instead of letting it surface as `os error 193`. A
    /// program written with a directory in it is the caller's own choice and
    /// is never an error here, whatever it ends in.
    #[cfg(windows)]
    #[test]
    fn an_unlaunchable_program_on_path_is_named_rather_than_left_to_error_193() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("shim-agent.py");
        std::fs::write(&script, "print('x')\n").expect("write");

        assert_eq!(
            resolve_program(&script.display().to_string()),
            Ok(ResolvedProgram::direct(&script.display().to_string())),
            "an explicit path is the caller's own choice"
        );

        // Temporarily put the directory on PATH so the bare name resolves the
        // way the shell would, with `.PY` advertised on PATHEXT.
        //
        // NEW-1: a guard, not a manual restore. The restore used to sit after
        // an `expect_err`, so a failing resolution left this process with a
        // mangled `PATH` and a `PATHEXT` of `.EXE;.CMD;.PY` -- the highest
        // blast radius of any leak in the suite, since every later test that
        // spawns anything resolves its program through both.
        let path = std::env::var("PATH").unwrap_or_default();
        let _path_guard = crate::commands::ctx::testenv::VarGuard::set(&[
            (
                "PATH",
                Some(format!("{};{}", dir.path().display(), path).as_str()),
            ),
            ("PATHEXT", Some(".EXE;.CMD;.PY")),
        ]);
        let err = resolve_program("shim-agent").expect_err("no launcher for .py");

        assert!(err.contains("shim-agent.py"), "the error names it: {err}");
        assert!(err.contains("shim-agent"), "and what was asked for: {err}");
    }

    /// H1/H2: both `readiness_note`'s "not ready yet" clause and `resolve_
    /// default`'s `Err(e) => continue` unready-skip branch lost their only
    /// coverage once codex's own `ready()` stopped hard-erroring -- nothing
    /// in the real registry is ever actually unready anymore, so a test that
    /// only reads the real `ADAPTERS` table can no longer exercise either
    /// branch at all. This forces claude's own bare `"claude"` name to
    /// resolve to an unlaunchable `.py` (the same PATH/PATHEXT rig `an_
    /// unlaunchable_program_on_path_is_named_rather_than_left_to_error_193`
    /// uses), which is the one real way `ready()` fails on this codebase,
    /// leaving codex genuinely unaffected (codex.cmd, wherever it resolves
    /// or fails to, is never a `ready()` error case) to prove the skip-and-
    /// continue path lands on it.
    #[cfg(windows)]
    #[test]
    fn readiness_note_and_the_fallback_skip_both_stay_covered_when_an_adapter_is_genuinely_unready()
    {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("claude.py"), "print('x')\n").expect("write");

        let path = std::env::var("PATH").unwrap_or_default();
        let _path_guard = crate::commands::ctx::testenv::VarGuard::set(&[
            (
                "PATH",
                Some(format!("{};{}", dir.path().display(), path).as_str()),
            ),
            ("PATHEXT", Some(".EXE;.CMD;.PY")),
        ]);

        // H1: the "not ready yet" clause is genuinely exercised again.
        let note = readiness_note();
        assert!(
            note.to_lowercase().contains("not ready"),
            "claude must be reported not ready under this rig: {note}"
        );
        assert!(note.contains("claude"), "got {note}");

        // H2: `resolve_default`'s fallback must skip claude's `Err` and land
        // on codex, exercising the `Err(e) => reasons.push(...); continue`
        // arm rather than the `Ok(())` one.
        let (adapter, origin) = resolve_default(&permissive_cfg()).expect("codex still qualifies");
        assert_eq!(adapter.name(), "codex");
        assert_eq!(origin, DefaultOrigin::FirstEnabledReady);
    }

    /// FIX 2a: a `cmd.exe /c <shim>` launch whose downstream arguments carry a
    /// cmd.exe metacharacter is refused, because cmd.exe re-parses that
    /// character as a command rather than passing it through to the shim. This
    /// is the RCE-closing guard, tested as a pure decision function -- no
    /// process is spawned.
    #[cfg(windows)]
    #[test]
    fn a_shim_form_launch_with_a_metachar_arg_is_refused() {
        let args = vec![
            "/c".to_string(),
            "C:\\tools\\claude.cmd".to_string(),
            "-p".to_string(),
            "foo&calc".to_string(),
        ];
        let err = guard_cmd_shim_reparse("cmd.exe", &args)
            .expect_err("a metachar after the shim path is command injection");
        assert!(
            err.contains("foo&calc"),
            "the error names the offending arg: {err}"
        );

        // A full-path, upper-cased COMSPEC is recognised structurally too.
        assert!(
            guard_cmd_shim_reparse(
                "C:\\Windows\\System32\\CMD.EXE",
                &[
                    "/C".to_string(),
                    "claude.cmd".to_string(),
                    "\"; calc; \"".to_string(),
                ],
            )
            .is_err(),
            "an embedded quote (the BatBadBut toggle) is rejected regardless of cmd casing"
        );
    }

    /// FIX 2a: the two shim-prefix tokens (`/c` and the shim path) are
    /// zirv-controlled and never trip the guard, and a clean downstream arg --
    /// including a real Bedrock model id with `:` `/` `.` -- passes. Runs on
    /// every platform: off Windows it exercises the no-op path, on Windows the
    /// real allow decision.
    #[test]
    fn a_shim_form_launch_with_only_clean_args_is_allowed() {
        let args = vec![
            "/c".to_string(),
            "C:\\tools\\claude.cmd".to_string(),
            "-p".to_string(),
            "do the thing".to_string(),
            "--model".to_string(),
            "us.anthropic.claude-sonnet-4-v1:0".to_string(),
        ];
        assert!(guard_cmd_shim_reparse("cmd.exe", &args).is_ok());
    }

    /// FIX D (defense-in-depth): the `powershell -NoProfile -File <script>`
    /// launcher form is guarded the same way as the cmd shim -- everything
    /// through the `-File <script>` pair is zirv-controlled prefix, and a
    /// metacharacter in a token after it is refused. The two prefix tokens and
    /// the script path never trip it.
    #[cfg(windows)]
    #[test]
    fn a_powershell_file_launch_is_guarded_after_the_script_path() {
        let bad = vec![
            "-NoProfile".to_string(),
            "-File".to_string(),
            "C:\\tools\\agent.ps1".to_string(),
            "foo&calc".to_string(),
        ];
        assert!(
            guard_cmd_shim_reparse("powershell", &bad).is_err(),
            "a metachar after the script path is refused"
        );

        let clean = vec![
            "-NoProfile".to_string(),
            "-File".to_string(),
            "C:\\tools\\agent.ps1".to_string(),
            "do the thing".to_string(),
        ];
        assert!(
            guard_cmd_shim_reparse("pwsh", &clean).is_ok(),
            "clean args on the powershell form pass, and the prefix never trips"
        );
    }

    /// FIX 2a: a direct `.exe` (no cmd.exe launcher prefix) is not the shim
    /// form, so the guard is a no-op even for an argument that would be
    /// dangerous through cmd.exe -- `CreateProcess` receives it as a literal.
    /// This is also what keeps the test harness's own `sh <script>` fake agents
    /// from being rejected.
    #[test]
    fn a_non_shim_launch_is_never_guarded() {
        let args = vec!["-p".to_string(), "foo&calc".to_string()];
        assert!(guard_cmd_shim_reparse("claude.exe", &args).is_ok());
        assert!(guard_cmd_shim_reparse("/opt/homebrew/bin/claude", &args).is_ok());
        assert!(guard_cmd_shim_reparse("sh", &["/tmp/fake-agent.sh".to_string()]).is_ok());
    }

    /// FINDING 3: an argv that is *already resolved* to the `cmd.exe /c <shim>`
    /// launcher form (what the interactive path hands `injection_args_for_
    /// session`) is recognised as reparsing, where re-resolving the literal
    /// head `cmd.exe` would have found a plain `.exe` and missed it. A direct
    /// `.exe` argv is not a launcher form and is not flagged.
    #[cfg(windows)]
    #[test]
    fn an_already_resolved_launcher_argv_is_recognised_as_reparsing() {
        let resolved_cmd = vec![
            "cmd.exe".to_string(),
            "/c".to_string(),
            "C:\\tools\\claude.cmd".to_string(),
            "the prompt".to_string(),
        ];
        assert!(launch_reparses_through_shim(&resolved_cmd));

        let resolved_ps = vec![
            "powershell".to_string(),
            "-NoProfile".to_string(),
            "-File".to_string(),
            "C:\\tools\\agent.ps1".to_string(),
            "arg".to_string(),
        ];
        assert!(launch_reparses_through_shim(&resolved_ps));

        let direct = vec!["C:\\tools\\claude.exe".to_string(), "--resume".to_string()];
        assert!(!launch_reparses_through_shim(&direct));
    }

    /// Off Windows there is no launcher reparse, so the detection is always
    /// `false` -- including for an argv that structurally looks like one.
    #[cfg(not(windows))]
    #[test]
    fn launch_reparse_detection_is_a_noop_off_windows() {
        let looks_like_cmd = vec![
            "cmd.exe".to_string(),
            "/c".to_string(),
            "claude.cmd".to_string(),
        ];
        assert!(!launch_reparses_through_shim(&looks_like_cmd));
        assert!(!launch_reparses_through_shim(&[]));
    }

    /// Issue #92: a third adapter that overrides nothing must still be
    /// protected against the reparse-argv class, because the trait default
    /// now derives its answer from `resolve_program`'s own resolution of
    /// `program()` instead of assuming the permissive `false`. This adapter
    /// deliberately does not implement `launches_through_cmd_shim` at all.
    #[derive(Debug)]
    struct NoOverrideAdapter(String);

    impl AgentAdapter for NoOverrideAdapter {
        fn name(&self) -> &'static str {
            "no-override"
        }

        fn program(&self) -> &str {
            &self.0
        }

        fn provider(&self) -> &'static str {
            "no-override"
        }

        fn ready(&self) -> CtxResult<()> {
            Ok(())
        }

        fn detect(&self, _command: &[String]) -> bool {
            false
        }

        fn headless_cmd(&self, _prompt: &str, _session: &SessionId, _extra: &[String]) -> Command {
            Command::new("true")
        }

        fn interactive_cmd(&self, _initial_prompt: Option<&str>, _extra: &[String]) -> Command {
            Command::new("true")
        }

        fn distiller_cmd(&self, _model: &str) -> Command {
            Command::new("true")
        }

        fn read_only_args(&self) -> Vec<String> {
            Vec::new()
        }

        fn system_prompt_args(&self, _prompt: &str) -> Vec<String> {
            Vec::new()
        }

        fn transcript_path(&self, _session: &SessionRef) -> PathBuf {
            PathBuf::new()
        }

        fn parse_events(&self, _jsonl: &str) -> Vec<NormalizedEvent> {
            Vec::new()
        }

        fn structural_context(&self, _jsonl: &str, _last_n: usize) -> StructuralContext {
            StructuralContext::default()
        }

        fn compact_command(&self) -> Option<&'static str> {
            None
        }

        fn quit_sequence(&self) -> &'static str {
            ""
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }

        fn register_turn_signal(&self, _session: &SessionRef, _socket: &Path) -> TurnSignalSetup {
            TurnSignalSetup {
                env: Vec::new(),
                instructions: String::new(),
            }
        }
    }

    /// A direct, non-shim program never reports the shim shape, on any
    /// platform, even though this adapter never overrides the trait method --
    /// mirrors `ClaudeAdapter`/`CodexAdapter`'s own identically-named tests.
    #[test]
    fn an_adapter_with_no_override_reports_no_shim_for_a_direct_program() {
        let adapter = NoOverrideAdapter("/tmp/fake-agent".to_string());
        assert!(!adapter.launches_through_cmd_shim());
    }

    /// The core of issue #92: an adapter that implements nothing beyond the
    /// required trait methods -- no `launches_through_cmd_shim` override at
    /// all -- still reports the shim shape correctly for a real `.cmd` on
    /// Windows, because the trait default derives it from `resolve_program`.
    /// Before this fix the default was a hardcoded `false`, so this exact
    /// adapter shape would have shipped unprotected.
    #[cfg(windows)]
    #[test]
    fn an_adapter_with_no_override_is_still_protected_from_a_cmd_shim() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shim = dir.path().join("no-override-agent.cmd");
        std::fs::write(&shim, "@echo off\r\n").expect("write shim");

        let adapter = NoOverrideAdapter(shim.display().to_string());
        assert!(
            adapter.launches_through_cmd_shim(),
            "a .cmd resolution must be reported as the shim shape even with no override"
        );
    }

    /// The trait default: an agent zirv has verified nothing about receives
    /// no base layer, rather than another agent's instructions.
    #[test]
    fn an_unverified_agent_receives_no_base_layer_by_default() {
        assert_eq!(NoOverrideAdapter(String::new()).base_system_prompt(), None);
    }

    /// Issue #167: both real adapters now have their own base layer, and
    /// each is genuinely its own text -- neither ever hands the other
    /// agent's tool-specific instructions.
    #[test]
    fn each_real_adapter_receives_its_own_distinct_base_layer() {
        let claude_layer = claude::ClaudeAdapter::new(None)
            .base_system_prompt()
            .expect("claude has one of its own");
        let codex_layer = codex::CodexAdapter::new(None)
            .base_system_prompt()
            .expect("codex has one of its own, issue #167");
        assert_ne!(claude_layer, codex_layer);
        assert!(
            !codex_layer.contains("Agent tool") && !codex_layer.contains(".claude/agents"),
            "claude-only vocabulary must not reach codex's own layer"
        );
    }

    #[test]
    fn explicit_name_wins() {
        let adapter = select(Some("claude"), &[], &permissive_cfg()).expect("claude selects");
        assert_eq!(adapter.name(), "claude");
    }

    /// `agent_bin` is one global override applied to whichever adapter gets
    /// selected. Naming codex explicitly while `agent_bin` points at a real
    /// `claude` install (stale config left over from switching agents is the
    /// plausible way this happens) would otherwise launch claude's binary
    /// dressed up in codex's own `exec <prompt>` argv shape -- wrong account,
    /// wrong safety model, no error naming what happened. Both names appear
    /// in the refusal, and it is basename-only: the full path is never a
    /// factor.
    #[test]
    fn agent_bin_naming_a_different_adapter_than_selected_is_refused() {
        let mut cfg = permissive_cfg();
        cfg.agent_bin = Some("/opt/homebrew/bin/claude".to_string());
        let err = select(Some("codex"), &[], &cfg).expect_err("cross-adapter agent_bin refuses");
        let msg = err.to_string();
        assert!(
            msg.contains("claude"),
            "names the binary's own agent: {msg}"
        );
        assert!(
            msg.contains("codex"),
            "names the one that was selected: {msg}"
        );
    }

    /// The same collision reached through `resolve_default`'s own
    /// *configured* arm (`cfg.agent` set explicitly, just not on the CLI) --
    /// still a hard refusal, unlike the fallback loop below.
    #[test]
    fn agent_bin_naming_a_different_adapter_is_refused_through_the_default_fallback_too() {
        let mut cfg = permissive_cfg();
        cfg.agent = Some("codex".to_string());
        cfg.agent_bin = Some("claude.exe".to_string());
        let err = resolve_default(&cfg).expect_err("cross-adapter agent_bin refuses");
        let msg = err.to_string();
        assert!(msg.contains("claude"), "got {msg}");
        assert!(msg.contains("codex"), "got {msg}");
    }

    /// Medium 2 (fix): with *no* `cfg.agent` configured, `resolve_default`'s
    /// own fallback loop tries `ADAPTERS` in registry order (`claude` first)
    /// -- before this fix, `agent_bin` naming a real codex install still hit
    /// claude first, and the cross-adapter guard's `?` aborted the whole
    /// fallback right there instead of continuing on to codex, the adapter
    /// that binary actually is. It must resolve to codex, not error.
    #[test]
    fn agent_bin_naming_codex_with_no_agent_configured_falls_through_to_codex() {
        let cfg = CtxConfig {
            agent_bin: Some("/definitely/not/a/real/path/codex".to_string()),
            ..permissive_cfg()
        };
        let (adapter, origin) =
            resolve_default(&cfg).expect("falls through past claude to codex, not an error");
        assert_eq!(adapter.name(), "codex");
        assert_eq!(origin, DefaultOrigin::FirstEnabledReady);
    }

    /// The other half of the same fix: a basename that names *no* registered
    /// adapter at all -- a stub path, or the `sh <fixture>.sh` wrapper shape
    /// this codebase's own tests use throughout -- is never a collision, no
    /// matter how unrelated it looks, and a value that happens to name the
    /// *same* adapter as the one selected (a differently located install) is
    /// explicitly fine too.
    #[test]
    fn agent_bin_naming_no_adapter_or_the_same_one_stays_allowed() {
        let cfg = permissive_cfg();
        assert_eq!(
            agent_bin_names_a_different_adapter(Some("/tmp/fake-codex"), "codex"),
            None,
            "a stub path matches nothing"
        );
        assert_eq!(
            agent_bin_names_a_different_adapter(
                Some("sh /repo/tests/fixtures/fake-codex-agent.sh"),
                "codex"
            ),
            None,
            "the wrapper shape's own basename is \"sh\", not an adapter name"
        );
        assert_eq!(
            agent_bin_names_a_different_adapter(Some("/opt/codex-beta/codex"), "codex"),
            None,
            "naming the selected adapter itself is not a collision"
        );

        let mut cfg = cfg;
        cfg.agent_bin = Some("/opt/codex-beta/codex".to_string());
        let adapter =
            select(Some("codex"), &[], &cfg).expect("same-adapter agent_bin is never refused");
        assert_eq!(adapter.name(), "codex");
    }

    #[test]
    fn detection_reads_the_wrapped_argv() {
        let cmd = vec![
            "/opt/homebrew/bin/claude".to_string(),
            "--resume".to_string(),
        ];
        let adapter = select(None, &cmd, &permissive_cfg()).expect("detect claude");
        assert_eq!(adapter.name(), "claude");
    }

    /// The property the fallback actually promises: whatever it picks is
    /// enabled and ready. Now that codex's own `ready()` succeeds too (see
    /// `CodexAdapter::ready`), both adapters qualify, so this also pins
    /// `ADAPTERS`' registry order (`("claude", ...)` first) as what actually
    /// decides the winner -- both are asserted, the property for its own
    /// sake and the concrete name because losing it silently would be a
    /// regression worth catching too.
    #[test]
    fn empty_command_defaults_to_claude() {
        let cfg = permissive_cfg();
        let adapter = select(None, &[], &cfg).expect("default");
        assert!(
            cfg.agents.is_enabled(adapter.name()),
            "must be gate-enabled"
        );
        assert!(adapter.ready().is_ok(), "must be ready");
        assert_eq!(adapter.name(), "claude");
    }

    #[test]
    fn unknown_name_is_an_error_that_lists_the_options() {
        let err = select(Some("gemini"), &[], &permissive_cfg()).expect_err("unknown agent");
        let msg = err.to_string();
        assert!(msg.contains("gemini"), "got {msg}");
        assert!(
            msg.contains("claude"),
            "error should list known adapters: {msg}"
        );
    }

    /// Task A3: an agent named explicitly is refused, and the message names
    /// the layer that disabled it (mirrors the settings-layer wording tests
    /// in `settings.rs`; here the point is that `select` actually surfaces
    /// it, not the exact wording).
    #[test]
    fn a_disabled_agent_named_explicitly_is_refused_with_the_layer_that_disabled_it() {
        let cfg = cfg_disabling("codex");
        let err = select(Some("codex"), &[], &cfg).expect_err("codex is disabled");
        let msg = err.to_string();
        assert!(msg.contains("codex"), "got {msg}");
        assert!(msg.contains("disabled"), "got {msg}");
        assert!(
            msg.contains(".settings.toml"),
            "names the file that disabled it: {msg}"
        );
    }

    /// The detection arm must refuse, not silently fall back to claude, the
    /// same invariant `detecting_codex_argv_does_not_silently_fall_back_to_claude`
    /// pins for the unready case.
    #[test]
    fn a_disabled_agent_detected_on_the_argv_does_not_fall_back_to_the_default() {
        let cfg = cfg_disabling("codex");
        let cmd = vec!["codex".to_string(), "exec".to_string(), "go".to_string()];
        let err = select(None, &cmd, &cfg).expect_err("must not misroute to claude");
        assert!(err.to_string().contains("codex"), "got {err}");
    }

    /// G: `select`'s empty-command default no longer silently lands on a
    /// different provider just because the repo checkout narrowed claude
    /// off the table -- codex's own `ready()` succeeding too used to make
    /// the fallback pick it automatically, which handed a repo checkout the
    /// power to select which vendor account gets spent. It must refuse
    /// instead, naming both adapters and the fix, exercised here through the
    /// public `select` entry point rather than `resolve_default` directly.
    #[test]
    fn the_default_fallback_refuses_rather_than_silently_switching_provider() {
        let cfg = cfg_disabling("claude");
        let err = select(None, &[], &cfg).expect_err("a repo may narrow, not select");
        let msg = err.to_string();
        assert!(msg.contains("claude"), "got {msg}");
        assert!(msg.contains("codex"), "got {msg}");
        assert!(msg.contains("--agent"), "must say how to fix it: {msg}");
    }

    /// The gate is checked before `ready()`: a disabled-and-unready agent
    /// (codex, always) must report the disable, not "not implemented yet".
    #[test]
    fn the_disable_is_reported_before_an_adapters_own_readiness() {
        let cfg = cfg_disabling("codex");
        let err = select(Some("codex"), &[], &cfg).expect_err("codex is disabled");
        let msg = err.to_string();
        assert!(
            !msg.contains("not implemented yet"),
            "the gate must win over ready(): {msg}"
        );
        assert!(msg.contains("disabled"), "got {msg}");
    }

    #[test]
    fn registry_exposes_both_v1_adapters() {
        let names: Vec<&str> = ADAPTERS.iter().map(|(name, _)| *name).collect();
        assert_eq!(names, vec!["claude", "codex"]);
    }

    /// The registry table is the one place a new adapter is wired in: `all`
    /// must produce exactly one instance per table entry, in table order,
    /// with matching names -- otherwise `all` and `ADAPTERS` could drift.
    #[test]
    fn adding_an_adapter_is_one_entry_in_the_constructor_table() {
        let instances = all(None);
        assert_eq!(instances.len(), ADAPTERS.len());
        for (instance, (name, _)) in instances.iter().zip(ADAPTERS.iter()) {
            assert_eq!(instance.name(), *name);
        }
    }

    /// A provider slug names a usage file, so it has to already *be* a slug:
    /// lowercase `[a-z0-9-]`, non-empty, and unchanged by the sanitiser that
    /// turns it into a file name. It is also the account, not the program --
    /// claude's is `anthropic`, not `claude`.
    #[test]
    fn every_adapter_names_the_account_its_limits_belong_to() {
        for adapter in all(None) {
            let provider = adapter.provider();
            assert!(!provider.is_empty(), "{} has no provider", adapter.name());
            assert_eq!(
                crate::commands::ctx::state::provider_slug(provider),
                provider,
                "{provider} is not already a filesystem-safe lowercase slug"
            );
        }

        let claude = claude::ClaudeAdapter::new(None);
        assert_ne!(
            claude.provider(),
            claude.name(),
            "the provider is the account, not the binary: two harnesses can share one"
        );
    }

    #[test]
    fn the_registry_names_are_unique_and_non_empty() {
        let names: Vec<&str> = ADAPTERS.iter().map(|(name, _)| *name).collect();
        for name in &names {
            assert!(!name.is_empty(), "no adapter may have an empty name");
        }
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            names.len(),
            "duplicate adapter name in {names:?}"
        );
    }

    #[test]
    fn an_empty_command_falls_back_to_the_first_enabled_and_ready_adapter() {
        let (adapter, origin) = resolve_default(&permissive_cfg()).expect("a default exists");
        assert_eq!(adapter.name(), "claude");
        assert_eq!(origin, DefaultOrigin::FirstEnabledReady);
    }

    /// G: now that codex's own `ready()` only checks that its program
    /// resolves (exactly like claude's, see `CodexAdapter::ready`),
    /// disabling claude via a repo-only `.settings.toml` does not leave the
    /// fallback with nothing enabled-and-ready -- codex, next in registry
    /// order, would qualify. `resolve_default` must refuse rather than
    /// silently landing on it: the repo checkout narrowed claude off the
    /// table, but selecting codex *instead* is not the repo's call to make
    /// (`AgentGate::disabled_only_by_repo`).
    #[test]
    fn the_fallback_refuses_to_silently_switch_provider_when_the_repo_disabled_the_default() {
        let cfg = cfg_disabling("claude");
        let err = resolve_default(&cfg).expect_err("a repo may narrow, not select");
        let msg = err.to_string();
        assert!(msg.contains("claude"), "names the narrowed adapter: {msg}");
        assert!(
            msg.contains("codex"),
            "names what it would have silently picked: {msg}"
        );
        assert!(msg.contains("--agent"), "says how to fix it: {msg}");
    }

    /// G2 (fix): the "would otherwise have been the default agent" refusal
    /// must not fire when the repo-disabled adapter was never actually a
    /// candidate -- here claude is *both* repo-disabled *and* genuinely
    /// unready (the same PATH/PATHEXT rig `readiness_note_and_the_fallback_
    /// skip_both_stay_covered_when_an_adapter_is_genuinely_unready` uses), so
    /// disabling it changed nothing: codex was always going to be the
    /// fallback either way, and the refusal's own premise would be false.
    #[cfg(windows)]
    #[test]
    fn the_narrowed_refusal_does_not_fire_for_an_adapter_that_was_never_ready_anyway() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("claude.py"), "print('x')\n").expect("write");
        let path = std::env::var("PATH").unwrap_or_default();
        let _path_guard = crate::commands::ctx::testenv::VarGuard::set(&[
            (
                "PATH",
                Some(format!("{};{}", dir.path().display(), path).as_str()),
            ),
            ("PATHEXT", Some(".EXE;.CMD;.PY")),
        ]);

        let cfg = cfg_disabling("claude");
        let (adapter, origin) =
            resolve_default(&cfg).expect("codex qualifies; claude was never a real candidate");
        assert_eq!(adapter.name(), "codex");
        assert_eq!(origin, DefaultOrigin::FirstEnabledReady);
    }

    /// Final wave item 3: the same false-premise class as the test above,
    /// but reached through `agent_bin` instead of an unresolvable `PATH`.
    /// Claude is repo-disabled, and `agent_bin`'s own basename names codex,
    /// not claude -- so the pre-check used to build `ClaudeAdapter::new(bin)`
    /// (a claude adapter whose `program` actually points at a codex binary)
    /// and ask *that* whether it is `ready()`, which can genuinely answer
    /// yes without claude's own real binary ever being consulted at all.
    /// Recording `repo_narrowed` from that would refuse with a false claim
    /// ("claude would otherwise have been the default agent") over a
    /// candidate `agent_bin_names_a_different_adapter` was always going to
    /// refuse anyway (Medium 2). It must instead land on codex.
    #[test]
    fn the_narrowed_refusal_does_not_fire_when_agent_bin_names_a_different_adapter() {
        let cfg = CtxConfig {
            agent_bin: Some("/definitely/not/a/real/path/codex".to_string()),
            ..cfg_disabling("claude")
        };
        let (adapter, origin) = resolve_default(&cfg)
            .expect("codex qualifies; agent_bin never actually named claude's own binary");
        assert_eq!(adapter.name(), "codex");
        assert_eq!(origin, DefaultOrigin::FirstEnabledReady);
    }

    /// G: the refusal is specific to a *repo-only* disable. An operator who
    /// disabled claude themselves (home file or environment) has already
    /// made the choice the fallback would otherwise be accused of making for
    /// them, so codex is picked normally, exactly as before this fix.
    #[test]
    fn an_operator_disable_still_falls_through_normally() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/.settings.toml"),
            "[agents.claude]\nenabled = false\n",
        )
        .expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let cfg = CtxConfig {
            agents: crate::settings::AgentGate::load(repo.path(), &|k| empty.get(k).cloned())
                .expect("load"),
            ..CtxConfig::default()
        };

        let (adapter, origin) = resolve_default(&cfg).expect("the operator's own choice");
        assert_eq!(adapter.name(), "codex");
        assert_eq!(origin, DefaultOrigin::FirstEnabledReady);
    }

    /// Disabling both known adapters leaves nothing to fall back to; the
    /// error must aggregate one line per adapter naming its own reason,
    /// reusing the gate's refusal text and each adapter's own `ready()` text
    /// rather than inventing new wording.
    #[test]
    fn when_no_adapter_is_both_enabled_and_ready_the_error_names_each_one_and_why() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/.settings.toml"),
            "[agents.claude]\nenabled = false\n[agents.codex]\nenabled = false\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let cfg = CtxConfig {
            agents: crate::settings::AgentGate::load(repo.path(), &|k| empty.get(k).cloned())
                .expect("load"),
            ..CtxConfig::default()
        };

        let err = resolve_default(&cfg).expect_err("both disabled");
        let msg = err.to_string();
        assert!(msg.contains("claude"), "must name claude: {msg}");
        assert!(msg.contains("codex"), "must name codex: {msg}");
        assert!(
            msg.contains("disabled"),
            "must say why claude lost out: {msg}"
        );
        assert!(
            msg.contains("not implemented yet") || msg.contains("disabled"),
            "must say why codex lost out: {msg}"
        );
    }

    /// The fallback is only reached when neither an explicit `--agent` nor
    /// detection named an adapter; either one must bypass it entirely.
    #[test]
    fn an_explicit_or_detected_agent_still_bypasses_the_fallback_entirely() {
        let cfg = cfg_disabling("claude");

        // H3: `resolve_default`'s own fallback would *refuse* under this
        // exact cfg (G: claude is disabled only by the repo layer, and
        // codex would otherwise be silently picked instead) -- proving that
        // if `select(Some("codex"), ...)` below were ever accidentally
        // routed through the fallback instead of truly bypassing it, this
        // test would see that refusal, not a quiet "codex" answer. The two
        // assertions below are provably distinguishable outcomes, not the
        // same value reached two different ways.
        resolve_default(&cfg).expect_err("the fallback itself must refuse here");

        // Explicit name: codex is still enabled by this gate and now
        // resolves successfully, so it is selected directly without ever
        // consulting the fallback.
        let adapter = select(Some("codex"), &[], &cfg).expect("codex is enabled and ready");
        assert_eq!(adapter.name(), "codex");

        // Detection: an argv that names claude explicitly is refused for
        // being disabled, not silently redirected into the fallback.
        let cmd = vec!["/usr/bin/claude".to_string()];
        let err = select(None, &cmd, &cfg).expect_err("claude is disabled");
        assert!(err.to_string().contains("disabled"), "got {err}");
    }

    #[test]
    fn resolve_default_reports_which_rule_chose_the_adapter() {
        let mut cfg = permissive_cfg();
        cfg.agent = Some("claude".to_string());
        let (adapter, origin) = resolve_default(&cfg).expect("claude is configured");
        assert_eq!(adapter.name(), "claude");
        assert_eq!(origin, DefaultOrigin::Configured);

        let (adapter, origin) = resolve_default(&permissive_cfg()).expect("fallback picks one");
        assert_eq!(adapter.name(), "claude");
        assert_eq!(origin, DefaultOrigin::FirstEnabledReady);
    }

    /// The gate wrap and exec use before injecting: a command that matches no
    /// adapter, with no explicit `--agent` to back it, must not be treated as
    /// a match just because `select` had to default to one.
    #[test]
    fn an_undetected_command_with_no_explicit_agent_does_not_match() {
        let adapter = claude::ClaudeAdapter::new(None);
        let command = vec!["echo".to_string(), "hello".to_string()];
        assert!(!command_matches_adapter(&adapter, false, &command));
    }

    #[test]
    fn an_explicit_agent_matches_regardless_of_the_command() {
        let adapter = claude::ClaudeAdapter::new(None);
        let command = vec!["echo".to_string(), "hello".to_string()];
        assert!(command_matches_adapter(&adapter, true, &command));
    }

    #[test]
    fn a_detected_command_matches_even_without_an_explicit_agent() {
        let adapter = claude::ClaudeAdapter::new(None);
        let command = vec!["/opt/homebrew/bin/claude".to_string()];
        assert!(command_matches_adapter(&adapter, false, &command));
    }

    // Worker model resolution (`resolve_worker_model`/`worker_model_args`):
    // the delegated-headless-worker analogue of `resolve_review_model`
    // above, but with a fixed adapter-owned default instead of a ladder.

    #[test]
    fn worker_model_args_uses_the_configured_value_over_the_adapter_default() {
        let adapter = claude::ClaudeAdapter::new(None);
        let cfg = CtxConfig {
            worker: crate::commands::ctx::config::WorkerConfig {
                claude: Some("opus".to_string()),
                codex: None,
            },
            ..permissive_cfg()
        };
        assert_eq!(
            worker_model_args(&cfg, "claude", &adapter),
            vec!["--model".to_string(), "opus".to_string()],
            "the operator's own worker.claude wins over the hard default"
        );
    }

    #[test]
    fn worker_model_args_falls_back_to_claudes_hard_sonnet_default() {
        let adapter = claude::ClaudeAdapter::new(None);
        let cfg = permissive_cfg();
        assert_eq!(cfg.worker.claude, None, "nothing configured");
        assert_eq!(
            worker_model_args(&cfg, "claude", &adapter),
            vec!["--model".to_string(), "sonnet".to_string()],
            "claude's own hard default stops a worker inheriting the operator's seat model"
        );
    }

    #[test]
    fn worker_model_args_adds_nothing_for_codex_with_no_configured_default() {
        let adapter = codex::CodexAdapter::new(None);
        let cfg = permissive_cfg();
        assert_eq!(cfg.worker.codex, None, "nothing configured");
        assert!(
            worker_model_args(&cfg, "codex", &adapter).is_empty(),
            "codex has no adapter-owned default, so its own CLI/config default applies untouched"
        );
    }

    #[test]
    fn worker_model_args_uses_the_configured_codex_value_when_set() {
        let adapter = codex::CodexAdapter::new(None);
        let cfg = CtxConfig {
            worker: crate::commands::ctx::config::WorkerConfig {
                claude: None,
                codex: Some("gpt-5.6-terra".to_string()),
            },
            ..permissive_cfg()
        };
        assert_eq!(
            worker_model_args(&cfg, "codex", &adapter),
            vec!["--model".to_string(), "gpt-5.6-terra".to_string()],
        );
    }

    // FIX A: `last_model_flag` recognises codex's `-m` short alias in every
    // form, not just claude's long `--model`.

    fn flags(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn last_model_flag_reads_the_separated_short_form() {
        assert_eq!(last_model_flag(&flags(&["-m", "opus"])), Some("opus"));
    }

    #[test]
    fn last_model_flag_reads_the_joined_equals_short_form() {
        assert_eq!(last_model_flag(&flags(&["-m=opus"])), Some("opus"));
    }

    #[test]
    fn last_model_flag_reads_the_attached_short_form() {
        assert_eq!(last_model_flag(&flags(&["-mopus"])), Some("opus"));
    }

    /// Last occurrence wins across every mixed spelling -- long, short
    /// separated, short joined, short attached -- in argv order.
    #[test]
    fn last_model_flag_last_wins_across_mixed_forms() {
        assert_eq!(
            last_model_flag(&flags(&["--model", "opus", "-mhaiku"])),
            Some("haiku"),
            "a later attached -m overrides an earlier long --model"
        );
        assert_eq!(
            last_model_flag(&flags(&["-mopus", "--model=sonnet"])),
            Some("sonnet"),
            "a later joined --model= overrides an earlier attached -m"
        );
        assert_eq!(
            last_model_flag(&flags(&["-m", "opus", "-m=haiku", "-msonnet"])),
            Some("sonnet"),
            "every short form in argv order, last wins"
        );
    }

    /// `--model-foo` starts with `-m` once its own leading `-` is peeled,
    /// but it is a `--`-prefixed long flag, not codex's short alias, and
    /// must never be misread as `-m` with an attached value of `odel-foo`.
    #[test]
    fn a_long_flag_that_merely_starts_with_m_does_not_match() {
        assert_eq!(last_model_flag(&flags(&["--model-foo", "opus"])), None);
    }

    /// A bare `-m` with nothing after it (end of args) has no value to
    /// contribute -- it must not be read as naming an empty/wrong model, and
    /// must not clear an earlier real match either.
    #[test]
    fn a_trailing_bare_short_flag_with_no_value_contributes_nothing() {
        assert_eq!(last_model_flag(&flags(&["-m"])), None);
        assert_eq!(
            last_model_flag(&flags(&["-m", "opus", "-m"])),
            Some("opus"),
            "a later dangling -m must not erase the earlier real match"
        );
    }

    #[test]
    fn last_model_flag_returns_none_with_no_model_flag_at_all() {
        assert_eq!(last_model_flag(&flags(&["--verbose", "-x"])), None);
    }

    // `model_only_flags`: the one trailing-flag shape a dashboard pane can
    // honour, in every spelling `classify_model_flag` reads.

    #[test]
    fn model_only_flags_reads_every_spelling_of_a_lone_model_pin() {
        for spelling in [
            vec!["--model", "haiku"],
            vec!["--model=haiku"],
            vec!["-m", "haiku"],
            vec!["-m=haiku"],
            vec!["-mhaiku"],
        ] {
            assert_eq!(
                model_only_flags(&flags(&spelling)),
                Some("haiku"),
                "{spelling:?} pins a model and nothing else"
            );
        }
    }

    /// Anything beyond a model pin means the pane cannot honour what the
    /// operator typed, so the delegation goes headless instead of silently
    /// dropping the rest.
    #[test]
    fn model_only_flags_rejects_flags_a_pane_cannot_honour() {
        for other in [
            vec![],
            vec!["--verbose"],
            vec!["--model", "haiku", "--verbose"],
            vec!["--dangerously-skip-permissions", "--model=haiku"],
        ] {
            assert_eq!(
                model_only_flags(&flags(&other)),
                None,
                "{other:?} is not a lone model pin"
            );
        }
    }

    /// A pin with no usable value is not a pin: a dangling bare flag, a blank
    /// value, and a flag-shaped value all decline the pane rather than build a
    /// `--model` argv token out of nonsense.
    #[test]
    fn model_only_flags_rejects_a_pin_with_no_usable_value() {
        assert_eq!(model_only_flags(&flags(&["--model"])), None);
        assert_eq!(model_only_flags(&flags(&["--model", "  "])), None);
        assert_eq!(model_only_flags(&flags(&["--model="])), None);
        assert_eq!(model_only_flags(&flags(&["--model", "--verbose"])), None);
    }

    // `AgentAdapter::policy_args`: one `EffectivePolicy` input, equivalent
    // real-launch restriction on both registered adapters (Bug B).

    /// The default, all-`Allow` policy must launch every adapter exactly as
    /// before this method existed: empty argv on both.
    #[test]
    fn policy_args_agree_on_no_restriction_under_the_default_policy() {
        let policy = crate::commands::ctx::policy::EffectivePolicy::default();
        assert!(
            claude::ClaudeAdapter::new(None)
                .policy_args(&policy, LaunchMode::Interactive)
                .is_empty()
        );
        assert!(
            codex::CodexAdapter::new(None)
                .policy_args(&policy, LaunchMode::Interactive)
                .is_empty()
        );
    }

    /// The same `EffectivePolicy` (`shell_exec = deny`) must carry an
    /// equivalent restriction to both adapters' real launch argv: claude's
    /// tool-deny pin, and codex's read-only sandbox paired with a suppressed
    /// approval prompt (the pairing that actually stops it escalating to a
    /// human -- see `CodexAdapter::policy_args`'s own doc comment). Neither
    /// is empty, and neither ever names the dangerous full-bypass flag.
    #[test]
    fn policy_args_carry_an_equivalent_restriction_to_both_adapters_from_the_same_policy() {
        use crate::commands::ctx::policy::{EffectivePolicy, Stance};
        let policy = EffectivePolicy {
            shell_exec: Stance::Deny,
            ..EffectivePolicy::default()
        };

        let claude_args =
            claude::ClaudeAdapter::new(None).policy_args(&policy, LaunchMode::Interactive);
        let codex_args =
            codex::CodexAdapter::new(None).policy_args(&policy, LaunchMode::Interactive);

        assert!(
            !claude_args.is_empty(),
            "claude must restrict: {claude_args:?}"
        );
        assert!(
            !codex_args.is_empty(),
            "codex must restrict too: {codex_args:?}"
        );
        assert!(
            claude_args.iter().any(|a| a.contains("Bash")),
            "claude denies shell execution via its tool pin: {claude_args:?}"
        );
        assert!(
            codex_args
                .windows(2)
                .any(|w| w == ["--sandbox", "read-only"]),
            "codex denies it via the read-only sandbox: {codex_args:?}"
        );
        assert!(
            codex_args
                .windows(2)
                .any(|w| w == ["--ask-for-approval", "never"]),
            "and must not merely fall back to prompting for it: {codex_args:?}"
        );
        for args in [&claude_args, &codex_args] {
            assert!(
                !args.iter().any(|a| a.contains("dangerously-bypass")),
                "an equivalent-restriction mapping must never widen: {args:?}"
            );
        }
    }

    // Issue #89: the codex distiller/reviewer sandbox-asymmetry announcement.

    /// The pure latch primitive `announce_sandbox_residual_once` builds on:
    /// exactly one caller ever wins, regardless of how many times it is
    /// asked, so the announcement itself cannot fire more than once per
    /// process even though `handoff::run_model` (and `read_only_args_for_
    /// agent_name`) call it on every single distiller/reviewer spawn.
    #[test]
    fn claim_once_wins_exactly_once() {
        let latch = std::sync::atomic::AtomicBool::new(false);
        assert!(claim_once(&latch), "the first call claims the latch");
        assert!(
            !claim_once(&latch),
            "a second call must find it already claimed"
        );
        assert!(!claim_once(&latch), "and every call after that too");
    }

    /// Claude's own distiller/reviewer argv must be byte-for-byte unchanged
    /// by issue #89: `sandbox_residual_note` stays `None` (the trait
    /// default), so `announce_sandbox_residual_once` is always a no-op for
    /// it regardless of the latch, and nothing about `read_only_args`/
    /// `distiller_cmd` changed for this adapter at all.
    #[test]
    fn claude_has_no_sandbox_residual_to_announce() {
        let adapter = claude::ClaudeAdapter::new(None);
        assert_eq!(adapter.sandbox_residual_note(), None);
        assert_eq!(
            adapter.read_only_args(),
            vec!["--disallowedTools=Write,Edit,Bash,NotebookEdit".to_string()],
            "unchanged by issue #89"
        );
    }

    /// `announce_sandbox_residual_once` must be a safe no-op for an adapter
    /// with nothing to disclose -- it must not touch the shared latch at
    /// all, so a claude call never steals the one announcement a later
    /// codex call in the same process is entitled to.
    #[test]
    fn announce_sandbox_residual_once_never_claims_the_latch_for_an_adapter_with_no_residual() {
        let latch = std::sync::atomic::AtomicBool::new(false);
        // Exercise the same "no residual -> no claim" branch
        // `announce_sandbox_residual_once` itself takes, against a
        // caller-owned latch so this is independent of whatever the real
        // process-wide static has already done in this test binary.
        let adapter = claude::ClaudeAdapter::new(None);
        assert!(adapter.sandbox_residual_note().is_none());
        assert!(
            !latch.load(std::sync::atomic::Ordering::SeqCst),
            "an adapter with nothing to report must never reach the claim step"
        );
    }

    // -- scratchpad_rules (issue #104) ---------------------------------

    /// A Windows-shaped temp dir: backslashes become forward slashes, the
    /// trailing slash is dropped, and -- since the path already carries no
    /// leading slash of its own (a drive letter) -- `//` is simply
    /// prepended.
    #[test]
    fn scratchpad_rules_projects_a_windows_temp_dir() {
        let rules = scratchpad_rules(Path::new(r"C:\Users\x\AppData\Local\Temp\"));
        assert_eq!(
            rules,
            [
                "Read(//C:/Users/x/AppData/Local/Temp/claude/**)".to_string(),
                "Edit(//C:/Users/x/AppData/Local/Temp/claude/**)".to_string(),
            ]
        );
    }

    /// A Unix-shaped temp dir already carries its own leading `/`, so the
    /// convention is `//` plus the path *without* that leading slash --
    /// `//tmp/claude/**`, not `///tmp/claude/**` (three slashes).
    #[test]
    fn scratchpad_rules_projects_a_unix_temp_dir() {
        let rules = scratchpad_rules(Path::new("/tmp"));
        assert_eq!(
            rules,
            [
                "Read(//tmp/claude/**)".to_string(),
                "Edit(//tmp/claude/**)".to_string(),
            ]
        );
    }
}
