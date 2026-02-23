#!/usr/bin/env bash
set -euo pipefail
# Builds both guest Docker images from source.
# Usage: ./scripts/build-guest-images-in-docker.sh [TAG]

TAG="${1:-latest}"

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

docker build --platform linux/arm64 --target oxydra-vm -t "oxydra-vm:$TAG" -f "$REPO_ROOT/docker/Dockerfile" "$REPO_ROOT"
docker build --platform linux/arm64 --target shell-vm -t "shell-vm:$TAG" -f "$REPO_ROOT/docker/Dockerfile" "$REPO_ROOT"

echo "Built oxydra-vm:$TAG and shell-vm:$TAG"
