#!/usr/bin/env bash
# setup.sh — prepare the vendor directory and verify the build toolchain.
#
# Run this once after cloning, and again whenever you update a vendor pin.
#
# Usage:
#   ./scripts/setup.sh          # set up / re-set up vendor/
#   ./scripts/setup.sh --check  # dry-run: verify patches apply, don't write

set -euo pipefail

BASEVIEW_URL="https://github.com/RustAudio/baseview.git"
BASEVIEW_REV="579130e"   # update this when upgrading baseview (see PATCHES.md)
VENDOR_DIR="vendor/baseview"
PATCHES_DIR="patches/baseview"
DRY_RUN=false

for arg in "$@"; do
  case $arg in
    --check) DRY_RUN=true ;;
    *) echo "Unknown argument: $arg"; exit 1 ;;
  esac
done

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

echo "==> baseview @ $BASEVIEW_REV"

if $DRY_RUN; then
  echo "==> Dry-run: cloning to temp dir and testing patches..."
  TMP=$(mktemp -d)
  trap 'rm -rf "$TMP"' EXIT
  git clone --quiet "$BASEVIEW_URL" "$TMP/baseview"
  cd "$TMP/baseview" && git checkout --quiet "$BASEVIEW_REV"
  for patch in "$REPO_ROOT/$PATCHES_DIR"/*.patch; do
    echo "    patch --dry-run: $(basename "$patch")"
    patch --dry-run -p1 < "$patch"
  done
  echo "==> All patches apply cleanly."
  exit 0
fi

echo "==> Removing old vendor/baseview..."
rm -rf "$VENDOR_DIR"

echo "==> Cloning upstream baseview..."
git clone --quiet "$BASEVIEW_URL" "$VENDOR_DIR"
cd "$VENDOR_DIR"
git checkout --quiet "$BASEVIEW_REV"
rm -rf .git .cargo-ok
cd "$REPO_ROOT"

echo "==> Applying patches..."
for patch in "$PATCHES_DIR"/*.patch; do
  echo "    $(basename "$patch")"
  patch -p1 -d "$VENDOR_DIR" < "$patch"
done

echo ""
echo "Done. Run 'cargo check' to verify."
