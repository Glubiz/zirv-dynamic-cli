#!/bin/sh
# Stands in for the user's real statusline script during tee tests.
#   FAKE_STATUSLINE_MODE=ok|fail|silent   (default ok)
# ok      echoes a recognizable line built from the JSON it received on stdin
# fail    exits non-zero without printing, so the fallback path is exercised
# silent  exits 0 printing nothing
set -eu
input=$(cat)
case "${FAKE_STATUSLINE_MODE:-ok}" in
  fail) exit 7 ;;
  silent) exit 0 ;;
  *)
    [ -z "${FAKE_STATUSLINE_LOG:-}" ] || printf '%s' "$input" > "$FAKE_STATUSLINE_LOG"
    printf 'CHAINED-OK bytes=%s\n' "$(printf '%s' "$input" | wc -c | tr -d ' ')"
    ;;
esac
