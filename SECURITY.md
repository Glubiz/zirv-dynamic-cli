# Security policy

## Supported versions

Only the [latest release](https://github.com/Glubiz/zirv-cli/releases) is supported
for security updates.

## Reporting a vulnerability

Use [GitHub private vulnerability reporting](https://github.com/Glubiz/zirv-cli/security/advisories/new)
via "Report a vulnerability" under the Security tab, or email josj@zirv.io.
Do not open public issues for vulnerabilities. Never include secrets or tokens
in reports.

Include:

- zirv version (`zirv --version`)
- Operating system
- Steps to reproduce
- Potential impact

Expect acknowledgement within 7 days.

## Critical files

Changes here need extra scrutiny in review -- PTY/subprocess spawn, argv
construction, permission/safety decisions, or repo-owned config parsing.
`scripts/security-scan.sh` reads this exact fenced list (path before the
first `:`) so the advisory CI scan and this document can never drift; a
trailing `*` means "this directory, any file directly or nested inside it".

```text
src/commands/ctx/wrap.rs: spawns and supervises the PTY-wrapped child process
src/commands/ctx/exec.rs: builds and launches the adapter subprocess argv
src/commands/ctx/run_loop.rs: drives the supervised run loop around the child
src/commands/ctx/adapters/*: construct the exact argv handed to each harness
src/commands/ctx/safety.rs: classifies commands as safe or unsafe to run
src/commands/ctx/permissions.rs: grants or denies tool permission requests
src/commands/ctx/policy.rs: classifies commands for the safety/permission layers
src/commands/ctx/config.rs: parses repo-layer config and enforces REPO_FORBIDDEN
src/commands/ctx/hook.rs: makes allow/deny decisions and rewrites updatedInput
src/commands/ctx/setup.rs: writes hook entries into a harness's own settings
src/script_runner/*: runs repo-owned `.zirv` scripts
```
