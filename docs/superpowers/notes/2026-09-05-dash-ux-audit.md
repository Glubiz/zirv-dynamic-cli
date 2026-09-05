# Dashboard UX audit and target design — issue #354

Date: 2026-09-05. Checkout: `D:/GitHub/zirv-ux`, branch `feat/354-dash-ux-audit`, HEAD `894b323e0cf6581c4e8ae936e326c2a91a5cb261`.

**Approval status: proposed, not approved.** This is an investigation and design report. No production Rust changed. The existing test-only capture helper was retained and extended. No version/dependency changes, commits, process termination, or live harness launches were performed. Issue #351's tabs, tiling and worktree-based workspace switching remain out of scope: one focused terminal grid plus sidebar stays.

## A. Current UI audit

### Operator priorities — resolve these first

1. **F01, P0: sidebar width is fixed.** `src/commands/ctx/config.rs:1194` defaults to 24; `src/commands/ctx/dash/mod.rs:6405` captures that value once; `src/commands/ctx/dash/ui.rs:499` only clamps it to terminal width. At 80 columns it occupies 30%; at 200 it occupies 12%. The extra width benefits the child and full-width chrome, but not the roster or aggregate. **Direction:** auto width `clamp(round(cols * 0.22), 20, 44)`, explicit positive column override, with a minimum-grid guard. PR1 delivers this.
2. **F02, P0: sidebar clicks do nothing.** `src/commands/ctx/dash/mod.rs:7949` decodes button presses/releases, then requires containment in `effective_main` before forwarding them. There is no sidebar hit test or focus action. A left press anywhere also clears the previous text selection at `:7990`. **Direction:** pure geometry-based chrome hit testing, routing an attached row click through the same select/focus transition as keyboard navigation. View-only rows select without stealing the existing grid. PR1 delivers this without changing child mouse encoding.
3. **Keep the information and identity.** This is a layout and interaction change, not a theme replacement. Retain #202/#209's cyan brand chip, semantic state colours, braille working spinner, rounded opaque dialogs, flat rules/divider, uniform selected-row reversal, rot score and both usage windows. Preserve every datum in an explicit surface, even when it moves into details at narrow widths. Harness-owned cells and styles remain verbatim.

### Evidence and limits

Read the vault entry point, Ctx Supervisors' dashboard and chrome sections, relevant Active Work entries (#202, #349 and #358), Known Issues' mouse/overlay/help/ownership entries, relevant Decision Log context, and the 2026-08-13 dashboard design. The issue #354 requirements above were supplied with the task; no remote issue lookup was necessary. The current code takes precedence over historical prose. In particular, the original design's prefix configuration, waiting-input glyph, last-output preview and general external-session sidebar are not all current contracts.

The captures below are **real ratatui TestBackend output from current production render functions with synthetic deterministic facts and vt100 content**. They are not hand-drawn reconstructions or screenshots of a live Claude/Codex process. They validate actual cell content and dimensions; they do not demonstrate colour perception, native clipboard integration, PTY latency or real terminal emoji behaviour.

Findings: **2 P0, 11 P1, 4 P2 = 17**, counted once by F-number. P0 addresses the operator's two blockers; P1 is an observable usability defect; P2 is polish or an edge-case limitation. F01/F02 are above; all remaining findings follow.

| ID / severity | Evidence (`src/commands/ctx/` unless stated) | Today and operator impact | Concrete fix direction |
|---|---|---|---|
| F03 / P1 | `dash/ui.rs:259`; `dash/mod.rs:653`, `:691`, `:6345`, `:7576`; `attention.rs:231`, `:415` | Rows contain only Working/Idle/Dead/Unknown. The loop records composed lifecycle observations, and navigation calls `mark_seen_io`, but `DiskFacts` has no SessionStatus map and the row has no reason/visibility fields. Approval, quota, failure and a completed unread turn can look like ordinary activity/idle. | Cache authoritative SessionStatus per stable session ID, project it once, show reason and done-unread, roll up explicit work-group membership. Do not infer approval from terminal text. |
| F04 / P1 | `dash/mod.rs:1488`, `:1530`, `:6938`, `:7044`; `attention.rs:415` | Ended panes are removed before row assembly, stale registry ghosts are excluded, and empty dashboards normally quit before drawing. The Dead renderer does not provide a persistent failed-work list. Every exit, including code 0, goes through `push_error`. `project(Exited)` is Failed; the quiet heuristic maps any Ended code to Exited, but reaping normally removes it before that later sync. | Preserve bounded completion/evidence records independently of PTY ownership. Distinguish clean completion, failure and operator stop using actual outcome evidence. Keep done-unread until viewed. Do not retain dead processes merely to retain rows. Decide the final-pane exit policy explicitly (proposal below). |
| F05 / P1 | `dash/ui.rs:169`, `:196`; `dash/mod.rs:1197`, `:8258`, `:8290` | A long aggregate string is drawn in one 24-column sidebar row; captures show `workers 3 running · 1 fa`, hiding spend, five-hour usage, provider pool and seat generation at all three sizes. `workers_running` actually uses total live rows, including idle/orchestrator/view-only; the failed/cost read walks the delegation ledger without a repo/session filter in this calculation. First enabled harness supplies five-hour usage. The apparent scope is misleading. | Compact summary with labelled scope and a dashboard inspector exposing every original value/provenance. Label total as live sessions; label historical ledger failures/cost separately from current actionable failures. Name the usage provider. Do not silently reinterpret old totals as current-group totals. |
| F06 / P1 | `dash/mod.rs:7086`, `:7882`, `:7891`, `:7955` | Only the key arm checks overlays. Wheel events anywhere on the frame scroll/forward to the focused child, even over the sidebar/header/footer or an open dialog. Buttons over a dialog located inside main reach the hidden child if it wants mouse. This violates modal ownership and can operate a harness control beneath a dialog. | Overlay hit layer takes all pointer events first; outside-modal clicks are consumed. With no overlay, sidebar wheel scrolls roster; grid events continue down the current path. Add zero-child-writes modal regression tests. |
| F07 / P1 | `dash/ui.rs:1834`, `:1899`, `:1947`, `:2034` | List dialogs build all rows into a Paragraph starting at row 0. Cursor affects style only; no viewport offset exists. Footer is appended after all data and clips too. At 80x20 the selected restore row 18 and `restore checked` are absent, confirmed by capture assertions. A hidden selection can still be toggled/confirmed by the reducer. | Shared bounded list viewport, cursor-follow scrolling, independent pinned footer, visible range/count and keyboard Page/Home/End. Hit-test only rendered rows. |
| F08 / P1 | `dash/ui.rs:2188`, `:2435`, `:2529`; `dash/mod.rs:7493` | Help is a static grouped list, no query/cursor/scroll. Any key closes it, including arrows that a reader might expect to scroll. At 80x20 the visible help ends near zoom, omitting select-mode/quit/no-prefix/dialog guidance and its own close hint. Existing tests explicitly accept clipping on 24 rows; the older Known Issues claim that this happens only below eligibility is stale. | Searchable help/palette sharing action descriptors and the fixed list viewport; Esc closes, arrows navigate, Enter invokes. Preserve all binding and dialog guidance in searchable entries. |
| F09 / P1 | `dash/ui.rs:1052`; `dash/mod.rs:70`, `:147`, `:6938` | Dead footer says `^A r restore`; DashAction/filter_key have no Restore action or `r` binding. Restore is an offered startup overlay. Moreover normal final-pane exit bypasses this footer. The advertised recovery cannot work as shown. | Add an actual restore action using eligible saved candidates and checks, or remove the hint until that action ships. Link every rendered hint to a tested action. Do not present renderer-only recovery as a live-loop feature. |
| F10 / P1 | `dash/ui.rs:1408`; `dash/mod.rs:777`, `:828`, `:7686`, `:7727` | Selection is reversal, focus bold, view-only dim. Their difference is weak when bold/dim are subtle, and text captures lose it entirely. Nudge targets selected; handover targets focused. `follow_focus` relies on attached rows being the first `pane_count` entries. Sorting without changing this assumption could send typing to the wrong session. | Explicit textual focus/selection markers and target labels. Use stable row/session identities before attention sorting. Menus capture their target ID and revalidate availability; never use a stale display index as a pane index. |
| F11 / P1 | `dash/mod.rs:888`, `:1000`, `:1260`, `:8226`; `dash/ui.rs:863` | Focused footer correctly reuses its row score, harness usage, own mail and stall latch, but gives no dashboard-wide attention or unread indication. Missing/disabled mail becomes 0 in assembly, then the same dash as known zero. Workflow is the dashboard repo's singleton, not a per-pane workflow despite its position beside pane facts. | Retain focused signals, label repo workflow scope; add global actionable/unread totals outside focused data. Preserve unknown/disabled versus known zero. Count distinct unread envelopes separately from recipient deliveries to avoid broadcast multiplication. |
| F12 / P1 | `dash/mod.rs:1802`, `:2063`, `:2079`, `:2414`, `:6345`; `dash/ui.rs:690` | Errors retain only the latest five entries with no TTL or dedupe; notices retain five, last fresh notice lasts 4 seconds. Rendering prioritizes any error over all notices. A normal exit permanently hides subsequent selection/scroll advice; repeated errors evict distinct ones. Attention writes themselves emit no transition notification. | Keep distinct error events with repeat counts and a separate dismissible acknowledgement state; preserve action feedback while errors exist. Notify only meaningful background attention transitions, deduped by semantic episode, not every poll/scroll/spinner. |
| F13 / P1 | `dash/ui.rs:1721`, `:1755`, `:1950`; `dash/mod.rs:8352` | Draft text and lists are clipped inside fixed-height, non-scrolling Paragraphs. The live grid caret is suppressed for all overlays and no draft renderer sets a replacement caret. Long/multiline input can extend beyond the visible area while still accepting keystrokes. | Draft viewport follows editing caret by display cells, explicit input focus, pinned confirm/back hints, scrollable body/detail text. Keep multiline Shift+Enter behaviour and show the platform limitation. |
| F14 / P2 | `src/style.rs:94`, `:174`; `dash/ui.rs:1265`, `:1513` | Shared width sums Unicode scalar widths; CJK is two cells and combining marks zero, but emoji ZWJ/flag clusters can be overcounted. Truncation avoids dangling ZWJ; it is not a full grapheme-width model. Rust `short:<8` pads scalar count, though real session shorts are ASCII. No CJK grid continuation off-by-one was found: render_grid skips the continuation cell explicitly. | Use the shared helpers consistently and test long Unicode labels at the row/dialog boundaries; define grapheme-safe clipping/alignment before exposing arbitrary titles in fixed columns. Do not claim current helper tests establish native emoji fidelity. |
| F15 / P2 | `dash/mod.rs:134`, `:179`; `dash/ui.rs:626`, `:2188` | Unprefixed keys go to the child; unknown keys after Ctrl+A are silently swallowed, and no prefix-armed marker is rendered. Only errors/help are permanently advertised by header; alive footer is signals, not the issue's presumed dense global action line. Operators cannot tell a pending prefix from ordinary typing. | Small transient `Ctrl+A: ...` hint strip when armed; invalid command feedback without forwarding the mistyped suffix. First-run dismissible guidance; context actions and help/palette preserve unprefixed child input. |
| F16 / P2 | `dash/ui.rs:469`, `:499`, `:2146`; `dash/mod.rs:2563`, `:2581` | Four chrome rows at the requested sizes; under four rows, header/footer consume space before decoration, leaving no body. Width at/below configured sidebar leaves zero main width; PTYs are clamped to at least 1x1. Zoom removes all chrome, including SELECT and escape hints. Empty renderer gives a blank grid. These are guarded, not confirmed panics. | Minimum-grid collapse policy, explicit tiny-frame message with Esc/back where possible, persistent accessible zoom exit via keyboard. Empty state with spawn/restore actions only if lifecycle policy permits it. Preserve the full-frame zoom contract; no permanent extra row in zoom. |
| F17 / P2 | `dash/ui.rs:923`; `dash/mod.rs:949`; `dash/pane.rs:1237`; `src/style.rs:294` | `supervised` is socket reachability from spawn, not proof of all adapter capabilities or ongoing loop health. Stalled replaces the entire supervision segment, hiding whether the socket bound. Semantic colours are basic palette tokens, but selected glyph colours are intentionally suppressed. | Keep stall and supervision as separate facts in inspector/status; describe supervision precisely, do not invent health for view-only rows. Keep glyph+text meaning and uniform reversal in limited-colour terminals. |

### Layout measured against the three sizes

| Frame | Header / top rule / body / bottom rule / footer | Sidebar / divider / main width | Roster rows after aggregate | Sidebar share |
|---|---|---|---|---|
| 80x20 | 1 / 1 / 16 / 1 / 1 | 24 / 1 / 55 | 15 | 30% |
| 120x40 | 1 / 1 / 36 / 1 / 1 | 24 / 1 / 95 | 35 | 20% |
| 200x50 | 1 / 1 / 46 / 1 / 1 | 24 / 1 / 175 | 45 | 12% |

`chrome_rows` safely yields header only at height 1, header+footer at 2, then top rule at 3, both rules at 4; the body becomes positive at 5. Its comment's claim that a one-row frame has all-zero chrome is imprecise: code reserves one header row. A nonzero Rect origin is added to coordinates; area subtraction is saturated. No zero-size panic was established.

For short `a0000001`, harness `claude`, score 12, age `1m`, `sidebar_row_parts` has these exact thresholds: width >=24 shows all fields; 21–23 loses age; 19–20 retains only rot glyph; 17–18 loses rot too; below 17 hard-truncates short/harness; width 1 only state glyph; width 0 empty. A longer age or harness raises thresholds. This is actual branch order, including the full-score/no-age tier omitted in one nearby comment. `rot_text` gives unknown/dead a placeholder regardless of cached score. Sidebar overflow does keep the selected row visible using `sidebar_offset`, but has no independent scroll position or visible range indicator. Wider main/footer/help regions genuinely gain columns; sidebar detail does not.

Resize is handled by Event::Resize and terminal-size reconciliation using the same effective_main calculation; each attached PTY/parser is resized and a selection whose grid size changed is cancelled. The proposed width policy must use the current frame in every one of these seams. Do not recompute auto width only in rendering: that would break coordinates and PTY size parity.

### Facts inventory — present is not the same as displayed

| Source and exact fields | Current use/display | Target destination and availability rule |
|---|---|---|
| `PaneRowMeta` (`dash/mod.rs:653`): short, harness, RowState, supervised | All passed to SidebarRow; supervision only reaches focused footer. | Keep, add immutable row identity/type and composed status; do not duplicate score derivation. |
| `SidebarRow` (`dash/ui.rs:259`): short, harness, age_secs, score, state, attached, selected, focused, supervised | State glyph, short, harness, rot, age; selected reversal, focused bold, external dim. No title, role, model, reason or group. | Compact attention row, selected details and inspector. Short remains accessible even if a friendly title is used in details. |
| `FactsCache` / `DiskFacts` (`dash/mod.rs:998`, `:1087`): registry, refresh instant, scores, dashboard mail, memory_count, usage, workflow, mail_by_session, stalled, spend, pool_harnesses, pool_seat | Throttled once/second. Score/sidebar, usage/mail/workflow/stall/footer, clipped aggregate; dashboard mail and memory count no longer in header. | Retain refresh age/source in inspector. No filesystem or network work in pure renderers or hit testing. |
| `HarnessUsage` (`dash/ui.rs:50`): name, five_hour, seven_day, credits | Both windows for focused harness, credits currently unread. Pool has raw headroom separately. | Keep provider usage, reset/unknown semantics; credits labelled as billing mode, not fabricated headroom. Never call provider usage a per-session token budget. |
| Registry `Record` (`sessions.rs:308`): session UUID/short, agent, repo/repo_slug, verb, pid, started_at, reachable, owner_pid, safety_policy_sha256, role, start_time, in_flight (verb/turn/since) | Identity/age/ownership/liveness used; most other facts not rendered. Own live records only; foreign/unowned/stale sessions excluded. `repo` is shared dashboard identity, not child cwd. | Inspector includes role, ownership, in-flight operation and reachability; retain current scope. Do not pretend a registry record contains model, worktree or parent/group fields. |
| `Pane` (`dash/pane.rs:700`, `:789`, `:809`, `:884`, accessors `:1519` onward): title, role, verb, session, cwd/owns_cwd, report_to, report reminder, work_group_id, parent_session, budget_tokens, result schema, runtime state/grid/scrollback and channel capability | Used for launch, mail, delegation, budget enforcement and teardown; title appears in some dialogs/errors; not normal sidebar. | Identity/role/group/cwd in details; budget ceiling and provenance from existing enforcement snapshots. Child grid remains separate. Internal handles, channel tokens and prompt/argv are not user-facing facts. |
| `SpawnRequest` (`dash/spawnreq.rs:80`): agent, prompt, cwd, requested_by, model, interactive, role, parent_session, work_group_id, budget_tokens, force, workdir, mode, owns_workdir, result_schema, envelope/path_scope/no_network/depth | Validated admission inputs. Model becomes launch args; workdir/cwd goes through acceptance; request is not authority. | Retain effective, validated metadata when needed. **There is no current per-row model cache or Pane model accessor.** Requested model is not proof of actual model; unknown/default stays unknown until authoritative launch/transcript metadata exists. Never scrape arbitrary argv/prompt into chrome. |
| `RosterPane` (`dash/roster.rs:38`): agent, session_id, role, short, title, report_to, report_reminder_sent, work_group_id, budget_tokens, interactive, parent_session | Saved continuity/restoration information, not a live row metadata cache. No model/branch/cwd fields in this record. | Restore inspector shows saved facts and eligibility; unavailable restored metadata stays unknown. Any future schema extension is separately tested with old rosters. |
| `SessionStatus` (`attention.rs:231`): lifecycle, attention, visibility, authority, evidence, confidence, last_transition, revision, skipped | Persisted and available through load/explain-status; dashboard writes QuietHeuristic and quota observations, acknowledges focus navigation, but does not render the model. | Canonical projection in row; reason category in row, full evidence/authority/confidence/skipped in inspector; unseen only cleared after actual viewing. Treat evidence as untrusted bounded text. |
| `WorkGroup` (`group.rs:21`): ID, parent, scope, child_limit, token_budget, spent/reserved, deadline, completion_contract, created/closed, admitted_children, sub-orchestrator | Group admission/settlement available; not in DiskFacts or SidebarRow. | Load referenced groups on facts cadence, roll up explicit child IDs. Group expansion is a tree view within the sidebar, not #351 worktree tabs. |
| Per-session usage / model / branch | `pane_transcript_usage` (`dash/mod.rs:1351`) reads a transcript for budget enforcement/accounting; not a free cached row datum. Model pin exists in request; branch requires new validated worktree facts. | Reuse/cache existing measured token snapshots; no transcript rescan or git command per frame. Show unavailable, never infer a branch from a path basename or invent a token balance. |

Attention detail: Lifecycle is Starting/Working/Waiting/Settled/Exited/Unknown. Attention is None/Approval/Question/Permission/Quota/WorkflowGate/WriterConflict/VerificationFailure/Stalled/Unknown. Projection prioritizes non-None Attention, then Exited→Failed, Settled+Unseen→DoneUnread, Settled+Seen→IdleSeen, Starting/Working→Working, Waiting→blocked with unknown reason. `reason()` can prefer evidence, so the UI needs a short category label plus inspectable evidence rather than placing arbitrary evidence directly in a compact row. Visibility has a latch: composition creates Unseen on Working→Settled and otherwise carries it; `mark_seen` is the only clear. Navigation currently calls it for the resulting focused pane even when an arrow moves onto a view-only row or a switch is a no-op. No general render-time viewed acknowledgement exists. Do not erase #349's independent lifecycle/attention/visibility axes when designing the row.

### Current mouse ownership, exhaustively by region

| Region | Wheel up/down | Down/up (left, middle, right) | Drag / other |
|---|---|---|---|
| Grid, child wants mouse | `Pane::scroll_wheel` emits child-protocol wheel, pane-local 1-based clamped coordinates. | `forward_mouse_button` uses child's protocol, only within main. | Child drag is **not forwarded today**. First attempted drag triggers the once-per-dashboard Ctrl+A v hint. Hover and horizontal wheel ignored. |
| Grid, child does not want mouse | Normal-screen history scroll; alternate screen without history returns FullScreen outcome. | Left starts/ends zirv selection; middle/right do nothing in child. | Left drag updates selection; release copies nonempty selection and keeps highlight. Single-cell click is not a copy. |
| Sidebar, aggregate, divider, header, footer/rules | Still routed to focused pane, with out-of-grid coordinates clamped. | Not forwarded; no chrome action. Left press clears selection. | No resizing/row drag/menu. A child-mouse drag may still cause the hint because that arm is not region gated. |
| Overlay over main | Same child wheel path. | Same child button path underneath dialog. No clickable dialog rows/hints. | Underlying grid selection path may engage. Keys, in contrast, are modal. |
| Mouse disabled or Ctrl+A v native select mode | Host normally emits no mouse events to zirv. | Native terminal owns pointer. | Keyboard navigation remains the fallback. No promise of clickable chrome while reporting is off. |

`term::dash_mouse_on_bytes` (`term.rs:301`) enables 1000/1002/1006; off (`:328`) disables 1000/1002/1003/1006. Never enable 1003 hover reporting: the documented motion flood competes with input. `Pane::wants_mouse` checks vt100 protocol mode; `forward_mouse_button` and `scroll_wheel` preserve child encoding and use `write_input`, not operator-prompt input accounting. `pane_local_mouse` (`dash/mod.rs:2135`) clamps and converts to 1-based; selection uses separate 0-based cell coordinates. `Selection` (`:2218`) includes pane_short, anchor/end; scroll, output and resize invalidation (`:2240`, `:2266`, `:2279`) prevent stale coordinates being copied. Retain all those protections. This audit found no need to alter their encoding algorithms.

### Keyboard, overlays, header and footer contract

Ctrl+A is hardcoded PREFIX. The matcher accepts both Ctrl+`a` and raw U+0001. Ordinary keys, including Tab/arrows/Page keys, are child input. After prefix: 1–9/Tab switch attached panes; Up/Down select the combined list and focus follows only attached rows; s spawn, n nudge, m mail, M memory, o handover, e errors, z zoom, v native select toggle, q quit, ?/h/H help; PageUp/PageDown/Home/End scroll history; Ctrl+A again sends the literal prefix. No global bare key should be appropriated by the redesign. Every existing DashAction has help/dispatch coverage, but that does not cover the stray restore footer string.

Keyboard-only actions include all chrome actions. Dialog lists generally use Up/Down or j/k, Enter confirmation and Esc cancellation; mail Enter is **read+consume**, restore Space toggles and Enter restores checked, memory r/d/v mutate remembered facts, handover Enter swaps, quit Enter ends owned sessions, errors j/k moves the cursor but the renderer does not scroll. Help's any-key-close is the exception. Compose Escape backs out to its parent. Draft Shift+Enter adds a newline only when distinguishable from Enter in the terminal protocol; preserve this known Windows/keyboard-enhancement limitation. Help documents both prefixed and unprefixed sections plus DIALOG_FOOTERS; some actual renderers repeat literal footer arrays, so future action descriptors should become their common source.

Dialogs normally use main, width `main.width - 4`, centered and height-clamped; border+horizontal padding consume four more content columns. Current content widths are 47/87/167. No max-width cap means a 200-column terminal produces extremely long dialog scan lines. `MIN_OVERLAY_COLS=8`, `MIN_OVERLAY_ROWS=3` trigger full-frame fallback below that main size; Clear makes dialogs opaque. Saturating row counts avoid u16 overflow even on huge lists. These guards are correct but do not solve scrolling, focus or clickability.

Header: brand; launch harness/model (not focused pane model); live/total; SELECT; error count/latest error, else last fresh notice; right-aligned errors/help hints. The actual header truncates the variable harness/model first to reserve chip/count/SELECT/hints, then budgets message space; at extremely small widths Paragraph clips even reserved pieces. With a very long launch model, all flexible error space can disappear. Preserve full launch identity in inspector. Notices lose to errors despite an outdated HeaderFacts comment saying the opposite.

Alive footer tier order: full harness/verdict+score/5h+7d/mail/workflow/supervision; drop usage; drop score digits; compress workflow; drop harness; finally drop workflow. Verdict word, mail and supervision survive until final hard clipping. Dead footer drops harness/workflow but retains exit age and the currently invalid restore hint. None draws nothing. Stalled overrides the supervision segment. `scroll_marker` is ASCII `SCROLL -N`, safe to use char count here, placed over the grid top-right and absent if too narrow; zero history gives no marker. Grid rendering preserves VT colours/modifiers, guards bounds, skips wide continuation cells, maps the live cursor only without overlay/history, and highlights only the selected range.

## B. Real current-renderer captures

Reproduce: `cargo build -j 8`, then `cargo test -j 8 --bin zirv dash::ui::tests -- --test-threads=1`. Both ran to completion in the foreground. Build exit 0: `Finished dev profile [unoptimized + debuginfo] target(s) in 5.29s`. Tests exit 0: `test result: ok. 86 passed; 0 failed; 0 ignored; 0 measured; 4501 filtered out; finished in 0.16s`.

Helper: `src/commands/ctx/dash/ui.rs:2573`, `capture_current_dashboard_for_354_audit`; output `target/dash-ux-current-captures.md`. It uses production layout/header/rules/aggregate/sidebar/divider/footer/grid/overlay functions, tick 0, sidebar 24, thresholds 40/70, and asserts each serialized row's display width. There are seven scenarios per size (21 total): three normal panes; nine attached pane fixtures including one dead plus one view-only row (10 rows total); zoom; help; restore with 18 entries and cursor on final entry; empty; dead footer. Synthetic aggregate ledger includes one historical failure even in the normal fixture, which need not imply a current dead pane.

**Reachability:** the dead row is injected to exercise the renderer; normal runtime reaps it before drawing. Empty and dead-footer frames are renderer-level boundary fixtures; normal empty-loop exit means they are not durable runtime screens. Restore is instantiated over synthetic panes for rendering, not a claim that its startup lifecycle was replayed. Text does not carry selected reversal, bold, colour or cursor visibility. Neither captures nor the passing UI tests constitute a live mouse/PTY performance test. The helper's assertions deliberately document the current clipping defects; implementation PRs should replace them with the corrected behavioural expectations.

### CURRENT 80x20 — normal

```text
 zirv  claude (opus) · 3/3 live                           ^A e errors  ^A ? help
────────────────────────┬───────────────────────────────────────────────────────
workers 3 running · 1 fa│Harness terminal (synthetic audit fixture)             
⠋ a0000001 claude ✻12 1m│                                                       
⠋ a0000002 codex ✻21  2m│Task: review dashboard interaction                     
● a0000003 claude ✻30 3m│Reading source files...                                
                        │                                                       
                        │>                                                      
                        │                                                       
                        │                                                       
                        │                                                       
                        │                                                       
                        │                                                       
                        │                                                       
                        │                                                       
                        │                                                       
                        │                                                       
                        │                                                       
────────────────────────┴───────────────────────────────────────────────────────
claude   ✻ fresh 12   ◔ 61%·18%   ✉ 3   ▸ feature · design   ● supervised       
```

### CURRENT 80x20 — nine-panes

```text
 zirv  claude (opus) · 9/10 live                          ^A e errors  ^A ? help
────────────────────────┬───────────────────────────────────────────────────────
workers 9 running · 1 fa│Harness terminal (synthetic audit fixture)             
⠋ a0000001 claude ✻12 1m│                                                       
⠋ a0000002 codex ✻21  2m│Task: review dashboard interaction                     
● a0000003 claude ✻30 3m│Reading source files...                                
⠋ a0000004 codex ✻39  4m│                                                       
⠋ a0000005 claude ✻48 5m│>                                                      
⠋ a0000006 codex ✻57  6m│                                                       
⠋ a0000007 claude ✻66 7m│                                                       
✗ a0000008 codex –    8m│                                                       
⠋ a0000009 claude ✻84 9m│                                                       
· b0000010 codex –    1m│                                                       
                        │                                                       
                        │                                                       
                        │                                                       
                        │                                                       
                        │                                                       
────────────────────────┴───────────────────────────────────────────────────────
claude   ✻ fresh 12   ◔ 61%·18%   ✉ 3   ▸ feature · design   ● supervised       
```

### CURRENT 80x20 — zoomed

```text
Harness terminal (synthetic audit fixture)                                      
                                                                                
Task: review dashboard interaction                                              
Reading source files...                                                         
                                                                                
>                                                                               
                                                                                
                                                                                
                                                                                
                                                                                
                                                                                
                                                                                
                                                                                
                                                                                
                                                                                
                                                                                
                                                                                
                                                                                
                                                                                
                                                                                
```

### CURRENT 80x20 — help

```text
 zirv  claude (opus) · 3/3 live                           ^A e errors  ^A ? help
────────────────────────┬───────────────────────────────────────────────────────
workers 3 running · 1 fa│Ha╭help─────────────────────────────────────────────╮  
⠋ a0000001 claude ✻12 1m│  │ Ctrl+A, then:                                   │  
⠋ a0000002 codex ✻21  2m│Ta│ Ctrl+A             send a literal Ctrl+A        │  
● a0000003 claude ✻30 3m│Re│ Tab                next pane                    │  
                        │  │ Up / Down          select pane                  │  
                        │> │ 1-9                jump to pane                 │  
                        │  │ PageUp / PageDown  scroll                       │  
                        │  │ Home / End         scroll top / live            │  
                        │  │ s                  spawn                        │  
                        │  │ n                  nudge                        │  
                        │  │ m                  mail                         │  
                        │  │ M                  memory                       │  
                        │  │ o                  handover (swap model/harness │  
                        │  │ e                  recent errors                │  
                        │  │ z                  zoom                         │  
                        │  ╰─────────────────────────────────────────────────╯  
────────────────────────┴───────────────────────────────────────────────────────
claude   ✻ fresh 12   ◔ 61%·18%   ✉ 3   ▸ feature · design   ● supervised       
```

### CURRENT 80x20 — restore

```text
 zirv  claude (opus) · 3/3 live                           ^A e errors  ^A ? help
────────────────────────┬───────────────────────────────────────────────────────
workers 3 running · 1 fa│Ha╭restore · 18─────────────────────────────────────╮  
⠋ a0000001 claude ✻12 1m│  │ [x] worker 01 codex resume saved session        │  
⠋ a0000002 codex ✻21  2m│Ta│ [ ] worker 02 codex resume saved session        │  
● a0000003 claude ✻30 3m│Re│ [x] worker 03 codex resume saved session        │  
                        │  │ [x] worker 04 codex resume saved session        │  
                        │> │ [x] worker 05 codex resume saved session        │  
                        │  │ [x] worker 06 codex resume saved session        │  
                        │  │ [x] worker 07 codex resume saved session        │  
                        │  │ [x] worker 08 codex resume saved session        │  
                        │  │ [x] worker 09 codex resume saved session        │  
                        │  │ [x] worker 10 codex resume saved session        │  
                        │  │ [x] worker 11 codex resume saved session        │  
                        │  │ [x] worker 12 codex resume saved session        │  
                        │  │ [x] worker 13 codex resume saved session        │  
                        │  │ [x] worker 14 codex resume saved session        │  
                        │  ╰─────────────────────────────────────────────────╯  
────────────────────────┴───────────────────────────────────────────────────────
claude   ✻ fresh 12   ◔ 61%·18%   ✉ 3   ▸ feature · design   ● supervised       
```

### CURRENT 80x20 — empty

```text
 zirv  claude (opus) · 0/0 live                           ^A e errors  ^A ? help
────────────────────────┬───────────────────────────────────────────────────────
workers 0 running · 1 fa│                                                       
                        │                                                       
                        │                                                       
                        │                                                       
                        │                                                       
                        │                                                       
                        │                                                       
                        │                                                       
                        │                                                       
                        │                                                       
                        │                                                       
                        │                                                       
                        │                                                       
                        │                                                       
                        │                                                       
                        │                                                       
────────────────────────┴───────────────────────────────────────────────────────
                                                                                
```

### CURRENT 80x20 — dead-footer

```text
 zirv  claude (opus) · 0/0 live                           ^A e errors  ^A ? help
────────────────────────┬───────────────────────────────────────────────────────
workers 0 running · 1 fa│                                                       
                        │                                                       
                        │                                                       
                        │                                                       
                        │                                                       
                        │                                                       
                        │                                                       
                        │                                                       
                        │                                                       
                        │                                                       
                        │                                                       
                        │                                                       
                        │                                                       
                        │                                                       
                        │                                                       
                        │                                                       
────────────────────────┴───────────────────────────────────────────────────────
claude   ✗ exited 42s ago   ▸ –   ↺ ^A r restore                                
```

### CURRENT 120x40 — normal

```text
 zirv  claude (opus) · 3/3 live                                                                   ^A e errors  ^A ? help
────────────────────────┬───────────────────────────────────────────────────────────────────────────────────────────────
workers 3 running · 1 fa│Harness terminal (synthetic audit fixture)                                                     
⠋ a0000001 claude ✻12 1m│                                                                                               
⠋ a0000002 codex ✻21  2m│Task: review dashboard interaction                                                             
● a0000003 claude ✻30 3m│Reading source files...                                                                        
                        │                                                                                               
                        │>                                                                                              
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
────────────────────────┴───────────────────────────────────────────────────────────────────────────────────────────────
claude   ✻ fresh 12   ◔ 61%·18%   ✉ 3   ▸ feature · design   ● supervised                                               
```

### CURRENT 120x40 — nine-panes

```text
 zirv  claude (opus) · 9/10 live                                                                  ^A e errors  ^A ? help
────────────────────────┬───────────────────────────────────────────────────────────────────────────────────────────────
workers 9 running · 1 fa│Harness terminal (synthetic audit fixture)                                                     
⠋ a0000001 claude ✻12 1m│                                                                                               
⠋ a0000002 codex ✻21  2m│Task: review dashboard interaction                                                             
● a0000003 claude ✻30 3m│Reading source files...                                                                        
⠋ a0000004 codex ✻39  4m│                                                                                               
⠋ a0000005 claude ✻48 5m│>                                                                                              
⠋ a0000006 codex ✻57  6m│                                                                                               
⠋ a0000007 claude ✻66 7m│                                                                                               
✗ a0000008 codex –    8m│                                                                                               
⠋ a0000009 claude ✻84 9m│                                                                                               
· b0000010 codex –    1m│                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
────────────────────────┴───────────────────────────────────────────────────────────────────────────────────────────────
claude   ✻ fresh 12   ◔ 61%·18%   ✉ 3   ▸ feature · design   ● supervised                                               
```

### CURRENT 120x40 — zoomed

```text
Harness terminal (synthetic audit fixture)                                                                              
                                                                                                                        
Task: review dashboard interaction                                                                                      
Reading source files...                                                                                                 
                                                                                                                        
>                                                                                                                       
                                                                                                                        
                                                                                                                        
                                                                                                                        
                                                                                                                        
                                                                                                                        
                                                                                                                        
                                                                                                                        
                                                                                                                        
                                                                                                                        
                                                                                                                        
                                                                                                                        
                                                                                                                        
                                                                                                                        
                                                                                                                        
                                                                                                                        
                                                                                                                        
                                                                                                                        
                                                                                                                        
                                                                                                                        
                                                                                                                        
                                                                                                                        
                                                                                                                        
                                                                                                                        
                                                                                                                        
                                                                                                                        
                                                                                                                        
                                                                                                                        
                                                                                                                        
                                                                                                                        
                                                                                                                        
                                                                                                                        
                                                                                                                        
                                                                                                                        
                                                                                                                        
```

### CURRENT 120x40 — help

```text
 zirv  claude (opus) · 3/3 live                                                                   ^A e errors  ^A ? help
────────────────────────┬───────────────────────────────────────────────────────────────────────────────────────────────
workers 3 running · 1 fa│Harness terminal (synthetic audit fixture)                                                     
⠋ a0000001 claude ✻12 1m│  ╭help─────────────────────────────────────────────────────────────────────────────────────╮  
⠋ a0000002 codex ✻21  2m│Ta│ Ctrl+A, then:                                                                           │  
● a0000003 claude ✻30 3m│Re│ Ctrl+A             send a literal Ctrl+A                                                │  
                        │  │ Tab                next pane                                                            │  
                        │> │ Up / Down          select pane                                                          │  
                        │  │ 1-9                jump to pane                                                         │  
                        │  │ PageUp / PageDown  scroll                                                               │  
                        │  │ Home / End         scroll top / live                                                    │  
                        │  │ s                  spawn                                                                │  
                        │  │ n                  nudge                                                                │  
                        │  │ m                  mail                                                                 │  
                        │  │ M                  memory                                                               │  
                        │  │ o                  handover (swap model/harness)                                        │  
                        │  │ e                  recent errors                                                        │  
                        │  │ z                  zoom                                                                 │  
                        │  │ v                  toggle text selection                                                │  
                        │  │ q                  quit                                                                 │  
                        │  │ ? / h              this help screen                                                     │  
                        │  │                                                                                         │  
                        │  │ no prefix:                                                                              │  
                        │  │ (mouse wheel)      scroll the focused pane                                              │  
                        │  │                                                                                         │  
                        │  │ Esc closes, Enter confirms                                                              │  
                        │  │                                                                                         │  
                        │  │ dialogs:                                                                                │  
                        │  │ quit     ⏎ quit and shut down esc stay                                                  │  
                        │  │ mail     ⏎ read+consume c compose j/k move esc close                                    │  
                        │  │ memory   r remember d forget v verify esc close                                         │  
                        │  │ restore  space toggle ⏎ restore checked esc skip                                        │  
                        │  │ handover ⏎ swap esc cancel                                                              │  
                        │  │ errors   j/k scroll esc/q close                                                         │  
                        │  │                                                                                         │  
                        │  │ any key close                                                                           │  
                        │  ╰─────────────────────────────────────────────────────────────────────────────────────────╯  
                        │                                                                                               
────────────────────────┴───────────────────────────────────────────────────────────────────────────────────────────────
claude   ✻ fresh 12   ◔ 61%·18%   ✉ 3   ▸ feature · design   ● supervised                                               
```

### CURRENT 120x40 — restore

```text
 zirv  claude (opus) · 3/3 live                                                                   ^A e errors  ^A ? help
────────────────────────┬───────────────────────────────────────────────────────────────────────────────────────────────
workers 3 running · 1 fa│Harness terminal (synthetic audit fixture)                                                     
⠋ a0000001 claude ✻12 1m│                                                                                               
⠋ a0000002 codex ✻21  2m│Task: review dashboard interaction                                                             
● a0000003 claude ✻30 3m│Reading source files...                                                                        
                        │                                                                                               
                        │>                                                                                              
                        │                                                                                               
                        │  ╭restore · 18─────────────────────────────────────────────────────────────────────────────╮  
                        │  │ [x] worker 01 codex resume saved session                                                │  
                        │  │ [ ] worker 02 codex resume saved session                                                │  
                        │  │ [x] worker 03 codex resume saved session                                                │  
                        │  │ [x] worker 04 codex resume saved session                                                │  
                        │  │ [x] worker 05 codex resume saved session                                                │  
                        │  │ [x] worker 06 codex resume saved session                                                │  
                        │  │ [x] worker 07 codex resume saved session                                                │  
                        │  │ [x] worker 08 codex resume saved session                                                │  
                        │  │ [x] worker 09 codex resume saved session                                                │  
                        │  │ [x] worker 10 codex resume saved session                                                │  
                        │  │ [x] worker 11 codex resume saved session                                                │  
                        │  │ [x] worker 12 codex resume saved session                                                │  
                        │  │ [x] worker 13 codex resume saved session                                                │  
                        │  │ [x] worker 14 codex resume saved session                                                │  
                        │  │ [x] worker 15 codex resume saved session                                                │  
                        │  │ [x] worker 16 codex resume saved session                                                │  
                        │  │ [x] worker 17 codex resume saved session                                                │  
                        │  │ [x] worker 18 codex resume saved session                                                │  
                        │  │                                                                                         │  
                        │  │ space toggle   ⏎ restore checked   esc skip                                             │  
                        │  ╰─────────────────────────────────────────────────────────────────────────────────────────╯  
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
────────────────────────┴───────────────────────────────────────────────────────────────────────────────────────────────
claude   ✻ fresh 12   ◔ 61%·18%   ✉ 3   ▸ feature · design   ● supervised                                               
```

### CURRENT 120x40 — empty

```text
 zirv  claude (opus) · 0/0 live                                                                   ^A e errors  ^A ? help
────────────────────────┬───────────────────────────────────────────────────────────────────────────────────────────────
workers 0 running · 1 fa│                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
────────────────────────┴───────────────────────────────────────────────────────────────────────────────────────────────
                                                                                                                        
```

### CURRENT 120x40 — dead-footer

```text
 zirv  claude (opus) · 0/0 live                                                                   ^A e errors  ^A ? help
────────────────────────┬───────────────────────────────────────────────────────────────────────────────────────────────
workers 0 running · 1 fa│                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
                        │                                                                                               
────────────────────────┴───────────────────────────────────────────────────────────────────────────────────────────────
claude   ✗ exited 42s ago   ▸ –   ↺ ^A r restore                                                                        
```

### CURRENT 200x50 — normal

```text
 zirv  claude (opus) · 3/3 live                                                                                                                                                   ^A e errors  ^A ? help
────────────────────────┬───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────
workers 3 running · 1 fa│Harness terminal (synthetic audit fixture)                                                                                                                                     
⠋ a0000001 claude ✻12 1m│                                                                                                                                                                               
⠋ a0000002 codex ✻21  2m│Task: review dashboard interaction                                                                                                                                             
● a0000003 claude ✻30 3m│Reading source files...                                                                                                                                                        
                        │                                                                                                                                                                               
                        │>                                                                                                                                                                              
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
────────────────────────┴───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────
claude   ✻ fresh 12   ◔ 61%·18%   ✉ 3   ▸ feature · design   ● supervised                                                                                                                               
```

### CURRENT 200x50 — nine-panes

```text
 zirv  claude (opus) · 9/10 live                                                                                                                                                  ^A e errors  ^A ? help
────────────────────────┬───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────
workers 9 running · 1 fa│Harness terminal (synthetic audit fixture)                                                                                                                                     
⠋ a0000001 claude ✻12 1m│                                                                                                                                                                               
⠋ a0000002 codex ✻21  2m│Task: review dashboard interaction                                                                                                                                             
● a0000003 claude ✻30 3m│Reading source files...                                                                                                                                                        
⠋ a0000004 codex ✻39  4m│                                                                                                                                                                               
⠋ a0000005 claude ✻48 5m│>                                                                                                                                                                              
⠋ a0000006 codex ✻57  6m│                                                                                                                                                                               
⠋ a0000007 claude ✻66 7m│                                                                                                                                                                               
✗ a0000008 codex –    8m│                                                                                                                                                                               
⠋ a0000009 claude ✻84 9m│                                                                                                                                                                               
· b0000010 codex –    1m│                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
────────────────────────┴───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────
claude   ✻ fresh 12   ◔ 61%·18%   ✉ 3   ▸ feature · design   ● supervised                                                                                                                               
```

### CURRENT 200x50 — zoomed

```text
Harness terminal (synthetic audit fixture)                                                                                                                                                              
                                                                                                                                                                                                        
Task: review dashboard interaction                                                                                                                                                                      
Reading source files...                                                                                                                                                                                 
                                                                                                                                                                                                        
>                                                                                                                                                                                                       
                                                                                                                                                                                                        
                                                                                                                                                                                                        
                                                                                                                                                                                                        
                                                                                                                                                                                                        
                                                                                                                                                                                                        
                                                                                                                                                                                                        
                                                                                                                                                                                                        
                                                                                                                                                                                                        
                                                                                                                                                                                                        
                                                                                                                                                                                                        
                                                                                                                                                                                                        
                                                                                                                                                                                                        
                                                                                                                                                                                                        
                                                                                                                                                                                                        
                                                                                                                                                                                                        
                                                                                                                                                                                                        
                                                                                                                                                                                                        
                                                                                                                                                                                                        
                                                                                                                                                                                                        
                                                                                                                                                                                                        
                                                                                                                                                                                                        
                                                                                                                                                                                                        
                                                                                                                                                                                                        
                                                                                                                                                                                                        
                                                                                                                                                                                                        
                                                                                                                                                                                                        
                                                                                                                                                                                                        
                                                                                                                                                                                                        
                                                                                                                                                                                                        
                                                                                                                                                                                                        
                                                                                                                                                                                                        
                                                                                                                                                                                                        
                                                                                                                                                                                                        
                                                                                                                                                                                                        
                                                                                                                                                                                                        
                                                                                                                                                                                                        
                                                                                                                                                                                                        
                                                                                                                                                                                                        
                                                                                                                                                                                                        
                                                                                                                                                                                                        
                                                                                                                                                                                                        
                                                                                                                                                                                                        
                                                                                                                                                                                                        
                                                                                                                                                                                                        
```

### CURRENT 200x50 — help

```text
 zirv  claude (opus) · 3/3 live                                                                                                                                                   ^A e errors  ^A ? help
────────────────────────┬───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────
workers 3 running · 1 fa│Harness terminal (synthetic audit fixture)                                                                                                                                     
⠋ a0000001 claude ✻12 1m│                                                                                                                                                                               
⠋ a0000002 codex ✻21  2m│Task: review dashboard interaction                                                                                                                                             
● a0000003 claude ✻30 3m│Reading source files...                                                                                                                                                        
                        │                                                                                                                                                                               
                        │>                                                                                                                                                                              
                        │  ╭help─────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────╮  
                        │  │ Ctrl+A, then:                                                                                                                                                           │  
                        │  │ Ctrl+A             send a literal Ctrl+A                                                                                                                                │  
                        │  │ Tab                next pane                                                                                                                                            │  
                        │  │ Up / Down          select pane                                                                                                                                          │  
                        │  │ 1-9                jump to pane                                                                                                                                         │  
                        │  │ PageUp / PageDown  scroll                                                                                                                                               │  
                        │  │ Home / End         scroll top / live                                                                                                                                    │  
                        │  │ s                  spawn                                                                                                                                                │  
                        │  │ n                  nudge                                                                                                                                                │  
                        │  │ m                  mail                                                                                                                                                 │  
                        │  │ M                  memory                                                                                                                                               │  
                        │  │ o                  handover (swap model/harness)                                                                                                                        │  
                        │  │ e                  recent errors                                                                                                                                        │  
                        │  │ z                  zoom                                                                                                                                                 │  
                        │  │ v                  toggle text selection                                                                                                                                │  
                        │  │ q                  quit                                                                                                                                                 │  
                        │  │ ? / h              this help screen                                                                                                                                     │  
                        │  │                                                                                                                                                                         │  
                        │  │ no prefix:                                                                                                                                                              │  
                        │  │ (mouse wheel)      scroll the focused pane                                                                                                                              │  
                        │  │                                                                                                                                                                         │  
                        │  │ Esc closes, Enter confirms                                                                                                                                              │  
                        │  │                                                                                                                                                                         │  
                        │  │ dialogs:                                                                                                                                                                │  
                        │  │ quit     ⏎ quit and shut down esc stay                                                                                                                                  │  
                        │  │ mail     ⏎ read+consume c compose j/k move esc close                                                                                                                    │  
                        │  │ memory   r remember d forget v verify esc close                                                                                                                         │  
                        │  │ restore  space toggle ⏎ restore checked esc skip                                                                                                                        │  
                        │  │ handover ⏎ swap esc cancel                                                                                                                                              │  
                        │  │ errors   j/k scroll esc/q close                                                                                                                                         │  
                        │  │                                                                                                                                                                         │  
                        │  │ any key close                                                                                                                                                           │  
                        │  ╰─────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────╯  
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
────────────────────────┴───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────
claude   ✻ fresh 12   ◔ 61%·18%   ✉ 3   ▸ feature · design   ● supervised                                                                                                                               
```

### CURRENT 200x50 — restore

```text
 zirv  claude (opus) · 3/3 live                                                                                                                                                   ^A e errors  ^A ? help
────────────────────────┬───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────
workers 3 running · 1 fa│Harness terminal (synthetic audit fixture)                                                                                                                                     
⠋ a0000001 claude ✻12 1m│                                                                                                                                                                               
⠋ a0000002 codex ✻21  2m│Task: review dashboard interaction                                                                                                                                             
● a0000003 claude ✻30 3m│Reading source files...                                                                                                                                                        
                        │                                                                                                                                                                               
                        │>                                                                                                                                                                              
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │  ╭restore · 18─────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────╮  
                        │  │ [x] worker 01 codex resume saved session                                                                                                                                │  
                        │  │ [ ] worker 02 codex resume saved session                                                                                                                                │  
                        │  │ [x] worker 03 codex resume saved session                                                                                                                                │  
                        │  │ [x] worker 04 codex resume saved session                                                                                                                                │  
                        │  │ [x] worker 05 codex resume saved session                                                                                                                                │  
                        │  │ [x] worker 06 codex resume saved session                                                                                                                                │  
                        │  │ [x] worker 07 codex resume saved session                                                                                                                                │  
                        │  │ [x] worker 08 codex resume saved session                                                                                                                                │  
                        │  │ [x] worker 09 codex resume saved session                                                                                                                                │  
                        │  │ [x] worker 10 codex resume saved session                                                                                                                                │  
                        │  │ [x] worker 11 codex resume saved session                                                                                                                                │  
                        │  │ [x] worker 12 codex resume saved session                                                                                                                                │  
                        │  │ [x] worker 13 codex resume saved session                                                                                                                                │  
                        │  │ [x] worker 14 codex resume saved session                                                                                                                                │  
                        │  │ [x] worker 15 codex resume saved session                                                                                                                                │  
                        │  │ [x] worker 16 codex resume saved session                                                                                                                                │  
                        │  │ [x] worker 17 codex resume saved session                                                                                                                                │  
                        │  │ [x] worker 18 codex resume saved session                                                                                                                                │  
                        │  │                                                                                                                                                                         │  
                        │  │ space toggle   ⏎ restore checked   esc skip                                                                                                                             │  
                        │  ╰─────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────╯  
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
────────────────────────┴───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────
claude   ✻ fresh 12   ◔ 61%·18%   ✉ 3   ▸ feature · design   ● supervised                                                                                                                               
```

### CURRENT 200x50 — empty

```text
 zirv  claude (opus) · 0/0 live                                                                                                                                                   ^A e errors  ^A ? help
────────────────────────┬───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────
workers 0 running · 1 fa│                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
────────────────────────┴───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────
                                                                                                                                                                                                        
```

### CURRENT 200x50 — dead-footer

```text
 zirv  claude (opus) · 0/0 live                                                                                                                                                   ^A e errors  ^A ? help
────────────────────────┬───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────
workers 0 running · 1 fa│                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
                        │                                                                                                                                                                               
────────────────────────┴───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────
claude   ✗ exited 42s ago   ▸ –   ↺ ^A r restore                                                                                                                                                        
```



## C. Proposed target for operator approval

### Width and information hierarchy

Recommend **auto = clamp(round(terminal columns × 0.22), 20, 44)**. Absent or zero `dash.sidebar_cols` means auto; positive values retain explicit override semantics. Existing operator files with `sidebar_cols=24` remain fixed until changed; users relying on the old implicit default get responsive auto. Zero currently means hidden-width sidebar, so document that migration and use existing zoom for a full-frame grid. Do not silently rewrite operator config. This is a design approval item.

Compute requested width then clamp effective width to `max(0, cols - 1 - min(40, cols))`, retaining at least 40 main columns whenever the frame permits; override requests respect this emergency guard. If the result is less than 20, collapse the sidebar to zero and omit the divider; the grid gets all columns. At 41–60 columns, as at smaller widths, a palette command opens a compact drawer for the row list instead of drawing it in the 0–19 column leftover. Show a transient tiny-width explanation; do not fail the session. Arithmetic saturates, zero dimensions remain valid. Normal auto sizes:

| Frame | Sidebar | Divider | Main | Rationale |
|---|---|---|---|---|
| 80x20 | 20 | 1 | 59 | Recovers four grid columns; short ID + common reason fits. Long reasons use continuation rows. |
| 120x40 | 26 | 1 | 93 | Most reasons and short ID fit; normal rows can regain numeric rot. |
| 200x50 | 44 | 1 | 155 | Adds harness, rot and age when reason fits; keeps ample child width. No endless sidebar expansion. |

22% holds the middle case near the existing sidebar while removing both extreme imbalances. The 20-column floor is a deliberate compact format, not the old row squeezed unchanged. The 44-column cap can show useful facts without competing with the primary grid. This policy alone does not make the old aggregate sentence fit; that is a later independent PR.

Keep one header, two rules and the existing focused signal footer. Use one additional **action footer** at the target sizes, exchanging one body row for discoverability without dropping signal data. Proposed body heights are 15/35/45, main sizes 59x15, 93x35, 155x45. Under height pressure remove the extra action footer first (hints remain in header/palette), then decorative rules, then condensed summary/detail rows; protect at least one main row before optional chrome. Below four total rows show compact escape/help guidance and the remaining grid. PR1 retains current chrome height: this extra action row ships only in the footer PR.

Rows use stable short IDs and glyph+text state. In ASCII mocks, the first marker is `@` selected+focused, `>` selected only, `=` focused only, blank neither. Next is state: `w` working, `!` needs action, `x` failed, `*` done-unread, `o` idle, `?` unknown/view-only. Actual render uses existing spinner/semantic glyphs plus explicit reason words and the same positional focus markers. View-only adds `view only` in disclosure and menu target, with Focus visibly unavailable. A selected row retains uniform reversal and no coloured glyph backgrounds.

At 20 columns: marker+state+8-char short+space+reason, no mandatory metadata columns. `approval`, `question`, `quota`, `working`, `idle` fit. `done-unread`, `permission`, `workflow gate`, `writer conflict`, `verification failure`, `stalled` and unknown/waiting use available width or wrap into indented continuation rows when needed. Never abbreviate away the reason. Continuations hit the same row ID; viewport geometry counts physical lines. At 26, normal working/idle rows add numeric rot when it fits; longer reasons take that space first. At 44, restore harness then age as space permits after reason and numeric rot. Long reasons still outrank metadata.

Expansion order: required identity+reason; numeric rot; harness; age. On shrink reverse the optional order: age, harness, rot digits (retain rot glyph only if useful), then rot; reason wraps, and full short ID is preserved at supported sizes. Extra facts below the selected row are progressive disclosure, not permanently wider columns: title/role/parent/group, effective model, repo workflow phase, measured budget headroom, provider usage, branch/worktree, last transition/evidence, supervision details. At 20 use inspector on Enter; at 26 show a two-line selected detail strip if height allows; at 44 show up to five lines. The inspector always contains all facts; missing model/branch/usage says unknown, not an inferred value. No new disk read on row selection.

One compact summary at narrow width shows actionable count and unseen completions; at wide widths it can also show live count. `^A i`/click summary opens dashboard details: original live-session count, historical failure count and spend with ledger scope, first-provider five-hour usage with provider name, each pool state/headroom and seat generation, global unread envelopes/deliveries and memory count. Thus clipped aggregate data becomes reachable rather than discarded. A failed-work count is distinct from lifetime ledger failures.

### Attention order, groups and viewing

Sort buckets: (1) actionable approval/question/permission/quota/workflow gate/writer conflict/stalled/unknown attention/waiting-unknown; (2) failed, including verification failure; (3) done-unread; (4) starting/working; (5) idle/seen; unknown without attention remains explicitly unknown, ahead of idle. This is a UI priority mapping over #349's projection: VerificationFailure is currently `Blocked(VerificationFailure)`, not `Projection::Failed`; label and bucket it as failure without changing stored semantics. Within a bucket preserve a monotonic first-observed order plus stable ID as tie-break. Do not reorder every frame by age/score.

Group headers roll up the highest-priority member plus counts, including collapsed children; show e.g. `audit: 1 action, 1 failed, 1 new`. Children sort within their group by the same policy. A group belongs in the highest bucket of its descendants, so an urgent child is findable without opening every group. Ungrouped sessions share the same order. Only explicit work_group_id/parent_session links define membership; shared cwd alone does not. Collapse has a visible chevron and never acknowledges children. Collapsing a group with focused child leaves the grid untouched and includes its identity in the group/header; selection moves to the group header. Group and external rows never become a PTY index.

Replace selected/focused indices with stable IDs in the UI projection before sorting. Keep the pane Vec in existing spawn order for lifetime handling and numeric shortcuts. `^A 1..9` still addresses attached pane slots, whose shortcut appears in inspector/palette; arrows traverse visible row IDs and `follow_focus` uses an explicit attached-pane lookup. `^A Tab` cycles attached panes. Background spawns already preserve focus via insert_fixup; retain that contract, including when a new row ranks first.

Prevent moving targets: freeze displayed order for a sidebar navigation/selection interaction, any held pointer button, open menu/overlay or armed prefix. Outside those, commit pending sort only after 750 ms without operator input, retaining the selected row's screen line/scroll anchor; pin its group and selected row position for the next action. Apply updates to reason/count immediately, with `order pending` in details if relevant. A deliberate `Refresh order` palette action can apply it immediately. Typing in the child never changes focus. Sort timers use supplied timestamps in a pure reducer, not a renderer clock.

Done-unread is acknowledged only after the operator actually sees the corresponding live terminal output or completion/evidence detail: require explicit focus/inspect intent and one successful unoccluded render of that ID and status revision at live scroll position. A selection on a group/external row, old scrollback, a failed draw or a no-op numeric switch must not clear some other pane. Completion while already visibly focused can be acknowledged after its new revision is rendered; a modal suppresses that. Retain revision checking so a concurrent new completion cannot be accidentally cleared by an old viewing event. Seeing a group rollup alone does not clear children.

Keep exited worker records, keyed by session and completion revision, independent of Pane/PTY. Retain unseen completion records until viewed; compact seen history into an evidence/history surface with a bounded in-memory window backed by existing durable state. Do not equate every Exited lifecycle to unsuccessful work when actual exit/outcome evidence distinguishes it. Proposal: keep current auto-exit after the final pane in this issue's initial PRs. Persist unread completion evidence for the next dashboard/inspection; implementing an always-open empty dashboard is a separate operator choice, not a prerequisite for PR1. Dead menus/footers therefore operate on retained completed rows while a live pane remains, or saved restore/history overlays. Mock empty actions, if adopted later, must precede removal of auto-exit; never ship an unreachable recovery hint.

### Mouse contract and keyboard parity

Introduce a pure `hit_test(layout, x, y) -> Hit`, where layout is the **same frame snapshot** used to render, including viewport offsets, variable-height row ranges, span widths, zoom state and top overlay geometry. Hit variants: `HeaderHint(ActionId)`, `SidebarSummary`, `GroupToggle(GroupId)`, `SidebarRow(RowId)`, `Divider`, `Grid`, `FooterHint(ActionId)`, `OverlayRow(OverlayId, ItemId)`, `OverlayHint(ActionId)`, `OverlayInput`, `ModalBackdrop`, `None`. Bounds are half-open; zero-size/hidden rectangles yield no hit. Header/footer labels are actionable only over their displayed spans, not the entire line. Rules/spaces are inert.

Dispatch order: select-mode/mouse-disabled guard; modal layer; captured gesture owner; chrome hit; existing grid mouse logic. The dispatcher never calls child writers for a chrome hit. A gesture's Down establishes owner+ID, subsequent Drag/Up remain with that owner until cancellation, preventing a divider drag or a menu-closing release from clicking the child. Recompute geometry on resize, cancel invalid captures/selections, and reject stale row IDs. No new encoding, unwrap/expect, blocking I/O, transcript scan or process launch in hit testing. The PTY pump remains unchanged; supervision failures retain passthrough.

| Gesture | Proposed effect | Keyboard equivalent |
|---|---|---|
| Single left click attached row | Select + focus; reset stale selection; acknowledge only after view rule above. | Existing prefixed arrows/digits/Tab. |
| Click selected/focused row again | Idempotent focus, **no zoom**. Double-click also stays idempotent for initial design, avoiding accidental hides. | `^A z` or menu Zoom is the explicit zoom action. |
| Click view-only/completed row | Select; live grid focus remains. Details identify input destination. | Prefixed arrows, `^A i` inspect. |
| Click group chevron/header | Toggle expansion; never transfer keyboard to a group. | `^A Left/Right` collapse/expand (new); Enter in inspector/palette. |
| Right-click row/group | Open context menu, capture target ID; preserve grid focus until explicit Focus. | `^A c` opens same menu for selected ID (new). |
| Click header/footer hint | Invoke that descriptor once, on matching release. Disabled hint exposes reason. | Its displayed prefixed binding, or palette entry. |
| Divider Down+drag | Capture resize; show candidate width, coalesce PTY resize to one per draw tick, commit on release. Session override, not automatic config write. | `^A [` / `^A ]` decrease/increase 2 columns; `^A \\` reset auto (new, vacant bindings). Esc cancels a captured drag and restores starting width. |
| Wheel over sidebar | Scroll visible roster without focusing/acknowledging rows; keep an explicit viewport independent of selected. | Palette `Scroll roster up/down`; prefixed arrows reveal selection again. |
| Wheel over list/palette/menu | Scroll that overlay; cursor remains visible when moved by keyboard. | Up/Down, PageUp/PageDown, Home/End inside overlay. |
| Click list row | Select only. Restore checkbox toggles only its checkbox; Enter or explicit button confirms. Mail selection previews without consuming. | Arrows; Space checkbox; Enter documented confirm/read+consume. |
| Click outside modal | Consume event; no click-through, no destructive action. Esc goes back. | Esc. |
| Grid pointer | Existing wants_mouse forwarding or non-mouse-pane text selection. Grid right-click remains child's event when requested; no global context-menu interception. | Existing child keys/history keys. |

Select mode (`^A v`) continues to disable reporting and return native text selection; chrome clicks cannot work until re-enabled. Display SELECT plus keyboard-only guidance. With `dash.mouse=false`, give the same keyboard paths and make toggle behaviour honest; do not silently enable capture. Keep the once-per-session mouse-owner hint but allow it to be visible despite prior errors. Preserve child Down/Up and wheel protocol behaviour, no new free hover tracking, and do not advertise child drag forwarding as already implemented.

### Context menu, palette and consistent overlays

Every command descriptor has ID, label, shortcut, applicability predicate, disabled reason and target scope. Keyboard, mouse, help and footer share it. Action effects still go through existing validated reducers/services. Context menu target is displayed by short ID, role and `view only`/ended status. Show unavailable entries dimmed with a reason; never silently substitute the focused pane for a selected external one. The initial menu is a bounded scrollable list, anchored to the clicked row but clamped inside the frame; 80x20 uses a compact centered dialog when it cannot fit beside the row.

| Menu entry | Existing DashAction or new work |
|---|---|
| Inspect status / evidence | **New Inspect/Evidence**; display SessionStatus, outcome, transcript/handoff/report references, workflow verification evidence and cached pane facts. Reads are asynchronous/cached, opening a path is an explicit action. |
| Focus / Zoom | Existing Switch(pane slot)/Zoom effects behind stable-ID resolution; disable Focus for external/completed rows. Zoom explicitly focuses an attached target first. |
| Nudge | Existing Nudge reducer already distinguishes attached/external target. Preserve idle-gated delivery and clear target label. |
| Mail | Existing Mail/compose effects; **new target-prefill context** because current mailbox is dashboard identity and cannot be blindly retargeted. Reading never consumes until explicit read+consume. |
| Handover | Existing Handover picker/effect; adapt to explicit target or focus after explicit invocation. Re-run eligibility; no implicit approval/permission bypass. |
| Stop | **New StopSelected**, not Quit. Existing per-pane shutdown ladder, confirmation naming target and in-flight work; release/report/roster/permit semantics preserved. Never dashboard-wide shutdown for a row command. |
| Restore | **New Restore**, using startup candidate/eligibility/roster routines and full validation. Disabled if no resumable candidate. |
| Open worktree | **New OpenWorktree** with validated effective cwd; external unknown path disabled. Use OS argument-safe path APIs, never interpolated shell commands. |
| Retry | **New Retry**, only where durable task/worker/outcome information supports a validated new attempt. Auth-shaped blocker or missing launch contract disables with reason; do not replay arbitrary saved argv or auto-repeat work on a click. |
| Expand/collapse group | **New ToggleGroup**, pure view action, no new group creation/admission authority. |

Palette opens with new `^A p`; `^A ?`/h/H opens the same searchable surface in help mode. Query searches action name, description, reason/synonyms and key; includes child-input/select-mode explanations and all DIALOG_FOOTERS. Empty query shows context actions then global commands. Enter executes selected enabled command, not an arbitrary shell string; disabled selection shows why. Explicit Help detail rows open explanation instead of executing. Esc from details goes back; Esc from palette closes; query typing is never sent to child. Search zero-results includes Clear query; loading/errors preserve query and selection. Keep keyboard selection visible and announce result count in text. No fuzzy search dependency is necessary initially: case-insensitive token matching is sufficient.

Overlay widths target `min(available - 4, 72)` for menus/palette, up to 96 for inspect/evidence; minimum is clamped to actual frame, never an unchecked `clamp(min,max)`. Pinned title/query/footer are outside the scrolling list; logical item IDs map to rendered rows. Each input field draws a caret and handles Unicode display width, wrapping and pasted multiline drafts. Esc consistently goes back/cancel one layer, Enter explicitly confirms the labelled action; mail read+consume and destructive stop/quit keep their meaning. Do not use any-key-close once help is navigable.

First-run micro-onboarding: nonmodal `Ctrl+A ? help; click a pane; Ctrl+A v selects text` with a clickable Dismiss and palette `Dismiss tips`; one-time flag lives only in operator state. When mouse is off, show keyboard navigation instead of click guidance. It cannot intercept the first typed prompt or reopen after dismissal. Reopen from help. No settings prompt is necessary to use the dashboard.

### Footer, global signals and notifications

Keep focused signal footer grammar and tiering, including verdict/score, both usage windows, per-focused mail, repo workflow and supervision. Add textual `focus <short>` when selected differs, using the action footer/inspector target label at 80 columns. Preserve stalled and socket reachability separately in details; with room both are inline. Global summary/header retains live/total, error access, attention/unseen count and launch harness/model. Memory/ledger/pool/seat stay accessible through dashboard inspector at every width. Label unknown/disabled data; do not substitute zero. A grouped view counts unique sessions for attention, and a global mail total deduplicates envelope IDs while displaying delivery count separately if broadcasts target multiple sessions.

| Context | 80-column action footer priorities | Wider additions |
|---|---|---|
| Alive | `^A c actions`, `^A n nudge`, `^A m mail`, `^A ? help`; prefix label and focused target available. | Inspect, spawn, zoom, memory if room. |
| Needs action | `^A i reason`, `^A c actions`, `^A ? help`; specific approval/question/quota text in row/detail. | Evidence, mail, nudge; never create a generic Approve that bypasses harness/workflow authority. |
| Completed/failed selected | Inspect evidence, Restore/Retry only when actually enabled, Actions, Help. | Exit code/age, worktree access. Existing live focus signal footer remains labelled separately. |
| Overlay | Its own pinned hints own confirmation/back; action footer says dialog name/target and Esc back. | Search result count / selected item range. |
| Empty (only if later approved) | Spawn, eligible restore, help, quit. | Last exit outcome. No dead restore hint without a real action. |

Drop secondary hints before primary contextual action or Help; full palette is always reachable. The action row collapses before signal rows on short frames. Clicking a hint is exactly its named action, not a new shortcut path.

Notification rule: on cached-status transition into action-needed, failure or done-unread, enqueue one compact background notice; suppress while that same pane is visibly focused. No notification for output, poll, spinner, ordinary starting/working/idle changes or repeatedly reading unchanged files. Deduplicate by `(session, semantic attention episode)`; revision/transition timestamp tracks observations, but evidence-only wording changes and Seen acknowledgement do not create a new episode. Clear then re-entry rearms. Group notifications coalesce child changes per facts tick; show highest priority plus affected count, with individual evidence retained. Focused suppression is remembered as handled, so blurring later does not replay the same alert. On initial cache load, populate badges without replaying historical notices. Action acknowledgements such as copied/queued remain local feedback; they are separate from attention notifications. Error visibility must not suppress those messages. No automatic focus, host notification, sound or OS notification permission is needed in this issue.

### Target mock frames

The following **15 proposed ASCII frames** are design illustrations, not TestBackend output. Each is exactly the stated column count on every line and exactly the stated row count, verified by the report generator. Trailing spaces are intentional. Styles follow #202/#209 in implementation; ASCII markers expose semantics without relying on colour. Normal, attention-heavy, forced-narrow stress, context menu and searchable palette appear at each size. Forced-narrow uses a 20-column session override at 120/200 as an explicit collapse stress case; the 80-column case is its natural compact tier. Long reasons continue onto additional physical rows and remain part of the same selectable item.

Selected detail blocks occupy sidebar disclosure lines; mock main terminal text is illustrative harness output. The focused session label belongs in the top rule, never in the child's buffer. The grid itself retains its original VT content. No new panes, tabs or tiles are implied by groups.

### TARGET 80x20 — normal

```text
 zirv claude (opus) | 3/3 live                         [^A e errors] [^A ? help]
--------------------+ harness a0000001 -----------------------------------------
action0 new0 live3  |Claude Code                                                
v audit (3)         |                                                           
@wa0000001 working  |Task: audit the dashboard                                  
 wa0000002 working  |                                                           
 oa0000003 idle     |Reviewing interaction and layout...                        
                    |                                                           
                    |>                                                          
                    |                                                           
                    |                                                           
                    |                                                           
                    |                                                           
                    |                                                           
                    |                                                           
                    |                                                           
                    |                                                           
--------------------+-----------------------------------------------------------
claude *fresh12  5h61%/7d18%  mail3  repo:design  supervised                    
^A c actions  ^A n nudge  ^A m mail  ^A ? help                                  
```

### TARGET 80x20 — attention-heavy

```text
 zirv claude (opus) | 5/5 live                         [^A e errors] [^A ? help]
--------------------+ harness a0000001 -----------------------------------------
!1 fail1 new1       |Claude Code                                                
v audit !1 x1 *1    |                                                           
@!a0000001 approval |Approval requested for the next workflow step.             
 xa0000002          |                                                           
  verification      |Review the evidence before confirming.                     
  failure           |                                                           
 *a0000003          |>                                                          
  done-unread       |                                                           
 wa0000004 working  |                                                           
 oa0000005 idle     |                                                           
                    |                                                           
                    |                                                           
                    |                                                           
                    |                                                           
                    |                                                           
--------------------+-----------------------------------------------------------
claude *fresh12  5h61%/7d18%  mail3  repo:design!  supervised                   
^A i reason  ^A c actions  ^A ? help | focus a0000001                           
```

### TARGET 80x20 — narrow

```text
 zirv claude (opus) | 5/5 live                         [^A e errors] [^A ? help]
--------------------+ harness a0000001 -----------------------------------------
!1 fail1 new1       |Claude Code                                                
v audit !1 x1 *1    |                                                           
@!a0000001 approval |Approval requested for the next workflow step.             
 xa0000002          |                                                           
  verification      |Review the evidence before confirming.                     
  failure           |                                                           
 *a0000003          |>                                                          
  done-unread       |                                                           
 wa0000004 working  |                                                           
 oa0000005 idle     |                                                           
> selected detail   |                                                           
title: long-worktree|                                                           
review-worker-branch|                                                           
^A i full details   |                                                           
                    |                                                           
--------------------+-----------------------------------------------------------
claude *fresh12  5h61%/7d18%  mail3  repo:design!  supervised                   
^A i reason  ^A c actions  ^A ? help | focus a0000001                           
```

### TARGET 80x20 — context-menu

```text
 zirv claude (opus) | 3/3 live                         [^A e errors] [^A ? help]
--------------------+ harness a0000001 -----------------------------------------
action0 new0 live3  |C+ Actions: a0000002 ----------------------------------+   
v audit (3)         | | > Inspect status                                    |   
=wa0000001 working  |T|   Focus                                             |   
>wa0000002 working  | |   Nudge                                             |   
 oa0000003 idle     |R|   Mail to a0000002                                  |   
                    | |   Handover...                                       |   
                    |>|   Stop... (confirm)                                 |   
                    | |   Restore [off: live]                               |   
                    | |   Open worktree                                     |   
                    | |   Evidence                                          |   
                    | |   Retry [off: live]                                 |   
                    | | Up/Down move Enter run Esc back                     |   
                    | +-----------------------------------------------------+   
                    |                                                           
                    |                                                           
--------------------+-----------------------------------------------------------
claude *fresh12  5h61%/7d18%  mail3  repo:design  supervised                    
Menu target a0000002 | input focus a0000001 | Esc back                          
```

### TARGET 80x20 — palette

```text
 zirv claude (opus) | 3/3 live                         [^A e errors] [^A ? help]
--------------------+ harness a0000001 -----------------------------------------
action0 new0 live3  |Claude Code                                                
v au+ zirv commands / help ------------------------------------------------+    
@wa0| Find: mail_                                                          |    
 wa0| 3 results | target: a0000001                                         |    
 oa0|                                                                      |    
    | > Open mailbox                      ^A m                             |    
    |   Compose mail to selected session                                   |    
    |   Help: mail read + consume                                          |    
    |                                                                      |    
    | Enter runs the selected command.                                     |    
    | Esc closes; typing stays in this search.                             |    
    | Up/Down move Enter run Esc close                                     |    
    +----------------------------------------------------------------------+    
                    |                                                           
                    |                                                           
--------------------+-----------------------------------------------------------
claude *fresh12  5h61%/7d18%  mail3  repo:design  supervised                    
Command palette | Esc close | child input paused                                
```

### TARGET 120x40 — normal

```text
 zirv claude (opus) | 3/3 live                                                                 [^A e errors] [^A ? help]
--------------------------+ harness a0000001 ---------------------------------------------------------------------------
action0 new0 live3        |Claude Code                                                                                  
v audit (3)               |                                                                                             
@wa0000001 working r12 1m |Task: audit the dashboard                                                                    
 wa0000002 working r21 2m |                                                                                             
 oa0000003 idle r30 claude|Reviewing interaction and layout...                                                          
--- selected detail ---   |                                                                                             
worker / audit            |>                                                                                            
claude / opus (launch)    |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
--------------------------+---------------------------------------------------------------------------------------------
claude *fresh12  5h61%/7d18%  mail3  repo:design  supervised                                                            
^A c actions  ^A n nudge  ^A m mail  ^A ? help  ^A s spawn  ^A z zoom  ^A M memory                                      
```

### TARGET 120x40 — attention-heavy

```text
 zirv claude (opus) | 5/5 live                                                                 [^A e errors] [^A ? help]
--------------------------+ harness a0000001 ---------------------------------------------------------------------------
!1 fail1 new1             |Claude Code                                                                                  
v audit !1 x1 *1          |                                                                                             
@!a0000001 approval r12 1m|Approval requested for the next workflow step.                                               
 xa0000002                |                                                                                             
  verification failure    |Review the evidence before confirming.                                                       
 *a0000003 done-unread r21|                                                                                             
 wa0000004 working r21 2m |>                                                                                            
 oa0000005 idle r21 codex |                                                                                             
--- selected detail ---   |                                                                                             
role: worker / audit      |                                                                                             
reason: approval          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
--------------------------+---------------------------------------------------------------------------------------------
claude *fresh12  5h61%/7d18%  mail3  repo:design!  supervised                                                           
^A i reason  ^A c actions  ^A ? help | focus a0000001  ^A s spawn  ^A z zoom  ^A M memory                               
```

### TARGET 120x40 — narrow

```text
 zirv claude (opus) | 5/5 live                                                                 [^A e errors] [^A ? help]
--------------------+ harness a0000001 ---------------------------------------------------------------------------------
!1 fail1 new1       |Claude Code                                                                                        
v audit !1 x1 *1    |                                                                                                   
@!a0000001 approval |Approval requested for the next workflow step.                                                     
 xa0000002          |                                                                                                   
  verification      |Review the evidence before confirming.                                                             
  failure           |                                                                                                   
 *a0000003          |>                                                                                                  
  done-unread       |                                                                                                   
 wa0000004 working  |                                                                                                   
 oa0000005 idle     |                                                                                                   
> selected detail   |                                                                                                   
title: long-worktree|                                                                                                   
review-worker-branch|                                                                                                   
^A i full details   |                                                                                                   
                    |                                                                                                   
                    |                                                                                                   
                    |                                                                                                   
                    |                                                                                                   
                    |                                                                                                   
                    |                                                                                                   
                    |                                                                                                   
                    |                                                                                                   
                    |                                                                                                   
                    |                                                                                                   
                    |                                                                                                   
                    |                                                                                                   
                    |                                                                                                   
                    |                                                                                                   
                    |                                                                                                   
                    |                                                                                                   
                    |                                                                                                   
                    |                                                                                                   
                    |                                                                                                   
                    |                                                                                                   
                    |                                                                                                   
--------------------+---------------------------------------------------------------------------------------------------
claude *fresh12  5h61%/7d18%  mail3  repo:design!  supervised                                                           
^A i reason  ^A c actions  ^A ? help | focus a0000001  ^A s spawn  ^A z zoom  ^A M memory                               
```

### TARGET 120x40 — context-menu

```text
 zirv claude (opus) | 3/3 live                                                                 [^A e errors] [^A ? help]
--------------------------+ harness a0000001 ---------------------------------------------------------------------------
action0 new0 live3        |C+ Actions: a0000002 ---------------------------------------+                                
v audit (3)               | | > Inspect status                                         |                                
=wa0000001 working r12 1m |T|   Focus                                                  |                                
>wa0000002 working r21 2m | |   Nudge                                                  |                                
 oa0000003 idle r30 claude|R|   Mail to a0000002                                       |                                
--- selected detail ---   | |   Handover...                                            |                                
worker / audit            |>|   Stop... (confirm)                                      |                                
claude / opus (launch)    | |   Restore [off: live]                                    |                                
                          | |   Open worktree                                          |                                
                          | |   Evidence                                               |                                
                          | |   Retry [off: live]                                      |                                
                          | | Up/Down move Enter run Esc back                          |                                
                          | +----------------------------------------------------------+                                
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
--------------------------+---------------------------------------------------------------------------------------------
claude *fresh12  5h61%/7d18%  mail3  repo:design  supervised                                                            
Menu target a0000002 | input focus a0000001 | Esc back                                                                  
```

### TARGET 120x40 — palette

```text
 zirv claude (opus) | 3/3 live                                                                 [^A e errors] [^A ? help]
--------------------------+ harness a0000001 ---------------------------------------------------------------------------
action0 new0 live3        |Claude Code                                                                                  
v audit (3)             + zirv commands / help ------------------------------------------------+                        
@wa0000001 working r12 1| Find: mail_                                                          |                        
 wa0000002 working r21 2| 3 results | target: a0000001                                         |                        
 oa0000003 idle r30 clau|                                                                      |                        
--- selected detail --- | > Open mailbox                      ^A m                             |                        
worker / audit          |   Compose mail to selected session                                   |                        
claude / opus (launch)  |   Help: mail read + consume                                          |                        
                        |                                                                      |                        
                        | Enter runs the selected command.                                     |                        
                        | Esc closes; typing stays in this search.                             |                        
                        | Up/Down move Enter run Esc close                                     |                        
                        +----------------------------------------------------------------------+                        
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
                          |                                                                                             
--------------------------+---------------------------------------------------------------------------------------------
claude *fresh12  5h61%/7d18%  mail3  repo:design  supervised                                                            
Command palette | Esc close | child input paused                                                                        
```

### TARGET 200x50 — normal

```text
 zirv claude (opus) | 3/3 live                                                                                                                                                 [^A e errors] [^A ? help]
--------------------------------------------+ harness a0000001 -----------------------------------------------------------------------------------------------------------------------------------------
action0 new0 live3                          |Claude Code                                                                                                                                                
v audit (3)                                 |                                                                                                                                                           
@wa0000001 working r12 claude 1m            |Task: audit the dashboard                                                                                                                                  
 wa0000002 working r21 codex 2m             |                                                                                                                                                           
 oa0000003 idle r30 claude 3m               |Reviewing interaction and layout...                                                                                                                        
--- selected detail ---                     |                                                                                                                                                           
worker / audit                              |>                                                                                                                                                          
claude / opus (launch)                      |                                                                                                                                                           
budget: 12k / 80k measured                  |                                                                                                                                                           
branch: feat/354-dash-ux-audit              |                                                                                                                                                           
cwd: D:/GitHub/zirv-ux                      |                                                                                                                                                           
transition: working 1m ago                  |                                                                                                                                                           
socket reachable; ^A i evidence             |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
--------------------------------------------+-----------------------------------------------------------------------------------------------------------------------------------------------------------
claude *fresh12  5h61%/7d18%  mail3  repo:design  supervised                                                                                                                                            
^A c actions  ^A n nudge  ^A m mail  ^A ? help  ^A s spawn  ^A z zoom  ^A M memory                                                                                                                      
```

### TARGET 200x50 — attention-heavy

```text
 zirv claude (opus) | 5/5 live                                                                                                                                                 [^A e errors] [^A ? help]
--------------------------------------------+ harness a0000001 -----------------------------------------------------------------------------------------------------------------------------------------
!1 fail1 new1                               |Claude Code                                                                                                                                                
v audit !1 x1 *1                            |                                                                                                                                                           
@!a0000001 approval r12 claude 1m           |Approval requested for the next workflow step.                                                                                                             
 xa0000002 verification failure r21 codex 2m|                                                                                                                                                           
 *a0000003 done-unread r21 codex 2m         |Review the evidence before confirming.                                                                                                                     
 wa0000004 working r21 codex 2m             |                                                                                                                                                           
 oa0000005 idle r21 codex 2m                |>                                                                                                                                                          
--- selected detail ---                     |                                                                                                                                                           
role: worker / audit                        |                                                                                                                                                           
reason: approval                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
--------------------------------------------+-----------------------------------------------------------------------------------------------------------------------------------------------------------
claude *fresh12  5h61%/7d18%  mail3  repo:design!  supervised                                                                                                                                           
^A i reason  ^A c actions  ^A ? help | focus a0000001  ^A s spawn  ^A z zoom  ^A M memory                                                                                                               
```

### TARGET 200x50 — narrow

```text
 zirv claude (opus) | 5/5 live                                                                                                                                                 [^A e errors] [^A ? help]
--------------------+ harness a0000001 -----------------------------------------------------------------------------------------------------------------------------------------------------------------
!1 fail1 new1       |Claude Code                                                                                                                                                                        
v audit !1 x1 *1    |                                                                                                                                                                                   
@!a0000001 approval |Approval requested for the next workflow step.                                                                                                                                     
 xa0000002          |                                                                                                                                                                                   
  verification      |Review the evidence before confirming.                                                                                                                                             
  failure           |                                                                                                                                                                                   
 *a0000003          |>                                                                                                                                                                                  
  done-unread       |                                                                                                                                                                                   
 wa0000004 working  |                                                                                                                                                                                   
 oa0000005 idle     |                                                                                                                                                                                   
> selected detail   |                                                                                                                                                                                   
title: long-worktree|                                                                                                                                                                                   
review-worker-branch|                                                                                                                                                                                   
^A i full details   |                                                                                                                                                                                   
                    |                                                                                                                                                                                   
                    |                                                                                                                                                                                   
                    |                                                                                                                                                                                   
                    |                                                                                                                                                                                   
                    |                                                                                                                                                                                   
                    |                                                                                                                                                                                   
                    |                                                                                                                                                                                   
                    |                                                                                                                                                                                   
                    |                                                                                                                                                                                   
                    |                                                                                                                                                                                   
                    |                                                                                                                                                                                   
                    |                                                                                                                                                                                   
                    |                                                                                                                                                                                   
                    |                                                                                                                                                                                   
                    |                                                                                                                                                                                   
                    |                                                                                                                                                                                   
                    |                                                                                                                                                                                   
                    |                                                                                                                                                                                   
                    |                                                                                                                                                                                   
                    |                                                                                                                                                                                   
                    |                                                                                                                                                                                   
                    |                                                                                                                                                                                   
                    |                                                                                                                                                                                   
                    |                                                                                                                                                                                   
                    |                                                                                                                                                                                   
                    |                                                                                                                                                                                   
                    |                                                                                                                                                                                   
                    |                                                                                                                                                                                   
                    |                                                                                                                                                                                   
                    |                                                                                                                                                                                   
                    |                                                                                                                                                                                   
--------------------+-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------
claude *fresh12  5h61%/7d18%  mail3  repo:design!  supervised                                                                                                                                           
^A i reason  ^A c actions  ^A ? help | focus a0000001  ^A s spawn  ^A z zoom  ^A M memory                                                                                                               
```

### TARGET 200x50 — context-menu

```text
 zirv claude (opus) | 3/3 live                                                                                                                                                 [^A e errors] [^A ? help]
--------------------------------------------+ harness a0000001 -----------------------------------------------------------------------------------------------------------------------------------------
action0 new0 live3                          |C+ Actions: a0000002 ---------------------------------------+                                                                                              
v audit (3)                                 | | > Inspect status                                         |                                                                                              
=wa0000001 working r12 claude 1m            |T|   Focus                                                  |                                                                                              
>wa0000002 working r21 codex 2m             | |   Nudge                                                  |                                                                                              
 oa0000003 idle r30 claude 3m               |R|   Mail to a0000002                                       |                                                                                              
--- selected detail ---                     | |   Handover...                                            |                                                                                              
worker / audit                              |>|   Stop... (confirm)                                      |                                                                                              
claude / opus (launch)                      | |   Restore [off: live]                                    |                                                                                              
budget: 12k / 80k measured                  | |   Open worktree                                          |                                                                                              
branch: feat/354-dash-ux-audit              | |   Evidence                                               |                                                                                              
cwd: D:/GitHub/zirv-ux                      | |   Retry [off: live]                                      |                                                                                              
transition: working 1m ago                  | | Up/Down move Enter run Esc back                          |                                                                                              
socket reachable; ^A i evidence             | +----------------------------------------------------------+                                                                                              
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
--------------------------------------------+-----------------------------------------------------------------------------------------------------------------------------------------------------------
claude *fresh12  5h61%/7d18%  mail3  repo:design  supervised                                                                                                                                            
Menu target a0000002 | input focus a0000001 | Esc back                                                                                                                                                  
```

### TARGET 200x50 — palette

```text
 zirv claude (opus) | 3/3 live                                                                                                                                                 [^A e errors] [^A ? help]
--------------------------------------------+ harness a0000001 -----------------------------------------------------------------------------------------------------------------------------------------
action0 new0 live3                          |Claude Code                                                                                                                                                
v audit (3)                                 |                   + zirv commands / help ------------------------------------------------+                                                                
@wa0000001 working r12 claude 1m            |Task: audit the das| Find: mail_                                                          |                                                                
 wa0000002 working r21 codex 2m             |                   | 3 results | target: a0000001                                         |                                                                
 oa0000003 idle r30 claude 3m               |Reviewing interacti|                                                                      |                                                                
--- selected detail ---                     |                   | > Open mailbox                      ^A m                             |                                                                
worker / audit                              |>                  |   Compose mail to selected session                                   |                                                                
claude / opus (launch)                      |                   |   Help: mail read + consume                                          |                                                                
budget: 12k / 80k measured                  |                   |                                                                      |                                                                
branch: feat/354-dash-ux-audit              |                   | Enter runs the selected command.                                     |                                                                
cwd: D:/GitHub/zirv-ux                      |                   | Esc closes; typing stays in this search.                             |                                                                
transition: working 1m ago                  |                   | Up/Down move Enter run Esc close                                     |                                                                
socket reachable; ^A i evidence             |                   +----------------------------------------------------------------------+                                                                
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
                                            |                                                                                                                                                           
--------------------------------------------+-----------------------------------------------------------------------------------------------------------------------------------------------------------
claude *fresh12  5h61%/7d18%  mail3  repo:design  supervised                                                                                                                                            
Command palette | Esc close | child input paused                                                                                                                                                        
```


### Acceptance and regression plan

1. **Render matrix:** normal 0/1/3/9 panes, owned external row, group expanded/collapsed, each Lifecycle/Attention/Visibility combination that projects differently, working/failed/done-unread/seen, unknown score/usage/model, mail zero/unknown/disabled/unread, workflow absent/active/gated, stalled+unsupervised, prefix/SELECT/zoom, notices/errors, menu/palette/query-empty/no-results/loading/error, each dialog and long drafts at 80x20, 120x40, 200x50. Verify dimensions, pinned hints, row reasons, current input target and all original data accessible in inspector. Keep a renderer-only dead-footer fixture separate from lifecycle tests.
2. **Pure geometry/reducer tests:** half-open edges of every region, rules/aggregate offset, first/last visible row, multi-line reasons, scrolled sidebar/list, overlay fallback, zero/tiny rectangles, long labels and clipped spans, modal backdrop, disabled controls. Mouse and keyboard sequences converge to same stable selected/focused IDs and effects. Freeze/re-sort timestamps, group rollups, arrival/reap while cursor/menu is open, no-op navigation and revision-safe seen acknowledgement. New background pane never takes focus. Stale item/gesture targets never fall back to another pane.
3. **Mouse regression:** bytes and coordinates for normal/alternate screen × wants_mouse on/off × normal/zoom × mouse enabled/disabled/SELECT; left/middle/right press/release, wheel and child protocol encodings. No child writes from sidebar/header/footer/divider/menu/dialog/backdrop. Drag owner survives boundary crossings; modal opening/resize/focus change cancels safely. Existing normal-pane selection/copy and scroll/output/resize invalidation stay intact. No new hover flood or child drag claim.
4. **Overlay/recovery semantics:** click selects without consumption; explicit read+consume, toggle vs restore-all, stop confirmation, retry refusal, handover target validation, clipboard error, unknown/expired candidates, roster compatibility, final-pane auto-exit. Escape during async work cancels UI interest safely without orphaning subprocesses or applying a completed result to a new target.
5. **Unicode/accessibility/manual terminals:** ASCII, CJK, combining marks, emoji ZWJ/flags, very long model/group/worktree names; selected foreground reversal, non-colour markers, Windows Terminal and a second supported terminal with basic 16-colour/monochrome rendering. Test keyboard-only and native selection (Ctrl+A v), visible editing caret and clear focus/disabled text. UI text must not execute ANSI/OSC from untrusted labels/evidence. Automated display_width checks supplement, not replace, terminal glyph inspection.
6. **Performance:** replay identical PTY fixtures and high-volume output on main and each implementation PR, one and nine panes at all three sizes; record input-to-child and input-to-paint p50/p95/max, drain fairness, throughput, CPU and memory. Include 2,000-character paste, continuous output, sidebar wheel and divider drag with resize storm. No new I/O per frame or per pointer event; cached reads remain bounded/throttled, input cap (4096) and existing pane drain budgets stay. Compare against same-machine baseline; investigate >10% p95 regression or >one-frame added input lag before approval. Do not present unmeasured latency as a passed gate.

Required current-task build/UI tests passed as recorded in B; formatting result is recorded in the verification appendix below. Full PTY/manual/performance/nextest/clippy passes are implementation acceptance work, not claimed here. All future cargo commands on this machine use `-j 8`; before each implementation PR use the repository's five checks with that job limit. No test failure is excused by weakening an assertion; confirmed existing Windows wrap failures require named baseline comparison in the implementation PR.

## D. Phased implementation plan

All PRs below require operator approval of the relevant design before production edits. Sizes: S one compact area, M a bounded cross-file behaviour, L a state/data integration needing multiple verifiable steps. Each PR ships useful behaviour and tests independently, retains single-grid identity and avoids broad refactors. Future PRs follow repository version-bump/validation rules; this report does not change Cargo files.

| PR / size | Independently shippable result | Touched files | Main risk / focused tests |
|---|---|---|---|
| **PR1 — Responsive sidebar and clickable rows / M** | Auto/explicit width policy; shared frame geometry; pure Hit enum; select/focus attached row, select external row; modal pointer shielding. Retain existing row order/format, footer and child forwarding algorithms. Sidebar wheel moves an explicit roster viewport without focusing or reaching the child; keyboard navigation reveals selection again. | `dash/ui.rs`, `dash/mod.rs`, `config.rs`; inline tests; Ctx Supervisors/Untrusted Configuration docs. | Geometry mismatch across resize/zoom; row index offset by aggregate; stale clicks; select mode. Exact 20/26/44 widths, explicit/zero/tiny values, config REPO_FORBIDDEN/env tests, hit boundaries, view-only focus retention, zero modal child writes, existing selection/mouse tests. No pane.rs pump change. |
| **PR2 — Attention rows, stable IDs and group rollups / L** | Cache SessionStatus and referenced groups; stable-ID row projection/navigation; visible reason/done-unread; attention-first stable order; group collapse; retain completed worker evidence while live grid remains. | `dash/mod.rs`, `dash/ui.rs`, narrow read accessors in `dash/pane.rs`, `attention.rs` only if revision-safe seen write seam needs extension; Ctx Supervisors docs. | Sorting corrupts focus, acknowledgement race, false failure after clean exit, memory growth. Pure sort/group/viewed-revision tests, background creation, reap/parent removal, collapsed urgent child, status unavailable, every reason at three sizes. Keep current final-pane exit. Split state plumbing and render commits for review. |
| **PR3 — Scrollable dialogs and context actions / L** | Shared list viewport/pinned hints/caret; modal mouse parity; context menu via right-click/`^A c`; inspect/focus/nudge/mail/handover/stop plus real Restore, eligible worktree/evidence/retry actions. Disabled reasons when unavailable; remove false restore promise until wired. | `dash/ui.rs`, `dash/mod.rs`; reuse `roster.rs`, sessions/worktree/task services with narrow adapters as required. | Wrong destructive target, hidden selection, mail consumption, stale restore/retry metadata, external-path launch. Target-ID/disabled/confirm/scroll/cursor tests; lifecycle and old-roster tests; no arbitrary argv replay. L because it adds actions, not just a popup. |
| **PR4 — Searchable help and palette / M** | Shared action descriptors feed help, menu and `^A p`; all existing bindings and dialog semantics searchable; consistent Esc/back/confirm; dismissible nonmodal onboarding. | `dash/ui.rs`, `dash/mod.rs`, operator-state persistence seam; docs. | Child key theft, query/edit focus, empty results, help drift. Descriptor dispatch coverage, filter/ranking, keyboard/mouse equivalence, result scrolling, Unicode input, dismissed-state persistence; old shortcuts unchanged. |
| **PR5 — Context footer and accessible dashboard totals / M** | Context action row with height tiers; preserve focused signal segments; honest global attention/mail/ledger/pool/seat/memory summary and complete dashboard inspector. Keep scope and unavailable data clear. | `dash/ui.rs`, `dash/mod.rs`, bounded mail-summary helper if needed, Ctx Supervisors docs. | Footer takes too many rows, broadcast double count, focused/selected ambiguity, lifetime failures mislabelled. Per-size tier tests, distinct-envelope vs delivery totals, stalled+unsupervised, unknown/disabled, every former aggregate field reachable. |
| **PR6 — Meaningful notifications and error acknowledgement / M** | Deduped background attention notices, focused suppression, grouped bursts; local feedback visible alongside acknowledged errors with repeat counts. | `dash/mod.rs`, pure notice reducer (inline or small dash module), `dash/ui.rs`; docs. | Alert storms, replay after restart/focus, dropping distinct errors. Clock-driven episode tests, focused/modal/re-entry/evidence-only change, error capacity and acknowledgement, no attention auto-focus. |
| **PR7 — Progressive details and sidebar resize controls / M** | Effective role/model/group/budget/worktree disclosure, sidebar divider drag and keyboard resizing, selected detail strips; cache missing metadata at existing cadence. | `dash/mod.rs`, `dash/ui.rs`, minimal pane/roster metadata accessors only where required; docs. | Per-frame git/transcript I/O, model guessed as actual, resize flood, stale selection/copy. Measured cache cadence, unknown/restored metadata, width degradation, drag ownership/coalescing, Unicode and PTY throughput comparison. |

PR1 resolves both operator complaints before the more complex attention model and action work. It should land without waiting for menus or notification policy. Later PRs progressively expose the existing good information, with no theme-only phase.

### Operator decisions to approve

- Adopt auto 22% (20–44 columns), positive override, zero→auto migration and 40-column emergency grid guard?
- Adopt explicit zoom only (no click-again/double-click zoom) and session-only divider resize with keyboard equivalents?
- Adopt one extra context action footer row at supported heights while retaining the focused signal row?
- Keep current final-pane auto-exit for this issue, retaining completed worker evidence while another pane lives and across saved inspection, or extend scope to a persistent empty dashboard?
- Approve the stable-ID attention/group layout and phased sequence above, including full reasons wrapping at 20 columns?

### Verification appendix

Build and requested 86 UI tests passed (B). Only test-only Rust and this report are intended worktree changes. No production-code, Cargo, live PTY, permission/config or vault contract changes were made. No design implementation is authorized by this report itself.

`cargo fmt -- --check`: exit 0, no output. Final scope check: the entire Rust diff is the 177-line test-only capture helper inside `#[cfg(test)] mod tests`; removing that helper reproduces HEAD exactly. The only other changed path is this report. The report contains 21 current captures and 15 ASCII targets; all 36 heights and all target widths were checked after assembly. Current capture widths were asserted by the Rust helper. This audit did not run clippy, nextest, the full serial suite, live mouse or PTY performance tests; those remain implementation gates, not evidence claimed for this design task.
