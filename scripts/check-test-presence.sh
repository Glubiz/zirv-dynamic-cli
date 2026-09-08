#!/usr/bin/env bash
# Test-presence gate (issue #428): every `src/commands/**/*.rs` and
# `src/script_runner/**/*.rs` file that a diff adds or modifies must carry
# an inline `#[cfg(test)] mod tests` block, per this repo's CLAUDE.md
# ("Tests stay inline"). Three kinds of file are exempt:
#
#   - `mod.rs` (a pure re-export/glue file, nothing of its own to test);
#   - anything under `src/commands/ctx/schemas/` (generated/declarative
#     schema data, not logic);
#   - a file whose diff hunks touch ONLY comment/doc lines (`//`, `///`,
#     `//!` after trimming) or blank lines -- a change that could not
#     possibly need a new test.
#
# Usage:
#   check-test-presence.sh [--base <ref>]   # default base: HEAD~1
#   check-test-presence.sh --self-test      # prove pass/fail both ways
#
# Exits 1 and lists every offending file when the gate fails; exits 0
# (silently, beyond a short summary) when everything checked-in is exempt or
# already carries `#[cfg(test)]`.

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
# The gate itself. Assumes CWD is the root of the git repo to check.
# ---------------------------------------------------------------------
run_presence_gate() {
  base="$1"

  changed_files="$(git diff --name-only --diff-filter=AM "${base}...HEAD" 2>/dev/null || true)"
  if [ -z "$changed_files" ]; then
    return 0
  fi

  candidates="$(printf '%s\n' "$changed_files" | grep -E '^(src/commands/|src/script_runner/).*\.rs$' || true)"
  if [ -z "$candidates" ]; then
    return 0
  fi

  missing=""

  old_ifs="$IFS"
  IFS='
'
  for file in $candidates; do
    IFS="$old_ifs"

    base_name="$(basename "$file")"
    if [ "$base_name" = "mod.rs" ]; then
      continue
    fi
    case "$file" in
      src/commands/ctx/schemas/*) continue ;;
    esac

    # Comment/doc-only exemption: every changed line (added or removed),
    # trimmed, is either blank or starts with `//` (covers `//`, `///` and
    # `//!` alike). A file with no content hunks at all (a pure rename or
    # mode change) also counts as exempt here -- there is no code change to
    # need a test for.
    changed_lines="$(git diff --unified=0 "${base}...HEAD" -- "$file" 2>/dev/null \
      | grep -E '^[+-]' \
      | grep -Ev '^(\+\+\+|---)' \
      | sed -E 's/^[+-]//' \
      | sed -E 's/^[[:space:]]+//' || true)"
    non_comment_lines="$(printf '%s\n' "$changed_lines" | grep -Ev '^$' | grep -Ev '^//' || true)"
    if [ -z "$non_comment_lines" ]; then
      continue
    fi

    # Present at HEAD (added/modified files always exist at HEAD under
    # --diff-filter=AM).
    if ! git show "HEAD:${file}" 2>/dev/null | grep -Fq '#[cfg(test)]'; then
      missing="${missing}${file}\n"
    fi

    IFS='
'
  done
  IFS="$old_ifs"

  if [ -n "$missing" ]; then
    echo "test-presence gate: missing #[cfg(test)] in:" >&2
    printf '%b' "$missing" | sed '/^$/d' | sed 's/^/  - /' >&2
    return 1
  fi
  return 0
}

# ---------------------------------------------------------------------
# --self-test: builds throwaway repos in a temp dir and proves the gate
# fails when it should and passes when it should, by invoking THIS script
# end to end against each of them.
# ---------------------------------------------------------------------
run_self_test() {
  script_path="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"
  work="$(mktemp -d)"
  trap 'rm -rf "$work"' EXIT

  fails=0

  # scenario NAME EXPECTED_EXIT SETUP_FN
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
    dir="$1"
    msg="$2"
    ( cd "$dir" && git add -A && git commit -q -m "$msg" )
  }

  check_scenario() {
    name="$1"
    dir="$2"
    expect="$3"
    set +e
    ( cd "$dir" && "$script_path" --base HEAD~1 ) >"$work/$name.out" 2>&1
    actual=$?
    set -e
    if [ "$actual" -eq "$expect" ]; then
      echo "self-test ok: $name (exit $actual)"
    else
      echo "self-test FAILED: $name (expected exit $expect, got $actual)"
      sed 's/^/    /' "$work/$name.out"
      fails=$((fails + 1))
    fi
  }

  # 1. A new commands/** file with no tests -> must FAIL.
  dir="$(make_repo untested-new-file)"
  mkdir -p "$dir/src/commands/widget"
  cat >"$dir/src/commands/widget/mod.rs" <<'EOF'
pub fn noop() {}
EOF
  commit_all "$dir" "base"
  cat >"$dir/src/commands/widget/logic.rs" <<'EOF'
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}
EOF
  commit_all "$dir" "add logic.rs without tests"
  check_scenario "untested-new-file" "$dir" 1

  # 2. Same, but WITH an inline test block -> must PASS.
  dir="$(make_repo tested-new-file)"
  mkdir -p "$dir/src/commands/widget"
  cat >"$dir/src/commands/widget/mod.rs" <<'EOF'
pub fn noop() {}
EOF
  commit_all "$dir" "base"
  cat >"$dir/src/commands/widget/logic.rs" <<'EOF'
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds() {
        assert_eq!(add(1, 2), 3);
    }
}
EOF
  commit_all "$dir" "add logic.rs with tests"
  check_scenario "tested-new-file" "$dir" 0

  # 3. A doc-comment-only change to an already-untested file -> must PASS
  # (exempt: no code line changed).
  dir="$(make_repo doc-only-change)"
  mkdir -p "$dir/src/commands/widget"
  cat >"$dir/src/commands/widget/logic.rs" <<'EOF'
/// Adds two numbers.
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}
EOF
  commit_all "$dir" "base (already untested, pre-existing)"
  cat >"$dir/src/commands/widget/logic.rs" <<'EOF'
/// Adds two numbers together.
//
// Updated wording only, no behaviour change.
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}
EOF
  commit_all "$dir" "doc wording tweak only"
  check_scenario "doc-only-change" "$dir" 0

  # 4. mod.rs is always exempt, even untested.
  dir="$(make_repo modrs-exempt)"
  mkdir -p "$dir/src/commands/widget"
  echo "pub fn noop() {}" >"$dir/src/commands/widget/other.rs"
  commit_all "$dir" "base"
  cat >"$dir/src/commands/widget/mod.rs" <<'EOF'
pub mod other;
pub fn dispatch() {}
EOF
  commit_all "$dir" "add mod.rs without tests"
  check_scenario "modrs-exempt" "$dir" 0

  # 5. src/commands/ctx/schemas/** is always exempt, even untested.
  dir="$(make_repo schemas-exempt)"
  mkdir -p "$dir/src/commands/ctx/schemas"
  echo "pub fn noop() {}" >"$dir/src/commands/placeholder.rs"
  commit_all "$dir" "base"
  cat >"$dir/src/commands/ctx/schemas/shape.rs" <<'EOF'
pub const SCHEMA: &str = "{}";
EOF
  commit_all "$dir" "add schema data without tests"
  check_scenario "schemas-exempt" "$dir" 0

  if [ "$fails" -gt 0 ]; then
    echo "check-test-presence.sh --self-test: $fails scenario(s) failed" >&2
    return 1
  fi
  echo "check-test-presence.sh --self-test: all scenarios passed"
  return 0
}

if [ "$self_test" -eq 1 ]; then
  run_self_test
  exit $?
fi

repo_root="$(git rev-parse --show-toplevel 2>/dev/null)" || {
  echo "check-test-presence.sh: not inside a git repository" >&2
  exit 2
}
cd "$repo_root"
run_presence_gate "$base_ref"
exit $?
