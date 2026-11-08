#!/bin/sh
# ftc installer — macOS and Linux.
#
#   curl -fsSL https://raw.githubusercontent.com/cyberd4ch/free-torrent-cli/main/install.sh | sh
#
# Environment overrides:
#   FTC_VERSION=v3.0.0   install a specific release (default: latest)
#   FTC_PREFIX=~/bin     where to put the binary (default: /usr/local/bin, else ~/.local/bin)
#   FTC_NO_VERIFY=1      skip the SHA-256 check
set -eu

REPO="cyberd4ch/free-torrent-cli"
OS=$(uname -s)
ARCH=$(uname -m)

case "$OS-$ARCH" in
  Darwin-arm64)  ASSET=ftc-macos-arm64.tar.gz ;;
  Darwin-x86_64) ASSET=ftc-macos-x86_64.tar.gz ;;
  Linux-x86_64)  ASSET=ftc-linux-x86_64.tar.gz ;;
  Linux-aarch64 | Linux-arm64)
    echo "ftc: no Linux arm64 build yet — build one with: cargo build --release" >&2
    exit 1
    ;;
  *)
    echo "ftc: unsupported platform $OS/$ARCH" >&2
    exit 1
    ;;
esac

if [ "${FTC_VERSION:-latest}" = "latest" ]; then
  URL="https://github.com/$REPO/releases/latest/download"
else
  URL="https://github.com/$REPO/releases/download/${FTC_VERSION}"
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

fetch() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL -o "$2" "$1"
  elif command -v wget >/dev/null 2>&1; then
    wget -q -O "$2" "$1"
  else
    echo "ftc: need curl or wget" >&2
    exit 1
  fi
}

echo "-> $ASSET"
fetch "$URL/$ASSET" "$tmp/$ASSET"

if [ "${FTC_NO_VERIFY:-0}" != "1" ]; then
  if command -v shasum >/dev/null 2>&1; then
    HASH="shasum -a 256"
  elif command -v sha256sum >/dev/null 2>&1; then
    HASH="sha256sum"
  else
    HASH=""
  fi
  if [ -n "$HASH" ] && fetch "$URL/SHA256SUMS" "$tmp/SHA256SUMS" 2>/dev/null; then
    expected=$(awk -v a="$ASSET" '$2 == a { print $1 }' "$tmp/SHA256SUMS")
    actual=$($HASH "$tmp/$ASSET" | awk '{ print $1 }')
    if [ -n "$expected" ] && [ "$expected" != "$actual" ]; then
      echo "ftc: checksum mismatch for $ASSET" >&2
      exit 1
    fi
    echo "-> sha256 ok"
  fi
fi

tar -xzf "$tmp/$ASSET" -C "$tmp"

prefix="${FTC_PREFIX:-/usr/local/bin}"
if [ ! -d "$prefix" ] || [ ! -w "$prefix" ]; then
  prefix="$HOME/.local/bin"
  mkdir -p "$prefix"
fi
mv "$tmp/ftc" "$prefix/ftc"
chmod 755 "$prefix/ftc"

# Unsigned binaries are quarantined by Gatekeeper; the bits are already correct,
# this only stops the "cannot be opened" dialog.
if [ "$OS" = "Darwin" ]; then
  xattr -d com.apple.quarantine "$prefix/ftc" 2>/dev/null || true
fi

echo "-> installed $prefix/ftc"
"$prefix/ftc" --version || true

case ":$PATH:" in
  *":$prefix:"*) ;;
  *)
    printf '\n%s is not on your PATH yet:\n  export PATH="%s:$PATH"\n' "$prefix" "$prefix"
    ;;
esac
