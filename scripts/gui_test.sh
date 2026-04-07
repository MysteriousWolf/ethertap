#!/usr/bin/env bash
# gui_test.sh — Start the Python mock mixer and launch the EtherTap standalone
# GUI.  Both processes are tied together: Ctrl-C (or the GUI closing normally)
# stops the mock server too.
#
# Usage:
#   bash scripts/gui_test.sh
#
# The mock binds 0.0.0.0:10023 before cargo starts, so the GUI connects on the
# first heartbeat.  The built-in Rust mock (src/mock.rs) will print a harmless
# "port already in use" warning and exit — the Python server takes precedence.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# ── Require Python 3 ─────────────────────────────────────────────────────────
if ! command -v python3 &>/dev/null; then
    echo "error: python3 not found — install Python 3 to run the mock mixer" >&2
    exit 1
fi

# ── Start mock mixer in background ───────────────────────────────────────────
echo "==> Starting mock mixer…"
python3 "$SCRIPT_DIR/mock_mixer.py" &
MOCK_PID=$!

cleanup() {
    echo "==> Stopping mock mixer (pid $MOCK_PID)…"
    kill "$MOCK_PID" 2>/dev/null || true
    wait "$MOCK_PID" 2>/dev/null || true
}
trap cleanup EXIT

# Give the mock time to bind before cargo launches the binary
sleep 0.4

# ── Build and run ─────────────────────────────────────────────────────────────
echo "==> Building + launching ethertap-gui…"
cd "$REPO_ROOT"
cargo run --bin ethertap-gui --features standalone
