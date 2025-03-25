#!/bin/bash
VERSION=$1
ARTIFACT_PATH=$2
CHECKSUM=$(shasum -a 256 "$ARTIFACT_PATH" | awk '{print $1}')

# Update the formula file with sed (adjust the regex as needed)
sed -i '' -E "s/version \".*\"/version \"$VERSION\"/" zirv.rb
sed -i '' -E "s/url \".*\"/url \"https:\/\/github.com\/Glubiz\/zirv-dynamic-cli\/releases\/download\/v$VERSION\/zirv-macos-latest.tar.gz\"/" zirv.rb
sed -i '' -E "s/sha256 \".*\"/sha256 \"$CHECKSUM\"/" zirv.rb

# Commit and push changes
git add zirv.rb
git commit -m "Update zirv formula to version $VERSION"
git push origin main
