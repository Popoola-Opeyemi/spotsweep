#!/usr/bin/env bash
# Update Formula/spotsweep.rb after building a release.
# Usage: ./scripts/update-formula-sha.sh [VERSION]
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_DIR"

VERSION="${1:-$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)}"
ZIP="$REPO_DIR/dist/spotsweep-macos-universal.zip"
FORMULA="$REPO_DIR/Formula/spotsweep.rb"

if [[ ! -f "$ZIP" ]]; then
  echo "Missing $ZIP; run ./scripts/build-release.sh first." >&2
  exit 1
fi

HASH="$(shasum -a 256 "$ZIP" | awk '{print $1}')"
perl -0pi -e "s{releases/download/v[0-9.]+/spotsweep-macos-universal\.zip}{releases/download/v$VERSION/spotsweep-macos-universal.zip}g" "$FORMULA"
perl -0pi -e "s{sha256 \"[a-f0-9]+\"}{sha256 \"$HASH\"}g" "$FORMULA"

echo "Formula updated for v$VERSION"
echo "  sha256: $HASH"
