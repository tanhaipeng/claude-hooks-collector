#!/usr/bin/env bash
# launchd 入口：加载 collector.env 后 exec release 二进制。
# 手动启动仍然用 ./start.sh（debug + nohup），这个脚本专供 LaunchAgent。
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"

if [ -f collector.env ]; then
  set -a
  # shellcheck disable=SC1091
  . ./collector.env
  set +a
fi

if [ ! -x ./target/release/claude-hooks-collector ]; then
  echo "release binary missing; run: cargo build --release" >&2
  exit 1
fi

exec ./target/release/claude-hooks-collector
