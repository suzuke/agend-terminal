#!/bin/bash
# agend-wrapper.sh — restarts daemon on exit code 42
# Usage: ./scripts/agend-wrapper.sh [daemon args...]
#
# #851: AGEND_WRAPPED=1 is the explicit supervisor-marker the daemon's
# `is_restart_supervised()` check looks for. Without it, the MCP
# `restart_daemon` tool fail-closes — the daemon refuses to exit(42)
# because nothing would respawn it. Exporting the marker here is the
# wrapper-side half of the contract.
export AGEND_WRAPPED=1
while true; do
    agend-terminal daemon "$@"
    EXIT_CODE=$?
    if [ $EXIT_CODE -ne 42 ]; then
        echo "agend daemon exited with code $EXIT_CODE"
        exit $EXIT_CODE
    fi
    echo "agend daemon requested restart (exit 42), restarting..."
    sleep 1
done
