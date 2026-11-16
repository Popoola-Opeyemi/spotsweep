#!/usr/bin/env bash
# Remove the user LaunchAgent and installed executable.  The source repository
# and project files are never touched.  Only Spotlight exclusions added by this
# application are removed.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
PREFIX="${PREFIX:-$HOME/.local}"
DATA_DIR="${DATA_DIR:-$HOME/Library/Application Support/SpotSweep}"
PURGE=0
ASSUME_YES=0
KEEP_EXCLUSIONS=0
LABEL="com.spotsweep.agent"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
LEGACY_LABEL="dev.spotlightdevexclusions.agent"
LEGACY_PLIST="$HOME/Library/LaunchAgents/$LEGACY_LABEL.plist"
LOGS_DIR="$HOME/Library/Logs/SpotSweep"

usage() {
  cat <<'EOF'
Usage: ./scripts/uninstall.sh [options]

Options:
  --prefix DIR     Installation prefix (default: ~/.local)
  --data-dir DIR   Application Support directory
  --keep-data      Preserve configuration, database and exports (default)
  --purge-data     Also delete configuration, database and exports
  --keep-exclusions  Keep Spotlight exclusions this application added
  --yes            Do not prompt when --purge-data is used
  -h, --help       Show this help
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
    --data-dir) DATA_DIR="$(expand_tilde "${2:?--data-dir requires a value}")"; shift 2 ;;
    --data-dir=*) DATA_DIR="$(expand_tilde "${1#*=}")"; shift ;;
    --keep-data) PURGE=0; shift ;;
    --purge-data) PURGE=1; shift ;;
    --keep-exclusions) KEEP_EXCLUSIONS=1; shift ;;
    --yes) ASSUME_YES=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown option: $1" >&2; usage; exit 2 ;;
  esac
done

if [[ "$(id -u)" -eq 0 ]]; then
  echo "Run uninstallation as your user, not root." >&2
  exit 1
fi

BIN="$PREFIX/bin/spotsweep"
OLD_BIN="$PREFIX/bin/spotlight-dev"

# Stop and unload first.  Prefer the installed binary so the label and paths
# match what was installed; fall back to launchctl manually.
if [[ -x "$BIN" && -f "$PLIST" ]]; then
  "$BIN" --data-dir "$DATA_DIR" service uninstall || true
else
  launchctl bootout "gui/$(id -u)/$LABEL" >/dev/null 2>&1 || true
  launchctl bootout "gui/$(id -u)/$LEGACY_LABEL" >/dev/null 2>&1 || true
  rm -f "$PLIST" "$LEGACY_PLIST"
fi

if [[ "$KEEP_EXCLUSIONS" -eq 0 && -x "$BIN" && -f "$DATA_DIR/spotsweep.db" ]]; then
  echo "Removing Spotlight exclusions added by this application..."
  "$BIN" --data-dir "$DATA_DIR" uninstall --keep-data || true
else
  echo "Spotlight exclusions were kept."
fi

if [[ -x "$BIN" ]]; then
  rm -f "$BIN"
  echo "Removed executable: $BIN"
else
  echo "Executable was not installed at: $BIN"
fi
if [[ -x "$OLD_BIN" ]]; then
  if [[ -f "$HOME/Library/Application Support/SpotlightDevExclusions/state.sqlite3" ]]; then
    "$OLD_BIN" --data-dir "$HOME/Library/Application Support/SpotlightDevExclusions" uninstall --keep-data || true
  fi
  rm -f "$OLD_BIN"
  echo "Removed legacy executable: $OLD_BIN"
fi
rm -f "$PLIST" "$LEGACY_PLIST"
rmdir "$PREFIX/bin" 2>/dev/null || true

if [[ "$PURGE" -eq 1 ]]; then
  if [[ "$ASSUME_YES" -ne 1 ]]; then
    if [[ -t 0 ]]; then
      printf 'Delete scan history and configuration in %s? [y/N] ' "$DATA_DIR"
      read -r answer
      case "$answer" in
        y|Y|yes|YES) ;;
        *) echo "Keeping application data."; PURGE=0 ;;
      esac
    else
      echo "Refusing to purge data without --yes in a non-interactive session." >&2
      echo "Application data preserved: $DATA_DIR"
      PURGE=0
    fi
  fi
fi

if [[ "$PURGE" -eq 1 ]]; then
  # Safety: never allow a destructive path for /, $HOME, or a path that
  # contains the source repository.
  if [[ -z "$DATA_DIR" || "$DATA_DIR" == "/" || "$DATA_DIR" == "$HOME" ]]; then
    echo "Refusing unsafe data path: $DATA_DIR" >&2
    exit 1
  fi
  RESOLVED_DATA="$(cd "$DATA_DIR" 2>/dev/null && pwd -P || true)"
  if [[ -n "$RESOLVED_DATA" && ( "$REPO_DIR" == "$RESOLVED_DATA"* || "$RESOLVED_DATA" == "$REPO_DIR"* ) ]]; then
    echo "Refusing to delete a directory related to the source repository: $RESOLVED_DATA" >&2
    exit 1
  fi
  rm -rf "$DATA_DIR"
  echo "Removed application data: $DATA_DIR"
  if [[ "$DATA_DIR" == "$HOME/Library/Application Support/SpotSweep" ]]; then
    rm -rf "$LOGS_DIR"
    echo "Removed logs: $LOGS_DIR"
  fi
else
  echo "Application data preserved: $DATA_DIR"
fi

cat <<EOF

Uninstallation complete.
  Source repository: $REPO_DIR (untouched)
  Spotlight Search Privacy: app-managed entries $([[ "$KEEP_EXCLUSIONS" -eq 1 ]] && echo kept || echo removed); user entries preserved
  Project files: untouched
EOF
