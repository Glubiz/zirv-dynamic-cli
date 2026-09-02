# zirv -- Codex-specific working instructions

## How your context arrives

Codex has no system-prompt injection on a shell-shim launch (a Windows npm
`.cmd` resolved through `cmd.exe /c`), and only `developer_instructions` on a
direct launch. Your standing instructions therefore arrive through AGENTS.md
and appended task text, not an injected system prompt. Treat this file plus
`.zirv/context/common.md` and AGENTS.md as your instructions and re-read them
rather than assuming a harness injected them. Mail (`zirv ctx send`) reaches a
codex worker only by being appended to the task prompt, and only once -- quote
anything you need to keep.

## Verification

Run the checks yourself, in the foreground, to completion, per the tiers in
`.zirv/context/common.md`.

## Sandbox

zirv pins `--sandbox read-only` for read-only roles (the distiller and the
workflow reviewer). On a Windows install whose sandbox helper
`codex-windows-sandbox-setup.exe` is missing, that flag fails outright with
`orchestrator_helper_launch_failed ... program not found`. If you hit it, do
not retry with a wider sandbox: fall back to read-only tools (read / grep /
glob only), say so explicitly in your report, and let the caller decide.
`--ignore-rules` and `--ignore-user-config` exist only on codex-cli 0.146 and
later; npm publishes 0.105.0, which errors on them.

Documentation duties (which vault pages to update and when) are covered by
`.zirv/context/common.md`.
