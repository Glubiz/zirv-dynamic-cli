---
last-verified: 2026-08-12
---

# Work Journal

## How to use

- **Reading:** check the last 2–3 entries at the start of a session for recent context.
- **Writing:** entry after any non-trivial change (feature, refactor, bug fix, infra). Skip if a commit message already captures it.
- **Cap:** keep new entries to ~10 lines. If you need more, it's a spec or a [[Decision Log]] entry, not a journal note. Link out; don't inline.
- **Rotation:** when the active journal grows past ~10 entries, move the oldest ones to a quarterly file under `journal-archive/` (frontmatter `archived: true`, header stating the covered date range).

## Format

### YYYY-MM-DD: short title
**What:** one or two sentences.
**Key changes:** files/services touched.
**Follow-up:** anything unfinished (optional).

## Entries

### 2026-08-12: `zirv chat`/`zirv agent`, bare-zirv alias, and `status`'s chat/mail lines
**What:** Top-level routing for the "just run `zirv`" wave: bare `zirv` aliases to `zirv ctx chat` (repo has a `.zirv` dir, stdin is a tty) or `zirv help` otherwise, exiting 0 either way instead of clap's old usage-error exit 2. `zirv chat`/`zirv agent` are further top-level aliases for `zirv ctx chat`/`zirv ctx agent`, reserved so a script can never shadow them. `zirv ctx status` gained a `chat:` line (the adapter `chat` would launch and the rule that picked it, or why nothing qualifies) and a `mail: N unread` line, both degrading rather than failing the rest of the command.
**Key changes:** src/main.rs (`top_level_ctx_alias`, `rewrite_ctx_alias_args`, `bare_invocation_target`, `zirv_dir_present`), src/utils.rs (`RESERVED_COMMANDS` +chat/+agent), src/commands/help.rs, src/commands/ctx/status.rs (`describe_chat`), README, CLAUDE.md, vault pages. Landed alongside (not touched by this wave): `chat.rs`/`agent.rs`/`mail.rs` verbs, `chrome.rs`/`announce.rs` terminal chrome.
**Follow-up:** none for this wave; `announce.rs`'s event channel was still a placeholder at the time these docs were written — see its own module doc.

### 2026-08-12: Agent enable/disable gate (.zirv/.settings.toml)
**What:** New zirv-wide settings file toggling the claude/codex harnesses, enforced in `adapters::select` before `ready()`. Repo layer can only narrow; env is operator authority. Malformed repo file falls back to an operator-only/deny-all gate.
**Key changes:** src/settings.rs (new), adapters/mod.rs + 10 call sites, utils/help/input reserved-name guards, ctx status, README, vault pages. PR #18.
**Follow-up:** harness roadmap (session registry, mailbox, codex completion) awaits prioritization — see [[Decision Log]] and PR #18 description.

### 2026-08-12: Obsidian vault created
**What:** Set up the docs/obsidian vault (23 notes: Architecture, Modules, Concepts, Development) mirroring the zirv-fitness setup, plus Claude Code wiring: CLAUDE.md vault contract with doc-update trigger table, vault-keeper agent, doc-coverage push hook, staleness checker.
**Key changes:** docs/obsidian/**, CLAUDE.md, .claude/settings.json, .claude/agents/vault-keeper.md, scripts/check-doc-*.sh, .gitignore.
**Follow-up:** none.
