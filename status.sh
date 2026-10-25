#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"

if [ -f collector.pid ] && kill -0 "$(cat collector.pid)" 2>/dev/null; then
  echo "collector running (pid $(cat collector.pid))"
else
  echo "collector not running"
  exit 1
fi

listen_addr="${LISTEN_ADDR:-127.0.0.1:4321}"
health_host="${listen_addr/0.0.0.0/127.0.0.1}"
health_url="${HEALTH_URL:-http://${health_host}/health}"
jaeger_api="${JAEGER_API:-http://127.0.0.1:16686/api/v3/services}"

curl -s --max-time 3 "$health_url"
echo
curl -s --max-time 3 "$jaeger_api"
echo
