#!/usr/bin/env bash
# Build signed (ad-hoc) universal and per-architecture macOS release artifacts.
#
# Requirements: Xcode Command Line Tools, rustup targets:
#   rustup target add aarch64-apple-darwin x86_64-apple-darwin
#
# Output (dist/):
#   spotlight-dev-macos-universal.zip
#   spotlight-dev-aarch64-apple-darwin.tar.gz
#   spotlight-dev-x86_64-apple-darwin.tar.gz
#   SHA256SUMS
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_DIR"

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
if [[ -z "$VERSION" ]]; then
  echo "Could not read version from Cargo.toml" >&2
  exit 1
fi
DIST="$REPO_DIR/dist"
TARGETS=(aarch64-apple-darwin x86_64-apple-darwin)
DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-13.0}"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "Release artifacts must be built on macOS." >&2
  exit 1
fi

if command -v rustup >/dev/null 2>&1; then
  for target in "${TARGETS[@]}"; do
    if ! rustup target list --installed | grep -qx "$target"; then
      echo "Installing Rust target $target..."
      rustup target add "$target"
    fi
  done
fi

export MACOSX_DEPLOYMENT_TARGET="$DEPLOYMENT_TARGET"
for target in "${TARGETS[@]}"; do
  echo "Building $target (deployment target $DEPLOYMENT_TARGET)..."
  cargo build --release --target "$target"
done

rm -rf "$DIST"
mkdir -p "$DIST/stage-universal"

UNIVERSAL="$DIST/stage-universal/spotlight-dev"
lipo -create \
  "$REPO_DIR/target/aarch64-apple-darwin/release/spotlight-dev" \
  "$REPO_DIR/target/x86_64-apple-darwin/release/spotlight-dev" \
  -output "$UNIVERSAL"
chmod 0755 "$UNIVERSAL"

echo "Universal architectures: $(lipo -archs "$UNIVERSAL")"
cp "$REPO_DIR/LICENSE" "$DIST/stage-universal/LICENSE"
cp "$REPO_DIR/README.md" "$DIST/stage-universal/README.md"
cp "$REPO_DIR/docs/PRIVATE_API.md" "$DIST/stage-universal/PRIVATE_API.md"
codesign --force --sign - --timestamp=none "$UNIVERSAL"
ditto -c -k --sequesterRsrc --keepParent "$DIST/stage-universal" \
  "$DIST/spotlight-dev-macos-universal.zip"

for target in "${TARGETS[@]}"; do
  stage="$DIST/stage-$target"
  mkdir -p "$stage"
  cp "$REPO_DIR/target/$target/release/spotlight-dev" "$stage/spotlight-dev"
  chmod 0755 "$stage/spotlight-dev"
  codesign --force --sign - --timestamp=none "$stage/spotlight-dev"
  cp "$REPO_DIR/LICENSE" "$stage/LICENSE"
  cp "$REPO_DIR/README.md" "$stage/README.md"
  cp "$REPO_DIR/docs/PRIVATE_API.md" "$stage/PRIVATE_API.md"
  tar -C "$DIST" -czf "$DIST/spotlight-dev-$target.tar.gz" "stage-$target"
done

rm -rf "$DIST"/stage-*
(
  cd "$DIST"
  shasum -a 256 spotlight-dev-macos-universal.zip \
    spotlight-dev-aarch64-apple-darwin.tar.gz \
    spotlight-dev-x86_64-apple-darwin.tar.gz > SHA256SUMS
)

echo
echo "Release $VERSION ready in $DIST"
cat "$DIST/SHA256SUMS"
