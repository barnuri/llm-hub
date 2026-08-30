#!/bin/sh
# llm-hub installer: downloads the latest release binary for this OS/arch.
set -eu

REPO="barnuri/llm-hub"
INSTALL_DIR="${LLM_HUB_INSTALL_DIR:-/usr/local/bin}"

os=$(uname -s | tr '[:upper:]' '[:lower:]')
case "$os" in
  darwin) os="macos" ;;
  linux) os="linux" ;;
  *) echo "unsupported OS: $os" >&2; exit 1 ;;
esac

arch=$(uname -m)
case "$arch" in
  arm64|aarch64) arch="aarch64" ;;
  x86_64|amd64) arch="x86_64" ;;
  *) echo "unsupported arch: $arch" >&2; exit 1 ;;
esac

asset="llm-hub-${os}-${arch}"
url="https://github.com/${REPO}/releases/latest/download/${asset}"

echo "downloading ${url}"
tmp=$(mktemp)
curl -fsSL "$url" -o "$tmp"
chmod +x "$tmp"

if [ -w "$INSTALL_DIR" ]; then
  mv "$tmp" "$INSTALL_DIR/llm-hub"
else
  echo "writing to $INSTALL_DIR needs sudo"
  sudo mv "$tmp" "$INSTALL_DIR/llm-hub"
fi

echo "installed: $("$INSTALL_DIR/llm-hub" --version 2>/dev/null || echo "$INSTALL_DIR/llm-hub")"
echo "next: create a .env (see .env.example) and run: llm-hub"
