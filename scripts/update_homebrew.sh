#!/bin/bash
set -euo pipefail

if [ "$#" -ne 2 ]; then
    echo "Usage: $0 <version> <artifact_path>"
    exit 1
fi

VERSION=$1
ARTIFACT_PATH_INPUT=$2

# Ensure the artifacts directory exists (create it if missing)
mkdir -p artifacts

echo "Looking for artifact at provided path: '$ARTIFACT_PATH_INPUT'"
# List the artifacts folder for debugging
echo "Contents of artifacts folder:"
find artifacts -type f || echo "No files found"

# Check if the file exists at the provided path
if [ -f "$ARTIFACT_PATH_INPUT" ]; then
    ARTIFACT_PATH="$ARTIFACT_PATH_INPUT"
else
    # Check if the artifact exists inside a folder named as the artifact (without .tar.gz)
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

# Directly reference the Homebrew formula file (adjust path as needed)
FORMULA="homebrew/zirv/zirv.rb"

if [ ! -f "$FORMULA" ]; then
    echo "Error: Formula file '$FORMULA' not found!"
    exit 1
fi

echo "Found formula file: $FORMULA"
echo "Updating formula $FORMULA to version $VERSION"
echo "Using artifact checksum: $CHECKSUM"

# Update version (assuming the formula uses syntax like: version "..."
sed -i "s/\(version\s*=\s*\"*\)[^\"]*\(\"*\)/\1$VERSION\2/" "$FORMULA"
# Update URL (adjust this regex and URL as needed)
sed -i "s|\(url\s*=\s*\"*\)[^\"]*\(\"*\)|\1https://github.com/Glubiz/zirv-dynamic-cli/releases/download/v$VERSION/zirv-macos-latest.tar.gz\2|" "$FORMULA"
# Update SHA256
sed -i "s/\(sha256\s*=\s*\"*\)[^\"]*\(\"*\)/\1$CHECKSUM\2/" "$FORMULA"

# Commit the changes (if any)
git add "$FORMULA"
git commit -m "Update zirv formula to version $VERSION" || echo "No changes to commit"
git push origin main
