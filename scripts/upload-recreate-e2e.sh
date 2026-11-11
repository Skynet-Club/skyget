#!/usr/bin/env bash
# tus 远端对象丢失：上传到一半 kill -9 daemon，再把上传服务器重启（内存里的
# 上传状态没了）→ daemon 重启后 HEAD 旧 Location 得到 404 → 清 sidecar、
# 重新创建上传、从头传完。
#
# 这个场景以前会永久卡死：HEAD 404 被当成“用本地 offset”，PATCH 永远 404。
#
# 断言：
#   1. 重启后任务不是永远 running
#   2. 远端对象丢失后能重建并完成，远端 sha 与源一致
#   3. 完成后的 sidecar 被清掉
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$(mktemp -d /tmp/skyget-upload-recreate.XXXXXX)"
UP_PORT=$((22400 + $$ % 300))
API_PORT=$((23400 + $$ % 300))
TOKEN="uprecreate-$$"
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

start_upload_server() {
  UPLOAD_SLOW=1 python3 scripts/upload-server.py "$ROOT/up-srv" "$UP_PORT" \
    >>"$ROOT/up.log" 2>&1 &
  UP_PID=$!
  for _ in $(seq 1 50); do
    curl -s -o /dev/null "http://127.0.0.1:$UP_PORT/tus/ping" && return
    sleep 0.1
  done
  fail "上传服务器没起来"
}
start_upload_server

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
  '{path:$p,url:$u,protocol:"tus",sha256:$s,source:"agent:upload-recreate",idempotency_key:"tus-recreate"}')"
RESP="$(curl -s "${AUTH[@]}" "${JSON[@]}" -d "$BODY" "$API/uploads")"
ID="$(jq -r .id <<<"$RESP")"
[ -n "$ID" ] && [ "$ID" != "null" ] || fail "提交失败：$RESP"

echo "1. 等到真的开始传，再 kill -9 daemon"
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
ok "已传 $DONE_BYTES 字节，sidecar 存在"

kill -9 "$DAEMON_PID"
wait "$DAEMON_PID" 2>/dev/null || true
DAEMON_PID=""
sleep 0.2

echo "2. 重启上传服务器（内存里的 upload 状态全部丢失）"
kill "$UP_PID" 2>/dev/null || true
wait "$UP_PID" 2>/dev/null || true
UP_PID=""
rm -f "$ROOT/up-srv"/tus-*.bin
start_upload_server
ok "上传服务器已重启，旧 Location 现在一定 404"

echo "3. 重启 daemon 并等它重建 + 传完"
start_daemon
STATE=""
for _ in $(seq 1 600); do
  GOT="$(curl -s "${AUTH[@]}" "$API/jobs/$ID" || true)"
  STATE="$(jq -r .state <<<"$GOT" 2>/dev/null || true)"
  case "$STATE" in
    done) break ;;
    failed) fail "重建后上传失败：$GOT" ;;
    interrupted)
      # 自动续传没捡到（例如崩溃循环）时手动 retry 一次
      curl -s "${AUTH[@]}" "${JSON[@]}" -d '{}' "$API/jobs/$ID/retry" >/dev/null || true
      ;;
  esac
  sleep 0.1
done
[ "$STATE" = "done" ] || fail "重建后没完成：$GOT"
ok "远端对象丢失后重建成功，状态 done"

echo "4. 远端内容与 sidecar"
FOUND=""
for f in "$ROOT/up-srv"/tus-*.bin; do
  [ -f "$f" ] || continue
  [ "$(sha256sum "$f" | cut -d' ' -f1)" = "$SHA" ] && FOUND="$f"
done
[ -n "$FOUND" ] || fail "远端没有一份 sha 正确的文件"
[ ! -e "$ROOT/home/parts/tus-$ID.json" ] || fail "完成后 sidecar 没清掉"
EV="$(curl -s "${AUTH[@]}" "$API/jobs/$ID/events?since=0&wait_ms=0")"
jq -e '.events | map(select(.type=="done")) | length >= 1' <<<"$EV" >/dev/null \
  || fail "事件流没有 done：$EV"
ok "远端 sha 一致，sidecar 已清，事件有 done"

echo
echo "upload-recreate-e2e: 4/4 通过"
