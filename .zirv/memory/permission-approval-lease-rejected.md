## Memory
- Key: permission-approval-lease-rejected
- Written-by: claude
- Written: 1788980047
- Verified: 1788980047
- Source: explicit

zirv never caches a per-command permission decision as a reusable approval lease after a PostToolUse observation, although that would silence far more prompts. A hook can prove a command ran but not whether the human chose "allow once" or "allow for this session"; caching the former silently widens operator intent. Rejected repeatedly (2026-08-24). A friction-reduction idea built on execution history inherits this consent-scope problem.
