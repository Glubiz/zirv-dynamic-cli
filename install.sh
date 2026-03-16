#!/usr/bin/env sh
set -eu

REPO="Glubiz/zirv-dynamic-cli"
INSTALL_DIR="/usr/local/bin"
BINARY_NAME="zirv"

get_latest_version() {
    curl -sSf "https://api.github.com/repos/${REPO}/releases/latest" \
        | grep '"tag_name"' \
        | sed -E 's/.*"v([^"]+)".*/\1/'
}

detect_platform() {
    OS=$(uname -s | tr '[:upper:]' '[:lower:]')
    case "$OS" in
        linux*)  echo "linux" ;;
        darwin*) echo "macos" ;;
        *)
            echo "Error: Unsupported operating system: $OS" >&2
            exit 1
            ;;
    esac
}

main() {
    VERSION="${1:-$(get_latest_version)}"
    if [ -z "$VERSION" ]; then
        echo "Error: Could not determine latest version." >&2
        exit 1
    fi

    PLATFORM=$(detect_platform)
    ARCHIVE="${BINARY_NAME}-${VERSION}-${PLATFORM}.tar.gz"
    URL="https://github.com/${REPO}/releases/download/v${VERSION}/${ARCHIVE}"

    echo "Installing zirv v${VERSION} for ${PLATFORM}..."

    TMPDIR=$(mktemp -d)
    trap 'rm -rf "$TMPDIR"' EXIT

    echo "Downloading ${URL}..."
    curl -sSfL -o "${TMPDIR}/${ARCHIVE}" "$URL"

    echo "Extracting..."
    tar -xzf "${TMPDIR}/${ARCHIVE}" -C "$TMPDIR"
    chmod +x "${TMPDIR}/${BINARY_NAME}"

    if [ -w "$INSTALL_DIR" ]; then
        mv "${TMPDIR}/${BINARY_NAME}" "${INSTALL_DIR}/${BINARY_NAME}"
    else
        echo "Installing to ${INSTALL_DIR} (requires sudo)..."
        sudo mv "${TMPDIR}/${BINARY_NAME}" "${INSTALL_DIR}/${BINARY_NAME}"
    fi

    echo "zirv v${VERSION} installed to ${INSTALL_DIR}/${BINARY_NAME}"
    echo "Run 'zirv --version' to verify."
}

main "$@"
