#!/bin/sh
# Stands in for `claude -p --model haiku` during handoff tests.
# Reads the distillation prompt on stdin and answers per FAKE_MODEL_MODE:
#   good    (default) a well-formed handoff
#   partial a handoff with no next step, so callers must fall back
#   garbage prose with no sections
#   fail    non-zero exit
#   hang    reads the prompt and then never answers, the way a wedged model
#           call looks from the outside
#   echo    dumps the prompt it received to $FAKE_MODEL_PROMPT_LOG and answers good
#   flood   writes past a pipe buffer's worth of output *before* reading any
#           of stdin, the way a model that starts answering before the
#           caller has finished sending the prompt looks from the outside
#   harvest a well-formed set of `key: body` durable-fact lines, for
#           memory::harvest_from_handoff tests
#   consolidate  echoes back exactly one `key: body` line, using the
#           survivor key named in the prompt's "(KEEP THIS KEY)" marker,
#           for memory_optimize::apply_consolidation tests (issue #38)
set -eu

head_bin=head
sleep_bin=sleep
[ ! -x /usr/bin/head ] || head_bin=/usr/bin/head
[ ! -x /usr/bin/sleep ] || sleep_bin=/usr/bin/sleep
[ ! -x /bin/sleep ] || sleep_bin=/bin/sleep

case "${FAKE_MODEL_MODE:-good}" in
  flood)
    # Deliberately does not drain stdin first: a caller whose write and read
    # sides are not serviced concurrently can deadlock here, blocked writing
    # a stdin pipe this process has not started reading while this process
    # is blocked writing a stdout pipe the caller has not started draining.
    chunk=xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
    chunk_count=0
    while [ "$chunk_count" -lt 4096 ]; do
      printf '%s' "$chunk"
      chunk_count=$((chunk_count + 1))
    done
    cat >/dev/null
    exit 0
    ;;
esac

prompt=$(cat)
[ -z "${FAKE_MODEL_PROMPT_LOG:-}" ] || printf '%s' "$prompt" > "$FAKE_MODEL_PROMPT_LOG"

case "${FAKE_MODEL_MODE:-good}" in
  fail) exit 4 ;;
  hang) while true; do "$sleep_bin" 1; done ;;
  garbage) printf 'I had a look and things seem mostly fine.\n' ;;
  harvest)
    printf 'build-cmd: cargo build --release\n'
    printf 'staging-db-creds: staging DB creds live in 1Password under staging-db\n'
    ;;
  consolidate)
    key=$(printf '%s\n' "$prompt" | sed -n 's/^- \(.*\) (KEEP THIS KEY):.*/\1/p' | "$head_bin" -n1)
    printf '%s: merged body from the fake consolidation model\n' "$key"
    ;;
  partial)
    printf '## Task\nShip the webhook\n\n## Done\n- wrote the route\n'
    ;;
  *)
    printf '## Task\nShip the webhook\n\n'
    printf '## Done\n- wrote the route\n- wrote the parser\n\n'
    printf '## Remaining\n- signature verification\n\n'
    printf '## Next step\nAdd a failing test for an invalid signature\n\n'
    printf '## Files touched\n- src/routes/webhook.rs\n\n'
    printf '## Gotchas learned\n- the provider sends two events per charge\n'
    ;;
esac
