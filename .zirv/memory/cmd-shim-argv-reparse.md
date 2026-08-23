## Memory
- Key: cmd-shim-argv-reparse
- Written-by: setup-migration
- Written: 1787496000
- Verified: 1787496000
- Source: setup
- Importance: high
- Tags: security, windows, adapters, launch
- Paths: src/commands/ctx/adapters/mod.rs, src/commands/ctx/prompt.rs, src/commands/ctx/exec.rs

On Windows adapters::resolve_program rewrites an npm claude.cmd/codex.cmd to `cmd.exe /c <shim>`, and cmd.exe REPARSES the whole appended command line -- any argv element carrying & | < > ^ ( ) % ! " is executed as a command. The approach is to keep untrusted content off the argv entirely: chat.model charset validation, forced --append-system-prompt-file on a shim launch, headless prompt via stdin, and adapters::guard_cmd_shim_reparse as a fail-closed backstop at every spawn seam.
