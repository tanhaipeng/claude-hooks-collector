#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"

if [ -f collector.env ]; then
  set -a
  # shellcheck disable=SC1091
  . ./collector.env
  set +a
fi

if [ -f collector.pid ] && kill -0 "$(cat collector.pid)" 2>/dev/null; then
  echo "collector already running (pid $(cat collector.pid))"
  exit 0
fi

listen_addr="${LISTEN_ADDR:-127.0.0.1:4321}"
health_host="${listen_addr/0.0.0.0/127.0.0.1}"
health_url="${HEALTH_URL:-http://${health_host}/health}"

cargo build
rm -f collector.log
nohup ./target/debug/claude-hooks-collector >collector.log 2>&1 &
echo $! >collector.pid

for _ in $(seq 1 40); do
  if curl -fsS --max-time 2 -o /dev/null "$health_url" 2>/dev/null; then
    echo "collector started (pid $(cat collector.pid))"
    echo "hooks:  http://${health_host}/hooks"
    echo "health: ${health_url}"
    exit 0
  fi
  sleep 0.25
done

echo "collector failed to start; check collector.log" >&2
exit 1
