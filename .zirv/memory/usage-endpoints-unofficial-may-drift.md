## Memory
- Key: usage-endpoints-unofficial-may-drift
- Written-by: claude
- Written: 1788979275
- Verified: 1788979275
- Source: explicit

zirv's usage-poll endpoints are unofficial: Anthropic's oauth/usage endpoint is Claude Code's own internal one (verified against one real response), and codex's chatgpt.com backend endpoint is tested only against synthetic bodies. Both parsers in poll.rs degrade to None on an unrecognised shape rather than erroring, so a vendor change silently kills pacing input with no crash or log. If usage looks stuck stale, suspect endpoint drift before zirv logic.
