#!/usr/bin/env bash
# clean.sh — remove generated/vendored files.
#
# Usage:
#   ./scripts/clean.sh          # remove vendor/ only
#   ./scripts/clean.sh --all    # remove vendor/ and target/

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

echo "==> Removing vendor/..."
rm -rf vendor/

for arg in "$@"; do
  case $arg in
    --all)
      echo "==> Removing target/..."
      rm -rf target/
      ;;
    *) echo "Unknown argument: $arg"; exit 1 ;;
  esac
done

echo "Done. Run './scripts/setup.sh' to repopulate vendor/."
