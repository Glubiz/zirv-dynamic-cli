#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 2 ]; then
    echo "Usage: $0 <version> <artifact_path>"
    exit 1
fi

VERSION=$1
ARTIFACT_INPUT=$2
BASENAME=$(basename "$ARTIFACT_INPUT")

# Normalize Windows backslashes
ARTIFACT_INPUT=${ARTIFACT_INPUT//\\//}

echo "Looking for artifact matching: '$BASENAME'"
# Ensure artifacts directory exists
mkdir -p artifacts

echo "Contents of artifacts directory:"
find artifacts -type f || echo "(empty)"

# First, check if the direct path exists
if [ -f "$ARTIFACT_INPUT" ]; then
    ARTIFACT_PATH="$ARTIFACT_INPUT"
else
    # Otherwise, search recursively under artifacts/ for the filename
    FOUND=$(find artifacts -type f -name "$BASENAME" -print -quit || true)
    if [ -n "$FOUND" ]; then
        ARTIFACT_PATH="$FOUND"
    else
        echo "Error: Artifact '$BASENAME' not found under artifacts/"
        exit 1
    fi
fi

echo "Using artifact file: $ARTIFACT_PATH"

# Compute checksum
CHECKSUM=$(sha256sum "$ARTIFACT_PATH" | awk '{print $1}')
echo "Computed checksum: $CHECKSUM"

# Ensure token
if [ -z "${HOMEBREW_TOKEN:-}" ]; then
  echo "Error: HOMEBREW_TOKEN is not set!"
  exit 1
fi

# Clone tap and update formula
TAP_DIR=$(mktemp -d)

echo "Cloning homebrew-tap into $TAP_DIR"
git clone "https://${HOMEBREW_TOKEN}@github.com/Glubiz/homebrew-tap.git" "$TAP_DIR"
FORMULA="$TAP_DIR/Formula/zirv.rb"

if [ ! -f "$FORMULA" ]; then
    echo "Error: Formula not found at $FORMULA"
    exit 1
fi

echo "Updating $FORMULA to v$VERSION with new URL and checksum"

RELEASE_URL="https://github.com/Glubiz/zirv-dynamic-cli/releases/download/v${VERSION}/${BASENAME}"

# 1) Update the URL line
#    - single‑quoted sed program
#    - close quote, insert shell var, reopen quote
sed -i \
    's|^ *url *".*"|  url "'"${RELEASE_URL}"'"|' \
    "$FORMULA"

# 2) Update the version line
sed -i \
    's|^ *version *".*"|  version "'"${VERSION}"'"|' \
    "$FORMULA"

# 3) Update the sha256 line
sed -i \
    's|^ *sha256 *".*"|  sha256 "'"${CHECKSUM}"'"|' \
    "$FORMULA"

echo "Formula after update:"
sed -n '1,10p; /sha256/,/end/p; 1q' "$FORMULA"

# Commit and push
cd "$TAP_DIR"
git config user.email "ci@github.com"
git config user.name "GitHub Actions"

git add "$FORMULA"
if git diff-index --quiet HEAD --; then
    echo "No changes to commit"
else
    git commit -m "zirv: bump to v${VERSION}"
    git push origin main
    echo "Pushed formula update"
fi

rm -rf "$TAP_DIR"
