#!/usr/bin/env bash
# clean.sh — remove build artifacts.
#
# Usage:
#   ./scripts/clean.sh    # remove target/

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# ── Output helpers (gum when available; plain fallback) ───────────────────────
if command -v gum &>/dev/null; then
    step() { gum log --level info "$*"; }
    ok()   { printf '\n'; gum style --foreground 2 --bold "  ✓  $*"; printf '\n'; }
else
    step() { echo "  → $*"; }
    ok()   { echo; echo "✓ $*"; echo; }
fi

for arg in "$@"; do
  case $arg in
    *) echo "Unknown argument: $arg"; exit 1 ;;
  esac
done

step "Removing target/..."
rm -rf target/

ok "Done"
