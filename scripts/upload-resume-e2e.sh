#!/usr/bin/env bash
# tus 断点续传：上传到一半 kill -9 daemon → 重启 → 从远端 offset 接着传 → 远端 sha 一致。
#
# 断言：
#   1. 重启后任务不是永远 running
#   2. 事件流有 daemon.restarted
#   3. 续传完成，远端文件 sha 与源一致
#   4. tus sidecar 在完成后被清掉
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$(mktemp -d /tmp/skyget-upload-resume.XXXXXX)"
UP_PORT=$((20400 + $$ % 300))
API_PORT=$((21400 + $$ % 300))
TOKEN="upresume-$$"
DAEMON_PID=""
UP_PID=""

cleanup() {
  [ -n "$DAEMON_PID" ] && kill "$DAEMON_PID" 2>/dev/null || true
  [ -n "$UP_PID" ] && kill "$UP_PID" 2>/dev/null || true
  wait 2>/dev/null || true
  [ "${KEEP:-0}" = "1" ] && echo "保留现场：$ROOT" || rm -rf "$ROOT"
}
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; cat "$ROOT/daemon.log" >&2 || true; exit 1; }
ok() { echo "  ✓ $*"; }

mkdir -p "$ROOT/upload-root" "$ROOT/up-srv" "$ROOT/home"
head -c 67108864 /dev/urandom > "$ROOT/upload-root/big.bin"
SHA="$(sha256sum "$ROOT/upload-root/big.bin" | cut -d' ' -f1)"
UPLOAD_SLOW=1 python3 scripts/upload-server.py "$ROOT/up-srv" "$UP_PORT" \
  >"$ROOT/up.log" 2>&1 &
UP_PID=$!

API="http://127.0.0.1:$API_PORT"
AUTH=(-H "Authorization: Bearer $TOKEN")
JSON=(-H 'Content-Type: application/json')

start_daemon() {
  SKYGET_HOME="$ROOT/home" \
  SKYGET_ALLOW_PRIVATE=1 \
  SKYGET_API_TOKEN="$TOKEN" \
  SKYGET_UPLOAD_ROOTS="$ROOT/upload-root" \
  SKYGET_MAX_JOBS=1 \
    ./target/debug/skyget serve --port "$API_PORT" >>"$ROOT/daemon.log" 2>&1 &
  DAEMON_PID=$!
  for _ in $(seq 1 60); do
    curl -sf "$API/health" >/dev/null 2>&1 && return
    sleep 0.1
  done
  fail "daemon 没起来"
}

start_daemon
BODY="$(jq -nc --arg p "$ROOT/upload-root/big.bin" --arg u "http://127.0.0.1:$UP_PORT/tus" --arg s "$SHA" \
  '{path:$p,url:$u,protocol:"tus",sha256:$s,source:"agent:upload-resume",idempotency_key:"tus-resume"}')"
RESP="$(curl -s "${AUTH[@]}" "${JSON[@]}" -d "$BODY" "$API/uploads")"
ID="$(jq -r .id <<<"$RESP")"
[ -n "$ID" ] && [ "$ID" != "null" ] || fail "提交失败：$RESP"

echo "1. 等到真的开始传，再 kill -9"
for _ in $(seq 1 100); do
  ST="$(curl -s "${AUTH[@]}" "$API/jobs/$ID")"
  DONE_BYTES="$(jq -r .bytes_done <<<"$ST")"
  case "$(jq -r .state <<<"$ST")" in
    running) [ "$DONE_BYTES" -gt 0 ] && break ;;
    done) fail "传得太快，没赶上 kill -9" ;;
    failed) fail "上传失败：$ST" ;;
  esac
  sleep 0.1
done
[ "$(jq -r .state <<<"$ST")" = "running" ] || fail "任务没进入 running：$ST"
ok "已传 $DONE_BYTES 字节"

kill -9 "$DAEMON_PID"
wait "$DAEMON_PID" 2>/dev/null || true
DAEMON_PID=""
sleep 0.3

echo "2. 重启并等续传"
start_daemon
STATE=""
for _ in $(seq 1 400); do
  GOT="$(curl -s "${AUTH[@]}" "$API/jobs/$ID" || true)"
  STATE="$(jq -r .state <<<"$GOT" 2>/dev/null || true)"
  case "$STATE" in
    done) break ;;
    failed) fail "续传失败：$GOT" ;;
    interrupted)
      # 自动续传没捡到（例如崩溃循环）时手动 retry 一次
      curl -s "${AUTH[@]}" "${JSON[@]}" -d '{}' "$API/jobs/$ID/retry" >/dev/null || true
      ;;
  esac
  sleep 0.1
done
[ "$STATE" = "done" ] || fail "续传没完成：$GOT"
ok "状态 done"

echo "3. 远端内容与事件"
UPFILE="$(ls "$ROOT/up-srv"/tus-*.bin 2>/dev/null | head -1)"
[ -n "$UPFILE" ] || fail "远端没有文件"
[ "$(sha256sum "$UPFILE" | cut -d' ' -f1)" = "$SHA" ] || fail "远端 sha 不对"
EV="$(curl -s "${AUTH[@]}" "$API/jobs/$ID/events?since=0&wait_ms=0")"
jq -e '.events | map(select(.type=="interrupted" and .data.code=="daemon.restarted")) | length >= 1' <<<"$EV" >/dev/null \
  || fail "事件流没有 daemon.restarted：$EV"
jq -e '.events | map(select(.type=="done")) | length >= 1' <<<"$EV" >/dev/null \
  || fail "事件流没有 done：$EV"
[ ! -e "$ROOT/home/parts/tus-$ID.json" ] || fail "完成后 sidecar 没清掉"
ok "远端 sha 一致，事件有 daemon.restarted + done，sidecar 已清"

echo
echo "upload-resume-e2e: 3/3 通过"
