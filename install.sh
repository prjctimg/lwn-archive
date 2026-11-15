#!/usr/bin/env bash
set -euo pipefail

REPO="${LWN_ARCHIVE_REPO:-prjctimg/lwn-archive}"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"
BIN_NAME="lwn-reader"

need() { command -v "$1" >/dev/null 2>&1 || { echo "Missing required command: $1" >&2; exit 1; }; }
need curl
need tar

OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
ARCH="$(uname -m)"
case "$ARCH" in
  x86_64|amd64) ARCH="x86_64" ;;
  aarch64|arm64) ARCH="aarch64" ;;
esac

API="https://api.github.com/repos/${REPO}/releases/latest"
echo "Fetching latest release from ${REPO}..."
RELEASE_JSON="$(curl -fsSL -H "Accept: application/vnd.github+json" -H "User-Agent: lwn-reader-installer" ${GITHUB_TOKEN:+-H "Authorization: Bearer ${GITHUB_TOKEN}"} "$API")"

# Try to find a matching binary asset, else fall back to generic lwn-reader, else extract from bundle
ASSET_URL="$(printf '%s' "$RELEASE_JSON" | grep -o '"browser_download_url"[[:space:]]*:[[:space:]]*"[^"]*"' | sed 's/.*"\(https[^"]*\)".*/\1/' | grep "${BIN_NAME}" | grep "$OS" | grep "$ARCH" | head -n1 || true)"
if [ -z "$ASSET_URL" ]; then
  ASSET_URL="$(printf '%s' "$RELEASE_JSON" | grep -o '"browser_download_url"[[:space:]]*:[[:space:]]*"[^"]*"' | sed 's/.*"\(https[^"]*\)".*/\1/' | grep "/${BIN_NAME}$" | head -n1 || true)"
fi
BUNDLE_URL="$(printf '%s' "$RELEASE_JSON" | grep -o '"browser_download_url"[[:space:]]*:[[:space:]]*"[^"]*"' | sed 's/.*"\(https[^"]*\)".*/\1/' | grep 'bundle' | grep '\.tar\.gz' | head -n1 || true)"

mkdir -p "$INSTALL_DIR"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

if [ -n "$ASSET_URL" ]; then
  echo "Downloading ${BIN_NAME} binary..."
  curl -fL --progress-bar -H "User-Agent: lwn-reader-installer" ${GITHUB_TOKEN:+-H "Authorization: Bearer ${GITHUB_TOKEN}"} -o "$TMP/$BIN_NAME" "$ASSET_URL"
  chmod +x "$TMP/$BIN_NAME"
  mv "$TMP/$BIN_NAME" "$INSTALL_DIR/$BIN_NAME"
elif [ -n "$BUNDLE_URL" ]; then
  echo "Downloading release bundle..."
  curl -fL --progress-bar -H "User-Agent: lwn-reader-installer" ${GITHUB_TOKEN:+-H "Authorization: Bearer ${GITHUB_TOKEN}"} -o "$TMP/bundle.tar.gz" "$BUNDLE_URL"
  tar -xzf "$TMP/bundle.tar.gz" -C "$TMP"
  FOUND="$(find "$TMP" -type f -name "$BIN_NAME" | head -n1 || true)"
  if [ -z "$FOUND" ]; then
    echo "Could not find ${BIN_NAME} in bundle." >&2
    exit 1
  fi
  chmod +x "$FOUND"
  mv "$FOUND" "$INSTALL_DIR/$BIN_NAME"
else
  echo "No suitable release asset found for ${REPO}. Build from source: bun build --compile reader/lwn-reader.ts --outfile ${BIN_NAME}" >&2
  exit 1
fi

echo "Installed ${INSTALL_DIR}/${BIN_NAME}"
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) echo "Note: $INSTALL_DIR is not in your PATH. Add it, e.g.: export PATH=\"$INSTALL_DIR:\$PATH\"" ;;
esac
echo "Run '${BIN_NAME}' to start (it will download the archive on first run)."
