---
last-verified: 2026-08-24
---

# Known Issues

Gotchas that have cost debugging time. Remove an entry once it's resolved — this
file tracks live traps, not history (use [[Decision Log]] or [[Work Journal]]
for that).

Each entry gets a changelog comment at the top of the file, newest first:

```
<!-- Updated YYYY-MM-DD (branch, state): what changed -->
```

<!-- Updated 2026-08-24 (fix/codex-pane-messaging, review round F1-F7 on PR #116): amended the #114 entry -- the settle gap is now a deferred, tick-drained submit (Pane::pending_submit/drain_pending_submits) instead of a blocking sleep on the dashboard's UI thread, and wrap.rs's own /compact and mail-advisory injections share the same shape; also closed a report-back persistence gap (a restored worker pane used to permanently lose its report_to target) -->
<!-- Updated 2026-08-24 (fix/codex-pane-messaging, issues #114/#115, v2.25.2): resolved the codex-pane paste-burst CR-fold that left a nudge/mail/report-back injection typed but unsubmitted, and the silent report-back-omission/no-reminder gap for a dashboard-spawned worker; residual noted that live-codex behavioral proof of both is pending the Docker matrix -->
<!-- Updated 2026-08-24 (perf/test-suite-speed, PR #113, v2.25.1): resolved four test-hermeticity gaps surfaced while switching the suite to nextest's per-process isolation (real-$HOME leak in context_cli.rs/handoff.rs/handover.rs, review.rs's dash_channel_active hard-coded env read, a 24-name DASH_REQUESTS_ENV ambient-failure baseline in wrap.rs/resume.rs when run under a dash session, and the fake-codex-agent.sh --help-probe mode-shift that caused CI run 32723969751's one deterministic failure); recorded the drain_to_eof supervise-race fix and a still-open read_until blocking-read residual -->
<!-- Updated 2026-08-24 (feature/first-run-setup, v2.25.0): recorded two residuals from the guided first-run wizard -- the hook-install fallback installs both harnesses' hooks regardless of the operator's individual enable/disable answers, and a nesting-guard skip produces no output rather than a hint that `zirv setup` exists; also recorded that three setup.rs tests (the new N1 harvest-decline regression test plus the pre-existing issue-#87 pair) assert `cfg.memory.*` without clearing every `ZIRV_CTX_MEMORY*` env var -->
<!-- Updated 2026-08-23 (fix/shipped-posture-allows-zirv, issues #99/#100): recorded that Drop for SignalServer on Windows removes only the marker file, never the acceptor/drainer threads or the named-pipe instance itself -- discovered while adding signal::probe for the #99 orphaned-endpoint sweep (sessions::sweep_orphan_endpoints), since a same-process drop-then-reprobe cannot observe "endpoint gone" the way the unix build can -->
<!-- Updated 2026-08-23 (feat/close-open-issues, review-fix round): recorded that the handover structural (no-model) packet is thin -- see the new entry below; also documented the HOME-layer-vs-REPO-layer parse-failure split (CtxConfig::load vs load_for_launch) in Untrusted Configuration.md -->
<!-- Updated 2026-08-23 (feat/close-open-issues, vault keeper pass): removed "workflow::repo_gates's fail-closed test no longer matches CtxConfig::load's new parse-skip behaviour" -- verified against src/commands/workflow/{mod,verification}.rs: repo_gates's doc comment and the renamed test (an_unparseable_repo_config_does_not_disable_a_gate_it_never_controlled) already reflect the narrower fails-closed-only-on-Err claim the entry asked for; it described a residual from earlier in the same session's diff that a later part of the same commit (issues #88/#90/#91) already closed -->
<!-- Updated 2026-08-23 (feat/close-open-issues, issues #86/#85/#89): resolved "codex has no event parsing" -- CodexAdapter::parse_events/structural_context now derive turn boundaries and token totals from the same rollout JSON window.rs already parses (shared via window::parse_rollout_record), capabilities().events is honestly true, and rot scoring/status/pacing all light up for codex; tool calls/tool results/compaction remain unmapped (no verified rollout shape), recorded in the codex-shim-gap entry below. Surfaced (not closed) the Windows-codex context-injection fallback: zirv ctx status now reports "codex: context via task-text fallback" on a shim-resolved launch, and prompt::injection_event's wording matches; investigated and rejected codex's own -p/--profile file-form injection as a closure path (would require writing into the operator's own $CODEX_HOME, a boundary held read-only everywhere else). Surfaced the codex distiller/reviewer sandbox residual: --ignore-rules/--ignore-user-config are now added automatically when a --help probe confirms the installed codex-cli documents them, and a one-time zirv ▸ announcement fires when it does not -- see the rewritten "Codex's distiller/reviewer sandbox residual" entry below -->
<!-- Updated 2026-08-23 (feat/close-open-issues, ctx.toml parse-skip fix): a repo-only ctx.toml TOML-syntax error no longer fails CtxConfig::load (config.rs's UnparsableLayer, see the Decision Log) -- workflow::repo_gates's fail-closed Err arm therefore no longer fires for that one case, and verification.rs's an_unparseable_repo_config_closes_the_check_gate_instead_of_failing_the_run now fails; recorded below as a residual for the workflow subsystem to reconcile (out of scope for this change, which touched only config.rs/status.rs/announce.rs) -->
<!-- Updated 2026-08-23 (feat/close-open-issues, issues #88/#90/#91): resolved "Workflow classification's git-based safety net fails open outside a git repository" (classify.rs/engine.rs now fail safe: RiskMeasurement::Unavailable escalates the risk band one step) and "The workflow secret filter is a name denylist" (review.rs gained a second, content-based gate: token-shape + entropy detection behind the filename denylist); updated "Verification reports accumulate with no retention pruning" to record that pruning now runs (reusing telemetry.rs's prune_expired_except) but shares telemetry's own retention config rather than a dedicated key, a scope residual from a concurrent edit on src/commands/ctx/config.rs -- see the Decision Log. Also updated two now-stale tests (skill.rs, verification.rs) that asserted the pre-2026-08-23 all-or-nothing "any unparsable ctx.toml closes both workflow gates" contract; a concurrent, separate change (config.rs's per-layer parse-skip redesign, documented in [[Untrusted Configuration]]) means a plain repo-layer *syntax* error no longer disables repo_checks_enabled/repo_skills_enabled, since both are REPO_FORBIDDEN and were never repo-settable either way -- a REPO_FORBIDDEN key rejection still closes both gates, pinned by a new companion test -->
<!-- Updated 2026-08-23 (feat/close-open-issues, issue #92): resolved "The Windows cmd.exe-shim defense is opt-in per adapter" -- AgentAdapter::launches_through_cmd_shim's trait default now derives its answer from resolve_program(self.program()) instead of a hardcoded false, so an adapter that overrides nothing is protected; see Ctx Adapters and the Decision Log -->
<!-- Updated 2026-08-23 (fix/dashboard-and-harness-parity, vault keeper pass): resolved the T8 finding that the pacing gate did not cover interactive sessions at all -- T10 (already shipped in this diff) wired pace::resolve_interactive_gate into wrap's pre-spawn path and the dashboard's first-pane/worker-pane spawn seams, closing exactly the gap this entry described -->
<!-- Updated 2026-08-22 (fix/dashboard-and-harness-parity, zirv setup restore): recorded that ~/.claude.json (MCP registrations, OAuth/account linkage, project trust state) is out of scope for both `zirv setup reset` and the new `zirv setup restore` -- clearing it would sign the operator out, so it is neither backed up nor touched, and both commands say so in their own output -->
<!-- Updated 2026-08-22 (fix/dashboard-and-harness-parity): resolved the residual that wrap's own live mail advisory never fired for a signal-less adapter (codex) -- T13's eligibility check now branches on AgentAdapter::capabilities().turn_signal, the same way dash::pane::pane_is_idle already does, via new signal_less_mail_ready/mail_inject_ready and an InjectionState.last_input field -->
<!-- Updated 2026-08-21 (feat/frontend-quality, PR #78 quality-contract v2): expanded render evidence to three widths and the detector to 44 inventory-covered rules; the scored review raises accountability but still cannot prove model attention -->
<!-- Updated 2026-08-21 (docs/vault-merge-pass, memory+context+workflow rework merge train): recorded eleven residuals surfaced across the three topics' review rounds -- repo_slug canonicalization orphaning pre-existing state, the shared memory scope's unix-only symlink tests plus its inherent TOCTOU window, stamp_verified_in_place's CRLF normalization, the canonical policy model's malformed-repo-config hole (MUST close with #44), the ALL_LAYERS/Capability::ALL exhaustiveness ceiling, the shared-block closing-marker's literal-only match, a cross-surface duplicate with no eligible diff target, the workflow secret filter's name-denylist gap, classification's git-based safety net failing open outside a repo, the FNV-1a fingerprint hash, and verification reports' unbounded retention -->
<!-- Updated 2026-08-21 (feat/workflow-system, PR #59 review fixes): extended the codex --sandbox read-only entry -- the pin now also reaches the workflow reviewer via AgentAdapter::read_only_args, so a broken sandbox helper breaks review as well as the distiller -->
<!-- Updated 2026-08-19 (feat/dash-adaptive-poll-help-overlay, uncommitted, extends PR #29): resolved a mail-routing gap -- a dashboard-spawned worker pane's report-back reply used to broadcast rather than address, claimable by the wrong pane (issue #30); extended the Shift+Enter ESC-CR entry to cover Alt+Enter and the Windows-Terminal key-folding root cause; extended the crossterm::EnableMouseCapture entry for ?1002 now being on (click-drag text selection + OSC 52 copy) -->
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

## `OutputTap::try_lines`'s "final drain" could still lose a fast-exiting child's last line

**Resolved v2.25.1 (`perf/test-suite-speed`, PR #113).** `exec.rs`'s and `run_loop.rs`'s post-`supervise_child`/`supervise_run` "final drain" — added to close the race where a fast limit-hit exit slips past the last tick that would have caught it — called `tap.try_lines()`, a pure, instantaneous, non-blocking drain with no synchronization against `spawn_tapped`'s own reader threads reaching EOF. A child that printed its last line and exited immediately could still have that line in flight (the reader thread not yet scheduled to read it out of the pipe) at the exact instant `child.wait()`/`try_wait()` observed the OS-level exit — so the "final drain" could lose exactly the race its own comment claimed to close. Fixed by `OutputTap::drain_to_eof(budget)`, a bounded blocking drain (`supervise::FINAL_DRAIN_BUDGET`, 500ms) that waits only as long as it actually takes for the reader thread(s) to deliver more lines or disconnect; both call sites now use it. See [[Ctx Supervisors]].

## Test-suite hermeticity gaps found while moving to per-process (nextest) isolation

**Resolved v2.25.1 (`perf/test-suite-speed`, PR #113).** Switching the primary local/CI loop to `cargo nextest run` (one process per test, see [[Technology Stack]]) surfaced several tests that had been silently relying on the developer's own real environment rather than an injected/isolated one — invisible under a single shared-process serial run, where an earlier test's env mutation could coincidentally leave the right value in place.

- **Real-`$HOME` leak.** `context_cli.rs`'s test `repo()` helper, and one `--agent codex` test each in `handoff.rs` and `handover.rs`, resolved the developer's own global `~/CLAUDE.md`/context or ran `AgentGate::load` (both real-`$HOME`-backed via `crate::utils::home_dir()`) with no `HomeGuard` in place — a machine with a real `~/CLAUDE.md`, or codex disabled in the developer's own `~/.zirv/.settings.toml`, produced spurious drift findings or an unrelated refusal. Fixed by binding a `testenv::HomeGuard` (and, in `context_cli.rs`, a dedicated home tempdir) for the duration of each affected test.
- **`review.rs`'s `dash_channel_active` had no injectable seam.** It read `std::env::var(DASH_REQUESTS_ENV)` directly, unlike `sessions::nested_session_evidence`'s own `EnvLookup`-based read of the same variable — not observed to leak in practice, but the same shape that does once something mutates that var process-wide. Now takes a `config::EnvLookup<'_>`; the production call site still passes the real environment, so behavior is unchanged.
- **A 24-name `DASH_REQUESTS_ENV` ambient-failure baseline when the suite runs under a zirv dash session.** `sessions::SUPERVISION_ENV` deliberately excludes `DASH_REQUESTS_ENV` (a pane's own child must still be able to reach the spawn-request channel), so it was never scrubbed at any of wrap.rs's three real-pty-spawn test sites or resume.rs's one real-spawn test site. Running the suite itself from inside a zirv dashboard pane exports `DASH_REQUESTS_ENV` into that whole process tree, so every one of those spawned `zirv ctx wrap`/`zirv ctx resume` children inherited it and tripped the nesting guard the comment beside them already scrubs three other signals (`SUPERVISION_ENV`, `CLAUDE_PID`, `CLAUDECODE`) for. Fixed by scrubbing `DASH_REQUESTS_ENV` explicitly at all four real-spawn sites. One of these (`a_broken_transcript_path_never_stops_the_session`) had been hanging outright once the scrub stopped short-circuiting the guard before the child's own exit path was reached (reproduced twice, ~20+ minutes combined) — `wrap.rs` gained a `wait_bounded` helper (mirroring the pre-existing `win::wait_bounded` for `std::process::Child`) so a portable-pty child that is genuinely wedged is killed and the test fails fast (10s) instead of hanging.
- **`fake-codex-agent.sh`'s `--help` capability probe stole a fixture mode line.** `CodexAdapter::detect_ignore_flags` probes `codex exec --help` before composing a nudge/rot-restart's distiller call; the test override routes that probe through the same fixture used for the main agent, and the probe's argv has no `--sandbox read-only` pair, so the fixture's pre-existing `is_distiller` exemption did not cover it — it popped a real line off `FAKE_AGENT_MODE_FILE`, shifting every later hang/limit/healthy mode by one and silently turning a "limit" stage into "healthy" (no prompt-injection log entry, no limit-park). This was CI run 32723969751's one deterministic failure (`a_post_nudge_park_carries_the_nudges_own_mail_not_the_stale_launch_mail`) and was distinct from the `try_lines`/`drain_to_eof` race above, which the same test also happened to exercise. Fixed by giving the fixture its own `is_help_probe` branch: still logged (a test relies on seeing `exec --help` in `FAKE_AGENT_ARGV_LOG`), never pops a mode, and answers without `--ignore-rules`/`--ignore-user-config` so the probe reads "unsupported", matching CI's pre-fix behavior.

See [[Technology Stack]]'s "Test runner" section and [[Ctx Supervisors]] for the mechanism; `.config/nextest.toml`'s `exec-nudge-restart` test-group (serializing the family of writer-thread-driven nudge tests against each other, `threads-required = "num-test-threads"`) is the belt half of the belt-and-braces fix for the underlying scheduling contention, not a fix for any of the four items above.

**Residual, not fixed: `wrap.rs`'s `read_until` test helper is a genuinely blocking read with no per-read timeout.** Its outer `while Instant::now() < deadline` loop only re-checks the deadline *between* calls to `reader.read(&mut buf)` — a single call that never returns (no data, pipe not closed) can hold the loop past its nominal timeout under extreme host load, independent of anything fixed above. Not touched by this branch; recorded so a `wrap.rs` pty test that stalls well past its stated `read_until` timeout is checked against this before being treated as a new regression.

## The first-run wizard's hook-install fallback ignores the operator's per-harness answers, and a nested skip is silent (v2.25.0)

`commands::setup::apply_first_run_answers`'s `install_hooks` branch, when the current directory has no local `.zirv` to run `run_apply` against, calls `install_claude_integration` *and* `install_codex_integration` unconditionally — an operator who answered "no" to enabling codex in the harness-enable step still gets codex's hooks installed if they accept the later "install harness hook integration now?" prompt. The two questions are independent by construction (`FirstRunAnswers.harness_enabled` is never consulted by the `install_hooks` arm). Not fixed — deferred past the two-fix-round cap as a non-blocking finding.

Separately, `main.rs`'s `maybe_run_first_run_wizard` checks `ctx::sessions::nesting_refusal("chat", &env, false)` before ever prompting, and returns with no output at all when it refuses — a never-configured operator who types a bare `zirv`/`zirv chat` from inside an existing agent session (a dashboard pane, a nested `wrap`) gets silence, not a hint that `zirv setup` exists to configure things later. Also recorded, not fixed: `zirv chat --allow-nested` (or any other flag after `chat`) never arms the wizard either, since the gate is `verb == "chat" && argv.len() == 2` — a deliberate, accepted side effect of the same fix that stops `zirv chat --help` from running it (see [[Built-in Commands]]'s "The guided first-run wizard").

## Three `commands::setup` tests assert `cfg.memory.*` without clearing every `ZIRV_CTX_MEMORY*` env var (v2.25.0)

`apply_first_run_answers_lets_a_decline_actually_turn_off_a_previously_true_harvest` (new) clears `ZIRV_CTX_AGENT`/`ZIRV_CTX_CHAT_MODEL`/`ZIRV_CTX_MEMORY` via `VarGuard` before asserting `cfg.memory.harvest`, but not `ZIRV_CTX_MEMORY_HARVEST` — the exact env var `REPO_FORBIDDEN`-overrides the very field it asserts on (see [[Ctx Subsystem]]'s `REPO_FORBIDDEN` table). The two pre-existing issue-#87 tests (`declining_memory_harvest_leaves_the_default_off_but_remembers_it_was_asked`, `accepting_memory_harvest_writes_only_to_the_home_layer_never_the_repo`, `setup.rs`) have the identical gap. On a machine or CI runner with `ZIRV_CTX_MEMORY_HARVEST` set in the real process environment, `ctx::config::CtxConfig::load`'s env layer would silently override whatever the test just wrote to `ctx.toml`, producing a false pass or a false failure unrelated to the code under test. Not fixed — recorded so a red result on one of these three tests is checked against the runner's own environment before being treated as a regression.

## The handover structural (no-model) packet is thin

`zirv ctx handover`'s live swap (and `--dry-run` preview) distills a handoff packet via `handoff::distill_or_structural`. When the target adapter has no verified event parsing, or the distiller model call fails/times out, that function falls back to the mechanical `structural(ctx)` extraction rather than a real model summary. The structural packet carries only a task line (the last user prompt) and the most recent tool error, if any -- no done/remaining/gotchas breakdown, no file list. It is enough for the successor to know roughly what it was doing, not a substitute for a real handoff. The distilled path (a real model call) is the one that actually produces a useful packet; `--dry-run`'s own header prints `packet source: distilled`/`structural`/`no data` so an operator can tell which one a preview actually got, but the live swap's ack (`HandoverAck`) carries no equivalent field today -- a swap that silently fell back to structural is not otherwise surfaced at the point of the swap.

## `~/.claude.json` is out of scope for `zirv setup reset`/`restore`

`~/.claude.json` holds Claude Code's MCP server registrations, OAuth/account linkage, and per-project trust state. `zirv setup reset` never adds it as a candidate at either scope, and `zirv setup restore` therefore never has anything to restore for it either — both are correct today, but an operator could reasonably assume a full `restore` reconstitutes *everything* a `reset` might have disturbed, including MCP servers. It does not: this file is deliberately excluded because clearing it would sign the operator out, well beyond "clear the harness's configuration." `reset --dry-run` and `restore --list` both print an explicit note naming the file and the reason. If MCP server configuration is ever lost, it must be reconstructed by hand (`claude mcp add ...`) or from the operator's own backup — zirv has never touched this file and has no copy of it.

## Autonomous frontend rendering covers static discoverable routes on a local Chromium-family browser

`zirv frontend render` now discovers plain HTML, Vite-based React/Vue/Svelte, Next, Astro, Nuxt, and Dioxus web targets in bounded nested workspace paths, pins known servers to loopback with adapter-owned argv, rejects unknown generic scripts, and validates actual PNG dimensions. It still does not synthesize authenticated sessions or fixture data, instantiate dynamic routes, drive native/mobile/browser-engine-specific UIs, or use a harness-native screenshot API. A runner, browser, script, route, or server that is missing or times out is recorded as `unavailable`/`failed`, and review/verify cannot pass — there is deliberately no source-only fallback that claims visual quality. Closing the remaining state and route coverage gaps needs explicit adapter-owned render providers or a bounded fixture contract; it must not reintroduce human server startup or screenshot registration.

The browser's host-resolver rule maps DNS names away from external hosts and the served URL is loopback, but it is not an OS network sandbox: a page that fetches a literal external IP may still reach it if the operator's environment allows that traffic. Repository start scripts remain arbitrary checkout-authored code and therefore run only when the operator-only `workflow.repo_checks_enabled` gate is open, with direct argv, fixed local host/port inputs, timeouts, and process-tree cleanup.

## Frontend detector craft rules are high-signal heuristics, not a proof of visual taste

Blocking detector rules are kept to structural accessibility hazards with relatively objective source evidence; contextual craft/UX rules are advisory because a flagged technique can be legitimate in an established product system. Explicit, provenance-preserving waivers handle known legitimate uses without silently weakening the global rule set, and repository configuration cannot suppress blocking findings. The AI visual review must inspect rendered evidence and use the autonomous profile/product provenance to resolve context. A pass requires 13 explicit UI/UX scores at 4/5 or better, no unresolved finding, fresh capture linkage, and an isolated reviewer/model identity; CLI callers can no longer enter scores or verdicts. Zirv still cannot prove a model attended to every pixel or exercised an undiscoverable state. The 41-case benchmark proves all 44 deterministic rules are represented across focused plus HTML/React/Vue/Svelte/Astro/Dioxus/utility-CSS fixtures and catches detector drift, not subjective design-quality regressions across model releases.

## Repository state slugs now canonicalize, and a relocated slug orphans state filed under the old one

**Fixed 2026-08-21 (`fix(ctx): give one repository one state slug`).** `state::repo_slug` now canonicalizes the path before slugging it, so macOS's `/var` → `/private/var` split, or any symlinked checkout, no longer writes one repository's memory/mail/handoffs/workflow state across two different slugs depending on which spelling a given caller happened to pass in.

**Residual: no migration for state filed under the old, non-canonical slug.** A repository whose raw and canonical paths already differed before this fix has real state sitting under the old slug; after upgrading, every reader computes the new canonical slug and simply does not find it — the old directory is not moved, merged, or even flagged as orphaned. There is no cleanup command for it today.

## The shared memory scope's symlink defenses are unix-tested only, and can't close every race or forgery

`memory.rs`'s `safe_shared_dir` refuses `.zirv/`/`.zirv/memory` when either is a symlink, and `read_entries`/`is_regular_file` skip a symlinked entry file within an otherwise-legitimate shared bank — both defenses are exercised only via `std::os::unix::fs::symlink` in tests. **Residuals, not fixed:** (1) no test exercises a Windows junction/reparse point, which `symlink_metadata`'s `is_symlink()` may not classify the same way a real unix symlink does. (2) The check is inherently a TOCTOU window: `safe_shared_dir` calls `symlink_metadata` and a later read/write reopens the path by name, so a concurrent local process that swaps a real directory for a symlink between the two can still be raced — closing this for good needs an `O_NOFOLLOW`-style atomic open this code does not use. (3) `select_memory_within_cap`'s closing-marker suppression (see the 2026-08-21 [[Decision Log]] entry) matches only the exact `SHARED_BLOCK_END_MARKER` literal, case-insensitively — a lookalike (extra whitespace, a homoglyph, a different case *of a different string*) renders as ordinary body text rather than being caught, which is the deliberately narrow, honest scope of that suppression, not a bug in it.

## `stamp_verified_in_place` normalizes CRLF to LF on a Windows checkout

`memory.rs`'s `stamp_verified_in_place` (the write path of `zirv memory verify`) splits the file on `text.lines()`, which strips both `\n` and `\r\n` without recording which one it saw, then rejoins every line with a plain `\n` (`out.join("\n")`). A memory file checked out with Windows line endings survives a verify call with its byte content correct but every one of its line endings silently converted from `\r\n` to `\n`.

## The canonical permissions policy has no enforcement caller yet, and a malformed repo `[policy]` table still erases an operator's own narrowing

`policy.rs`'s `EffectivePolicy`/`evaluate`/`PolicyReport` are fully built and tested but have no production caller — issue #44 (the context compiler) is what will pin a stance onto a real launch, and issue #46 (`zirv ctx status`) what will render a report. Until then, `hook.rs`'s `a_malformed_repo_policy_table_still_erases_the_operators_own_policy_narrowing` test pins a real, recorded gap: `cfg_or_operator_only_gate`'s error arm (reached when a repo `[policy]` table fails to parse) falls back to `CtxConfig::default()`, whose `policy` field is all-`Allow` — the exact trust hole review finding 1 already closed for `[agents]` (which does have an operator-only salvage path), but not yet for `[policy]`. **This is a MUST-close item, not an accepted residual:** issue #44 must add the same salvage path `AgentGate` already has and flip this test's assertion from `Stance::Allow` to `Stance::Deny`, or the canonical policy model would ship on top of a hole one malformed repo file can already open.

## Two hand-maintained exhaustiveness guards can't catch a variant that is never appended to their own array

Both `optimize.rs`'s `ALL_LAYERS: &[Layer]` and `policy.rs`'s `Capability::ALL` are hand-maintained arrays paired with an exhaustive (no-wildcard) match, each pinned by a position-agreement test. What that test catches: a variant duplicated or reordered relative to the array (the realistic drift — copy-pasting an arm instead of adding one). What it provably cannot catch: a brand-new enum variant that gets its own honest match arm but is never appended to `ALL_LAYERS`/`Capability::ALL` at all — such a variant is simply never exercised, since every test call is seeded from the array's own contents. Closing this needs a derive macro (`strum::EnumIter`) or nightly's `variant_count`, neither pulled in; documented as a ceiling on the current design, not a bug in it.

## A cross-surface duplicate whose only other copies are canonical gets no proposed diff

`optimize.rs`'s `lint_redundancy` proposes a diff (delete the later copy) for a repeated instruction, but `is_eligible_deletion_target` refuses two kinds of copy as a deletion target: an operator's own global surface, and — when the duplicate spans more than one surface — a `Layer::ContextCommon`/`ContextClaude`/`ContextCodex` copy in the canonical `.zirv/context/` layer. A duplicate group whose only candidates besides the first occurrence are canonical-layer copies therefore still gets a `Finding`, correctly flagging the redundancy, but with `proposed_diff: None` — nothing for an operator to apply, only the observation that it exists.

## The verification fingerprint hash is FNV-1a, not a cryptographic hash

`verification.rs`'s `change_fingerprint` (the value that proves a verification report matches the tree it was run against) hashes `git rev-parse HEAD` plus a diff plus every changed path's blob hash through `event::input_hash` — FNV-1a 64, chosen originally for the rot engine's own deterministic-across-compilers event hashing. FNV-1a has no collision resistance against a deliberately constructed input; a party who could already engineer a specific fingerprint collision could already control the diff and paths being fingerprinted, so this is a low-severity, recorded reuse of a non-cryptographic hash for an integrity-adjacent purpose, not a new attack surface.

## `package.json` discovery has no size cap before it is read and parsed

`verification.rs`'s check discovery reads the whole of a repo's `package.json` into a `String` and parses it as JSON with no byte-size guard, unlike `optimize.rs`'s surface collection (`cfg.optimize.max_surface_bytes`) or `review.rs`'s untracked-file cap (`MAX_UNTRACKED_FILE_BYTES`). An unusually large `package.json` is read and parsed in full before any of its `scripts` are consulted.

## Verification report retention shares telemetry's config key, not a dedicated one

**Resolved 2026-08-23 (issue #91), with a scope residual.** `verification.rs`'s `save_report` now names each report file with a leading zero-padded timestamp (`{finished_at:020}-{id}.json`, the same shape `telemetry.rs` already uses) and calls `telemetry::prune_expired_except` after every write, so a long-lived repository no longer accumulates one file per verification run indefinitely; the `latest` report's own filename is always passed as a protected entry, so it survives pruning even when it is itself older than the retention window.

**Residual: the retention value is `[workflow] telemetry_retention_days`, not a dedicated `verification_retention_days`.** The issue asked for a verification-specific `REPO_FORBIDDEN` key following telemetry's shape, but `WorkflowConfig` lives in `src/commands/ctx/config.rs`, which a concurrent branch (`SetupConfig`, issues #87/#93/#95) was actively editing while this change landed — adding a field there risked clobbering that work. `verification.rs`'s `resolved_retention_days_from_config` calls `telemetry::TelemetryConfig::from_config(cfg).retention_days` directly instead, so the same already-`REPO_FORBIDDEN`, already-clamped value governs both. The two windows cannot be tuned independently today; `resolved_retention_days_from_config` is the one seam to change if a genuinely separate key is added later. See the Decision Log entry.

## A nudge/mail delivery queued for a live codex dashboard pane used to wait forever

**Resolved 2026-08-18 (live inter-session messaging).** Before `pane::pane_is_idle` gained a signal-less branch, a pane's idleness was decided purely by `signal_still_stands`, which requires at least one turn-boundary signal to have been seen. Codex's adapter has no turn-signal mechanism at all (`register_turn_signal` is a no-op for it), so a codex pane's `last_signal_at` never advanced past `None` and the pane read `Working` forever — the mail sweep and the nudge drain, both gated on `Idle`/`Pane::injectable`, could queue something for such a pane and it would simply sit there, undelivered, for the pane's entire life. Fixed by branching `pane_is_idle` on `AgentAdapter::capabilities().turn_signal`: a signal-less pane is now read idle by `dash.idle_quiet_ms` of pty-output quiescence instead (further hardened the same day to measure from the *latest* of output and zirv's own local input — see the 2026-08-18 [[Decision Log]] entry).

**Resolved 2026-08-22 (`fix/dashboard-and-harness-parity`).** `wrap`'s own live mail advisory (T13, above) had no equivalent for a signal-less adapter: `wrap::may_inject` requires `InjectionState.signals_seen > 0`, sourced from the same turn-signal socket a codex session never posts to — a separate mechanism from `dash::pane`'s own idleness check, and only the latter had gained a signal-less branch. A plain `zirv ctx wrap --agent codex` (or `chat`/bare `zirv` falling through to `wrap` on a too-small terminal) therefore never typed the mail advisory line at all; `MailWatch::decide` always took the `Announce` branch instead — harmless (the stderr line still fired, mail was still readable via `zirv ctx inbox`) but meant live-typed delivery was a dashboard-only capability for codex, unlike claude, which got it in both supervisors. Fixed the same way `pane_is_idle` was: `wrap.rs`'s new `signal_less_mail_ready`/`mail_inject_ready` branch T13's own eligibility check on `AgentAdapter::capabilities().turn_signal`, measuring quiet off the *later* of `InjectionState`'s `last_output` and a new `last_input` field (mirroring `dash::pane::latest_of`) against `cfg.dash.idle_quiet_ms` — the same non-`REPO_FORBIDDEN` timing knob `dash::pane::Pane::idle_quiet` already reads, reused rather than adding a second wrap-only one for the identical question.

**Resolved 2026-08-24 (`fix/codex-pane-messaging`, issue #114, v2.25.2; mechanism amended same day, review F1/F2/F4 on PR #116).** Even once a codex pane read `Idle` and the nudge/mail/report-back sweeps delivered into it, the delivery itself could still sit unsubmitted: `inject_visible` wrote the labelled line and its single trailing `\r` in one `write_all`, and codex's ratatui composer reads a burst of bytes arriving within a few milliseconds of each other as a paste — folding the `\r` inside that burst into the pasted text as a literal newline instead of reading it as a submit keypress. The message then sat typed but unsubmitted in the composer until a human happened to press Enter, indistinguishable from the pre-fix "waits forever" symptom this same section already covers. The first cut of the fix (`pane::write_two_phase_injection`) split the write in two but still *blocked the caller* for `INJECTION_SUBMIT_DELAY` (a hardcoded 50ms, deliberately not a config key — see the 2026-08-24 [[Decision Log]] entry) between them — on the dashboard that caller is the single UI thread every sweep shares, so a handful of injections in one tick could freeze redraw/input for the sum of their delays (up to ~1.35s across nine panes), and a failed second-phase write left `injected_awaiting_turn`/`last_local_input_at` unset, letting a retry sweep double-type a line onto the still-unsubmitted first. The shipped mechanism is now a **deferred, tick-drained submit**: `write_injection_phase1` writes the line and returns immediately; `Pane::inject_visible` stamps pane state right away and records a `pending_submit` deadline; `dash::mod::drain_pending_submits`, called every tick, writes the lone `\r` (`write_submit_cr`) once the deadline passes, and `Pane::write_operator_input` drains a pane's own pending submit eagerly before an operator's own keystroke reaches the composer — no blocking sleep anywhere, and a failed `\r` write is a safe, idempotent retry. `wrap.rs`'s own `/compact` injection and T13 mail advisory share this exact shape (see [[Ctx Supervisors]]'s "Both the mail advisory and `Action::Compact` share..."), closing the same paste-fold hazard there and fixing a second bug along the way: the mail advisory used to call `MailWatch::commit_injected` the instant its single write succeeded, marking a message delivered before the child had even seen the submitting keypress. See [[Ctx Supervisors]]'s "Injection semantics".

**Resolved 2026-08-24 (`fix/codex-pane-messaging`, issue #115, v2.25.2): a dashboard-spawned worker's report-back contract used to be silent and unverified.** `compose_worker_prompt`'s report-back layer was already skipped for an unaddressable requester, but nothing told the operator a worker pane had been launched with no way to report its outcome back, and nothing ever reminded a worker that had gone quiet without sending one. Fixed by (a) a `"report-back-omitted"` decision-log entry logged whenever mail is enabled but the requester is unaddressable, and (b) a new `report_back_reminder_sweep` (same cadence as `mail_sweep`) that injects one reminder — naming the exact `zirv ctx send --to-session <id>` command — into an idle worker pane that has produced output and carries a `report_to` address, exactly once per pane's life; unconditional on whether the report already went out, since the only mail-delivery log today (`"mail-consumed"`) is written on the recipient's *read*, not the sender's send, so there is no durable send-side "already sent" signal to gate on. A same-day review round (F3/F6, PR #116) closed a persistence gap the initial fix left open: `spawn_restored_pane` never called `Pane::set_report_to`, so a worker pane that survived a dashboard restart permanently lost its reminder target; `roster::RosterPane` now carries `report_to`/`report_reminder_sent` (`#[serde(default)]`, so an older roster file with neither key still parses) and a restore hands both back. See [[Ctx Supervisors]]'s "Pane model" and the 2026-08-24 [[Decision Log]] entry.

**Residual, both #114 and #115: live-codex behavioral proof is still pending the Docker matrix.** Both fixes are unit-tested against an in-memory/stub writer and a real (non-codex) child process respectively — see [[Ctx Supervisors]] — but neither has been verified against a real, running codex-cli TUI reading the two-phase write or the reminder text, which is where the original paste-burst/CR-fold behavior was only ever *observed*, not reproduced in an automated test. Follow the machine's own Docker-matrix recipe (CLAUDE.md's "Windows dev machine" notes) before relying on either fix in a live codex pane.

## A dashboard-spawned worker pane's report-back mail used to broadcast, not address, and could be claimed by the wrong pane

**Resolved 2026-08-19 (uncommitted, extends PR #29, issue #30).** A dashboard-spawned worker pane (codex especially — its `register_turn_signal` legitimately returns an empty `env`, since it has no turn-signal mechanism at all) launched without `ZIRV_CTX_SESSION` set, so its own `zirv ctx send` recorded `from_session = "unknown"` and, more importantly, could not `--to-session`-address its reply back at the requester even though `compose_worker_prompt`'s `with_report_back_layer` told it to. The reply landed as *broadcast* mail (no `to_session`) instead — deliverable to any session matching the recipient agent — so a later worker-pane mail sweep or spawn-time harvest running on a **different** pane could claim the reply first, archiving it into `read/` invisibly to the pane whose result it actually was, with nothing in `zirv ctx inbox` ever showing it to the intended recipient.

Fixed by making `build_turn_env` set the session-identity env var **unconditionally** for every spawned pane (skipped only when a turn-signal-capable adapter's own `setup.env` already set it, so no duplicate), so a report-back send is always directed regardless of which adapter's pane sent it. Also added as a review-visibility measure for the underlying trust gap: every mail consumption that happens *on a session's behalf* rather than through that session's own `zirv ctx inbox` call (`exec`/`loop`'s launch-prompt drain, the dashboard's spawn-time drain, its mail sweep, its mail-overlay `Consume`) now logs a `mail-consumed` decision-log entry naming the file and the claimant — see [[Ctx Subsystem]]'s mail section and [[Ctx Supervisors]]'s "Injection semantics". Broadcast-harvest semantics for mail genuinely addressed to `"any"` worker of an agent are deliberately unchanged; this fix removed only the *accidental* broadcast source, not the intentional one.

## `Drop for SignalServer` on Windows removes only the marker file, not the pipe listener

Discovered 2026-08-23 while adding `signal::probe` for issue #99's orphaned-turn-signal-endpoint sweep (`sessions::sweep_orphan_endpoints`). `SignalServer::bind` (Windows) spawns an acceptor thread and a drainer thread that own the actual named-pipe instance directly; `Drop for SignalServer` only does `std::fs::remove_file(&self.path)` — the marker file `zirv ctx status` lists a session from — and never signals either background thread to stop. Because `spawn_acceptor`'s loop reposts a fresh `ConnectNamedPipe` on every completed connect and never terminates on its own, the pipe object named `\\.\pipe\zirv-ctx-<short>` keeps answering a client `CreateFileW` for the rest of *that process's* life, independent of whether the `SignalServer` Rust value has been dropped.

This does not affect the production scenario `signal::probe` exists for: a genuinely dead *process* has every handle it held, named pipes included, released unconditionally by Windows at exit. It does mean a same-process test that drops a `SignalServer` and immediately re-probes the same path cannot observe "endpoint gone" the way the unix build can (removing the socket *file* is enough there, since a new `connect()` resolves the path first) — `signal.rs`'s own windows test suite documents this on `probe_answers_true_for_a_bound_server_and_false_for_one_never_bound`'s doc comment rather than asserting the false-after-drop case. Not fixed: closing it would need a real shutdown handshake for the acceptor/drainer threads, a bigger change to a core supervisor primitive than issue #99 called for.

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
-- so codex is a selectable, launchable adapter. Direct launches carry
composed context through the official `developer_instructions` config
override; shell-shim launches fail closed and use task-text fallbacks only
(see the entry below on that fallback now being surfaced, issue #85).
`zirv setup` also registers the documented Codex lifecycle hooks.
**Event parsing is no longer empty (issue #86, 2026-08-23):**
`parse_events`/`structural_context` now derive turn boundaries and token
totals from the same rollout JSON this file's own collector reads (see [[Ctx
Adapters]]), so `capabilities().events` is `true` and a codex session gets a
real rot score. Tool calls, tool results, and any compaction boundary still
have no verified rollout shape and are not modeled -- the residual half of
[issue #11](https://github.com/Glubiz/zirv-dynamic-cli/issues/11) (marker
signal and the turn-signal socket mechanism remain unverified/absent too).
See [[Ctx Adapters]], [[Rot Engine]].

## Codex's distiller/reviewer sandbox residual is now surfaced, and closes itself on a newer codex-cli (issue #89, 2026-08-23)

`CodexAdapter::distiller_cmd` (and, via `AgentAdapter::read_only_args`, the
workflow reviewer too) pins `--sandbox read-only`, codex's analogue of
claude's `--disallowedTools` pin -- but unlike claude's, which is the *whole*
restriction claude needs, codex-cli ships two more flags that close a gap
`--sandbox` does not touch: `--ignore-rules` (skip project/user execpolicy
`.rules` files) and `--ignore-user-config` (skip `$CODEX_HOME/config.toml`).
They are documented on `codex exec --help` for codex-cli 0.146.0 and 0.147.0
(the brew/standalone captures in
`docs/superpowers/notes/2026-07-31-codex-cli-facts.md`), but **not** on
0.105.0, the version `npm install -g @openai/codex` actually publishes.
Passing either flag on an install that does not recognize it would very
likely error as an unrecognized argument, breaking the distiller/reviewer
outright.

`CodexAdapter::read_only_args` now probes the installed binary's own `codex
exec --help` (`ignore_flags_supported`, cached, the same `--help`-probe
shape `ClaudeAdapter::supports_system_prompt_file` already uses rather than
a hardcoded version cutoff -- the real minimum supporting version between
0.105.0 and 0.146.0 was never captured) and adds both flags only when it
finds them documented; it fails closed on any doubt (binary missing,
timeout, only one flag present). When the flags cannot be added,
`CodexAdapter::sandbox_residual_note` names the residual, and
`adapters::announce_sandbox_residual_once` fires a one-time `zirv ▸`
announcement (wired into every production distiller/judgment call site --
`handoff::run_model`'s callers in `handoff.rs`, `memory.rs`,
`memory_optimize.rs`, `optimize.rs` -- and the workflow reviewer's own
`read_only_args_for_agent_name`) so an operator whose judgment/review child
runs on an un-upgraded codex-cli is told, rather than only finding this in a
doc file. On an install where the probe finds both flags, the announcement
stops firing and the operator gets the stronger guarantee automatically.
**Recorded, narrow residual:** the announcement's opt-out only checks the
`--quiet`/`ZIRV_CTX_QUIET` env var, not the full layered `cfg.chrome.events`
(no `CtxConfig`/`repo` path is reliably in hand at every one of these call
sites) -- an operator whose only opt-out is `~/.zirv/ctx.toml`'s `[chrome]
events = false` still sees this one announcement.

**File-form injection investigated and rejected, not just deferred (issue
#89's sibling gap, issue #85).** Closing the residual outright -- rather
than surfacing it -- would need codex to read `developer_instructions` (or
an equivalent) from a zirv-controlled file instead of argv, the way claude's
`--append-system-prompt-file` does. The only candidate on the real installed
CLI (`codex --help`/`codex exec --help`, codex-cli 0.147.0) is `-p,
--profile <NAME>`, which layers `$CODEX_HOME/<name>.config.toml` -- but that
file *must* live inside the operator's own `$CODEX_HOME` (default
`~/.codex/`), a directory this codebase holds read-only everywhere else
(`policy_support`'s own `CONFIG` constant: "codex's own `approval` setting
in `~/.codex/config.toml`, which zirv reads and never rewrites"). Writing a
profile file there is a new trust-boundary widening, not a narrow local
seam, and redirecting `$CODEX_HOME` for the launch instead would also move
`auth.json` (breaking authentication) unless it were copied too -- a
materially bigger, riskier change. Not implemented; recorded here as the
investigated-and-rejected option rather than an oversight.

## `--sandbox read-only` fails outright on a codex-cli install with the Windows sandbox helper missing

On one real machine (codex-cli 0.147.0, the standalone OpenAI installer, `[windows] sandbox = "elevated"` in `~/.codex/config.toml`), `codex exec --sandbox read-only` — the exact flag `CodexAdapter::distiller_cmd` pins for `zirv ctx optimize`/handoff's report-only guarantee (see the entry above) — fails immediately with `windows sandbox: orchestrator_helper_launch_failed ... helper=codex-windows-sandbox-setup.exe ... program not found`, rather than degrading or falling back. `codex exec` with no sandbox flag at all works on the same install. Since `distiller_cmd` always passes `--sandbox read-only` unconditionally, every `zirv ctx optimize`/handoff run that resolves to the codex distiller fails the same way on such an install until either the sandbox helper binary is present or the pin is made conditional on the installed CLI actually supporting it. Not fixed here — recorded so a codex-distiller failure that looks like a zirv bug is checked against this first. **Wider blast radius since 2026-08-21:** the pin now comes from `AgentAdapter::read_only_args`, which the workflow reviewer (`zirv workflow review run --agent codex`, see [[Workflows]]) also applies, so on such an install the reviewer fails for the same reason as the distiller.

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
together, with no way to enable only wheel/button/drag reporting without also
getting `?1003`'s free-running any-motion flood. A probe on a real
Windows Terminal session showed `?1003` emitting a `MouseEventKind::Moved`
event for every pixel of pointer movement -- dozens from one sweep across the
window, with no button ever held -- competing with keystrokes inside the
bounded per-tick input drain. The dashboard writes `?1000h?1002h?1006h` itself
as raw bytes instead (`term::dash_mouse_on_bytes` -- wheel + button reporting,
button-*drag* tracking, SGR coordinates; `?1003` stays off) and resets all
four modes on exit regardless of which were enabled. `?1002` was off too until
2026-08-19 (uncommitted, extends PR #29) — it was excluded for the same
"nothing reads it" reason `?1003` still is, until the dashboard's own
click-drag text selection/OSC-52-copy feature gave its `Drag` events a
consumer (see [[Ctx Supervisors]] "Click-drag text selection and clipboard
copy"). `[dash] mouse` (default true, `REPO_FORBIDDEN`, env override
`ZIRV_CTX_DASH_MOUSE`) is a genuine trade, not a strict improvement: enabling
mouse reporting takes over the terminal's own native click-drag text
selection -- the dashboard's own replacement only covers a pane whose child
does not itself want mouse reporting; a pane that does still has no zirv-side
selection (hold Shift to bypass the terminal's own capture there, the same
trade every real terminal multiplexer makes).

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

**`ESC CR` now also fires on Alt+Enter, not just Shift+Enter (2026-08-19, uncommitted, extends PR #29) — because Windows Terminal itself can turn one into the other.** An empirical ConPTY probe against real claude v2.1.235 found the actual root cause of Shift+Enter *submitting* instead of newlining in a pane on some setups: once an operator's Windows Terminal has claude's own `/terminal-setup` binding installed, WT rewrites Shift+Enter into `ESC CR` itself before zirv ever reads a keystroke, and zirv's own console layer folds that byte pair back into a single `Enter` keydown carrying **ALT**, not SHIFT — which fell straight through to the bare-`\r` submit branch. `encode_key`'s `Enter` arm now checks `modifiers.intersects(SHIFT | ALT)` rather than SHIFT alone; bare Enter and Ctrl+Enter are unaffected. The same probe also established claude treats `\x1b\r` as a newline regardless of chunking and negotiates win32-input-mode (`?9001`), never the kitty protocol — reinforcing the original `ESC CR` choice over CSI-u.

## The `Ctrl+A ?` help overlay clips its tail on a terminal shorter than ~22 rows

`render_dialog`'s height is `lines.len() + 2` (top/bottom border), clamped to
the available area — on a standard 80x24 terminal that area is `24 - 1 = 23`
rows after the header, and the help table's own content (19 lines) fits
exactly. A terminal shorter than that clips the dialog's bottom rows with no
scrolling and no visible truncation marker. Known and accepted, not fixed:
the dashboard's own eligibility floor is 80x20 (`MIN_DASH_COLS`/`MIN_DASH_ROWS`),
so this can only happen on a terminal resized smaller after the dashboard is
already running. A future addition to `HELP_BINDINGS` should re-run
`the_help_overlay_fits_a_standard_24_row_terminal` (`dash/ui.rs`) before
assuming there's still room.

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
(21 as of 2026-08-19; a local skip list wants 24, adding the three that open a pty directly
with `native_pty_system`) hangs on one reference macOS machine (Darwin 25.5.0): the spawned
`zirv ctx wrap` child reaches kernel
exit state `?Es` after its `/exit` and never reaps, so the test blocks forever in
`Child::wait`. **Pre-existing and unrelated to any branch** — A/B-verified 6/6 against
unmodified `main`, both sandboxed and unsandboxed. Killing the parent test binary's specific
pid clears the wedge (never `pkill`/`killall` by name — other real sessions share those
names). Linux CI runs the whole family normally, and it is the authority for them.

Two traps when building that skip list, both hit on 2026-08-19: three *windows-only* tests
inside `#[cfg(windows)] mod win` have their own `spawn_wrap` helper, so a grep for the helper
name over the whole file returns 27 and three of those names do not exist on macOS at all
(`--skip` on a name nothing matches is silently a no-op, and the runner's own "N filtered out"
count is what gives it away); and `cargo test` must be run with stdin closed
(`< /dev/null`), or `commands::ctx::tests::a_rejected_statusline_tee_still_exits_zero` — which
exercises `zirv ctx usage tee`, and so reads stdin to EOF — blocks the whole suite
indefinitely, roughly two thirds of the way through, with no failure output.

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

## Polling is structurally inert on API-key setups (macOS Keychain fixed 2026-08-22)

`poll::anthropic_token` reads `~/.claude/.credentials.json` first; **as of 2026-08-22 (`fix/dashboard-and-harness-parity`)** it also falls back, on `#[cfg(target_os = "macos")]` builds only, to macOS Keychain (`security find-generic-password -s "Claude Code-credentials" -w`) when that file is absent — Claude Code on macOS is keychain-only, so the file never existed there, which meant the active poll (the only claude usage source for an operator who has not wired the statusline tee) was structurally dead on every macOS machine while working on Windows/Linux; this was the root cause of "the header/bar shows no usage data on macOS" (see [[Usage and Pacing]]'s `poll.rs` section). Unverified against a real macOS host — no Mac was available to confirm the `security` service name or output shape; reasoned from the CLI's own documented behavior and cross-checked externally. An operator authenticating via API key / Bedrock still has no OAuth token anywhere, keychain included, so the active poll remains structurally inert for that setup on every platform. Combined with `has_no_usage_source` being a plain no-data check, such a machine with no statusline tee gets no usage-based pacing at all — the gate announces `pacing off: anthropic has no usage source` once per run (that one-time announcement is the signal; estimator-based pacing, if configured, still applies as of the 2026-08-17 fix round). The remedy on such machines is wiring the statusline tee (`zirv ctx usage tee`), which needs no credentials -- `zirv setup` wires it automatically for a Claude Code install with no pre-existing `statusLine`, and (issue #93, 2026-08-23) now also offers to wrap an *existing* custom statusLine (`zirv ctx usage tee -- <existing command>`) at an interactive `apply`, rather than silently skipping that operator.

**The keychain read itself is bounded and announced (2026-08-22 follow-up).** `security find-generic-password` reading an item zirv is not in the ACL of (Claude Code created it) can pop a GUI authorization dialog; there is no documented `security` flag to suppress it, so `anthropic_token_from_keychain` spawns it non-blocking and polls `try_wait` against a `KEYCHAIN_TIMEOUT_SECS` (3s, matching `HTTP_CONNECT_TIMEOUT_SECS`) deadline, killing and abandoning the child past it rather than hanging a headless/SSH session with nobody to answer. A one-time-per-process `zirv ▸` announcement (`Event::MacosKeychainPromptExpected`, gated on `cfg.chrome.events` like every other announcement) fires before the first attempt, naming the service and that "Always Allow" makes it a one-time cost. Structurally unreachable from `wrap`'s status-bar redraw path (`HttpPoller` is never constructed there). `zirv ctx status` also now explains *why* a provider has no usage source (`poll::usage_source_hint`) rather than just saying so — file absent, macOS Keychain access needed, or the tee never wired — each with the concrete next step.

## Pace's hard-park path can admit a rolled-over reading `window::available` would already hide

`pace::binding()` deliberately keeps a *stale* collector reading binding — skipping the normal `collector_max_age_secs` freshness check — when it was last seen at or above `max_percent` and its `resets_at` hasn't passed yet, since a window cannot free up before its own reset (see [[Usage and Pacing]]'s `pace.rs` section). This is intentional and test-pinned, not a bug. It is, however, a genuinely different staleness rule from the 2026-08-18 display filter, `window::available`: a reading young enough to still be `binding` for pacing purposes can simultaneously be old enough, or have a `resets_at` close enough, to render as a blank usage segment on the header/bar the moment `available` would drop it for a different reason (e.g. the same slot rolling over between the pacing check and the next redraw). An operator can therefore see a session correctly parked with no visible usage percentage explaining why. `zirv ctx usage` (no subcommand) is the surface that shows the raw, unfiltered reading behind a park.

## `zirv ctx usage` prints a bare epoch for an already-passed `resets_at`

`usage::report`'s `line_for` formats a known `resets_at` as `"resets at unix <n>"` regardless of whether that instant is already in the past — unlike every display surface `window::available` now filters (dash header, `wrap`'s bar, `zirv ctx status`), which read a passed `resets_at` as `unknown`. This is deliberate (the verb is meant to show the raw data pacing is deciding from, see [[Usage and Pacing]]'s gotchas), but the wording itself is not: an operator reading `report`'s output has to notice the epoch is in the past themselves, with no "(already reset)" annotation to flag it.

## The pacing gate did not cover interactive sessions at all (T8 finding)

**Resolved 2026-08-22 (T10, `fix/dashboard-and-harness-parity`).** `pace::wait_for_window` used to be called only from `exec.rs` and `run_loop.rs` (`zirv ctx exec`/`loop`, and by extension `zirv ctx agent`) — `wrap.rs` (standalone `zirv ctx wrap`, and `zirv ctx chat`'s orchestrator) and every `dash` pane never consulted the gate at all, before launch or at any point during the session, only the reactive `scan_for_limit`/`is_limit_hit` output-scan catching a limit after the vendor had already rejected it. Fixed via `pace::resolve_interactive_gate`/`InteractiveGate` (a one-shot `Launch`/`Pause`/`Refuse` mapping from the same `PaceDecision`), applied blockingly by `wrap::run_with`'s pre-spawn path (both standalone and `chat`'s orchestrator fallback) and the dashboard's own first-pane launch, gated on both stdin and stdout being real terminals; a dashboard **worker** pane spawned from the live event loop cannot block on a keypress, so it resolves the same gate non-interactively instead (soft band spawns anyway with a notice, hard ceiling refuses via `SpawnRefusal::policy`). See [[Usage and Pacing]] and [[Decision Log]].
