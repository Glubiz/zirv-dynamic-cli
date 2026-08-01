# Claude usage-window facts (verified 2026-07-31)

Verified on this macOS machine by read-only inspection plus the official docs (code.claude.com/docs statusline + errors pages). Basis for the spec's "Usage pacing" section and the plan's Phase E tasks.

## Local persisted state
- verified: NONE exists. `~/.claude.json` (top-level keys inspected) has no usage/window fields. No statsig dir. `~/.claude/stats-cache.json` holds historical per-model token totals only, no reset timestamp or percentage. Nothing named usage/quota/rate-limit-state anywhere under `~/.claude/`.

## Statusline input JSON
- verified (docs): fields `rate_limits.five_hour.used_percentage`, `rate_limits.five_hour.resets_at` (unix epoch), `rate_limits.seven_day.used_percentage`, `rate_limits.seven_day.resets_at`.
- verified (docs caveat): appears only for Claude.ai subscribers (Pro/Max) after the first API response in the session; each window may be independently absent.
- verified (local): existing `~/.claude/statusline-command.sh` reads only `context_window.used_percentage`, `model.display_name`, `cwd`. No `rate_limits` capture deployed today; `~/.claude/debug/` contains zero `rate_limits` hits so the fields are documented but not yet empirically captured on this machine.
- verified (docs): statusline command fires on new assistant message, `/compact`, permission-mode change, vim-mode toggle, and a configurable `refreshInterval` timer. Event-driven within a live session only.

## Transcript reconstruction
- verified: every assistant event in `~/.claude/projects/**/*.jsonl` carries `usage` with `input_tokens`, `cache_creation_input_tokens`, `cache_read_input_tokens`, `output_tokens` (sampled 360/360 events in a live main-session file, 35/35 in a subagent file).
- verified: subagent turns live in separate `<session>/subagents/*.jsonl` files (flagged `isSidechain:true`), NOT interleaved in the main session file. A machine-wide sum must walk `~/.claude/projects/**/*.jsonl` including `subagents/` subdirectories.
- BLOCKED: whether the subscription limiter weights these token classes identically (cache discounts etc.) is not documented. Transcript sums are an approximation, never ground truth.

## Limit-hit signature
- verified (docs, exact strings): `You've hit your session limit · resets 3:45pm`, `You've hit your weekly limit · resets Mon 12:00am`, and a model-specific variant (`...your Opus limit · resets 3:45pm`). Session/weekly limits are shared across models.
- BLOCKED: no documented exit code or JSON event shape for `claude -p` under an exhausted window; zero genuine limit events exist in local transcripts (21 grep hits were all unrelated false positives). Cannot be verified empirically without deliberately exhausting a window. Any matcher ships docs-verified with an empirical follow-up.
- FOLLOW-UP (opened with Phase E, task E4): the matcher in `src/commands/ctx/pace.rs`
  (`LIMIT_HIT_PATTERNS`) ships with exactly the three strings documented above and
  nothing else. Two plausible phrasings ("hit your sonnet limit", "hit your usage
  limit") are listed as commented-out candidates in that constant's doc comment
  and are deliberately NOT matched. Confirm empirically the next time a window is
  genuinely exhausted: capture the exact stdout/stderr line and the exit code of
  `claude -p` under an exhausted window, then promote the observed string into the
  list and record the exit code here. Until then a limit hit is detected by output
  text alone, never by exit code.

## Headless query
- verified: NONE exists. `claude --help` full command list has no usage/quota command or flag. The `/usage` TUI screen pulls a remote endpoint with a stale-cache fallback and has no scriptable equivalent.

## codex
- verified: no usage/limit surface. `~/.codex/` has no usage-named file; `codex --help` and `codex features` expose nothing relevant.
