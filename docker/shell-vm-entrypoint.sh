#!/bin/sh
# shell-vm container entrypoint.
#
# Starts Pinchtab (browser automation server) when BROWSER_ENABLED=true,
# waits for it to become healthy, pre-warms Chrome, then runs a lightweight
# crash-recovery watchdog — all as long-lived child processes.
#
# IMPORTANT: this script deliberately does NOT use `exec` to hand off to
# shell-daemon.  Using `exec` replaces the entrypoint shell (PID 1), which
# causes the kernel to kill background processes (Pinchtab, watchdog) in this
# container environment.  Instead we run shell-daemon in the background and
# `wait` for it, keeping the shell alive as PID 1 so all children survive
# for the lifetime of the container.
#
# `set -e` is intentionally absent: health-check polls and pre-warmup curl
# can return non-zero without being fatal, and explicit `|| true` guards are
# used where needed.

export HOME="${HOME:-/tmp/oxydra-home}"
export XDG_CONFIG_HOME="${XDG_CONFIG_HOME:-${HOME}/.config}"
export XDG_CACHE_HOME="${XDG_CACHE_HOME:-${HOME}/.cache}"
export XDG_STATE_HOME="${XDG_STATE_HOME:-${HOME}/.local/state}"
mkdir -p "${HOME}" "${XDG_CONFIG_HOME}" "${XDG_CACHE_HOME}" "${XDG_STATE_HOME}" \
    2>/dev/null || true

# Helper: kill a process gracefully, then forcefully if it lingers, and
# wait until the port it was using is released before returning.
# Usage: kill_and_wait_port <pid> <port>
kill_and_wait_port() {
    _pid="$1"
    _port="$2"
    # SIGTERM first
    kill -TERM "${_pid}" 2>/dev/null || true
    # Wait up to 5 s for graceful exit, then SIGKILL
    _w=0
    while kill -0 "${_pid}" 2>/dev/null && [ "${_w}" -lt 5 ]; do
        sleep 1
        _w=$((_w + 1))
    done
    kill -9 "${_pid}" 2>/dev/null || true
    # Wait up to 5 s for the port to be released
    _w=0
    while nc -z 127.0.0.1 "${_port}" 2>/dev/null && [ "${_w}" -lt 5 ]; do
        sleep 1
        _w=$((_w + 1))
    done
}

if [ "${BROWSER_ENABLED}" = "true" ]; then
    PROFILES_DIR="${BRIDGE_STATE_DIR:-/shared/.pinchtab}/profiles"
    BRIDGE_PORT="${BRIDGE_PORT:-9867}"
    HEALTH_URL="http://${BRIDGE_BIND:-127.0.0.1}:${BRIDGE_PORT}/health"
    BASE_URL="http://${BRIDGE_BIND:-127.0.0.1}:${BRIDGE_PORT}"

    # ── 1. Clean Chrome singleton locks from previous crashes ─────────────────
    # Skip in external CDP mode — there is no local Chrome profile directory.
    if [ -z "${BROWSER_EXTERNAL_CDP_URL}" ] && [ -d "${PROFILES_DIR}" ]; then
        find "${PROFILES_DIR}" -name "SingletonLock" -delete 2>/dev/null || true
        find "${PROFILES_DIR}" -name ".com.google.Chrome.SingletonLock" \
            -delete 2>/dev/null || true
    fi

    # ── 2. Start Pinchtab ─────────────────────────────────────────────────────
    /usr/local/bin/pinchtab &
    PINCHTAB_PID=$!
    echo "INFO: Pinchtab started (PID=${PINCHTAB_PID})" >&2

    # ── 3. Wait for Pinchtab to become healthy (up to 90 s) ──────────────────
    # NOTE: Pinchtab requires Authorization even on /health (returns 401 otherwise).
    RETRIES=0
    MAX_RETRIES=90
    PINCHTAB_HEALTHY=0
    while [ "${RETRIES}" -lt "${MAX_RETRIES}" ]; do
        if curl -sf --max-time 2 \
                -H "Authorization: Bearer ${BRIDGE_TOKEN}" \
                -o /dev/null "${HEALTH_URL}" 2>/dev/null; then
            PINCHTAB_HEALTHY=1
            break
        fi
        if ! kill -0 "${PINCHTAB_PID}" 2>/dev/null; then
            echo "WARNING: Pinchtab exited unexpectedly during startup" >&2
            break
        fi
        RETRIES=$((RETRIES + 1))
        sleep 1
    done

    if [ "${PINCHTAB_HEALTHY}" -eq 1 ]; then
        # ── 4. Pre-warm Chrome ────────────────────────────────────────────────
        # Pinchtab starts Chrome lazily on the first real request.  We trigger
        # that initialisation now so the LLM's first browser call has no
        # multi-second Chrome startup delay.
        curl -sf --max-time 60 \
            -X POST "${BASE_URL}/navigate" \
            -H "Authorization: Bearer ${BRIDGE_TOKEN}" \
            -H 'Content-Type: application/json' \
            -d '{"url":"about:blank"}' \
            -o /dev/null 2>/dev/null \
            && echo "INFO: Chrome pre-warmed successfully" >&2 \
            || echo "INFO: Chrome pre-warm request sent (Chrome still initialising)" >&2
    else
        echo "WARNING: Pinchtab did not become healthy within ${MAX_RETRIES}s; Chrome not pre-warmed" >&2
    fi

    # ── 5. Crash-recovery watchdog ────────────────────────────────────────────
    # Polls Pinchtab every 60 s and restarts it if unresponsive.
    # Runs as a background subshell — a child of this script (PID 1).
    #
    # Key design decisions:
    #   • Auth header required: Pinchtab returns 401 without it, which curl -f
    #     treats as a failure, so the watchdog would restart Pinchtab endlessly
    #     without the header.
    #   • SIGTERM + SIGKILL: Go ignores SIGTERM by default; we wait up to 5 s
    #     then SIGKILL to guarantee the old process is gone.
    #   • Port-free wait: we wait until the TCP port is released before starting
    #     the replacement so the new Pinchtab does not fail with "address in use".
    #   • 15 s poll interval — short enough to recover quickly on
    #     resource-constrained devices (e.g. Raspberry Pi) while still
    #     avoiding false-positive restarts under normal load.
    (
        watchdog_pid="${PINCHTAB_PID}"
        while true; do
            sleep 15
            if ! curl -sf --max-time 5 \
                    -H "Authorization: Bearer ${BRIDGE_TOKEN}" \
                    -o /dev/null "${HEALTH_URL}" 2>/dev/null; then
                echo "WATCHDOG: Pinchtab unresponsive, restarting (PID=${watchdog_pid})..." >&2
                kill_and_wait_port "${watchdog_pid}" "${BRIDGE_PORT}"
                find "${PROFILES_DIR}" \
                    -name "SingletonLock" -delete 2>/dev/null || true
                find "${PROFILES_DIR}" \
                    -name ".com.google.Chrome.SingletonLock" -delete 2>/dev/null || true
                /usr/local/bin/pinchtab &
                watchdog_pid=$!
                echo "WATCHDOG: Pinchtab restarted (PID=${watchdog_pid})" >&2
            fi
        done
    ) &
fi

# ── Run shell-daemon and wait for it ─────────────────────────────────────────
# shell-daemon is started as a child of this script rather than via exec so
# that the Pinchtab and watchdog processes above remain alive as siblings
# under this script (PID 1).  exec would replace PID 1 (the shell) and
# orphan those siblings in a way that gets them killed in Docker.
/usr/local/bin/shell-daemon "$@" &
DAEMON_PID=$!
echo "INFO: shell-daemon started (PID=${DAEMON_PID})" >&2

# Forward SIGTERM / SIGINT to shell-daemon for graceful shutdown.
trap 'kill -TERM $DAEMON_PID 2>/dev/null' TERM INT

# Wait for shell-daemon.  When it exits the container exits with its code.
wait $DAEMON_PID
