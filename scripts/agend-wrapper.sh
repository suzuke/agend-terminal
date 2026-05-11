#!/bin/bash
# agend-wrapper.sh — restarts daemon on exit code 42
# Usage: ./scripts/agend-wrapper.sh [daemon args...]
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
