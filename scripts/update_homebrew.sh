#!/bin/bash
set -euo pipefail

if [ "$#" -ne 2 ]; then
    echo "Usage: $0 <version> <artifact_path>"
    exit 1
fi

VERSION=$1
ARTIFACT_PATH_INPUT=$2

echo "Looking for artifact at provided path: '$ARTIFACT_PATH_INPUT'"
# Ensure artifacts directory exists
mkdir -p artifacts
echo "Contents of artifacts directory:"
find artifacts -type f || echo "No files found"

# Check if the artifact exists at the provided path
if [ -f "$ARTIFACT_PATH_INPUT" ]; then
    ARTIFACT_PATH="$ARTIFACT_PATH_INPUT"
else
    BASE=$(basename "$ARTIFACT_PATH_INPUT" .tar.gz)
    ALT_PATH="artifacts/$BASE/$BASE.tar.gz"
    if [ -f "$ALT_PATH" ]; then
        ARTIFACT_PATH="$ALT_PATH"
    else
        echo "Error: Artifact not found at '$ARTIFACT_PATH_INPUT' or '$ALT_PATH'"
        exit 1
    fi
fi

echo "Using artifact file: $ARTIFACT_PATH"

# Compute the SHA256 checksum of the artifact
CHECKSUM=$(sha256sum "$ARTIFACT_PATH" | awk '{print $1}')

# Define a temporary directory for the tap repository
TAP_DIR=$(mktemp -d)
echo "Cloning Homebrew tap repository into: $TAP_DIR"
git clone https://github.com/Glubiz/homebrew-tap.git "$TAP_DIR"

# The formula file is assumed to be in the Formula folder of the tap repository.
FORMULA="$TAP_DIR/Formula/zirv.rb"
if [ ! -f "$FORMULA" ]; then
    echo "Error: Formula file '$FORMULA' not found in the tap repository!"
    exit 1
fi

echo "Found formula file: $FORMULA"
echo "Updating formula $FORMULA to version $VERSION"
echo "Using artifact checksum: $CHECKSUM"

# Update the formula file with the new version, URL, and checksum.
sed -i "s/\(version\s*=\s*\"*\)[^\"]*\(\"*\)/\1$VERSION\2/" "$FORMULA"
sed -i "s|\(url\s*=\s*\"*\)[^\"]*\(\"*\)|\1https://github.com/Glubiz/zirv-dynamic-cli/releases/download/v$VERSION/zirv-macos-latest.tar.gz\2|" "$FORMULA"
sed -i "s/\(sha256\s*=\s*\"*\)[^\"]*\(\"*\)/\1$CHECKSUM\2/" "$FORMULA"

# Configure git identity for the tap repository
git -C "$TAP_DIR" config user.email "ci@github.com"
git -C "$TAP_DIR" config user.name "GitHub Actions"

# Commit and push changes, if any.
cd "$TAP_DIR"
git add Formula/zirv.rb
if git diff-index --quiet HEAD --; then
    echo "No changes to commit in the formula."
else
    git commit -m "Update zirv formula to version $VERSION"
    git push origin main
fi

# Clean up the temporary tap repository clone
rm -rf "$TAP_DIR"
