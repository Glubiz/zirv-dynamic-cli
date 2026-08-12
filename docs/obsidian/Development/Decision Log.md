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

### 2026-08-12 — `.settings.toml` per-agent gate: AND-fold, not deep merge; warn vs error
**Context:** Adding agent enable/disable needed a repo-vs-operator trust rule like `ctx.toml`'s `REPO_FORBIDDEN`, but a boolean toggle (not a value a repo is simply blocked from setting) has to let the operator override in *either* direction while still letting a repo narrow.
**Decision:** Each layer (`~/.zirv/.settings.toml`, `<repo>/.zirv/.settings.toml`) is parsed on its own, never deep-merged; the three answers fold as `env(name) if set else home(name).unwrap_or(true) && repo(name).unwrap_or(true)`, so a repo's `enabled = true` is a silent no-op. Unknown agent names and unknown top-level sections warn (forward-compat); an unknown key inside a known `[agents.<name>]` table hard-errors (`deny_unknown_fields`) since that's a typo, not a future feature.
**Rejected:** Deep-merging all layers into one table like `ctx.toml` does — collapses "repo said true, operator said nothing" and "repo said true, operator said false" into the same merged value, losing which layer to blame in the refusal message. Denying unknown agent names outright — breaks a future repo shipping a `.settings.toml` that names an adapter this build predates.
**Consequences:** Any second `.settings.toml` section will need its own fold rule stated explicitly; the AND-fold here is specific to a boolean "may zirv use this" switch, not a general merge pattern to copy.
**Spec / link:** `src/settings.rs` module doc.
