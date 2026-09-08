//! Issue #388 (wave 2): the Factory Droid CLI adapter.
//!
//! Droid ships as the npm package `@factory/cli` (binary name `droid`,
//! platform binaries under `@factory/cli-<platform>-<arch>`), a proprietary,
//! closed-source, curl/npm-installed agent. Not installed on this machine, and
//! there is no public source to read the way `pi.rs`/`gemini.rs` could read
//! bundled JS -- every fact below is instead verified by directly running the
//! REAL compiled binary (`@factory/cli-win32-x64@0.213.0`, downloaded via
//! `npm pack` into this worktree's `target/` on 2026-09-07 and never
//! installed globally) rather than trusting `docs.factory.ai`, whose own
//! pages disagree with the shipped binary on multiple flags (see "Doc vs.
//! binary disagreements" below). Anything not cited to a specific probe or
//! doc page here is UNSUPPORTED and left at the trait default.
//!
//! # Verified against the real binary (`droid.exe --help`, `droid.exe exec
//! --help`, 0.213.0, 2026-09-07)
//!
//! - Interactive: `droid [options] [command] [prompt...]` -- a bare `droid`
//!   starts the REPL, `droid "review app.tsx"` starts it with an initial
//!   prompt (`--help`'s own Examples block).
//! - Headless: `droid exec [options] [prompt]`. `-o, --output-format
//!   <format>` (default `"text"`); `-m, --model <id>` (default
//!   `gpt-5.6-sol`); `-r, --reasoning-effort <level>`; `--auto
//!   low|medium|high`; `-s, --session-id <id>` -- **"Existing session to
//!   continue (requires a prompt)"**, verified NOT a create-with-this-id
//!   flag the way claude's/pi's own `--session-id` is (see "Session id: no
//!   pin" below); `--skip-permissions-unsafe` (never used here, per this
//!   adapter's own brief); `--append-system-prompt <text>` and
//!   `--append-system-prompt-file <path>` (both present verbatim in `exec
//!   --help`, not merely inferred from a shorthand the way claude's own file
//!   flag had to be); `--only-tools <ids>` / `--add-tools <ids>` /
//!   `--remove-tools <ids>` (comma-separated tool ids or `MCP:<server>
//!   [/<tool>]` selectors) -- **not** `--restrict-tools`/`--disabled-tools`,
//!   which do not exist on this binary at all despite appearing in
//!   `docs.factory.ai/droid-cli/cli-reference`'s own flag table (see "Doc vs.
//!   binary disagreements"). `--cwd <path>`.
//! - Output-format probe (`exec --output-format <fmt> --cwd <dir> "hi"`,
//!   `FACTORY_API_KEY` set to a deliberately invalid value so the call fails
//!   fast without ever reaching a real model): `text` and `json` both run and
//!   fail cleanly on the bad key; `json` yields ONE final object --
//!   `{"type":"result","subtype":"...","is_error":bool,"duration_ms":...,
//!   "num_turns":...,"result":"...","session_id":"...","usage":{...}}`.
//!   `stream-json` yields newline-delimited per-event objects on STDOUT
//!   (`{"type":"system","subtype":"init",...}`, `{"type":"error",...}`, each
//!   carrying its own `session_id`) -- this is the shape [`headless_cmd`]
//!   uses, since it is the only one that reports incrementally rather than
//!   only at exit. `stream-jsonrpc` blocks waiting on stdin (a long-lived
//!   control channel, matching `--input-format`'s own description) and is
//!   unusable for a one-shot headless launch.
//! - Tool ids and read-only enforcement (`exec --list-tools`, `--only-tools`,
//!   `--remove-tools`, all run with no prompt and no `FACTORY_API_KEY` at
//!   all -- `--list-tools` needs no network or auth): the default (no
//!   `--auto`) autonomy already lists itself as "read-only", but `Execute`
//!   still shows `status: allowed` there, so `--auto`-only is NOT a
//!   verified structural deny. `--only-tools Read,Grep,Glob,LS` IS verified
//!   structural: every other tool (`TodoWrite`, `WebSearch`, `FetchUrl`,
//!   `ConnectorSearch`, `Execute`, `ApplyPatch`, ...) flips to `status:
//!   blocked` in the same `--list-tools` dump. `--remove-tools Execute`
//!   independently verified too (`Execute` flips to `status: blocked
//!   override`). [`read_only_args`] uses the allow-list form, the same
//!   four-tool inspection boundary `pi::PiAdapter::read_only_args` draws
//!   (`Read`, `Grep`, `Glob`/`find`, `LS`/`ls`). An empty `--only-tools ""`
//!   is verified to be a no-op (identical output to no flag at all), so it
//!   cannot express "zero tools" -- [`distiller_cmd`] reuses the same
//!   allow-list rather than inventing an unverified stronger pin.
//! - Session storage (a live end-to-end run against a local mock
//!   OpenAI-compatible server wired up as a BYOK custom model -- see
//!   `tests/fixtures/droid/README.md` for the exact reproduction -- since no
//!   real `FACTORY_API_KEY` exists here): transcripts live at
//!   `~/.factory/sessions/<cwd-slug>/<session-id>.jsonl`, alongside a
//!   `<session-id>.settings.json` sibling -- **not** `~/.factory/projects/...`
//!   as `docs.factory.ai/reference/hooks-reference` states (that page
//!   describes a hook's OWN `transcript_path` field, and may simply be
//!   stale; the real on-disk layout observed from the shipped 0.213.0 binary
//!   is what this adapter is built against). `<cwd-slug>` is
//!   [`session_dir_slug`] -- verified against two independent real runs
//!   (`C:\Users\...\Temp` -> `-C-Users-...-Temp`, and a path containing a
//!   literal space and existing hyphens, which both survived unmangled).
//!   `<session-id>` is droid's OWN minted uuid, written verbatim as the
//!   filename -- never zirv's.
//! - Session id: no pin (verified: `-s, --session-id`'s own `--help` text is
//!   "Existing session to continue (**requires a prompt**)", i.e. an
//!   as-yet-unknown id is refused rather than adopted as a new
//!   conversation's id). This is `gemini::GeminiAdapter`'s exact "Deliberately
//!   UNSUPPORTED" situation, not `pi::PiAdapter`'s: pi's own `--session-id`
//!   documents "creating it if missing", droid's does not, and nothing here
//!   claims otherwise. [`resume_args`]/[`session_pin_args`] therefore stay at
//!   the trait default for the same reason gemini's do -- there is no flag
//!   that could ever make zirv's own uuid become droid's real session id, so
//!   [`transcript_path`] resolves the same directory-scan-and-pin way
//!   `codex`/`gemini` do (see [`resolve_session_file`]), except keyed by
//!   filesystem timestamp rather than an in-content one (see next point).
//! - Transcript row shape (same live end-to-end run): line 1 is
//!   `{"type":"session_start","id":"<uuid>","title":"...","owner":"...",
//!   "version":2,"cwd":"...","hostId":"...","isSessionTitleManuallySet":
//!   bool}` -- carries no timestamp field at all, unlike
//!   `gemini::session_start_ms`'s own metadata record, which is why
//!   [`resolve_session_file`] falls back to the FILE's own filesystem
//!   creation/modified time instead (a documented, disclosed deviation from
//!   the in-content-timestamp shape every other multi-file adapter here
//!   uses). Every later line is `{"type":"message","id":"<uuid>",
//!   "parentId":"<uuid-or-absent>","timestamp":"<iso8601>","message":{
//!   "role":"user"|"assistant","content":[...],...}}` -- the same
//!   `{"type":"message","message":{"role":...}}` ENVELOPE `pi::PiAdapter`'s
//!   own transcript uses, but with Anthropic-shaped content BLOCKS inside it
//!   (`{"type":"text","text":...}`, `{"type":"tool_use","id","name",
//!   "input"}`, `{"type":"tool_result","tool_use_id","is_error","content"}`)
//!   -- the same block vocabulary `claude::ClaudeAdapter`'s own transcript
//!   uses. An `assistant`-role message additionally carries its own
//!   `modelId` field verbatim (`"custom:Mock-Model-0"` for a BYOK custom
//!   model in the verifying run) -- [`model_hint`] reads this directly;
//!   whether a BUILT-IN (non-BYOK) model reports its own bare id
//!   (`"gpt-5.6-sol"`) in the identical field was not independently
//!   exercised (that needs a real `FACTORY_API_KEY`), so this is presumed
//!   symmetric, not confirmed. droid ALSO injects its own large
//!   system-context as a synthetic `user`-role row tagged `"id":
//!   "context-<uuid>"` and `"message":{...,"visibility":"llm_only"}` --
//!   [`parse_events`]/[`structural_context`] both filter any row matching
//!   either marker out of `TurnStart`/`user_messages`, since counting it as
//!   a genuine human turn would badly skew rot's turn-based signals.
//! - Token usage: verified ABSENT from the `.jsonl` transcript entirely in
//!   the same live run -- every class (`inputTokens`/`outputTokens`/
//!   `cacheReadTokens`/`cacheCreationTokens`) instead lives in the sibling
//!   `<session-id>.settings.json` (`tokenUsage`/`inclusiveTokenUsage`,
//!   cumulative, rewritten whole each turn). [`AgentAdapter::
//!   transcript_usage`] only ever receives the `.jsonl` CONTENT, never a
//!   path it could use to open that sibling file, so there is no way to
//!   implement it without violating the trait's own line-local contract --
//!   left at the trait default (`None`), same as `AssistantFinal::
//!   input_tokens` staying an honest `0` in [`parse_events`].
//! - `stdin` prompt delivery for a `.cmd`-shim launch: verified in `exec
//!   --help`'s own Examples block (`echo "analyze code" | droid exec`, `cat
//!   prompt.txt | droid exec --auto medium`) -- omitting the positional
//!   prompt with piped stdin runs headlessly on stdin text, the same shape
//!   [`headless_cmd_stdin`] needs.
//! - Models: a large fixed built-in lineup spanning Anthropic/OpenAI/Google/
//!   xAI/zAI/Moonshot/etc. vendors as BARE ids (`claude-sonnet-5`,
//!   `gpt-5.6-sol`, `gemini-3.1-pro-preview`, `grok-4.6`, ...), plus
//!   `custom:<id>` for a BYOK model (`exec --help`'s own "Available Models"
//!   block). Unlike `pi`/`opencode`'s `provider/model` namespacing, these are
//!   the SAME bare ids `catalogue.rs`'s own ladders already key on, so
//!   [`provider_for_model`] resolves through `catalogue::vendor_of` directly,
//!   with no prefix-stripping needed -- `catalogue::vendor_of("custom:...")`
//!   correctly finds nothing and falls back to this adapter's own `"factory"`.
//!
//! # Doc vs. binary disagreements (docs.factory.ai, fetched 2026-09-07)
//!
//! `docs.factory.ai/droid-cli/cli-reference` and `.../droid-exec/overview`
//! both document `--restrict-tools`/`--disabled-tools`/`--additional-tools`
//! and a `~/.factory/projects/<slug>/<session-id>.jsonl` transcript path.
//! Neither exists on the real 0.213.0 binary: `exec --help` instead shows
//! `--only-tools`/`--add-tools`/`--remove-tools`, and the real transcript
//! lives under `~/.factory/sessions/...` (verified above). This module cites
//! the BINARY, not the docs, wherever the two disagree; the doc pages are
//! cited only for facts the binary's own `--help` cannot state (the
//! interactive slash-command vocabulary below, and the BYOK settings.json
//! schema used to run the live verification).
//!
//! # Doc-only facts (not independently run against the binary)
//!
//! - `/compress` and `/quit` (alias `exit`) are real interactive slash
//!   commands (`docs.factory.ai/droid-cli/cli-reference`'s own slash-command
//!   list, and independently `docs.factory.ai/cli/configuration/
//!   custom-slash-commands`, which names `/compress` among the commands that
//!   "modify the conversation or session"). `droid --help`'s own output
//!   never lists slash commands (those only ever appear inside a live REPL,
//!   `/help`), so [`compact_command`]/[`quit_sequence`] are sourced from
//!   these two independent doc pages rather than a probe.
//!
//! # Deliberately UNSUPPORTED
//!
//! - `resume_args`/`session_pin_args`: see "Session id: no pin" above.
//! - `transcript_usage`/`Capabilities::token_usage`: see "Token usage" above.
//! - Tool calls' `files_read`/`files_modified`/`tool_errors` on
//!   [`StructuralContext`]: `tool_use.input`'s own per-tool argument key for
//!   a file path (`file_path`? `path`? `target_file`?) was never observed
//!   against a REAL Factory-routed tool call (the verifying mock model only
//!   exercised the generic `Execute` tool with a `command` argument) -- left
//!   empty rather than guessed, mirroring `gemini`/`codex`'s own honest gaps
//!   here.
//! - `Compaction`: no verified row shape for a persisted compaction record in
//!   this transcript format (unlike `pi`'s own confirmed `type: "compaction"`
//!   entry) -- `parse_events` never emits it.
//! - `supports_headless_compact`/`headless_resume_cmd`: droid's only verified
//!   compaction mechanism is the interactive `/compress` slash command above;
//!   there is no verified headless compact-then-resume flow to pair it with.
//! - `default_sandbox_args`/`policy_args`/`extra_writable_root_args`/
//!   `base_system_prompt`/`worker_system_prompt`: out of scope for this wave,
//!   matching `pi::PiAdapter`/`gemini::GeminiAdapter`, neither of which
//!   overrides them either.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;

use serde_json::Value;

use super::super::CtxResult;
use super::super::catalogue;
use super::super::event::{
    Capabilities, NormalizedEvent, SessionId, SessionRef, StructuralContext, input_hash,
};
use super::{AgentAdapter, ResolvedProgram, TurnSignalSetup};

/// See this module's own doc comment ("Session storage") for the verification
/// trail. `Read`, `Grep`, `Glob`, `LS` -- the four inspection-shaped tool ids
/// verified via a real `--list-tools` dump, mirroring `pi::PiAdapter::
/// read_only_args`'s own four-tool boundary.
const READ_ONLY_TOOLS: &str = "Read,Grep,Glob,LS";

#[derive(Debug, Clone)]
pub struct DroidAdapter {
    program: String,
    bin_args: Vec<String>,
    home: Option<PathBuf>,
    /// Test seam only, mirroring `GeminiAdapter::forced_state_root` exactly:
    /// pins the zirv state root [`DroidAdapter::transcript_path`] resolves
    /// its own rollout pin from, instead of the real platform state
    /// directory.
    #[cfg(test)]
    forced_state_root: Option<PathBuf>,
}

impl DroidAdapter {
    /// `bin` may carry arguments, mirroring every other adapter's own
    /// `new` exactly (`"sh /tmp/stub.sh"`, `"/usr/bin/env droid"` both work).
    pub fn new(bin: Option<&str>) -> Self {
        let raw = bin.unwrap_or("droid").trim();
        let mut parts = raw.split_whitespace().map(str::to_string);
        let program = parts.next().unwrap_or_else(|| "droid".to_string());
        Self {
            program,
            bin_args: parts.collect(),
            home: None,
            #[cfg(test)]
            forced_state_root: None,
        }
    }

    /// Test seam: pins the home directory [`transcript_path`](AgentAdapter::
    /// transcript_path) resolves the sessions root from.
    #[cfg(test)]
    pub fn with_home(mut self, home: PathBuf) -> Self {
        self.home = Some(home);
        self
    }

    /// Test seam: see the field's own doc comment.
    #[cfg(test)]
    pub fn with_state_root(mut self, root: PathBuf) -> Self {
        self.forced_state_root = Some(root);
        self
    }

    /// Every command starts here, mirroring `GeminiAdapter::base`/
    /// `PiAdapter::base` exactly: the program is routed through
    /// [`super::resolve_program`] so an npm-installed `droid.cmd` shim
    /// launches on Windows.
    fn base(&self) -> Command {
        let resolved = super::resolve_program(&self.program)
            .unwrap_or_else(|_| ResolvedProgram::direct(&self.program));
        let mut cmd = Command::new(&resolved.program);
        cmd.args(&resolved.prefix);
        cmd.args(&self.bin_args);
        cmd
    }

    fn home_dir(&self) -> PathBuf {
        self.home
            .clone()
            .or_else(|| crate::utils::home_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// `~/.factory/sessions` -- see this module's own doc comment ("Session
    /// storage") for the real-binary verification.
    fn sessions_root(&self) -> PathBuf {
        self.home_dir().join(".factory").join("sessions")
    }

    /// The deterministic per-cwd session directory -- see
    /// [`session_dir_slug`].
    fn session_dir(&self, cwd: &Path) -> PathBuf {
        self.sessions_root().join(session_dir_slug(cwd))
    }

    #[cfg(test)]
    fn state_dir(&self) -> Option<super::super::state::StateDir> {
        self.forced_state_root
            .clone()
            .map(super::super::state::StateDir::from_root)
    }

    #[cfg(not(test))]
    fn state_dir(&self) -> Option<super::super::state::StateDir> {
        super::super::state::StateDir::resolve(&super::super::config::env_from_process()).ok()
    }

    /// Resolution 2/3 of [`AgentAdapter::transcript_path`], mirroring
    /// `codex`/`gemini`'s own pin-then-scan shape exactly, EXCEPT the
    /// candidate ordering signal: droid's own `session_start` row carries no
    /// timestamp field (this module's own doc comment), so
    /// [`resolve_session_file`] orders candidates by filesystem
    /// creation/modified time instead of an in-content one.
    fn pinned_session_file(&self, session: &SessionRef) -> Option<PathBuf> {
        let state = self.state_dir()?;
        let short = super::super::sessions::short_id(session.id.as_str());
        let pin = state.rollouts().join(format!("{short}.droid.path"));
        if let Ok(recorded) = std::fs::read_to_string(&pin) {
            let recorded = PathBuf::from(recorded.trim());
            if recorded.is_file() {
                return Some(recorded);
            }
        }
        let record = super::super::sessions::load_record(&state, &short)?;
        let started_ms = record.started_at.saturating_mul(1_000);
        let dir = self.session_dir(&session.cwd);
        let resolved = resolve_session_file(&dir, started_ms)?;
        if super::super::state::create_private_dir_all(&state.rollouts()).is_ok() {
            let _ = super::super::state::write_private(&pin, &resolved.display().to_string());
        }
        Some(resolved)
    }
}

/// The real on-disk session-directory name for `cwd` -- verified against two
/// independent real runs of `droid.exe` (see this module's own doc comment,
/// "Session storage"): a Windows drive-letter prefix (`C:`) loses its colon
/// and gains a leading separator (`C:\Users\...` behaves exactly as
/// `\C\Users\...` would), then every remaining `/` or `\` becomes a single
/// `-`. Verified NOT to touch any other character: an existing hyphen and a
/// literal space both survived unmangled in the second probe. The
/// POSIX-style leading-slash case (no drive letter to rewrite, so a cwd like
/// `/Users/x/repo` already starts with the separator this transform would
/// otherwise manufacture) is the natural extension of the same single rule,
/// not itself independently run -- the win32-x64 binary is the only platform
/// package this adapter was verified against.
fn session_dir_slug(cwd: &Path) -> String {
    let raw = cwd.to_string_lossy().into_owned();
    let bytes = raw.as_bytes();
    let with_root = if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        format!("/{}{}", &raw[0..1], &raw[2..])
    } else {
        raw
    };
    with_root
        .chars()
        .map(|c| if c == '/' || c == '\\' { '-' } else { c })
        .collect()
}

/// The moment `path` most plausibly appeared, in unix milliseconds: its
/// filesystem creation time, falling back to its modified time when the
/// platform/filesystem does not track birth time (e.g. some Linux
/// filesystems) -- never a guess when neither is readable at all. See this
/// module's own doc comment for why no in-content timestamp is available to
/// use instead.
fn candidate_started_ms(path: &Path) -> Option<u64> {
    let meta = std::fs::metadata(path).ok()?;
    let time = meta.created().or_else(|_| meta.modified()).ok()?;
    let since_epoch = time.duration_since(UNIX_EPOCH).ok()?;
    Some(since_epoch.as_millis() as u64)
}

/// The `.jsonl` file inside `dir` this session most plausibly created: the
/// EARLIEST one whose own observed start time (see [`candidate_started_ms`])
/// is at or after `started_ms` -- mirrors `codex::resolve_rollout`'s
/// "earliest, not newest" rationale exactly (a session's own launch is the
/// first file to appear after it started; "newest" would hand an older
/// session a younger concurrent one's transcript). Non-recursive and scoped
/// to `.jsonl` extensions only, so the `.settings.json`/`.settings.json.bak`
/// siblings droid also writes are never candidates.
fn resolve_session_file(dir: &Path, started_ms: u64) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut candidates: Vec<(u64, PathBuf)> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("jsonl")
        })
        .filter_map(|path| candidate_started_ms(&path).map(|ms| (ms, path)))
        .filter(|(ms, _)| *ms >= started_ms)
        .collect();
    candidates.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    candidates.into_iter().next().map(|(_, path)| path)
}

/// Whether `row_id`/`message` mark a droid-injected system-context row rather
/// than a genuine human turn -- see this module's own doc comment for the
/// two verified markers (`"context-"`-prefixed id, `"visibility":
/// "llm_only"`).
fn is_context_injection(row_id: &str, message: &Value) -> bool {
    row_id.starts_with("context-")
        || message.get("visibility").and_then(Value::as_str) == Some("llm_only")
}

/// The concatenated text of a `user`/`assistant` message's own `text`-typed
/// content blocks, dropping every other block type (`tool_use`/
/// `tool_result`) -- the same "text blocks only" rule `claude::text_of`/
/// `pi::assistant_text_of` already apply, shared here across both roles
/// since droid's own `user` and `assistant` rows carry the identical
/// content-block array shape (this module's own doc comment).
fn text_of(message: &Value) -> String {
    message
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

impl AgentAdapter for DroidAdapter {
    fn name(&self) -> &'static str {
        "droid"
    }

    fn program(&self) -> &str {
        &self.program
    }

    /// droid itself spends no account of its own -- it is a front end onto
    /// whichever vendor its selected model belongs to. `"factory"` is the
    /// honest static answer for a launch with no model in hand yet;
    /// [`provider_for_model`] resolves the real billed vendor whenever a
    /// model string is available.
    fn provider(&self) -> &'static str {
        "factory"
    }

    /// See this module's own doc comment ("Models"): droid's own model ids
    /// are bare (no `provider/` namespace), so `catalogue::vendor_of`
    /// resolves them directly. `None` (an id this catalogue does not
    /// recognize -- including every `custom:...` BYOK id, which never
    /// matches any vendor's ladder) falls back to this adapter's own static
    /// `"factory"`, never a guess. Mirrors `pi::PiAdapter::
    /// provider_for_model` exactly.
    fn provider_for_model(&self, model: Option<&str>) -> &'static str {
        model
            .and_then(catalogue::vendor_of)
            .unwrap_or(self.provider())
    }

    fn ready(&self) -> CtxResult<()> {
        super::resolve_program(&self.program)?;
        Ok(())
    }

    fn detect(&self, command: &[String]) -> bool {
        command
            .first()
            .and_then(|p| Path::new(p).file_name())
            .map(|f| {
                let f = f.to_string_lossy();
                f == "droid" || f == "droid.exe" || f == "droid.cmd"
            })
            .unwrap_or(false)
    }

    /// `exec -o stream-json <prompt>`: `-o stream-json` is the one output
    /// format verified to report per-event JSON incrementally rather than
    /// only a single object at exit (`-o json`) or block on stdin
    /// (`stream-jsonrpc`) -- see this module's own doc comment ("Output-format
    /// probe"). `session` is never placed on argv: droid's own `-s` cannot
    /// adopt a caller-chosen id for a brand new conversation (see "Session
    /// id: no pin"), so a fresh headless launch always lets droid mint its
    /// own, exactly like `codex::CodexAdapter::headless_cmd`/
    /// `gemini::GeminiAdapter::headless_cmd`.
    fn headless_cmd(&self, prompt: &str, _session: &SessionId, extra: &[String]) -> Command {
        let mut cmd = self.base();
        cmd.arg("exec")
            .arg("-o")
            .arg("stream-json")
            .arg(prompt)
            .args(extra);
        cmd
    }

    /// For the Windows `.cmd`-shim case (see [`launches_through_cmd_shim`]):
    /// omitting the positional prompt with piped stdin runs headlessly on
    /// stdin text -- verified directly in `exec --help`'s own Examples
    /// block (`echo "analyze code" | droid exec`).
    fn headless_cmd_stdin(&self, _session: &SessionId, extra: &[String]) -> Option<Command> {
        let mut cmd = self.base();
        cmd.arg("exec").arg("-o").arg("stream-json").args(extra);
        Some(cmd)
    }

    /// Same derivation every adapter gets by default, overridden explicitly
    /// to read the same way across every adapter file -- mirrors
    /// `PiAdapter`/`GeminiAdapter`'s own identical override.
    fn launches_through_cmd_shim(&self) -> bool {
        super::launches_through_cmd_shim(&self.program)
    }

    /// `droid [prompt...]` with no subcommand is the interactive launch
    /// (verified: `droid.exe --help`'s own top-level usage line and Examples
    /// block).
    fn interactive_cmd(&self, initial_prompt: Option<&str>, extra: &[String]) -> Command {
        let mut cmd = self.base();
        if let Some(prompt) = initial_prompt {
            cmd.arg(prompt);
        }
        cmd.args(extra);
        cmd
    }

    /// The distiller needs no repository write/execute access -- reuses
    /// [`read_only_args`] exactly like `claude`/`gemini`'s own distiller
    /// (`--only-tools ""` was verified to be a no-op, this module's own doc
    /// comment, so it cannot express a stronger "zero tools" pin than the
    /// allow-list already does). The prompt is delivered on stdin (no
    /// positional token), matching every other adapter's own distiller
    /// shape: the caller (`handoff::run_model`) pipes it in.
    fn distiller_cmd(&self, model: &str) -> Command {
        let mut cmd = self.base();
        cmd.arg("exec").args(self.read_only_args());
        if !model.is_empty() {
            cmd.arg("-m").arg(model);
        }
        cmd
    }

    /// `--only-tools Read,Grep,Glob,LS` -- see this module's own doc comment
    /// ("Tool ids and read-only enforcement") for the full verification
    /// trail on why this, and not `--auto`'s own default read-only posture,
    /// is the structural deny mechanism.
    fn read_only_args(&self) -> Vec<String> {
        vec!["--only-tools".to_string(), READ_ONLY_TOOLS.to_string()]
    }

    /// Empty, NOT [`Self::read_only_args`] unchanged: `--only-tools` was
    /// verified only against `droid exec --help` (this module's own doc
    /// comment, "Tool ids and read-only enforcement") -- every citation for
    /// it names the `exec` subcommand's own help text, never the top-level
    /// `droid [options] [command] [prompt...]` parser's. `AgentAdapter::
    /// interactive_read_only_args`'s own doc comment records the exact same
    /// mistake for codex's `--ignore-rules`/`--ignore-user-config` (verified
    /// `exec`-only, applied to `codex`'s top-level interactive launch,
    /// instant clap exit code 2 on a `--mode read-only` dashboard pane): the
    /// trait default (falling back to `read_only_args()` unchanged) would
    /// repeat that bug here, since `dash`'s own spawn-request pane variant
    /// and `read_only_args_for_agent_name`/`extend_read_only_args`
    /// (`mod.rs`) both apply this to an interactive launch too. Unlike
    /// codex, no verified interactive-safe restriction exists to substitute
    /// -- `--auto`'s own default posture is explicitly documented as NOT a
    /// structural deny (this module's own doc comment) -- so empty (never
    /// refuse the launch, degrade the guarantee honestly) is the correct
    /// answer, not a guessed flag.
    fn interactive_read_only_args(&self) -> Vec<String> {
        Vec::new()
    }

    /// Verified: `exec --help`, `--append-system-prompt <text>`.
    fn system_prompt_args(&self, prompt: &str) -> Vec<String> {
        if prompt.trim().is_empty() {
            return Vec::new();
        }
        vec!["--append-system-prompt".to_string(), prompt.to_string()]
    }

    fn user_system_prompt_flag(&self) -> Option<&'static str> {
        Some("--append-system-prompt")
    }

    /// Verified: `exec --help`, `--append-system-prompt-file <path>` --
    /// present verbatim on the real binary's own help text (unlike claude's
    /// identically-named flag, which needed a `--help` PROBE because it only
    /// appears folded into a shorthand). No `supports_system_prompt_file`
    /// override, though: this wave built no probe-and-cache machinery (like
    /// `pi`/`gemini`, neither of which built one either), so delivery stays
    /// on argv via [`system_prompt_args`] -- this flag name is offered so a
    /// future caller with a verified need for file delivery has it in hand.
    fn system_prompt_file_flag(&self) -> Option<&'static str> {
        Some("--append-system-prompt-file")
    }

    /// See [`DroidAdapter::pinned_session_file`] for the pin-then-scan
    /// resolution. The fallback (a file that has never existed) mirrors
    /// `gemini::GeminiAdapter::transcript_path`'s own "unresolved" naming.
    fn transcript_path(&self, session: &SessionRef) -> PathBuf {
        if let Some(pinned) = self.pinned_session_file(session) {
            return pinned;
        }
        self.session_dir(&session.cwd).join(format!(
            "unresolved-{}.jsonl",
            super::super::sessions::short_id(session.id.as_str())
        ))
    }

    /// See this module's own doc comment ("Transcript row shape") for the
    /// full verification trail. `session_start` rows carry no turn signal
    /// and are skipped. A `user`-role `message` row starts a turn ONLY when
    /// it carries a `text` block AND is not a droid-injected system-context
    /// row ([`is_context_injection`]); any `tool_result` blocks on it
    /// (verified to co-occur on a plain `user` row, never a separate role)
    /// each emit their own [`NormalizedEvent::ToolResult`]. An
    /// `assistant`-role row emits its own `modelId` field (when present) as
    /// [`NormalizedEvent::ModelId`], its concatenated text as
    /// [`NormalizedEvent::AssistantFinal`] (`input_tokens: 0` -- verified
    /// absent from this transcript shape, see "Token usage" in this module's
    /// own doc comment) with an [`NormalizedEvent::AssistantFirstText`]
    /// sibling whenever that text is non-empty, and one
    /// [`NormalizedEvent::ToolCall`] per `tool_use` content block.
    fn parse_events(&self, jsonl: &str) -> Vec<NormalizedEvent> {
        let mut events = Vec::new();
        for line in jsonl.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(row) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if row.get("type").and_then(Value::as_str) != Some("message") {
                continue;
            }
            let row_id = row.get("id").and_then(Value::as_str).unwrap_or_default();
            let Some(message) = row.get("message") else {
                continue;
            };
            let at_ms = row
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(super::super::window::parse_iso8601_utc_ms);

            match message.get("role").and_then(Value::as_str) {
                Some("user") => {
                    if is_context_injection(row_id, message) {
                        continue;
                    }
                    let blocks = message.get("content").and_then(Value::as_array);
                    let has_text = blocks
                        .map(|bs| {
                            bs.iter()
                                .any(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                        })
                        .unwrap_or(false);
                    if has_text {
                        events.push(NormalizedEvent::TurnStart { at_ms });
                    }
                    if let Some(blocks) = blocks {
                        for block in blocks.iter().filter(|b| {
                            b.get("type").and_then(Value::as_str) == Some("tool_result")
                        }) {
                            let is_error = block
                                .get("is_error")
                                .and_then(Value::as_bool)
                                .unwrap_or(false);
                            events.push(NormalizedEvent::ToolResult { is_error });
                        }
                    }
                }
                Some("assistant") => {
                    if let Some(id) = message.get("modelId").and_then(Value::as_str) {
                        events.push(NormalizedEvent::ModelId { id: id.to_string() });
                    }
                    let text = text_of(message);
                    if !text.trim().is_empty() {
                        events.push(NormalizedEvent::AssistantFirstText { at_ms });
                    }
                    events.push(NormalizedEvent::AssistantFinal {
                        text,
                        input_tokens: 0,
                        at_ms,
                    });
                    if let Some(blocks) = message.get("content").and_then(Value::as_array) {
                        for block in blocks
                            .iter()
                            .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
                        {
                            let name = block
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("unknown")
                                .to_string();
                            let raw = block.get("input").map(Value::to_string).unwrap_or_default();
                            events.push(NormalizedEvent::ToolCall {
                                name,
                                input_hash: input_hash(&raw),
                                at_ms,
                            });
                        }
                    }
                }
                _ => {}
            }
        }
        events
    }

    /// Only `user_messages`/`assistant_texts` are populated -- see this
    /// module's own doc comment, "Deliberately UNSUPPORTED", for why
    /// `files_read`/`files_modified`/`tool_errors` stay empty. Droid-injected
    /// system-context rows are excluded from `user_messages` the same way
    /// [`parse_events`] excludes them from `TurnStart`.
    fn structural_context(&self, jsonl: &str, last_n: usize) -> StructuralContext {
        let mut user_messages = Vec::new();
        let mut assistant_texts = Vec::new();
        for line in jsonl.lines() {
            let Ok(row) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            if row.get("type").and_then(Value::as_str) != Some("message") {
                continue;
            }
            let row_id = row.get("id").and_then(Value::as_str).unwrap_or_default();
            let Some(message) = row.get("message") else {
                continue;
            };
            match message.get("role").and_then(Value::as_str) {
                Some("user") if !is_context_injection(row_id, message) => {
                    let text = text_of(message);
                    if !text.trim().is_empty() {
                        user_messages.push(text);
                    }
                }
                Some("assistant") => {
                    let text = text_of(message);
                    if !text.trim().is_empty() {
                        assistant_texts.push(text);
                    }
                }
                _ => {}
            }
        }
        if assistant_texts.len() > last_n {
            assistant_texts.drain(..assistant_texts.len() - last_n);
        }
        StructuralContext {
            user_messages,
            assistant_texts,
            ..StructuralContext::default()
        }
    }

    /// The most recently observed `message.modelId` off an `assistant`-role
    /// row, newest-to-oldest -- mirrors `pi::PiAdapter::model_hint`'s own
    /// `.rev()` approach. See this module's own doc comment for the
    /// verification trail and its one residual (unconfirmed for a built-in,
    /// non-BYOK model).
    fn model_hint(&self, jsonl: &str) -> Option<String> {
        jsonl.lines().rev().find_map(|line| {
            let row = serde_json::from_str::<Value>(line.trim()).ok()?;
            if row.get("type").and_then(Value::as_str) != Some("message") {
                return None;
            }
            let message = row.get("message")?;
            if message.get("role").and_then(Value::as_str) != Some("assistant") {
                return None;
            }
            message.get("modelId")?.as_str().map(str::to_string)
        })
    }

    // No `transcript_usage` override -- see this module's own doc comment,
    // "Deliberately UNSUPPORTED", for why the trait default (`None`) is the
    // honest answer: the `.jsonl` this method receives carries no usage
    // field at all, and the trait signature gives no path to reach the
    // sibling `.settings.json` file that does.

    /// See this module's own doc comment, "Deliberately UNSUPPORTED": no
    /// verified per-tool-call result shape names a file-path argument, so
    /// `structural_context` never derives `files_read`/`files_modified` from
    /// one. `ToolCall`/`ToolResult` events themselves ARE verified and real
    /// (this module's own doc comment), so -- unlike `codex`/`gemini`, whose
    /// `counts_tool_calls` is `false` because their `parse_events` never
    /// emits a `ToolCall` at all -- this stays at the trait default `true`.
    fn compact_command(&self) -> Option<&'static str> {
        Some("/compress")
    }

    /// Verified doc-only (this module's own doc comment, "Doc-only facts"):
    /// `/quit` (alias `exit`). `\r` mirrors every other adapter's own PTY
    /// keystroke terminator.
    fn quit_sequence(&self) -> &'static str {
        "/quit\r"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            marker_signal: false,
            // Verified absent from the transcript this adapter can actually
            // read -- see this module's own doc comment, "Token usage".
            token_usage: false,
            turn_signal: false,
            system_prompt: true,
            events: true,
            // Unverified: droid's own interactive composer paste/submit
            // behavior was never observed (no live REPL was driven here).
            defer_injection_submit: false,
            context_window_tokens: None,
        }
    }

    fn context_window_tokens(&self, model: Option<&str>) -> Option<u64> {
        let vendor = model
            .and_then(catalogue::vendor_of)
            .and_then(catalogue::vendor)?;
        catalogue::context_window(vendor, model)
    }

    /// This adapter's own vendor is resolved per-model (see
    /// [`provider_for_model`]'s own doc comment), so this answers from the
    /// SEAT's vendor when recognized, else the trait default (`""`, "no
    /// verified ladder for this seat") -- mirrors `pi::PiAdapter::
    /// review_model_below` exactly, for the identical reason: `seat` could
    /// be spending any of several vendors' accounts.
    fn review_model_below(&self, seat: Option<&str>) -> &'static str {
        seat.and_then(catalogue::vendor_of)
            .and_then(catalogue::vendor)
            .map(|v| catalogue::rung_below(v, seat))
            .unwrap_or("")
    }

    /// Same per-model vendor resolution as [`review_model_below`].
    fn model_strength(&self, model: &str) -> Option<u8> {
        catalogue::vendor_of(model)
            .and_then(catalogue::vendor)
            .and_then(|v| catalogue::strength(v, model))
    }

    fn launch_prefix_len(&self) -> usize {
        1 + self.bin_args.len()
    }

    /// Verified: `exec --help`, `-m, --model <id>`. Same unverified-on-
    /// interactive gap as [`Self::read_only_args`] documents for
    /// `--only-tools` -- `-m` is cited only against `exec --help` here too --
    /// but `AgentAdapter` has no `interactive_model_args` split to override
    /// (`model_args` is the one method every launch surface shares), so
    /// closing it is out of scope for this fix; `worker_model_args`/
    /// `dispatch_agent` (`mod.rs`) are the call sites that would need one.
    fn model_args(&self, model: &str) -> Vec<String> {
        vec!["-m".to_string(), model.to_string()]
    }

    // No `resume_args`/`session_pin_args` override -- see this module's own
    // doc comment, "Session id: no pin", for why the trait defaults
    // (`None`/empty) are the honest answer here, mirroring
    // `gemini::GeminiAdapter`'s identical situation.

    /// No verified per-run turn-boundary signal for droid -- mirrors
    /// `codex`/`gemini`/`pi`'s own identical no-op.
    fn register_turn_signal(&self, _session: &SessionRef, _socket: &Path) -> TurnSignalSetup {
        TurnSignalSetup {
            env: Vec::new(),
            instructions: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter() -> DroidAdapter {
        DroidAdapter::new(Some("droid"))
    }

    fn flatten(cmd: Command) -> Vec<String> {
        super::super::built_args("droid", &cmd)
    }

    // -- command shapes -----------------------------------------------

    #[test]
    fn headless_cmd_uses_stream_json_and_never_places_the_session_on_argv() {
        let args = flatten(adapter().headless_cmd(
            "do the thing",
            &SessionId::new_v4(),
            &["--extra".to_string()],
        ));
        assert_eq!(args[0], "exec");
        assert!(args.contains(&"-o".to_string()));
        assert!(args.contains(&"stream-json".to_string()));
        assert!(args.contains(&"do the thing".to_string()));
        assert!(args.contains(&"--extra".to_string()));
        assert!(!args.iter().any(|a| a == "-s" || a == "--session-id"));
    }

    #[test]
    fn headless_cmd_stdin_carries_no_positional_prompt() {
        let args = flatten(
            adapter()
                .headless_cmd_stdin(&SessionId::new_v4(), &[])
                .expect("droid has a verified stdin form"),
        );
        assert!(args.contains(&"exec".to_string()));
        assert!(!args.contains(&"do the thing".to_string()));
    }

    #[test]
    fn interactive_cmd_carries_a_positional_prompt() {
        let cmd = adapter().interactive_cmd(Some("review app.tsx"), &[]);
        assert_eq!(flatten(cmd), vec!["review app.tsx".to_string()]);

        let bare = adapter().interactive_cmd(None, &[]);
        assert!(flatten(bare).is_empty());
    }

    #[test]
    fn distiller_cmd_reuses_the_read_only_allow_list_and_delivers_the_prompt_on_stdin() {
        let cmd = adapter().distiller_cmd("gpt-5.6-luna");
        let args = flatten(cmd);
        assert_eq!(args[0], "exec");
        assert!(args.contains(&"--only-tools".to_string()));
        assert!(args.contains(&READ_ONLY_TOOLS.to_string()));
        assert!(args.contains(&"-m".to_string()));
        assert!(args.contains(&"gpt-5.6-luna".to_string()));
        assert!(!args.iter().any(|a| a.contains("do the thing")));

        let no_model = adapter().distiller_cmd("");
        assert!(!flatten(no_model).contains(&"-m".to_string()));
    }

    #[test]
    fn read_only_args_allow_lists_only_inspection_shaped_tools() {
        let args = adapter().read_only_args();
        assert_eq!(
            args,
            vec!["--only-tools".to_string(), "Read,Grep,Glob,LS".to_string()]
        );
        for mutating in ["Execute", "ApplyPatch", "Create", "TodoWrite", "WebSearch"] {
            assert!(
                !args.iter().any(|a| a.contains(mutating)),
                "{mutating} must never appear in the read-only allow-list"
            );
        }
    }

    #[test]
    fn interactive_read_only_args_never_carries_the_exec_only_only_tools_flag() {
        // `--only-tools` is verified only against `exec --help`; applying it
        // to the top-level interactive launch a dashboard pane uses is the
        // same bug codex's `--ignore-rules`/`--ignore-user-config` had.
        let args = adapter().interactive_read_only_args();
        assert!(args.is_empty());
        assert_ne!(args, adapter().read_only_args());
    }

    #[test]
    fn system_prompt_args_uses_the_verified_append_flag() {
        let adapter = adapter();
        assert_eq!(
            adapter.system_prompt_args("be careful"),
            vec![
                "--append-system-prompt".to_string(),
                "be careful".to_string()
            ]
        );
        assert_eq!(adapter.system_prompt_args(""), Vec::<String>::new());
        assert_eq!(
            adapter.user_system_prompt_flag(),
            Some("--append-system-prompt")
        );
        assert_eq!(
            adapter.system_prompt_file_flag(),
            Some("--append-system-prompt-file")
        );
    }

    #[test]
    fn model_args_uses_the_verified_flag() {
        assert_eq!(
            adapter().model_args("claude-sonnet-5"),
            vec!["-m".to_string(), "claude-sonnet-5".to_string()]
        );
    }

    #[test]
    fn resume_args_and_session_pin_args_stay_unsupported() {
        assert_eq!(adapter().resume_args("some-id"), None);
        assert_eq!(adapter().session_pin_args("some-id"), Vec::<String>::new());
    }

    #[test]
    fn quit_and_compact_use_the_verified_slash_commands() {
        assert_eq!(adapter().quit_sequence(), "/quit\r");
        assert_eq!(adapter().compact_command(), Some("/compress"));
    }

    #[test]
    fn capabilities_report_real_events_but_no_token_usage_marker_or_turn_signal() {
        let caps = adapter().capabilities();
        assert!(caps.events);
        assert!(caps.system_prompt);
        assert!(!caps.token_usage);
        assert!(!caps.marker_signal);
        assert!(!caps.turn_signal);
        assert!(adapter().counts_tool_calls());
    }

    #[test]
    fn detect_matches_the_bare_binary_and_windows_extensions() {
        let a = adapter();
        assert!(a.detect(&["droid".to_string()]));
        assert!(a.detect(&["droid.exe".to_string()]));
        assert!(a.detect(&["droid.cmd".to_string()]));
        assert!(a.detect(&["/usr/local/bin/droid".to_string()]));
        assert!(!a.detect(&["codex".to_string()]));
        assert!(!a.detect(&[]));
    }

    #[test]
    fn all_registers_droid() {
        let names: Vec<&str> = super::super::all(None).iter().map(|a| a.name()).collect();
        assert!(names.contains(&"droid"), "got {names:?}");
    }

    #[test]
    fn launch_prefix_len_counts_every_bin_arg_token() {
        assert_eq!(
            DroidAdapter::new(Some("sh /tmp/stub.sh")).launch_prefix_len(),
            2
        );
        assert_eq!(DroidAdapter::new(None).launch_prefix_len(), 1);
    }

    // -- provider / ladder resolution ----------------------------------

    #[test]
    fn provider_for_model_resolves_the_billed_vendor_from_bare_model_ids() {
        let a = adapter();
        assert_eq!(a.provider(), "factory");
        assert_eq!(a.provider_for_model(Some("claude-sonnet-5")), "anthropic");
        assert_eq!(a.provider_for_model(Some("gpt-5.6-sol")), "openai");
        assert_eq!(
            a.provider_for_model(Some("gemini-3.1-pro-preview")),
            "google"
        );
        assert_eq!(a.provider_for_model(Some("custom:mock-model")), "factory");
        assert_eq!(a.provider_for_model(None), "factory");
    }

    #[test]
    fn ladder_methods_answer_from_the_models_own_vendor_when_recognized() {
        let a = adapter();
        let anthropic = catalogue::vendor("anthropic").expect("anthropic is registered");
        assert_eq!(
            a.review_model_below(Some("claude-sonnet-5")),
            catalogue::rung_below(anthropic, Some("claude-sonnet-5"))
        );
        assert_eq!(
            a.model_strength("claude-sonnet-5"),
            catalogue::strength(anthropic, "claude-sonnet-5")
        );
        assert_eq!(
            a.context_window_tokens(Some("claude-sonnet-5")),
            catalogue::context_window(anthropic, Some("claude-sonnet-5"))
        );

        // An unrecognized model/seat falls back to the trait's own "no
        // verified ladder"/"unknown capacity" answers, never a guessed
        // vendor.
        assert_eq!(a.review_model_below(Some("custom:mock-model")), "");
        assert_eq!(a.model_strength("custom:mock-model"), None);
        assert_eq!(a.context_window_tokens(Some("custom:mock-model")), None);
        assert_eq!(a.context_window_tokens(None), None);
    }

    // -- session_dir_slug -------------------------------------------------

    #[test]
    fn session_dir_slug_matches_the_verified_real_binary_transform() {
        // Verified probe 1: `C:\Users\josj\AppData\Local\Temp`.
        assert_eq!(
            session_dir_slug(Path::new("C:\\Users\\josj\\AppData\\Local\\Temp")),
            "-C-Users-josj-AppData-Local-Temp"
        );
        // Verified probe 2: a path with an existing hyphen and a literal
        // space, both of which must survive unmangled.
        assert_eq!(
            session_dir_slug(Path::new(
                "D:/GitHub/zirv-w2-droid/target/probe-cwd-a/sub dir"
            )),
            "-D-GitHub-zirv-w2-droid-target-probe-cwd-a-sub dir"
        );
    }

    // -- transcript_path ------------------------------------------------

    #[test]
    fn transcript_path_is_unresolved_when_nothing_has_been_registered() {
        let home = tempfile::tempdir().expect("home");
        let state = tempfile::tempdir().expect("state");
        let a = DroidAdapter::new(None)
            .with_home(home.path().to_path_buf())
            .with_state_root(state.path().to_path_buf());
        let session = SessionRef {
            id: SessionId::parse("11111111-2222-4333-8444-555555555555"),
            cwd: PathBuf::from("/work/repo"),
        };
        let resolved = a.transcript_path(&session);
        assert!(!resolved.exists());
        assert!(resolved.to_string_lossy().contains("unresolved-"));
    }

    #[test]
    fn transcript_path_finds_and_pins_the_real_uuid_named_file() {
        let home = tempfile::tempdir().expect("home");
        let state = tempfile::tempdir().expect("state");
        let repo = tempfile::tempdir().expect("repo");
        let a = DroidAdapter::new(None)
            .with_home(home.path().to_path_buf())
            .with_state_root(state.path().to_path_buf());
        let session = SessionRef {
            id: SessionId::parse("11111111-2222-4333-8444-555555555555"),
            cwd: repo.path().to_path_buf(),
        };

        // Register the session so `pinned_session_file` has a `started_at`
        // floor to resolve against, mirroring `codex`/`gemini`'s own test
        // pattern.
        let state_dir =
            crate::commands::ctx::state::StateDir::from_root(state.path().to_path_buf());
        let short = crate::commands::ctx::sessions::short_id(session.id.as_str());
        std::fs::create_dir_all(state_dir.sessions()).expect("mkdir sessions");
        let mut record = crate::commands::ctx::sessions::Record::new(
            session.id.as_str(),
            "droid",
            repo.path(),
            crate::commands::ctx::sessions::Verb::Wrap,
        );
        record.started_at = 0; // floor of zero admits any real file's mtime
        std::fs::write(
            state_dir.sessions().join(format!("{short}.json")),
            serde_json::to_string(&record).expect("record json"),
        )
        .expect("write record");

        let dir = home
            .path()
            .join(".factory")
            .join("sessions")
            .join(session_dir_slug(repo.path()));
        std::fs::create_dir_all(&dir).expect("mkdir session dir");
        let real = dir.join("699d3bc5-08dc-40c3-8ea6-febaebdecbaa.jsonl");
        std::fs::write(&real, "{\"type\":\"session_start\"}\n").expect("write session file");
        // A non-`.jsonl` sibling must never be treated as a candidate.
        std::fs::write(
            dir.join("699d3bc5-08dc-40c3-8ea6-febaebdecbaa.settings.json"),
            "{}",
        )
        .expect("write settings sibling");

        let resolved = a.transcript_path(&session);
        assert_eq!(resolved, real);

        // A later, newer file must not steal the already-pinned answer.
        std::fs::write(
            dir.join("newer-00000000-0000-4000-8000-000000000000.jsonl"),
            "{}",
        )
        .expect("write newer file");
        assert_eq!(a.transcript_path(&session), real);
    }

    // -- parse_events / structural_context / model_hint ------------------

    fn fixture_jsonl() -> String {
        std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/droid/session.jsonl"),
        )
        .expect("fixture read")
    }

    #[test]
    fn fixture_every_line_is_valid_json() {
        for line in fixture_jsonl().lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            serde_json::from_str::<Value>(line)
                .unwrap_or_else(|e| panic!("invalid JSON line {line:?}: {e}"));
        }
    }

    #[test]
    fn parse_events_filters_the_injected_system_context_row() {
        let events = adapter().parse_events(&fixture_jsonl());
        let turn_starts = events
            .iter()
            .filter(|e| matches!(e, NormalizedEvent::TurnStart { .. }))
            .count();
        // The fixture carries one context-injection row and one real user
        // turn; only the real one may start a turn.
        assert_eq!(turn_starts, 1, "got {events:?}");
    }

    #[test]
    fn parse_events_maps_tool_calls_results_and_final_text_from_the_fixture() {
        let events = adapter().parse_events(&fixture_jsonl());

        assert!(
            events
                .iter()
                .any(|e| matches!(e, NormalizedEvent::ToolCall { name, .. } if name == "Execute"))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, NormalizedEvent::ToolResult { is_error: true }))
        );

        let finals: Vec<&NormalizedEvent> = events
            .iter()
            .filter(|e| matches!(e, NormalizedEvent::AssistantFinal { .. }))
            .collect();
        assert_eq!(finals.len(), 2);
        match finals[0] {
            NormalizedEvent::AssistantFinal { text, .. } => assert_eq!(text, ""),
            _ => unreachable!(),
        }
        match finals[1] {
            NormalizedEvent::AssistantFinal { text, .. } => {
                assert_eq!(text, "Added GET /health returning 200 OK.")
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn model_hint_reads_the_newest_assistant_model_id() {
        assert_eq!(
            adapter().model_hint(&fixture_jsonl()),
            Some("custom:Mock-Model-1".to_string())
        );
    }

    #[test]
    fn structural_context_excludes_injected_context_and_caps_assistant_texts() {
        let ctx = adapter().structural_context(&fixture_jsonl(), 1);
        assert_eq!(ctx.user_messages, vec!["please fix the bug".to_string()]);
        assert_eq!(
            ctx.assistant_texts,
            vec!["Added GET /health returning 200 OK.".to_string()]
        );
        assert!(ctx.files_read.is_empty());
        assert!(ctx.files_modified.is_empty());
    }

    #[test]
    fn parse_events_is_line_local_across_a_split_chunk() {
        // The incremental scoring path feeds fragments cut at newlines; a
        // whole-file parse must equal the concatenation of piecewise parses.
        let a = adapter();
        let whole = fixture_jsonl();
        let lines: Vec<&str> = whole.lines().collect();
        let mid = lines.len() / 2;
        let first_half = lines[..mid].join("\n");
        let second_half = lines[mid..].join("\n");

        let mut piecewise = a.parse_events(&first_half);
        piecewise.extend(a.parse_events(&second_half));
        assert_eq!(piecewise, a.parse_events(&whole));
    }
}
