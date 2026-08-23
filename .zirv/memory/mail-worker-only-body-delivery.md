## Memory
- Key: mail-worker-only-body-delivery
- Written-by: setup-migration
- Written: 1787496000
- Verified: 1787496000
- Source: setup
- Tags: mail, prompt, adapters
- Paths: src/commands/ctx/mail.rs, src/commands/ctx/prompt.rs, src/commands/ctx/exec.rs

zirv ctx send/inbox deliver full message BODIES only into a headless Worker session (exec/loop, gated by cfg.mail.enabled, consumed via mail::consume right after so a later cycle does not re-see them). An interactive Orchestrator (chat/wrap) never gets bodies -- only a one-line advisory typed in at a verified-idle boundary (wrap's MAIL_POLL poll, or the dashboard's mail sweep). claude gets it via with_mail_layer; codex has no injection, so task_prompt_with_mail_fallback appends it to the task text.
