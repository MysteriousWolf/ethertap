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
BASEVIEW_REV="91e3b4a"   # update this when upgrading baseview (see PATCHES.md)

NIH_PLUG_URL="https://github.com/robbert-vdh/nih-plug.git"
NIH_PLUG_REV="f36931f7af4646065488a9845d8f8c2f95252c23"   # update this when upgrading nih-plug (see PATCHES.md)

DRY_RUN=false

for arg in "$@"; do
  case $arg in
    --check) DRY_RUN=true ;;
    *) echo "Unknown argument: $arg"; exit 1 ;;
  esac
done

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# ── Output helpers (gum when available; plain fallback) ───────────────────────
if command -v gum &>/dev/null; then
    step()     { gum log --level info "$*"; }
    ok()       { printf '\n'; gum style --foreground 2 --bold "  ✓  $*"; printf '\n'; }
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
    ok()       { echo; echo "✓ $*"; echo; }
    spin_cmd() { local _t="$1"; shift; step "$_t"; "$@"; }
fi

vendor_one() {
  local label="$1"
  local url="$2"
  local rev="$3"
  local vendor_dir="$4"
  local patches_dir="$5"

  step "$label @ $rev"

  if $DRY_RUN; then
    step "Dry-run: cloning to temp dir and testing patches..."
    TMP=$(mktemp -d)
    trap 'rm -rf "$TMP"' EXIT
    git clone --quiet "$url" "$TMP/$label"
    cd "$TMP/$label" && git checkout --quiet "$rev" && cd "$REPO_ROOT"
    for patch in "$patches_dir"/*.patch; do
      step "patch --dry-run: $(basename "$patch")"
      patch --dry-run -p1 -d "$TMP/$label" < "$patch"
    done
    step "All patches apply cleanly."
    return
  fi

  step "Removing old $vendor_dir…"
  rm -rf "$vendor_dir"

  spin_cmd "Cloning $label @ $rev…" \
      bash -c 'set -e
               git clone --quiet "$1" "$2"
               cd "$2"
               git checkout --quiet "$3"
               rm -rf .git .cargo-ok' \
      -- "$url" "$vendor_dir" "$rev"

  step "Applying patches…"
  for patch in "$patches_dir"/*.patch; do
    step "  $(basename "$patch")"
    patch -p1 -d "$vendor_dir" < "$patch"
  done
}

vendor_one "baseview"  "$BASEVIEW_URL"  "$BASEVIEW_REV"  "vendor/baseview"  "patches/baseview"
vendor_one "nih-plug"  "$NIH_PLUG_URL"  "$NIH_PLUG_REV"  "vendor/nih-plug"  "patches/nih-plug"

ok "Vendor ready — run 'cargo check' to verify"
