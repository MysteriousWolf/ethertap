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

# ── Version ────────────────────────────────────────────────────────────────────
if [ -z "${VERSION:-}" ]; then
  VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)".*/\1/')
fi
echo "==> Version: $VERSION"

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
echo "==> Platform: $PLATFORM"

# ── Vendor ─────────────────────────────────────────────────────────────────────
if [ ! -d "vendor/baseview" ]; then
  echo "==> Running setup..."
  bash ./scripts/setup.sh
fi

# ── Build ──────────────────────────────────────────────────────────────────────
BUNDLE_DIR="target/bundled"
BUNDLE_NAME="ethertap.vst3"
BINARY_PATH="$BUNDLE_DIR/$BUNDLE_NAME/Contents/MacOS/ethertap"

if $UNIVERSAL; then
  if [ "$OS" != "Darwin" ]; then
    echo "Error: --universal requires macOS" >&2
    exit 1
  fi

  echo "==> Building aarch64-apple-darwin..."
  cargo run -p xtask -- bundle ethertap --release --target aarch64-apple-darwin
  cp "$BINARY_PATH" /tmp/ethertap-arm64

  echo "==> Building x86_64-apple-darwin..."
  cargo run -p xtask -- bundle ethertap --release --target x86_64-apple-darwin
  cp "$BINARY_PATH" /tmp/ethertap-x86_64

  echo "==> Creating universal binary..."
  lipo -create /tmp/ethertap-arm64 /tmp/ethertap-x86_64 -output "$BINARY_PATH"
  rm -f /tmp/ethertap-arm64 /tmp/ethertap-x86_64

  echo "==> Re-signing universal bundle..."
  codesign --force --sign - "$BUNDLE_DIR/$BUNDLE_NAME" 2>/dev/null || true
else
  echo "==> Building for host..."
  cargo run -p xtask -- bundle ethertap --release
fi

# ── Export ─────────────────────────────────────────────────────────────────────
mkdir -p dist
EXPORT_NAME="ethertap-${VERSION}-${PLATFORM}.vst3"
DEST="$REPO_ROOT/dist/$EXPORT_NAME"

rm -rf "$DEST"
cp -R "$BUNDLE_DIR/$BUNDLE_NAME" "$DEST"
echo "==> Exported: dist/$EXPORT_NAME"

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
  echo "Error: neither 'zip' nor '7z' found for packaging" >&2
  exit 1
fi
echo "==> Packaged: dist/${EXPORT_NAME}.zip"

# ── Checksum ───────────────────────────────────────────────────────────────────
# Named after the .vst3 bundle (not the .zip) so it's obvious which plugin
# build the checksum verifies.
if command -v sha256sum >/dev/null 2>&1; then
  sha256sum "${EXPORT_NAME}.zip" > "${EXPORT_NAME}.sha256"
else
  shasum -a 256 "${EXPORT_NAME}.zip" > "${EXPORT_NAME}.sha256"
fi
echo "==> Checksum: dist/${EXPORT_NAME}.sha256"
cd "$REPO_ROOT"

echo ""
echo "Bundle: $BUNDLE_DIR/$BUNDLE_NAME"
