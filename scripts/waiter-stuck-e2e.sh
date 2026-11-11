#!/usr/bin/env bash
# 0.22.2 回归：B 在等 A 时，C 线路卡住 45 秒仍要报“卡住”；等待提示不得带对端 id。
set -u

cd "$(dirname "$0")/.." || exit 1
ROOT="$(mktemp -d /tmp/skyget-waiter-stuck.XXXXXX)"
SRV_PORT=$((39200 + $$ % 120))
HANG_PORT=$((39400 + $$ % 120))
API_PORT=$((39600 + $$ % 120))
SRV_PID=""; HANG_PID=""; DAEMON_PID=""

cleanup() {
  [ -n "$DAEMON_PID" ] && kill "$DAEMON_PID" 2>/dev/null
  [ -n "$SRV_PID" ] && kill "$SRV_PID" 2>/dev/null
  [ -n "$HANG_PID" ] && kill "$HANG_PID" 2>/dev/null
  wait 2>/dev/null
  [ "${KEEP:-0}" = "1" ] && echo "保留现场：$ROOT" || rm -rf "$ROOT"
}
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; cat "$ROOT/daemon.log" >&2 2>/dev/null; exit 1; }
ok() { echo "  ✓ $*"; }

mkdir -p "$ROOT/srv" "$ROOT/home" "$ROOT/a" "$ROOT/b" "$ROOT/c"
# 96MB / slow3：每 256KB 停 1 秒。
# 0.23.0 起 daemon 的 `-j 0` 会按体积自动加线程（96MB → 12 条），光靠文件大小
# 撑不出足够时间；这里把「同一主机的连接上限」钉到 1，A 就稳定跑 6 分钟以上，
# B 才有足够时间停在等待状态。C 连一个只发响应头的服务器，45 秒收不到正文。
head -c 100663296 /dev/urandom >"$ROOT/srv/big.bin"
SHA=$(sha256sum "$ROOT/srv/big.bin" | cut -d' ' -f1)
python3 scripts/testserver.py "$SRV_PORT" "$ROOT/srv" >"$ROOT/srv.log" 2>&1 &
SRV_PID=$!
python3 scripts/hangserver.py "$HANG_PORT" >"$ROOT/hang.log" 2>&1 &
HANG_PID=$!

SKYGET_HOME="$ROOT/home" SKYGET_ALLOW_PRIVATE=1 SKYGET_MAX_PER_HOST=1 \
  ./target/debug/skyget serve --port "$API_PORT" >"$ROOT/daemon.log" 2>&1 &
DAEMON_PID=$!
API="http://127.0.0.1:$API_PORT"
export SKYGET_API="$API" SKYGET_HOME="$ROOT/home"
for _ in $(seq 1 100); do
  curl -sf "$API/health" >/dev/null 2>&1 && break
  sleep 0.1
done
curl -sf "$API/health" >/dev/null || fail "daemon 没起来"

B="http://127.0.0.1:$SRV_PORT"
bash scripts/wait-http.sh "$B/" || fail "测试服务器没起来"
OUT_A=$(./target/debug/skyget --json get "$B/slow3/big.bin" --to "$ROOT/a" \
  --sha256 "$SHA" --as agent:a --detach 2>/dev/null)
IDA=$(jq -r .id <<<"$OUT_A")
[ -n "$IDA" ] && [ "$IDA" != "null" ] || fail "A 提交失败：$OUT_A"
sleep 1
OUT_B=$(./target/debug/skyget --json get "$B/slow3/big.bin" --to "$ROOT/b" \
  --sha256 "$SHA" --as agent:b --detach 2>/dev/null)
IDB=$(jq -r .id <<<"$OUT_B")
[ -n "$IDB" ] && [ "$IDB" != "null" ] || fail "B 提交失败：$OUT_B"
sleep 1
OUT_C=$(./target/debug/skyget --json get "http://127.0.0.1:$HANG_PORT/stall.bin" \
  --to "$ROOT/c" --as agent:c --detach 2>/dev/null)
IDC=$(jq -r .id <<<"$OUT_C")
[ -n "$IDC" ] && [ "$IDC" != "null" ] || fail "C 提交失败：$OUT_C"

echo "1. 等 55 秒：B 仍在等，C 已过 45 秒无字节"
sleep 55
EVB=$(curl -s "$API/jobs/$IDB/events?since=0&wait_ms=0")
EVC=$(curl -s "$API/jobs/$IDC/events?since=0&wait_ms=0")

NOTES_B=$(jq -r '.events[] | select(.type=="note") | .data.text' <<<"$EVB")
grep -q "同内容已有任务在下载，等它完成后直接秒传" <<<"$NOTES_B" \
  || fail "B 没有等待提示：$NOTES_B"
grep -Eq '[0-9a-f]{12}' <<<"$NOTES_B" \
  && fail "等待提示泄露了对端 id：$NOTES_B"
ok "B 等待提示存在且不含 id"

STATE_B=$(./target/debug/skyget --json show "$IDB" 2>/dev/null | jq -r .state)
[ "$STATE_B" = "running" ] || fail "B 本应仍在等，实际 $STATE_B"
ok "B 仍在等待"

NOTES_C=$(jq -r '.events[] | select(.type=="note") | .data.text' <<<"$EVC")
grep -q "45 秒没有一个字节进来" <<<"$NOTES_C" \
  || fail "C 卡住 45 秒却没有提示：$NOTES_C"
ok "C 的卡住提示照常（全局静音回归已修复）"

echo "2. 收尾"
curl -s -o /dev/null -X POST "$API/jobs/$IDB/cancel"
curl -s -o /dev/null -X POST "$API/jobs/$IDC/cancel"
curl -s -o /dev/null -X POST "$API/jobs/$IDA/cancel"
ok "已取消"
echo "全部通过"
