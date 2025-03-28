#!/bin/bash
set -euo pipefail

if [ "$#" -ne 2 ]; then
    echo "Usage: $0 <version> <artifact_path>"
    exit 1
fi

VERSION=$1
ARTIFACT_PATH_INPUT=$2

echo "Looking for artifact at provided path: '$ARTIFACT_PATH_INPUT'"
# Ensure the artifacts directory exists
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
echo "Computed checksum: $CHECKSUM"

# Clone the Homebrew tap repository into a temporary folder
TAP_DIR=$(mktemp -d)
echo "Cloning Homebrew tap repository into: $TAP_DIR"
git clone https://github.com/Glubiz/homebrew-tap.git "$TAP_DIR"

# Define the formula file location (inside the Formula folder of your tap repo)
FORMULA="$TAP_DIR/Formula/zirv.rb"
if [ ! -f "$FORMULA" ]; then
    echo "Error: Formula file '$FORMULA' not found in the tap repository!"
    exit 1
fi

echo "Found formula file: $FORMULA"
echo "Updating formula to version $VERSION with checksum $CHECKSUM"

# Update the version in the formula file
sed -i "s/^ *version *\"[^\"]*\"/  version \"$VERSION\"/" "$FORMULA"

# Update the URL to point to the new release asset.
# Adjust the URL pattern if necessary.
sed -i "s|^ *url *\"[^\"]*\"|  url \"https://github.com/Glubiz/zirv-dynamic-cli/releases/download/v$VERSION/zirv-macos-latest.tar.gz\"|" "$FORMULA"

# Update the SHA256 value
sed -i "s/^ *sha256 *\"[^\"]*\"/  sha256 \"$CHECKSUM\"/" "$FORMULA"

echo "Updated formula file contents:"
cat "$FORMULA"

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
