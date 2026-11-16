#!/usr/bin/env bash
# Install SpotSweep for the current user.
#
# The script never overwrites an existing configuration or SQLite database.
# It builds the release binary from this repository, copies it to
# <prefix>/bin/spotsweep, initialises state, installs the LaunchAgent and
# performs the first discovery scan.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
PREFIX="${PREFIX:-$HOME/.local}"
DATA_DIR="${DATA_DIR:-$HOME/Library/Application Support/SpotSweep}"
INSTALL_SERVICE=1
RUN_SCAN=1
RUN_SELFTEST=1
DETECT_ROOTS=1
FORCE_BUILD=0
LABEL="com.spotsweep.agent"
LEGACY_LABEL="dev.spotlightdevexclusions.agent"
LEGACY_PLIST="$HOME/Library/LaunchAgents/$LEGACY_LABEL.plist"

usage() {
  cat <<'EOF'
Usage: ./scripts/install.sh [options]

Options:
  --prefix DIR       Install executable under DIR/bin (default: ~/.local)
  --data-dir DIR     Application Support directory
                     (default: ~/Library/Application Support/SpotSweep)
  --no-service       Do not install/start the LaunchAgent
  --no-scan          Do not run the initial discovery scan/report
  --no-selftest      Do not run the Spotlight exclusion selftest
  --no-detect        Do not add existing common development folders
  --build            Force a release rebuild
  -h, --help         Show this help
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
    --no-service) INSTALL_SERVICE=0; shift ;;
    --no-scan) RUN_SCAN=0; shift ;;
    --no-selftest) RUN_SELFTEST=0; shift ;;
    --no-detect) DETECT_ROOTS=0; shift ;;
    --build) FORCE_BUILD=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown option: $1" >&2; usage; exit 2 ;;
  esac
done

if [[ "$(id -u)" -eq 0 ]]; then
  echo "Do not run this installer as root. It installs a user LaunchAgent." >&2
  exit 1
fi
if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "This application targets macOS." >&2
  exit 1
fi

ARCH="$(uname -m)"
case "$ARCH" in
  arm64) WANT_ARCH="arm64" ;;
  x86_64) WANT_ARCH="x86_64" ;;
  *) echo "Unsupported Mac architecture: $ARCH" >&2; exit 1 ;;
esac
echo "Mac architecture: $ARCH"

# Stop the pre-rename agent before its data directory is migrated.
if launchctl print "gui/$(id -u)/$LEGACY_LABEL" >/dev/null 2>&1; then
  echo "Stopping legacy SpotlightDevExclusions LaunchAgent..."
  launchctl bootout "gui/$(id -u)/$LEGACY_LABEL" >/dev/null 2>&1 || true
fi

BIN_DIR="$PREFIX/bin"
BIN="$BIN_DIR/spotsweep"
SRC_BIN="$REPO_DIR/target/release/spotsweep"
CARGO="${CARGO:-}"
if [[ -z "$CARGO" ]]; then
  if command -v cargo >/dev/null 2>&1; then
    CARGO="$(command -v cargo)"
  elif [[ -x "$HOME/.cargo/bin/cargo" ]]; then
    CARGO="$HOME/.cargo/bin/cargo"
  else
    echo "Rust/cargo was not found. Install the Rust toolchain first:" >&2
    echo "  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh" >&2
    exit 1
  fi
fi

needs_build() {
  [[ "$FORCE_BUILD" -eq 1 ]] && return 0
  [[ ! -x "$SRC_BIN" ]] && return 0
  if command -v lipo >/dev/null 2>&1; then
    local archs
    archs="$(lipo -archs "$SRC_BIN" 2>/dev/null || true)"
    if [[ -n "$archs" && "$archs" != *"$WANT_ARCH"* ]]; then
      return 0
    fi
  fi
  if find "$REPO_DIR/src" "$REPO_DIR/migrations" "$REPO_DIR/Cargo.toml" \
      -newer "$SRC_BIN" -print -quit 2>/dev/null | grep -q .; then
    return 0
  fi
  return 1
}

if needs_build; then
  echo "Building release binary (this can take a minute)..."
  ( cd "$REPO_DIR" && "$CARGO" build --release )
else
  echo "Using existing release binary: $SRC_BIN"
fi

if [[ ! -x "$SRC_BIN" ]]; then
  echo "Release build did not produce $SRC_BIN" >&2
  exit 1
fi
if command -v lipo >/dev/null 2>&1; then
  ARCHS="$(lipo -archs "$SRC_BIN" 2>/dev/null || true)"
  if [[ -n "$ARCHS" && "$ARCHS" != *"$WANT_ARCH"* ]]; then
    echo "Binary architecture ($ARCHS) does not match this Mac ($WANT_ARCH)." >&2
    exit 1
  fi
fi

mkdir -p "$BIN_DIR"
TMP_BIN="$BIN_DIR/.spotsweep.tmp.$$"
cp "$SRC_BIN" "$TMP_BIN"
chmod 0755 "$TMP_BIN"
mv -f "$TMP_BIN" "$BIN"
echo "Installed executable: $BIN"

OLD_BIN="$BIN_DIR/spotlight-dev"
if [[ -f "$OLD_BIN" ]]; then
  rm -f "$OLD_BIN"
  echo "Removed legacy executable: $OLD_BIN"
fi

# `status` triggers legacy data migration and creates the default config and
# database only when no old data was migrated.  Never overwrite either.
"$BIN" --data-dir "$DATA_DIR" status >/dev/null
if [[ -f "$DATA_DIR/config.toml" ]]; then
  echo "Configuration ready: $DATA_DIR/config.toml"
else
  echo "Warning: configuration was not created at $DATA_DIR/config.toml" >&2
fi
echo "SQLite ready: $DATA_DIR/spotsweep.db"

SELFTEST_OK=0
if [[ "$RUN_SELFTEST" -eq 1 ]]; then
  echo "Checking that Spotlight honors exclusions on this macOS (10-35 seconds)..."
  if "$BIN" --data-dir "$DATA_DIR" selftest; then
    SELFTEST_OK=1
  else
    SELFTEST_STATUS=$?
    if [[ "$SELFTEST_STATUS" -eq 2 ]]; then
      echo "Selftest was inconclusive (Spotlight may be busy)." >&2
    else
      echo "Selftest failed or the private API is unavailable." >&2
    fi
    echo "Automatic exclusion stays gated; the manual export workflow remains available." >&2
  fi
fi

if [[ "$DETECT_ROOTS" -eq 1 ]]; then
  ADDED=0
  for candidate in \
    "$HOME/Projects" "$HOME/Developer" "$HOME/development" "$HOME/Code" "$HOME/code" \
    "$HOME/Work" "$HOME/work" "$HOME/src" "$HOME/dev" "$HOME/git"; do
    if [[ -d "$candidate" ]]; then
      "$BIN" --data-dir "$DATA_DIR" add-path "$candidate" --no-scan >/dev/null 2>&1 || true
      ADDED=$((ADDED + 1))
    fi
  done
  if [[ "$ADDED" -gt 0 ]]; then
    echo "Checked $ADDED existing development folders as scan roots."
  fi
fi

REPORT_PATH=""
if [[ "$RUN_SCAN" -eq 1 ]]; then
  echo "Running initial discovery scan..."
  "$BIN" --data-dir "$DATA_DIR" scan
  REPORT_PATH="$DATA_DIR/inventory-$(date +%Y%m%d-%H%M%S).txt"
  "$BIN" --data-dir "$DATA_DIR" report --output "$REPORT_PATH" >/dev/null
  echo "Initial inventory: $REPORT_PATH"
fi

if [[ -f "$LEGACY_PLIST" ]]; then
  rm -f "$LEGACY_PLIST"
  echo "Removed legacy LaunchAgent plist: $LEGACY_PLIST"
fi

SERVICE_OK=0
if [[ "$INSTALL_SERVICE" -eq 1 ]]; then
  if launchctl print "gui/$(id -u)" >/dev/null 2>&1; then
    echo "Installing LaunchAgent..."
    if "$BIN" --data-dir "$DATA_DIR" service install --binary "$BIN"; then
      SERVICE_OK=1
      "$BIN" --data-dir "$DATA_DIR" service start || true
    else
      echo "Warning: LaunchAgent installation failed; the CLI still works." >&2
    fi
  else
    echo "Warning: no GUI launchd session detected; skipped LaunchAgent installation." >&2
    echo "You can install it later with: $BIN service install --binary $BIN" >&2
  fi
fi

echo
"$BIN" --data-dir "$DATA_DIR" status || true

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *)
    echo
    echo "Add the command to your PATH for future shells:"
    echo "  echo 'export PATH=\"$BIN_DIR:\$PATH\"' >> ~/.zshrc"
    ;;
esac

if [[ -n "$REPORT_PATH" ]]; then
  echo
  echo "Inventory summary (first 20 pending entries):"
  "$BIN" --data-dir "$DATA_DIR" pending --limit 20 || true
fi

cat <<EOF

Automatic exclusion: $([[ "$SELFTEST_OK" -eq 1 ]] && echo "enabled (selftest passed)" || echo "gated (run: $BIN selftest)")
Check and reconcile at any time:
  $BIN verify ~/path/to/node_modules
  $BIN sync --dry-run
  $BIN sync

If automatic exclusion is unavailable or not confirmed, use the fallback:
  1. $BIN export
  2. System Settings -> Spotlight -> Search Privacy -> add the exported folders
  3. $BIN mark --all
  4. $BIN verify --confirm

The only Spotlight write is through the private _MDSetExclusion call, verified
by re-reading Spotlight's list. No plist or private database is edited.
LaunchAgent installed: $([[ "$SERVICE_OK" -eq 1 ]] && echo yes || echo no)
Data directory: $DATA_DIR
Configuration: $DATA_DIR/config.toml
Database: $DATA_DIR/spotsweep.db
Logs: $(if [[ "$DATA_DIR" == "$HOME/Library/Application Support/SpotSweep" ]]; then echo "$HOME/Library/Logs/SpotSweep"; else echo "$DATA_DIR/logs"; fi)
EOF
