#!/usr/bin/env bash
# format.sh — run cargo fmt --all with gum-styled status output.
#
# Usage:
#   ./scripts/format.sh

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

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

spin_cmd "Formatting…" cargo fmt --all
ok "Format done"
