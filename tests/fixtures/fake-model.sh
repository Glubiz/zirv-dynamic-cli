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
set -eu
prompt=$(cat)
[ -z "${FAKE_MODEL_PROMPT_LOG:-}" ] || printf '%s' "$prompt" > "$FAKE_MODEL_PROMPT_LOG"

case "${FAKE_MODEL_MODE:-good}" in
  fail) exit 4 ;;
  hang) while true; do sleep 1; done ;;
  garbage) printf 'I had a look and things seem mostly fine.\n' ;;
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
