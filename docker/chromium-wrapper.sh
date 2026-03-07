#!/bin/sh
# Chromium stability wrapper for containerised/headless environments.
#
# Pinchtab reads CHROME_BINARY to determine the Chrome executable.  Pointing
# it here guarantees that the essential container-stability flags are always
# forwarded to Chromium regardless of how Pinchtab internally handles the
# separate CHROME_FLAGS environment variable (whose handling has had bugs in
# some Pinchtab versions).
#
# Two modes of operation:
#
# 1. LOCAL (default): launches the bundled Chromium with container-stability
#    flags. Flags applied:
#      --no-sandbox              Required: containers have no user-namespace support.
#      --disable-dev-shm-usage   /dev/shm is typically only 64 MB in Docker;
#                                Chrome uses it for IPC shared memory and
#                                crashes without this flag on the default size.
#      --disable-gpu             Headless containers have no GPU; avoids GPU-
#                                related crashes.
#      --disable-software-rasterizer
#                                Disables the software rasterizer that Chrome
#                                falls back to when no GPU is available — it is
#                                slow and can OOM-crash on constrained devices
#                                such as Raspberry Pi.
#      --js-flags=--max-old-space-size=256
#                                Caps V8 heap to 256 MB, preventing Chrome from
#                                consuming all available RAM on memory-constrained
#                                devices. The default (~1.5 GB) can cause OOM
#                                kills on Raspberry Pi and similar SBCs.
#      --disable-extensions      Reduces memory usage and startup time.
#      --disable-background-networking
#                                Disables background network activity (update
#                                checks, etc.) that wastes bandwidth and CPU on
#                                constrained devices.
#
# 2. EXTERNAL CDP (BROWSER_EXTERNAL_CDP_URL is set): connects Pinchtab to a
#    Chromium instance running outside the container.  The external Chromium
#    must be started with --remote-debugging-port and (if on another host)
#    --remote-debugging-address=0.0.0.0.
#
#    BROWSER_EXTERNAL_CDP_URL must be the HTTP DevTools base URL, e.g.:
#      http://192.168.1.100:9222   (remote host)
#      http://172.17.0.1:9222      (Docker host via bridge gateway)
#      http://127.0.0.1:9222       (same host, port-mapped into container)
#
#    In this mode the wrapper:
#      1. Fetches the WebSocket debugger URL from <url>/json/version.
#      2. Prints "DevTools listening on <ws_url>" — the exact format Pinchtab
#         waits for on the Chrome process stdout.
#      3. Sleeps indefinitely, keeping the "Chrome process" alive so Pinchtab
#         maintains its connection for the session lifetime.

if [ -n "${BROWSER_EXTERNAL_CDP_URL}" ]; then
    echo "INFO: external CDP mode — connecting to ${BROWSER_EXTERNAL_CDP_URL}" >&2

    # Fetch the WebSocket debugger URL from Chrome's DevTools HTTP API.
    WS_URL=$(curl -sf --max-time 10 "${BROWSER_EXTERNAL_CDP_URL}/json/version" 2>/dev/null \
        | jq -r '.webSocketDebuggerUrl // empty' 2>/dev/null)

    if [ -z "$WS_URL" ]; then
        echo "ERROR: could not fetch webSocketDebuggerUrl from ${BROWSER_EXTERNAL_CDP_URL}/json/version" >&2
        echo "       Is Chromium running with --remote-debugging-port on that host?" >&2
        exit 1
    fi

    # Mimic Chrome's startup log line so Pinchtab finds the WebSocket URL.
    echo "DevTools listening on ${WS_URL}"

    # Keep this process alive — Pinchtab holds it as the "Chrome process".
    # SIGTERM from Pinchtab on shutdown will cleanly kill sleep.
    exec sleep infinity
fi

# Local mode: discover and launch the bundled Chromium binary.
# The binary name varies across distros and Alpine versions.
CHROMIUM_BIN=$(command -v chromium-browser 2>/dev/null || command -v chromium 2>/dev/null)
if [ -z "$CHROMIUM_BIN" ]; then
    echo "ERROR: no chromium binary found in PATH" >&2
    exit 1
fi

exec "$CHROMIUM_BIN" \
    --no-sandbox \
    --disable-dev-shm-usage \
    --disable-gpu \
    --disable-software-rasterizer \
    --js-flags=--max-old-space-size=256 \
    --disable-extensions \
    --disable-background-networking \
    "$@"
