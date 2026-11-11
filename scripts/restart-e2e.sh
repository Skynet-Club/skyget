#!/usr/bin/env bash
# 重启收敛：下载到一半 kill -9 daemon → 重启 → 对账成 interrupted → 自动续传完成。
#
# 断言：
#   1. 重启后的台账不再显示「永远在跑」
#   2. 事件流里有 interrupted + code=daemon.restarted
#   3. 最终文件 sha 与源一致（自动续传或手动 retry 后）
#
# 用法：scripts/restart-e2e.sh
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$(mktemp -d /tmp/skyget-restart-e2e.XXXXXX)"
SRV_PORT=$((18300 + $$ % 300))
API_PORT=$((19300 + $$ % 300))
TOKEN="restart-$$"
DAEMON_PID=""
SRV_PID=""

cleanup() {
  [ -n "$DAEMON_PID" ] && kill "$DAEMON_PID" 2>/dev/null || true
  [ -n "$SRV_PID" ] && kill "$SRV_PID" 2>/dev/null || true
  wait 2>/dev/null || true
  [ "${KEEP:-0}" = "1" ] && echo "保留现场：$ROOT" || rm -rf "$ROOT"
}
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; cat "$ROOT/daemon.log" >&2 || true; exit 1; }
ok() { echo "  ✓ $*"; }

mkdir -p "$ROOT/srv" "$ROOT/home"
head -c 8388608 /dev/urandom > "$ROOT/srv/slow.bin"
SHA="$(sha256sum "$ROOT/srv/slow.bin" | cut -d' ' -f1)"
python3 scripts/testserver.py "$SRV_PORT" "$ROOT/srv" >"$ROOT/srv.log" 2>&1 &
SRV_PID=$!
API="http://127.0.0.1:$API_PORT"
TOKEN_H=(-H "Authorization: Bearer $TOKEN")
JSON_H=(-H 'Content-Type: application/json')

start_daemon() {
  SKYGET_HOME="$ROOT/home" \
  SKYGET_ALLOW_PRIVATE=1 \
  SKYGET_API_TOKEN="$TOKEN" \
  SKYGET_MAX_JOBS=1 \
  SKYGET_MAX_QUEUED=8 \
    ./target/debug/skyget serve --port "$API_PORT" >>"$ROOT/daemon.log" 2>&1 &
  DAEMON_PID=$!
  for _ in $(seq 1 60); do
    curl -sf "$API/health" >/dev/null 2>&1 && return
    sleep 0.1
  done
  fail "daemon 没起来"
}

start_daemon
URL="http://127.0.0.1:$SRV_PORT/slow3/slow.bin"
BODY="$(jq -nc --arg u "$URL" --arg d "$ROOT/out" --arg s "$SHA" \
  '{url:$u,dest:$d,sha256:$s,source:"agent:restart",idempotency_key:"restart-1"}')"
RESP="$(curl -s "${TOKEN_H[@]}" "${JSON_H[@]}" -d "$BODY" "$API/jobs")"
ID="$(jq -r .id <<<"$RESP")"
[ -n "$ID" ] && [ "$ID" != "null" ] || fail "提交失败：$RESP"

# 等它真的开始下（state=running），再杀
for _ in $(seq 1 100); do
  ST="$(curl -s "${TOKEN_H[@]}" "$API/jobs/$ID")"
  case "$(jq -r .state <<<"$ST")" in
    running) break ;;
    done) fail "任务太快完成了，没赶上 kill -9" ;;
  esac
  sleep 0.1
done
[ "$(jq -r .state <<<"$ST")" = "running" ] || fail "任务没进入 running：$ST"

echo "1. kill -9 daemon（下载中）"
kill -9 "$DAEMON_PID"
wait "$DAEMON_PID" 2>/dev/null || true
DAEMON_PID=""
sleep 0.3

echo "2. 重启并对账"
start_daemon
GOT=""
for _ in $(seq 1 100); do
  GOT="$(curl -s "${TOKEN_H[@]}" "$API/jobs/$ID" || true)"
  [ -n "$GOT" ] && [ "$(jq -r .state <<<"$GOT" 2>/dev/null)" != "running" ] && break
  sleep 0.1
done
[ -n "$GOT" ] || fail "重启后查不到任务"
STATE="$(jq -r .state <<<"$GOT")"
case "$STATE" in
  interrupted|pending|done) ok "重启后状态是 $STATE（不再是永远 running）" ;;
  *) fail "重启后状态异常：$GOT" ;;
esac
# 自动续传可能已经把它跑完，所以最终的 error_code 会被清空；
# 「重启过」的证据在事件流里（下一步查），不要求终态还挂着错误码。

echo "3. 事件流里有重启证据"
EV="$(curl -s "${TOKEN_H[@]}" "$API/jobs/$ID/events?since=0&wait_ms=0")"
jq -e '.events | map(select(.type=="interrupted" and .data.code=="daemon.restarted")) | length >= 1' <<<"$EV" >/dev/null \
  || fail "事件流没有 daemon.restarted：$EV"
ok "interrupted 事件带 daemon.restarted"

echo "4. 续传完成"
# serve 启动时会自动接着下最多 2 条；等它完成。
for _ in $(seq 1 300); do
  GOT="$(curl -s "${TOKEN_H[@]}" "$API/jobs/$ID")"
  [ "$(jq -r .state <<<"$GOT")" = "done" ] && break
  [ "$(jq -r .state <<<"$GOT")" = "failed" ] && fail "续传失败：$GOT"
  sleep 0.1
done
[ "$(jq -r .state <<<"$GOT")" = "done" ] || fail "续传没完成：$GOT"
[ -f "$ROOT/out/slow.bin" ] || fail "文件没落地"
[ "$(sha256sum "$ROOT/out/slow.bin" | cut -d' ' -f1)" = "$SHA" ] || fail "续传后 sha 不对"
ok "续传完成且 sha 一致"

echo "5. 删除墓碑跨重启仍在（增量客户端不会把删掉的行挂回来）"
CURSOR="$(curl -s "${TOKEN_H[@]}" "$API/jobs/changes?since=0" | jq -r .now)"
[ "$CURSOR" -gt 0 ] || fail "changes 游标不对：$CURSOR"
DEL="$(curl -s "${TOKEN_H[@]}" -X DELETE "$API/jobs/$ID")"
[ "$(jq -r .forgotten <<<"$DEL")" = "true" ] || fail "删除失败：$DEL"
curl -s "${TOKEN_H[@]}" "$API/jobs/changes?since=$CURSOR" | \
  jq -e --arg id "$ID" '.deleted | index($id) != null' >/dev/null \
  || fail "删除后增量里没有墓碑"
# 再杀一次、再起：墓碑落库，不能只活在进程内存里。
kill -9 "$DAEMON_PID"
wait "$DAEMON_PID" 2>/dev/null || true
DAEMON_PID=""
start_daemon
curl -s "${TOKEN_H[@]}" "$API/jobs/changes?since=$CURSOR" | \
  jq -e --arg id "$ID" '.deleted | index($id) != null' >/dev/null \
  || fail "重启后墓碑丢了（删除对 since>0 客户端不可见）"
ok "墓碑跨 daemon 重启仍可见"

echo
echo "restart-e2e: 5/5 通过"
