## Memory
- Key: shipped-posture-allows-zirv
- Written-by: setup-migration
- Written: 1787496000
- Verified: 1787496000
- Source: setup
- Importance: high
- Tags: safety, policy, prompt, invariant
- Paths: src/commands/ctx/safety.rs, src/commands/ctx/prompt.rs

The injected prompt must never mandate a command family the shipped posture denies (issue #98). SHIPPED_POSTURE_ALLOW in safety.rs had no zirv entry, so the prompt's own mandated `zirv ctx status/inbox/send/nudge/remember/recall`, `zirv agent <name>`, and `zirv <script>` were silently denied under dontAsk -- final for the session, no escalation prompt. Fixed by adding Bash(zirv *), Bash(cargo fmt *), Bash(cargo clippy *). Pinned by prompt_mandated_zirv_commands_are_allowed_by_the_shipped_posture.
