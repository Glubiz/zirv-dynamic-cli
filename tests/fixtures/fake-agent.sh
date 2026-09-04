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
#   FAKE_AGENT_PROMPT_LOG=<path>            issue #299 (prompt-prefix
#                                           stability harness): append the
#                                           composed system prompt this run
#                                           was actually handed, byte-exact,
#                                           framed as \036<turn-index>\036
#                                           followed by the raw bytes -- no
#                                           normalization, no trailing
#                                           newline, no shell interpolation
#                                           of the payload (cat/printf '%s'
#                                           only). Turn index starts at 1 and
#                                           is tracked in a companion
#                                           "$FAKE_AGENT_PROMPT_LOG.turn"
#                                           file, since one log file spans
#                                           every launch of one test.
#                                           zirv's claude adapter delivers
#                                           the composed prompt one of two
#                                           ways: inline on argv
#                                           (--append-system-prompt <text>,
#                                           logged verbatim) or, when the
#                                           installed binary supports it,
#                                           through a private file
#                                           (--append-system-prompt-file
#                                           <path>, whose BYTES are logged,
#                                           never the path). The codex
#                                           adapter has no per-run
#                                           system-prompt mechanism at all
#                                           and instead folds the composed
#                                           prompt into a `-c
#                                           developer_instructions=<json>`
#                                           config override, also on argv
#                                           (the raw, still-JSON-encoded
#                                           value is logged, matching
#                                           whatever actually reached this
#                                           process). A run with none of the
#                                           three present still logs its
#                                           frame with an empty payload.
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
#   contract-ok   issue #318: writes a healthy transcript, then a final
#            assistant message whose text ends in a fenced ```json block
#            satisfying the sample `{"fields":[{"name":"status",...}]}`
#            OUTPUT CONTRACT tests declare (`{"status": "done"}`), exits 0
#   contract-bad  issue #318: same shape as contract-ok, but the fenced
#            json block's `status` value (`"bogus"`) is not one of the
#            contract's declared enum values, exits 0
#
# Issue #314: an invocation with NO --session-id at all is not a headless
# agent launch -- it is a one-shot distiller/judge call
# (`handoff::run_model`/adapter `distiller_cmd`, e.g. `-p --model <m>
# --output-format text ...`, prompt on stdin, nothing written to a
# transcript). Answered separately, per FAKE_JUDGE_MODE (default "done"):
#   done            {"verdict":"done","reason":"objective satisfied"}
#   blocked         {"verdict":"blocked","reason":"missing credential"}
#   continue        {"verdict":"continue","reason":"more work remains"}
#   wait_seconds    {"verdict":"wait","wait_on":{"seconds":1},...}
#   wait_pid        {"verdict":"wait","wait_on":{"pid":999999999},...} (an
#                   implausible pid, so a real liveness check sees it dead)
#   wait_file       {"verdict":"wait","wait_on":{"file":"$FAKE_JUDGE_WAIT_FILE"}}
#   bad             plain prose with no JSON at all, an unparseable answer
#   fail            non-zero exit
#   hang            never answers
# FAKE_JUDGE_PROMPT_LOG=<path>  writes the received prompt verbatim, so a
#                   test can assert what the judge was actually asked.
# FAKE_JUDGE_MODE_FILE=<path>   one mode per line, popped per judge call --
#                   the FAKE_JUDGE_MODE analogue of FAKE_AGENT_MODE_FILE, so
##                   one test can script a sequence of verdicts (e.g. "wait"
#                   then "done" once the loop resumes). A `--help` invocation
#                   (claude adapter's own pre-existing file-support probe,
#                   `detect_help_flag`) is answered separately and never
#                   touches FAKE_JUDGE_MODE/FAKE_JUDGE_MODE_FILE at all.
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
# Captured before the arg-parsing loop below (which consumes "$@" into named
# variables and drops anything it does not recognise): whether this
# invocation is claude adapter's own pre-existing `--help` capability probe
# (`detect_help_flag`, stdin nulled, no --session-id) rather than a real
# headless launch OR a distiller/judge call, both of which this script tells
# apart by --session-id's presence alone below. Without this, issue #314's
# own "no --session-id means judge" rule would misclassify that unrelated,
# already-existing probe as a judge call, silently consuming a scripted
# FAKE_JUDGE_MODE_FILE line meant for the real one.
original_all_args="$*"

session=""
prompt=""
resumed=false
# Issue #299: the three verified ways zirv's own adapters actually deliver
# the composed system prompt to this process -- see FAKE_AGENT_PROMPT_LOG's
# own doc comment above for which adapter uses which.
sys_prompt=""
sys_prompt_set=false
sys_prompt_file=""
codex_dev_instructions=""
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
    --append-system-prompt)
      sys_prompt="${2:-}"
      sys_prompt_set=true
      shift 2
      ;;
    --append-system-prompt-file)
      sys_prompt_file="${2:-}"
      shift 2
      ;;
    -c)
      case "${2:-}" in
        developer_instructions=*)
          codex_dev_instructions="${2#developer_instructions=}"
          ;;
      esac
      shift 2
      ;;
    *) shift ;;
  esac
done
if [ -z "$session" ]; then
  case " $original_all_args " in
    *' --help '*)
      # claude adapter's own pre-existing `--help` capability probe -- see
      # this block's own comment above. Deliberately never names
      # --append-system-prompt-file, so `system_prompt_supported` reads
      # "unsupported" here exactly as it always has (fake-agent.sh never
      # supported that probe answering "yes").
      printf 'usage: claude [options]\n'
      exit 0
      ;;
  esac
  # Issue #314: no --session-id (and not the --help probe above) means this
  # is a distiller/judge call, not a headless agent launch -- see this
  # file's own header comment. Entirely separate from FAKE_AGENT_MODE/
  # FAKE_AGENT_MODE_FILE above, which only ever apply to a real
  # --session-id launch.
  judge_prompt=$(cat)
  [ -z "${FAKE_JUDGE_PROMPT_LOG:-}" ] || printf '%s' "$judge_prompt" > "$FAKE_JUDGE_PROMPT_LOG"
  judge_mode="${FAKE_JUDGE_MODE:-done}"
  if [ -n "${FAKE_JUDGE_MODE_FILE:-}" ] && [ -s "${FAKE_JUDGE_MODE_FILE}" ]; then
    judge_mode=$("$head_bin" -n 1 "$FAKE_JUDGE_MODE_FILE")
    "$tail_bin" -n +2 "$FAKE_JUDGE_MODE_FILE" > "$FAKE_JUDGE_MODE_FILE.next"
    "$mv_bin" "$FAKE_JUDGE_MODE_FILE.next" "$FAKE_JUDGE_MODE_FILE"
  fi
  case "$judge_mode" in
    fail) echo "fake judge blocked by sandbox" >&2; exit 4 ;;
    hang) while true; do "$sleep_bin" 1; done ;;
    bad) printf 'I looked things over and it seems mostly fine.\n' ;;
    blocked) printf '{"verdict":"blocked","reason":"missing credential"}\n' ;;
    continue) printf '{"verdict":"continue","reason":"more work remains"}\n' ;;
    wait_seconds) printf '{"verdict":"wait","reason":"napping","wait_on":{"seconds":1}}\n' ;;
    wait_pid) printf '{"verdict":"wait","reason":"napping","wait_on":{"pid":999999999}}\n' ;;
    wait_file)
      printf '{"verdict":"wait","reason":"napping","wait_on":{"file":"%s"}}\n' \
        "${FAKE_JUDGE_WAIT_FILE:-/nonexistent}"
      ;;
    *) printf '{"verdict":"done","reason":"objective satisfied"}\n' ;;
  esac
  exit 0
fi

if [ -n "${FAKE_AGENT_PROMPT_LOG:-}" ]; then
  turn_file="$FAKE_AGENT_PROMPT_LOG.turn"
  turn_n=1
  [ -s "$turn_file" ] && turn_n=$("$head_bin" -n 1 "$turn_file")
  printf '%s' "$((turn_n + 1))" > "$turn_file"
  printf '\036%s\036' "$turn_n" >> "$FAKE_AGENT_PROMPT_LOG"
  if [ -n "$sys_prompt_file" ]; then
    cat "$sys_prompt_file" >> "$FAKE_AGENT_PROMPT_LOG"
  elif [ "$sys_prompt_set" = true ]; then
    printf '%s' "$sys_prompt" >> "$FAKE_AGENT_PROMPT_LOG"
  elif [ -n "$codex_dev_instructions" ]; then
    printf '%s' "$codex_dev_instructions" >> "$FAKE_AGENT_PROMPT_LOG"
  fi
fi

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

# Issue #318: one more assistant turn whose final text carries a fenced
# json block -- written directly (not through emit_turn, whose %s
# substitution cannot safely carry embedded quotes/backticks/newlines) so
# the OUTPUT CONTRACT extraction/validation tests have a real transcript to
# read a candidate out of.
case "$mode" in
  contract-ok)
    printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"All done.\n\n```json\n{\"status\": \"done\"}\n```"}]}}' >> "$t"
    ;;
  contract-bad)
    printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"All done.\n\n```json\n{\"status\": \"bogus\"}\n```"}]}}' >> "$t"
    ;;
esac

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
