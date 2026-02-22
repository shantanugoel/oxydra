# Multi-stage Dockerfile for Oxydra guest images.
# Build both targets from the repo root:
#   docker build --target oxydra-vm -t oxydra-vm:latest -f docker/Dockerfile .
#   docker build --target shell-vm  -t shell-vm:latest  -f docker/Dockerfile .

FROM rust:1-trixie AS builder

WORKDIR /build
COPY . .

RUN rustup target add wasm32-wasip1
RUN cargo build --release --bin oxydra-vm --bin shell-daemon

# --- oxydra-vm image ---
FROM debian:trixie-slim AS oxydra-vm

RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/oxydra-vm /usr/local/bin/oxydra-vm
ENTRYPOINT ["/usr/local/bin/oxydra-vm"]

# --- shell-vm (shell-daemon sidecar) image ---
FROM debian:trixie-slim AS shell-vm

RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates chromium && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/shell-daemon /usr/local/bin/shell-daemon
ENTRYPOINT ["/usr/local/bin/shell-daemon"]
