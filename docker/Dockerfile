# Multi-stage Dockerfile for Oxydra guest images.
# Build both targets from the repo root:
#   docker build --target oxydra-vm -t oxydra-vm:latest -f docker/Dockerfile .
#   docker build --target shell-vm  -t shell-vm:latest  -f docker/Dockerfile .

FROM rust:1-trixie AS builder

WORKDIR /build
COPY . .

RUN apt-get update && apt-get install -y cmake pip && rm -rf /var/lib/apt/lists/*
RUN pip3 install --break-system-packages ziglang && cargo install cargo-zigbuild

# Detect architecture and set the appropriate musl target
RUN case "$(uname -m)" in \
      x86_64|amd64) echo "x86_64-unknown-linux-musl" > /tmp/rust-target ;; \
      aarch64|arm64) echo "aarch64-unknown-linux-musl" > /tmp/rust-target ;; \
      *) echo "unsupported architecture: $(uname -m)" && exit 1 ;; \
    esac

RUN rustup target add wasm32-wasip1 "$(cat /tmp/rust-target)"
RUN cargo zigbuild --release --target "$(cat /tmp/rust-target)" --bin oxydra-vm --bin shell-daemon

# --- oxydra-vm image ---
FROM alpine:3 AS oxydra-vm

RUN apk add --no-cache ca-certificates
COPY --from=builder /build/target/*/release/oxydra-vm /usr/local/bin/oxydra-vm
ENTRYPOINT ["/usr/local/bin/oxydra-vm"]

# --- shell-vm (shell-daemon sidecar) image ---
FROM debian:trixie-slim AS shell-vm

RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates chromium && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/*/release/shell-daemon /usr/local/bin/shell-daemon
ENTRYPOINT ["/usr/local/bin/shell-daemon"]
