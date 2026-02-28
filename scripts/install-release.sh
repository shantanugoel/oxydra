#!/usr/bin/env bash
set -euo pipefail

REPO="shantanugoel/oxydra"
TAG=""
INSTALL_DIR="${OXYDRA_INSTALL_DIR:-$HOME/.local/bin}"
SYSTEM_INSTALL=false
BASE_DIR="."
SKIP_CONFIG=false
OVERWRITE_CONFIG=false

SCRIPT_DIR=""
if [[ -n "${BASH_SOURCE[0]:-}" ]]; then
  SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" 2>/dev/null && pwd || true)"
fi

usage() {
  cat <<'EOF'
Install Oxydra binaries from GitHub Releases.

Usage:
  install-release.sh [options]

Options:
  --tag <tag>            Install a specific release tag (for example: v0.3.0)
                         If omitted, installs the latest release.
  --repo <owner/name>    GitHub repository (default: shantanugoel/oxydra)
  --install-dir <path>   Target directory for binaries (default: ~/.local/bin)
  --system               Install to /usr/local/bin (uses sudo when needed)
  --base-dir <path>      Base directory where .oxydra config templates are written
                         (default: current directory)
  --skip-config          Install binaries only (skip writing .oxydra config templates)
  --overwrite-config     Replace existing .oxydra template files if present
  -h, --help             Show help

Examples:
  install-release.sh
  install-release.sh --tag v0.3.0
  install-release.sh --tag v0.3.0 --system
  install-release.sh --tag v0.3.0 --base-dir /path/to/workspace
  install-release.sh --repo myorg/oxydra-fork --tag v0.3.0
EOF
}

log() {
  printf '[oxydra-install] %s\n' "$*"
}

fail() {
  printf '[oxydra-install] Error: %s\n' "$*" >&2
  exit 1
}

resolve_latest_tag() {
  local response
  local api_url="https://api.github.com/repos/${REPO}/releases/latest"

  if ! response="$(curl -fsSL \
    -H 'Accept: application/vnd.github+json' \
    -H 'User-Agent: oxydra-install-script' \
    "$api_url")"; then
    fail "failed to query latest release from ${api_url}. Check network access or pass --tag explicitly."
  fi

  local latest
  latest="$(printf '%s\n' "$response" | sed -n 's/^[[:space:]]*"tag_name":[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1)"
  [[ -n "$latest" ]] || fail "could not determine latest release tag from GitHub API response"
  printf '%s' "$latest"
}

detect_platform() {
  local os arch
  os="$(uname -s)"
  arch="$(uname -m)"

  case "$os" in
    Darwin)
      case "$arch" in
        arm64|aarch64)
          printf '%s' "macos-arm64"
          ;;
        x86_64|amd64)
          fail "macOS x86_64 release artifacts are not published. Build from source instead."
          ;;
        *)
          fail "unsupported macOS architecture: ${arch}"
          ;;
      esac
      ;;
    Linux)
      case "$arch" in
        x86_64|amd64)
          printf '%s' "linux-amd64"
          ;;
        aarch64|arm64)
          printf '%s' "linux-arm64"
          ;;
        *)
          fail "unsupported Linux architecture: ${arch}"
          ;;
      esac
      ;;
    *)
      fail "unsupported OS: ${os}. Supported: macOS (arm64), Linux (amd64/arm64)."
      ;;
  esac
}

install_binary_local() {
  local source="$1"
  local destination="$2"

  if command -v install >/dev/null 2>&1; then
    install -m 0755 "$source" "$destination"
  else
    cp "$source" "$destination"
    chmod 0755 "$destination"
  fi
}

install_binary_system() {
  local source="$1"
  local destination="$2"

  if command -v install >/dev/null 2>&1; then
    sudo install -m 0755 "$source" "$destination"
  else
    sudo cp "$source" "$destination"
    sudo chmod 0755 "$destination"
  fi
}

copy_or_download_config_template() {
  local source_name="$1"
  local destination="$2"
  local local_source="${SCRIPT_DIR}/../examples/config/${source_name}"

  if [[ -f "$destination" && "$OVERWRITE_CONFIG" != "true" ]]; then
    log "Config exists, leaving unchanged: ${destination}"
    return
  fi

  mkdir -p "$(dirname "$destination")"

  if [[ -n "$SCRIPT_DIR" && -f "$local_source" ]]; then
    cp "$local_source" "$destination"
    log "Copied template: ${destination}"
    return
  fi

  local source_url="https://raw.githubusercontent.com/${REPO}/${TAG}/examples/config/${source_name}"
  if ! curl -fL --retry 3 -o "$destination" "$source_url"; then
    fail "failed to fetch config template ${source_name} from ${source_url}"
  fi
  log "Downloaded template: ${destination}"
}

initialize_config_templates() {
  local base_dir="$1"
  local config_root="${base_dir}/.oxydra"
  local users_dir="${config_root}/users"

  mkdir -p "$users_dir"

  copy_or_download_config_template "agent.toml" "${config_root}/agent.toml"
  copy_or_download_config_template "runner.toml" "${config_root}/runner.toml"
  copy_or_download_config_template "runner-user.toml" "${users_dir}/alice.toml"

  cat <<EOF
[oxydra-install] Config templates are ready in ${config_root}
[oxydra-install] Update these values before first run:
  1) ${config_root}/runner.toml
     - set default_tier = "container" (or "process" / "micro_vm")
     - set [guest_images] to:
       oxydra_vm = "ghcr.io/shantanugoel/oxydra-vm:${TAG}"
       shell_vm  = "ghcr.io/shantanugoel/shell-vm:${TAG}"
  2) ${config_root}/agent.toml
     - set [selection].provider and [selection].model
     - ensure matching [providers.registry.<name>] api_key_env is correct
  3) Export your provider API key environment variable:
     OPENAI_API_KEY or ANTHROPIC_API_KEY or GEMINI_API_KEY
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --tag)
      TAG="${2:?Missing value for --tag}"
      shift 2
      ;;
    --repo)
      REPO="${2:?Missing value for --repo}"
      shift 2
      ;;
    --install-dir)
      INSTALL_DIR="${2:?Missing value for --install-dir}"
      shift 2
      ;;
    --system)
      SYSTEM_INSTALL=true
      shift
      ;;
    --base-dir)
      BASE_DIR="${2:?Missing value for --base-dir}"
      shift 2
      ;;
    --skip-config)
      SKIP_CONFIG=true
      shift
      ;;
    --overwrite-config)
      OVERWRITE_CONFIG=true
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      fail "unknown argument: $1"
      ;;
  esac
done

if [[ "$SYSTEM_INSTALL" == "true" ]]; then
  INSTALL_DIR="/usr/local/bin"
fi

command -v curl >/dev/null 2>&1 || fail "curl is required"
command -v tar >/dev/null 2>&1 || fail "tar is required"

[[ -n "$TAG" ]] || TAG="$(resolve_latest_tag)"
PLATFORM="$(detect_platform)"

if ! mkdir -p "$BASE_DIR"; then
  fail "failed to create base directory: ${BASE_DIR}"
fi
BASE_DIR="$(cd "$BASE_DIR" && pwd)"

ARCHIVE="oxydra-${TAG}-${PLATFORM}.tar.gz"
DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${TAG}/${ARCHIVE}"

TMP_DIR="$(mktemp -d)"
cleanup() {
  rm -rf "$TMP_DIR"
}
trap cleanup EXIT

log "Installing Oxydra ${TAG} (${PLATFORM})"
log "Downloading ${DOWNLOAD_URL}"

if ! curl -fL --retry 3 -o "$TMP_DIR/$ARCHIVE" "$DOWNLOAD_URL"; then
  fail "download failed. Verify the tag/repository and that release assets are published for ${PLATFORM}."
fi

tar -xzf "$TMP_DIR/$ARCHIVE" -C "$TMP_DIR"

binaries=(runner oxydra-vm shell-daemon oxydra-tui)
for binary in "${binaries[@]}"; do
  [[ -f "$TMP_DIR/$binary" ]] || fail "archive missing binary: ${binary}"
done

if [[ "$SYSTEM_INSTALL" == "true" ]]; then
  if [[ "$(id -u)" -eq 0 ]]; then
    mkdir -p "$INSTALL_DIR"
    for binary in "${binaries[@]}"; do
      install_binary_local "$TMP_DIR/$binary" "$INSTALL_DIR/$binary"
    done
  else
    command -v sudo >/dev/null 2>&1 || fail "--system requires sudo (or run as root)"
    sudo mkdir -p "$INSTALL_DIR"
    for binary in "${binaries[@]}"; do
      install_binary_system "$TMP_DIR/$binary" "$INSTALL_DIR/$binary"
    done
  fi
else
  mkdir -p "$INSTALL_DIR"
  for binary in "${binaries[@]}"; do
    install_binary_local "$TMP_DIR/$binary" "$INSTALL_DIR/$binary"
  done
fi

log "Installed binaries to ${INSTALL_DIR}:"
for binary in "${binaries[@]}"; do
  log "  - ${binary}"
done

if [[ ":$PATH:" != *":$INSTALL_DIR:"* ]]; then
  cat <<EOF
[oxydra-install] ${INSTALL_DIR} is not in PATH.
[oxydra-install] Add it with:
  export PATH="${INSTALL_DIR}:\$PATH"
EOF
fi

if [[ "$SKIP_CONFIG" != "true" ]]; then
  initialize_config_templates "$BASE_DIR"
else
  log "Skipping config template initialization (--skip-config)"
fi

cat <<EOF
[oxydra-install] Done.
[oxydra-install] Installed tag: ${TAG}
[oxydra-install] Workspace base directory: ${BASE_DIR}
[oxydra-install] Start the runner with:
  runner --config "${BASE_DIR}/.oxydra/runner.toml" --user alice start
EOF
