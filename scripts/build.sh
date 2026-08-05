#!/usr/bin/env bash
# build.sh — Build and export EtherTap VST3 bundles for release.
#
# Usage:
#   ./scripts/build.sh [--universal]
#
#   --universal   macOS only: build aarch64 + x86_64 and merge with lipo.
#                 Requires both targets:
#                   rustup target add aarch64-apple-darwin x86_64-apple-darwin
#
# Output: dist/ethertap-{version}-{platform}.vst3.zip
#
# Set VERSION in the environment to override the value from Cargo.toml
# (CI sets it from the git tag).

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

UNIVERSAL=false
for arg in "$@"; do
  case $arg in
    --universal) UNIVERSAL=true ;;
    *) echo "Unknown argument: $arg"; exit 1 ;;
  esac
done

# ── Output helpers (gum when available; plain fallback) ───────────────────────
if command -v gum &>/dev/null; then
    step()     { gum log --level info "$*"; }
    err()      { gum log --level error "$*"; }
    ok()       { printf '\n'; gum style --foreground 2 --bold "  ✓  $*"; printf '\n'; }
    bail()     { printf '\n'; gum style --foreground 1 --bold "  ✗  $*"; printf '\n'; }
    spin_cmd() {
        local _t="$1"; shift
        local _out _err _rc
        _out=$(mktemp); _err=$(mktemp)
        "$@" >"$_out" 2>"$_err" &
        local _pid=$!
        gum spin --spinner dot --title "$_t" -- \
            sh -c "while kill -0 $_pid 2>/dev/null; do sleep 0.1; done" 2>/dev/null || true
        wait "$_pid" 2>/dev/null; _rc=$?
        [[ $_rc -ne 0 ]] && { cat "$_out"; cat "$_err" >&2; }
        rm -f "$_out" "$_err"
        return $_rc
    }
else
    step()     { echo "  → $*"; }
    err()      { echo "ERROR: $*" >&2; }
    ok()       { echo; echo "✓ $*"; echo; }
    bail()     { echo; echo "✗ $*"; }
    spin_cmd() { local _t="$1"; shift; step "$_t"; "$@"; }
fi

# ── Version ────────────────────────────────────────────────────────────────────
if [ -z "${VERSION:-}" ]; then
  VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)".*/\1/')
fi
step "Version: $VERSION"

# ── Platform label ─────────────────────────────────────────────────────────────
OS="$(uname -s)"
case "$OS" in
  Darwin)
    if $UNIVERSAL; then
      PLATFORM="macos-universal"
    else
      PLATFORM="macos-$(uname -m)"
    fi
    ;;
  Linux)
    PLATFORM="linux-x86_64"
    ;;
  MINGW*|MSYS*|CYGWIN*)
    PLATFORM="windows-x86_64"
    ;;
  *)
    PLATFORM="$(echo "$OS" | tr '[:upper:]' '[:lower:]')-$(uname -m)"
    ;;
esac
step "Platform: $PLATFORM"

# ── Build ──────────────────────────────────────────────────────────────────────
BUNDLE_DIR="target/bundled"
BUNDLE_NAME="ethertap.vst3"
BINARY_PATH="$BUNDLE_DIR/$BUNDLE_NAME/Contents/MacOS/ethertap"

if $UNIVERSAL; then
  if [ "$OS" != "Darwin" ]; then
    err "--universal requires macOS"
    exit 1
  fi

  spin_cmd "Building aarch64-apple-darwin…" \
      cargo run -p xtask -- bundle ethertap --release --target aarch64-apple-darwin
  cp "$BINARY_PATH" /tmp/ethertap-arm64

  spin_cmd "Building x86_64-apple-darwin…" \
      cargo run -p xtask -- bundle ethertap --release --target x86_64-apple-darwin
  cp "$BINARY_PATH" /tmp/ethertap-x86_64

  step "Merging into universal binary…"
  lipo -create /tmp/ethertap-arm64 /tmp/ethertap-x86_64 -output "$BINARY_PATH"
  rm -f /tmp/ethertap-arm64 /tmp/ethertap-x86_64

else
  spin_cmd "Building for host…" \
      cargo run -p xtask -- bundle ethertap --release
fi

# ── macOS Local Network declaration ───────────────────────────────────────────
# EtherTap talks to the console over UDP broadcast on the local network. From
# macOS 15 that needs the Local Network privacy grant, and a denied grant drops
# every datagram silently — no error, no devices found, no connection. The
# prompt is driven by the host application, but the plug-in bundle declaring a
# purpose string is what puts EtherTap's own reason in front of the user
# instead of a bare host name. Written after the build (the nih-plug bundler
# generates Info.plist itself) and before signing, since editing a bundle
# invalidates its signature.
if [ "$OS" = "Darwin" ]; then
  PLIST="$BUNDLE_DIR/$BUNDLE_NAME/Contents/Info.plist"
  USAGE="EtherTap discovers and syncs X32/M32 consoles over your local network."
  step "Declaring local network usage…"
  /usr/libexec/PlistBuddy -c \
    "Add :NSLocalNetworkUsageDescription string $USAGE" "$PLIST" 2>/dev/null ||
    /usr/libexec/PlistBuddy -c \
      "Set :NSLocalNetworkUsageDescription $USAGE" "$PLIST"

  step "Signing bundle…"
  codesign --force --sign - "$BUNDLE_DIR/$BUNDLE_NAME" 2>/dev/null || true
fi

# ── Export ─────────────────────────────────────────────────────────────────────
mkdir -p dist
EXPORT_NAME="ethertap-${VERSION}-${PLATFORM}.vst3"
DEST="$REPO_ROOT/dist/$EXPORT_NAME"

rm -rf "$DEST"
cp -R "$BUNDLE_DIR/$BUNDLE_NAME" "$DEST"
step "Exported: dist/$EXPORT_NAME"

# ── Package ────────────────────────────────────────────────────────────────────
# VST3 is a directory bundle on every platform; zip it so CI's
# upload-artifact step (dist/*.zip) has a single file to find.
# GitHub's Windows runners don't have `zip` on PATH for bash, but do have
# 7-Zip (`7z`), so fall back to that.
cd dist
rm -f "${EXPORT_NAME}.zip" "${EXPORT_NAME}.sha256"
if command -v zip >/dev/null 2>&1; then
  zip -r -q "${EXPORT_NAME}.zip" "$EXPORT_NAME"
elif command -v 7z >/dev/null 2>&1; then
  7z a -tzip -bd -bb0 "${EXPORT_NAME}.zip" "$EXPORT_NAME" >/dev/null
else
  err "neither 'zip' nor '7z' found for packaging"
  exit 1
fi
step "Packaged: dist/${EXPORT_NAME}.zip"

# ── Checksum ───────────────────────────────────────────────────────────────────
# Named after the .vst3 bundle (not the .zip) so it's obvious which plugin
# build the checksum verifies.
if command -v sha256sum >/dev/null 2>&1; then
  sha256sum "${EXPORT_NAME}.zip" > "${EXPORT_NAME}.sha256"
else
  shasum -a 256 "${EXPORT_NAME}.zip" > "${EXPORT_NAME}.sha256"
fi
step "Checksum: dist/${EXPORT_NAME}.sha256"
cd "$REPO_ROOT"

ok "Bundle ready: $BUNDLE_DIR/$BUNDLE_NAME"
