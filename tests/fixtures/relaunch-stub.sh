#!/bin/sh
# Minimal interactive stand-in for an agent TUI, used only by the restart/
# relaunch test. Deliberately simpler than stub-tui.sh: that script's
# /compact branch (long JSON string literals inside a case arm) reproducibly
# causes the process spawned on a relaunch's fresh pty to exit immediately
# on this machine, even though the branch is never reached (see
# batch10-report.md for the full bisection). The restart test never sends
# /compact, so this fixture only needs to greet, echo input back so
# passthrough after the relaunch is checkable, and exit on /exit or /quit.
set -eu
printf 'stub-tui ready\n'
while IFS= read -r line; do
  printf 'echo: %s\n' "$line"
  case "$line" in
    /exit|/quit) printf 'bye\n'; exit 0 ;;
    /fail) exit 5 ;;
  esac
done
