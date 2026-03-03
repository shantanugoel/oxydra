#!/bin/sh
# Restart the Pinchtab browser service.
# Safe to run from shell_exec or from the watchdog.
set -e

PINCHTAB_PID_FILE="/tmp/pinchtab.pid"
PINCHTAB_RESTART_LOCK_DIR="/tmp/pinchtab-restart.lock"
PROFILES_DIR="${BRIDGE_STATE_DIR:-/shared/.pinchtab}/profiles"
HEALTH_URL="http://${BRIDGE_BIND:-127.0.0.1}:${BRIDGE_PORT:-9867}/health"
MAX_HEALTH_RETRIES="${PINCHTAB_HEALTH_RETRIES:-30}"
LOCK_WAIT_SECS="${PINCHTAB_LOCK_WAIT_SECS:-30}"
RESTART_ONLY_IF_UNHEALTHY="${PINCHTAB_RESTART_IF_UNHEALTHY:-0}"
LOCK_ACQUIRED=0

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

acquire_restart_lock() {
    attempts=0
    while ! mkdir "${PINCHTAB_RESTART_LOCK_DIR}" 2>/dev/null; do
        holder_pid=""
        if [ -f "${PINCHTAB_RESTART_LOCK_DIR}/pid" ]; then
            holder_pid=$(cat "${PINCHTAB_RESTART_LOCK_DIR}/pid" 2>/dev/null || echo "")
            if is_numeric_pid "${holder_pid}" && ! kill -0 "${holder_pid}" 2>/dev/null; then
                rm -rf "${PINCHTAB_RESTART_LOCK_DIR}" 2>/dev/null || true
                continue
            fi
        fi
        attempts=$((attempts + 1))
        if [ "${attempts}" -ge "${LOCK_WAIT_SECS}" ]; then
            echo "ERROR: Timed out waiting for browser restart lock." >&2
            return 1
        fi
        sleep 1
    done
    echo "$$" > "${PINCHTAB_RESTART_LOCK_DIR}/pid"
    LOCK_ACQUIRED=1
}

release_restart_lock() {
    if [ "${LOCK_ACQUIRED}" -eq 1 ]; then
        rm -rf "${PINCHTAB_RESTART_LOCK_DIR}" 2>/dev/null || true
        LOCK_ACQUIRED=0
    fi
}

pinchtab_is_healthy() {
    pid=$(cat "${PINCHTAB_PID_FILE}" 2>/dev/null || echo "")
    if ! is_numeric_pid "${pid}" || ! kill -0 "${pid}" 2>/dev/null; then
        return 1
    fi
    curl -sf --max-time 3 -o /dev/null "${HEALTH_URL}" 2>/dev/null
}

# Remove Chrome singleton locks so a fresh instance can start cleanly.
clean_chrome_locks() {
    if [ -d "${PROFILES_DIR}" ]; then
        find "${PROFILES_DIR}" -name "SingletonLock" -delete 2>/dev/null || true
        find "${PROFILES_DIR}" -name ".com.google.Chrome.SingletonLock" -delete 2>/dev/null || true
    fi
}

stop_pinchtab() {
    pid=$(cat "${PINCHTAB_PID_FILE}" 2>/dev/null || echo "")
    rm -f "${PINCHTAB_PID_FILE}"
    if ! is_numeric_pid "${pid}" || ! kill -0 "${pid}" 2>/dev/null; then
        return 0
    fi

    cmdline=$(tr '\000' ' ' < "/proc/${pid}/cmdline" 2>/dev/null || echo "")
    case "${cmdline}" in
        *pinchtab*|*chromium*)
            ;;
        *)
            echo "WARNING: PID ${pid} does not look like Pinchtab; skipping stop." >&2
            return 0
            ;;
    esac

    echo "Stopping Pinchtab (PID ${pid})..." >&2
    pgrp=$(cut -d ' ' -f5 "/proc/${pid}/stat" 2>/dev/null || echo "")
    if [ "${pgrp}" = "${pid}" ]; then
        kill -TERM "-${pid}" 2>/dev/null || true
    fi
    kill -TERM "${pid}" 2>/dev/null || true

    retries=0
    while [ "${retries}" -lt 5 ]; do
        if ! kill -0 "${pid}" 2>/dev/null; then
            return 0
        fi
        retries=$((retries + 1))
        sleep 1
    done

    echo "Pinchtab did not exit after TERM; forcing stop." >&2
    if [ "${pgrp}" = "${pid}" ]; then
        kill -KILL "-${pid}" 2>/dev/null || true
    fi
    kill -KILL "${pid}" 2>/dev/null || true
}

# Launch Pinchtab in the background.
start_pinchtab() {
    clean_chrome_locks
    if command -v setsid >/dev/null 2>&1; then
        setsid /usr/local/bin/pinchtab &
    else
        /usr/local/bin/pinchtab &
    fi
    pid=$!
    echo "${pid}" > "${PINCHTAB_PID_FILE}"
    echo "Pinchtab started (PID ${pid})" >&2
}

# Wait for Pinchtab to become healthy (up to 30s).
wait_pinchtab_healthy() {
    retries=0
    while [ "${retries}" -lt "${MAX_HEALTH_RETRIES}" ]; do
        if curl -sf --max-time 3 -o /dev/null "${HEALTH_URL}" 2>/dev/null; then
            echo "Browser service is ready." >&2
            return 0
        fi
        pid=$(cat "${PINCHTAB_PID_FILE}" 2>/dev/null || echo "")
        if is_numeric_pid "${pid}" && ! kill -0 "${pid}" 2>/dev/null; then
            echo "WARNING: Pinchtab process exited unexpectedly." >&2
            return 1
        fi
        retries=$((retries + 1))
        sleep 1
    done
    echo "WARNING: Browser service did not become healthy within ${MAX_HEALTH_RETRIES}s." >&2
    return 1
}

trap 'release_restart_lock' EXIT INT TERM

echo "Restarting browser service..." >&2
acquire_restart_lock

if [ "${RESTART_ONLY_IF_UNHEALTHY}" = "1" ] && pinchtab_is_healthy; then
    echo "Browser service already healthy; skipping restart." >&2
    exit 0
fi

stop_pinchtab
start_pinchtab
if wait_pinchtab_healthy; then
    exit 0
fi

echo "WARNING: Browser service restart did not complete successfully." >&2
exit 1
