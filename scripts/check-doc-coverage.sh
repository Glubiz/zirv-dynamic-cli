#!/usr/bin/env bash
# PreToolUse hook: on `git push`, checks whether source changes under
# tracked areas are accompanied by the matching Obsidian doc page in the
# same diff.
#
# Output contract (PreToolUse only documents "allow"/"deny"/"ask" via
# hookSpecificOutput.permissionDecision; additionalContext is a documented
# no-op on this hook event, see anthropics/claude-code#19432):
#   - Nothing missing, non-push command, or any parse/git failure: print
#     NOTHING and exit 0. Absence of output is the documented default-allow
#     behavior; there is no "allow" JSON form to emit.
#   - Missing docs, first time seen for this (HEAD, missing-set) state:
#     emit a `permissionDecision: deny` with the pairs list in
#     `permissionDecisionReason`, so Claude sees the reason and can fix the
#     docs and retry. This is advisory rather than a hard gate because of
#     the warn-once cache below: the same (HEAD, missing-set) never denies
#     twice, so a deliberate "push anyway" always succeeds on retry.
#   - Same (HEAD, missing-set) seen before (per the on-disk warn-once
#     cache): print nothing and exit 0 — the warning already fired.
#
# Must work in Git Bash on Windows without requiring jq.

set -u

payload="$(cat)"

# Extract tool_input.command without jq. This is a best-effort, POSIX-safe
# scrape: find the first "command":"..." occurrence in the payload. Real
# PreToolUse payloads for the Bash tool put tool_input.command first, before
# any other "command"-named field, so this is reliable in practice; if it
# fails for any reason we degrade to allow (print nothing) rather than guess.
command_line="$(printf '%s' "$payload" | grep -o '"command"[[:space:]]*:[[:space:]]*"[^"]*"' | head -n 1 | sed -E 's/^"command"[[:space:]]*:[[:space:]]*"//; s/"$//')"

if [ -z "$command_line" ]; then
  exit 0
fi

# Token-boundary match for a `git push` invocation: "git" and "push" as
# standalone words separated by whitespace, with "git" preceded by a command
# boundary (start of string, `;`, `&`, `|`, or whitespace) and "push"
# followed by whitespace or end of string. This can still match `git push`
# appearing mid-sentence inside an unrelated string argument (e.g.
# `echo please run git push now`) because the matcher only knows about token
# boundaries, not shell quoting/semantics — the tradeoff is a rare extra
# check on a lookalike command, over a missed real push. Never fails closed
# either way.
if ! printf '%s' "$command_line" | grep -Eq '(^|[;&|[:space:]])git[[:space:]]+push([[:space:]]|$)'; then
  exit 0
fi

# Hook cwd is not guaranteed to be the project root; anchor to it when the
# harness tells us where it is, and fail open if that directory is bad.
# Manual/CI runs (no CLAUDE_PROJECT_DIR set) just use the current directory.
if [ -n "${CLAUDE_PROJECT_DIR:-}" ]; then
  if ! cd "$CLAUDE_PROJECT_DIR" 2>/dev/null; then
    exit 0
  fi
fi

# Not in a git repo, or git unavailable: nothing we can check.
if ! git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  exit 0
fi

base_ref=""
if git rev-parse --verify -q origin/main >/dev/null 2>&1; then
  base_ref="origin/main"
elif git rev-parse --verify -q main >/dev/null 2>&1; then
  base_ref="main"
else
  exit 0
fi

changed_files="$(git diff "${base_ref}...HEAD" --name-only 2>/dev/null)"
if [ -z "$changed_files" ]; then
  exit 0
fi

# pattern<TAB>doc page pairs. A tab is used as the delimiter (not "|")
# because several patterns contain "|" themselves for regex alternation.
# Mirrors the trigger table in this repo's CLAUDE.md under "Obsidian
# Documentation Updates".
pairs="$(printf '%s\t%s\n' \
  '^src/script_runner/' 'docs/obsidian/Modules/Script Runner.md' \
  '^src/commands/ctx/adapters/' 'docs/obsidian/Modules/Ctx Adapters.md' \
  '^src/settings\.rs' 'docs/obsidian/Modules/Ctx Adapters.md' \
  '^src/commands/ctx/(rot|event|score)\.rs' 'docs/obsidian/Modules/Rot Engine.md' \
  '^src/commands/ctx/(run_loop|exec|wrap|signal|supervise|term)\.rs' 'docs/obsidian/Modules/Ctx Supervisors.md' \
  '^src/commands/ctx/(pace|usage|window)\.rs' 'docs/obsidian/Modules/Usage and Pacing.md' \
  '^src/(main|input)\.rs' 'docs/obsidian/Modules/Built-in Commands.md' \
  '^Cargo\.toml' 'docs/obsidian/Architecture/Technology Stack.md' \
  '^src/commands/ctx/(mod|config|state|log|handoff|resume|hook|status)\.rs' 'docs/obsidian/Modules/Ctx Subsystem.md' \
  '^src/commands/(mod|create|init|help|version)\.rs' 'docs/obsidian/Modules/Built-in Commands.md' \
  '^src/utils\.rs' 'docs/obsidian/Modules/Utilities.md' \
  '^src/commands/ctx/(optimize|prompt)\.rs' 'docs/obsidian/Modules/Utilities.md' \
  '^src/commands/ctx/(chat|agent|mail)\.rs' 'docs/obsidian/Modules/Ctx Subsystem.md' \
  '^src/commands/ctx/(chrome|announce)\.rs' 'docs/obsidian/Modules/Ctx Supervisors.md' \
  '^src/commands/ctx/sessions\.rs' 'docs/obsidian/Modules/Ctx Subsystem.md' \
  '^src/commands/ctx/memory\.rs' 'docs/obsidian/Modules/Ctx Subsystem.md')"

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
  exit 0
fi

# --- Warn-once cache ---------------------------------------------------
# Deny is advisory, not a permanent gate: the same (HEAD, missing-set)
# state only denies once. Key it on a stable hash of the current HEAD
# commit plus the sorted missing-pairs list, stored under the repo's git
# dir (never committed, machine-local). Any failure anywhere in this
# section fails open (prints nothing, allows) rather than denying — a
# broken cache must never turn into a permanent block.
gitdir="$(git rev-parse --git-dir 2>/dev/null)"
if [ -z "$gitdir" ]; then
  exit 0
fi

head_sha="$(git rev-parse HEAD 2>/dev/null)"
if [ -z "$head_sha" ]; then
  exit 0
fi

sorted_missing="$(printf '%b' "$missing" | sort)"
hash_input="${head_sha}
${sorted_missing}"

new_hash="$(printf '%s' "$hash_input" | git hash-object --stdin 2>/dev/null)"
if [ -z "$new_hash" ]; then
  exit 0
fi

cache_file="$gitdir/zirv-doc-coverage-warned"
old_hash=""
if [ -f "$cache_file" ]; then
  if ! old_hash="$(cat "$cache_file" 2>/dev/null)"; then
    exit 0
  fi
fi

if [ -n "$old_hash" ] && [ "$old_hash" = "$new_hash" ]; then
  # Already warned for this exact state: allow silently.
  exit 0
fi

if ! printf '%s' "$new_hash" > "$cache_file" 2>/dev/null; then
  exit 0
fi

# --- Emit the deny -------------------------------------------------------
# Note: $(...) strips trailing newlines from command substitution, so the
# separator before the closing message is added explicitly rather than
# relying on the trailing "\n" already in $missing.
reason="$(printf '%b' "$missing")
Update the relevant Obsidian pages before pushing, or verify the docs are still accurate, then push again (this warning fires once per diff state)."
# Escape backslashes, double quotes, and newlines for JSON string embedding.
reason_json="$(printf '%s' "$reason" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' | awk '{printf "%s\\n", $0}')"
# Strip trailing literal \n added by the loop above.
reason_json="${reason_json%\\n}"

printf '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"%s"}}\n' "$reason_json"
exit 0
