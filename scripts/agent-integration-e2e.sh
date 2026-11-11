#!/usr/bin/env bash
# agent 调用路径验收：真 daemon + 真 MCP stdio + 真 CLI + 真本地 HTTP 源。
#
# 断言：
#   1. MCP initialize / tools/list 暴露 want/watch/status/upload
#   2. MCP skyget_want 提交 → skyget_status 到 done → 文件 sha 正确
#   3. MCP skyget_watch 能重放事件并看到 done
#   4. 同幂等键重复调用 dedupe 到同一条任务
#   5. CLI --as claude --daemon 走同一台账，命中共享缓存，输出 done
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$(mktemp -d /tmp/skyget-agent-integration.XXXXXX)"
SRV_PORT=$((28400 + $$ % 300))
API_PORT=$((29400 + $$ % 300))
TOKEN="agent-int-$$"
SRV_PID=""
DAEMON_PID=""

cleanup() {
  [ -n "$DAEMON_PID" ] && kill "$DAEMON_PID" 2>/dev/null || true
  [ -n "$SRV_PID" ] && kill "$SRV_PID" 2>/dev/null || true
  wait 2>/dev/null || true
  [ "${KEEP:-0}" = "1" ] && echo "保留现场：$ROOT" || rm -rf "$ROOT"
}
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; cat "$ROOT/daemon.log" >&2 || true; exit 1; }
ok() { echo "  ✓ $*"; }

mkdir -p "$ROOT/srv" "$ROOT/home" "$ROOT/mcp-out" "$ROOT/cli-out"
head -c 2097152 /dev/urandom > "$ROOT/srv/file.bin"
SHA="$(sha256sum "$ROOT/srv/file.bin" | cut -d' ' -f1)"
head -c 4194304 /dev/urandom > "$ROOT/srv/live.bin"
LIVE_SHA="$(sha256sum "$ROOT/srv/live.bin" | cut -d' ' -f1)"
python3 scripts/testserver.py "$SRV_PORT" "$ROOT/srv" >"$ROOT/srv.log" 2>&1 &
SRV_PID=$!

SKYGET_HOME="$ROOT/home" \
SKYGET_ALLOW_PRIVATE=1 \
SKYGET_API_TOKEN="$TOKEN" \
SKYGET_AGENTS="$TOKEN=claude" \
SKYGET_MAX_JOBS=2 \
  ./target/debug/skyget serve --port "$API_PORT" >"$ROOT/daemon.log" 2>&1 &
DAEMON_PID=$!
API="http://127.0.0.1:$API_PORT"
for _ in $(seq 1 60); do
  curl -sf "$API/health" >/dev/null 2>&1 && break
  sleep 0.1
done
curl -sf "$API/health" >/dev/null || fail "daemon 没起来"

# 每个 MCP 调用一个独立 stdio 进程；MCP 只做协议翻译，任务都在 daemon 里。
mcp_call() {
  printf '%s\n' "$1" | SKYGET_API="$API" SKYGET_API_TOKEN="$TOKEN" \
    ./target/debug/skynet-mcp
}

echo "1. MCP initialize / tools/list"
MCP_INIT="$(jq -nc '{jsonrpc:"2.0",id:1,method:"initialize",params:{protocolVersion:"2025-06-18",capabilities:{},clientInfo:{name:"agent-integration",version:"0"}}}')"
INIT_OUT="$(mcp_call "$MCP_INIT")"
jq -e '.result.serverInfo.name == "skynet-mcp"' <<<"$INIT_OUT" >/dev/null \
  || fail "MCP initialize 不对：$INIT_OUT"
TOOLS="$(mcp_call "$(jq -nc '{jsonrpc:"2.0",id:2,method:"tools/list"}')")"
for t in skyget_want skyget_watch skyget_status skyget_upload; do
  jq -e --arg t "$t" '.result.tools | map(.name) | index($t) != null' <<<"$TOOLS" >/dev/null \
    || fail "MCP 缺少工具 $t：$TOOLS"
done
ok "initialize / tools/list 正常"

echo "2. MCP 提任务 → status → done"
URL="http://127.0.0.1:$SRV_PORT/f/file.bin"
WANT_ARGS="$(jq -nc --arg u "$URL" --arg d "$ROOT/mcp-out" --arg s "$SHA" \
  '{url:$u,dest:$d,sha256:$s,agent:"claude",idempotency_key:"agent-integration-1"}')"
WANT_REQ="$(jq -nc --argjson a "$WANT_ARGS" \
  '{jsonrpc:"2.0",id:3,method:"tools/call",params:{name:"skyget_want",arguments:$a}}')"
WANT_OUT="$(mcp_call "$WANT_REQ")"
ID="$(jq -r '.result.content[0].text | fromjson | .id' <<<"$WANT_OUT")"
[ -n "$ID" ] && [ "$ID" != "null" ] || fail "skyget_want 没返回 id：$WANT_OUT"
STATE=""
for _ in $(seq 1 200); do
  STATUS_REQ="$(jq -nc --arg id "$ID" \
    '{jsonrpc:"2.0",id:4,method:"tools/call",params:{name:"skyget_status",arguments:{job_id:$id}}}')"
  STATUS_OUT="$(mcp_call "$STATUS_REQ")"
  VIEW="$(jq -r '.result.content[0].text' <<<"$STATUS_OUT")"
  STATE="$(jq -r .state <<<"$VIEW")"
  [ "$STATE" = "done" ] && break
  [ "$STATE" = "failed" ] && fail "MCP 任务失败：$VIEW"
  sleep 0.1
done
[ "$STATE" = "done" ] || fail "MCP 任务没完成：$STATE"
[ "$(sha256sum "$ROOT/mcp-out/file.bin" | cut -d' ' -f1)" = "$SHA" ] \
  || fail "MCP 落地文件 sha 不对"
jq -e '.source == "agent:claude" and .sha256 == $s' --arg s "$SHA" <<<"$VIEW" >/dev/null \
  || fail "MCP 任务的来源/校验值不对：$VIEW"
ok "MCP 任务 done，文件 sha 正确，来源 agent:claude"

echo "3. MCP watch 事件流"
WATCH_REQ="$(jq -nc --arg id "$ID" \
  '{jsonrpc:"2.0",id:5,method:"tools/call",params:{name:"skyget_watch",arguments:{job_id:$id,since:0,wait_ms:0}}}')"
WATCH_OUT="$(mcp_call "$WATCH_REQ")"
WATCH_JSON="$(jq -r '.result.content[0].text' <<<"$WATCH_OUT")"
jq -e '.events | map(select(.type=="done")) | length >= 1' <<<"$WATCH_JSON" >/dev/null \
  || fail "MCP watch 没看到 done：$WATCH_JSON"
ok "watch 能重放并通过 done 收尾"

echo "4. 同幂等键 dedupe"
DUP_OUT="$(mcp_call "$WANT_REQ")"
DUP_ID="$(jq -r '.result.content[0].text | fromjson | .id' <<<"$DUP_OUT")"
[ "$DUP_ID" = "$ID" ] || fail "重复 want 返回了不同任务：$DUP_OUT"
jq -e '.result.content[0].text | fromjson | .deduped == true' <<<"$DUP_OUT" >/dev/null \
  || fail "重复 want 没标 deduped：$DUP_OUT"
ok "同幂等键返回同一条任务"

echo "5. CLI --as claude --daemon"
CLI_OUT="$(SKYGET_API="$API" SKYGET_API_TOKEN="$TOKEN" \
  ./target/debug/skyget --json get "$URL" -o "$ROOT/cli-out" --sha256 "$SHA" --as claude --daemon)"
[ "$(wc -l <<<"$CLI_OUT")" -eq 1 ] || fail "--json 不是一行：$CLI_OUT"
jq -e '.state == "done" and .source == "agent:claude"' <<<"$CLI_OUT" >/dev/null \
  || fail "CLI 结果不对：$CLI_OUT"
[ "$(sha256sum "$ROOT/cli-out/file.bin" | cut -d' ' -f1)" = "$SHA" ] \
  || fail "CLI 落地文件 sha 不对"
# 第一份内容已经进共享缓存；第二份应是 cached=true
jq -e '.cached == true' <<<"$CLI_OUT" >/dev/null \
  || fail "CLI 没有命中 MCP 刚下好的共享缓存：$CLI_OUT"
ok "CLI 走同一 daemon/台账，命中共享缓存"

echo "6. MCP watch 进度通知"
LIVE_URL="http://127.0.0.1:$SRV_PORT/slow/live.bin"
LIVE_ARGS="$(jq -nc --arg u "$LIVE_URL" --arg d "$ROOT/mcp-live" --arg s "$LIVE_SHA" \
  '{url:$u,dest:$d,sha256:$s,agent:"claude"}')"
LIVE_WANT="$(jq -nc --argjson a "$LIVE_ARGS" \
  '{jsonrpc:"2.0",id:6,method:"tools/call",params:{name:"skyget_want",arguments:$a}}')"
LIVE_ID="$(mcp_call "$LIVE_WANT" | jq -r '.result.content[0].text | fromjson | .id')"
[ -n "$LIVE_ID" ] && [ "$LIVE_ID" != "null" ] || fail "live want 没返回 id"
WATCH_LIVE="$(jq -nc --arg id "$LIVE_ID" \
  '{jsonrpc:"2.0",id:7,method:"tools/call",params:{name:"skyget_watch",arguments:{job_id:$id,since:0,wait_ms:5000},_meta:{progressToken:"live-1"}}}')"
LIVE_OUT="$(mcp_call "$WATCH_LIVE")"
NOTES="$(grep -c 'notifications/progress' <<<"$LIVE_OUT" || true)"
[ "$NOTES" -ge 1 ] || fail "watch 没发 progress 通知：$LIVE_OUT"
grep -q '"progressToken":"live-1"' <<<"$LIVE_OUT" || fail "通知里没有 progressToken"
FINAL="$(grep '"id":7' <<<"$LIVE_OUT" | tail -1)"
jq -e '.result.content[0].text | fromjson | .events | map(select(.type=="done")) | length >= 1' <<<"$FINAL" >/dev/null \
  || fail "带进度的 watch 没看到 done：$FINAL"
[ "$(sha256sum "$ROOT/mcp-live/live.bin" | cut -d' ' -f1)" = "$LIVE_SHA" ] \
  || fail "live 文件 sha 不对"
ok "watch 发出 $NOTES 条进度通知，最终 done，文件 sha 正确"

echo
echo "agent-integration-e2e: 6/6 通过"
