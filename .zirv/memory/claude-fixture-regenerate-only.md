## Memory
- Key: claude-fixture-regenerate-only
- Written-by: claude
- Written: 1788979272
- Verified: 1788979272
- Source: explicit

The claude fixture pair tests/fixtures/claude-real-session.jsonl + .expected.json is a capture of a real session and must never be hand-edited: regenerate it with scripts/record-claude-fixture.py <source.jsonl> so the parser is tested against shapes Claude Code actually emits, not hand-shaped approximations.
