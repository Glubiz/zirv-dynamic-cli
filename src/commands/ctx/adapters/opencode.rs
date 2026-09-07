//! Issue #385: adapter for [OpenCode](https://opencode.ai) (npm package
//! `opencode-ai`, GitHub `anomalyco/opencode`). OpenCode is not installed on
//! this machine, so every fact this module relies on is verified against the
//! actual published release **v1.18.29** (2026-09-07) rather than probed or
//! guessed -- source files were fetched at the exact git tag `v1.18.29` (not
//! `dev`, which can be ahead of what is actually shipped), via `gh api
//! repos/anomalyco/opencode/contents/<path>?ref=v1.18.29`. File/symbol
//! citations are attached at each fact below. Anything this pass could not
//! verify is left on the trait's own "no verified mechanism" default rather
//! than guessed at -- see the per-method doc comments for exactly what that
//! covers.
//!
//! ## CLI surface (verified: `packages/opencode/src/cli/cmd/run.ts`,
//! `packages/opencode/src/cli/cmd/tui.ts`, both at `v1.18.29`)
//!
//! Headless: `opencode run [message..]`, with `--format {default,json}`
//! (default `"default"`), `--agent <name>`, `-m/--model <provider/model>`,
//! `-s/--session <id>` (resumes an EXISTING session only -- `run.ts`'s own
//! `session()` helper calls `sdk.session.get({sessionID})` and exits
//! `"Session not found"` when it does not already exist, so a zirv-minted id
//! can never be adopted this way), `--fork` (requires `--continue`/
//! `--session`), `--auto` (auto-approves anything not explicitly denied --
//! never emitted by this adapter, since it would widen rather than narrow).
//! No `--system` flag exists on `run` (grepped the full source: absent) and
//! no `-q`/`--quiet` flag exists either (also absent) -- both are UNSUPPORTED
//! here, correcting an earlier (wrong) community-doc claim that a `--system`
//! flag exists.
//!
//! Interactive: bare `opencode [project]` (`TuiThreadCommand`, `command: "$0
//! [project]"`), with `--prompt <text>` (a real flag, NOT a positional --
//! verified in `tui.ts`), plus the same `--agent`/`-m`/`-s`/`-c`/`--fork`/
//! `--auto` surface as `run`. No `--quiet` here either.
//!
//! ## Storage (verified: `packages/core/src/global.ts`,
//! `packages/core/src/database/database.ts`, `packages/core/src/session/
//! sql.ts`, `packages/schema/src/session-message.ts`, all at `v1.18.29`)
//!
//! `Global.Path.data = path.join(xdgData, "opencode")`, where `xdgData` comes
//! from the `xdg-basedir` npm package: `env.XDG_DATA_HOME ||
//! path.join(homedir, ".local", "share")` -- verified from `xdg-basedir`'s
//! own `index.js` (`sindresorhus/xdg-basedir`). This package does **not**
//! special-case Windows or macOS, so on every OS the default (absent
//! `XDG_DATA_HOME`) is `<home>/.local/share/opencode` -- **not**
//! `%LOCALAPPDATA%\opencode\data\`, a claim a third-party doc site made that
//! this pass could not confirm against the actual source and is not relied
//! on here.
//!
//! The database path (`database.ts::path()`): `OPENCODE_DB` overrides
//! entirely (`:memory:` or an absolute path used as-is, else joined under
//! `Global.Path.data`); otherwise `opencode.db` under `Global.Path.data` for
//! the `"latest"|"beta"|"prod"` installation channels (what an npm-installed
//! release always reports), or `opencode-<channel>.db` for a dev/canary
//! build from source -- this adapter always assumes `opencode.db`, matching
//! every ordinary install, and honors `OPENCODE_DB` when set, but does not
//! reproduce the channel-suffix branch (a residual affecting only an
//! operator running OpenCode built from source on a non-release channel).
//!
//! Schema (Drizzle `sqliteTable` definitions, `session/sql.ts`): `session`
//! carries `id`, `directory` (the session's cwd, `DatabasePath.
//! directoryColumn()`), `time_created`/`time_updated` (via the shared
//! `Timestamps` helper, `database/schema.sql.ts`: `time_created` set once on
//! insert, `time_updated` refreshed by Drizzle's `$onUpdate` on every UPDATE
//! -- confirming rows are mutated in place, not append-only). `session_
//! message` carries `id`, `session_id`, `type`, `seq`, `time_created`/
//! `time_updated`, and `data` (a JSON-encoded TEXT column, `Omit<Message
//! ["Encoded"], "type" | "id">` -- i.e. every field of the tagged union in
//! `packages/schema/src/session-message.ts` EXCEPT `type`/`id`, which live as
//! their own top-level columns).
//!
//! **Known limitation from the `$onUpdate` fact above:** [`ShadowTranscript::
//! sync_sqlite`](super::super::transcript_source::ShadowTranscript::sync_sqlite)'s
//! cursor is `WHERE rowid > ?1` -- correct for a genuinely append-only log,
//! but a `session_message` row that is UPDATED after this adapter has already
//! synced it (e.g. an `assistant` row's tool-call state advancing from
//! `"pending"`/`"running"` to `"completed"`/`"error"` in place, which the
//! `time_updated` column's own `$onUpdate` proves happens) will never be
//! re-synced: the shadow keeps whatever the row looked like at the moment its
//! `rowid` first crossed the cursor. [`parse_events`](OpenCodeAdapter::
//! parse_events) reads a tool's status defensively for exactly this reason
//! (see its own doc comment) rather than assuming a later poll will correct
//! an in-flight status.
//!
//! ## System prompt and permissions (verified: `packages/core/src/v1/config/
//! agent.ts`, `.../v1/config/permission.ts`, `packages/opencode/src/session/
//! llm/request.ts`, `packages/core/src/config/config.ts`, all at `v1.18.29`)
//!
//! `agent.<name>.prompt` (config key, plain optional string) genuinely
//! REPLACES the harness's own provider-identity header for that agent, never
//! merely appending to it -- verified at the exact call site,
//! `request.ts::prepare`: `...(input.agent.prompt ? [input.agent.prompt] :
//! SystemPrompt.provider(input.model)), ...input.system, ...`. Environment
//! info, `AGENTS.md`/`CLAUDE.md` instructions, MCP and skills text
//! (`input.system`, assembled in `session/prompt.ts`) still follow
//! afterward regardless -- an agent's `prompt` cannot suppress those. This is
//! a genuine one-shot override channel, the same shape codex's `-c
//! developer_instructions=...` and claude's launch-settings file are.
//!
//! For a BRAND NEW agent name (not one of OpenCode's own built-ins), `agent.
//! ts`'s own config-merge loop starts that agent's `prompt` at `undefined`
//! before applying `value.prompt ?? item.prompt` -- so a zirv-defined agent's
//! `prompt` is exactly the text zirv supplied, nothing else layered under it
//! at that stage.
//!
//! `agent.<name>.permission` (verified schema, `v1/config/permission.ts`):
//! each of `read`, `edit`, `glob`, `grep`, `list`, `bash`, `task`,
//! `external_directory`, `lsp`, `skill` accepts either a plain `"ask"|
//! "allow"|"deny"` or a per-pattern object; `todowrite`, `question`,
//! `webfetch`, `websearch`, `doom_loop` accept only a plain action. There is
//! **no separate `"write"` key** -- `v1/config/agent.ts`'s own `normalize()`
//! folds the deprecated `tools.write`/`tools.edit`/`tools.patch` booleans
//! into `permission.edit`, so denying `edit` is what denies writes.
//!
//! The only channel to DELIVER a custom agent definition for a single launch
//! is a JSON config file, pointed at by the `OPENCODE_CONFIG` environment
//! variable (verified in `config.ts`: `if (Flag.OPENCODE_CONFIG) { yield*
//! merge(Flag.OPENCODE_CONFIG, ...) }`) -- there is no `--config <path>` CLI
//! flag (a feature request for one, issue #2066 on the upstream tracker, is
//! still open as of this pass). Since [`AgentAdapter::system_prompt_args`]/
//! [`AgentAdapter::read_only_args`] can only return argv tokens with no env
//! channel of their own, this adapter materializes the config file from
//! inside those methods (the same "I/O inside a method that looks pure"
//! precedent `CodexAdapter::pinned_rollout` already sets) and has its own
//! [`headless_cmd`](OpenCodeAdapter::headless_cmd)/[`interactive_cmd`]
//! (OpenCodeAdapter::interactive_cmd)/[`distiller_cmd`](OpenCodeAdapter::
//! distiller_cmd) -- which build the real [`Command`] and so can set env
//! directly -- recognise the sentinel `--agent <name>` pair these methods
//! emit and attach `OPENCODE_CONFIG` accordingly. See [`apply_zirv_agent_env`]
//! for the exact mechanism, and [`sandbox_residual_note`](AgentAdapter::
//! sandbox_residual_note) below for the residual this design carries:
//! `config.ts`'s own merge order is `global config -> OPENCODE_CONFIG flag ->
//! project configs -> .opencode directories`, so a repository's own
//! `opencode.json`/`.opencode/agent/<name>.md` -- an UNTRUSTED, repo-owned
//! surface per this crate's own policy -- loads AFTER a zirv-set
//! `OPENCODE_CONFIG` file and can redefine an agent sharing zirv's chosen
//! name, widening what zirv pinned. This is disclosed, never silently
//! assumed safe.
//!
//! ## Compaction and quitting (verified: `opencode.ai/docs/commands/`,
//! `opencode.ai/docs/keybinds/`)
//!
//! No slash command exists for compaction (the documented built-ins are
//! `/init`, `/undo`, `/redo`, `/share`, `/help` only) -- compaction is either
//! fully automatic or triggered by the `session_compact` keybind (default
//! `<leader>c`), which this adapter has no safe way to encode as injectable
//! text (a leader-key chord is operator-configurable, unlike a fixed slash
//! command). [`compact_command`](OpenCodeAdapter::compact_command) returns
//! `None`, exactly mirroring `CodexAdapter`'s own answer and relying on the
//! same existing fallback+verification path in `wrap.rs` (`Action::Compact`
//! injects the trait's own default `"/compact"` literal and then verifies
//! whether real compaction actually followed, reporting "not verified"
//! rather than crashing when it did not).
//!
//! The documented `app_exit` keybind is `"ctrl+c,ctrl+d,<leader>q"`.
//! `quit_sequence` never sends `ctrl+c` (`\x03`) -- `wrap::quit_child`'s own
//! doc comment explains why that byte specifically must never reach a pty
//! master again (F1: on Windows ConPTY, `\x03` is not delivered to the one
//! child a supervisor owns, it is broadcast by conhost as a console control
//! event to every process sharing that pseudoconsole). Ctrl-D (`\x04`, EOF)
//! carries no such broadcast semantics on Windows -- it is an ordinary
//! character, not one of the special `GenerateConsoleCtrlEvent` codes -- so
//! this adapter uses `"\x04"`, the one documented exit chord that does not
//! collide with the banned byte.
//!
//! ## Providers and the model ladder (verified: `catalogue.rs`, issue #381)
//!
//! OpenCode itself is not one vendor's account: `-m provider/model` can name
//! any of 75+ providers Models.dev knows. `provider()` is the static
//! `"opencode"` slug (issue #382's own multi-provider precedent -- no single
//! usage-window account to report). [`provider_for_model`](AgentAdapter::
//! provider_for_model) resolves the ACTUAL billed vendor via `catalogue::
//! vendor_of`, falling back to `"opencode"` only when the model string names
//! no vendor this crate's catalogue recognises -- see that method's own trait
//! doc comment for why this distinction exists. The ladder methods
//! (`review_model_below`/`model_strength`/`context_window_tokens`) resolve
//! the SAME way: through the model's own vendor when known, else the trait's
//! plain "nothing verified" default (`""`/`None`/`None`) -- there is no
//! OpenCode-specific ladder of its own to fall back to, unlike claude/codex.
//!
//! ## Session identification (verified: `packages/schema/src/session-id.ts`,
//! `packages/schema/src/identifier.ts`)
//!
//! A session id is always `"ses_" + descending()`, where `descending()`
//! (`identifier.ts`) emits characters only from `[0-9A-Za-z]` after the
//! literal `ses_` prefix -- i.e. the full id's charset is `[0-9A-Za-z_]`.
//! [`is_valid_opencode_id`] enforces exactly that before any id is
//! interpolated into a SQL string (`ShadowTranscript::sync_sqlite`'s query
//! contract has no parameter slot for it -- see that method's own doc
//! comment), refusing anything else outright.
//!
//! There is no way to CREATE a session under a caller-chosen id (`run.ts`'s
//! `session()` helper requires `--session` to already exist, verified
//! above), so unlike `CodexAdapter::pinned_rollout` this cannot ever expect
//! zirv's own uuid to appear as-is; instead [`pinned_session_id`] discovers
//! the newest `session` row created at or after this zirv session's own
//! registered start time (`sessions::load_record`), preferring one whose
//! `directory` column matches this session's cwd when any do -- the same
//! "earliest plausible match, cwd as the tiebreaker" heuristic `codex::
//! resolve_rollout` already uses, with the identical residual (two OpenCode
//! runs started in the same directory within the same poll cannot be told
//! apart) -- and pins it under `StateDir::rollouts()`, exactly like codex's
//! own rollout pin.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use super::super::CtxResult;
use super::super::catalogue;
use super::super::event::{
    self, Capabilities, NormalizedEvent, SessionId, SessionRef, StructuralContext, TranscriptUsage,
};
use super::super::sessions;
use super::super::state::{self, StateDir};
use super::super::transcript_source::ShadowTranscript;
use super::{AgentAdapter, ResolvedProgram, TurnSignalSetup};

/// The fixed agent name naming zirv's structurally-deny reviewer/distiller
/// pin (see [`OpenCodeAdapter::read_only_args`]). A fixed, public (this crate
/// is open source) name rather than a random one -- obscurity is not this
/// pin's protection, the materialized `permission` map is.
const READ_ONLY_AGENT: &str = "zirv-read-only";

/// Prefix identifying a zirv-materialized system-prompt agent
/// ([`OpenCodeAdapter::system_prompt_args`]) inside an `extra` argv slice, so
/// [`OpenCodeAdapter::apply_zirv_agent_env`] can recognise one without a
/// shared lookup table: the fingerprint naming the config file is embedded
/// directly in the agent name.
const SYSTEM_PROMPT_AGENT_PREFIX: &str = "zirv-sp-";

/// Verified facts backing this adapter are cited per-fact in the module doc
/// comment above (opencode-ai v1.18.29, `anomalyco/opencode` at git tag
/// `v1.18.29`). Unlike `CodexAdapter`, OpenCode is never installed anywhere
/// this pass could probe, so `ready()` only ever refuses the one thing it
/// can know without a live binary: a bare name that resolves (via `PATH`) to
/// a file this OS cannot execute at all -- identical to `CodexAdapter::
/// ready`'s own reasoning.
#[derive(Debug, Clone)]
pub struct OpenCodeAdapter {
    program: String,
    bin_args: Vec<String>,
    home: Option<PathBuf>,
    /// Test seam only: pins the zirv state root this adapter resolves the
    /// session registry, its own session-id pin, and its zirv-owned config
    /// files from, instead of the real platform state directory. Mirrors
    /// `CodexAdapter::forced_state_root` exactly.
    #[cfg(test)]
    forced_state_root: Option<PathBuf>,
    /// Test seam only: pins `db_path()`'s answer instead of resolving
    /// `OPENCODE_DB`/`data_dir()`. Deliberately a field, not a mutation of
    /// the real `OPENCODE_DB` process environment variable: edition 2024
    /// makes `std::env::set_var` `unsafe` precisely because it races other
    /// threads, and the full (non-nextest) serial suite runs every test in
    /// one process.
    #[cfg(test)]
    forced_db_path: Option<PathBuf>,
}

impl OpenCodeAdapter {
    /// `bin` may carry arguments, mirroring `CodexAdapter::new`/
    /// `ClaudeAdapter::new` exactly.
    pub fn new(bin: Option<&str>) -> Self {
        let raw = bin.unwrap_or("opencode").trim();
        let mut parts = raw.split_whitespace().map(str::to_string);
        let program = parts.next().unwrap_or_else(|| "opencode".to_string());
        Self {
            program,
            bin_args: parts.collect(),
            home: None,
            #[cfg(test)]
            forced_state_root: None,
            #[cfg(test)]
            forced_db_path: None,
        }
    }

    /// Test seam: pins the home directory `data_dir`/`db_path` are built
    /// from.
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

    /// Test seam: see the field's own doc comment.
    #[cfg(test)]
    pub fn with_db_path(mut self, path: PathBuf) -> Self {
        self.forced_db_path = Some(path);
        self
    }

    /// Every command starts here, mirroring `CodexAdapter::base`/
    /// `ClaudeAdapter::base` exactly: the program is routed through
    /// `super::resolve_program` so an npm-installed shim (if one is ever
    /// produced by a different install method than the native binary this
    /// module's own doc comment cites) still launches on Windows.
    ///
    /// The published `opencode-ai` npm package ships a native `opencode.exe`
    /// (verified: `npm pack opencode-ai@1.18.29` in this worktree's own
    /// `target/opencode-pack/` produced a `bin/opencode.exe` entry, a real
    /// launcher binary, not a `.cmd`/`.bat` text shim requiring `cmd.exe` --
    /// unlike claude/codex's npm-installed JS entry points), so the trait's
    /// own default `launches_through_cmd_shim`/`headless_cmd_stdin` behavior
    /// (derived from `resolve_program`'s resolution of `program()`) is
    /// already correct here and is not overridden.
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

    #[cfg(test)]
    fn state_dir(&self) -> Option<StateDir> {
        self.forced_state_root.clone().map(StateDir::from_root)
    }

    #[cfg(not(test))]
    fn state_dir(&self) -> Option<StateDir> {
        StateDir::resolve(&super::super::config::env_from_process()).ok()
    }

    /// `Global.Path.data` (`packages/core/src/global.ts`, verified above):
    /// `$XDG_DATA_HOME/opencode`, else `<home>/.local/share/opencode` on
    /// every OS -- the `xdg-basedir` npm package this fact is verified
    /// against special-cases nothing.
    fn data_dir(&self) -> PathBuf {
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| self.home_dir().join(".local").join("share"));
        base.join("opencode")
    }

    /// `database.ts::path()` (verified above): `OPENCODE_DB` overrides
    /// entirely (`:memory:` literal, or an absolute path used as-is, else
    /// joined under `data_dir()`); otherwise `opencode.db` under
    /// `data_dir()` -- the name every ordinary (`"latest"|"beta"|"prod"`
    /// channel) npm install reports. The dev/canary
    /// `opencode-<channel>.db` branch is not reproduced (see the module doc
    /// comment's own residual note).
    fn db_path(&self) -> PathBuf {
        #[cfg(test)]
        if let Some(forced) = &self.forced_db_path {
            return forced.clone();
        }
        if let Ok(raw) = std::env::var("OPENCODE_DB") {
            if raw == ":memory:" {
                return PathBuf::from(raw);
            }
            let candidate = PathBuf::from(&raw);
            if candidate.is_absolute() {
                return candidate;
            }
            return self.data_dir().join(candidate);
        }
        self.data_dir().join("opencode.db")
    }

    /// Where a zirv-owned, per-purpose OpenCode config file lives, under this
    /// adapter's own corner of the zirv state root -- a sibling convention to
    /// `StateDir::rollouts()`/`StateDir::shadow()`, not a new top-level
    /// `StateDir` accessor, since nothing outside this module ever needs to
    /// name it.
    fn runtime_dir(state: &StateDir) -> PathBuf {
        state.root().join("runtime").join("opencode")
    }

    fn read_only_config_path(state: &StateDir) -> PathBuf {
        Self::runtime_dir(state).join("read-only.json")
    }

    /// The agent name a given composed system prompt materializes to:
    /// content-fingerprinted (via the crate's existing FNV-1a `event::
    /// input_hash`) so two different prompts never collide on one file, and
    /// so [`apply_zirv_agent_env`] can recover the same config path from the
    /// name alone, with no shared lookup table between this method and
    /// [`OpenCodeAdapter::system_prompt_args`].
    fn system_prompt_agent_name(prompt: &str) -> String {
        format!(
            "{SYSTEM_PROMPT_AGENT_PREFIX}{:016x}",
            event::input_hash(prompt)
        )
    }

    fn system_prompt_config_path(state: &StateDir, agent_name: &str) -> PathBuf {
        Self::runtime_dir(state).join(format!("{agent_name}.json"))
    }

    /// Writes `{"agent": {<agent_name>: <entry>}}` to `path`, best-effort
    /// (private, atomic write via `state::write_private`, mirroring
    /// `ClaudeAdapter::launch_settings_path`'s own fallback shape: a write
    /// failure here costs the caller its restriction/prompt, never a broken
    /// launch, because callers only ever add the `--agent` argv token when
    /// this returns `Ok`).
    fn write_agent_config(
        path: &Path,
        agent_name: &str,
        entry: serde_json::Map<String, Value>,
    ) -> std::io::Result<()> {
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        state::create_private_dir_all(dir)?;
        let mut agent_map = serde_json::Map::new();
        agent_map.insert(agent_name.to_string(), Value::Object(entry));
        let mut root = serde_json::Map::new();
        root.insert("agent".to_string(), Value::Object(agent_map));
        let mut body =
            serde_json::to_string_pretty(&Value::Object(root)).map_err(std::io::Error::other)?;
        body.push('\n');
        state::write_private(path, &body)
    }

    /// The read-only pin's own `permission` map: `bash`/`edit`/`webfetch`
    /// denied per the brief's own requirement (verified keys, `v1/config/
    /// permission.ts`; `edit` is what denies writes -- there is no separate
    /// `"write"` key, see the module doc comment), plus `websearch`/
    /// `external_directory` (the same network/filesystem-escape shape as
    /// `webfetch`/`edit`) and `task` (denying subagent spawn: a fresh
    /// subagent's own permission does NOT inherit this agent's restriction --
    /// verified in `agent.ts`'s config-merge loop, a new agent always starts
    /// from `Permission.merge(defaults, user)`, never the parent's resolved
    /// permission -- so leaving `task` open would be a live bypass of this
    /// entire pin).
    fn read_only_permission() -> serde_json::Map<String, Value> {
        let mut permission = serde_json::Map::new();
        for key in [
            "bash",
            "edit",
            "webfetch",
            "websearch",
            "external_directory",
            "task",
        ] {
            permission.insert(key.to_string(), Value::String("deny".to_string()));
        }
        permission
    }

    /// Best-effort materialization of the read-only agent's config file.
    /// `Ok(())` only when the file is actually in place -- callers must
    /// never emit `--agent zirv-read-only` unless this succeeded, since an
    /// agent name OpenCode cannot resolve fails the whole launch outright
    /// (a launch surface this pass could not verify tolerates gracefully).
    fn materialize_read_only_config(state: &StateDir) -> std::io::Result<()> {
        let mut entry = serde_json::Map::new();
        entry.insert(
            "permission".to_string(),
            Value::Object(Self::read_only_permission()),
        );
        Self::write_agent_config(&Self::read_only_config_path(state), READ_ONLY_AGENT, entry)
    }

    /// Sets `OPENCODE_CONFIG` on `cmd` when `extra` carries one of this
    /// adapter's own sentinel `--agent <name>` pairs -- the mechanism the
    /// module doc comment's "System prompt and permissions" section
    /// describes. Never touches an operator's own unrelated `--agent`
    /// choice: only the two name shapes this adapter itself ever emits
    /// (`READ_ONLY_AGENT`, `SYSTEM_PROMPT_AGENT_PREFIX`-prefixed) are
    /// recognised.
    ///
    /// **Known interaction (documented, not fixed):** `extend_read_only_args`
    /// (`adapters/mod.rs`) appends this adapter's `read_only_args()` after
    /// whatever a caller already put in `extra` -- so a launch that requested
    /// BOTH a system prompt (`dispatch_agent`'s own `system_prompt_args`
    /// call) AND the read-only pin ends up with two `--agent` tokens, and
    /// OpenCode's single-valued `--agent` flag takes the LAST one: the
    /// read-only agent wins, and this loop's last-write-wins env assignment
    /// agrees (it, too, ends on the read-only path). The system-prompt text
    /// is silently dropped for that one launch. This is the safe-direction
    /// failure -- the read-only restriction is what actually matters for a
    /// distiller/reviewer child -- so it is accepted and documented rather
    /// than solved with a merged-config redesign this pass had no time to
    /// verify safely.
    fn apply_zirv_agent_env(&self, cmd: &mut Command, extra: &[String]) {
        let Some(state) = self.state_dir() else {
            return;
        };
        for pair in extra.windows(2) {
            if pair[0] != "--agent" {
                continue;
            }
            let name = pair[1].as_str();
            if name == READ_ONLY_AGENT {
                cmd.env("OPENCODE_CONFIG", Self::read_only_config_path(&state));
            } else if name.starts_with(SYSTEM_PROMPT_AGENT_PREFIX) {
                cmd.env(
                    "OPENCODE_CONFIG",
                    Self::system_prompt_config_path(&state, name),
                );
            }
        }
    }

    /// Where [`pinned_session_id`](Self::pinned_session_id) records the
    /// OpenCode session id this zirv session's own child actually created --
    /// a sibling of `CodexAdapter`'s own `<short>.path` pin under the SAME
    /// `StateDir::rollouts()` directory (that accessor's own doc comment
    /// already describes it generically as "one tiny pointer file per
    /// session"), just naming a session id instead of a rollout file path.
    fn session_pin_path(state: &StateDir, short: &str) -> PathBuf {
        state.rollouts().join(format!("{short}.opencode-session"))
    }

    /// Resolution 2 of [`AgentAdapter::transcript_path`]: reads a previously
    /// written pin, or discovers and pins the session this zirv session's
    /// own child created. See the module doc comment's "Session
    /// identification" section for the discovery heuristic and its residual.
    fn pinned_session_id(&self, state: &StateDir, session: &SessionRef) -> Option<String> {
        let short = sessions::short_id(session.id.as_str());
        let pin = Self::session_pin_path(state, &short);
        if let Ok(recorded) = std::fs::read_to_string(&pin) {
            let recorded = recorded.trim().to_string();
            if is_valid_opencode_id(&recorded) {
                return Some(recorded);
            }
        }
        let record = sessions::load_record(state, &short)?;
        let started_ms = record.started_at.saturating_mul(1_000);
        let resolved = resolve_session_id(&self.db_path(), started_ms, &session.cwd)?;
        if state::create_private_dir_all(&state.rollouts()).is_ok() {
            let _ = state::write_private(&pin, &resolved);
        }
        Some(resolved)
    }
}

/// The full verified id charset (module doc comment's "Session
/// identification" section): `ses_` followed only by `[0-9A-Za-z]`. Refuses
/// anything else outright -- this is what makes interpolating the id
/// directly into `ShadowTranscript::sync_sqlite`'s query string safe, since
/// the query contract has no parameter slot for it.
fn is_valid_opencode_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The `session.id` most plausibly created by a session that began at
/// `started_ms` and runs in `cwd`: the earliest `session` row created at or
/// after `started_ms`, restricted to rows whose `directory` matches `cwd`
/// whenever any do. Mirrors `codex::resolve_rollout`'s own heuristic and
/// residual exactly (two OpenCode runs started in the same directory within
/// the same poll cannot be told apart). A missing/unopenable database, or a
/// query that cannot prepare, reads as "nothing to pin yet" -- never a guess.
fn resolve_session_id(db: &Path, started_ms: u64, cwd: &Path) -> Option<String> {
    if !db.is_file() {
        return None;
    }
    let conn = rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    let mut stmt = conn
        .prepare(
            "SELECT id, directory, time_created FROM session \
             WHERE time_created >= ?1 ORDER BY time_created ASC",
        )
        .ok()?;
    let mut rows = stmt.query(rusqlite::params![started_ms as i64]).ok()?;

    let mut candidates: Vec<(i64, bool, String)> = Vec::new();
    while let Ok(Some(row)) = rows.next() {
        let Ok(id) = row.get::<_, String>(0) else {
            continue;
        };
        let directory: Option<String> = row.get(1).ok();
        let time_created: i64 = row.get(2).unwrap_or(started_ms as i64);
        let same_cwd = directory
            .as_deref()
            .is_some_and(|recorded| Path::new(recorded) == cwd);
        candidates.push((time_created, same_cwd, id));
    }
    if candidates.iter().any(|(_, same_cwd, _)| *same_cwd) {
        candidates.retain(|(_, same_cwd, _)| *same_cwd);
    }
    candidates.sort_by_key(|(time_created, _, _)| *time_created);
    candidates.into_iter().next().map(|(_, _, id)| id)
}

/// `{providerID}/{modelID}` from a `Model.Ref`-shaped JSON value
/// (`{"providerID": ..., "modelID": ...}`, verified: `packages/schema/src/
/// session-message.ts`'s `Assistant`/`ModelSwitched` both carry `model:
/// Model.Ref`), or `None` when either field is missing/not a string.
fn model_ref_id(model: Option<&Value>) -> Option<String> {
    let model = model?;
    let provider = model.get("providerID").and_then(Value::as_str)?;
    let id = model.get("modelID").and_then(Value::as_str)?;
    Some(format!("{provider}/{id}"))
}

/// A `Schema.Finite` token count field as `u64`, saturating a negative or
/// non-numeric value to `0` rather than guessing -- these fields serialize as
/// plain JSON numbers, but effect's `Finite` schema does not itself forbid a
/// negative one, so this is a defensive floor, not a fact about the format.
fn token_count(value: Option<&Value>) -> u64 {
    value
        .and_then(Value::as_f64)
        .map(|f| f.max(0.0) as u64)
        .unwrap_or(0)
}

impl AgentAdapter for OpenCodeAdapter {
    fn name(&self) -> &'static str {
        "opencode"
    }

    fn program(&self) -> &str {
        &self.program
    }

    /// OpenCode is not one vendor's account -- see the module doc comment's
    /// "Providers and the model ladder" section. `"opencode"` is the same
    /// kind of static, no-single-account slug `StateDir::usage_for` and
    /// pacing key off, distinct from whichever REAL provider a given launch
    /// is billed to (`provider_for_model`, below).
    fn provider(&self) -> &'static str {
        "opencode"
    }

    /// Resolves the real billed vendor from `model`'s own `provider/model`
    /// prefix via `catalogue::vendor_of` -- issue #382's own multi-provider
    /// precedent -- falling back to the static `"opencode"` slug only when
    /// the model names no vendor this crate's catalogue recognises (`model:
    /// None` included, via `catalogue::vendor_of`'s own `?` on a missing
    /// `/`-prefix).
    fn provider_for_model(&self, model: Option<&str>) -> &'static str {
        model.and_then(catalogue::vendor_of).unwrap_or("opencode")
    }

    /// Mirrors `CodexAdapter::ready`/`ClaudeAdapter::ready` exactly: the one
    /// thing refusable without a live binary is a bare name that *does*
    /// resolve (via `PATH`) to a file this OS cannot execute at all.
    /// `resolve_program` fails open for "resolves to nothing", so `--agent
    /// opencode` on a machine without it fails at spawn time with the OS's
    /// own "not found", not here.
    fn ready(&self) -> CtxResult<()> {
        super::resolve_program(&self.program)?;
        Ok(())
    }

    fn detect(&self, command: &[String]) -> bool {
        command
            .first()
            .and_then(|p| Path::new(p).file_name())
            .map(|f| f.to_string_lossy() == "opencode")
            .unwrap_or(false)
    }

    /// `opencode run [message..]` has no flag to create a session under a
    /// caller-chosen id (`-s/--session` only RESUMES an existing one --
    /// verified, module doc comment), so `session` cannot appear in the
    /// built command, exactly like `CodexAdapter::headless_cmd`'s own
    /// identical situation. `--format json` (verified flag) asks for raw
    /// events on stdout rather than the human-formatted default, which is
    /// what a supervised/piped launch wants.
    fn headless_cmd(&self, prompt: &str, _session: &SessionId, extra: &[String]) -> Command {
        let mut cmd = self.base();
        cmd.arg("run").arg(prompt).arg("--format").arg("json");
        cmd.args(extra);
        self.apply_zirv_agent_env(&mut cmd, extra);
        cmd
    }

    /// Bare `opencode [project]` starts the interactive TUI (verified:
    /// `tui.ts`'s own `command: "$0 [project]"`); `--prompt <text>` is a real
    /// flag there (verified, NOT a positional argument the way `run`'s
    /// message is), so an initial prompt is delivered that way rather than
    /// as a bare trailing argv token, which would instead be parsed as the
    /// `[project]` positional.
    fn interactive_cmd(&self, initial_prompt: Option<&str>, extra: &[String]) -> Command {
        let mut cmd = self.base();
        if let Some(prompt) = initial_prompt {
            cmd.arg("--prompt").arg(prompt);
        }
        cmd.args(extra);
        self.apply_zirv_agent_env(&mut cmd, extra);
        cmd
    }

    /// `run.ts`'s own handler reads the ENTIRE piped stdin as the message
    /// whenever no positional `message` args are given and stdin is not a
    /// TTY (verified: `const piped = ...; message = resolveRunInput(message,
    /// piped) ?? ""`), so a distiller/reviewer child -- which, like codex's
    /// and claude's own `distiller_cmd`, receives its prompt on stdin from
    /// `handoff::run_model`, never as a parameter here -- needs no
    /// positional message token at all.
    fn distiller_cmd(&self, model: &str) -> Command {
        let mut cmd = self.base();
        cmd.arg("run");
        if !model.is_empty() {
            cmd.arg("--model").arg(model);
        }
        let args = self.read_only_args();
        cmd.args(&args);
        self.apply_zirv_agent_env(&mut cmd, &args);
        cmd
    }

    /// Structurally denies `bash`/`edit`/`webfetch` (plus `websearch`/
    /// `external_directory`/`task`) on a dedicated, fixed-name agent -- see
    /// [`OpenCodeAdapter::read_only_permission`]'s own doc comment for the
    /// exact map and why each extra key is included. Emits the `--agent`
    /// pin ONLY when the config file actually materialized: an unresolvable
    /// `--agent` name is a launch that never starts at all on the one
    /// verified surface this pass could check (`run.ts`'s own agent lookup),
    /// which is worse than the unrestricted fallback every other "no
    /// verified mechanism" default on this trait already accepts (mirrors
    /// `ClaudeAdapter::launch_settings_path`'s own documented failure
    /// shape).
    fn read_only_args(&self) -> Vec<String> {
        let Some(state) = self.state_dir() else {
            return Vec::new();
        };
        if Self::materialize_read_only_config(&state).is_err() {
            return Vec::new();
        }
        vec!["--agent".to_string(), READ_ONLY_AGENT.to_string()]
    }

    /// Content-fingerprints `prompt` into a dedicated agent name (see
    /// [`OpenCodeAdapter::system_prompt_agent_name`]) and materializes an
    /// `agent.<name>.prompt` config entry for it -- the one verified
    /// per-launch system-prompt override channel (module doc comment).
    /// Empty when the config cannot be materialized (no state dir resolved,
    /// or the write itself fails) -- the same fail-closed-to-unrestricted
    /// shape [`read_only_args`](Self::read_only_args) uses, for the
    /// identical reason (an unresolvable `--agent` name breaks the launch
    /// outright).
    fn system_prompt_args(&self, prompt: &str) -> Vec<String> {
        if prompt.is_empty() {
            return Vec::new();
        }
        let Some(state) = self.state_dir() else {
            return Vec::new();
        };
        let agent_name = Self::system_prompt_agent_name(prompt);
        let path = Self::system_prompt_config_path(&state, &agent_name);
        let mut entry = serde_json::Map::new();
        entry.insert("prompt".to_string(), Value::String(prompt.to_string()));
        if Self::write_agent_config(&path, &agent_name, entry).is_err() {
            return Vec::new();
        }
        vec!["--agent".to_string(), agent_name]
    }

    /// This adapter's own vendor-aware ladder step (module doc comment's
    /// "Providers and the model ladder" section): answers from the seat
    /// model's own vendor when the catalogue recognises it, else the same
    /// `""` the trait's own unoverridden default would give.
    fn review_model_below(&self, seat: Option<&str>) -> &'static str {
        seat.and_then(catalogue::vendor_of)
            .and_then(catalogue::vendor)
            .map(|vendor| catalogue::rung_below(vendor, seat))
            .unwrap_or_default()
    }

    fn model_strength(&self, model: &str) -> Option<u8> {
        catalogue::vendor_of(model)
            .and_then(catalogue::vendor)
            .and_then(|vendor| catalogue::strength(vendor, model))
    }

    fn context_window_tokens(&self, model: Option<&str>) -> Option<u64> {
        let vendor = model
            .and_then(catalogue::vendor_of)
            .and_then(catalogue::vendor)?;
        catalogue::context_window(vendor, model)
    }

    /// Issue #382: resolves this session's own OpenCode session id (pinning
    /// it, per [`OpenCodeAdapter::pinned_session_id`]'s own doc comment), then
    /// syncs `session_message` rows past the last-seen `rowid` into a shadow
    /// JSONL via [`ShadowTranscript::sync_sqlite`]. The id is validated by
    /// [`is_valid_opencode_id`] before being interpolated into the query
    /// string (the sync contract's one `?1` slot is reserved for the rowid
    /// cursor, per that method's own doc comment). Falls back to the native
    /// DB path when no state dir resolves or no session id can be pinned --
    /// the same degraded shape `CodexAdapter::transcript_path` falls back to
    /// when nothing resolves.
    fn transcript_path(&self, session: &SessionRef) -> PathBuf {
        let db = self.db_path();
        let Some(state) = self.state_dir() else {
            return db;
        };
        let Some(id) = self.pinned_session_id(&state, session) else {
            return db;
        };
        if !is_valid_opencode_id(&id) {
            return db;
        }
        let query = format!(
            "SELECT rowid, type, seq, data, time_created FROM session_message \
             WHERE session_id = '{id}' AND rowid > ?1 ORDER BY rowid"
        );
        let shadow = ShadowTranscript::for_session(&state, session);
        match shadow.sync_sqlite(&db, &query) {
            Ok(path) => path,
            Err(_) => shadow.path().to_path_buf(),
        }
    }

    /// Maps the three `session_message.type` shapes this pass could verify a
    /// concrete structure for (`packages/schema/src/session-message.ts`):
    ///
    /// - `"user"` -> `TurnStart`, plus `UserText` when `data.text` is
    ///   non-empty.
    /// - `"assistant"` -> one `ModelId` (from `data.model`, when present),
    ///   one `AssistantThinking` per non-empty `reasoning` content item, one
    ///   `ToolCall`/`ToolResult` pair per `content` item of `type: "tool"`
    ///   whose own `state.status` is a TERMINAL state (`"completed"` or
    ///   `"error"`; `"pending"`/`"running"` emit only the `ToolCall`, no
    ///   verdict, since this row may never be re-synced after the status
    ///   advances -- see the module doc comment's `$onUpdate` residual), one
    ///   `AssistantFirstText` before the first non-empty `text` content item,
    ///   and exactly one `AssistantFinal` per row concatenating every `text`
    ///   item's own text (mirroring claude's own "text holds the
    ///   concatenated text blocks" convention) with `input_tokens` from
    ///   `data.tokens.input`.
    /// - `"compaction"` -> `Compaction`.
    /// - `"model-switched"` -> `ModelId`.
    ///
    /// `"system"`/`"shell"`/`"synthetic"`/`"agent-switched"` rows are not
    /// mapped: `shell` carries no verified success/failure signal (no
    /// exit-code-shaped field on its own schema), and the others carry
    /// nothing this vocabulary has a slot for.
    fn parse_events(&self, jsonl: &str) -> Vec<NormalizedEvent> {
        let mut events = Vec::new();
        for line in jsonl.lines() {
            let Ok(row) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            let Some(msg_type) = row.get("type").and_then(Value::as_str) else {
                continue;
            };
            let at_ms = row
                .get("time_created")
                .and_then(Value::as_i64)
                .map(|v| v.max(0) as u64);

            match msg_type {
                "compaction" => {
                    events.push(NormalizedEvent::Compaction);
                    continue;
                }
                "user" | "assistant" | "model-switched" => {}
                _ => continue,
            }

            let Some(data_text) = row.get("data").and_then(Value::as_str) else {
                continue;
            };
            let Ok(data) = serde_json::from_str::<Value>(data_text) else {
                continue;
            };

            match msg_type {
                "user" => {
                    events.push(NormalizedEvent::TurnStart { at_ms });
                    if let Some(text) = data.get("text").and_then(Value::as_str)
                        && !text.is_empty()
                    {
                        events.push(NormalizedEvent::UserText {
                            byte_len: text.len() as u64,
                        });
                    }
                }
                "model-switched" => {
                    if let Some(id) = model_ref_id(data.get("model")) {
                        events.push(NormalizedEvent::ModelId { id });
                    }
                }
                "assistant" => {
                    if let Some(id) = model_ref_id(data.get("model")) {
                        events.push(NormalizedEvent::ModelId { id });
                    }
                    let input_tokens = token_count(data.get("tokens").and_then(|t| t.get("input")));
                    let mut text = String::new();
                    let mut first_text_emitted = false;
                    if let Some(content) = data.get("content").and_then(Value::as_array) {
                        for item in content {
                            match item.get("type").and_then(Value::as_str) {
                                Some("text") => {
                                    let Some(t) = item.get("text").and_then(Value::as_str) else {
                                        continue;
                                    };
                                    if t.is_empty() {
                                        continue;
                                    }
                                    if !first_text_emitted {
                                        events.push(NormalizedEvent::AssistantFirstText { at_ms });
                                        first_text_emitted = true;
                                    }
                                    if !text.is_empty() {
                                        text.push('\n');
                                    }
                                    text.push_str(t);
                                }
                                Some("reasoning") => {
                                    if let Some(t) = item.get("text").and_then(Value::as_str)
                                        && !t.is_empty()
                                    {
                                        events.push(NormalizedEvent::AssistantThinking {
                                            byte_len: t.len() as u64,
                                        });
                                    }
                                }
                                Some("tool") => {
                                    let name = item
                                        .get("name")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default()
                                        .to_string();
                                    let state_obj = item.get("state");
                                    let status = state_obj
                                        .and_then(|s| s.get("status"))
                                        .and_then(Value::as_str);
                                    let input_hash = state_obj
                                        .and_then(|s| s.get("input"))
                                        .map(|v| event::input_hash(&v.to_string()))
                                        .unwrap_or(0);
                                    events.push(NormalizedEvent::ToolCall {
                                        name,
                                        input_hash,
                                        at_ms,
                                    });
                                    match status {
                                        Some("completed") => {
                                            events.push(NormalizedEvent::ToolResult {
                                                is_error: false,
                                            });
                                        }
                                        Some("error") => {
                                            events.push(NormalizedEvent::ToolResult {
                                                is_error: true,
                                            });
                                            if let Some(message) = state_obj
                                                .and_then(|s| s.get("error"))
                                                .and_then(|e| e.get("message"))
                                                .and_then(Value::as_str)
                                            {
                                                events.push(NormalizedEvent::ToolErrorText {
                                                    hash: event::error_text_hash(message),
                                                });
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    events.push(NormalizedEvent::AssistantFinal {
                        text,
                        input_tokens,
                        at_ms,
                    });
                }
                _ => {}
            }
        }
        events
    }

    /// The newest `"assistant"`/`"model-switched"` row's own model, walking
    /// from the end since a shadow's lines are in ascending `rowid` (hence
    /// chronological) order.
    fn model_hint(&self, jsonl: &str) -> Option<String> {
        jsonl.lines().rev().find_map(|line| {
            let row = serde_json::from_str::<Value>(line.trim()).ok()?;
            let msg_type = row.get("type").and_then(Value::as_str)?;
            if msg_type != "assistant" && msg_type != "model-switched" {
                return None;
            }
            let data_text = row.get("data").and_then(Value::as_str)?;
            let data = serde_json::from_str::<Value>(data_text).ok()?;
            model_ref_id(data.get("model"))
        })
    }

    /// Only `assistant_texts`/`user_messages` are populated, from the same
    /// verified `"text"`-content and `"user".data.text` shapes
    /// [`parse_events`](Self::parse_events) draws from -- `files_read`/
    /// `files_modified`/`tool_errors`/`last_verification` stay at their
    /// `StructuralContext::default()` values: no verified tool-name/
    /// input-key schema exists for the built-in tools' file arguments, and
    /// the one message type with a raw command (`"shell"`) carries no
    /// verified success/failure signal to build a `ToolInvocation` from
    /// (`last_verification_run`'s own contract needs one). `last_n`
    /// truncation mirrors `codex::structural_context`'s own `keep_last`.
    fn structural_context(&self, jsonl: &str, last_n: usize) -> StructuralContext {
        let mut user_messages: Vec<String> = Vec::new();
        let mut assistant_texts: Vec<String> = Vec::new();
        for line in jsonl.lines() {
            let Ok(row) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            let Some(msg_type) = row.get("type").and_then(Value::as_str) else {
                continue;
            };
            let Some(data_text) = row.get("data").and_then(Value::as_str) else {
                continue;
            };
            let Ok(data) = serde_json::from_str::<Value>(data_text) else {
                continue;
            };
            match msg_type {
                "user" => {
                    if let Some(t) = data.get("text").and_then(Value::as_str)
                        && !t.trim().is_empty()
                    {
                        user_messages.push(t.to_string());
                    }
                }
                "assistant" => {
                    let mut text = String::new();
                    if let Some(content) = data.get("content").and_then(Value::as_array) {
                        for item in content {
                            if item.get("type").and_then(Value::as_str) != Some("text") {
                                continue;
                            }
                            if let Some(t) = item.get("text").and_then(Value::as_str)
                                && !t.is_empty()
                            {
                                if !text.is_empty() {
                                    text.push('\n');
                                }
                                text.push_str(t);
                            }
                        }
                    }
                    if !text.trim().is_empty() {
                        assistant_texts.push(text);
                    }
                }
                _ => {}
            }
        }
        if user_messages.len() > last_n {
            user_messages.drain(..user_messages.len() - last_n);
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

    /// Sums `data.tokens.{input,output,cache.{read,write}}` across every
    /// `"assistant"` row in `jsonl` -- there is no cumulative-total field on
    /// this schema (unlike codex's `total_token_usage`), so, like claude's
    /// own `transcript_usage`, this adapter folds the per-message deltas
    /// itself. `None` only when the fragment carries no assistant row with a
    /// `tokens` object at all -- an honest "no data", never a zeroed
    /// reading.
    fn transcript_usage(&self, jsonl: &str) -> Option<TranscriptUsage> {
        let mut usage = TranscriptUsage::default();
        let mut observed = false;
        for line in jsonl.lines() {
            let Ok(row) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            if row.get("type").and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            let Some(data_text) = row.get("data").and_then(Value::as_str) else {
                continue;
            };
            let Ok(data) = serde_json::from_str::<Value>(data_text) else {
                continue;
            };
            let Some(tokens) = data.get("tokens") else {
                continue;
            };
            observed = true;
            usage.input_tokens = usage
                .input_tokens
                .saturating_add(token_count(tokens.get("input")));
            usage.output_tokens = usage
                .output_tokens
                .saturating_add(token_count(tokens.get("output")));
            let cache = tokens.get("cache");
            usage.cache_read_input_tokens = usage
                .cache_read_input_tokens
                .saturating_add(token_count(cache.and_then(|c| c.get("read"))));
            usage.cache_creation_input_tokens = usage
                .cache_creation_input_tokens
                .saturating_add(token_count(cache.and_then(|c| c.get("write"))));
        }
        observed.then_some(usage)
    }

    /// No slash command or safely-injectable text triggers compaction --
    /// see the module doc comment's "Compaction and quitting" section.
    /// Mirrors `CodexAdapter::compact_command`'s own `None` and its existing
    /// fallback+verification path in `wrap.rs`.
    fn compact_command(&self) -> Option<&'static str> {
        None
    }

    /// Ctrl-D (`\x04`), never Ctrl-C -- see the module doc comment's
    /// "Compaction and quitting" section and `wrap::quit_child`'s own F1
    /// doc comment for why `\x03` specifically is permanently banned from
    /// every adapter's answer here.
    fn quit_sequence(&self) -> &'static str {
        "\x04"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            marker_signal: false,
            token_usage: false,
            turn_signal: false,
            system_prompt: true,
            events: true,
            // UNVERIFIED: no real OpenCode TUI was available to confirm
            // whether its composer folds a same-burst trailing `\r` into
            // pasted text (issue #118's own codex-specific finding) or
            // submits it correctly. `false` is the trait's own base
            // assumption -- every adapter this crate ships defaults to it
            // and only `CodexAdapter` overrides it, with a live-verified
            // reason. Revisit once a real install can be probed.
            defer_injection_submit: false,
            context_window_tokens: self.context_window_tokens(None),
        }
    }

    /// Verified: `-m, --model <provider/model>` on both `run` and the
    /// interactive TUI (`run.ts`/`tui.ts`, both cited in the module doc
    /// comment).
    fn model_args(&self, model: &str) -> Vec<String> {
        vec!["--model".to_string(), model.to_string()]
    }

    // No `resume_args`/`session_pin_args` override: unlike claude (which
    // pins its own conversation id via `session_pin_args` so a later
    // `resume_args` can find it by that same literal id), OpenCode has no
    // way to CREATE a session under a caller-chosen id at all -- `-s/
    // --session` only resumes one that already exists (verified, module doc
    // comment's "CLI surface" section: `run.ts`'s own `session()` helper
    // exits `"Session not found"` otherwise). Pinning zirv's own uuid as a
    // session-pin argument would therefore always fail at spawn time, so
    // both stay on the trait's own "no verified mechanism" defaults
    // (`None`/empty), exactly mirroring `CodexAdapter`'s identical
    // reasoning and its own absence of either override.

    fn register_turn_signal(&self, _session: &SessionRef, _socket: &Path) -> TurnSignalSetup {
        TurnSignalSetup {
            env: Vec::new(),
            instructions: String::new(),
        }
    }

    /// Names the repo-config-override residual the module doc comment's
    /// "System prompt and permissions" section describes: OpenCode's own
    /// config merge order loads a repository's `opencode.json`/`.opencode/
    /// agent/<name>.md` AFTER the `OPENCODE_CONFIG` file this adapter sets,
    /// so a hostile repo that predicts (or simply reads, since this crate is
    /// open source) zirv's fixed read-only agent name could redefine its
    /// `permission` map and widen it. Unconditional -- unlike codex's own
    /// version-gated `sandbox_residual_note`, this is structural to how
    /// OpenCode resolves config, not fixable by a newer install.
    fn sandbox_residual_note(&self) -> Option<String> {
        Some(
            "OpenCode's own config-merge order (global config -> OPENCODE_CONFIG -> project \
             config -> .opencode directories, verified in packages/core/src/config/config.ts) \
             loads a repository's own opencode.json/.opencode/agent/zirv-read-only.md AFTER the \
             zirv-owned config this adapter sets, so a repository that redefines that fixed \
             agent name could widen the permission map this pin relies on. This is a structural \
             property of OpenCode's config system, not a version gap a newer install closes."
                .to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn adapter() -> OpenCodeAdapter {
        OpenCodeAdapter::new(None)
    }

    fn flatten(command: Command) -> Vec<String> {
        super::super::flatten_command(command)
    }

    // -- command shapes ----------------------------------------------------

    #[test]
    fn headless_cmd_carries_the_prompt_and_json_format() {
        let argv = flatten(adapter().headless_cmd("hello there", &SessionId::new_v4(), &[]));
        assert_eq!(argv[0], "opencode");
        assert!(argv.contains(&"run".to_string()));
        assert!(argv.contains(&"hello there".to_string()));
        let format_at = argv
            .iter()
            .position(|a| a == "--format")
            .expect("--format present");
        assert_eq!(argv[format_at + 1], "json");
    }

    #[test]
    fn interactive_cmd_delivers_the_initial_prompt_via_the_prompt_flag() {
        let argv = flatten(adapter().interactive_cmd(Some("resume please"), &[]));
        let at = argv
            .iter()
            .position(|a| a == "--prompt")
            .expect("--prompt present");
        assert_eq!(argv[at + 1], "resume please");
    }

    #[test]
    fn interactive_cmd_with_no_prompt_carries_no_prompt_flag() {
        let argv = flatten(adapter().interactive_cmd(None, &[]));
        assert!(!argv.contains(&"--prompt".to_string()));
    }

    #[test]
    fn with_home_overrides_the_computed_home_dir() {
        let a = adapter().with_home(PathBuf::from("/custom/home"));
        assert_eq!(a.home_dir(), PathBuf::from("/custom/home"));
    }

    #[test]
    fn model_args_uses_the_verified_flag() {
        assert_eq!(
            adapter().model_args("anthropic/claude-sonnet-5"),
            vec![
                "--model".to_string(),
                "anthropic/claude-sonnet-5".to_string()
            ]
        );
    }

    #[test]
    fn resume_args_is_unverified_because_session_only_resumes_an_existing_id() {
        assert_eq!(
            adapter().resume_args("11111111-2222-4333-8444-555555555555"),
            None
        );
        assert_eq!(
            adapter().session_pin_args("11111111-2222-4333-8444-555555555555"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn distiller_cmd_carries_the_read_only_pin_and_model_but_no_positional_message() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = adapter().with_state_root(dir.path().to_path_buf());
        let argv = flatten(a.distiller_cmd("gpt-5.6-luna"));
        assert!(argv.contains(&"run".to_string()));
        let model_at = argv
            .iter()
            .position(|x| x == "--model")
            .expect("model flag");
        assert_eq!(argv[model_at + 1], "gpt-5.6-luna");
        assert!(argv.contains(&"--agent".to_string()));
        assert!(argv.contains(&READ_ONLY_AGENT.to_string()));
    }

    #[test]
    fn distiller_cmd_omits_the_model_flag_when_none_is_given() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = adapter().with_state_root(dir.path().to_path_buf());
        let argv = flatten(a.distiller_cmd(""));
        assert!(!argv.contains(&"--model".to_string()));
    }

    // -- read-only config ----------------------------------------------------

    #[test]
    fn read_only_args_materializes_a_config_that_denies_bash_edit_and_webfetch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = adapter().with_state_root(dir.path().to_path_buf());
        let args = a.read_only_args();
        assert_eq!(
            args,
            vec!["--agent".to_string(), READ_ONLY_AGENT.to_string()]
        );

        let state = StateDir::from_root(dir.path().to_path_buf());
        let config_path = OpenCodeAdapter::read_only_config_path(&state);
        let body: Value =
            serde_json::from_str(&std::fs::read_to_string(&config_path).expect("config written"))
                .expect("valid json");
        let permission = &body["agent"][READ_ONLY_AGENT]["permission"];
        assert_eq!(permission["bash"], "deny");
        assert_eq!(permission["edit"], "deny");
        assert_eq!(permission["webfetch"], "deny");
        assert_eq!(permission["task"], "deny");
    }

    #[test]
    fn read_only_args_is_empty_when_no_state_dir_resolves() {
        // The default (non-test) `state_dir()` would resolve the real
        // platform directory; the `#[cfg(test)]` seam with no root set
        // reproduces "nothing to pin against" without touching a real
        // machine's state.
        let a = adapter();
        assert_eq!(a.state_dir(), None);
        assert_eq!(a.read_only_args(), Vec::<String>::new());
    }

    #[test]
    fn headless_cmd_sets_opencode_config_only_for_its_own_read_only_agent_token() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = adapter().with_state_root(dir.path().to_path_buf());
        let ro_args = a.read_only_args();
        let cmd = a.headless_cmd("prompt", &SessionId::new_v4(), &ro_args);
        let envs: Vec<_> = cmd.get_envs().collect();
        assert!(
            envs.iter()
                .any(|(k, v)| k.to_str() == Some("OPENCODE_CONFIG") && v.is_some()),
            "{envs:?}"
        );

        // An operator's own unrelated --agent choice must never be touched.
        let operator_args = vec!["--agent".to_string(), "my-own-agent".to_string()];
        let cmd2 = a.headless_cmd("prompt", &SessionId::new_v4(), &operator_args);
        let envs2: Vec<_> = cmd2.get_envs().collect();
        assert!(
            !envs2
                .iter()
                .any(|(k, _)| k.to_str() == Some("OPENCODE_CONFIG")),
            "{envs2:?}"
        );
    }

    // -- system prompt ----------------------------------------------------

    #[test]
    fn system_prompt_args_materializes_a_per_prompt_agent_and_env() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = adapter().with_state_root(dir.path().to_path_buf());
        let args = a.system_prompt_args("be terse");
        assert_eq!(args.len(), 2);
        assert_eq!(args[0], "--agent");
        assert!(args[1].starts_with(SYSTEM_PROMPT_AGENT_PREFIX));

        let cmd = a.headless_cmd("hi", &SessionId::new_v4(), &args);
        let envs: Vec<_> = cmd.get_envs().collect();
        let config_env = envs
            .iter()
            .find(|(k, _)| k.to_str() == Some("OPENCODE_CONFIG"))
            .expect("env set");
        let path = PathBuf::from(config_env.1.expect("value"));
        let body: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("config written"))
                .expect("valid json");
        assert_eq!(body["agent"][args[1].as_str()]["prompt"], "be terse");
    }

    #[test]
    fn system_prompt_args_is_empty_for_an_empty_prompt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = adapter().with_state_root(dir.path().to_path_buf());
        assert_eq!(a.system_prompt_args(""), Vec::<String>::new());
    }

    #[test]
    fn two_different_prompts_materialize_two_different_agent_names() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = adapter().with_state_root(dir.path().to_path_buf());
        let one = a.system_prompt_args("prompt one");
        let two = a.system_prompt_args("prompt two");
        assert_ne!(one[1], two[1]);
    }

    // -- transcript_path / session pinning ----------------------------------

    fn make_test_db(path: &Path) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open(path).expect("open db");
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE session (
                 id TEXT PRIMARY KEY,
                 directory TEXT,
                 time_created INTEGER
             );
             CREATE TABLE session_message (
                 id TEXT PRIMARY KEY,
                 session_id TEXT,
                 type TEXT,
                 seq INTEGER,
                 data TEXT,
                 time_created INTEGER
             );",
        )
        .expect("create tables");
        conn
    }

    fn insert_session(conn: &rusqlite::Connection, id: &str, directory: &str, time_created: i64) {
        conn.execute(
            "INSERT INTO session (id, directory, time_created) VALUES (?1, ?2, ?3)",
            rusqlite::params![id, directory, time_created],
        )
        .expect("insert session");
    }

    fn insert_message(
        conn: &rusqlite::Connection,
        session_id: &str,
        seq: i64,
        msg_type: &str,
        data: &Value,
    ) {
        conn.execute(
            "INSERT INTO session_message (id, session_id, type, seq, data, time_created) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                format!("msg_{seq}"),
                session_id,
                msg_type,
                seq,
                data.to_string(),
                1_700_000_000_000i64 + seq,
            ],
        )
        .expect("insert message");
    }

    fn register_session(state_root: &Path, session: &SessionRef, started_at: u64) {
        let state = StateDir::from_root(state_root.to_path_buf());
        std::fs::create_dir_all(state.sessions()).expect("mkdir sessions");
        let short = sessions::short_id(session.id.as_str());
        let mut record = sessions::Record::new(
            session.id.as_str(),
            "opencode",
            &session.cwd,
            sessions::Verb::Wrap,
        );
        record.started_at = started_at;
        std::fs::write(
            state.sessions().join(format!("{short}.json")),
            serde_json::to_string(&record).expect("record json"),
        )
        .expect("write record");
    }

    #[test]
    fn transcript_path_pins_the_session_created_after_this_zirv_session_started() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path().join("repo");
        std::fs::create_dir_all(&cwd).expect("mkdir");
        let db_path = dir.path().join("data").join("opencode.db");
        std::fs::create_dir_all(db_path.parent().unwrap()).expect("mkdir data");
        let conn = make_test_db(&db_path);
        let started_ms = 1_700_000_000_000u64;
        insert_session(
            &conn,
            "ses_before",
            cwd.to_str().unwrap(),
            (started_ms - 10_000) as i64,
        );
        insert_session(
            &conn,
            "ses_after",
            cwd.to_str().unwrap(),
            (started_ms + 10_000) as i64,
        );
        insert_message(
            &conn,
            "ses_after",
            1,
            "user",
            &serde_json::json!({"text": "hi", "files": [], "agents": []}),
        );

        let session = SessionRef {
            id: SessionId::parse("11111111-2222-4333-8444-555555555555"),
            cwd: cwd.clone(),
        };
        let state_root = dir.path().join("state");
        register_session(&state_root, &session, started_ms / 1_000);

        let a = OpenCodeAdapter::new(None)
            .with_state_root(state_root.clone())
            .with_db_path(db_path.clone());

        let path = a.transcript_path(&session);
        let lines: Vec<String> = std::fs::read_to_string(&path)
            .expect("shadow written")
            .lines()
            .map(String::from)
            .collect();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("\"type\":\"user\""), "{lines:?}");

        // A second call appends nothing new (the message was already synced).
        let path2 = a.transcript_path(&session);
        assert_eq!(path, path2);
        let lines2: Vec<String> = std::fs::read_to_string(&path2)
            .expect("shadow read")
            .lines()
            .map(String::from)
            .collect();
        assert_eq!(lines2.len(), 1, "no duplicate rows: {lines2:?}");

        // A new message on the SAME session is picked up on the next call.
        insert_message(
            &conn,
            "ses_after",
            2,
            "assistant",
            &serde_json::json!({
                "agent": "build",
                "model": {"providerID": "anthropic", "modelID": "claude-sonnet-5"},
                "content": [{"type": "text", "id": "t1", "text": "hello"}],
                "tokens": {"input": 10, "output": 5, "reasoning": 0, "cache": {"read": 0, "write": 0}},
                "time": {"created": started_ms + 20_000},
            }),
        );
        let path3 = a.transcript_path(&session);
        let lines3: Vec<String> = std::fs::read_to_string(&path3)
            .expect("shadow read")
            .lines()
            .map(String::from)
            .collect();
        assert_eq!(lines3.len(), 2, "{lines3:?}");

        // Also proves a session STARTED BEFORE this zirv session never wins.
        assert!(!lines3.iter().any(|l| l.contains("ses_before")));
    }

    #[test]
    fn transcript_path_falls_back_to_the_native_db_path_with_no_state_dir() {
        let a = OpenCodeAdapter::new(None).with_db_path(PathBuf::from("/nowhere/opencode.db"));
        let session = SessionRef {
            id: SessionId::parse("11111111-2222-4333-8444-555555555555"),
            cwd: PathBuf::from("/work/repo"),
        };
        assert_eq!(
            a.transcript_path(&session),
            PathBuf::from("/nowhere/opencode.db")
        );
    }

    // -- parse_events / structural_context / usage --------------------------

    fn shadow_row(msg_type: &str, seq: i64, data: &Value, time_created: i64) -> String {
        serde_json::json!({
            "rowid": seq,
            "type": msg_type,
            "seq": seq,
            "data": data.to_string(),
            "time_created": time_created,
        })
        .to_string()
    }

    #[test]
    fn parse_events_maps_user_and_assistant_rows() {
        let jsonl = format!(
            "{}\n{}\n",
            shadow_row(
                "user",
                1,
                &serde_json::json!({"text": "hello", "files": [], "agents": []}),
                1_000,
            ),
            shadow_row(
                "assistant",
                2,
                &serde_json::json!({
                    "agent": "build",
                    "model": {"providerID": "anthropic", "modelID": "claude-sonnet-5"},
                    "content": [
                        {"type": "text", "id": "t1", "text": "hi there"},
                        {"type": "tool", "id": "c1", "name": "bash", "state": {"status": "completed", "input": {"command": "ls"}, "content": [], "structured": {}}},
                    ],
                    "tokens": {"input": 42, "output": 7, "reasoning": 0, "cache": {"read": 0, "write": 0}},
                }),
                2_000,
            ),
        );

        let adapter = adapter();
        let events = adapter.parse_events(&jsonl);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, NormalizedEvent::TurnStart { .. })),
            "{events:?}"
        );
        assert!(
            events.contains(&NormalizedEvent::UserText { byte_len: 5 }),
            "{events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, NormalizedEvent::ModelId { id } if id == "anthropic/claude-sonnet-5")),
            "{events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, NormalizedEvent::ToolCall { name, .. } if name == "bash")),
            "{events:?}"
        );
        assert!(
            events.contains(&NormalizedEvent::ToolResult { is_error: false }),
            "{events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                NormalizedEvent::AssistantFinal { text, input_tokens: 42, .. } if text == "hi there"
            )),
            "{events:?}"
        );
    }

    #[test]
    fn parse_events_marks_a_terminal_error_tool_state() {
        let jsonl = shadow_row(
            "assistant",
            1,
            &serde_json::json!({
                "agent": "build",
                "model": {"providerID": "openai", "modelID": "gpt-5.6-terra"},
                "content": [
                    {"type": "tool", "id": "c1", "name": "edit", "state": {
                        "status": "error", "input": {}, "content": [], "structured": {},
                        "error": {"type": "unknown", "message": "permission denied"}
                    }},
                ],
                "tokens": {"input": 1, "output": 1, "reasoning": 0, "cache": {"read": 0, "write": 0}},
            }),
            1_000,
        ) + "\n";
        let events = adapter().parse_events(&jsonl);
        assert!(events.contains(&NormalizedEvent::ToolResult { is_error: true }));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, NormalizedEvent::ToolErrorText { .. }))
        );
    }

    #[test]
    fn parse_events_never_emits_a_verdict_for_a_non_terminal_tool_state() {
        let jsonl = shadow_row(
            "assistant",
            1,
            &serde_json::json!({
                "agent": "build",
                "model": {"providerID": "openai", "modelID": "gpt-5.6-terra"},
                "content": [
                    {"type": "tool", "id": "c1", "name": "bash", "state": {"status": "running", "input": {}, "structured": {}, "content": []}},
                ],
                "tokens": {"input": 1, "output": 1, "reasoning": 0, "cache": {"read": 0, "write": 0}},
            }),
            1_000,
        ) + "\n";
        let events = adapter().parse_events(&jsonl);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, NormalizedEvent::ToolCall { .. }))
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, NormalizedEvent::ToolResult { .. })),
            "a non-terminal status must never report a verdict: {events:?}"
        );
    }

    #[test]
    fn parse_events_over_the_shadow_rows_fixture() {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/opencode/shadow-rows.jsonl");
        let jsonl = std::fs::read_to_string(fixture).expect("read fixture");
        let events = adapter().parse_events(&jsonl);

        assert!(
            events
                .iter()
                .any(|e| matches!(e, NormalizedEvent::TurnStart { .. })),
            "{events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, NormalizedEvent::ToolResult { is_error: false })),
            "{events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, NormalizedEvent::ToolResult { is_error: true })),
            "{events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, NormalizedEvent::ToolErrorText { .. })),
            "{events:?}"
        );
        assert!(events.contains(&NormalizedEvent::Compaction), "{events:?}");
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, NormalizedEvent::AssistantFinal { .. }))
                .count(),
            2,
            "one AssistantFinal per assistant row: {events:?}"
        );
    }

    #[test]
    fn parse_events_maps_compaction_rows() {
        let jsonl = shadow_row(
            "compaction",
            1,
            &serde_json::json!({"reason": "auto", "summary": "s", "recent": "r"}),
            1_000,
        ) + "\n";
        assert_eq!(
            adapter().parse_events(&jsonl),
            vec![NormalizedEvent::Compaction]
        );
    }

    #[test]
    fn structural_context_carries_user_and_assistant_text_only() {
        let jsonl = format!(
            "{}\n{}\n",
            shadow_row(
                "user",
                1,
                &serde_json::json!({"text": "question", "files": [], "agents": []}),
                1_000,
            ),
            shadow_row(
                "assistant",
                2,
                &serde_json::json!({
                    "agent": "build",
                    "model": {"providerID": "anthropic", "modelID": "claude-sonnet-5"},
                    "content": [{"type": "text", "id": "t1", "text": "answer"}],
                    "tokens": {"input": 1, "output": 1, "reasoning": 0, "cache": {"read": 0, "write": 0}},
                }),
                2_000,
            ),
        );
        let ctx = adapter().structural_context(&jsonl, 10);
        assert_eq!(ctx.user_messages, vec!["question".to_string()]);
        assert_eq!(ctx.assistant_texts, vec!["answer".to_string()]);
        assert!(ctx.files_read.is_empty());
        assert!(ctx.files_modified.is_empty());
        assert!(ctx.last_verification.is_none());
    }

    #[test]
    fn transcript_usage_sums_across_assistant_rows() {
        let jsonl = format!(
            "{}\n{}\n",
            shadow_row(
                "assistant",
                1,
                &serde_json::json!({
                    "agent": "build",
                    "model": {"providerID": "anthropic", "modelID": "claude-sonnet-5"},
                    "content": [],
                    "tokens": {"input": 10, "output": 5, "reasoning": 0, "cache": {"read": 2, "write": 3}},
                }),
                1_000,
            ),
            shadow_row(
                "assistant",
                2,
                &serde_json::json!({
                    "agent": "build",
                    "model": {"providerID": "anthropic", "modelID": "claude-sonnet-5"},
                    "content": [],
                    "tokens": {"input": 20, "output": 15, "reasoning": 0, "cache": {"read": 8, "write": 0}},
                }),
                2_000,
            ),
        );
        assert_eq!(
            adapter().transcript_usage(&jsonl),
            Some(TranscriptUsage {
                input_tokens: 30,
                cache_creation_input_tokens: 3,
                cache_read_input_tokens: 10,
                output_tokens: 20,
            })
        );
        assert!(!adapter().transcript_usage_is_cumulative());
    }

    #[test]
    fn transcript_usage_is_none_with_no_assistant_rows() {
        let jsonl = shadow_row(
            "user",
            1,
            &serde_json::json!({"text": "hi", "files": [], "agents": []}),
            1_000,
        ) + "\n";
        assert_eq!(adapter().transcript_usage(&jsonl), None);
    }

    #[test]
    fn model_hint_reads_the_newest_assistant_model() {
        let jsonl = format!(
            "{}\n{}\n",
            shadow_row(
                "assistant",
                1,
                &serde_json::json!({
                    "agent": "build",
                    "model": {"providerID": "openai", "modelID": "gpt-5.6-terra"},
                    "content": [],
                    "tokens": {"input": 1, "output": 1, "reasoning": 0, "cache": {"read": 0, "write": 0}},
                }),
                1_000,
            ),
            shadow_row(
                "assistant",
                2,
                &serde_json::json!({
                    "agent": "build",
                    "model": {"providerID": "anthropic", "modelID": "claude-sonnet-5"},
                    "content": [],
                    "tokens": {"input": 1, "output": 1, "reasoning": 0, "cache": {"read": 0, "write": 0}},
                }),
                2_000,
            ),
        );
        assert_eq!(
            adapter().model_hint(&jsonl),
            Some("anthropic/claude-sonnet-5".to_string())
        );
    }

    // -- provider / catalogue ladder -----------------------------------------

    #[test]
    fn provider_is_the_static_opencode_slug() {
        assert_eq!(adapter().provider(), "opencode");
    }

    #[test]
    fn provider_for_model_resolves_the_real_vendor_when_known() {
        assert_eq!(
            adapter().provider_for_model(Some("anthropic/claude-sonnet-5")),
            "anthropic"
        );
        assert_eq!(
            adapter().provider_for_model(Some("unknown-vendor/x")),
            "opencode"
        );
        assert_eq!(adapter().provider_for_model(None), "opencode");
    }

    #[test]
    fn ladder_methods_match_the_catalogue_for_a_known_google_model() {
        let vendor = catalogue::vendor("google").expect("google is a built-in vendor");
        let model = "google/gemini-3.1-pro-preview";
        assert_eq!(
            adapter().review_model_below(Some(model)),
            catalogue::rung_below(vendor, Some(model))
        );
        assert_eq!(
            adapter().model_strength(model),
            catalogue::strength(vendor, model)
        );
        assert_eq!(
            adapter().context_window_tokens(Some(model)),
            catalogue::context_window(vendor, Some(model))
        );
    }

    #[test]
    fn ladder_methods_fall_back_to_trait_defaults_for_an_unknown_vendor() {
        assert_eq!(
            adapter().review_model_below(Some("totally-unknown-thing")),
            ""
        );
        assert_eq!(adapter().model_strength("totally-unknown-thing"), None);
        assert_eq!(
            adapter().context_window_tokens(Some("totally-unknown-thing")),
            None
        );
        assert_eq!(adapter().context_window_tokens(None), None);
    }

    // -- misc trait surface --------------------------------------------------

    #[test]
    fn detect_matches_the_opencode_binary_name_only() {
        assert!(adapter().detect(&["opencode".to_string(), "run".to_string()]));
        assert!(!adapter().detect(&["codex".to_string()]));
        assert!(!adapter().detect(&[]));
    }

    #[test]
    fn quit_sequence_is_ctrl_d_never_ctrl_c() {
        assert_eq!(adapter().quit_sequence(), "\x04");
        assert_ne!(adapter().quit_sequence(), "\x03");
    }

    #[test]
    fn compact_command_is_unsupported() {
        assert_eq!(adapter().compact_command(), None);
    }

    #[test]
    fn capabilities_report_events_and_system_prompt_but_no_marker_or_turn_signal() {
        let caps = adapter().capabilities();
        assert!(caps.events);
        assert!(caps.system_prompt);
        assert!(!caps.marker_signal);
        assert!(!caps.turn_signal);
        assert!(!caps.token_usage);
    }

    #[test]
    fn all_contains_opencode() {
        assert!(
            super::super::all(None)
                .iter()
                .any(|a| a.name() == "opencode")
        );
    }

    #[test]
    fn sandbox_residual_note_is_always_disclosed() {
        assert!(adapter().sandbox_residual_note().is_some());
    }
}
