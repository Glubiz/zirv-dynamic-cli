---
last-verified: 2026-08-19
---

# Known Issues

Gotchas that have cost debugging time. Remove an entry once it's resolved — this
file tracks live traps, not history (use [[Decision Log]] or [[Work Journal]]
for that).

Each entry gets a changelog comment at the top of the file, newest first:

```
<!-- Updated YYYY-MM-DD (branch, state): what changed -->
```

<!-- Updated 2026-08-19 (feat/chat-token-economy, role-gated worker prompt): recorded three gotchas -- the user-layer role split means a Worker no longer reads ~/.zirv/system-prompt.md at all (an operator with standing worker instructions must create ~/.zirv/system-prompt.worker.md), wrap's pty-harness tests wedge a spawned child in kernel exit state ?Es on this macOS machine (pre-existing on unmodified main, A/B-verified, run them on Linux CI), and five exec nudge tests time out (exit 76) intermittently in a full-suite batch while passing in isolation -->
<!-- Updated 2026-08-18 (feat/chat-token-economy, live inter-session messaging): recorded that on a standalone-installer codex-cli 0.147.0 with [windows] sandbox = "elevated", `codex exec --sandbox read-only` fails outright with a missing-helper error, so CodexAdapter::distiller_cmd's pinned --sandbox read-only breaks optimize/handoff on such installs until the sandbox helper exists or the pin is made conditional -->
<!-- Updated 2026-08-18 (feat/chat-token-economy, live inter-session messaging): recorded that a nudge/mail delivery queued for a live codex dashboard pane before dash.idle_quiet_ms output-quiescence existed simply waited forever, since a signal-less pane never reported Idle at all -- now resolved by pane_is_idle's signal-less branch, kept here as a historical trap for anyone reading an older build's behavior -->
<!-- Updated 2026-08-18 (feat/chat-token-economy): recorded an operator-machine gotcha -- a ~/.codex/config.toml model pin unsupported by a ChatGPT-plan login breaks every zirv codex delegation with a 400, since zirv passes no --model by default; resolved on this machine by removing the pin -->
<!-- Updated 2026-08-18 (feat/review-model-config): recorded the codex review-ladder model catalog as sourced from a codex-cli 0.146.0 capture, not re-verified against 0.105.0 (npm) -- the same version-split residual as the existing distiller --ignore-rules/--ignore-user-config gap -- and the equals-seat wording residual (an operator-configured review model equal in tier but spelled differently from a full-id seat keeps the strict never-clause wording, since equality is checked case-insensitively on the exact strings only) -->

<!-- Updated 2026-08-18 (feat/usage-two-window-display): both usage windows now render per harness, filtered through the new window::available staleness rule, at every display surface (dash header, wrap's bar, zirv ctx status); the refresh gates were fixed to treat a display-dropped slot as stale so a rolled-over window refreshes promptly; two residuals recorded, not fixed -- pace's hard-park path deliberately admits a rolled-over-but-recently-observed reading (test-pinned, pre-existing, distinct from `available`'s own rule), and `zirv ctx usage` prints a bare unix epoch for a passed resets_at with no "already reset" wording -->
<!-- Updated 2026-08-17 (feat/usage-credits-throttle, final review round): recorded the silent-poll-failure deviation (spec promised a one-time zirv announcement on a failed poll attempt; the shipped code degrades silently -- deviation recorded, fix deferred to a mockable-transport follow-up) and gated the usage verb's active poll on pace.enabled -->
<!-- Updated 2026-08-17 (feat/usage-credits-throttle): resolved "a limit-park is guaranteed unthrottled for a provider with no usage collector" -- codex now has both a passive rollout-file collector and an active HTTP poll fallback, so the "no collector exists at all" premise no longer holds; added three residuals -- the codex ChatGPT-backend poll endpoint ships unverified (no readable token on the reference machine), the codex rollout collector is verified against codex-cli 0.105.0's shape only, and the Anthropic OAuth usage endpoint is unofficial and may drift without notice -->
<!-- Updated 2026-08-16 (fix/process-lifecycle, c843891+222b24f): resolved the roster liveness gap (dash::roster::partition_live + sessions::short_is_live) but recorded three residuals -- the age window still applies to a genuinely-dead candidate, a live session's roster entry is re-seeded every launch and so never ages out while it stays alive, and a held-back candidate is lost if the dashboard never reaches on_quit (abort_setup's terminal-setup failure arms); narrowed the portable-pty do_kill and supervise::terminate entries to note every teardown path now tree-kills first; added three new gotchas -- a dropped ChildGuard kills its child on Windows, Job-Object assignment races a shim's own grandchild, and the distiller's kill_tree escalation ships without a dedicated test on either platform -->
<!-- Updated 2026-08-16 (feat/harness-roster-prompt, review round): owner_pid stamping moved from dash/pane.rs into SessionGuard::register itself, uniformly attributing every registration path -- a new entry records the residual it does not close: a raw pid cannot express "owned by dashboard X" from a genuinely separate process, so a dashboard pane's own child falling back to zirv ctx agent's in-process headless worker is correctly, but unhelpfully, invisible to that dashboard's own sidebar -->
<!-- Updated 2026-08-16 (feat/harness-roster-prompt): dashboard sidebar scoped to sessions owned by this dashboard (owner_pid), reversing ac40418's "show every registered session" -- a new entry records the roster-restore liveness gap found while investigating this, not fixed -->
<!-- Updated 2026-08-16 (feat/dashboard, codex review-fix final wave): agent joined REPO_FORBIDDEN (a repo ctx.toml could otherwise pick which vendor account gets spent, since resolve_default's configured arm never consulted the repo-narrowing guard); exec.rs's prompt_via_stdin/.ps1 shim-routing dropped its adapter_builds_launch conjunct so a relaunch of an explicit-command run correctly uses stdin on a shim-resolved agent instead of tripping guard_cmd_shim_reparse; the repo_narrowed pre-check and the usage/status provider fallback both got narrower false-premise fixes (agent_bin cross-adapter skip; resolve_default tried before name-derivation) -- a new entry records the limit-park loop's structural lack of throttling for a provider with no usage collector, not fixed -->
<!-- Updated 2026-08-15 (feat/dashboard, codex review-fix round 3): closed the dash/mod.rs shim-guard trip (a Windows npm codex.cmd launch now degrades to the bare task prompt instead of being refused whenever mail was pending); status.rs became the third per-provider usage surface; the nudge arm, an eventless adapter's handoff distillation, resolve_default's repo-narrowed refusal and the usage.rs no-subcommand fallback all got the same mail_deliverable/capabilities().events/ready() honesty fixes as their siblings; a new agent_bin cross-adapter collision guard was added -- codex's distiller sandbox residual (still reads .rules/config.toml on the npm-published 0.105.0) is recorded below as a new entry, not fixed -->
<!-- Updated 2026-08-15 (feat/dashboard, codex review-fix round 2): closed the two remaining mail-destruction copies (dash/mod.rs's worker-pane spawn, now also folding the F3 report-back instruction the same way; exec.rs's explicit-command shape, which has no task-prompt text to append to so it now leaves undeliverable mail untouched instead of destroying it) and the wrap.rs status-bar residual (now per-provider, same window::load_for/has_no_usage_source fix pace.rs/usage.rs got) -- the "wrap's status bar" Known Issues entry from round 1 is removed as resolved -->
<!-- Updated 2026-08-15 (feat/dashboard, codex review-fix round): mail now reaches an injection-less adapter's task prompt instead of being silently destroyed; codex's distiller pinned to --sandbox read-only and no longer defaults to claude's "haiku"; a no-events adapter reports unknown/no-data instead of a fabricated Healthy/0; pacing and `zirv ctx usage` became per-provider (wrap's status bar is the one residual unscoped reader, see below); a repo-only disable of the default agent now refuses rather than silently switching provider; two new entries recorded rather than fixed -- wrap's status bar gap, and the per-adapter (not secure-by-default) shim defense -->
<!-- Updated 2026-08-15 (feat/dashboard, codex support round): closed the codex adapter's cmd.exe shim gap (resolve_program + launches_through_cmd_shim + stdin prompt delivery) and codex::ready() no longer hard-errors -- codex is now a selectable, launchable adapter with an honestly degraded surface; what remains open is event parsing (issue #11) -->
<!-- Updated 2026-08-15 (feat/dashboard, scrolling+overlay+header round): vt100's alternate-screen scrollback trap (two independent mechanisms) plus its Ctrl+A PageUp corollary; the invisible/transparent-overlay class closed (Clear + full-frame fallback); crossterm's EnableMouseCapture banned from the dashboard; removed the now-resolved "no rot score in a pane" entry -- score::cached_score is wired into both the header and the sidebar -->
<!-- Updated 2026-08-14 (feat/dashboard, round-9 review): closed the help-probe RCE and the case-folded reserved-name bypass; Windows tree-kill, atomic state writes, and memory-prune parse safety; dashboard cursor/key-encoding/quit-latency fixes and the ⏸ glyph's removal -->
<!-- Updated 2026-08-14 (feat/dashboard, security round): cmd.exe argv-reparse injection class recorded, with the two shipped defenses and the deferred file-preference hardening -->
<!-- Updated 2026-08-14 (feat/agent-coordination, mail trust round): two latent traps recorded -- exec/loop's mail gate keys off prompt composition; wrap's status bar paints without raw mode -->
<!-- Updated 2026-08-14 (feat/dashboard, review fixes): `Ord::clamp` panics on a zero-width rect -->
<!-- Updated 2026-08-13 (feat/dashboard, docs sweep): dashboard panes carry no rot score yet -->
<!-- Updated 2026-08-13 (feat/agent-coordination, review round): markdown header absorption; registry short is a stable address; supervision env scrubbed on every spawn -->
<!-- Updated 2026-08-13 (feat/agent-coordination, console-safety round): portable-pty do_kill inversion; ConPTY control-byte broadcast; empty nudge prefixes -->

## A nudge/mail delivery queued for a live codex dashboard pane used to wait forever

**Resolved 2026-08-18 (live inter-session messaging).** Before `pane::pane_is_idle` gained a signal-less branch, a pane's idleness was decided purely by `signal_still_stands`, which requires at least one turn-boundary signal to have been seen. Codex's adapter has no turn-signal mechanism at all (`register_turn_signal` is a no-op for it), so a codex pane's `last_signal_at` never advanced past `None` and the pane read `Working` forever — the mail sweep and the nudge drain, both gated on `Idle`/`Pane::injectable`, could queue something for such a pane and it would simply sit there, undelivered, for the pane's entire life. Fixed by branching `pane_is_idle` on `AgentAdapter::capabilities().turn_signal`: a signal-less pane is now read idle by `dash.idle_quiet_ms` of pty-output quiescence instead (further hardened the same day to measure from the *latest* of output and zirv's own local input — see the 2026-08-18 [[Decision Log]] entry).

**Residual: `wrap`'s own live mail advisory (T13, above) has no equivalent for a signal-less adapter.** `wrap::may_inject` requires `InjectionState.signals_seen > 0`, sourced from the same turn-signal socket a codex session never posts to — this is a separate mechanism from `dash::pane`'s own idleness check, and only the latter gained a signal-less branch. A plain `zirv ctx wrap --agent codex` (or `chat`/bare `zirv` falling through to `wrap` on a too-small terminal) therefore never types the mail advisory line at all; `MailWatch::decide` always takes the `Announce` branch instead, which is harmless (the stderr line still fires, mail is still readable via `zirv ctx inbox`) but means live-typed delivery is a dashboard-only capability for codex today, unlike claude, which gets it in both supervisors.

## Sidebar ownership is a raw pid, so a pane child's own in-process headless fallback is invisibly unowned

`sessions::SessionGuard::register` stamps `owner_pid` with the *registering process's own* pid (`std::process::id()`), which is exactly right for a dashboard pane (registered by the dashboard process itself) but cannot express "owned by dashboard X" from a process that isn't the dashboard at all. `zirv ctx agent <name> <prompt>` run as a **child of a pane's own child** (e.g. a claude session inside a pane spawning `zirv ctx agent` as a subprocess) first tries `agent::try_join_dashboard`; when that request comes back with a `retryable: true` refusal (a channel-level failure, not a policy one — see [[Ctx Subsystem]]), it falls back to plain headless `exec::run_with`, running **inside that spawned `zirv ctx agent` process**, not the dashboard's. `SessionGuard::register` then correctly stamps that process's own pid — which is not the dashboard's — so the resulting session never appears in the spawning dashboard's sidebar, even though it started inside one of that dashboard's own panes. The session is not lost: it still reports its outcome back by mail (`prompt::with_report_back_layer`) and is listed by `zirv ctx status`, just not in any dashboard's panel.

Recorded, not fixed: closing this needs process-independent ownership — e.g. stamping the *dashboard's own registry short id* rather than a pid, threaded down through the spawn-request/fallback path — which is a deliberate non-goal of the round that added `owner_pid` scoping (see [[Decision Log]], [[Ctx Supervisors]] "View-only rows are scoped...").

## The dashboard's quit/restore roster now checks liveness, but three residuals remain

**Resolved 2026-08-16 (`fix/process-lifecycle`, P4):** `dash::roster::take_roster` used to gate a restore offer on age (`cfg.dash.roster_max_age_secs`, default 7 days) and role only, never on whether the pane it describes is actually gone — relaunching a dashboard could resurrect a worker whose previous incarnation was still running (verified on a real machine: a dashboard quit, its roster offered and restored the next day, and every "restored" worker turned out to still be the original process, alive roughly 23 hours later, still burning quota against the same repo). `run_dashboard` now partitions `take_roster`'s candidates through `roster::partition_live` (in production, `sessions::short_is_live` — a direct read of the candidate's own registry record, not `sessions::list`'s sweeping read) before ever offering the restore dialog; a candidate whose recorded pid is still alive is held back and announced (`"not restoring … : that session is still running (kept for next launch)"`) rather than offered.

**Residual 1 — the age window still exists, now for a different reason too.** A candidate whose process really has exited is still subject to `roster_max_age_secs` exactly as before; the liveness check only ever *adds* a reason to withhold an offer, never removes the age gate.

**Residual 2 — a live session's roster entry is effectively immortal for as long as it stays alive.** A held-back candidate is written straight back into the fresh roster (`deferred_restore`, merged by `on_quit`/`merge_unoffered`), and `on_quit` stamps the *whole roster* with one fresh `written: now_secs()` timestamp on every write — there is no per-pane age field. So a session that is still running gets its roster entry's age reset to zero on every dashboard launch-then-quit cycle for as long as it stays alive, and `roster_max_age_secs` never gets a chance to apply to it at all; only a session that actually exits ever starts aging out. This is the corollary of the fix, not a bug in it: the alternative (letting a live candidate's entry age out) is exactly the false-negative this round closed.

**Residual 3 — a held-back candidate can still be lost if the dashboard never reaches `on_quit`.** `deferred_restore` (and `unoffered` generally) is only ever written to disk by `on_quit`. The three terminal-setup failure arms in `run_dashboard` (`enable_raw_mode`, `EnterAlternateScreen`, `Terminal::new` failing) call `abort_setup` and return `Err` directly, never reaching `on_quit` — so a candidate this same launch just held back for being live is lost the moment setup fails, sharing the pre-existing hole every deferred/unoffered candidate has always had (a pane-cap-deferred candidate loses the same way). Not new to P4, but P4 is the first thing that can put a *live, still-running* session's roster entry through this path and lose it.

See [[Ctx Supervisors]] ("Quit and restore roster") and [[Decision Log]].

## Windows `cmd.exe` argv reparse: repo config can reach a shell command line

On Windows, `adapters::resolve_program` rewrites an npm-installed `claude.cmd`
(or `.bat`) to `cmd.exe /c <shim>`. cmd.exe then **re-parses the whole
appended command line** before invoking the shim, so any downstream argv
element bearing a cmd.exe metacharacter (`& | < > ^ ( ) % ! "` newline) is
interpreted as a *command*, not passed through as a literal argument.
portable-pty and `std::process` both append no-whitespace metachar args RAW,
and an embedded `"` defeats any quoting they add (BatBadBut / CVE-2024-24576
quote-toggle). The approach is **keep untrusted content off the reparsed
argv** rather than try to quote around cmd.exe. Defenses that ship:

1. **`chat.model` charset validation** (`config::CtxConfig::load`): the one
   repo-settable string on this path is constrained to `[A-Za-z0-9-._:/@]`, so
   it cannot express a metacharacter (see [[Decision Log]] chat.model security
   amendment).
2. **Composed prompt via file form on the cmd shim** (FIX A,
   `prompt::injection_args_for_session`): the composed system prompt folds in
   repo-sourced text (repo `system-prompt.md`, repo CLAUDE.md via the
   command-line layer). When the launch resolves to the `cmd.exe /c <shim>`
   form (`adapters::launches_through_cmd_shim`), the file form
   (`--append-system-prompt-file <zirv-controlled-path>`) is *forced*
   regardless of the `--help` probe, so that text never reaches the reparsed
   argv at all; the inline `--append-system-prompt <text>` form is never used
   for composed text there, and if the file cannot be written the launch fails
   closed (an error) rather than degrading to inline. This closes the
   repo-config RCE at every launch seam at once (`wrap`, `exec`, `loop`,
   `resume`, `chat`, dash pane). A **non-shim** launch — a direct `.exe`, or an
   `sh <script>` — is not reparsed by any shell (CreateProcess hands argv to the
   target verbatim), so inline there is safe; the `--help` probe still gates the
   file form purely as an `ps`-visibility hardening, identical on every
   platform.
3. **Headless prompt via stdin on the shim form** (FIX B, `exec`/`loop`
   through `supervise::spawn_tapped`): on a `cmd.exe /c <shim>` launch the
   headless `-p` prompt — operator task text, plus any mail folded into a
   nudge/restart relaunch — is delivered on the child's **stdin** (the
   distiller's own mechanism, `AgentAdapter::headless_cmd_stdin`) rather than
   as an argv token, so a normal prompt containing `()`/`&` works instead of
   being refused, and cmd.exe never parses it. Gated on
   `AgentAdapter::launches_through_cmd_shim`, so off Windows and for a direct
   `.exe` the prompt stays on argv and every `sh`-based fake-agent test is
   byte-identical.
4. **`adapters::guard_cmd_shim_reparse`**: the fail-closed *backstop* at every
   spawn seam (`supervise::spawn_tapped` for `exec`/`loop`; the
   `CommandBuilder` assembly in `wrap` and `dash::pane` for the pty path; and
   `resume`'s own direct `command.status()` on Windows, added with FIX C). It
   rejects a launch whose program is the `cmd.exe /c <shim>` — or
   `powershell -File` (FIX D, defense-in-depth) — form and whose args carry a
   cmd.exe metacharacter. After FIX A/B the only free text still on a reparsed
   argv is an **interactive positional prompt** (a chat first message, a
   `resume` handoff prompt, a dash worker task) — operator/zirv-generated and
   rarely metachar-bearing. A no-op off Windows and for any non-shim program.

**Round-9 fixes to the above (2026-08-14) — two gaps found by adversarial
review, both now closed:**

5. **The `--help` capability probe was itself unguarded.** `detect_help_flag`
   spawned `cmd.exe /c <shim> <bin_args> --help` to test for
   `--append-system-prompt-file` support *before* FIX A/B/D's own logic ever
   ran — and `program_invocation` forwards every positional before the first
   flag, so on `zirv chat --resume` (whose handoff summary is distilled from
   the untrusted checkout) a repo-controlled metacharacter reached `cmd.exe`
   inside the probe itself. This was a live RCE independent of fixes 1-4.
   `detect_help_flag` now runs `guard_cmd_shim_reparse` against the exact
   probe argv before spawning, reporting "unsupported" on rejection.
6. **FIX A/B never actually engaged on the launches that most needed them.**
   `adapters::launches_through_cmd_shim` re-resolved `launch.first()` and saw
   a plain `cmd.exe` with an empty prefix, returning `false` — so the forced
   file-form injection was inert on `zirv chat`, bare `zirv`, and the
   dashboard's orchestrator pane (defense present but never applied), and
   `zirv chat --resume` was then hard-refused by the FIX-D backstop with no
   way to succeed. `adapters::launch_reparses_through_shim` now also
   recognises an **already-resolved** `cmd.exe /c <shim>` (or
   `powershell -File`) argv, not just one `resolve_program` would still
   rewrite.

**Residual (usability, not security):** an *interactive* initial prompt that
contains a raw cmd.exe metacharacter is still refused by the backstop on a
Windows npm `.cmd` install (rephrase it). Headless is the common automation
path and is not subject to this (FIX B delivers it via stdin).

**The codex adapter's shim gap is closed.** `CodexAdapter::base()` now
routes `self.program` through `resolve_program`, exactly like claude, so an
npm-installed `codex.cmd` launches through `cmd.exe /c <shim>` on Windows
instead of failing outright. `launches_through_cmd_shim` is overridden the
same way claude's is (`super::launches_through_cmd_shim(&self.program)`), so
`exec`/`run_loop`'s FIX B branch recognises a codex shim launch and delivers
the headless prompt via `headless_cmd_stdin` (codex's own verified stdin
fallback: `codex exec` with `[PROMPT]` omitted reads from stdin) instead of
as an argv token cmd.exe would reparse. `codex::ready()` no longer hard-errors
either -- it mirrors `ClaudeAdapter::ready` (`resolve_program(&self.program)?`)
-- so codex is a selectable, launchable adapter, just with no event parsing
wired up yet (`parse_events`/`structural_context` stay empty; no rot score, no
usage source, no turn signal, no injected system prompt). There is no
`system_prompt_file_flag` override because there is still no verified
per-run system-prompt mechanism at all for codex, so nothing is ever put on
argv for that flag to move off of. Full event support is tracked in
[issue #11](https://github.com/Glubiz/zirv-dynamic-cli/issues/11). See [[Ctx
Adapters]].

## The Windows `cmd.exe`-shim defense is opt-in per adapter, not secure by default

`AgentAdapter`'s trait defaults for `launches_through_cmd_shim` (`false`) and
`headless_cmd_stdin` (`None`) are the *insecure* answers -- "this adapter is
never a shim launch" and "no stdin form, keep the prompt on argv." Both of
today's adapters override them correctly (`claude.rs`, `codex.rs`, see [[Ctx
Adapters]]), so the reparse-argv RCE class documented earlier in this file is
closed for both. But nothing enforces the override: a third adapter that
implements `AgentAdapter` and simply doesn't override these two methods
compiles cleanly and passes every existing test, and silently ships with a
headless prompt (operator task text, plus any mail folded in via `task_
prompt_with_mail_fallback`) sitting on argv even on a Windows npm `.cmd`
install -- `guard_cmd_shim_reparse` still catches an actual metacharacter at
spawn time (the fail-closed backstop holds), but a clean prompt sails through
unprotected where claude's and codex's own prompts would have moved to
stdin. Recorded here deliberately as a note, not a fix: making the trait
default secure (`launches_through_cmd_shim` defaulting to "ask `resolve_
program`" rather than `false`, or restructuring so an adapter cannot omit
the override at all) is a real refactor with its own blast radius, out of
scope for the round that found this gap.

## Codex's distiller sandbox still reads the repo's `.rules` and the operator's `~/.codex/config.toml`

`CodexAdapter::distiller_cmd` pins `--sandbox read-only`, codex's analogue of
claude's `--disallowedTools` pin backing `zirv ctx optimize`'s report-only
guarantee -- but unlike claude's `--disallowedTools`, which is the *whole*
restriction claude needs, codex-cli genuinely ships two more flags that would
close a gap `--sandbox` does not touch: `--ignore-rules` (skip project/user
execpolicy `.rules` files) and `--ignore-user-config` (skip
`$CODEX_HOME/config.toml`). They are documented on `codex exec --help` for
codex-cli 0.146.0 (the brew-installed capture in
`docs/superpowers/notes/2026-07-31-codex-cli-facts.md`), but **not** on
0.105.0, the version `npm install -g @openai/codex` actually publishes
(verified on a real Windows machine) and the one `distiller_cmd`'s own doc
comment is written against. Passing either flag on 0.105.0 would very likely
error as an unrecognized argument, breaking the distiller for the common
install path. So today, a repo's own `.rules` execpolicy files and the
operator's own `~/.codex/config.toml` still shape what this "report-only"
judgment child does, on top of AGENTS.md already being embedded in its
prompt (the one residual claude's distiller has too, and cannot close either
-- `--disallowedTools` restricts tools, not what text the model reads). Add
`--ignore-rules --ignore-user-config` to `distiller_cmd` once the
npm-published codex-cli ships them, verified against that installed CLI the
same way `-s, --sandbox` was.

## `--sandbox read-only` fails outright on a codex-cli install with the Windows sandbox helper missing

On one real machine (codex-cli 0.147.0, the standalone OpenAI installer, `[windows] sandbox = "elevated"` in `~/.codex/config.toml`), `codex exec --sandbox read-only` — the exact flag `CodexAdapter::distiller_cmd` pins for `zirv ctx optimize`/handoff's report-only guarantee (see the entry above) — fails immediately with `windows sandbox: orchestrator_helper_launch_failed ... helper=codex-windows-sandbox-setup.exe ... program not found`, rather than degrading or falling back. `codex exec` with no sandbox flag at all works on the same install. Since `distiller_cmd` always passes `--sandbox read-only` unconditionally, every `zirv ctx optimize`/handoff run that resolves to the codex distiller fails the same way on such an install until either the sandbox helper binary is present or the pin is made conditional on the installed CLI actually supporting it. Not fixed here — recorded so a codex-distiller failure that looks like a zirv bug is checked against this first.

## A `~/.codex/config.toml` model pin unsupported by the operator's login breaks every zirv codex delegation with a 400

zirv passes no `--model` to codex by default (`CodexAdapter::default_worker_model()` is `None`, and `worker.codex`/`review.codex` are both unset unless the operator configures them — see [[Ctx Adapters]]'s "The delegated-worker model default"), so an unconfigured codex launch runs on whatever `codex exec`'s own resolution picks. If the operator's own `~/.codex/config.toml` pins a `model` that the account's actual login (a ChatGPT-plan session, not an API key) does not support, every codex launch fails at the vendor with an HTTP 400 — indistinguishable at zirv's level from any other codex startup failure, since zirv never sees or validates the pinned name; it is entirely outside zirv's own config surface. Resolved on this machine by removing the pin from `~/.codex/config.toml` and letting codex's own default apply. If a codex delegation (`zirv ctx agent codex ...`, a dashboard codex pane, or codex code review) fails outright with a 400 and no obviously bad zirv config, check `~/.codex/config.toml` for a `model` line before looking anywhere in this codebase.

## The codex review-ladder model catalog is sourced from a 0.146.0 capture, not re-verified against npm's 0.105.0

`CodexAdapter::review_model_below`'s ladder (`gpt-5.6-sol` → `gpt-5.6-terra` →
`gpt-5.6-luna` → `gpt-5.4-mini`) is sourced from `codex debug models` in
`docs/superpowers/notes/2026-07-31-codex-cli-facts.md`, captured on codex-cli
**0.146.0** (a brew-only capture). It has **not** been re-verified against
0.105.0, the version `npm install -g @openai/codex` actually publishes and
the version most operators get — the same version-split residual as the
existing `distiller_cmd` gap ("Codex's distiller sandbox still reads the
repo's `.rules`...", below): a real catalog difference on 0.105.0 would ship
silently wrong review-model names in the harness-roster prompt line, not a
crash. Verify against a real 0.105.0 install and update the ladder (or note
the split) once one is available to test with.

**Related, narrower wording residual:** `review_roster_line`'s never-clause
softens to "never on a model above the named one" only when a resolved
review model's text equals the orchestrator seat's text, checked
case-insensitively on the *exact strings*. An operator who configures a
review model equal in tier but spelled differently from a full-id seat (e.g.
`chat.model = "claude-opus-4-5"` with `review.claude = "opus"`) is not
detected as equal, so the line keeps the strict "never on an orchestrator
seat's own model" wording even though the two names may resolve to the same
underlying model. Not a security gap (the routing rule itself still holds,
`review.claude` still wins), just a cosmetic case the equality check does not
catch.

## The codex ChatGPT-backend poll endpoint ships unverified

`poll::HttpPoller`'s codex arm (`https://chatgpt.com/backend-api/codex/usage`)
was implemented and unit-tested against synthetic response bodies only — no
readable `~/.codex/auth.json` token existed on the reference machine to
exercise it against the real endpoint. `parse_codex_usage` is exercised by
`codex_response_parser_accepts_rate_limits_shapes_and_rejects_junk` against
hand-built JSON, but nothing in the suite has ever seen a genuine response
from this URL. It ships best-effort per an explicit user ruling (see
`docs/superpowers/specs/2026-08-16-usage-credits-throttle-design.md`): every
failure mode (wrong shape, wrong auth header, endpoint moved) degrades to
`None`, same as any other poll failure, so the worst case is "the fallback
poll never contributes data," not a crash or a bad reading. Verify against a
real response and add a fixture (mirroring
`tests/fixtures/anthropic-oauth-usage.json`) once a working codex OAuth token
is available to test with.

## The codex rollout collector is verified against codex-cli 0.105.0's shape only

`window::parse_rollout_line`/`windows_from_rate_limits` were verified against
the real rollout JSON codex-cli 0.105.0 writes (the same npm-published
version the rest of this codebase's codex-shim work is pinned against —
see [[Ctx Adapters]]'s distiller-sandbox residual for the sibling case), and
`tests/fixtures/codex-rollout-rate-limits.jsonl` is a fixture of that exact
shape. A future codex-cli release that changes the `token_count` event's
`rate_limits` object (renamed fields, restructured `primary`/`secondary`, a
different timestamp format) silently degrades to "snapshot not recognized,
provider treated as having no data" rather than erroring loudly — the
collector was built to fail this way on purpose (an unrecognized shape must
never crash a pacing decision), but that also means a real shape change on a
newer codex-cli would ship silently broken until someone notices codex usage
never appears in the header/status. No version-detection or shape-versioning
exists to catch this.

## The Anthropic OAuth usage endpoint is unofficial and may drift

`https://api.anthropic.com/api/oauth/usage` is not a documented, versioned
public API — it is the same endpoint Claude Code's own CLI calls internally,
reverse-engineered for `poll::HttpPoller`'s Anthropic arm and verified
against one real response (`tests/fixtures/anthropic-oauth-usage.json`).
Anthropic can change or remove it without notice. `parse_anthropic_usage`
degrades to `None` on any shape it doesn't recognize (missing `five_hour`/
`seven_day` objects, a renamed `utilization` field, a body that isn't valid
JSON at all) rather than erroring, so a drifted endpoint silently stops
contributing to the poll fallback — the passive statusline-tee collector is
unaffected either way, since it reads Claude Code's own rendered payload,
not this endpoint.

## `x.saturating_sub(n).clamp(1, x)` panics when `x` is 0

`Ord::clamp` asserts `min <= max` and panics otherwise, so the idiom
"shrink by a margin, but keep at least 1 and never exceed the area" is a live
panic whenever the area is zero -- which a real session reaches (a terminal
narrowed to at most `dash.sidebar_cols` makes `ui::layout`'s own main rect
zero-width, and `ZIRV_CTX_DASH_SIDEBAR_COLS` larger than the terminal does it
at startup). The release profile is `panic = "abort"`, so this is not a
recoverable error anywhere near a TUI. Use `.max(1).min(x.max(1))` instead
(`ui::dialog_width`), and guard whole renderers with `Rect::is_empty`.

## A full-screen child on the alternate screen has NO vt100 scrollback -- raising `Parser::new`'s scrollback argument does not fix it

An empirical probe (spawning the real harness in a pty, answering ConPTY's
`ESC[6n` cursor-position query so output actually flowed, and parsing the
stream through vt100) established that Claude Code spends the whole session on
the alternate screen (`ESC[?1049h`) and enables its own mouse reporting
(`?1000h ?1002h ?1003h ?1006h`, SGR). Two **independent** traps live in vt100
0.16.2, not one:

1. The alternate grid's scrollback is hardcoded to zero
   (`vt100-0.16.2/src/screen.rs:76`) -- a pane hosting a full-screen harness
   can never accumulate scrollback no matter how large `Parser::new`'s own
   scrollback argument is. Raising it from 0 to 1000 (an earlier fix) was
   necessary but not sufficient for exactly this reason.
2. `grid.rs:566` only retires rows into scrollback when `scrollback_len > 0 &&
   !scroll_region_active()`, so a child that also sets a DECSTBM scroll region
   silently defeats scrollback a second, independent way.

**Consequence:** `Ctrl+A PageUp`/`Home`/`End` do nothing but print an
explanatory notice on a full-screen child -- there is no synthesised wheel
event to send. Unprefixed `PageUp` still reaches the child directly. This is
expected behavior, not a bug to chase: the real fix is not more vt100
scrollback, it's the dashboard deciding per pane at scroll time whether to
forward the wheel to a child that owns the mouse or fall back to vt100
scrollback on a normal-screen child. See [[Decision Log]].

## An overlay that draws nothing over part of its rect silently eats every keystroke, not just looks wrong

`render_dialog` used to draw only a `Block` border and a `Paragraph`, so any
cell inside the dialog's rect the text didn't reach kept whatever the pane
grid had drawn there -- a "dialog" that read as pane content bleeding through
a border. `render_overlay` separately returned early when the main rect was
too small to host it, leaving nothing drawn at all while the overlay still
owned input. Neither failure mode is cosmetic: while **any** overlay is open
the event loop never calls `filter_key` at all -- `Ctrl+A` isn't even tested
for prefix-ness -- and every reducer keeps itself open on an unmatched key, so
only `Enter`/`Esc` close one. An invisible modal therefore presents as "the
dashboard has stopped responding to `Ctrl+A` entirely," which is exactly how
this was reported from a real session: the first `Ctrl+A s` opened the spawn
dialog, and everything typed after it was silently swallowed by a dialog the
operator could not see. Fixed: `render_dialog` renders `Clear` first (opaque),
and `render_overlay` falls back to the full frame when the main rect can't
host it. Any future overlay must render `Clear` and must never treat "too
small to draw into" as a reason to skip drawing -- input is already committed
to it either way.

## `crossterm::EnableMouseCapture` must never be used in the dashboard

It turns on `?1000h` (click) **and** `?1002h`/`?1003h` (motion tracking)
together, with no way to enable only wheel/button reporting. A probe on a real
Windows Terminal session showed `?1003` emitting a `MouseEventKind::Moved`
event for every pixel of pointer movement -- dozens from one sweep across the
window -- competing with keystrokes inside the bounded per-tick input drain,
for a feature (`Ctrl+A`-scrolled panes) that only ever reads
`ScrollUp`/`ScrollDown`. The dashboard writes `?1000h?1006h` itself as raw
bytes instead (`term::dash_mouse_on_bytes` -- wheel + button reporting, SGR
coordinates only) and resets all four modes on exit regardless of which were
enabled. `[dash] mouse` (default true, `REPO_FORBIDDEN`, env override
`ZIRV_CTX_DASH_MOUSE`) is a genuine trade, not a strict improvement: enabling
mouse reporting takes over the terminal's own native click-drag text
selection (hold Shift to bypass it -- the same trade every real terminal
multiplexer makes).

## Dashboard special-key encoding must carry the xterm modifier parameter, and crossterm's own control-key pre-mapping must be undone explicitly

Special keys (arrows, Home/End/PageUp/…) used to be encoded with no modifier
information at all (`CSI <final>` / `CSI <n>~`), so e.g. Ctrl+Left reached
the child as a plain unmodified Left — word-wise movement was unreachable in
any pane. Fixed: they now carry the xterm modifier parameter (`CSI
1;<mod><final>` / `CSI <n>;<mod>~`, `mod = 1 + shift + 2*alt + 4*ctrl`).
Separately, crossterm pre-maps several control combinations to plain `Char`
events before zirv ever sees them (`Ctrl+Space` arrives as `Char(' ')`,
`Ctrl+\` as `Char('\\')`, etc.), so encoding those literally typed the
visible character instead of sending a control byte. The pane's key encoder
now special-cases them back to their real bytes: Ctrl+Space→`0x00`,
Ctrl+\→`0x1c`, Ctrl+]→`0x1d`, Ctrl+^→`0x1e`, Ctrl+_→`0x1f`. Shift+Enter sends
`ESC CR` (does not submit); bare Enter still sends `\r` and submits — see the
[[Decision Log]] entry for why `ESC CR` was chosen over CSI-u. Any future
terminal-input feature must check both failure modes — a missing modifier
parameter, and a control combination crossterm already collapsed to a bare
character — not just the common `Char` + `CONTROL` shape.

## `PaneState::WaitingInput` and its `⏸` glyph do not exist

Removed 2026-08-14 (round-9 review): the variant had no producer and never
rendered in the real dashboard render loop, so the sidebar could never show
it. Real glyphs are `●` working, `○` idle, `·` view-only, `✕` ended. A true
"waiting on input" indicator would need a new turn-signal kind end-to-end,
not just a state variant — do not re-add the enum case without one.

## The mail-vs-`composed` delivery decision is open-coded at ~11 call sites, not one seam

The original version of this entry described a real bug: `exec`/`loop` used
to gate "was this mail delivered?" on "did we build a `composed` prompt?"
rather than on `adapter.capabilities().system_prompt`, which for an
injection-less adapter (codex) under `--simple`, or on a Windows `cmd.exe`
shim launch, silently destroyed mail or refused a spawn outright depending on
which copy of the bug a given call site had. That class is now closed —
`exec.rs` (the launch, park, rot-restart and nudge arms), `run_loop.rs`,
`dash/mod.rs`'s `compose_worker_prompt`/`fulfill_spawn_request`, and
`wrap.rs`'s own restart arm each now compute their own
`mail_deliverable`/`should_list_mail`/shim-safety condition from
`adapter.capabilities().system_prompt` (and, where relevant,
`launches_through_cmd_shim()`) rather than from `composed.is_some()` alone.

What is left, deliberately not fixed in the same round that closed the bug
itself: that condition is hand-written independently at roughly eleven call
sites rather than behind one shared delivery-seam function. Nothing is wrong
today — every site was verified and tested individually — but a future
adapter whose capability shape does not match claude's or codex's (or a
future call site added without reading this note) can drift from the others
without either compiler or test catching it, since there is no single
function whose signature would force the new site to ask the same question
the same way. Extracting one seam (something like `fn mail_channel_for
(adapter, launch_shape) -> MailChannel`) is a real refactor with its own
blast radius across every one of those files — out of scope for the round
that closed the underlying bug.

## `wrap`'s status bar paints whenever stdout is a tty

Bar eligibility is decided on stdout being a terminal, but the bar reserves a
screen row and repaints assuming it owns the display, which really requires
raw mode on stdin. When stdout is a tty and stdin (or raw mode) is not, the
bar still reserves and paints, leaving reserved-row artifacts in scrollback.
Cosmetic only — nothing is lost and the session is unaffected — but it is why
a `wrap` run with redirected stdin can litter the terminal.

## A markdown header block ends at the first blank line

`mail::parse_markdown` and `memory::parse_markdown` both read a `## Message` /
`## Memory` header of `- key: value` bullets followed by a free-form body. The
header block ends at the **first blank line** -- the one `to_markdown` always
writes after the last bullet -- and everything after it is body, verbatim.

This is a trust boundary, not a formatting detail. Both bodies are
agent-authored text. When a blank line merely `continue`d (leaving the parser
in header mode), a body whose first line happened to be a `- key: value`
bullet was absorbed as header: a mail message could re-address itself
(`- To-session: victim`) or forge its sender, and a memory entry could rewrite
the `Key` it is filed under or promote itself from `handoff` to `explicit`. It
also silently ate any honest bulleted body (`- build: cargo build`), which is
how it was first noticed.

If either parser grows a new header field, keep the terminator rule intact.

## A supervisor's registry short id is a stable address, not its session id

`Record.short` is minted once at `SessionGuard::register` and deliberately
**not** rotated by `refresh_session`, even though `Record.session` is. It is
the address `resolve_prefix` hands a sender, what `send --to-session` and
`zirv ctx nudge` store on a message, and what `zirv ctx status` prints.

Rotating it (which is what `loop` did per cycle and `exec` per restart) made
every message addressed to a live session undeliverable the instant that
session was replaced -- the sender resolved a real address and the supervisor
then stopped answering to it. Every mail listing a supervisor performs on its
own behalf must therefore be scoped to the registry short, never to
`short_id(current session)`.

Consequences to preserve: `loop` filters on it too (passing `None` made a loop
swallow *and consume* mail addressed to other sessions), and `exec`'s nudge
marker is claimed under it (deriving it from `session` meant a nudge sent after
the first restart was never claimed).

## Anything spawned from a supervised session must have its supervision env scrubbed

`sessions::SUPERVISION_ENV` (`ZIRV_CTX_SESSION`, `ZIRV_CTX_SOCKET`,
`ZIRV_CTX_TRANSCRIPT`) has to be `env_remove`d from **every** child command
before the spawner sets whichever of it it owns -- `portable_pty::CommandBuilder::new`
and `std::process::Command` both inherit the parent environment, so "not set"
means "inherited", not "absent".

This is not limited to the supervisors' own agent children. It also covers
`handoff::run_model` (the distiller, and therefore `memory::harvest_from_handoff`,
which spawns through it) and `resume`'s hand-over launch. A distiller that
inherits its parent's session id posts turn signals into the parent's own rot
engine while the parent sits blocked waiting for that very call to return.
<!-- Updated 2026-08-12 (feat/obsidian-vault, seeded): initial gotchas pulled from repo CLAUDE.md -->

## `supervise::terminate` on Windows used to kill only the direct child, not the tree

On an npm-installed `claude`, the process a supervisor spawns is
`cmd.exe /c claude.cmd`, and `claude.cmd` runs node — so the direct child is
the launcher, not the agent. `terminate`'s non-unix arm called `child.kill()`
(`TerminateProcess` on that one pid), so every rot verdict, timeout, and
nudge relaunch killed the launcher, `try_wait` reported success, and the
supervisor spawned a **second** agent against the same repo while the first
kept running underneath — two live sessions burning quota and writing files,
invisible to each other. Fixed 2026-08-14: the Windows arm now runs
`taskkill /T /F /PID <pid>` (a numeric pid, no shell, no new dependency)
first and falls back to `child.kill()` only if that fails.

**This fixed only `supervise::terminate` (`exec`/`loop`'s own escalation
ladder) — the pty seams (`wrap`, the dashboard's panes) had no tree-kill at
all until 2026-08-16.** `wrap::quit_child` and `dash::pane::Pane::finish_
shutdown` had only ever called portable-pty's own `Child::kill()`, so a
quit or a rot-restart on either could leave the very same npm-shim
grandchild running that this entry describes for `exec`/`loop`. The
underlying primitive is now shared: `taskkill_tree` was renamed `kill_tree`
and promoted `pub(crate)` so both pty seams (plus the distiller's own
timeout escalation in `handoff.rs`) call the identical function `exec`/`loop`
always have, rather than reimplementing it. See [[Ctx Supervisors]] and the
2026-08-16 [[Decision Log]] entry for the fuller Windows lifecycle picture
(the console-close pid registry and the Job-Object backstop this same round
added on top).

## `state::write_private` used to leave a zero-length window a concurrent reader could observe

Writing was create-truncate-then-write. A read landing in that window (e.g.
`sessions::list`) saw a zero-byte file — indistinguishable from "record
absent" — and `sweep_orphaned_markers` then deleted that session's pending
`.nudge` as orphaned, silently losing the wake-up. Fixed 2026-08-14:
`write_private` now writes a temp sibling and renames over the target
(atomic on both platforms), with the unix `0600` forcing moved onto the temp
file so writing over a pre-existing world-readable file still lands private.

**Residual, by design, not a bug:** `memory::prune_to_cap` now refuses to
delete any entry it cannot confidently parse (a partial read used to score
`written=0`, sort first, and get evicted — so a racing `verify` could lose an
entry outright), and `remember`'s best-effort duplicate-collapse for one key
is not a lock. A genuine two-writer race can still transiently leave two
files for the same key on disk; the next list-based operation (`recall`,
`prune_to_cap`) converges back to one, and reads stay deterministic
meanwhile — but don't assume "one file per key" holds at every instant.

## portable-pty's Windows `do_kill` inverts its own success check

`WinChild::do_kill` in portable-pty 0.9.0 (`src/win/mod.rs`, lines 41–50) reads:

```rust
let res = unsafe { TerminateProcess(proc.as_raw_handle() as _, 1) };
let err = IoError::last_os_error();
if res != 0 { Err(err) } else { Ok(()) }
```

Win32 `TerminateProcess` returns **non-zero on success**, so this reports a
successful kill as an error and a failed one as success. `ChildKiller::kill`
then swallows the result with `.ok()` anyway, so zirv never learns a kill
failed: `child.kill()` in `wrap::quit_child` always looks like it worked.

Do **not** vendor or patch portable-pty for this. Treat `kill()` as
best-effort and never build logic on its return value — `try_wait()` /
`wait_for_exit` are the only trustworthy evidence a child is actually gone,
and `quit_child` already keys on those.

**The guidance stands; the blast radius shrank (2026-08-16).** Every pty
seam (`wrap::quit_child`, `dash::pane::Pane::finish_shutdown`) now runs
`supervise::kill_tree` (`taskkill /T /F /PID`, by pid only) *ahead of* the
narrow `child.kill()` this entry describes, the same escalation `exec`/`loop`
have always had. `kill_tree`'s own return value is still not evidence of
anything — it is a best-effort escalation, not a replacement for
`try_wait`/`wait` — so this entry's core guidance (never trust a kill's
return value; only wait/try_wait proves death) is unchanged. What changed is
what a failed narrow kill now leaves behind: with `kill_tree` run first, the
*direct* child (and, on Windows, whatever a kill-on-close Job Object still
holds — see below) is very likely already gone by the time `child.kill()`'s
untrustworthy result is even consulted, so the inverted-check bug's practical
consequence — "zirv thinks the tree died when it didn't" — is a narrower
window than before this round.

## Dropping a `ChildGuard` on Windows kills the child it guards

`supervise::ChildGuard` (2026-08-16) owns a supervised child's membership in the console-close pid registry (P2) and its kill-on-close Job Object (P3). `ChildGuard::release()`/`Drop` close the job handle — and closing the *last* handle to a `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` job is precisely what makes the kernel terminate every process still in it. That is the intended backstop when the child is already confirmed dead (closing an empty job kills nothing), but it means **the guard must never go out of scope while its child might still be alive** — an early return, a `?` on an unrelated error, or any other unplanned scope exit between adopting a guard and confirming the child's exit (`try_wait`/`wait`) will kill a live agent as a side effect of ordinary Rust cleanup, not a bug in the job-object mechanism itself. Every current call site (`spawn_tapped`'s returned guard held for the whole `exec`/`loop` cycle, `wrap`'s `child_guard` released explicitly after `pump` returns, `dash::pane::Pane`'s `lifecycle` field released in `shutdown`/`finish_shutdown`) is written to hold the guard for exactly the child's lifetime; a future call site that stores a `ChildGuard` in something with a shorter lifetime than its child will silently kill that child the moment the guard drops.

## Job-Object assignment is a race against a shim's own grandchild, not a guarantee

`JobGuard::adopt` calls `AssignProcessToJobObject` immediately after spawn, but `AssignProcessToJobObject` only pulls in descendants a process creates **after** it is assigned — anything the process already spawned before assignment landed is not retroactively added. On an npm-installed shim launch (`cmd.exe /c claude.cmd` → `node`), the assignment happens on the very next statement after `spawn()`, so in practice it lands well before `cmd.exe` has started `node` — but this is a timing race, not a structural guarantee, and portable-pty offers no `CREATE_SUSPENDED` (or equivalent) seam to close it outright. This is why the Job Object is a backstop layered *underneath* `kill_tree`'s `taskkill /T`, never a replacement for it: `taskkill /T` walks whatever tree exists **at kill time**, so it still catches a grandchild the job assignment raced and missed. Losing this race requires an unusually slow assignment or an unusually fast shim; no reproduction is recorded, but the mitigation (the other two lifecycle layers) is unconditional regardless. See [[Decision Log]].

## The distiller's `kill_tree` timeout escalation ships without a dedicated test on either platform

`handoff::run_model`'s timeout arm now calls `supervise::kill_tree(child.id())` on Windows before its existing `child.kill()`/`wait()` (2026-08-16, P1), the same escalation `wrap`/`exec`/`loop` gained. The existing coverage (`run_model_gives_up_at_the_timeout`) exercises the timeout path itself (a hung fake model, asserting `run_model` returns `Err` inside the deadline) but asserts nothing about tree-kill behavior specifically. On Linux CI the call is `#[cfg(not(unix))]` and never compiled at all; on a real Windows machine it compiles and runs as part of that same test, but nothing in the suite spawns a shim-shaped grandchild and asserts it died. Verification for this call site today is the same manual recipe as the rest of the Windows lifecycle work (`term.rs`'s doc comment), not an automated one — consistent with the existing Windows-environmental-gap pattern this project already has for other Windows-only paths.

## Never write a control byte into a pty master

Writing `\x03` (or any console control byte) into the pty master is not a
signal to *that one child*. On Windows the master is a ConPTY and conhost
turns the byte into a console control event broadcast to **every** process
attached to the pseudoconsole; portable-pty 0.9.0 spawns without
`CREATE_NEW_PROCESS_GROUP`, so there is no process group to narrow it to. On
unix the line discipline delivers SIGINT to the whole foreground process
group of that pty — better, but still not one process.

This is why `wrap::quit_child`'s ladder is quit sequence → grace →
`child.kill()` with nothing in between, and why the removed Ctrl-C rung must
not come back. See the [[Decision Log]] entry for 2026-08-13.

## An empty or very short `zirv ctx nudge` prefix is refused

`sessions::resolve_prefix` accepts any *unique* prefix, including `""`
(`starts_with("")` is always true). That is fine for a read-only lookup but
not for a nudge, which wakes and — in `exec` — restarts the session it
resolves to: a single mistyped character can still be unique. `zirv ctx
nudge` therefore refuses any prefix shorter than four characters
(`sessions::MIN_NUDGE_PREFIX`) unless it exactly equals a live session's
whole short id.

Test helpers that used to lean on the empty prefix (`exec`'s
`nudge_live_session`, `wrap`'s interactive-nudge test) now resolve the live
short id from the registry and pass it whole. A test that still passes `""`
fails with `prefix too short`, and — if its cleanup runs after the call —
can leak `FAKE_AGENT_*` environment variables into every later test in the
same process.

## A delegated worker no longer reads `~/.zirv/system-prompt.md` at all

The user prompt layer became role-scoped on 2026-08-19: an Orchestrator session reads
`~/.zirv/system-prompt.md` as before, a Worker session reads the separate, optional
`~/.zirv/system-prompt.worker.md` (`prompt::WORKER_PROMPT_FILE`) instead, and **neither role
reads the other's file**. Before the split a Worker read `system-prompt.md` too, so an
operator whose standing instructions there were partly aimed at delegated workers silently
loses that half on upgrade: the fix is to copy the worker-relevant part into the new file.
Nothing warns about this — an absent worker file is a completely normal state (it means "no
worker user layer", exactly like an absent `system-prompt.md` means no orchestrator one), so
there is no signal zirv could honestly distinguish from an operator who never wanted one.
Both files live in the operator's home directory; a repo checkout has no equivalent (its own
`.zirv/system-prompt.md` is still the single, capped, labeled Repo layer for both roles).
See [[Utilities]] for the layer list and [[Decision Log]] for the reasoning.

## `ctx` shadows `.zirv/ctx.yaml`

`zirv ctx` is a built-in resolved in `main.rs` before YAML script lookup, so a
`.zirv/ctx.yaml` script named `ctx` is silently shadowed and never runs.
`.zirv/ctx.toml` is a different file — it's the ctx config, and it's excluded
from script listing in `help.rs`.

**Reserved-name interception must fold case, and must gate every dispatch
path, not just the pre-clap one.** Fixed 2026-08-14 (round-9 review): the
pre-clap `ctx`/`chat`/`agent` interception in `main.rs` compared `argv[1]`
case-sensitively, while `utils::is_reserved_command` (case-insensitive) was
never consulted from the built-in lookup path — so `zirv Chat` fell through
to script lookup and ran a repo `.zirv/Chat.yaml`, a file `zirv help`
simultaneously reported as "shadowed by a built-in, unreachable." Both now
fold case, and `is_reserved_command` gates script dispatch before
`get_file_path()` for the clap-dispatched built-ins too. Deliberate UX change
worth knowing: a mis-cased built-in like `zirv Help` now exits 1 with
"reserved command name" rather than printing help.

## Tests must run with `--test-threads=1`

`cargo test --verbose -- --test-threads=1` is required, not optional — tests
share state (state dir, fixtures) and will flake or corrupt each other under
the default parallel test runner.

## `wrap`'s pty-harness tests wedge their spawned child on at least one macOS machine

Every `#[cfg(unix)]` test in `wrap.rs` that goes through `spawn_wrap`/`spawn_wrap_with_flags`
(24 as of 2026-08-19; a local run skips 27, adding the three that open a pty directly with
`native_pty_system`) hangs on one reference macOS machine (Darwin 25.5.0): the spawned
`zirv ctx wrap` child reaches kernel
exit state `?Es` after its `/exit` and never reaps, so the test blocks forever in
`Child::wait`. **Pre-existing and unrelated to any branch** — A/B-verified 6/6 against
unmodified `main`, both sandboxed and unsandboxed. Killing the parent test binary's specific
pid clears the wedge (never `pkill`/`killall` by name — other real sessions share those
names). Linux CI runs the whole family normally, and it is the authority for them.

Practical consequence: a full local suite on such a machine must skip the family, e.g. one
`--skip commands::ctx::wrap::tests::<name>` per test, and any change to a pty test's own
synchronisation can only be reasoned about locally, not executed — see the 2026-08-19
[[Work Journal]] entry for a change made under exactly that constraint.

## Five `exec` nudge tests time out intermittently in a full-suite batch

`commands::ctx::exec::tests::a_nudge_on_a_simple_codex_run_still_delivers_its_own_guidance`,
`…::a_nudge_on_an_explicit_command_codex_run_delivers_the_nudge_mail_on_the_relaunch`,
`…::a_post_nudge_park_carries_the_nudges_own_mail_not_the_stale_launch_mail`,
`…::a_nudge_restart_does_not_spend_the_rot_restart_budget`, and
`…::a_headless_worker_stops_at_the_next_poll_and_relaunches_with_the_guidance` fail with
exit-76 (`EXIT_TIMEOUT`) assertions, and each of them passes on its own in well under a
second. **Pre-existing and non-deterministic, verified by A/B on 2026-08-19:** running exactly
these five as one filtered batch fails two of them in ~61s on both `feat/chat-token-economy`
and its own merge-base commit — and *which* two differs between runs (branch:
`…explicit_command_codex_run…` + `…post_nudge_park…`; base: `…rot_restart_budget…` +
`…post_nudge_park…`). Each spawns a real supervised child whose progress depends on a nudge
landing inside a 5-second window (`nudge_live_session`'s own wait gives up silently), so on a
loaded machine the nudge misses, the fake agent stays in `hang` mode, and the run burns its
whole 30-second wall clock instead.

Practical consequence: a red result here is only evidence when the test is run alone. Rerun
the individual test before treating it as a regression, and report both outcomes rather than
either one alone — a batch result on its own cannot tell a regression from this.

## `wrap`'s hot path assumes `panic = "abort"`

The release profile is `panic = "abort"`, so a panic on `wrap`'s hot path
cannot unwind to a cleanup handler — raw-mode terminal restore must happen in
explicit arms, not in a `Drop` guard relying on unwind. No `unwrap`/`expect` on
that path; any supervision failure must degrade to pure passthrough instead of
leaving the terminal in raw mode.

## A failed usage poll is silent (recorded spec deviation)

The approved design (spec §3/§6, `docs/superpowers/specs/2026-08-16-usage-credits-throttle-design.md`) called for a one-time `zirv ▸` announcement per process the first time an active usage poll is attempted and fails, mirroring the `pace_no_source_announced` latch. The shipped `poll::maybe_poll` cannot distinguish "did not poll" from "polled and failed" in its return type, so no caller can announce the failure, and an operator with an expired or missing OAuth token sees only silently stale usage. Recorded as a deliberate deviation at the final whole-branch review (2026-08-17) rather than an oversight; the fix wants a `maybe_poll` return-type change plus a mockable transport for testing (see the Task 5 deferred note), not a quick patch. Until then: if usage looks stuck-stale on a machine that should be polling, check the token files by hand.

## Polling is structurally inert on keychain / API-key setups

`poll::anthropic_token` reads only `~/.claude/.credentials.json`. Claude Code installs that store OAuth tokens elsewhere (macOS Keychain) or authenticate via API key / Bedrock have no such file, so the active poll can never acquire data there. Combined with `has_no_usage_source` being a plain no-data check, a claude machine with no statusline tee and no readable credentials file gets no usage-based pacing at all — the gate announces `pacing off: anthropic has no usage source` once per run (that one-time announcement is the signal; estimator-based pacing, if configured, still applies as of the 2026-08-17 fix round). The remedy on such machines is wiring the statusline tee (`zirv ctx usage tee`), which needs no credentials.

## Pace's hard-park path can admit a rolled-over reading `window::available` would already hide

`pace::binding()` deliberately keeps a *stale* collector reading binding — skipping the normal `collector_max_age_secs` freshness check — when it was last seen at or above `max_percent` and its `resets_at` hasn't passed yet, since a window cannot free up before its own reset (see [[Usage and Pacing]]'s `pace.rs` section). This is intentional and test-pinned, not a bug. It is, however, a genuinely different staleness rule from the 2026-08-18 display filter, `window::available`: a reading young enough to still be `binding` for pacing purposes can simultaneously be old enough, or have a `resets_at` close enough, to render as a blank usage segment on the header/bar the moment `available` would drop it for a different reason (e.g. the same slot rolling over between the pacing check and the next redraw). An operator can therefore see a session correctly parked with no visible usage percentage explaining why. `zirv ctx usage` (no subcommand) is the surface that shows the raw, unfiltered reading behind a park.

## `zirv ctx usage` prints a bare epoch for an already-passed `resets_at`

`usage::report`'s `line_for` formats a known `resets_at` as `"resets at unix <n>"` regardless of whether that instant is already in the past — unlike every display surface `window::available` now filters (dash header, `wrap`'s bar, `zirv ctx status`), which read a passed `resets_at` as `unknown`. This is deliberate (the verb is meant to show the raw data pacing is deciding from, see [[Usage and Pacing]]'s gotchas), but the wording itself is not: an operator reading `report`'s output has to notice the epoch is in the past themselves, with no "(already reset)" annotation to flag it.
