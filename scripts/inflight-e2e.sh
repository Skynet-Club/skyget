#!/usr/bin/env bash
# 同内容并发：A 在下载时，B 等它完成并秒传，而不是再下一遍。
set -u

cd "$(dirname "$0")/.." || exit 1
ROOT="$(mktemp -d /tmp/skyget-inflight.XXXXXX)"
SRV_PORT=$((37400 + $$ % 150))
API_PORT=$((38600 + $$ % 150))
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

mkdir -p "$ROOT/srv" "$ROOT/home" "$ROOT/a" "$ROOT/b"
head -c 8388608 /dev/urandom >"$ROOT/srv/file.bin"
SHA=$(sha256sum "$ROOT/srv/file.bin" | cut -d' ' -f1)
python3 scripts/testserver.py "$SRV_PORT" "$ROOT/srv" >"$ROOT/srv.log" 2>&1 &
SRV_PID=$!

SKYGET_HOME="$ROOT/home" SKYGET_ALLOW_PRIVATE=1 \
  ./target/debug/skyget serve --port "$API_PORT" >"$ROOT/daemon.log" 2>&1 &
DAEMON_PID=$!
API="http://127.0.0.1:$API_PORT"
export SKYGET_API="$API"
export SKYGET_HOME="$ROOT/home"
for _ in $(seq 1 60); do
  curl -sf "$API/health" >/dev/null 2>&1 && break
  sleep 0.1
done
curl -sf "$API/health" >/dev/null || fail "daemon 没起来"

B="http://127.0.0.1:$SRV_PORT"
bash scripts/wait-http.sh "$B/" || fail "测试服务器没起来"
OUT_A=$(./target/debug/skyget --json get "$B/slow3/file.bin" --to "$ROOT/a" \
  --sha256 "$SHA" --as agent:a --detach 2>/dev/null)
IDA=$(jq -r .id <<<"$OUT_A")
[ -n "$IDA" ] && [ "$IDA" != "null" ] || fail "A 提交失败：$OUT_A"
sleep 0.8
OUT_B=$(./target/debug/skyget --json get "$B/slow3/file.bin" --to "$ROOT/b" \
  --sha256 "$SHA" --as agent:b --detach 2>/dev/null)
IDB=$(jq -r .id <<<"$OUT_B")
[ -n "$IDB" ] && [ "$IDB" != "null" ] || fail "B 提交失败：$OUT_B"

echo "1. A 在下载，B 等待后秒传"
STATE_A="pending"; STATE_B="pending"
for _ in $(seq 1 180); do
  STATE_A=$(./target/debug/skyget --json show "$IDA" 2>/dev/null | jq -r .state)
  STATE_B=$(./target/debug/skyget --json show "$IDB" 2>/dev/null | jq -r .state)
  [ "$STATE_A" = "done" ] && [ "$STATE_B" = "done" ] && break
  sleep 0.5
done
[ "$STATE_A" = "done" ] || fail "A 没完成：$STATE_A"
[ "$STATE_B" = "done" ] || fail "B 没完成：$STATE_B"
ok "A/B 都完成"

SHOW_B=$(./target/debug/skyget --json show "$IDB" 2>/dev/null)
jq -e '.cached == true' <<<"$SHOW_B" >/dev/null \
  || fail "B 没有走秒传（cached=false）：$SHOW_B"
ok "B cached=true"

[ "$(sha256sum "$ROOT/a/file.bin" | cut -d' ' -f1)" = "$SHA" ] || fail "A 落地 sha 不对"
[ "$(sha256sum "$ROOT/b/file.bin" | cut -d' ' -f1)" = "$SHA" ] || fail "B 落地 sha 不对"
ok "两份文件 sha 一致"

echo "2. 等待中的任务可以取消"
OUT_C=$(./target/debug/skyget --json get "$B/slow3/file.bin" --to "$ROOT/a" \
  --sha256 "$SHA" --as agent:c --detach 2>/dev/null)
IDC=$(jq -r .id <<<"$OUT_C")
[ -n "$IDC" ] && [ "$IDC" != "null" ] || fail "C 提交失败：$OUT_C"
sleep 0.5
OUT_D=$(./target/debug/skyget --json get "$B/slow3/file.bin" --to "$ROOT/b" \
  --sha256 "$SHA" --as agent:d --detach 2>/dev/null)
IDD=$(jq -r .id <<<"$OUT_D")
[ -n "$IDD" ] && [ "$IDD" != "null" ] || fail "D 提交失败：$OUT_D"
sleep 1
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/jobs/$IDD/cancel")
[ "$CODE" = "200" ] || fail "取消等待中的 D 返回 $CODE"
sleep 1
./target/debug/skyget --json show "$IDD" >/dev/null 2>&1 \
  && fail "D 取消后记录还在"
curl -s -o /dev/null -X POST "$API/jobs/$IDC/cancel"
ok "等待者可取消，记录已清"

echo "全部通过"
