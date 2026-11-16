#!/usr/bin/env bash
# Download and install the universal spotlight-dev release for the current user.
#
# Examples:
#   curl -fsSL https://raw.githubusercontent.com/Popoola-Opeyemi/spotlight-dev-exclusions/main/scripts/install-release.sh | bash
#   VERSION=v0.2.0 bash scripts/install-release.sh
#   SPOTLIGHT_DEV_NO_SERVICE=1 bash scripts/install-release.sh
set -euo pipefail

REPOSITORY="${REPOSITORY:-Popoola-Opeyemi/spotlight-dev-exclusions}"
VERSION="${VERSION:-latest}"
PREFIX="${PREFIX:-$HOME/.local}"
DATA_DIR="${DATA_DIR:-$HOME/Library/Application Support/SpotlightDevExclusions}"
NO_SERVICE="${SPOTLIGHT_DEV_NO_SERVICE:-0}"
NO_SELFTEST="${SPOTLIGHT_DEV_NO_SELFTEST:-0}"
NO_SCAN="${SPOTLIGHT_DEV_NO_SCAN:-0}"
BIN_DIR="$PREFIX/bin"
BIN="$BIN_DIR/spotlight-dev"
ASSET="spotlight-dev-macos-universal.zip"

usage() {
  cat <<'EOF'
Usage: install-release.sh

Environment:
  VERSION           latest (default) or a release tag such as v0.2.0
  PREFIX            install prefix (default: ~/.local)
  DATA_DIR          Application Support directory
  REPOSITORY        GitHub owner/repo (default: Popoola-Opeyemi/spotlight-dev-exclusions)
  SPOTLIGHT_DEV_NO_SERVICE=1   do not install/start the LaunchAgent
  SPOTLIGHT_DEV_NO_SELFTEST=1  skip the Spotlight exclusion selftest
  SPOTLIGHT_DEV_NO_SCAN=1      skip the initial discovery scan
EOF
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

if [[ "$(id -u)" -eq 0 ]]; then
  echo "Do not run this installer as root; it installs a user LaunchAgent." >&2
  exit 1
fi
if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "This application targets macOS." >&2
  exit 1
fi
case "$(uname -m)" in
  arm64|x86_64) ;;
  *) echo "Unsupported Mac architecture: $(uname -m)" >&2; exit 1 ;;
esac

if [[ "$VERSION" == "latest" ]]; then
  RELEASE_BASE="https://github.com/$REPOSITORY/releases/latest/download"
else
  RELEASE_BASE="https://github.com/$REPOSITORY/releases/download/$VERSION"
fi

TMP="$(mktemp -d "${TMPDIR:-/tmp}/spotlight-dev-install.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

echo "Downloading $ASSET from $RELEASE_BASE..."
curl -fL --retry 3 --connect-timeout 15 "$RELEASE_BASE/$ASSET" -o "$TMP/$ASSET"

if curl -fsL --retry 2 --connect-timeout 15 "$RELEASE_BASE/SHA256SUMS" -o "$TMP/SHA256SUMS" 2>/dev/null; then
  EXPECTED="$(awk -v name="$ASSET" '$2 == name { print $1 }' "$TMP/SHA256SUMS")"
  if [[ -n "$EXPECTED" ]]; then
    ACTUAL="$(shasum -a 256 "$TMP/$ASSET" | awk '{print $1}')"
    if [[ "$EXPECTED" != "$ACTUAL" ]]; then
      echo "Checksum mismatch for $ASSET" >&2
      echo "  expected: $EXPECTED" >&2
      echo "  actual:   $ACTUAL" >&2
      exit 1
    fi
    echo "Checksum verified."
  fi
else
  echo "Warning: SHA256SUMS was not available; continuing without checksum verification." >&2
fi

mkdir -p "$TMP/extract"
ditto -x -k "$TMP/$ASSET" "$TMP/extract"
SOURCE_BIN="$(find "$TMP/extract" -type f -name spotlight-dev -perm -111 -print -quit)"
if [[ -z "$SOURCE_BIN" ]]; then
  echo "The release archive did not contain an executable spotlight-dev." >&2
  exit 1
fi

mkdir -p "$BIN_DIR" "$DATA_DIR"
TMP_BIN="$BIN_DIR/.spotlight-dev.tmp.$$"
cp "$SOURCE_BIN" "$TMP_BIN"
chmod 0755 "$TMP_BIN"
mv -f "$TMP_BIN" "$BIN"
# Remove the quarantine bit from our downloaded binary if it was set.
xattr -d com.apple.quarantine "$BIN" >/dev/null 2>&1 || true
echo "Installed: $BIN ($(lipo -archs "$BIN" 2>/dev/null || echo 'architecture unknown'))"

if [[ ! -f "$DATA_DIR/config.toml" ]]; then
  "$BIN" --data-dir "$DATA_DIR" config init >/dev/null
  echo "Created configuration: $DATA_DIR/config.toml"
else
  echo "Preserved existing configuration: $DATA_DIR/config.toml"
fi
"$BIN" --data-dir "$DATA_DIR" status >/dev/null
echo "SQLite ready: $DATA_DIR/state.sqlite3"

if [[ "$NO_SELFTEST" != "1" ]]; then
  echo "Running the Spotlight exclusion selftest (10-35 seconds)..."
  if "$BIN" --data-dir "$DATA_DIR" selftest; then
    echo "Selftest passed; automatic exclusions are enabled."
  else
    echo "Selftest failed or was inconclusive; automatic exclusions stay gated." >&2
    echo "The manual export workflow is still available." >&2
  fi
fi

if [[ "$NO_SCAN" != "1" ]]; then
  "$BIN" --data-dir "$DATA_DIR" scan || true
fi

if [[ "$NO_SERVICE" != "1" ]]; then
  if launchctl print "gui/$(id -u)" >/dev/null 2>&1; then
    if "$BIN" --data-dir "$DATA_DIR" service install --binary "$BIN"; then
      "$BIN" --data-dir "$DATA_DIR" service start || true
    else
      echo "Warning: LaunchAgent installation failed; the CLI still works." >&2
    fi
  else
    echo "Warning: no GUI launchd session detected; skipped LaunchAgent." >&2
  fi
fi

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *)
    echo
    echo "Add the command to your PATH:"
    echo "  echo 'export PATH=\"$BIN_DIR:\$PATH\"' >> ~/.zshrc"
    ;;
esac

cat <<EOF

spotlight-dev installed.
  Binary: $BIN
  Data:   $DATA_DIR
  Status: $BIN --data-dir "$DATA_DIR" status
  Verify: $BIN --data-dir "$DATA_DIR" verify /path/to/node_modules
  Sync:   $BIN --data-dir "$DATA_DIR" sync --dry-run

The release is ad-hoc signed, not notarized.
EOF
