#!/usr/bin/env bash
# Start the EtherTap standalone GUI without automatically launching the mock mixer.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

cd "$REPO_ROOT"
cargo run --bin ethertap-gui --features standalone
