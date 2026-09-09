## Memory
- Key: permission-approval-lease-rejected
- Written-by: claude
- Written: 1788979274
- Verified: 1788979274
- Source: explicit

zirv never caches a per-command permission decision as a reusable approval lease after a PostToolUse observation, although that would silence far more prompts. A hook can prove a command ran but not whether the human chose "allow once" or "allow for this session"; caching the former silently widens operator intent. Rejected repeatedly (2026-08-24). Any friction-reduction idea built on execution history must solve this consent-scope problem first.
