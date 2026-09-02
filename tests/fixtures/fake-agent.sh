#!/bin/sh
# Stands in for a headless agent. Writes a claude-format transcript to exactly
# the path the claude adapter computes, so the tests exercise real path
# derivation instead of a test-only shortcut.
#
# Invoked through the adapter as:
#   fake-agent.sh -p <prompt> --session-id <uuid> [extra...]
#
# Behavior comes from the environment:
#   FAKE_AGENT_MODE=healthy|rot|compact-tier|hang|fail   (default healthy)
#   FAKE_AGENT_MODE_FILE=<path>             one mode per line, popped per run
#   FAKE_AGENT_TURNS=<n>                    (default 12)
#   FAKE_AGENT_SLEEP=<secs>                 rot mode only (default 0)
#   FAKE_AGENT_SESSION_ENV_LOG=<path>       append $ZIRV_CTX_SESSION per run,
#                                           so a test can see what each child
#                                           was told to report signals as
#   FAKE_AGENT_GROUP_ENV_LOG=<path>         append $ZIRV_CTX_WORK_GROUP per
#                                           run, so a test can see the work
#                                           group the child inherited
#   FAKE_AGENT_PARENT_ENV_LOG=<path>        append $ZIRV_CTX_PARENT_SESSION
#                                           per run (issue #249), so a test
#                                           can see which session the child
#                                           was told is its own supervisor
#   FAKE_AGENT_HEADLESS_ENV_LOG=<path>      append $ZIRV_CTX_HEADLESS per run
#   FAKE_AGENT_ARGV_LOG=<path>              append the full argv of each run,
#                                           so a test can assert on injected
#                                           flags such as --append-system-prompt
#   FAKE_AGENT_CWD_LOG=<path>               append the process's own working
#                                           directory per run (issue #228: a
#                                           test can assert a headless
#                                           spawn's child actually launched
#                                           in the requested --workdir)
#   FAKE_AGENT_COMPACTION_EVENT=0           omit the compact-boundary event,
#                                           exercising fail-closed verification
#
#   healthy  distinct tool inputs, marker on every final, 20k tokens
#   rot      identical tool input, every result an error, marker only on the
#            first two turns, 170k tokens: score 100, verdict restart
#   compact-tier  healthy signals at 165k tokens: verdict compact
#   hang     writes a healthy transcript then never exits
#   fail     writes a healthy transcript then exits 3
#   limit    writes a healthy transcript, prints the documented limit-hit
#            notice on stdout, then exits 1 the way an exhausted window would
#   drift    prints a line that only loosely resembles a limit notice (the
#            wording the strict patterns do NOT recognize) and exits 0, so a
#            test can prove the breadcrumb is left without a park
#   capacity issue #227: writes a healthy transcript, prints codex's
#            "Selected model is at capacity" notice on stdout, then exits 1
#            the way a transient provider capacity error would
#   account  issue #227: writes a healthy transcript, prints an
#            insufficient_quota/billing-exhaustion notice on stdout, then
#            exits 1 the way a hard, non-retryable account exhaustion would
#
# FAKE_AGENT_MODE_FILE lets one test script a sequence across restarts, for
# example "rot" then "healthy" to prove a restarted child is supervised on its
# own transcript. FAKE_AGENT_SLEEP applies only in rot mode, so a rotted run
# stays alive long enough to be scored while a healthy one exits promptly.
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

session=""
prompt=""
resumed=false
while [ $# -gt 0 ]; do
  case "$1" in
    --session-id) session="${2:-}"; shift 2 ;;
    --resume) session="${2:-}"; resumed=true; shift 2 ;;
    -p)
      if [ $# -gt 1 ] && [ "${2#--}" = "$2" ]; then
        prompt=$2
        shift 2
      else
        shift
      fi
      ;;
    *) shift ;;
  esac
done
[ -n "$session" ] || { echo "fake-agent: no --session-id given" >&2; exit 64; }

mode="${FAKE_AGENT_MODE:-healthy}"
case "$prompt" in
  /compact*) ;;
  *)
    if [ -n "${FAKE_AGENT_MODE_FILE:-}" ] && [ -s "${FAKE_AGENT_MODE_FILE}" ]; then
      mode=$("$head_bin" -n 1 "$FAKE_AGENT_MODE_FILE")
      "$tail_bin" -n +2 "$FAKE_AGENT_MODE_FILE" > "$FAKE_AGENT_MODE_FILE.next"
      "$mv_bin" "$FAKE_AGENT_MODE_FILE.next" "$FAKE_AGENT_MODE_FILE"
    fi
    ;;
esac

if [ -n "${FAKE_AGENT_SESSION_ENV_LOG:-}" ]; then
  printf '%s\n' "${ZIRV_CTX_SESSION:-}" >> "$FAKE_AGENT_SESSION_ENV_LOG"
fi
if [ -n "${FAKE_AGENT_GROUP_ENV_LOG:-}" ]; then
  printf '%s\n' "${ZIRV_CTX_WORK_GROUP:-}" >> "$FAKE_AGENT_GROUP_ENV_LOG"
fi
if [ -n "${FAKE_AGENT_PARENT_ENV_LOG:-}" ]; then
  printf '%s\n' "${ZIRV_CTX_PARENT_SESSION:-}" >> "$FAKE_AGENT_PARENT_ENV_LOG"
fi
if [ -n "${FAKE_AGENT_HEADLESS_ENV_LOG:-}" ]; then
  printf '%s\n' "${ZIRV_CTX_HEADLESS:-}" >> "$FAKE_AGENT_HEADLESS_ENV_LOG"
fi
turns="${FAKE_AGENT_TURNS:-12}"

cwd=$(pwd)
if windows_cwd=$(pwd -W 2>/dev/null); then
  cwd="$windows_cwd"
fi
[ -z "${FAKE_AGENT_CWD_LOG:-}" ] || printf '%s\n' "$cwd" >> "$FAKE_AGENT_CWD_LOG"
slug=$(printf '%s' "$cwd" | tr -c 'A-Za-z0-9-' '-')
dir="$HOME/.claude/projects/$slug"
mkdir -p "$dir"
t="$dir/$session.jsonl"
if [ "$resumed" = false ]; then
  : > "$t"
fi

# A headless in-place compaction resumes the existing conversation with the
# adapter's compact command as its prompt. It must not consume the next normal
# run mode: the continuation launched immediately afterwards owns that entry.
case "$prompt" in
  /compact*)
    if [ "${FAKE_AGENT_COMPACTION_EVENT:-1}" != "0" ]; then
      printf '{"type":"system","subtype":"compact_boundary","content":"compacted"}\n' >> "$t"
    fi
    exit 0
    ;;
esac

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
  elif [ "$mode" = "compact-tier" ]; then
    emit_turn "{\"command\":\"ls $i\"}" false "[zirv] step $i" 165000
  else
    emit_turn "{\"command\":\"ls $i\"}" false "[zirv] step $i" 20000
  fi
  i=$((i + 1))
done

sleep_secs="${FAKE_AGENT_SLEEP:-0}"
if { [ "$mode" = "rot" ] || [ "$mode" = "compact-tier" ]; } && [ "$sleep_secs" != "0" ]; then
  "$sleep_bin" "$sleep_secs"
fi

case "$mode" in
  hang) while true; do "$sleep_bin" 1; done ;;
  fail) exit 3 ;;
  limit)
    printf "You've hit your session limit · resets 3:45pm\n"
    exit 1
    ;;
  drift)
    printf "Notice: you have reached your limit for this model\n"
    exit 0
    ;;
  capacity)
    printf "ERROR: Selected model is at capacity. Please try a different model.\n"
    exit 1
    ;;
  account)
    printf "Error: insufficient_quota - You exceeded your current quota, please check your plan and billing details.\n"
    exit 1
    ;;
  *) exit 0 ;;
esac
