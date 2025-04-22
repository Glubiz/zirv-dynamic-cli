#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 1 ]; then
  echo "Usage: $0 <version>"
  exit 1
fi

VERSION=$1
REPO="Glubiz/zirv-dynamic-cli"
TAP_REPO="Glubiz/homebrew-tap"
FORMULA_PATH="Formula/zirv.rb"
TMPDIR=$(mktemp -d)
RELEASE_URL="https://github.com/${REPO}/releases/download/v${VERSION}/zirv-macos-latest.tar.gz"
TARBALL="${TMPDIR}/zirv-macos-latest.tar.gz"

echo "• Downloading release asset for version ${VERSION}"
curl -L -o "${TARBALL}" "${RELEASE_URL}"

echo "• Computing SHA256 checksum"
CHECKSUM=$(shasum -a 256 "${TARBALL}" | awk '{print $1}')
echo "  → ${CHECKSUM}"

echo "• Cloning your Homebrew tap"
git clone "https://${HOMEBREW_TOKEN}@github.com/${TAP_REPO}.git" "${TMPDIR}/tap"
cd "${TMPDIR}/tap"

echo "• Updating ${FORMULA_PATH}"
# Update URL
sed -i '' "s|^ *url .*|  url \"${RELEASE_URL}\"|" "${FORMULA_PATH}"
# Update SHA256
sed -i '' "s|^ *sha256 .*|  sha256 \"${CHECKSUM}\"|" "${FORMULA_PATH}"
# Update version
sed -i '' "s|^ *version .*|  version \"${VERSION}\"|" "${FORMULA_PATH}"

echo "• Preview of updated formula:"
sed -n '1,20p' "${FORMULA_PATH}"
echo "  …"
sed -n '$(wc -l < "${FORMULA_PATH}")-20,$ p' "${FORMULA_PATH}"

echo "• Committing and pushing back to ${TAP_REPO}"
git config user.name  "GitHub Actions"
git config user.email "ci@github.com"
git add "${FORMULA_PATH}"
if git diff-index --quiet HEAD --; then
  echo "  No changes detected in formula; aborting commit."
else
  git commit -m "zirv: bump to v${VERSION}"
  git push origin main
  echo "  ✅ Pushed updated formula"
fi

# Cleanup
rm -rf "${TMPDIR}"
