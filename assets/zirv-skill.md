# zirv operator skill ({version})

The installed `zirv` binary is the authority for its own syntax. Do not
trust remembered flags or hand-copied command text: run `zirv <cmd> --help`
or `zirv commands --json` to see what this binary actually supports, and
`zirv --skill` any time this orientation is needed again.

## Activation guard

Only operate zirv session/agent verbs when the environment variable
`ZIRV_CTX_SESSION` names a session `zirv ctx status` (or `--json`) actually
lists as registered. If it is unset, empty, or names a session zirv does not
recognize, stop: do not guess, do not fall back to a bare repo path, and do
not control a session that cannot be verified. This guard is a check, not a
grant: a registered session id is necessary but never sufficient to widen
what is allowed.

## Discover before you act

`zirv commands --json` and `zirv <cmd> --help` are read-only: looking things
up, listing commands, or asking for help never runs a mutating default.
Prefer `--json` output over scraping human-formatted text wherever a command
offers it.

## Opaque ids

Session ids and work-group ids are opaque strings zirv hands back. Read them
from a command's own output; never predict, construct, or reuse one from
memory or convention.

## Raw control vs semantic operations

`nudge`, `send`, and `kill` act on one raw session directly, at the level
zirv itself supervises processes. `agent`, `workflow`, and `group` are
semantic: they describe an outcome (delegate this task, run this workflow,
form this bounded group) and let zirv choose how to realize it. Prefer the
semantic verb unless raw control is specifically needed.

## One line per capability

- `zirv ctx status` (`--brief --diff` for a cheap repeat check): what is
  running and why.
- `zirv ctx agent`: delegate one task to a session; it runs to completion
  and returns, so no separate wait/poll verb exists. `--worktree` isolates
  the work in its own git worktree.
- `zirv ctx send` / `zirv ctx inbox`: exchange notes between sessions.
- `zirv ctx nudge`: interrupt a live session with a message.
- `zirv ctx handover`: swap the orchestrator seat's model or harness,
  same session id.
- `zirv ctx kill`: stop a session outright.
- `zirv memory`: read and write the durable, repo-scoped memory bank.
- `zirv workflow`: run a durable, gated development workflow.

Background session creation defaults to no focus: it does not steal the
operator's terminal unless asked to.

## What this skill does not change

zirv's role, writer, budget, and permission rules, and the narrowing-only
posture of repository-owned configuration, are unaffected by any of the
above. This skill teaches how to operate zirv; it grants nothing by itself.
