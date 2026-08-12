#!/usr/bin/env bash
# PreToolUse hook: on `git push`, warn (advisory only) when source changes
# under tracked areas aren't accompanied by the matching Obsidian doc page
# in the same diff. Never blocks — only ever "allow" with optional
# additionalContext. Must degrade to allow on any parse/lookup failure and
# must work in Git Bash on Windows without requiring jq.

set -u

allow() {
  printf '{"decision":"allow"}\n'
  exit 0
}

payload="$(cat)"

# Extract tool_input.command without jq. This is a best-effort, POSIX-safe
# scrape: find the first "command":"..." occurrence in the payload. Real
# PreToolUse payloads for the Bash tool put tool_input.command first, before
# any other "command"-named field, so this is reliable in practice; if it
# fails for any reason we degrade to allow rather than guess.
command_line="$(printf '%s' "$payload" | grep -o '"command"[[:space:]]*:[[:space:]]*"[^"]*"' | head -n 1 | sed -E 's/^"command"[[:space:]]*:[[:space:]]*"//; s/"$//')"

if [ -z "$command_line" ]; then
  allow
fi

case "$command_line" in
  *"git push"*) ;;
  *) allow ;;
esac

# Not in a git repo, or git unavailable: nothing we can check.
if ! git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  allow
fi

base_ref=""
if git rev-parse --verify -q origin/main >/dev/null 2>&1; then
  base_ref="origin/main"
elif git rev-parse --verify -q main >/dev/null 2>&1; then
  base_ref="main"
else
  allow
fi

changed_files="$(git diff "${base_ref}...HEAD" --name-only 2>/dev/null)"
if [ -z "$changed_files" ]; then
  allow
fi

# pattern<TAB>doc page pairs. A tab is used as the delimiter (not "|")
# because several patterns contain "|" themselves for regex alternation.
pairs="$(printf '%s\t%s\n' \
  '^src/script_runner/' 'docs/obsidian/Modules/Script Runner.md' \
  '^src/commands/ctx/adapters/' 'docs/obsidian/Modules/Ctx Adapters.md' \
  '^src/commands/ctx/(rot|event|score)\.rs' 'docs/obsidian/Modules/Rot Engine.md' \
  '^src/commands/ctx/(run_loop|exec|wrap|signal|supervise|term)\.rs' 'docs/obsidian/Modules/Ctx Supervisors.md' \
  '^src/commands/ctx/(pace|usage|window)\.rs' 'docs/obsidian/Modules/Usage and Pacing.md' \
  '^src/(main|input)\.rs' 'docs/obsidian/Modules/Built-in Commands.md' \
  '^Cargo\.toml' 'docs/obsidian/Architecture/Technology Stack.md')"

missing=""

old_ifs="$IFS"
IFS='
'
for pair in $pairs; do
  pattern="$(printf '%s' "$pair" | cut -f1)"
  page="$(printf '%s' "$pair" | cut -f2)"

  if printf '%s\n' "$changed_files" | grep -Eq "$pattern"; then
    if ! printf '%s\n' "$changed_files" | grep -Fxq "$page"; then
      missing="${missing}${pattern} -> ${page}\n"
    fi
  fi
done
IFS="$old_ifs"

if [ -z "$missing" ]; then
  allow
fi

# Note: $(...) strips trailing newlines from command substitution, so the
# separator before the closing message is added explicitly rather than
# relying on the trailing "\n" already in $missing.
context="$(printf '%b' "$missing")
Update the relevant Obsidian pages before pushing, or verify the docs are still accurate."
# Escape backslashes, double quotes, and newlines for JSON string embedding.
context_json="$(printf '%s' "$context" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' | awk '{printf "%s\\n", $0}')"
# Strip trailing literal \n added by the loop above.
context_json="${context_json%\\n}"

printf '{"hookSpecificOutput":{"hookEventName":"PreToolUse","additionalContext":"%s"}}\n' "$context_json"
exit 0
