## Memory
- Key: windows-sandbox-broker-evaluation-bar
- Written-by: claude
- Written: 1788979290
- Verified: 1788979290
- Source: explicit

Native Windows Claude Code has no OS sandbox; containment there rests on the PreToolUse safety hook alone, an accepted standing gap. Brokers Landstrip, Arapuca and Microsoft MXC were evaluated and rejected (2026-08-24) as mandatory invisible brokers: none proves one identical security contract across Windows/macOS/Linux, MXC is preview-only. Any broker adopted here must pass adversarial three-OS tests, fail closed, keep approval presentation and expose an auditable policy.
