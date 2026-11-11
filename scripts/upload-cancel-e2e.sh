#!/usr/bin/env bash
# 上传取消：慢速 tus 传到一半 cancel —— 记录必须真的没了，不能几秒后
# 以 interrupted 复活；sidecar 也不能被在途 PATCH 写回来。
#
# 审计发现：以前初始/中段用全量 store.save，取消删行后哈希/传输结束又把行插回；
# 取消先删 sidecar，在途 PATCH 完成后还会 save_tus 重建。这个脚本钉住修复。
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$(mktemp -d /tmp/skyget-upload-cancel.XXXXXX)"
UP_PORT=$((24400 + $$ % 300))
API_PORT=$((25400 + $$ % 300))
TOKEN="upcancel-$$"
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

SKYGET_HOME="$ROOT/home" \
SKYGET_ALLOW_PRIVATE=1 \
SKYGET_API_TOKEN="$TOKEN" \
SKYGET_UPLOAD_ROOTS="$ROOT/upload-root" \
SKYGET_MAX_JOBS=1 \
  ./target/debug/skyget serve --port "$API_PORT" >>"$ROOT/daemon.log" 2>&1 &
DAEMON_PID=$!
for _ in $(seq 1 60); do
  curl -sf "$API/health" >/dev/null 2>&1 && break
  sleep 0.1
done
curl -sf "$API/health" >/dev/null || fail "daemon 没起来"

BODY="$(jq -nc --arg p "$ROOT/upload-root/big.bin" --arg u "http://127.0.0.1:$UP_PORT/tus" --arg s "$SHA" \
  '{path:$p,url:$u,protocol:"tus",sha256:$s,source:"agent:upload-cancel",idempotency_key:"tus-cancel"}')"
RESP="$(curl -s "${AUTH[@]}" "${JSON[@]}" -d "$BODY" "$API/uploads")"
ID="$(jq -r .id <<<"$RESP")"
[ -n "$ID" ] && [ "$ID" != "null" ] || fail "提交失败：$RESP"

echo "1. 等到真的传起来，再 cancel"
for _ in $(seq 1 150); do
  ST="$(curl -s "${AUTH[@]}" "$API/jobs/$ID")"
  DONE_BYTES="$(jq -r .bytes_done <<<"$ST")"
  case "$(jq -r .state <<<"$ST")" in
    running) [ "$DONE_BYTES" -ge 8388608 ] && break ;;
    done) fail "传得太快，没赶上 cancel" ;;
    failed) fail "上传失败：$ST" ;;
  esac
  sleep 0.1
done
[ "$(jq -r .state <<<"$ST")" = "running" ] || fail "任务没进入 running：$ST"
ok "已传 $DONE_BYTES 字节"

echo "2. cancel 后记录必须真的消失，并保持消失（防幽灵复活）"
CANCEL="$(curl -s "${AUTH[@]}" "${JSON[@]}" -d '{}' "$API/jobs/$ID/cancel")"
[ "$(jq -r .state <<<"$CANCEL")" = "cancelled" ] || fail "cancel 没成功：$CANCEL"
for _ in $(seq 1 30); do
  CODE="$(curl -s -o /dev/null -w '%{http_code}' "${AUTH[@]}" "$API/jobs/$ID")"
  [ "$CODE" = "404" ] && break
  sleep 0.1
done
[ "$CODE" = "404" ] || fail "cancel 后记录还在（HTTP $CODE）"
# 关键：等在途 PATCH / 哈希收尾（6 秒），看它会不会把行写回来。
for _ in $(seq 1 24); do
  sleep 0.25
  CODE="$(curl -s -o /dev/null -w '%{http_code}' "${AUTH[@]}" "$API/jobs/$ID")"
  [ "$CODE" = "404" ] || fail "取消后的任务被后台线程复活（HTTP $CODE）"
done
ok "记录在 6 秒观察窗口内始终 404，没有复活"

echo "3. sidecar 没有被写回，事件里留下 cancelled"
[ ! -e "$ROOT/home/parts/tus-$ID.json" ] || fail "取消后 sidecar 被在途 PATCH 写回来了"
EV="$(curl -s "${AUTH[@]}" "$API/jobs/$ID/events?since=0&wait_ms=0")"
jq -e '.events | map(select(.type=="cancelled")) | length >= 1' <<<"$EV" >/dev/null \
  || fail "事件流没有 cancelled：$EV"
ok "sidecar 已清，事件有 cancelled"

echo
echo "upload-cancel-e2e: 3/3 通过"
