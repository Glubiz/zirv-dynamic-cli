#!/bin/sh
# Stands in for an `[objective] gates` command (issue #314). `run_loop`'s
# own gate runner replaces the literal `zirv` word in a configured gate
# command with the resolved gate binary (`ZIRV_CTX_OBJECTIVE_GATE_BIN` in
# tests, `std::env::current_exe()` in production) and passes the rest of
# the configured words through as argv, which this stub ignores -- it never
# runs a real check, it just answers deterministically from the
# environment:
#   FAKE_GATE_EXIT=<n>   exit code (default 0)
#   FAKE_GATE_LOG=<path> appends one line per invocation, so a test can
#                        assert how many times the gate actually ran (the
#                        #287 unchanged-workspace skip means it should not
#                        run again once a matching failure is on record)
set -eu
[ -z "${FAKE_GATE_LOG:-}" ] || printf 'run\n' >> "$FAKE_GATE_LOG"
printf 'fake gate stdout\n'
printf 'fake gate stderr\n' >&2
exit "${FAKE_GATE_EXIT:-0}"
