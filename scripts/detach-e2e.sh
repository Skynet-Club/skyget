#!/usr/bin/env bash
# detach / 客户端死亡验收：
#   1. --detach 提交后 CLI 立即返回，任务在 daemon 里继续到 done
#   2. 不带 --detach 的 CLI 被 SIGKILL，任务仍继续到 done
#
# 用法：先 cargo build --bin skyget，再 scripts/detach-e2e.sh
set -u

cd "$(dirname "$0")/.." || exit 1
ROOT="$(mktemp -d /tmp/skyget-detach.XXXXXX)"
SRV_PORT=$((30400 + $$ % 300))
API_PORT=$((31400 + $$ % 300))
SRV_PID=""
DAEMON_PID=""

cleanup() {
  [ -n "$DAEMON_PID" ] && kill "$DAEMON_PID" 2>/dev/null
  [ -n "$SRV_PID" ] && kill "$SRV_PID" 2>/dev/null
  wait 2>/dev/null
  [ "${KEEP:-0}" = "1" ] && echo "保留现场：$ROOT" || rm -rf "$ROOT"
}
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; cat "$ROOT/daemon.log" >&2 2>/dev/null; exit 1; }
ok() { echo "  ✓ $*"; }

mkdir -p "$ROOT/srv" "$ROOT/home" "$ROOT/dl1" "$ROOT/dl2"
head -c 8388608 /dev/urandom >"$ROOT/srv/slow1.bin"
head -c 8388608 /dev/urandom >"$ROOT/srv/slow2.bin"
SHA1=$(sha256sum "$ROOT/srv/slow1.bin" | cut -d' ' -f1)
SHA2=$(sha256sum "$ROOT/srv/slow2.bin" | cut -d' ' -f1)

python3 scripts/testserver.py "$SRV_PORT" "$ROOT/srv" >"$ROOT/srv.log" 2>&1 &
SRV_PID=$!

SKYGET_HOME="$ROOT/home" SKYGET_ALLOW_PRIVATE=1 \
  ./target/debug/skyget serve --port "$API_PORT" >"$ROOT/daemon.log" 2>&1 &
DAEMON_PID=$!
API="http://127.0.0.1:$API_PORT"
for _ in $(seq 1 60); do
  curl -sf "$API/health" >/dev/null 2>&1 && break
  sleep 0.1
done
curl -sf "$API/health" >/dev/null || fail "daemon 没起来"

B="http://127.0.0.1:$SRV_PORT"

echo "1. --detach：提交即返回，任务在 daemon 里跑完"
t0=$(date +%s%N)
out=$(SKYGET_API="$API" ./target/debug/skyget get "$B/slow/slow1.bin" \
  --to "$ROOT/dl1" --as agent:test --detach --json 2>/dev/null)
ms=$((($(date +%s%N) - t0) / 1000000))
id=$(jq -r .id <<<"$out")
[ -n "$id" ] && [ "$id" != "null" ] || fail "detach 没返回 id: $out"
[ "$ms" -lt 2000 ] && ok "提交即返回（${ms}ms）" || fail "detach 太慢：${ms}ms"
state=$(curl -sf "$API/jobs/$id" | jq -r .state)
case "$state" in
  pending | running) ok "提交后状态 $state" ;;
  *) fail "提交后状态异常：$state" ;;
esac
for _ in $(seq 1 300); do
  state=$(curl -sf "$API/jobs/$id" | jq -r .state)
  if [ "$state" = "done" ] || [ "$state" = "failed" ] || [ "$state" = "interrupted" ]; then
    break
  fi
  sleep 0.1
done
[ "$state" = "done" ] || fail "任务没跑完：$state"
actual=$(sha256sum "$ROOT/dl1/slow1.bin" 2>/dev/null | cut -d' ' -f1)
[ "$actual" = "$SHA1" ] && ok "文件 sha 正确" || fail "sha 不符：$actual"

echo "2. 客户端被 SIGKILL：任务仍继续到 done"
setsid env SKYGET_API="$API" ./target/debug/skyget get "$B/slow/slow2.bin" \
  --to "$ROOT/dl2" --as agent:test --daemon --json \
  >"$ROOT/cli2.out" 2>"$ROOT/cli2.err" &
cli_pid=$!
id=""
for _ in $(seq 1 50); do
  id=$(curl -sf "$API/jobs?limit=10" |
    jq -r --arg u "$B/slow/slow2.bin" '.[] | select(.url==$u) | .id' | head -1)
  [ -n "$id" ] && break
  sleep 0.1
done
[ -n "$id" ] || fail "没找到第二个任务"
sleep 0.8
kill -KILL -"$cli_pid" 2>/dev/null
sleep 0.5
state=$(curl -sf "$API/jobs/$id" | jq -r .state)
case "$state" in
  pending | running) ok "客户端死后任务仍 $state" ;;
  *) fail "客户端死后任务变成 $state" ;;
esac
for _ in $(seq 1 300); do
  state=$(curl -sf "$API/jobs/$id" | jq -r .state)
  if [ "$state" = "done" ] || [ "$state" = "failed" ] || [ "$state" = "interrupted" ]; then
    break
  fi
  sleep 0.1
done
[ "$state" = "done" ] || fail "客户端死后任务没跑完：$state"
actual=$(sha256sum "$ROOT/dl2/slow2.bin" 2>/dev/null | cut -d' ' -f1)
[ "$actual" = "$SHA2" ] && ok "客户端死后文件仍正确" || fail "sha 不符：$actual"

echo "全部通过"
