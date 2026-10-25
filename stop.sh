#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"

if [ ! -f collector.pid ]; then
  echo "collector is not running (no collector.pid)"
  exit 0
fi

pid="$(cat collector.pid)"
if ! kill -0 "$pid" 2>/dev/null; then
  echo "collector pid $pid is not running"
  rm -f collector.pid
  exit 0
fi

kill "$pid" 2>/dev/null || true

# The exporter may need up to ~10s to drain its queue.
for _ in $(seq 1 60); do
  if ! kill -0 "$pid" 2>/dev/null; then
    echo "collector stopped (pid $pid)"
    rm -f collector.pid
    exit 0
  fi
  sleep 0.25
done

echo "collector did not exit after SIGTERM; sending SIGKILL (pid $pid)" >&2
kill -9 "$pid" 2>/dev/null || true
sleep 0.5
rm -f collector.pid
