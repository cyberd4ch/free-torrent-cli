#!/bin/sh
# ftc installer — macOS and Linux.
#
#   curl -fsSL https://raw.githubusercontent.com/cyberd4ch/footorrent/main/install.sh | sh
#
# Environment overrides:
#   FTC_VERSION=v3.0.0   install a specific release (default: latest)
#   FTC_PREFIX=~/bin     where to put the binary (default: /usr/local/bin, else ~/.local/bin)
#   FTC_NO_VERIFY=1      skip the SHA-256 check
set -eu

REPO="cyberd4ch/footorrent"
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

# FTC_BASE_URL overrides the download root (used to test this script against a
# local directory without publishing a release).
if [ -n "${FTC_BASE_URL:-}" ]; then
  URL="$FTC_BASE_URL"
elif [ "${FTC_VERSION:-latest}" = "latest" ]; then
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
  if [ -z "$HASH" ]; then
    echo "-> sha256 SKIPPED — no shasum/sha256sum on PATH; verify by hand" >&2
  elif ! fetch "$URL/SHA256SUMS" "$tmp/SHA256SUMS" 2>/dev/null; then
    echo "-> sha256 SKIPPED — this release carries no SHA256SUMS" >&2
  else
    expected=$(awk -v a="$ASSET" '$2 == a { print $1 }' "$tmp/SHA256SUMS")
    actual=$($HASH "$tmp/$ASSET" | awk '{ print $1 }')
    if [ -n "$expected" ] && [ "$expected" != "$actual" ]; then
      echo "ftc: checksum mismatch for $ASSET" >&2
      exit 1
    fi
    echo "-> sha256 ok"
  fi
else
  echo "-> sha256 SKIPPED — FTC_NO_VERIFY is set" >&2
fi

tar -xzf "$tmp/$ASSET" -C "$tmp"

# An explicit FTC_PREFIX is a choice, not a hint: honour it, and create it when
# it does not exist. The fallback only applies when we pick the location.
prefix="${FTC_PREFIX:-/usr/local/bin}"
if [ -n "${FTC_PREFIX:-}" ]; then
  mkdir -p "$prefix"
elif [ ! -d "$prefix" ] || [ ! -w "$prefix" ]; then
  prefix="$HOME/.local/bin"
  mkdir -p "$prefix"
fi
mv "$tmp/ftc" "$prefix/ftc"
chmod 755 "$prefix/ftc"
# Brand alias: the same binary under the product name, so existing scripts that
# call `ftc` keep working and new ones can use `footorrent`.
ln -sfn ftc "$prefix/footorrent"

# Unsigned binaries are quarantined by Gatekeeper; the bits are already correct,
# this only stops the "cannot be opened" dialog.
if [ "$OS" = "Darwin" ]; then
  xattr -d com.apple.quarantine "$prefix/ftc" 2>/dev/null || true
fi

echo "-> installed $prefix/ftc (alias: $prefix/footorrent)"
"$prefix/ftc" --version || true

# One binary, both interfaces: `ftc` in a terminal, and a launcher that runs
# `ftc gui` (web ui + default browser) for everyone who would rather click.
version=$("$prefix/ftc" --version 2>/dev/null | awk '{print $2}')
[ -n "$version" ] || version=unknown

if [ "$OS" = Darwin ]; then
  apps="${FTC_APPS_DIR:-$HOME/Applications}"
  app="$apps/Footorrent.app"
  mkdir -p "$apps"
  # The real app bundle: a WKWebView window compiled in CI, so double-clicking
  # gives a window rather than a browser tab. It is built for Apple silicon
  # only, and an arm64 bundle on an Intel Mac does not launch — so fall through
  # to the launcher there instead of installing something that cannot start.
  if [ "$ARCH" = arm64 ] &&
     fetch "$URL/ftc-macos-app.zip" "$tmp/ftc-app.zip" 2>/dev/null &&
     unzip -q -o "$tmp/ftc-app.zip" -d "$apps" >/dev/null 2>&1 &&
     [ -d "$app" ]; then
    xattr -dr com.apple.quarantine "$app" 2>/dev/null || true
    echo "-> installed $app (double-click for the app window)"
  else
    # Releases before the app bundle only carry the binary, and the bundle is
    # Apple silicon only; either way, give these users a launcher that opens the
    # same UI in the browser.
    echo "-> no native app bundle for this Mac; writing a browser launcher"
    mkdir -p "$app/Contents/MacOS"
    cat > "$app/Contents/MacOS/ftc" <<LAUNCHER
#!/bin/sh
exec "$prefix/ftc" gui "\$@"
LAUNCHER
    chmod 755 "$app/Contents/MacOS/ftc"
    cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>Footorrent</string>
    <key>CFBundleDisplayName</key><string>Footorrent</string>
    <key>CFBundleIdentifier</key><string>dev.ftc.webui</string>
    <key>CFBundleExecutable</key><string>ftc</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$version</string>
    <key>CFBundleVersion</key><string>$version</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
    echo "-> installed $app (double-click for the web ui)"
  fi
elif [ "$OS" = Linux ]; then
  desktop_dir="${FTC_APPS_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}}/applications"
  mkdir -p "$desktop_dir"
  cat > "$desktop_dir/ftc.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=ftc
Comment=BitTorrent client — web ui
Exec="$prefix/ftc" gui
Terminal=false
Categories=Network;FileTransfer;
DESKTOP
  chmod 644 "$desktop_dir/ftc.desktop"
  echo "-> installed $desktop_dir/ftc.desktop"
fi

case ":$PATH:" in
  *":$prefix:"*) ;;
  *)
    # shellcheck disable=SC2016  # the literal "$PATH" is what we want to print
    printf '\n%s is not on your PATH yet:\n  export PATH="%s:$PATH"\n' "$prefix" "$prefix"
    ;;
esac
