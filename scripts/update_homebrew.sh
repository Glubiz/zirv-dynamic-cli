#!/bin/bash
set -euo pipefail

if [ "$#" -ne 2 ]; then
    echo "Usage: $0 <version> <artifact_path>"
    exit 1
fi

VERSION=$1
ARTIFACT_PATH=$2

# Compute the SHA256 checksum of the artifact
CHECKSUM=$(sha256sum "$ARTIFACT_PATH" | awk '{print $1}')

# Set the path to your Homebrew formula.
# Update this if your formula is in a different folder.
FORMULA="zirv.rb"

if [ ! -f "$FORMULA" ]; then
    echo "Error: Formula file '$FORMULA' not found!"
    exit 1
fi

echo "Updating formula $FORMULA to version $VERSION"
echo "Using artifact checksum: $CHECKSUM"

# Update version
sed -i "s/\(version\s*=\s*\"*\)[^\"]*\(\"*\)/\1$VERSION\2/" "$FORMULA"
# Update URL (assuming the URL pattern is as shown)
sed -i "s|\(url\s*=\s*\"*\)[^\"]*\(\"*\)|\1https://github.com/Glubiz/zirv-dynamic-cli/releases/download/v$VERSION/zirv-macos-latest.tar.gz\2|" "$FORMULA"
# Update SHA256
sed -i "s/\(sha256\s*=\s*\"*\)[^\"]*\(\"*\)/\1$CHECKSUM\2/" "$FORMULA"

# Optionally, update other fields (like releaseNotes, summary, etc.)

# Commit the changes
git add "$FORMULA"
git commit -m "Update zirv formula to version $VERSION" || echo "No changes to commit"
git push origin main
