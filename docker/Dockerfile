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
RUN strip /build/target/*/release/oxydra-vm /build/target/*/release/shell-daemon || true

# --- oxydra-vm image ---
FROM alpine:3 AS oxydra-vm

RUN apk add --no-cache ca-certificates tzdata \
    && addgroup -S oxydra \
    && adduser -S -D -H -G oxydra oxydra
ENV HOME=/tmp/oxydra-home \
    XDG_CONFIG_HOME=/tmp/oxydra-home/.config \
    XDG_CACHE_HOME=/tmp/oxydra-home/.cache \
    XDG_STATE_HOME=/tmp/oxydra-home/.local/state \
    TZ=UTC
COPY --from=builder /build/target/*/release/oxydra-vm /usr/local/bin/oxydra-vm
USER oxydra:oxydra
ENTRYPOINT ["/usr/local/bin/oxydra-vm"]

# --- shell-vm (shell-daemon sidecar) image ---
FROM alpine:3 AS shell-vm

ARG PINCHTAB_VERSION=0.7.6

RUN apk add --no-cache \
    ca-certificates \
    curl \
    jq \
    nss \
    tzdata \
    ttf-freefont \
    chromium \
    && addgroup -S oxydra \
    && adduser -S -D -H -G oxydra oxydra

# Download Pinchtab binary for the target architecture.
RUN ARCH=$(uname -m) && \
    case "$ARCH" in \
      x86_64)  ARCH="amd64" ;; \
      aarch64) ARCH="arm64" ;; \
      *) echo "Unsupported arch: $ARCH" && exit 1 ;; \
    esac && \
    curl -fSL "https://github.com/pinchtab/pinchtab/releases/download/v${PINCHTAB_VERSION}/pinchtab-linux-${ARCH}" \
      -o /usr/local/bin/pinchtab && \
    chmod +x /usr/local/bin/pinchtab

COPY --from=builder /build/target/*/release/shell-daemon /usr/local/bin/shell-daemon
COPY docker/chromium-wrapper.sh /usr/local/bin/chromium-wrapper
RUN chmod +x /usr/local/bin/chromium-wrapper
COPY docker/shell-vm-entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh
ENV HOME=/tmp/oxydra-home \
    XDG_CONFIG_HOME=/tmp/oxydra-home/.config \
    XDG_CACHE_HOME=/tmp/oxydra-home/.cache \
    XDG_STATE_HOME=/tmp/oxydra-home/.local/state \
    TZ=UTC
USER oxydra:oxydra
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
