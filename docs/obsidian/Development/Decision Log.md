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
