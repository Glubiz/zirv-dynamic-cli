#!/bin/sh
# Stands in for a headless codex CLI in tests. Unlike fake-agent.sh this
# needs no --session-id (codex's own headless_cmd never emits one -- codex
# always mints its own session id) and never writes a transcript (codex's
# own parse_events/structural_context are stubbed to always return
# empty/default, so nothing ever reads one).
#
# Invoked through the adapter as:
#   fake-codex-agent.sh exec <prompt> [extra...]
#   fake-codex-agent.sh exec [extra...]     (prompt on stdin, shim launches)
#
# Behavior comes from the environment:
#   FAKE_AGENT_ARGV_LOG=<path>   append the full argv of each run, so a test
#                                can assert on the delivered task prompt text
#                                -- codex has no system-prompt injection, so
#                                that argv token (or stdin, drained below) is
#                                the only place mail can ever land.
#   FAKE_AGENT_MODE_FILE=<path>  one mode per line, popped per run
#                                (hang|limit|healthy, default healthy) --
#                                codex has no rot scoring at all
#                                (parse_events/structural_context are
#                                stubbed empty), so the only way a test
#                                drives a restart is a nudge or a `limit`
#                                park. `hang` is what keeps the process
#                                alive (liveness is a real OS pid check, not
#                                transcript-derived) long enough for a
#                                nudge to land; `limit` prints the same
#                                documented limit-hit notice fake-agent.sh
#                                uses and exits 1, so a codex run can be
#                                parked the same way a claude one can.
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

# Drain stdin so a stdin-delivered prompt (the shim-launch form) does not
# block the caller's pipe, and log it the same way argv is logged.
if [ ! -t 0 ]; then
  stdin_text=$(cat)
  if [ -n "$stdin_text" ] && [ -n "${FAKE_AGENT_ARGV_LOG:-}" ]; then
    printf 'stdin: %s\n' "$stdin_text" >> "$FAKE_AGENT_ARGV_LOG"
  fi
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
