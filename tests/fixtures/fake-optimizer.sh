#!/bin/sh
# Stands in for the judgment model call during optimize tests.
#   FAKE_OPTIMIZER_MODE=good|garbage|fail|hang   (default good)
# good     two well-formed findings, one with a unified diff
# garbage  prose with no findings
# fail     non-zero exit
# hang     never exits, for the timeout path
set -eu
prompt=$(cat)
[ -z "${FAKE_OPTIMIZER_PROMPT_LOG:-}" ] || printf '%s' "$prompt" > "$FAKE_OPTIMIZER_PROMPT_LOG"

case "${FAKE_OPTIMIZER_MODE:-good}" in
  fail) exit 4 ;;
  hang) while true; do sleep 1; done ;;
  garbage) printf 'Everything looks fine to me.\n' ;;
  *)
    repo_claude=$(printf '%s\n' "$prompt" | sed -n 's/^## \(.*\) (repo CLAUDE.md)$/\1/p' | sed -n '1p')
    global_claude=$(printf '%s\n' "$prompt" | sed -n 's/^## \(.*\) (global CLAUDE.md)$/\1/p' | sed -n '1p')
    user_settings=$(printf '%s\n' "$prompt" | sed -n 's/^## \(.*\) (user settings.json)$/\1/p' | sed -n '1p')
    [ -n "$repo_claude" ] || repo_claude=/repo/CLAUDE.md
    [ -n "$global_claude" ] || global_claude=/home/CLAUDE.md
    [ -n "$user_settings" ] || user_settings=/home/.claude/settings.json
    printf '### FINDING\n'
    printf 'kind: contradiction\n'
    printf 'severity: high\n'
    printf 'title: Commit message rules disagree between layers\n'
    if [ -n "$prompt" ]; then
      printf 'evidence: %s:2, %s:2\n' "$repo_claude" "$global_claude"
    else
      printf 'evidence: %s:4, %s:2\n' "$repo_claude" "$global_claude"
    fi
    printf 'detail: The repo file requires a scope and the global file forbids one.\n'
    printf 'diff:\n'
    printf '```diff\n'
    printf -- '--- a/repo/CLAUDE.md\n'
    printf -- '+++ b/repo/CLAUDE.md\n'
    printf '@@ -4,1 +4,1 @@\n'
    printf -- '-- commit messages must have a scope\n'
    printf '+- commit messages follow the global rule in ~/CLAUDE.md\n'
    printf '```\n'
    printf '\n'
    printf '### FINDING\n'
    printf 'kind: contradiction\n'
    printf 'severity: warning\n'
    printf 'title: A hook contradicts a written instruction\n'
    printf 'evidence: %s:1\n' "$user_settings"
    printf 'detail: The Stop hook blocks while the instructions promise it never does.\n'
    ;;
esac
