#!/bin/sh
# Stands in for a headless agent. Writes a claude-format transcript to exactly
# the path the claude adapter computes, so the tests exercise real path
# derivation instead of a test-only shortcut.
#
# Invoked through the adapter as:
#   fake-agent.sh -p <prompt> --session-id <uuid> [extra...]
#
# Behavior comes from the environment:
#   FAKE_AGENT_MODE=healthy|rot|hang|fail   (default healthy)
#   FAKE_AGENT_MODE_FILE=<path>             one mode per line, popped per run
#   FAKE_AGENT_TURNS=<n>                    (default 12)
#   FAKE_AGENT_SLEEP=<secs>                 rot mode only (default 0)
#
#   healthy  distinct tool inputs, marker on every final, 20k tokens
#   rot      identical tool input, every result an error, marker only on the
#            first two turns, 170k tokens: score 100, verdict restart
#   hang     writes a healthy transcript then never exits
#   fail     writes a healthy transcript then exits 3
#   limit    writes a healthy transcript, prints the documented limit-hit
#            notice on stdout, then exits 1 the way an exhausted window would
#
# FAKE_AGENT_MODE_FILE lets one test script a sequence across restarts, for
# example "rot" then "healthy" to prove a restarted child is supervised on its
# own transcript. FAKE_AGENT_SLEEP applies only in rot mode, so a rotted run
# stays alive long enough to be scored while a healthy one exits promptly.
set -eu

session=""
while [ $# -gt 0 ]; do
  case "$1" in
    --session-id) session="${2:-}"; shift 2 ;;
    *) shift ;;
  esac
done
[ -n "$session" ] || { echo "fake-agent: no --session-id given" >&2; exit 64; }

mode="${FAKE_AGENT_MODE:-healthy}"
if [ -n "${FAKE_AGENT_MODE_FILE:-}" ] && [ -s "${FAKE_AGENT_MODE_FILE}" ]; then
  mode=$(head -n 1 "$FAKE_AGENT_MODE_FILE")
  tail -n +2 "$FAKE_AGENT_MODE_FILE" > "$FAKE_AGENT_MODE_FILE.next"
  mv "$FAKE_AGENT_MODE_FILE.next" "$FAKE_AGENT_MODE_FILE"
fi
turns="${FAKE_AGENT_TURNS:-12}"

slug=$(printf '%s' "$(pwd)" | tr -c 'A-Za-z0-9-' '-')
dir="$HOME/.claude/projects/$slug"
mkdir -p "$dir"
t="$dir/$session.jsonl"
: > "$t"

emit_turn() { # $1 tool input  $2 is_error  $3 final text  $4 tokens
  printf '{"type":"user","message":{"content":"do the thing"}}\n' >> "$t"
  printf '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t","name":"Bash","input":%s}],"usage":{"input_tokens":2,"cache_read_input_tokens":%s}}}\n' "$1" "$4" >> "$t"
  printf '{"type":"user","message":{"content":[{"type":"tool_result","content":"out","is_error":%s}]}}\n' "$2" >> "$t"
  printf '{"type":"assistant","message":{"content":[{"type":"text","text":"%s"}],"usage":{"input_tokens":2,"cache_read_input_tokens":%s}}}\n' "$3" "$4" >> "$t"
}

i=1
while [ "$i" -le "$turns" ]; do
  if [ "$mode" = "rot" ]; then
    if [ "$i" -le 2 ]; then final="[zirv] step $i"; else final="step $i"; fi
    emit_turn '{"command":"ls"}' true "$final" 170000
  else
    emit_turn "{\"command\":\"ls $i\"}" false "[zirv] step $i" 20000
  fi
  i=$((i + 1))
done

sleep_secs="${FAKE_AGENT_SLEEP:-0}"
if [ "$mode" = "rot" ] && [ "$sleep_secs" != "0" ]; then
  sleep "$sleep_secs"
fi

case "$mode" in
  hang) while true; do sleep 1; done ;;
  fail) exit 3 ;;
  limit)
    printf "You've hit your session limit · resets 3:45pm\n"
    exit 1
    ;;
  *) exit 0 ;;
esac
