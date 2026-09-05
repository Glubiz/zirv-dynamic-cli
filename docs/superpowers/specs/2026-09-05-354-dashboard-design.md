# Dashboard redesign — approved design (issue #354)

Date: 2026-09-05. Status: **approved by the operator** on the 200×50 reference frame (mock artifact round 3). Supersedes the width-tier proposals in `docs/superpowers/notes/2026-09-05-dash-ux-audit.md` Part C; that note's Part A audit (findings F01–F17) and Part B captures remain the evidence base.

## Operator decisions

1. **The sidebar is fixed at 44 columns at every terminal width.** `dash.sidebar_cols` default changes from 24 to 44; an explicit operator value is still honoured. The narrower row tiers (20/26 columns), the auto-width policy and divider drag-to-resize are scrapped. The existing dash eligibility floor (`MIN_DASH_COLS`×`MIN_DASH_ROWS`) is unchanged.
2. **Chrome stays non-intrusive**: one header row, one top rule, the sidebar with a one-column divider, one bottom rule, one footer row. No second footer row, no borders around the grid, no animation beyond the existing spinner, no popup that steals focus from the child terminal.
3. **Rows keep spawn order and never re-sort by attention.** Attention is expressed by the state glyph only (▲ needs action, ✗ failed, ◆ done-unread, ⠋ working, ● idle, · unknown) plus glyph counts in the summary line and the group header. No reason words in rows.
4. **Work groups render as a tree.** One header per work group (`▾ <scope> · <sub-orchestrator short> · <n> workers`, attention rollup right-aligned); the sub-orchestrator row is the first child; workers hang off `├` and the last off `└`. Ungrouped sessions are flat but keep the same glyph column. Membership comes only from `work_group_id` / `parent_session`, never from a shared cwd.
5. **Disclosure under the selected row**: reason, group and parent, model, budget and usage, branch, writer permit and cwd, time in state, turn-signal state, in aligned key/value lines hanging off the tree's `│`. `^A i` opens the full inspector with evidence.
6. **Mouse and keyboard parity** via a pure hit-test; click a row to select and focus; right-click for the context menu; wheel over the sidebar scrolls the roster; chrome hits never reach the child; overlay takes pointer events first.
7. **Header right cluster is context-sensitive** (the actions that apply to the selected row); the header middle carries one transient notice or the sticky error line as today.
8. **The focus rule names the focused session** (`short · harness model · role [in group]`) on the left and its checkout on the right. The child's buffer is never written to.
9. **Footer keeps today's grammar** (harness, verdict+score, both usage windows, unread mail, repo workflow step, supervision) and gains a dim right-aligned `$<spend> this session · pool <harness> <headroom>%…` segment.

## Reference frames (200×50, approved)

Both frames are exact 200-column, 50-row renders. Colour follows `src/style.rs` tokens: brand chip cyan reversed; ⠋ cyan, ● green, ✗ red, ▲ yellow, ◆ magenta, · dim; rot `✻n` banded green/yellow/red by `score.advise_at`/`compact_at`; the selected row is uniformly REVERSED (glyphs drop their own colour, #209 §B) and bold when it is also the keyboard focus; tree glyphs, keys, ages and roles are dim; disclosure values are normal weight.

### A sub-orchestrator waiting for approval, selected

```text
 zirv claude · fable · 5/5 live   ✉ a0000003 → a0000002 queued until idle                                                                          ^A i inspect   ^A c actions   ^A n nudge   ^A ? help 
────────────────────────────────────────────┬─ a0000002 · codex gpt-6-astra · sub-orch audit ─────────────────────────────────────────────────────────────────────────────────────── D:/GitHub/zirv-ux ─
  5 live                         ▲1  ✗1  ◆1 │Codex · gpt-6-astra · D:/GitHub/zirv-ux · feat/354-dash-ux-audit                                                                                           
  ⠋ a0000001 ✻12 14m orch     fable         │                                                                                                                                                           
▾ audit · a0000002 · 4 workers      ▲1 ✗1 ◆1│› Plan for the width policy is written to docs/superpowers/plans/2026-09-05-354-pr1.md.                                                                    
├ ▲ a0000002 ✻21  9m sub-orch gpt-6-astra   │  Next step is the render matrix. That needs the test suite once, in the foreground.                                                                       
│   reason    approval · workflow gate      │                                                                                                                                                           
│   group     audit · parent a0000001       │  Approval requested                                                                                                                                       
│   model     codex gpt-6-astra             │  Run cargo test -j 8 --bin zirv dash:: -- --test-threads=1 in D:/GitHub/zirv-ux?                                                                          
│   budget    40k / 200k · 5h 84%           │  Estimated 2–4 minutes on this machine. Writes only under target/.                                                                                        
│   branch    feat/354-dash-ux-audit        │                                                                                                                                                           
│   writer    held · D:/GitHub/zirv-ux      │  y approve   n decline   a always for this session                                                                                                        
│   since     waiting 1m · started 9m ago   │                                                                                                                                                           
│   signal    socket bound · ^A i evidence  │  Tool calls 31 · tokens 40.2k of 200k · turn 12                                                                                                           
├ ✗ a0000003 ✻8   7m worker   sonnet        │                                                                                                                                                           
├ ◆ a0000004 ✻30  6m worker   gpt-5.6-terra │────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────── 
└ ● a0000005 ✻4   5m worker   haiku         │› ▌                                                                                                                                                        
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
────────────────────────────────────────────┴───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────
 codex   ✻ fresh 21   ◔ 5h 84% · 7d 55%   ✉ 1   ▸ design · awaits approval   ● supervised                                                              $0.42 this session · pool claude 64% · codex 16% 
```

### Quiet state, a worker selected while it edits

```text
 zirv claude · fable · 4/4 live                                                                                                                       ^A c actions   ^A n nudge   ^A m mail   ^A ? help 
────────────────────────────────────────────┬─ a0000003 · claude sonnet · worker in audit ────────────────────────────────────────────────────────────────────────────────────────── D:/GitHub/zirv-ux ─
  4 live                             ⠋3  ●1 │Claude Code · sonnet · D:/GitHub/zirv-ux                                                                                                                   
  ⠋ a0000001 ✻12 14m orch     fable         │                                                                                                                                                           
▾ audit · a0000002 · 3 workers         ⠋2 ●1│⏺ Read(src/commands/ctx/dash/ui.rs)                                                                                                                        
├ ⠋ a0000002 ✻21  9m sub-orch gpt-6-astra   │  ⎿  Read 4540 lines                                                                                                                                       
├ ⠋ a0000003 ✻8   7m worker   sonnet        │                                                                                                                                                           
│   reason    none · working                │⏺ The sidebar width is fixed in ui::layout; I'm replacing the clamp with the auto policy and adding a                                                      
│   group     audit · parent a0000002       │  minimum-grid guard, then the geometry tests at 80, 120 and 200 columns.                                                                                  
│   model     claude sonnet                 │                                                                                                                                                           
│   budget    12k / 80k · 5h 61%            │⏺ Update(src/commands/ctx/dash/ui.rs)                                                                                                                      
│   branch    feat/354-dash-ux-audit        │  ⎿  Updated src/commands/ctx/dash/ui.rs with 31 additions and 6 removals                                                                                  
│   writer    held · D:/GitHub/zirv-ux      │                                                                                                                                                           
│   since     working 3m · started 7m ago   │⏺ Bash(cargo build -j 8)                                                                                                                                   
│   signal    socket bound · ^A i evidence  │  ⎿  Running…                                                                                                                                              
├ ⠋ a0000004 ✻30  6m worker   gpt-5.6-terra │                                                                                                                                                           
└ ● a0000005 ✻4   5m worker   haiku         │› ▌                                                                                                                                                        
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
────────────────────────────────────────────┴───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────
 claude   ✻ fresh 8   ◔ 5h 61% · 7d 18%   ✉ 0   ▸ design   ● supervised                                                                                $0.42 this session · pool claude 64% · codex 16% 
```

## Sidebar column contract (44 columns)

| Column | Width | Content |
|---|---|---|
| tree | 2 | `  ` for ungrouped rows, `▾ ` group header, `├ ` child, `└ ` last child, `│ ` + 2 spaces for disclosure lines |
| glyph | 1 | state glyph (colour + shape; never colour alone) |
| short | 8 | session short id |
| rot | 3 | `✻` + score left-aligned in 2 (`✻8 `, `✻21`); dead/unknown shows the shared placeholder |
| age | 3 | right-aligned `format_age` (`14m`, ` 9m`, ` 2h`) |
| role | 8 | `orch`, `sub-orch`, `worker`, `review`, … left-aligned |
| model | rest | effective model, or the placeholder when unknown; truncated with `truncate_display` |

Single spaces between columns. Row = `tree(2) glyph(1) sp short(8) sp rot(3) sp age(3) sp role(8) sp model(≤13)` = 44.

Summary line (first sidebar row): `  <n> live` left, glyph counts right-aligned (`▲1  ✗1  ◆1`, or `⠋3  ●1` when nothing needs attention). Group header: `▾ <scope> · <lead short> · <n> workers` left, rollup glyph counts right-aligned; a collapsed group shows `▸` and keeps its rollup. Disclosure line: `│   ` + key padded to 10 + value (≤ 30 display columns).

## Behaviour contract

- **Selection vs focus**: `selected` is the sidebar cursor (REVERSED); `focused` is the pane receiving keystrokes (bold). Clicking an attached row does both; clicking a view-only or ended row selects only and the header/footer say which pane still has input focus.
- **Order**: pane spawn order, grouped under their work group at the position of the group's first member. New background panes never take focus (`insert_fixup` contract kept).
- **Disclosure** is drawn only under the selected row. Under height pressure disclosure lines drop before any session row; the summary line and group headers never drop.
- **Attention source**: the composed `attention::SessionStatus` cached per session on the `FactsCache` cadence (never per frame). Done-unread (`◆`) clears only after the operator actually views that pane (focus + one unoccluded render at live scroll, revision-checked). Completed workers keep a row while another pane lives; the final-pane auto-exit stays.
- **Mouse**: pure `hit_test(layout, x, y) -> Hit { HeaderHint(action), SidebarSummary, GroupToggle(group), SidebarRow(row id), Divider, Grid, FooterHint(action), OverlayRow(..), OverlayHint(..), ModalBackdrop, None }` over the same frame snapshot that was rendered. Dispatch: select-mode/mouse-off guard → modal layer → captured gesture owner → chrome hit → existing grid path. Chrome hits never write to the child; child forwarding, text selection and their invalidation rules are unchanged.
- **Keyboard**: every mouse action has a prefixed key: arrows/Tab/1-9 as today, `^A c` context menu, `^A i` inspector, `^A p` palette, `^A Left/Right` collapse/expand group. Unprefixed keys stay child input.
- **Header cluster** shows at most four `^A x label` hints chosen by the selected row's state (alive: actions, nudge, mail, help; needs action: inspect, actions, nudge, help; ended: inspect, restore, actions, help; overlay: its own back/confirm).
- **Notifications**: one compact notice on a cached-status transition into needs-action, failed or done-unread; suppressed for the focused pane; deduped per (session, attention episode).

## Non-goals

Tabs/tiled panes/worktree groups (#351); attention-first sorting; reason words in rows; a second footer row; sidebar auto-width or resize; hover tracking (`?1003`); child drag forwarding; a persistent empty dashboard.

## Phases

1. Sidebar row format + fixed 44 default + summary line + group tree + disclosure + focus rule label + header context cluster + footer spend segment; pure hit-test; click selects/focuses; sidebar wheel scrolls the roster; overlay pointer shielding.
2. Attention glyphs and rollups from `SessionStatus`; done-unread acknowledgement; completed-worker rows retained; work-group membership and collapse.
3. Scrollable list dialogs with pinned hints and caret; context menu (`^A c` / right-click) with inspect, focus, nudge, mail, handover, stop, restore, open worktree, evidence, retry (disabled with reason when unavailable); real Restore action or hint removed.
4. Searchable palette/help (`^A p`, `^A ?`) fed by one action-descriptor table; consistent Esc/Enter; dismissible first-run tip.
5. Notifications and error acknowledgement.
