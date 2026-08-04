#!/usr/bin/env bash
# install.sh — Build EtherTap and install the VST3 bundle into a plug-in folder.
#
# Usage:
#   ./scripts/install.sh [--universal] [--no-build] [--dest DIR] [--yes]
#
#   --universal   macOS only: build a universal binary (passed to build.sh).
#   --no-build    Install the existing target/bundled/ethertap.vst3 as-is.
#   --dest DIR    Install into DIR without asking.
#   --yes         Overwrite an existing install without confirming.
#
# With no --dest, the script offers the standard VST3 folders for the current
# platform and asks which one to use when there is more than one candidate.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

UNIVERSAL=false
DO_BUILD=true
DEST_DIR=""
ASSUME_YES=false

while [ $# -gt 0 ]; do
  case "$1" in
    --universal) UNIVERSAL=true ;;
    --no-build)  DO_BUILD=false ;;
    --yes|-y)    ASSUME_YES=true ;;
    --dest)      DEST_DIR="${2:-}"; shift ;;
    --dest=*)    DEST_DIR="${1#--dest=}" ;;
    *) echo "Unknown argument: $1"; exit 1 ;;
  esac
  shift
done

# ── Output helpers (gum when available; plain fallback) ───────────────────────
HAVE_GUM=false
if command -v gum &>/dev/null; then
    HAVE_GUM=true
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

# Ask a yes/no question. Non-interactive shells answer "no" so an unattended
# run never destroys an existing install by accident.
confirm() {
    local prompt="$1"
    $ASSUME_YES && return 0
    if [ ! -t 0 ]; then
        return 1
    fi
    if $HAVE_GUM; then
        gum confirm "$prompt"
    else
        local reply
        read -r -p "$prompt [y/N] " reply
        [[ "$reply" =~ ^[Yy]$ ]]
    fi
}

# ── Candidate plug-in folders ─────────────────────────────────────────────────
OS="$(uname -s)"
CANDIDATES=()
case "$OS" in
  Darwin)
    CANDIDATES=(
      "$HOME/Library/Audio/Plug-Ins/VST3"
      "/Library/Audio/Plug-Ins/VST3"
    )
    ;;
  Linux)
    CANDIDATES=(
      "$HOME/.vst3"
      "/usr/local/lib/vst3"
      "/usr/lib/vst3"
    )
    ;;
  MINGW*|MSYS*|CYGWIN*)
    CANDIDATES=(
      "${COMMONPROGRAMFILES:-/c/Program Files/Common Files}/VST3"
      "${LOCALAPPDATA:-$HOME/AppData/Local}/Programs/Common/VST3"
    )
    ;;
  *)
    err "Unsupported platform: $OS — pass --dest DIR"
    exit 1
    ;;
esac

# ── Build ─────────────────────────────────────────────────────────────────────
BUNDLE="target/bundled/ethertap.vst3"

if $DO_BUILD; then
  BUILD_ARGS=()
  $UNIVERSAL && BUILD_ARGS+=(--universal)
  bash ./scripts/build.sh "${BUILD_ARGS[@]}"
fi

if [ ! -d "$BUNDLE" ]; then
  bail "No bundle at $BUNDLE — run without --no-build"
  exit 1
fi

# ── Pick destination ──────────────────────────────────────────────────────────
if [ -z "$DEST_DIR" ]; then
  if [ "${#CANDIDATES[@]}" -eq 1 ]; then
    DEST_DIR="${CANDIDATES[0]}"
  elif [ ! -t 0 ]; then
    DEST_DIR="${CANDIDATES[0]}"
    step "Non-interactive — defaulting to $DEST_DIR"
  elif $HAVE_GUM; then
    CHOICE=$(printf '%s\n' "${CANDIDATES[@]}" "Other…" |
      gum choose --header "Install EtherTap.vst3 where?")
    if [ -z "$CHOICE" ]; then
      bail "Cancelled"
      exit 1
    fi
    if [ "$CHOICE" = "Other…" ]; then
      CHOICE=$(gum input --placeholder "/path/to/VST3" --prompt "Folder: ")
      [ -z "$CHOICE" ] && { bail "Cancelled"; exit 1; }
    fi
    DEST_DIR="$CHOICE"
  else
    echo "Install EtherTap.vst3 where?"
    select CHOICE in "${CANDIDATES[@]}"; do
      [ -n "$CHOICE" ] && { DEST_DIR="$CHOICE"; break; }
    done
  fi
fi

DEST_DIR="${DEST_DIR%/}"
DEST="$DEST_DIR/ethertap.vst3"

# ── Privilege ─────────────────────────────────────────────────────────────────
# Writing into a system-wide plug-in folder needs root; a user folder does not.
# Decide by testing the deepest existing ancestor rather than the leaf, since
# the folder itself may not exist yet.
probe="$DEST_DIR"
while [ ! -d "$probe" ] && [ "$probe" != "/" ] && [ -n "$probe" ]; do
  probe="$(dirname "$probe")"
done

SUDO=()
if [ ! -w "$probe" ]; then
  if command -v sudo >/dev/null 2>&1; then
    step "$DEST_DIR needs elevated permissions — using sudo"
    SUDO=(sudo)
  else
    bail "No write access to $DEST_DIR and sudo is unavailable"
    exit 1
  fi
fi

# ── Overwrite check ───────────────────────────────────────────────────────────
if [ -e "$DEST" ]; then
  if ! confirm "$DEST already exists. Replace it?"; then
    bail "Left existing install untouched"
    exit 1
  fi
  "${SUDO[@]}" rm -rf "$DEST"
fi

# ── Install ───────────────────────────────────────────────────────────────────
"${SUDO[@]}" mkdir -p "$DEST_DIR"
"${SUDO[@]}" cp -R "$BUNDLE" "$DEST"

# cp preserves the ad-hoc signature build.sh applied, but re-signing keeps the
# installed copy valid if anything in the tree was touched on the way over.
if [ "$OS" = "Darwin" ]; then
  "${SUDO[@]}" codesign --force --sign - "$DEST" 2>/dev/null || true
fi

ok "Installed: $DEST"
