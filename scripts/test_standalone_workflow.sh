#!/usr/bin/env bash
# test_standalone_workflow.sh — End-to-end standalone GUI integration test.
#
# Launches the real EtherTap standalone GUI (visible window) and mock-suite
# in headless mode, then verifies that the plugin connects and dispatches
# BPM sync commands to the mixer. Uses ETHERTAP_TEST_PORT to trigger
# auto-connect from the plugin's initialize() hook without GUI interaction.
#
# Requirements:
#   - macOS (the standalone binary opens an Iced window via CPAL)
#   - A display must be available (not headless CI without Xvfb)
#   - Run from the repository root: ./scripts/test_standalone_workflow.sh
#
# Exit codes:
#   0  — all expectations satisfied (connection + BPM sync received)
#   1  — timeout: mock-suite didn't see the expected OSC traffic
#   2  — build or startup error
#
# Usage:
#   ./scripts/test_standalone_workflow.sh [--port PORT] [--bpm BPM] [--timeout SECS]

set -euo pipefail

MOCK_PORT=10023
TIMEOUT=20
BPM=120

while [[ $# -gt 0 ]]; do
    case $1 in
        --port)    MOCK_PORT="$2"; shift 2 ;;
        --bpm)     BPM="$2";       shift 2 ;;
        --timeout) TIMEOUT="$2";   shift 2 ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

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
    spin_wait() {
        local _pid="$1" _title="$2"
        gum spin --spinner dot --title "$_title" -- \
            sh -c "while kill -0 $_pid 2>/dev/null; do sleep 0.1; done" 2>/dev/null || true
    }
else
    step()     { echo "  → $*"; }
    err()      { echo "ERROR: $*" >&2; }
    ok()       { echo; echo "✓ $*"; echo; }
    bail()     { echo; echo "✗ $*"; }
    spin_cmd() { local _t="$1"; shift; step "$_t"; "$@"; }
    spin_wait() { :; }
fi

# ── Build ────────────────────────────────────────────────────────────────────

spin_cmd "Building mock-suite…" cargo build -p mock-suite --quiet || {
    err "mock-suite build failed"
    exit 2
}

spin_cmd "Building ethertap standalone…" \
    cargo build --bin ethertap-gui --features standalone --quiet || {
    err "standalone build failed"
    exit 2
}

MOCK_BIN="$ROOT/target/debug/mock-suite"
GUI_BIN="$ROOT/target/debug/ethertap-gui"

# ── Start mock-suite ─────────────────────────────────────────────────────────
# Expectations:
#   /info:3      — at least 3 heartbeats (confirms stable connection)
#   sync:1:1     — at least one BPM sync to slot 1 (auto-sync after connect)

step "Starting mock-suite on :$MOCK_PORT..."
"$MOCK_BIN" \
    --no-tui \
    --port "$MOCK_PORT" \
    --slots "dly:$BPM,empty,empty,empty,empty,empty,empty,empty" \
    --expect "/info:3" \
    --expect "sync:1:1" \
    --duration "$TIMEOUT" \
    &
MOCK_PID=$!

# Give mock-suite a moment to bind its UDP socket before the plugin tries to
# connect (races on localhost at startup are rare but possible).
sleep 0.3

# ── Launch standalone GUI ────────────────────────────────────────────────────
# ETHERTAP_TEST_PORT triggers initialize() to set target 127.0.0.1:$MOCK_PORT
# and send ConnectToLast before the first audio callback, so no GUI click needed.

step "Launching ethertap-gui (ETHERTAP_TEST_PORT=$MOCK_PORT) — a window will appear"
ETHERTAP_TEST_PORT="$MOCK_PORT" "$GUI_BIN" &
GUI_PID=$!

# ── Wait for mock-suite result ───────────────────────────────────────────────

spin_wait "$MOCK_PID" "Waiting for OSC traffic (timeout: ${TIMEOUT}s)…"
wait "$MOCK_PID"
MOCK_EXIT=$?

# ── Clean up ─────────────────────────────────────────────────────────────────

kill "$GUI_PID" 2>/dev/null || true
wait "$GUI_PID" 2>/dev/null || true

# ── Report ───────────────────────────────────────────────────────────────────

if [[ $MOCK_EXIT -eq 0 ]]; then
    ok "PASS — connection + BPM sync verified"
else
    bail "FAIL — timeout: expected OSC traffic not received (exit $MOCK_EXIT)"
fi

exit "$MOCK_EXIT"
