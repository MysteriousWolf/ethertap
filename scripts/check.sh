#!/usr/bin/env bash
# check.sh — clippy (both feature sets) + fmt check, with gum-styled output.
#
# Clippy output streams in real time so warnings are visible as they appear.
#
# Usage:
#   ./scripts/check.sh

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if command -v gum &>/dev/null; then
    step() { gum log --level info "$*"; }
    err()  { gum log --level error "$*"; }
    ok()   { printf '\n'; gum style --foreground 2 --bold "  ✓  $*"; printf '\n'; }
    bail() { printf '\n'; gum style --foreground 1 --bold "  ✗  $*"; printf '\n'; }
else
    step() { echo "  → $*"; }
    err()  { echo "ERROR: $*" >&2; }
    ok()   { echo; echo "✓ $*"; echo; }
    bail() { echo; echo "✗ $*"; }
fi

run_step() {
    local label="$1"; shift
    step "$label"
    if ! "$@"; then
        bail "$label failed"
        exit 1
    fi
}

run_step "Clippy (default)" \
    cargo clippy --workspace --all-targets -- -D warnings

run_step "Clippy (standalone)" \
    cargo clippy -p ethertap --all-targets --features standalone -- -D warnings

run_step "Format check" \
    cargo fmt --all --check

ok "All checks passed"
