#!/bin/sh
set -e

if [ "${BROWSER_ENABLED}" = "true" ]; then
    # Clean up Chrome locks from previous crashes
    PROFILES_DIR="${BRIDGE_STATE_DIR:-/shared/.pinchtab}/profiles"
    if [ -d "${PROFILES_DIR}" ]; then
        find "${PROFILES_DIR}" -name "SingletonLock" -delete 2>/dev/null || true
        find "${PROFILES_DIR}" -name ".com.google.Chrome.SingletonLock" -delete 2>/dev/null || true
    fi

    /usr/local/bin/pinchtab &
    PINCHTAB_PID=$!

    # Wait for Pinchtab to become healthy (up to 30s).
    # The runner also polls /health externally, but this avoids a race where
    # shell-daemon starts accepting commands before Pinchtab is ready.
    HEALTH_URL="http://${BRIDGE_BIND:-127.0.0.1}:${BRIDGE_PORT:-9867}/health"
    RETRIES=0
    MAX_RETRIES=30
    while [ $RETRIES -lt $MAX_RETRIES ]; do
        if curl -sf -o /dev/null "$HEALTH_URL" 2>/dev/null; then
            break
        fi
        # Check if Pinchtab process is still alive
        if ! kill -0 $PINCHTAB_PID 2>/dev/null; then
            echo "WARNING: Pinchtab process exited unexpectedly" >&2
            break
        fi
        RETRIES=$((RETRIES + 1))
        sleep 1
    done
    if [ $RETRIES -eq $MAX_RETRIES ]; then
        echo "WARNING: Pinchtab health check timed out after ${MAX_RETRIES}s" >&2
    fi
fi

exec /usr/local/bin/shell-daemon "$@"
