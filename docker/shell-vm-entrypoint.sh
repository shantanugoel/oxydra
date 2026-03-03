#!/bin/sh
set -e

PINCHTAB_PID_FILE="/tmp/pinchtab.pid"
HEALTH_URL="http://${BRIDGE_BIND:-127.0.0.1}:${BRIDGE_PORT:-9867}/health"

is_numeric_pid() {
    case "${1}" in
        ''|*[!0-9]*)
            return 1
            ;;
        *)
            return 0
            ;;
    esac
}

pinchtab_is_healthy() {
    pid=$(cat "${PINCHTAB_PID_FILE}" 2>/dev/null || echo "")
    if ! is_numeric_pid "${pid}" || ! kill -0 "${pid}" 2>/dev/null; then
        return 1
    fi
    curl -sf --max-time 3 -o /dev/null "${HEALTH_URL}" 2>/dev/null
}

pinchtab_watchdog() {
    while true; do
        sleep 10
        if ! pinchtab_is_healthy; then
            echo "WATCHDOG: Pinchtab unhealthy or exited - restarting..." >&2
            PINCHTAB_RESTART_IF_UNHEALTHY=1 /usr/local/bin/restart-browser || true
        fi
    done
}

if [ "${BROWSER_ENABLED}" = "true" ]; then
    /usr/local/bin/restart-browser || true

    # Launch watchdog in the background; it survives the exec below.
    pinchtab_watchdog &
fi

exec /usr/local/bin/shell-daemon "$@"
