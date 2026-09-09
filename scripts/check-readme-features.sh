#!/usr/bin/env bash
# README features coverage gate (issue #407): every depth-1 and depth-2 verb
# `zirv commands --json` reports must appear as a backticked token
# (`` `verb` ``) inside README.md's "## Features" section, so the exhaustive,
# grouped feature list documented there can never silently drift from the
# real command surface the binary actually ships.
#
# "Depth" is counted on a path's words AFTER the leading "zirv ": for
# "zirv ctx hook audit" depth-1 is "ctx" and depth-2 is "hook" -- "audit"
# (depth-3) is NOT required, since `zirv ctx <verb>` is the acceptance
# criterion, not every leaf under it. A leaf path with only one word after
# "zirv" (e.g. "zirv init") contributes only its depth-1 token.
#
# Usage:
#   check-readme-features.sh [--binary <path>] [--readme <path>]
#   check-readme-features.sh --self-test
#
# Exits 1 and lists every undocumented verb when the gate fails; exits 2 if
# the binary can't be run or the README/Features section can't be found;
# exits 0, printing a coverage count, when every verb is documented.

set -euo pipefail

binary="./target/debug/zirv"
readme="README.md"
self_test=0

usage() {
  echo "usage: $0 [--binary <path>] [--readme <path>] [--self-test]" >&2
}

while [ $# -gt 0 ]; do
  case "$1" in
    --binary)
      if [ $# -lt 2 ]; then usage; exit 2; fi
      binary="$2"
      shift 2
      ;;
    --readme)
      if [ $# -lt 2 ]; then usage; exit 2; fi
      readme="$2"
      shift 2
      ;;
    --self-test)
      self_test=1
      shift
      ;;
    *)
      usage
      exit 2
      ;;
  esac
done

# ---------------------------------------------------------------------
# extract_verbs <json-file>: prints the deduplicated, sorted set of
# depth-1/depth-2 verb tokens found across every "path" value in a
# `zirv commands --json` document. Prefers jq (present on the CI image);
# falls back to a grep/sed extraction of the `"path": "..."` fields when
# jq is unavailable, so this still runs on a bare dev machine.
# ---------------------------------------------------------------------
extract_verbs() {
  json_file="$1"
  if command -v jq >/dev/null 2>&1; then
    jq -r '.[].path' "$json_file"
  else
    grep -o '"path"[[:space:]]*:[[:space:]]*"[^"]*"' "$json_file" \
      | sed -E 's/.*:[[:space:]]*"//; s/"$//'
  fi | awk '{ if (NF >= 2) print $2; if (NF >= 3) print $3 }' | sort -u
}

# ---------------------------------------------------------------------
# features_slice <readme-file>: prints the text from the "## Features"
# heading (exclusive of that line, but inclusive of everything after it)
# up to -- but not including -- the next line starting with "## ". Prints
# nothing if no "## Features" heading exists.
# ---------------------------------------------------------------------
features_slice() {
  awk '
    /^## Features$/ { flag=1; next }
    flag && /^## /  { exit }
    flag            { print }
  ' "$1"
}

# ---------------------------------------------------------------------
# check_coverage <verbs-file> <readme-path>: the gate itself. Prints a
# summary line and returns 0 when every verb in <verbs-file> (one per
# line) appears as a backticked token in <readme-path>'s Features section;
# otherwise lists every missing verb on stderr and returns 1. Returns 2
# if the README or its Features section cannot be found.
# ---------------------------------------------------------------------
check_coverage() {
  verbs_file="$1"
  readme_path="$2"

  if [ ! -f "$readme_path" ]; then
    echo "check-readme-features.sh: README not found at $readme_path" >&2
    return 2
  fi

  slice="$(features_slice "$readme_path")"
  if [ -z "$slice" ]; then
    echo "check-readme-features.sh: no '## Features' section found in $readme_path" >&2
    return 2
  fi

  missing=""
  total=0
  while IFS= read -r verb; do
    [ -z "$verb" ] && continue
    total=$((total + 1))
    if ! printf '%s\n' "$slice" | grep -qF "\`${verb}\`"; then
      missing="${missing}${verb}\n"
    fi
  done <"$verbs_file"

  if [ -n "$missing" ]; then
    echo "README features gate: verb(s) missing a backticked mention in the Features section:" >&2
    printf '%b' "$missing" | sed '/^$/d' | sed 's/^/  - /' >&2
    return 1
  fi

  echo "README features: ${total} verbs covered"
  return 0
}

# ---------------------------------------------------------------------
# --self-test: builds a fake `zirv commands --json` binary and a matching
# README fixture, then invokes THIS script end to end (as a subprocess,
# exactly the way an operator would run it) to prove both the pass and
# the fail path, plus the two "can't even run" exit-2 paths.
# ---------------------------------------------------------------------
run_self_test() {
  script_path="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"
  work="$(mktemp -d)"
  trap 'rm -rf "$work"' EXIT

  fails=0

  make_fake_binary() {
    dir="$1"
    bin="$dir/zirv"
    cat >"$bin" <<'EOF'
#!/usr/bin/env bash
cat <<'JSON'
[
  {"path": "zirv init"},
  {"path": "zirv ctx hook audit"},
  {"path": "zirv workflow advance"}
]
JSON
EOF
    chmod +x "$bin"
    printf '%s' "$bin"
  }

  check_scenario() {
    name="$1"
    expect="$2"
    shift 2
    set +e
    bash "$script_path" "$@" >"$work/$name.out" 2>&1
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

  # 1. Every required verb (init, ctx, hook, workflow, advance) documented
  # as a backticked token -> must PASS.
  dir="$work/pass"
  mkdir -p "$dir"
  bin="$(make_fake_binary "$dir")"
  cat >"$dir/README.md" <<'EOF'
# Fixture

## Features

Covers `init`, `ctx`, `hook`, `workflow`, and `advance` in full.

## Installation

Nothing here matters.
EOF
  check_scenario "full-coverage-passes" 0 --binary "$bin" --readme "$dir/README.md"

  # 2. One required verb ("advance") only ever appears embedded in a
  # multi-word span, never standalone -> must FAIL and name it.
  dir="$work/fail"
  mkdir -p "$dir"
  bin="$(make_fake_binary "$dir")"
  cat >"$dir/README.md" <<'EOF'
# Fixture

## Features

Covers `init`, `ctx`, `hook`, and `workflow`, but the state-machine verb is
only ever written as one combined span: `workflow advance`.

## Installation

Nothing here matters.
EOF
  check_scenario "missing-verb-fails" 1 --binary "$bin" --readme "$dir/README.md"
  if ! grep -q '^  - advance$' "$work/missing-verb-fails.out"; then
    echo "self-test FAILED: missing-verb-fails did not name 'advance' as missing"
    fails=$((fails + 1))
  fi

  # 3. A verb mentioned only past the NEXT "## " heading (out of the
  # Features slice entirely) -> must still FAIL, proving the slice is
  # actually bounded rather than scanning the whole file.
  dir="$work/out-of-slice"
  mkdir -p "$dir"
  bin="$(make_fake_binary "$dir")"
  cat >"$dir/README.md" <<'EOF'
# Fixture

## Features

Covers `init`, `ctx`, `hook`, and `workflow`.

## Installation

Only mentioned here, out of scope: `advance`.
EOF
  check_scenario "out-of-slice-fails" 1 --binary "$bin" --readme "$dir/README.md"

  # 4. A binary that cannot be run -> exit 2, not 1.
  check_scenario "missing-binary-is-inconclusive" 2 \
    --binary "$work/pass/does-not-exist" --readme "$work/pass/README.md"

  # 5. A missing README -> exit 2, not 1.
  bin="$(make_fake_binary "$work/pass")"
  check_scenario "missing-readme-is-inconclusive" 2 \
    --binary "$bin" --readme "$work/pass/NO-SUCH-README.md"

  if [ "$fails" -gt 0 ]; then
    echo "check-readme-features.sh --self-test: $fails scenario(s) failed"
    return 1
  fi
  echo "check-readme-features.sh --self-test: all scenarios passed"
  return 0
}

if [ "$self_test" -eq 1 ]; then
  run_self_test
  exit $?
fi

if ! command -v "$binary" >/dev/null 2>&1 && [ ! -x "$binary" ]; then
  echo "check-readme-features.sh: binary not found or not executable: $binary" >&2
  exit 2
fi

tmp_json="$(mktemp)"
tmp_verbs="$(mktemp)"
trap 'rm -f "$tmp_json" "$tmp_verbs"' EXIT

if ! "$binary" commands --json >"$tmp_json" 2>/dev/null; then
  echo "check-readme-features.sh: '$binary commands --json' failed to run" >&2
  exit 2
fi

extract_verbs "$tmp_json" >"$tmp_verbs"
check_coverage "$tmp_verbs" "$readme"
exit $?
