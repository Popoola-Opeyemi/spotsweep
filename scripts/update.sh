#!/usr/bin/env bash
# Upgrade workflow: pull, build, replace the executable, restart the agent.
# SQLite state, configuration and exports are preserved.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
PREFIX="${PREFIX:-$HOME/.local}"
DO_PULL=1

usage() {
  cat <<'EOF'
Usage: ./scripts/update.sh [options]

Options:
  --prefix DIR   Installation prefix (default: ~/.local)
  --no-pull      Build the current checkout without git pull
  -h, --help     Show this help
EOF
}

expand_tilde() {
  case "$1" in
    "~") printf '%s\n' "$HOME" ;;
    "~/"*) printf '%s\n' "$HOME/${1#\~/}" ;;
    *) printf '%s\n' "$1" ;;
  esac
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --prefix) PREFIX="$(expand_tilde "${2:?--prefix requires a value}")"; shift 2 ;;
    --prefix=*) PREFIX="$(expand_tilde "${1#*=}")"; shift ;;
    --no-pull) DO_PULL=0; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown option: $1" >&2; usage; exit 2 ;;
  esac
done

if [[ "$(id -u)" -eq 0 ]]; then
  echo "Run the upgrade as your user, not root." >&2
  exit 1
fi

cd "$REPO_DIR"

if [[ "$DO_PULL" -eq 1 ]]; then
  if [[ -d .git ]]; then
    if git remote get-url origin >/dev/null 2>&1; then
      echo "Pulling latest changes..."
      git pull --ff-only
    else
      echo "No git remote configured; building the local checkout."
    fi
  else
    echo "Not a git checkout; building the local source."
  fi
fi

echo "Installing the latest release binary..."
"$SCRIPT_DIR/install-binary.sh" --build --prefix "$PREFIX" --restart

DATA_DIR="$HOME/Library/Application Support/SpotSweep"
BIN="$PREFIX/bin/spotsweep"
if [[ -x "$BIN" ]]; then
  echo
  "$BIN" --data-dir "$DATA_DIR" status || true
  echo
  echo "Upgrade complete. Configuration and scan history were preserved."
  echo "Database: $DATA_DIR/spotsweep.db"
fi
