#!/usr/bin/env bash
# clean.sh — remove generated/vendored files.
#
# Usage:
#   ./scripts/clean.sh          # remove vendor/ only
#   ./scripts/clean.sh --all    # remove vendor/ and target/

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

step "Removing vendor/..."
rm -rf vendor/

for arg in "$@"; do
  case $arg in
    --all)
      step "Removing target/..."
      rm -rf target/
      ;;
    *) echo "Unknown argument: $arg"; exit 1 ;;
  esac
done

ok "Done — run './scripts/setup.sh' to repopulate vendor/"
