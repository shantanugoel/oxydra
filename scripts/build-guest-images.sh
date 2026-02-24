#!/usr/bin/env bash
set -euo pipefail
# Cross-compiles guest binaries locally and packages them into Docker images.
#
# Prerequisites:
#   - cargo-zigbuild: cargo install cargo-zigbuild
#   - zig:            brew install zig  (macOS) or https://ziglang.org/download/
#
# Usage: ./scripts/build-guest-images.sh <ARCH> [TAG]
#   ARCH: amd64 or arm64

ARCH="${1:?Usage: $0 <amd64|arm64> [TAG]}"
TAG="${2:-latest}"

case "$ARCH" in
  amd64) TARGET="x86_64-unknown-linux-musl"  ; PLATFORM="linux/amd64" ;;
  arm64) TARGET="aarch64-unknown-linux-musl"  ; PLATFORM="linux/arm64" ;;
  *)
    echo "Error: ARCH must be 'amd64' or 'arm64'" >&2
    exit 1
    ;;
esac

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# ── preflight checks ───────────────────────────────────────────────
if ! command -v cargo-zigbuild &>/dev/null; then
  echo "Error: cargo-zigbuild is required." >&2
  echo "  Install with:  cargo install cargo-zigbuild" >&2
  echo "  Also install zig:  brew install zig  (macOS)" >&2
  exit 1
fi

if ! command -v zig &>/dev/null; then
  echo "Error: zig is required." >&2
  echo "  Install with:  brew install zig  (macOS)" >&2
  echo "  Or download from: https://ziglang.org/download/" >&2
  exit 1
fi

rustup target add "$TARGET" 2>/dev/null || true
rustup target add wasm32-wasip1 2>/dev/null || true

# ── cross-compile ──────────────────────────────────────────────────
# Build the wasm guest first, since the tools build depends on it. We use a separate target dir to avoid deadlocks on Cargo's file lock.
# Let cargo resolve the target dir itself (respects build-dir, CARGO_TARGET_DIR, etc.)
TARGET_DIR=$(cargo metadata --no-deps --format-version 1 | \
  python3 -c "import sys,json; print(json.load(sys.stdin)['target_directory'])")

WASM_TARGET_DIR="$TARGET_DIR/wasm-guest-build"

echo "Building wasm guest into $WASM_TARGET_DIR ..."
cargo build \
  --target wasm32-wasip1 \
  --release \
  -p wasm-guest \
  --target-dir "$WASM_TARGET_DIR"

mkdir -p "$REPO_ROOT/crates/tools/guest"
cp "$WASM_TARGET_DIR/wasm32-wasip1/release/oxydra_wasm_guest.wasm" \
   "$REPO_ROOT/crates/tools/guest/oxydra_wasm_guest.wasm"


# macos has a low fd limit, need to increasse it for zig
ulimit -n 65536
echo "Cross-compiling oxydra-vm and shell-daemon for $TARGET ..."
OXYDRA_WASM_PREBUILT=1  cargo zigbuild --release --target "$TARGET" --bin oxydra-vm --bin shell-daemon

echo "Building Docker images for $PLATFORM ..."

# ── stage binaries for Docker (target/ is in .dockerignore) ───────
STAGING_DIR="$REPO_ROOT/docker/staging"
mkdir -p "$STAGING_DIR"

# Get the actual target dir cargo used (respects build-dir on SSD etc.)
CARGO_TARGET_DIR=$(cargo metadata --no-deps --format-version 1 | \
  python3 -c "import sys,json; print(json.load(sys.stdin)['target_directory'])")

cp "$CARGO_TARGET_DIR/$TARGET/release/oxydra-vm"    "$STAGING_DIR/oxydra-vm"
cp "$CARGO_TARGET_DIR/$TARGET/release/shell-daemon" "$STAGING_DIR/shell-daemon"

# ── package into Docker images ─────────────────────────────────────
docker build \
  --platform "$PLATFORM" \
  --build-arg "BINARY=docker/staging/oxydra-vm" \
  --target oxydra-vm \
  -t "oxydra-vm:$TAG" \
  -f "$REPO_ROOT/docker/Dockerfile.prebuilt" \
  "$REPO_ROOT"

docker build \
  --platform "$PLATFORM" \
  --build-arg "BINARY=docker/staging/shell-daemon" \
  --target shell-vm \
  -t "shell-vm:$TAG" \
  -f "$REPO_ROOT/docker/Dockerfile.prebuilt" \
  "$REPO_ROOT"

# ── cleanup staging ────────────────────────────────────────────────
rm -rf "$STAGING_DIR"