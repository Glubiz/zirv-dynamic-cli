## Memory
- Key: ctx-about-oncelock
- Written-by: setup-migration
- Written: 1787496000
- Verified: 1787496000
- Source: setup
- Tags: performance, adapters, cli
- Paths: src/commands/ctx/mod.rs, src/commands/ctx/adapters/mod.rs

ctx/mod.rs's CtxCli `about` text calls adapters::readiness_note(), which calls ready() on every registered adapter (each a process probe). It is cached in a process-wide OnceLock via ctx_about(), because it otherwise re-runs on every dispatch() call -- including hook and statusline invocations that never display the about text at all. Do not reintroduce a direct readiness_note() call on the about path.
