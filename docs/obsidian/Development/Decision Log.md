---
last-verified: 2026-08-12
---

# Decision Log

**Entry shape (hard cap ~15 lines):**

```
### YYYY-MM-DD — Short title
**Context:** 2–3 sentences on the situation.
**Decision:** 2–3 sentences on what was chosen.
**Rejected:** one or two lines per alternative with a *concrete* reason. "Felt over-engineered" is not a reason.
**Consequences:** 2–3 lines — the forward-looking implication, not a rehash of the decision.
**Spec / link:** path to the spec or Known Issues entry if relevant.
```

## Guardrails

- Self-contained: a reader should understand the entry without reading three prior ones; link prior context, don't recap it.
- No generic rationale: cut "for simplicity" / "to reduce complexity"; state the actual constraint that made the other option worse.
- Supersede by deletion, not by appending: if a later decision overrides an earlier one, delete the superseded entry; git log preserves history.
- If the entry is longer than the cap, the "why" is a spec, not an ADR — write it under `docs/superpowers/specs/` and link to it.

## Decisions

### 2026-08-12 — Bare `zirv` becomes an alias, not a usage error
**Context:** `Input::command` is clap's required positional, so a bare `zirv` (no arguments) was a usage error, exit 2. With `zirv chat`/`zirv ctx chat` now the fastest way to start a session, requiring the extra word every time undercuts the "just run `zirv`" pitch this wave is named for. An independent review (2026-08-12) found the first cut too permissive on both axes: it treated a *global* `~/.zirv` as enough to call any directory "a zirv-managed repo," and it checked only stdin for a real terminal, so `zirv | less` (interactive stdin, redirected stdout) would have opened a chat session into the pipe.
**Decision:** A bare invocation is intercepted on raw argv, before clap: `zirv ctx chat` when a **local** `./.zirv` directory exists and **both** stdin and stdout are a real terminal, else `zirv help` — both exiting 0. A global `~/.zirv` alone no longer counts (matches the approved design's own wording, "in a repo with a `.zirv` directory"); `zirv chat`/`zirv agent` are separate top-level aliases for `zirv ctx chat`/`zirv ctx agent`, not subject to either check, and reserved (`utils::RESERVED_COMMANDS`, compared case-insensitively) so a script can never shadow them.
**Rejected:** Making the bare form always open chat regardless of `.zirv`/TTY — would hang a CI job or a piped invocation waiting on an interactive session nothing is there to drive. Counting a global `~/.zirv` — would open a chat session for a bare `zirv` in literally any directory for anyone who has ever run `zirv create --global` once, which says nothing about the directory actually being run from. Checking stdin alone — misses the `zirv | less` case, an interactive terminal with a redirected destination.
**Consequences:** Any script or automation piping into bare `zirv` now gets help text instead of an error, silently, since both are exit 0 — an agent workflow that wants a specific verb should keep naming it (`zirv ctx chat`, `zirv ctx status`, ...) rather than relying on the bare form's exit code to distinguish outcomes.
**Spec / link:** `src/main.rs`'s `bare_invocation_target`/`zirv_dir_present`/`top_level_ctx_alias`; [[Built-in Commands]], [[Architecture/Script Resolution]].

### 2026-08-12 — Mail is agent-authored text, not repo config: delivered to Worker only, labeled per batch, two keys repo-forbidden
**Context:** `zirv ctx send`/`inbox` adds a fourth untrusted-text surface (after `ctx.toml`'s repo layer, `.settings.toml`'s repo layer, and the repo system-prompt layer), but it doesn't come from a checkout — it's written by another agent session. An independent review (2026-08-12) also found the initial cut let a disabled mailbox still get folded into a launch prompt, let a repo raise its own delivered-mail cap or silence the announcement channel, and let two same-second sends silently overwrite one another on disk.
**Decision:** Mail bodies are delivered only into a headless **Worker** session's composed prompt (`exec`/`loop`, gated by `cfg.mail.enabled`), never into the interactive **Orchestrator** seat (`chat`/`wrap`), which gets a one-line unread-count advisory instead. `with_mail_layer` wraps the whole delivered batch in an explicit label, the same pattern the repo prompt layer uses (not per message — the repo layer is a single block; mail is a stream of independent messages, so one label per batch covers the same ground without repeating boilerplate). Delivered mail is consumed (`mail::consume`) right after, so a later launch/cycle does not see it again; a failed consume is swallowed, not fatal. `mail.enabled` and `mail.max_delivered_bytes` are `REPO_FORBIDDEN` (mirroring `prompt.max_repo_bytes`'s rationale): without the second, the untrusted layer could raise its own cap; without the first, a repo could re-enable delivery an operator turned off, or (read the other way) a repo could not be trusted to leave delivery on either. Mail filenames get a collision-free suffix (`_001`, `_002`, ...) on a same-second collision, sorting after the unsuffixed file so oldest-first order survives it.
**Rejected:** Delivering mail into the orchestrator's own prompt (the original design) — a human is already at the keyboard in that seat and can just run `zirv ctx inbox`; folding unreviewed agent-to-agent text straight into the one session steering delegation decisions is a bigger blast radius for a manipulative note than a headless worker that has no further harnesses to steer. A `REPO_FORBIDDEN`-style forbidden-*key* list for mail's own content — mail carries no configuration keys to forbid, only text; what needed forbidding was the two config knobs governing delivery, not the messages themselves.
**Consequences:** If mail ever grows structure beyond free text (e.g. a message that could request a config change), that structured case would need its own review — free-text notes alone don't need it.
**Spec / link:** `src/commands/ctx/mail.rs`, `src/commands/ctx/exec.rs`, `src/commands/ctx/run_loop.rs`; [[Untrusted Configuration]]'s "Mail" section, [[Ctx Supervisors]].

### 2026-08-12 — `.settings.toml` per-agent gate: AND-fold, not deep merge; warn vs error
**Context:** Adding agent enable/disable needed a repo-vs-operator trust rule like `ctx.toml`'s `REPO_FORBIDDEN`, but a boolean toggle (not a value a repo is simply blocked from setting) has to let the operator override in *either* direction while still letting a repo narrow.
**Decision:** Each layer (`~/.zirv/.settings.toml`, `<repo>/.zirv/.settings.toml`) is parsed on its own, never deep-merged; the three answers fold as `env(name) if set else home(name).unwrap_or(true) && repo(name).unwrap_or(true)`, so a repo's `enabled = true` is a silent no-op. Unknown agent names and unknown top-level sections warn (forward-compat); an unknown key inside a known `[agents.<name>]` table hard-errors (`deny_unknown_fields`) since that's a typo, not a future feature.
**Rejected:** Deep-merging all layers into one table like `ctx.toml` does — collapses "repo said true, operator said nothing" and "repo said true, operator said false" into the same merged value, losing which layer to blame in the refusal message. Denying unknown agent names outright — breaks a future repo shipping a `.settings.toml` that names an adapter this build predates.
**Consequences:** Any second `.settings.toml` section will need its own fold rule stated explicitly; the AND-fold here is specific to a boolean "may zirv use this" switch, not a general merge pattern to copy.
**Spec / link:** `src/settings.rs` module doc.
