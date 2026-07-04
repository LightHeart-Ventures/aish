#!/bin/sh
# worker-entrypoint.sh — PID 1 for aish container workers.
#
# Default path (non-nested workers): exec aish directly, byte-for-byte the same
# as the historical `ENTRYPOINT ["/usr/local/bin/aish"]`. Nothing changes.
#
# Nested path (Sysbox): when the container was launched under a nested-capable
# OCI runtime (sysbox-runc), container.rs sets AISH_START_INNER_DOCKERD=1. We
# then boot a PRIVATE in-container dockerd BEFORE handing off to the coordinator,
# so the in-container aish's own `run_argv` has a real daemon to talk to and the
# grandchild container is created in THIS mount namespace — meaning the worktree
# path the child emits resolves identically for the daemon that creates it (the
# whole reason Sysbox beats docker-out-of-docker socket sharing).
#
# This only works under Sysbox: a stock-runtime worker runs --cap-drop=ALL and
# an unprivileged dockerd cannot start there. container.rs gates the env var on
# the runtime override precisely so this branch is never taken without it.
set -e

if [ "${AISH_START_INNER_DOCKERD:-0}" = "1" ]; then
    # Background the daemon; keep logs out of the coordinator's stdout.
    dockerd >/var/log/dockerd.log 2>&1 &
    # Wait (bounded) for the socket so the first `docker run` doesn't race boot.
    i=0
    while [ ! -S /var/run/docker.sock ]; do
        i=$((i + 1))
        if [ "$i" -gt 30 ]; then
            echo "worker-entrypoint: inner dockerd did not come up in 15s" >&2
            break
        fi
        sleep 0.5
    done
fi

exec /usr/local/bin/aish "$@"
