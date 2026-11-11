#!/usr/bin/env bash
# 并发 soak：40 个任务抢 4 个 worker，提交后立刻取消 10 个；
# 其余 30 个必须完成且 sha 一致，台账里不能留下 running/pending 幽灵，
# 同一幂等键重复提交必须 dedupe。
#
# 用法：scripts/soak-e2e.sh
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$(mktemp -d /tmp/skyget-soak.XXXXXX)"
SRV_PORT=$((26400 + $$ % 300))
API_PORT=$((27400 + $$ % 300))
TOKEN="soak-$$"
SRV_PID=""
DAEMON_PID=""

cleanup() {
  [ -n "$DAEMON_PID" ] && kill "$DAEMON_PID" 2>/dev/null || true
  [ -n "$SRV_PID" ] && kill "$SRV_PID" 2>/dev/null || true
  wait 2>/dev/null || true
  [ "${KEEP:-0}" = "1" ] && echo "保留现场：$ROOT" || rm -rf "$ROOT"
}
trap cleanup EXIT

mkdir -p "$ROOT/srv" "$ROOT/home" "$ROOT/out"
head -c 4194304 /dev/urandom > "$ROOT/srv/base.bin"
SHA="$(sha256sum "$ROOT/srv/base.bin" | cut -d' ' -f1)"
# 每个任务一个文件名（同一个 inode）：URL 不同，内容相同，方便测缓存命中。
for i in $(seq 0 39); do ln "$ROOT/srv/base.bin" "$ROOT/srv/f$i.bin"; done

python3 scripts/testserver.py "$SRV_PORT" "$ROOT/srv" >"$ROOT/srv.log" 2>&1 &
SRV_PID=$!
SKYGET_HOME="$ROOT/home" \
SKYGET_ALLOW_PRIVATE=1 \
SKYGET_API_TOKEN="$TOKEN" \
SKYGET_MAX_JOBS=4 \
SKYGET_MAX_QUEUED=64 \
  ./target/debug/skyget serve --port "$API_PORT" >"$ROOT/daemon.log" 2>&1 &
DAEMON_PID=$!
for _ in $(seq 1 60); do
  curl -sf "http://127.0.0.1:$API_PORT/health" >/dev/null 2>&1 && break
  sleep 0.1
done
curl -sf "http://127.0.0.1:$API_PORT/health" >/dev/null || {
  cat "$ROOT/daemon.log" >&2
  echo "FAIL: daemon 没起来" >&2
  exit 1
}

python3 scripts/soak.py "http://127.0.0.1:$API_PORT" "$TOKEN" \
  "http://127.0.0.1:$SRV_PORT" "$SHA" "$ROOT/out"

echo
echo "soak-e2e: 通过"
