#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 3 ]; then
    echo "Usage: $0 <version> <macos_artifact_path> <linux_artifact_path>"
    exit 1
fi

VERSION=$1
MACOS_ARTIFACT_INPUT=$2
LINUX_ARTIFACT_INPUT=$3
MACOS_BASENAME=$(basename "$MACOS_ARTIFACT_INPUT")
LINUX_BASENAME=$(basename "$LINUX_ARTIFACT_INPUT")

# Normalize Windows backslashes
MACOS_ARTIFACT_INPUT=${MACOS_ARTIFACT_INPUT//\\//}
LINUX_ARTIFACT_INPUT=${LINUX_ARTIFACT_INPUT//\\//}

mkdir -p artifacts

find_artifact() {
    local INPUT=$1
    local BASENAME=$2

    if [ -f "$INPUT" ]; then
        echo "$INPUT"
        return
    fi

    FOUND=$(find artifacts -type f -name "$BASENAME" -print -quit || true)
    if [ -n "$FOUND" ]; then
        echo "$FOUND"
        return
    fi

    echo "Error: Artifact '$BASENAME' not found under artifacts/" >&2
    exit 1
}

MACOS_PATH=$(find_artifact "$MACOS_ARTIFACT_INPUT" "$MACOS_BASENAME")
LINUX_PATH=$(find_artifact "$LINUX_ARTIFACT_INPUT" "$LINUX_BASENAME")

echo "Using macOS artifact: $MACOS_PATH"
echo "Using Linux artifact: $LINUX_PATH"

MACOS_CHECKSUM=$(sha256sum "$MACOS_PATH" | awk '{print $1}')
LINUX_CHECKSUM=$(sha256sum "$LINUX_PATH" | awk '{print $1}')

echo "macOS checksum: $MACOS_CHECKSUM"
echo "Linux checksum: $LINUX_CHECKSUM"

if [ -z "${HOMEBREW_TOKEN:-}" ]; then
  echo "Error: HOMEBREW_TOKEN is not set!"
  exit 1
fi

TAP_DIR=$(mktemp -d)

echo "Cloning homebrew-tap into $TAP_DIR"
git clone "https://${HOMEBREW_TOKEN}@github.com/Glubiz/homebrew-tap.git" "$TAP_DIR"
FORMULA="$TAP_DIR/Formula/zirv.rb"

if [ ! -f "$FORMULA" ]; then
    echo "Error: Formula not found at $FORMULA"
    exit 1
fi

MACOS_URL="https://github.com/Glubiz/zirv-dynamic-cli/releases/download/v${VERSION}/${MACOS_BASENAME}"
LINUX_URL="https://github.com/Glubiz/zirv-dynamic-cli/releases/download/v${VERSION}/${LINUX_BASENAME}"

echo "Writing updated formula"

cat > "$FORMULA" << RUBY
class Zirv < Formula
  desc "Dynamic CLI tool to streamline tasks and boost productivity"
  homepage "https://github.com/Glubiz/zirv-dynamic-cli"
  license "MIT"
  version "${VERSION}"

  if OS.mac?
    url "${MACOS_URL}"
    sha256 "${MACOS_CHECKSUM}"
  elsif OS.linux?
    url "${LINUX_URL}"
    sha256 "${LINUX_CHECKSUM}"
  end

  def install
    bin.install "zirv"
  end

  test do
    system "#{bin}/zirv", "--version"
  end
end
RUBY

echo "Formula after update:"
cat "$FORMULA"

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
