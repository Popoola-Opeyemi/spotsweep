#!/usr/bin/env bash
# Install (or replace) only the compiled executable.  Configuration, SQLite
# history, exports and logs are never touched.  Can be run from any directory.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
PREFIX="${PREFIX:-$HOME/.local}"
SOURCE_BIN=""
FORCE_BUILD=0
RESTART=0

usage() {
  cat <<'EOF'
Usage: ./scripts/install-binary.sh [options]

Options:
  --prefix DIR    Install to DIR/bin/spotlight-dev (default: ~/.local)
  --binary FILE   Install a specific compiled binary
  --build         Force a release rebuild before installing
  --restart       Restart the LaunchAgent if it is installed
  -h, --help      Show this help
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
    --binary) SOURCE_BIN="$(expand_tilde "${2:?--binary requires a value}")"; shift 2 ;;
    --binary=*) SOURCE_BIN="$(expand_tilde "${1#*=}")"; shift ;;
    --build) FORCE_BUILD=1; shift ;;
    --restart) RESTART=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown option: $1" >&2; usage; exit 2 ;;
  esac
done

if [[ "$(id -u)" -eq 0 ]]; then
  echo "Install the user executable without sudo." >&2
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

CARGO="${CARGO:-}"
if [[ -z "$CARGO" ]]; then
  if command -v cargo >/dev/null 2>&1; then
    CARGO="$(command -v cargo)"
  elif [[ -x "$HOME/.cargo/bin/cargo" ]]; then
    CARGO="$HOME/.cargo/bin/cargo"
  fi
fi

if [[ -z "$SOURCE_BIN" ]]; then
  SOURCE_BIN="$REPO_DIR/target/release/spotlight-dev"
  NEED_BUILD=0
  [[ "$FORCE_BUILD" -eq 1 ]] && NEED_BUILD=1
  [[ ! -x "$SOURCE_BIN" ]] && NEED_BUILD=1
  if [[ -x "$SOURCE_BIN" ]] && find "$REPO_DIR/src" "$REPO_DIR/migrations" \
      "$REPO_DIR/Cargo.toml" -newer "$SOURCE_BIN" -print -quit 2>/dev/null | grep -q .; then
    NEED_BUILD=1
  fi
  if [[ "$NEED_BUILD" -eq 1 ]]; then
    if [[ -z "$CARGO" ]]; then
      echo "No compiled binary and cargo is unavailable. Build first with: cargo build --release" >&2
      exit 1
    fi
    echo "Building release binary..."
    ( cd "$REPO_DIR" && "$CARGO" build --release )
  fi
fi

if [[ ! -x "$SOURCE_BIN" ]]; then
  echo "No executable found at $SOURCE_BIN" >&2
  exit 1
fi
if command -v lipo >/dev/null 2>&1; then
  ARCHS="$(lipo -archs "$SOURCE_BIN" 2>/dev/null || true)"
  if [[ -n "$ARCHS" && "$ARCHS" != *"$WANT_ARCH"* ]]; then
    echo "Binary architecture ($ARCHS) does not match this Mac ($WANT_ARCH)." >&2
    exit 1
  fi
fi

BIN_DIR="$PREFIX/bin"
BIN="$BIN_DIR/spotlight-dev"
mkdir -p "$BIN_DIR"
TMP_BIN="$BIN_DIR/.spotlight-dev.tmp.$$"
cp "$SOURCE_BIN" "$TMP_BIN"
chmod 0755 "$TMP_BIN"
mv -f "$TMP_BIN" "$BIN"
echo "Installed: $BIN"
echo "Configuration, database and exports were not modified."

if [[ "$RESTART" -eq 1 ]]; then
  PLIST="$HOME/Library/LaunchAgents/dev.spotlightdevexclusions.agent.plist"
  if [[ -f "$PLIST" ]]; then
    echo "Restarting LaunchAgent..."
    if ! "$BIN" service restart; then
      echo "Warning: LaunchAgent restart failed; run: $BIN service status" >&2
    fi
  else
    echo "LaunchAgent is not installed; restart skipped."
  fi
fi
