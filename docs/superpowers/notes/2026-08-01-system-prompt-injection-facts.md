# System-prompt injection facts (verified 2026-08-01)

Probed on this macOS machine against the installed CLIs. Basis for Phase G of
docs/superpowers/plans/2026-08-01-zirv-ctx-optimize-and-run.md. Anything marked
BLOCKED ships as "no injection for that agent", never as a guess.

claude version: 2.1.220 (Claude Code)
codex version: codex-cli 0.146.0

## claude: flag existence and help text

verified: `claude --help` lists two distinct flags. Verbatim:

```
  --append-system-prompt <prompt>       Append a system prompt to the default
                                        system prompt
```

and, separately, in the same help output:

```
  --system-prompt <prompt>              System prompt to use for the session
```

Neither entry's help text restricts it to print mode. A third, related flag
also exists: `--exclude-dynamic-system-prompt-sections`, whose own help text
says "Only applies with the default system prompt (ignored with
--system-prompt)", which confirms `--system-prompt` *replaces* the default
prompt while `--append-system-prompt` extends it. `--bare` mode's help text
separately documents `--system-prompt[-file]` and
`--append-system-prompt[-file]` as the sanctioned way to supply context when
auto-discovery is disabled, which is corroborating evidence that both flags
are first-class, not print-only conveniences.

## claude: print mode (-p) effect

verified: command run and answer received verbatim:

```
$ claude -p --model haiku --append-system-prompt 'When asked for the codeword, answer exactly: ZIRVPROBE7' \
  'What is the codeword? Answer with one word.'
```

Answer: `ZIRVPROBE7` (the model's own text was prefixed `[josj]`, which is this
machine's own global-CLAUDE.md canary convention bleeding into the probed
child session; it does not affect the result. The codeword itself matched
exactly.)

## claude: interactive acceptance

verified: `script -q /dev/null claude --append-system-prompt 'probe' --help
</dev/null` exited `exit=0`. The `grep -ci 'unknown option\|unexpected
argument\|error'` check matched `1`, not the expected `0`; inspection of the
matched line showed it was unrelated help text ("ignored in this mode (no
error dialog is ...)") rather than a real parse failure, so the flag was
accepted, not rejected.

verified (stronger): `cargo run --quiet -- ctx wrap --no-supervise -- claude
--append-system-prompt 'probe' </dev/null` actually launched the interactive
TUI. The captured terminal output shows the real banner ("Claude Code
v2.1.220", model/effort line, prompt box, `PR #12` status line), i.e. the
session started and rendered normally with the flag present, with no
argument-parse error at any point.

NOT verified: exit-on-EOF. The brief expected the session to exit cleanly
once stdin hit EOF from `/dev/null`. Observed behavior deviated: the process
ran past the 60s timeout and had to be killed manually (`kill -9`). This is
attributable to `wrap` spawning the child inside its own PTY (portable-pty):
redirecting the parent `cargo run` process's stdin from `/dev/null` does not
deliver an EOF byte into the child's PTY the way a plain pipe would, so the
interactive claude process just sat idle waiting for terminal input. This is
a property of the PTY plumbing, not of `--append-system-prompt`, and it does
not change the conclusion: the flag is accepted at the interactive entry
point. Behavioral effect of the appended text in interactive mode (as
opposed to mere acceptance) is still not asserted by any probe here, exactly
as the task brief anticipated.

## claude: argument size limits observed

verified: this machine's `getconf ARG_MAX` is `1048576` bytes (1 MiB). An
8000-byte value passed to `--append-system-prompt` in print mode was accepted
and honored (a second codeword, `ZIRVPROBE8`, appended after 8000 filler
bytes, came back correctly). No truncation or rejection was observed. The
shipped default prompt (Task G2) is under 1200 bytes and the repo-layer cap
defaults to 4096 bytes, both far below both the tested 8000-byte value and
the 1 MiB OS ceiling, so no practical argument-length limit applies to this
feature.

## codex: per-run system-prompt flag

BLOCKED: no per-run system-prompt (or equivalent instructions) flag exists.
`codex --help`, `codex exec --help`, and a full-text grep of `codex --help`
for `system|instruction|prompt|config|profile` and separately for
`AGENTS|instructions|memory` produced no such flag. The only prompt-shaped
thing codex accepts is the positional `[PROMPT]` user message argument
itself (`codex [OPTIONS] [PROMPT]`, `codex exec [OPTIONS] [PROMPT]`), which is
a user turn, not a system prompt.

## codex: config keys (-c) that affect instructions

BLOCKED: `-c, --config <key=value>` overrides arbitrary dotted keys in
`~/.codex/config.toml` (e.g. `-c model="o3"`, `-c
sandbox_permissions=["disk-full-read-access"]`, `-c
shell_environment_policy.inherit=all`), but no `~/.codex/config.toml` exists
on this machine to enumerate real key names against (`cat` reported "no
config.toml"), and the help text names no `instructions`-shaped key. Probing
further would mean guessing key names against a running session, which is
out of scope for a help-and-config-level probe. Not verified either way
beyond: no documented key surfaced.

## codex: AGENTS.md layering

BLOCKED: nothing in `codex --help` (129 lines, full text) mentions AGENTS.md,
"instructions", or "memory" in any form. `~/.codex/` exists on this machine
(prior unrelated use created `sessions/`, `skills/`, sqlite state files) but
holds no config.toml and nothing that documents file-based instruction
layering. Whether codex reads a project `AGENTS.md` the way claude reads
`CLAUDE.md` cannot be confirmed from the installed CLI's own documented
surface without running an actual session, which this task is barred from
doing while codex is unauthenticated.

## Conclusion: capability matrix

| agent | injection mechanism | interactive | print/headless |
|---|---|---|---|
| claude | `--append-system-prompt <text>` | verified: flag accepted, TUI launches normally; behavioral effect of the text in interactive mode not asserted | verified: flag accepted and behaviorally effective (`-p` codeword test) |
| codex | none found (BLOCKED) | not applicable, no mechanism | not applicable, no mechanism |

Task G3 encodes only the claude row. Codex's adapter returns an empty
argument list and reports `system_prompt: false` in its capabilities, per
the Global Constraints' "BLOCKED fact" rule.

## I6 fix round (2026-08-01): restricting the judgment/distiller model child's tools

Background: `handoff::run_model` spawns `adapter.distiller_cmd(model)` with no
tool restriction, and the analysed repo's own (untrusted) CLAUDE.md text is
embedded in the prompt handed to that child. `optimize`'s judgment call and
`handoff::distill`/`distill_or_structural` both go through it. Probed against
the real installed CLI (2.1.220, this user's `~/.claude/settings.json` has
`defaultMode: auto` and allows `Edit`/`Write`/`Bash`) to find a flag that
provably blocks tool use, per the fix ruling's verify-first requirement.

All probes ran `claude -p --model haiku "<prompt>"` in a fresh, empty temp
directory and checked both the exit code and whether a file the prompt asked
for actually appeared on disk.

**Baseline, no restriction flag**: prompt "Create a file named
PROBE_MARKER.txt ... containing exactly the text: probed-ok". Exit 0,
`PROBE_MARKER.txt` created with the exact content. Confirms the risk is real
under this machine's own default permission settings, not merely a
theoretical concern.

**`--allowedTools=""` (empty allow-list)**: same prompt, exit 0,
`PROBE_MARKER.txt` still created. An empty allow-list does **not** deny
everything by default; it appears to be treated as no filter at all. Ruled
out: this is the "obvious" fix and it does not work.

**`--permission-mode plan`**: same prompt. Took roughly 90+ seconds (versus
low single-digit seconds for every other probe) and the file was not
created, but the model's own answer explained why: `ExitPlanMode` and
`AskUserQuestion` are not callable in a non-interactive `-p` session, so
plan mode has no way to hand back control and finish normally in this
invocation shape. No file was created, but the slow, ambiguous completion
and the wrong output shape (a written plan artifact, not the plain-text
answer `run_model` expects and bounds with its own `timeout`) rule this out
as the encoded fix; a model that reliably takes 90s+ to answer a bounded
distillation call is itself a regression.

**`--disallowedTools=Write,Edit,Bash,NotebookEdit` (comma-separated, `=`-bound
to the flag)**: same prompt, exit 0, no file created. The model's own answer
named the exact cause ("The Write tool is not currently available in this
context"). Repeated with a prompt that explicitly told the model to fall
back to Bash if Write was blocked: **excluding `Bash` from the deny list
let the model create the file anyway via a shell redirect** (`echo ... >
file`), so `Bash` must be included, not just `Write`/`Edit`. With the full
four-tool list, repeated once more with a prompt explicitly nudging toward
Task/subagent delegation or "any other indirect way": the model reported it
exhausted every method (Write, Bash, Monitor/background delegation,
TaskCreate) and could not create the file. No file was created in either
adversarial rerun.

**Argv-shape gotcha, verified the hard way**: passing the flag and its value
as two separate argv entries (`--disallowedTools "Write Edit Bash
NotebookEdit"`, i.e. what `cmd.arg("--disallowedTools").arg("Write,Edit,...")`
would produce in `std::process::Command`) made the CLI's variadic tools
parser swallow the *next* argv entry too — in the probe that next entry was
the prompt itself, which came back word-split into a dozen bogus "Permission
deny rule ... matches no known tool" warnings and then failed outright with
"Input must be provided either through stdin or as a prompt argument". Binding
the value to the flag with `=` as a single argv token
(`--disallowedTools=Write,Edit,Bash,NotebookEdit`) does not have this problem.
This matters less in production here (the prompt travels over stdin, not
argv), but the flag is still encoded as one `=`-bound argv token to match
exactly what was verified rather than the two-token form that was verified
broken.

**Chosen fix**: `ClaudeAdapter::distiller_cmd` appends
`--disallowedTools=Write,Edit,Bash,NotebookEdit` as a single argv token. This
is the only probed option that is both fast (comparable latency to the
unrestricted baseline) and adversarially verified to block tool use,
including an explicit attempt to route around it via Bash or Task
delegation. Codex is out of scope for this fix: it has no verified
per-run permission-restriction flag any more than it has a verified
system-prompt flag (see the BLOCKED codex sections above), and its adapter
is not touched.
