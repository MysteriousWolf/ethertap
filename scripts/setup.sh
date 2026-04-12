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

NIH_PLUG_URL="https://github.com/robbert-vdh/nih-plug.git"
NIH_PLUG_REV="28b149ec4d62757d0b448809148a0c3ca6e09a95"   # update this when upgrading nih-plug (see PATCHES.md)

DRY_RUN=false

for arg in "$@"; do
  case $arg in
    --check) DRY_RUN=true ;;
    *) echo "Unknown argument: $arg"; exit 1 ;;
  esac
done

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

vendor_one() {
  local label="$1"
  local url="$2"
  local rev="$3"
  local vendor_dir="$4"
  local patches_dir="$5"

  echo "==> $label @ $rev"

  if $DRY_RUN; then
    echo "    Dry-run: cloning to temp dir and testing patches..."
    TMP=$(mktemp -d)
    trap 'rm -rf "$TMP"' EXIT
    git clone --quiet "$url" "$TMP/$label"
    cd "$TMP/$label" && git checkout --quiet "$rev" && cd "$REPO_ROOT"
    for patch in "$patches_dir"/*.patch; do
      echo "    patch --dry-run: $(basename "$patch")"
      patch --dry-run -p1 -d "$TMP/$label" < "$patch"
    done
    echo "    All patches apply cleanly."
    return
  fi

  echo "    Removing old $vendor_dir..."
  rm -rf "$vendor_dir"

  echo "    Cloning upstream..."
  git clone --quiet "$url" "$vendor_dir"
  cd "$vendor_dir"
  git checkout --quiet "$rev"
  rm -rf .git .cargo-ok
  cd "$REPO_ROOT"

  echo "    Applying patches..."
  for patch in "$patches_dir"/*.patch; do
    echo "    $(basename "$patch")"
    patch -p1 -d "$vendor_dir" < "$patch"
  done
}

vendor_one "baseview"  "$BASEVIEW_URL"  "$BASEVIEW_REV"  "vendor/baseview"  "patches/baseview"
vendor_one "nih-plug"  "$NIH_PLUG_URL"  "$NIH_PLUG_REV"  "vendor/nih-plug"  "patches/nih-plug"

echo ""
echo "Done. Run 'cargo check' to verify."
