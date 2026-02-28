#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Build Oxydra release artifacts and guest Docker images.

Usage:
  ./scripts/build-release-assets.sh [options]

Options:
  --tag <tag>                     Artifact/image tag (default: latest)
  --platforms <csv>               Comma-separated: macos-arm64,linux-amd64,linux-arm64
                                  (default: macos-arm64,linux-amd64,linux-arm64)
  --no-docker                     Skip Docker image builds
  --push-docker                   Push Docker images to remote registry
  --registry <ghcr|dockerhub>     Target registry when pushing (default: ghcr)
                                  (can also be provided via IMAGE_REGISTRY env var)
  --image-namespace <name>        Image namespace/org/user (for ghcr: owner/org path)
                                  (can also be provided via IMAGE_NAMESPACE env var)
  --dockerhub-namespace <name>    Back-compat alias of --image-namespace + --registry dockerhub
  -h, --help                      Show this help

Examples:
  ./scripts/build-release-assets.sh
  ./scripts/build-release-assets.sh --platforms linux-amd64,linux-arm64 --tag v0.2.0
  ./scripts/build-release-assets.sh --platforms linux-amd64,linux-arm64 --tag v0.2.0 --push-docker
  ./scripts/build-release-assets.sh --platforms linux-amd64,linux-arm64 --tag v0.2.0 \
    --push-docker --registry dockerhub --image-namespace your-org
EOF
}

TAG="latest"
PLATFORMS_CSV="macos-arm64,linux-amd64,linux-arm64"
BUILD_DOCKER=true
PUSH_DOCKER=false
IMAGE_REGISTRY="${IMAGE_REGISTRY:-ghcr}"
IMAGE_NAMESPACE="${IMAGE_NAMESPACE:-}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --tag)
      TAG="${2:?Missing value for --tag}"
      shift 2
      ;;
    --platforms)
      PLATFORMS_CSV="${2:?Missing value for --platforms}"
      shift 2
      ;;
    --no-docker)
      BUILD_DOCKER=false
      shift
      ;;
    --push-docker)
      PUSH_DOCKER=true
      shift
      ;;
    --registry)
      IMAGE_REGISTRY="${2:?Missing value for --registry}"
      shift 2
      ;;
    --image-namespace)
      IMAGE_NAMESPACE="${2:?Missing value for --image-namespace}"
      shift 2
      ;;
    --dockerhub-namespace)
      IMAGE_NAMESPACE="${2:?Missing value for --dockerhub-namespace}"
      IMAGE_REGISTRY="dockerhub"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown argument: $1" >&2
      usage >&2
      exit 1
      ;;
  esac
done

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIST_DIR="$REPO_ROOT/dist"
DOCKERFILE_PREBUILT="$REPO_ROOT/docker/Dockerfile.prebuilt"

BUILD_MACOS_ARM64=false
BUILD_LINUX_AMD64=false
BUILD_LINUX_ARM64=false

IFS=',' read -r -a REQUESTED_PLATFORMS <<< "$PLATFORMS_CSV"
for platform in "${REQUESTED_PLATFORMS[@]}"; do
  case "$platform" in
    macos-arm64) BUILD_MACOS_ARM64=true ;;
    linux-amd64) BUILD_LINUX_AMD64=true ;;
    linux-arm64) BUILD_LINUX_ARM64=true ;;
    *)
      echo "Unsupported platform: $platform" >&2
      echo "Supported values: macos-arm64, linux-amd64, linux-arm64" >&2
      exit 1
      ;;
  esac
done

if ! command -v cargo >/dev/null 2>&1; then
  echo "Error: cargo is required." >&2
  exit 1
fi

if ! command -v rustup >/dev/null 2>&1; then
  echo "Error: rustup is required." >&2
  exit 1
fi

if ! command -v python3 >/dev/null 2>&1; then
  echo "Error: python3 is required." >&2
  exit 1
fi

if "$BUILD_LINUX_AMD64" || "$BUILD_LINUX_ARM64"; then
  if ! command -v cargo-zigbuild >/dev/null 2>&1; then
    echo "Error: cargo-zigbuild is required for linux cross-builds." >&2
    echo "Install with: cargo install cargo-zigbuild" >&2
    exit 1
  fi
  if ! command -v zig >/dev/null 2>&1; then
    echo "Error: zig is required for linux cross-builds." >&2
    echo "Install zig from https://ziglang.org/download/" >&2
    exit 1
  fi
fi

if "$BUILD_DOCKER"; then
  if ! command -v docker >/dev/null 2>&1; then
    echo "Error: docker is required when Docker builds are enabled." >&2
    exit 1
  fi
fi

case "$IMAGE_REGISTRY" in
  ghcr|dockerhub) ;;
  *)
    echo "Error: --registry must be ghcr or dockerhub" >&2
    exit 1
    ;;
esac

if "$PUSH_DOCKER" && [[ -z "$IMAGE_NAMESPACE" ]]; then
  if [[ "$IMAGE_REGISTRY" == "ghcr" ]]; then
    IMAGE_NAMESPACE="${GITHUB_REPOSITORY_OWNER:-}"
  else
    IMAGE_NAMESPACE="${DOCKERHUB_NAMESPACE:-${DOCKERHUB_USERNAME:-}}"
  fi
fi

if [[ "$IMAGE_REGISTRY" == "ghcr" && -n "$IMAGE_NAMESPACE" ]]; then
  IMAGE_NAMESPACE="$(printf '%s' "$IMAGE_NAMESPACE" | tr '[:upper:]' '[:lower:]')"
fi

if "$PUSH_DOCKER" && [[ -z "$IMAGE_NAMESPACE" ]]; then
  echo "Error: --push-docker requires --image-namespace (or IMAGE_NAMESPACE env var)." >&2
  exit 1
fi

if "$BUILD_MACOS_ARM64"; then
  if [[ "$(uname -s)" != "Darwin" || "$(uname -m)" != "arm64" ]]; then
    echo "Error: macos-arm64 builds require running on a macOS arm64 host." >&2
    echo "Use --platforms linux-amd64,linux-arm64 on non-macOS hosts." >&2
    exit 1
  fi
fi

mkdir -p "$DIST_DIR"

rustup target add wasm32-wasip1 >/dev/null 2>&1 || true
rustup target add x86_64-unknown-linux-musl >/dev/null 2>&1 || true
rustup target add aarch64-unknown-linux-musl >/dev/null 2>&1 || true
if "$BUILD_MACOS_ARM64"; then
  rustup target add aarch64-apple-darwin >/dev/null 2>&1 || true
fi

CARGO_TARGET_DIR="$(cargo metadata --no-deps --format-version 1 | python3 -c 'import sys,json; print(json.load(sys.stdin)["target_directory"])')"

build_wasm_guest() {
  local wasm_target_dir="$CARGO_TARGET_DIR/wasm-guest-build"
  echo "Building wasm guest into $wasm_target_dir ..."
  cargo build \
    --target wasm32-wasip1 \
    --release \
    -p wasm-guest \
    --target-dir "$wasm_target_dir"

  mkdir -p "$REPO_ROOT/crates/tools/guest"
  cp "$wasm_target_dir/wasm32-wasip1/release/oxydra_wasm_guest.wasm" \
     "$REPO_ROOT/crates/tools/guest/oxydra_wasm_guest.wasm"
}

copy_release_binaries() {
  local target="$1"
  local out_dir="$2"
  mkdir -p "$out_dir"
  cp "$CARGO_TARGET_DIR/$target/release/runner" "$out_dir/runner"
  cp "$CARGO_TARGET_DIR/$target/release/oxydra-vm" "$out_dir/oxydra-vm"
  cp "$CARGO_TARGET_DIR/$target/release/shell-daemon" "$out_dir/shell-daemon"
  cp "$CARGO_TARGET_DIR/$target/release/oxydra-tui" "$out_dir/oxydra-tui"
}

package_platform_tarball() {
  local platform="$1"
  local platform_dir="$DIST_DIR/$platform"
  local archive="$DIST_DIR/oxydra-${TAG}-${platform}.tar.gz"
  tar -C "$platform_dir" -czf "$archive" runner oxydra-vm shell-daemon oxydra-tui
}

if "$BUILD_LINUX_AMD64" || "$BUILD_LINUX_ARM64"; then
  build_wasm_guest
  ulimit -n 65536 || true
fi

if "$BUILD_LINUX_AMD64"; then
  echo "Building linux-amd64 release binaries ..."
  OXYDRA_WASM_PREBUILT=1 cargo zigbuild \
    --release \
    --target x86_64-unknown-linux-musl \
    --bin runner \
    --bin oxydra-vm \
    --bin shell-daemon \
    --bin oxydra-tui
  copy_release_binaries "x86_64-unknown-linux-musl" "$DIST_DIR/linux-amd64"
  package_platform_tarball "linux-amd64"
fi

if "$BUILD_LINUX_ARM64"; then
  echo "Building linux-arm64 release binaries ..."
  OXYDRA_WASM_PREBUILT=1 cargo zigbuild \
    --release \
    --target aarch64-unknown-linux-musl \
    --bin runner \
    --bin oxydra-vm \
    --bin shell-daemon \
    --bin oxydra-tui
  copy_release_binaries "aarch64-unknown-linux-musl" "$DIST_DIR/linux-arm64"
  package_platform_tarball "linux-arm64"
fi

if "$BUILD_MACOS_ARM64"; then
  echo "Building macos-arm64 release binaries ..."
  cargo build \
    --release \
    --target aarch64-apple-darwin \
    --bin runner \
    --bin oxydra-vm \
    --bin shell-daemon \
    --bin oxydra-tui
  copy_release_binaries "aarch64-apple-darwin" "$DIST_DIR/macos-arm64"
  package_platform_tarball "macos-arm64"
fi

if "$BUILD_DOCKER" && { "$BUILD_LINUX_AMD64" || "$BUILD_LINUX_ARM64"; }; then
  STAGING_DIR="$REPO_ROOT/docker/staging"
  rm -rf "$STAGING_DIR"
  mkdir -p "$STAGING_DIR"

  if [[ -n "$IMAGE_NAMESPACE" ]]; then
    if [[ "$IMAGE_REGISTRY" == "ghcr" ]]; then
      IMAGE_PREFIX="ghcr.io/${IMAGE_NAMESPACE}"
    else
      IMAGE_PREFIX="${IMAGE_NAMESPACE}"
    fi
    OXYDRA_IMAGE="${IMAGE_PREFIX}/oxydra-vm"
    SHELL_IMAGE="${IMAGE_PREFIX}/shell-vm"
  else
    OXYDRA_IMAGE="oxydra-vm"
    SHELL_IMAGE="shell-vm"
  fi

  OXYDRA_MANIFEST_SOURCES=()
  SHELL_MANIFEST_SOURCES=()

  if "$BUILD_LINUX_AMD64"; then
    mkdir -p "$STAGING_DIR/linux-amd64"
    cp "$CARGO_TARGET_DIR/x86_64-unknown-linux-musl/release/oxydra-vm" "$STAGING_DIR/linux-amd64/oxydra-vm"
    cp "$CARGO_TARGET_DIR/x86_64-unknown-linux-musl/release/shell-daemon" "$STAGING_DIR/linux-amd64/shell-daemon"

    docker build \
      --platform linux/amd64 \
      --build-arg "BINARY=docker/staging/linux-amd64/oxydra-vm" \
      --target oxydra-vm \
      -t "${OXYDRA_IMAGE}:${TAG}-linux-amd64" \
      -f "$DOCKERFILE_PREBUILT" \
      "$REPO_ROOT"

    docker build \
      --platform linux/amd64 \
      --build-arg "BINARY=docker/staging/linux-amd64/shell-daemon" \
      --target shell-vm \
      -t "${SHELL_IMAGE}:${TAG}-linux-amd64" \
      -f "$DOCKERFILE_PREBUILT" \
      "$REPO_ROOT"

    OXYDRA_MANIFEST_SOURCES+=("${OXYDRA_IMAGE}:${TAG}-linux-amd64")
    SHELL_MANIFEST_SOURCES+=("${SHELL_IMAGE}:${TAG}-linux-amd64")
  fi

  if "$BUILD_LINUX_ARM64"; then
    mkdir -p "$STAGING_DIR/linux-arm64"
    cp "$CARGO_TARGET_DIR/aarch64-unknown-linux-musl/release/oxydra-vm" "$STAGING_DIR/linux-arm64/oxydra-vm"
    cp "$CARGO_TARGET_DIR/aarch64-unknown-linux-musl/release/shell-daemon" "$STAGING_DIR/linux-arm64/shell-daemon"

    docker build \
      --platform linux/arm64 \
      --build-arg "BINARY=docker/staging/linux-arm64/oxydra-vm" \
      --target oxydra-vm \
      -t "${OXYDRA_IMAGE}:${TAG}-linux-arm64" \
      -f "$DOCKERFILE_PREBUILT" \
      "$REPO_ROOT"

    docker build \
      --platform linux/arm64 \
      --build-arg "BINARY=docker/staging/linux-arm64/shell-daemon" \
      --target shell-vm \
      -t "${SHELL_IMAGE}:${TAG}-linux-arm64" \
      -f "$DOCKERFILE_PREBUILT" \
      "$REPO_ROOT"

    OXYDRA_MANIFEST_SOURCES+=("${OXYDRA_IMAGE}:${TAG}-linux-arm64")
    SHELL_MANIFEST_SOURCES+=("${SHELL_IMAGE}:${TAG}-linux-arm64")
  fi

  if "$PUSH_DOCKER"; then
    for image in "${OXYDRA_MANIFEST_SOURCES[@]}"; do
      docker push "$image"
    done
    for image in "${SHELL_MANIFEST_SOURCES[@]}"; do
      docker push "$image"
    done

    docker manifest rm "${OXYDRA_IMAGE}:${TAG}" >/dev/null 2>&1 || true
    docker manifest rm "${SHELL_IMAGE}:${TAG}" >/dev/null 2>&1 || true

    docker manifest create "${OXYDRA_IMAGE}:${TAG}" "${OXYDRA_MANIFEST_SOURCES[@]}"
    docker manifest create "${SHELL_IMAGE}:${TAG}" "${SHELL_MANIFEST_SOURCES[@]}"
    docker manifest push "${OXYDRA_IMAGE}:${TAG}"
    docker manifest push "${SHELL_IMAGE}:${TAG}"
  fi

  rm -rf "$STAGING_DIR"
fi

if ls "$DIST_DIR"/oxydra-"$TAG"-*.tar.gz >/dev/null 2>&1; then
  shasum -a 256 "$DIST_DIR"/oxydra-"$TAG"-*.tar.gz > "$DIST_DIR/SHA256SUMS"
fi

echo "Build complete."
echo "Artifacts: $DIST_DIR/oxydra-${TAG}-<platform>.tar.gz"
if "$BUILD_DOCKER"; then
  if [[ -n "$IMAGE_NAMESPACE" ]]; then
    if [[ "$IMAGE_REGISTRY" == "ghcr" ]]; then
      echo "Docker images: ghcr.io/${IMAGE_NAMESPACE}/oxydra-vm:${TAG}-linux-{amd64,arm64}, ghcr.io/${IMAGE_NAMESPACE}/shell-vm:${TAG}-linux-{amd64,arm64}"
    else
      echo "Docker images: ${IMAGE_NAMESPACE}/oxydra-vm:${TAG}-linux-{amd64,arm64}, ${IMAGE_NAMESPACE}/shell-vm:${TAG}-linux-{amd64,arm64}"
    fi
  else
    echo "Docker images: oxydra-vm:${TAG}-linux-{amd64,arm64}, shell-vm:${TAG}-linux-{amd64,arm64}"
  fi
fi
