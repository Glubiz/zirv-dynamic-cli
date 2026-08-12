#!/usr/bin/env bash
# Manual/CI utility: check-doc-staleness.sh [DAYS] (default 30)
#
# Walks docs/obsidian/*.md (excluding .obsidian/), reads the `last-verified:`
# date from each page's YAML frontmatter, and reports pages that are stale,
# missing a date, or have an unparsable date. Exits 1 if any findings were
# reported, 0 otherwise.

set -u

days="${1:-30}"

case "$days" in
  ''|*[!0-9]*)
    echo "Usage: $0 [DAYS]  (DAYS must be a positive integer, default 30)" >&2
    exit 2
    ;;
esac

repo_root="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
vault_dir="$repo_root/docs/obsidian"

if [ ! -d "$vault_dir" ]; then
  echo "No vault found at $vault_dir" >&2
  exit 0
fi

# Determine "now" as epoch seconds, and whether GNU `date -d` is available.
have_gnu_date=0
if date -d "1970-01-01" +%s >/dev/null 2>&1; then
  have_gnu_date=1
fi
now_epoch="$(date +%s)"

# to_epoch DATE -> prints epoch seconds on stdout, or nothing on failure.
to_epoch() {
  d="$1"
  if [ "$have_gnu_date" -eq 1 ]; then
    date -d "$d" +%s 2>/dev/null
    return
  fi
  # BSD/macOS date fallback (also occasionally what Git Bash ships).
  if date -j -f "%Y-%m-%d" "$d" +%s >/dev/null 2>&1; then
    date -j -f "%Y-%m-%d" "$d" +%s 2>/dev/null
    return
  fi
  # Last-resort manual parse for strict YYYY-MM-DD, assuming no exotic
  # calendar edge cases (good enough for a staleness check, not banking).
  case "$d" in
    [0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9])
      y="${d%%-*}"
      rest="${d#*-}"
      m="${rest%%-*}"
      day="${rest#*-}"
      # Days since epoch approximation via awk (no external date math needed).
      awk -v y="$y" -v m="$m" -v d="$day" 'BEGIN {
        a = int((14 - m) / 12);
        yy = y + 4800 - a;
        mm = m + 12 * a - 3;
        jdn = d + int((153 * mm + 2) / 5) + 365 * yy + int(yy / 4) - int(yy / 100) + int(yy / 400) - 32045;
        epoch_jdn = 2440588; # JDN for 1970-01-01
        printf "%d\n", (jdn - epoch_jdn) * 86400;
      }'
      ;;
    *)
      return 1
      ;;
  esac
}

findings=0

# Portable recursive .md listing excluding any .obsidian/ path component.
while IFS= read -r file; do
  case "$file" in
    */.obsidian/*|*/.obsidian) continue ;;
  esac

  # Pull the value of `last-verified:` from the YAML frontmatter (the block
  # between the first two `---` lines).
  frontmatter="$(awk 'BEGIN{n=0} /^---[[:space:]]*$/{n++; if(n==2) exit; next} n==1{print}' "$file")"
  raw_value="$(printf '%s\n' "$frontmatter" | grep -E '^last-verified:' | head -n 1 | sed -E 's/^last-verified:[[:space:]]*//')"
  # Strip surrounding quotes, if any.
  raw_value="$(printf '%s' "$raw_value" | sed -E 's/^"(.*)"$/\1/; s/^'"'"'(.*)'"'"'$/\1/')"

  rel="${file#"$repo_root"/}"

  if [ -z "$raw_value" ]; then
    echo "MISSING DATE: $rel"
    findings=$((findings + 1))
    continue
  fi

  if ! printf '%s' "$raw_value" | grep -Eq '^[0-9]{4}-[0-9]{2}-[0-9]{2}$'; then
    echo "INVALID DATE: $rel"
    findings=$((findings + 1))
    continue
  fi

  file_epoch="$(to_epoch "$raw_value")"
  if [ -z "$file_epoch" ]; then
    echo "INVALID DATE: $rel"
    findings=$((findings + 1))
    continue
  fi

  age_days=$(( (now_epoch - file_epoch) / 86400 ))

  if [ "$age_days" -gt "$days" ]; then
    echo "STALE ($age_days days): $rel"
    findings=$((findings + 1))
  fi
done <<EOF
$(find "$vault_dir" -type f -name '*.md' 2>/dev/null)
EOF

if [ "$findings" -gt 0 ]; then
  exit 1
fi

exit 0
