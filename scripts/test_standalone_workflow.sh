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

# ── Build ────────────────────────────────────────────────────────────────────

echo "[test_standalone_workflow] building mock-suite…"
cargo build -p mock-suite --quiet 2>&1 || {
    echo "[test_standalone_workflow] mock-suite build failed" >&2
    exit 2
}

echo "[test_standalone_workflow] building ethertap standalone…"
cargo build --bin ethertap-gui --features standalone --quiet 2>&1 || {
    echo "[test_standalone_workflow] standalone build failed" >&2
    exit 2
}

MOCK_BIN="$ROOT/target/debug/mock-suite"
GUI_BIN="$ROOT/target/debug/ethertap-gui"

# ── Start mock-suite ─────────────────────────────────────────────────────────
# Expectations:
#   /info:3      — at least 3 heartbeats (confirms stable connection)
#   sync:1:1     — at least one BPM sync to slot 1 (auto-sync after connect)

echo "[test_standalone_workflow] starting mock-suite on :$MOCK_PORT…"
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

echo "[test_standalone_workflow] launching ethertap-gui (ETHERTAP_TEST_PORT=$MOCK_PORT)…"
echo "[test_standalone_workflow] ↳ a window will appear — this is intentional"
ETHERTAP_TEST_PORT="$MOCK_PORT" "$GUI_BIN" &
GUI_PID=$!

# ── Wait for mock-suite result ───────────────────────────────────────────────

wait "$MOCK_PID"
MOCK_EXIT=$?

# ── Clean up ─────────────────────────────────────────────────────────────────

kill "$GUI_PID" 2>/dev/null || true
wait "$GUI_PID" 2>/dev/null || true

# ── Report ───────────────────────────────────────────────────────────────────

if [[ $MOCK_EXIT -eq 0 ]]; then
    echo "[test_standalone_workflow] PASS — connection + BPM sync verified"
else
    echo "[test_standalone_workflow] FAIL — timeout: expected OSC traffic not received (exit $MOCK_EXIT)" >&2
fi

exit "$MOCK_EXIT"
