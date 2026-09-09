## Memory
- Key: codex-compact-resume-unsupported
- Written-by: claude
- Written: 1788979272
- Verified: 1788979272
- Source: explicit

codex exec resume <id> "/compact" does NOT compact: verified live on codex-cli 0.147.0, the text is sent as an ordinary user message and no compaction is recorded afterwards. Rejected as a compaction trigger (issue #303, 2026-09-02); nothing implements it. Codex still has no verified compaction mechanism of any kind, so a future codex-compaction feature needs a genuinely different primitive, not this one.
