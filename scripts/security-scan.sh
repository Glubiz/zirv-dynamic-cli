#!/usr/bin/env bash
# Advisory security-scan (issue #429): greps ADDED diff lines for a short
# list of higher-risk patterns, and flags any touched file that appears on
# SECURITY.md's "Critical files" list. Advisory only: see ci.yaml's
# `security-scan` job (PR-only, `continue-on-error: true`) -- this script
# itself also always exits 0 outside `--self-test`, so it can never fail a
# build on its own even if that job configuration ever changes.
#
# Patterns (added lines in *.rs files only):
#   - Command::new("sh"|"bash"|"cmd"|"powershell"|"pwsh")
#   - unsafe
#   - .unwrap() / .expect( -- APPROXIMATED as "outside test code": a hit
#     only counts when the line lands at or above the file's first
#     `#[cfg(test)]` line (as it stands at HEAD). This is not real Rust
#     scope analysis -- a `#[cfg(test)]` anywhere later in the file (even
#     one unrelated to the hunk) silences everything below it, and a stray
#     doc example inside a doc comment above the marker would still count.
#     Good enough for an advisory nudge, not a substitute for review.
#   - reqwest | ureq | TcpStream | TcpListener
#
# Usage:
#   security-scan.sh [--base <ref>]   # default base: HEAD~1
#   security-scan.sh --self-test
#
# Prints a short Markdown-ish report to stdout -- nothing at all when there
# are no hits and no touched critical file -- suitable for appending to
# $GITHUB_STEP_SUMMARY. Exit code is always 0 outside --self-test.

set -euo pipefail

self_test=0
base_ref="HEAD~1"

while [ $# -gt 0 ]; do
  case "$1" in
    --self-test)
      self_test=1
      shift
      ;;
    --base)
      if [ $# -lt 2 ]; then
        echo "usage: $0 [--base <ref>] [--self-test]" >&2
        exit 2
      fi
      base_ref="$2"
      shift 2
      ;;
    *)
      echo "usage: $0 [--base <ref>] [--self-test]" >&2
      exit 2
      ;;
  esac
done

# ---------------------------------------------------------------------
# Reads SECURITY.md's "## Critical files" fenced list -- lines shaped
# `path: reason` -- and prints just the paths, one per line. The scan and
# the doc read the SAME list, so they cannot drift.
# ---------------------------------------------------------------------
critical_files_from_security_md() {
  security_md="$1"
  [ -f "$security_md" ] || return 0
  awk '
    /^## Critical files/ { in_section = 1; next }
    in_section && /^## / { in_section = 0 }
    in_section && /^```/ { in_fence = !in_fence; next }
    in_section && in_fence && NF { print }
  ' "$security_md" \
    | sed -E 's/^[[:space:]]*-?[[:space:]]*//' \
    | sed -E 's/:.*$//' \
    | sed -E 's/[[:space:]]+$//'
}

# Whether $1 (a repo-relative changed-file path) matches any pattern in the
# newline-separated list on stdin. A pattern ending in `*` matches anything
# under that prefix (`src/commands/ctx/adapters/*` matches every file
# directly, and any file nested, under that directory); anything else must
# match exactly.
file_matches_any() {
  file="$1"
  while IFS= read -r pattern; do
    [ -z "$pattern" ] && continue
    case "$pattern" in
      */\*)
        prefix="${pattern%\*}"
        case "$file" in
          "$prefix"*) echo "$pattern"; return 0 ;;
        esac
        ;;
      *)
        if [ "$file" = "$pattern" ]; then
          echo "$pattern"
          return 0
        fi
        ;;
    esac
  done
  return 1
}

# Prints "NEWLINE<TAB>CONTENT" for every ADDED line (never a `+++` header)
# in a `--unified=0` diff read from stdin, tracking the new-file line
# number from each hunk's `@@ -a,b +c,d @@` header.
added_lines_with_numbers() {
  awk '
    /^@@/ {
      line = $0
      sub(/^@@ -[0-9]+(,[0-9]+)? \+/, "", line)
      split(line, parts, /[ ,]/)
      newline = parts[1] + 0
      next
    }
    /^\+\+\+/ { next }
    /^\+/ {
      print newline "\t" substr($0, 2)
      newline++
      next
    }
    { next }
  '
}

# ---------------------------------------------------------------------
# Core scan. Assumes CWD is the repo root. Prints the report; caller
# decides what to do with the exit code (always successful -- advisory).
# ---------------------------------------------------------------------
run_scan() {
  base="$1"

  changed_files="$(git diff --name-only --diff-filter=AM "${base}...HEAD" 2>/dev/null || true)"
  if [ -z "$changed_files" ]; then
    return 0
  fi

  critical_list="$(critical_files_from_security_md "SECURITY.md")"

  pattern_hits=""
  critical_hits=""

  while IFS= read -r file; do
    [ -z "$file" ] && continue

    matched_pattern="$(printf '%s' "$critical_list" | file_matches_any "$file" || true)"
    if [ -n "$matched_pattern" ]; then
      critical_hits="${critical_hits}- \`$file\` (matches critical-file entry \`$matched_pattern\`)\n"
    fi

    case "$file" in
      *.rs) : ;;
      *) continue ;;
    esac

    cfg_test_line=0
    if [ -f "$file" ]; then
      cfg_test_line="$(grep -n '#\[cfg(test)\]' "$file" 2>/dev/null | head -n1 | cut -d: -f1 || true)"
      [ -z "$cfg_test_line" ] && cfg_test_line=0
    fi

    while IFS=$'\t' read -r lineno content; do
      [ -z "${lineno:-}" ] && continue

      if printf '%s' "$content" | grep -Eq 'Command::new\(("|'"'"')(sh|bash|cmd|powershell|pwsh)("|'"'"')'; then
        pattern_hits="${pattern_hits}- \`$file:$lineno\` spawns a shell/interpreter directly: \`$(printf '%s' "$content" | sed -E 's/^[[:space:]]+//')\`\n"
      fi
      if printf '%s' "$content" | grep -Eq '(^|[^a-zA-Z_])unsafe([^a-zA-Z_]|$)'; then
        pattern_hits="${pattern_hits}- \`$file:$lineno\` introduces \`unsafe\`: \`$(printf '%s' "$content" | sed -E 's/^[[:space:]]+//')\`\n"
      fi
      if printf '%s' "$content" | grep -Eq '\.(unwrap\(\)|expect\()'; then
        if [ "$cfg_test_line" -eq 0 ] || [ "$lineno" -lt "$cfg_test_line" ]; then
          pattern_hits="${pattern_hits}- \`$file:$lineno\` \`.unwrap()\`/\`.expect(\` outside the file's test module (approximate -- see this script's header): \`$(printf '%s' "$content" | sed -E 's/^[[:space:]]+//')\`\n"
        fi
      fi
      if printf '%s' "$content" | grep -Eq '\b(reqwest|ureq|TcpStream|TcpListener)\b'; then
        pattern_hits="${pattern_hits}- \`$file:$lineno\` touches network I/O (\`reqwest\`/\`ureq\`/\`TcpStream\`/\`TcpListener\`): \`$(printf '%s' "$content" | sed -E 's/^[[:space:]]+//')\`\n"
      fi
    done < <(git diff --unified=0 "${base}...HEAD" -- "$file" 2>/dev/null | added_lines_with_numbers)
  done <<EOF
$changed_files
EOF

  if [ -z "$pattern_hits" ] && [ -z "$critical_hits" ]; then
    return 0
  fi

  echo "### Advisory security scan"
  echo ""
  echo "This is a pattern-matching nudge, not a verdict -- review each line,"
  echo "then decide. Nothing here blocks the merge."
  echo ""
  if [ -n "$critical_hits" ]; then
    echo "**Touched critical files** (see SECURITY.md):"
    echo ""
    printf '%b' "$critical_hits"
    echo ""
  fi
  if [ -n "$pattern_hits" ]; then
    echo "**Pattern hits on added lines:**"
    echo ""
    printf '%b' "$pattern_hits"
    echo ""
  fi
  echo "**Manual-review checklist:**"
  echo ""
  echo "- [ ] Any new subprocess/shell spawn uses a fixed argv, never interpolated user/repo text into a shell string"
  echo "- [ ] Any new \`unsafe\` is justified in a comment and as small as possible"
  echo "- [ ] Any new \`.unwrap()\`/\`.expect(\` outside tests cannot be reached on a hot path with untrusted input"
  echo "- [ ] Any new network call has a bounded timeout and does not leak secrets in its request"
  return 0
}

# ---------------------------------------------------------------------
# --self-test
# ---------------------------------------------------------------------
run_self_test() {
  script_path="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"
  work="$(mktemp -d)"
  trap 'rm -rf "$work"' EXIT
  fails=0

  make_repo() {
    name="$1"
    dir="$work/$name"
    mkdir -p "$dir"
    ( cd "$dir" \
      && git init -q \
      && git config user.email "test@example.com" \
      && git config user.name "Test User" )
    printf '%s' "$dir"
  }

  commit_all() {
    dir="$1"; msg="$2"
    ( cd "$dir" && git add -A && git commit -q -m "$msg" )
  }

  base_security_md() {
    cat <<'EOF'
# Security policy

## Critical files

```text
src/commands/ctx/wrap.rs: spawns and supervises the PTY-wrapped child process
src/commands/ctx/adapters/*: construct the exact argv handed to each harness
```
EOF
  }

  check_scenario() {
    name="$1"; dir="$2"; must_contain="$3"; must_not_contain="$4"
    set +e
    out="$( cd "$dir" && "$script_path" --base HEAD~1 )"
    code=$?
    set -e
    ok=1
    if [ "$code" -ne 0 ]; then
      ok=0
    fi
    if [ -n "$must_contain" ] && ! printf '%s' "$out" | grep -Fq "$must_contain"; then
      ok=0
    fi
    if [ -n "$must_not_contain" ] && printf '%s' "$out" | grep -Fq "$must_not_contain"; then
      ok=0
    fi
    if [ "$ok" -eq 1 ]; then
      echo "self-test ok: $name"
    else
      echo "self-test FAILED: $name (exit $code)"
      printf '%s\n' "$out" | sed 's/^/    /'
      fails=$((fails + 1))
    fi
  }

  # 1. A shell spawn on an added line -> flagged.
  dir="$(make_repo shell-spawn)"
  base_security_md >"$dir/SECURITY.md"
  mkdir -p "$dir/src/commands"
  echo 'pub fn noop() {}' >"$dir/src/commands/thing.rs"
  commit_all "$dir" "base"
  cat >"$dir/src/commands/thing.rs" <<'EOF'
pub fn noop() {}

pub fn run_shell() {
    std::process::Command::new("bash").arg("-c").spawn().unwrap();
}
EOF
  commit_all "$dir" "spawn a shell"
  check_scenario "shell-spawn" "$dir" "spawns a shell/interpreter directly" ""

  # 2. .unwrap() above any #[cfg(test)] marker -> flagged.
  dir="$(make_repo unwrap-outside-tests)"
  base_security_md >"$dir/SECURITY.md"
  mkdir -p "$dir/src/commands"
  cat >"$dir/src/commands/thing.rs" <<'EOF'
pub fn noop() {}

#[cfg(test)]
mod tests {
    #[test]
    fn ok() {}
}
EOF
  commit_all "$dir" "base"
  cat >"$dir/src/commands/thing.rs" <<'EOF'
pub fn noop() {}

pub fn risky(v: Option<i32>) -> i32 {
    v.unwrap()
}

#[cfg(test)]
mod tests {
    #[test]
    fn ok() {}
}
EOF
  commit_all "$dir" "add an unwrap outside tests"
  check_scenario "unwrap-outside-tests" "$dir" ".unwrap()" ""

  # 3. .unwrap() ADDED inside an existing #[cfg(test)] module -> NOT flagged.
  dir="$(make_repo unwrap-inside-tests)"
  base_security_md >"$dir/SECURITY.md"
  mkdir -p "$dir/src/commands"
  cat >"$dir/src/commands/thing.rs" <<'EOF'
pub fn noop() {}

#[cfg(test)]
mod tests {
    #[test]
    fn ok() {}
}
EOF
  commit_all "$dir" "base"
  cat >"$dir/src/commands/thing.rs" <<'EOF'
pub fn noop() {}

#[cfg(test)]
mod tests {
    #[test]
    fn ok() {}

    #[test]
    fn another() {
        let v: Option<i32> = Some(1);
        v.unwrap();
    }
}
EOF
  commit_all "$dir" "add a test that unwraps"
  check_scenario "unwrap-inside-tests" "$dir" "" ".unwrap()"

  # 4. Touching a critical-list file -> flagged, even with no pattern hit.
  dir="$(make_repo critical-file-touch)"
  base_security_md >"$dir/SECURITY.md"
  mkdir -p "$dir/src/commands/ctx"
  echo 'pub fn spawn() {}' >"$dir/src/commands/ctx/wrap.rs"
  commit_all "$dir" "base"
  echo 'pub fn spawn() { /* tweak */ }' >"$dir/src/commands/ctx/wrap.rs"
  commit_all "$dir" "tweak wrap.rs"
  check_scenario "critical-file-touch" "$dir" "critical-file entry" ""

  # 5. A clean diff -> no output at all.
  dir="$(make_repo clean-diff)"
  base_security_md >"$dir/SECURITY.md"
  mkdir -p "$dir/src/commands"
  echo 'pub fn noop() {}' >"$dir/src/commands/thing.rs"
  commit_all "$dir" "base"
  cat >"$dir/src/commands/thing.rs" <<'EOF'
pub fn noop() {}

pub fn add(a: i32, b: i32) -> i32 {
    a + b
}
EOF
  commit_all "$dir" "ordinary change, nothing risky"
  set +e
  out="$( cd "$dir" && "$script_path" --base HEAD~1 )"
  code=$?
  set -e
  if [ "$code" -eq 0 ] && [ -z "$out" ]; then
    echo "self-test ok: clean-diff"
  else
    echo "self-test FAILED: clean-diff (exit $code, output: $out)"
    fails=$((fails + 1))
  fi

  if [ "$fails" -gt 0 ]; then
    echo "security-scan.sh --self-test: $fails scenario(s) failed" >&2
    return 1
  fi
  echo "security-scan.sh --self-test: all scenarios passed"
  return 0
}

if [ "$self_test" -eq 1 ]; then
  run_self_test
  exit $?
fi

repo_root="$(git rev-parse --show-toplevel 2>/dev/null)" || {
  echo "security-scan.sh: not inside a git repository" >&2
  exit 0
}
cd "$repo_root"
run_scan "$base_ref"
exit 0
