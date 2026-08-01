# zirv ctx: optimize and run

Date: 2026-08-01
Status: Approved; ships in the same 2.5.0 release/PR as the context-management work (Jonathan's call, 2026-08-01)
Target release: zirv 2.5.0 (PR #12)

## Feature 1: `zirv ctx optimize`

Analyzes the agent-configuration surfaces that steer every session and reports redundancy, contradictions, and recurring failure patterns, with concrete proposed edits. Report-only: it never modifies CLAUDE.md, settings, or any other analyzed file.

### Inputs analyzed

- CLAUDE.md hierarchy: global (`~/CLAUDE.md` / `~/.claude/CLAUDE.md`), repo root, and nested directory files, as the agent actually layers them.
- `settings.json` layers (user, project, local): hooks, permissions, env.
- Recent session transcripts (bounded sample) and the zirv ctx decision log: repeated tool errors, repeated user corrections, canary/rot events.
- The repo's own review history where cheap to obtain (recurring findings noted in commit messages or PR text are out of scope for v1; transcripts and decision log are the evidence base).

### Findings it hunts

- Redundancy: instructions stated in more than one layer or repeated within a file.
- Contradictions: instructions that conflict between layers or within a file (e.g. differing commit-message rules), including hook behavior contradicting written instructions.
- Dead references: instructions naming files, commands, or flags that no longer exist.
- Evidence-backed friction: instruction gaps correlated with repeated tool failures or corrections found in transcripts and the decision log.

### Output

A markdown report (stdout and a copy under the state dir): one section per finding with severity, evidence (file:line or transcript refs), and a concrete proposed rewrite as a unified diff the user can apply by hand or with `git apply`. Every run appends a decision-log entry. Exit 0 always; findings do not fail the command.

### Triggering

- v1: manual `zirv ctx optimize` only.
- Autonomous path (same release, low-risk): the existing Stop-hook already scores sessions; when a finished session shows evidence thresholds (tool-failure rate, corrections), the hook appends an "optimize recommended" decision-log entry and the statusline advisory mentions it once. The hook never runs the analysis itself (model calls are too heavy for a hook); it queues the recommendation. A scheduled/loop-driven run can be added later without design changes.

### Analysis engine

The linting checks (redundancy across layers, dead references) are deterministic Rust. The judgment checks (contradiction detection, rewrite proposals) run one fresh headless model call via the existing adapter distiller mechanism, with a versioned prompt, bounded input (excerpted files, not whole transcripts), and the same fake-model test pattern as handoff distillation. No network in tests.

## Feature 2: consistent-session system prompt + simple run

When an agent is started through zirv (`zirv ctx wrap`, `zirv ctx exec`, `zirv ctx loop`, `zirv ctx resume`), zirv injects a system prompt so sessions behave consistently every time. A "simple run" starts the agent with no zirv-injected instructions at all.

### Content layering (mirrors ctx.toml)

1. Shipped default: a small, versioned prompt baked into the binary (consistency rules: respect repo conventions, deterministic tool habits, honest failure reporting). Kept minimal; it is a floor, not a policy engine.
2. `~/.zirv/system-prompt.md`: user override/extension.
3. `<repo>/.zirv/system-prompt.md`: repo extension. Subject to the same trust boundary as ctx.toml: the repo layer may add instructions but the mechanism documents that repo-provided prompt text is untrusted input to the session.
4. Layers concatenate in that order with clear separators; later layers never silently replace earlier ones.

### Injection mechanism (verify-first, A9-style)

- claude: `--append-system-prompt` is the expected vehicle for both interactive and `-p` runs; verified against the real CLI before the adapter encodes it. If interactive injection proves unsupported, fall back to prepending an initial-context message and record the fact.
- codex: no verified per-run system-prompt flag is known. A verification task probes the real CLI (config `-c` keys, AGENTS.md layering) and the adapter encodes only verified facts; if blocked, codex ships without injection and the capability matrix says so.

### Simple run

- `--simple` flag on `wrap`, `exec`, `loop`, `resume`: skips ALL zirv prompt injection (shipped default and user/repo layers). Everything else (supervision, pacing, hooks) still applies.
- The injected-or-not state is recorded in the decision log per session start, so transcripts are attributable.

## Non-goals

- optimize never applies edits, even behind a flag, in this release.
- No new daemon, no scheduler inside zirv; autonomous optimize triggering stays hook-recommendation-only.
- The shipped system prompt does not attempt agent-specific behavior tuning beyond consistency basics.

## Open items for the plan phase

- Verify claude `--append-system-prompt` interactive behavior and codex injection surface against the installed CLIs (record in notes files, BLOCKED lines where applicable).
- Decide the exact shipped-default prompt text with Jonathan during plan review.
- Versioning: stays 2.5.0 (unreleased branch, one minor bump for the whole release).
