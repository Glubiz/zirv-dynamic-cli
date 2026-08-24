#!/bin/sh
# Stands in for a headless codex CLI in tests. Unlike fake-agent.sh this
# needs no --session-id (codex's own headless_cmd never emits one -- codex
# always mints its own session id).
#
# Invoked through the adapter as:
#   fake-codex-agent.sh exec <prompt> [extra...]
#   fake-codex-agent.sh exec [extra...]     (prompt on stdin, shim launches)
#   fake-codex-agent.sh exec --model <m> --sandbox read-only [...]
#                                            (the distiller/reviewer child,
#                                             issue #86 -- see below)
#
# Behavior comes from the environment:
#   FAKE_AGENT_ARGV_LOG=<path>   append the full argv of each run, so a test
#                                can assert on the delivered task prompt text
#                                -- codex has no system-prompt injection, so
#                                that argv token (or stdin, drained below) is
#                                the only place mail can ever land.
#   FAKE_AGENT_MODE_FILE=<path>  one mode per line, popped per run
#                                (hang|limit|healthy, default healthy) --
#                                the only way a test drives a restart of the
#                                *main* agent is a nudge or a `limit` park.
#                                `hang` is what keeps the process alive
#                                (liveness is a real OS pid check, not
#                                transcript-derived) long enough for a nudge
#                                to land; `limit` prints the same documented
#                                limit-hit notice fake-agent.sh uses and
#                                exits 1, so a codex run can be parked the
#                                same way a claude one can.
#
# Issue #86 (2026-08-23): codex's own event parsing is no longer stubbed
# empty, so `distill_or_structural` (on a nudge restart, a rot restart, or a
# clean-exit harvest) genuinely spawns this same binary again as the
# distiller/reviewer child (`CodexAdapter::distiller_cmd`: `exec --model <m>
# --sandbox read-only [...]`, prompt on stdin, no positional prompt token).
# `ZIRV_CTX_AGENT_BIN` is one binary for the whole session, so that spawn
# reaches this same script -- and would otherwise silently steal a line off
# `FAKE_AGENT_MODE_FILE` meant for the *main* agent's next launch, shifting
# every mode after it by one. Detected here by an adjacent `--sandbox
# read-only` pair on argv -- NOT a bare `--sandbox` alone, since the
# shipped-default sandbox posture (2026-08-22) now also puts `--sandbox
# workspace-write` on every *ordinary* launch, and matching that too would
# wrongly treat the main agent's own launch as the distiller. `read-only` is
# the one value only `distiller_cmd`/the workflow reviewer ever pass. Handled
# as its own case, entirely decoupled from mode-file consumption: still
# logged to `FAKE_AGENT_ARGV_LOG` like any other call (a test may want to
# assert on it), but never pops a mode and always answers with a minimal,
# real, parseable handoff so `distill()` succeeds instead of falling back to
# the mechanical "structural" extraction.
#
# CI run 32723969751 (2026-08-24) found the same one-binary-per-session
# problem on a second, independent code path: `detect_ignore_flags`
# (`adapters/codex.rs`) probes capability support by spawning `exec --help`
# on this same override before composing a nudge/rot-restart's distiller
# call. That argv has no `--sandbox read-only` pair, so `is_distiller` above
# does not exempt it, and (before this fix) it fell through to the
# mode-file branch and silently stole a line meant for the main agent's
# next launch, shifting every mode after it by one. Handled as its own case
# for the same reason `is_distiller` is: still logged to
# `FAKE_AGENT_ARGV_LOG` (a test relies on seeing "exec --help" there), but
# never pops a mode. The answer deliberately omits `--ignore-rules` and
# `--ignore-user-config` so the probe reads "unsupported", matching the
# behavior CI observed before this fix.
set -eu

head_bin=head
tail_bin=tail
sleep_bin=sleep
mv_bin=mv
[ ! -x /usr/bin/head ] || head_bin=/usr/bin/head
[ ! -x /usr/bin/tail ] || tail_bin=/usr/bin/tail
[ ! -x /usr/bin/sleep ] || sleep_bin=/usr/bin/sleep
[ ! -x /bin/sleep ] || sleep_bin=/bin/sleep
[ ! -x /usr/bin/mv ] || mv_bin=/usr/bin/mv
[ ! -x /bin/mv ] || mv_bin=/bin/mv

[ -z "${FAKE_AGENT_ARGV_LOG:-}" ] || printf '%s\n' "$*" >> "$FAKE_AGENT_ARGV_LOG"

is_distiller=0
is_help_probe=0
prev=""
for arg in "$@"; do
  if [ "$prev" = "--sandbox" ] && [ "$arg" = "read-only" ]; then
    is_distiller=1
  fi
  if [ "$arg" = "--help" ]; then
    is_help_probe=1
  fi
  prev="$arg"
done

# Drain stdin so a stdin-delivered prompt (the shim-launch form, and every
# distiller/reviewer call) does not block the caller's pipe, and log it the
# same way argv is logged.
if [ ! -t 0 ]; then
  stdin_text=$(cat)
  if [ -n "$stdin_text" ] && [ -n "${FAKE_AGENT_ARGV_LOG:-}" ]; then
    printf 'stdin: %s\n' "$stdin_text" >> "$FAKE_AGENT_ARGV_LOG"
  fi
fi

if [ "$is_distiller" -eq 1 ]; then
  printf '## Task\nfake distilled task\n\n## Next step\nfake next step\n'
  exit 0
fi

if [ "$is_help_probe" -eq 1 ]; then
  printf 'codex-exec\n\nUsage: codex exec [OPTIONS] [PROMPT]\n\nOptions:\n  -h, --help  Print help\n'
  exit 0
fi

mode="healthy"
if [ -n "${FAKE_AGENT_MODE_FILE:-}" ] && [ -s "${FAKE_AGENT_MODE_FILE}" ]; then
  mode=$("$head_bin" -n 1 "$FAKE_AGENT_MODE_FILE")
  "$tail_bin" -n +2 "$FAKE_AGENT_MODE_FILE" > "$FAKE_AGENT_MODE_FILE.next"
  "$mv_bin" "$FAKE_AGENT_MODE_FILE.next" "$FAKE_AGENT_MODE_FILE"
fi

case "$mode" in
  hang) while true; do "$sleep_bin" 1; done ;;
  limit)
    printf "You've hit your session limit · resets 3:45pm\n"
    exit 1
    ;;
  *) exit 0 ;;
esac
