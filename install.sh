#!/usr/bin/env sh
set -eu

REPO="Glubiz/zirv-cli"
INSTALL_DIR="${ZIRV_INSTALL_DIR:-/usr/local/bin}"
BINARY_NAME="zirv"

get_latest_version() {
    curl -sSf "https://api.github.com/repos/${REPO}/releases/latest" \
        | grep '"tag_name"' \
        | sed -E 's/.*"v([^"]+)".*/\1/'
}

detect_platform() {
    OS=$(uname -s | tr '[:upper:]' '[:lower:]')
    ARCH=$(uname -m)

    case "$OS" in
        linux*)
            case "$ARCH" in
                x86_64|amd64) ;;
                *)
                    echo "Error: Unsupported architecture for Linux: ${ARCH}." >&2
                    echo "The prebuilt Linux release is x86_64-only. Build from source instead:" >&2
                    echo "  cargo install --git https://github.com/Glubiz/zirv-cli" >&2
                    exit 1
                    ;;
            esac
            echo "linux"
            ;;
        darwin*)
            case "$ARCH" in
                x86_64|amd64|arm64|aarch64) ;;
                *)
                    echo "Error: Unsupported architecture for macOS: ${ARCH}." >&2
                    exit 1
                    ;;
            esac
            echo "macos"
            ;;
        *)
            echo "Error: Unsupported operating system: $OS" >&2
            exit 1
            ;;
    esac
}

verify_checksum() {
    tmpdir="$1"
    archive="$2"
    url="$3"

    if command -v sha256sum >/dev/null 2>&1; then
        checker="sha256sum -c"
    elif command -v shasum >/dev/null 2>&1; then
        checker="shasum -a 256 -c"
    else
        echo "Warning: neither sha256sum nor shasum is available; skipping checksum verification." >&2
        return 0
    fi

    checksum_url="${url}.sha256"
    echo "Downloading ${checksum_url}..."
    if ! curl -sSfL -o "${tmpdir}/${archive}.sha256" "$checksum_url"; then
        echo "Error: this release does not publish a checksum for ${archive} (${checksum_url})." >&2
        echo "Refusing to install an unverified download. Pass a version that publishes one:" >&2
        echo "  https://github.com/${REPO}/releases" >&2
        exit 1
    fi

    if ! ( cd "$tmpdir" && $checker "${archive}.sha256" ); then
        echo "Error: checksum verification failed for ${archive}." >&2
        echo "The download may be corrupted or tampered with -- not installing it." >&2
        exit 1
    fi
    echo "Checksum verified."
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
    curl -sSfL -o "${TMPDIR}/${ARCHIVE}" "$URL" || {
        echo "Error: Failed to download ${URL}" >&2
        echo "Check that v${VERSION} exists: https://github.com/${REPO}/releases" >&2
        exit 1
    }

    verify_checksum "$TMPDIR" "$ARCHIVE" "$URL"

    echo "Extracting..."
    tar -xzf "${TMPDIR}/${ARCHIVE}" -C "$TMPDIR"
    chmod +x "${TMPDIR}/${BINARY_NAME}"

    if [ ! -d "$INSTALL_DIR" ]; then
        echo "Creating ${INSTALL_DIR}..."
        mkdir -p "$INSTALL_DIR" 2>/dev/null || sudo mkdir -p "$INSTALL_DIR"
    fi

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
