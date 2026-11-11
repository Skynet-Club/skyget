#!/usr/bin/env bash
# 等一个本地测试服务器能接受连接（HTTP 404 也算就绪），默认最多 10 秒。
set -u
url="${1:?用法: wait-http.sh URL [tries]}"
tries="${2:-100}"
for _ in $(seq 1 "$tries"); do
  if curl -s -o /dev/null --max-time 1 "$url" 2>/dev/null; then
    exit 0
  fi
  sleep 0.1
done
exit 1
