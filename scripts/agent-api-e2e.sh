#!/usr/bin/env bash
# agent API 端到端验收：真源、真守护进程、真 token、真事件流、真 MCP。
#
# 覆盖：
#   1. /health 不鉴权；其它接口没 token 回 401
#   2. 同一幂等键重复提交 → 同一条任务（deduped=true）
#   3. 事件流有 queued → done，可按 since 续读
#   4. 第二个不同落地目录、同 sha 的任务命中缓存（cached=true）
#   5. MCP stdio：initialize / tools/list / skyget_want 能提交任务
#
# 用法：scripts/agent-api-e2e.sh
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$(mktemp -d /tmp/skyget-agent-e2e.XXXXXX)"
SRV_PORT=$((18000 + $$ % 500))
API_PORT=$((19000 + $$ % 500))
UP_PORT=$((20000 + $$ % 500))
MULTI_PORT=$((21000 + $$ % 500))
EVIL_PORT=$((22000 + $$ % 500))
LEAK_PORT=$((22700 + $$ % 200))
FAKE_PORT=$((23600 + $$ % 200))
TOKEN="e2e-$$"
EVIL_TOKEN="evil-secret-$$"
# 第二个 agent token：证明 per-agent token 只看自己，而主 token 能看全。
CODEX_TOKEN="codex-$$"
MASTER_TOKEN="master-$$"
DAEMON_PID=""
SRV_PID=""
UP_PID=""
MULTI_PID=""
EVIL_PID=""
LEAK_PID=""
FAKE_PID=""

cleanup() {
  [ -n "$DAEMON_PID" ] && kill "$DAEMON_PID" 2>/dev/null || true
  [ -n "$SRV_PID" ] && kill "$SRV_PID" 2>/dev/null || true
  [ -n "$UP_PID" ] && kill "$UP_PID" 2>/dev/null || true
  [ -n "$MULTI_PID" ] && kill "$MULTI_PID" 2>/dev/null || true
  [ -n "$EVIL_PID" ] && kill "$EVIL_PID" 2>/dev/null || true
  [ -n "$LEAK_PID" ] && kill "$LEAK_PID" 2>/dev/null || true
  [ -n "$FAKE_PID" ] && kill "$FAKE_PID" 2>/dev/null || true
  wait 2>/dev/null || true
  if [ "${KEEP:-0}" = "1" ]; then
    echo "保留现场：$ROOT"
  else
    rm -rf "$ROOT"
  fi
}
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; echo "--- daemon log ---" >&2; cat "$ROOT/daemon.log" >&2 || true; exit 1; }
ok()   { echo "  ✓ $*"; }

# ── 源 ────────────────────────────────────────────────────────────────
mkdir -p "$ROOT/srv" "$ROOT/home" "$ROOT/out1" "$ROOT/out2" \
  "$ROOT/upload-root" "$ROOT/up-srv"
# per-agent 策略：claude 只能落到测试根目录、只能访问 127.0.0.1、只能上传 upload-root。
jq -n --arg root "$ROOT" --arg up "$ROOT/upload-root" \
  '{claude:{dest_roots:[$root],allowed_hosts:["127.0.0.1"],upload_roots:[$up],max_running:1}}' \
  > "$ROOT/agents.json"
head -c 4194304 /dev/urandom > "$ROOT/srv/file.bin"
SHA="$(sha256sum "$ROOT/srv/file.bin" | cut -d' ' -f1)"
python3 -m http.server "$SRV_PORT" --bind 127.0.0.1 --directory "$ROOT/srv" \
  >"$ROOT/srv.log" 2>&1 &
SRV_PID=$!
python3 scripts/upload-server.py "$ROOT/up-srv" "$UP_PORT" \
  >"$ROOT/up.log" 2>&1 &
UP_PID=$!
# 恶意/配置错误的 tus 服务：创建成功，但 Location 指向另一个源（换端口）。
# daemon 必须在发 HEAD/PATCH（带凭证和文件内容）之前拒绝。
mkdir -p "$ROOT/up-srv-evil"
# Location 指向一个会记录所有请求的端口；daemon 若发 HEAD/PATCH 过去，
# 日志里会留下方法/路径/是否带 Authorization —— 这才真正证明“没外发”。
python3 scripts/leak-recorder.py "$ROOT/leak.log" "$LEAK_PORT" \
  >"$ROOT/leak-srv.log" 2>&1 &
LEAK_PID=$!
UPLOAD_TUS_LOCATION="http://127.0.0.1:$LEAK_PORT/leak" \
  python3 scripts/upload-server.py "$ROOT/up-srv-evil" "$EVIL_PORT" \
  >"$ROOT/up-evil.log" 2>&1 &
EVIL_PID=$!
# /liar-data/ 会说谎（头部对、字节错），/f/ 是正常源；用来验证多源回退。
python3 scripts/testserver.py "$MULTI_PORT" "$ROOT/srv" \
  >"$ROOT/multi.log" 2>&1 &
MULTI_PID=$!

# ── 守护进程 ──────────────────────────────────────────────────────────
SKYGET_HOME="$ROOT/home" \
SKYGET_ALLOW_PRIVATE=1 \
SKYGET_ALLOWED_HOSTS="127.0.0.1" \
SKYGET_AGENTS="$TOKEN=claude,$CODEX_TOKEN=codex" \
SKYGET_API_TOKEN="$MASTER_TOKEN" \
SKYGET_AGENT_POLICIES="$ROOT/agents.json" \
SKYGET_UPLOAD_ROOTS="$ROOT/upload-root" \
SKYGET_UPLOAD_CREDENTIALS="EVIL_TOKEN" \
EVIL_TOKEN="$EVIL_TOKEN" \
SKYGET_MAX_JOBS=2 \
  "$PWD/target/debug/skyget" serve --port "$API_PORT" \
  >"$ROOT/daemon.log" 2>&1 &
DAEMON_PID=$!

API="http://127.0.0.1:$API_PORT"
AUTH=(-H "Authorization: Bearer $TOKEN")

for _ in $(seq 1 60); do
  curl -sf "$API/health" >/dev/null 2>&1 && break
  sleep 0.1
done
curl -sf "$API/health" >/dev/null || fail "守护进程没起来"

echo "1. 鉴权"
[ "$(curl -s -o /dev/null -w '%{http_code}' "$API/health")" = "200" ] || fail "health 不该要 token"
[ "$(curl -s -o /dev/null -w '%{http_code}' "$API/jobs")" = "401" ] || fail "无 token 的 /jobs 必须 401"
ok "health 放行、其它接口 401"

echo "2. 提交 + 幂等"
BODY="$(jq -nc --arg u "http://127.0.0.1:$SRV_PORT/file.bin" --arg d "$ROOT/out1" --arg s "$SHA" \
  '{url:$u,dest:$d,sha256:$s,source:"agent:e2e",idempotency_key:"run-1"}')"
FIRST="$(curl -s "${AUTH[@]}" -H 'Content-Type: application/json' -d "$BODY" "$API/jobs")"
CODE="$(curl -s -o /dev/null -w '%{http_code}' "${AUTH[@]}" -H 'Content-Type: application/json' -d "$BODY" "$API/jobs")"
ID="$(jq -r .id <<<"$FIRST")"
SECOND="$(curl -s "${AUTH[@]}" -H 'Content-Type: application/json' -d "$BODY" "$API/jobs")"
[ -n "$ID" ] && [ "$ID" != "null" ] || fail "第一次提交没拿到 id：$FIRST"
[ "$(jq -r .id <<<"$SECOND")" = "$ID" ] || fail "同一幂等键返回了不同 id：$SECOND"
[ "$(jq -r .deduped <<<"$SECOND")" = "true" ] || fail "第二次提交没标 deduped：$SECOND"
# 请求体自称 agent:e2e，但 token 属于 claude：台账必须以 token 为准。
GOT_SRC="$(curl -s "${AUTH[@]}" "$API/jobs/$ID" | jq -r .source)"
[ "$GOT_SRC" = "agent:claude" ] || fail "source 没被 token 身份覆盖：$GOT_SRC"
ok "重复提交复用同一条任务 ($ID)，source=agent:claude"

echo "3. 事件流 + 下载完成"
EV="$(curl -s "${AUTH[@]}" "$API/jobs/$ID/events?since=0&wait_ms=0")"
jq -e '.events | map(select(.type=="queued")) | length == 1' <<<"$EV" >/dev/null \
  || fail "事件流里没有 queued：$EV"
for _ in $(seq 1 100); do
  ST="$(curl -s "${AUTH[@]}" "$API/jobs/$ID")"
  [ "$(jq -r .state <<<"$ST")" = "done" ] && break
  [ "$(jq -r .state <<<"$ST")" = "failed" ] && fail "任务失败：$ST"
  sleep 0.1
done
[ "$(jq -r .state <<<"$ST")" = "done" ] || fail "任务没完成：$ST"
[ -f "$ROOT/out1/file.bin" ] || fail "文件没落地"
[ "$(sha256sum "$ROOT/out1/file.bin" | cut -d' ' -f1)" = "$SHA" ] || fail "落地文件 sha 不对"
EV2="$(curl -s "${AUTH[@]}" "$API/jobs/$ID/events?since=0&wait_ms=0")"
jq -e '.events | map(select(.type=="done")) | length >= 1' <<<"$EV2" >/dev/null \
  || fail "事件流里没有 done：$EV2"
NEXT="$(jq -r .next <<<"$EV2")"
EV3="$(curl -s "${AUTH[@]}" "$API/jobs/$ID/events?since=$NEXT&wait_ms=0")"
[ "$(jq '.events | length' <<<"$EV3")" = "0" ] || fail "since=next 还回了旧事件：$EV3"
ok "事件 queued→done，since 续读正确"

echo "4. 缓存命中（同 sha、不同落地目录）"
BODY2="$(jq -nc --arg u "http://127.0.0.1:$SRV_PORT/file.bin" --arg d "$ROOT/out2" --arg s "$SHA" \
  '{url:$u,dest:$d,sha256:$s,source:"agent:e2e",idempotency_key:"run-2"}')"
R2="$(curl -s "${AUTH[@]}" -H 'Content-Type: application/json' -d "$BODY2" "$API/jobs")"
ID2="$(jq -r .id <<<"$R2")"
for _ in $(seq 1 50); do
  ST2="$(curl -s "${AUTH[@]}" "$API/jobs/$ID2")"
  [ "$(jq -r .state <<<"$ST2")" = "done" ] && break
  sleep 0.1
done
[ "$(jq -r .state <<<"$ST2")" = "done" ] || fail "缓存任务没完成：$ST2"
[ "$(jq -r .cached <<<"$ST2")" = "true" ] || fail "第二个任务没标 cached：$ST2"
[ "$(sha256sum "$ROOT/out2/file.bin" | cut -d' ' -f1)" = "$SHA" ] || fail "缓存落地 sha 不对"
# 目标目录还不存在：缓存捷径也必须自己建父目录
BODY_C="$(jq -nc --arg u "http://127.0.0.1:$SRV_PORT/file.bin" --arg d "$ROOT/out-cache-new" --arg s "$SHA" \
  '{url:$u,dest:$d,sha256:$s,source:"agent:e2e",idempotency_key:"cache-new"}')"
RC="$(curl -s "${AUTH[@]}" -H 'Content-Type: application/json' -d "$BODY_C" "$API/jobs")"
IC="$(jq -r .id <<<"$RC")"
for _ in $(seq 1 50); do
  STC="$(curl -s "${AUTH[@]}" "$API/jobs/$IC")"
  [ "$(jq -r .state <<<"$STC")" = "done" ] && break
  sleep 0.1
done
[ -f "$ROOT/out-cache-new/file.bin" ] || fail "缓存落地的目标目录没被创建：$STC"
ok "同内容只下一次，第二份直接命中缓存（目标目录不存在时也能建）"

echo "5. MCP stdio"
MCP_IN="$(jq -nc '{jsonrpc:"2.0",id:1,method:"initialize",params:{protocolVersion:"2025-06-18",capabilities:{},clientInfo:{name:"e2e",version:"0"}}}')
$(jq -nc '{jsonrpc:"2.0",id:2,method:"tools/list"}')
$(jq -nc --arg u "http://127.0.0.1:$SRV_PORT/file.bin" --arg d "$ROOT/out3" --arg s "$SHA" \
  '{jsonrpc:"2.0",id:3,method:"tools/call",params:{name:"skyget_want",arguments:{url:$u,dest:$d,sha256:$s,idempotency_key:"mcp-1",agent:"mcp-e2e"}}}')
$(jq -nc '{jsonrpc:"2.0",id:4,method:"tools/call",params:{name:"skyget_agents",arguments:{}}}')"
mkdir -p "$ROOT/out3"
MCP_OUT="$(printf '%s\n' "$MCP_IN" | SKYGET_API="$API" SKYGET_API_TOKEN="$TOKEN" ./target/debug/skynet-mcp)"
jq -e 'select(.id==1) | .result.serverInfo.name == "skynet-mcp"' <<<"$MCP_OUT" >/dev/null \
  || fail "MCP initialize 不对：$MCP_OUT"
jq -e 'select(.id==2) | .result.tools | map(.name) | index("skyget_want") != null' <<<"$MCP_OUT" >/dev/null \
  || fail "MCP tools/list 没有 skyget_want：$MCP_OUT"
MID="$(jq -r 'select(.id==3) | .result.content[0].text | fromjson | .id' <<<"$MCP_OUT")"
[ -n "$MID" ] && [ "$MID" != "null" ] || fail "MCP want 没拿到 id：$MCP_OUT"
for _ in $(seq 1 100); do
  MST="$(curl -s "${AUTH[@]}" "$API/jobs/$MID")"
  [ "$(jq -r .state <<<"$MST")" = "done" ] && break
  [ "$(jq -r .state <<<"$MST")" = "failed" ] && fail "MCP 任务失败：$MST"
  sleep 0.1
done
[ "$(jq -r .state <<<"$MST")" = "done" ] || fail "MCP 任务没完成：$MST"
jq -e 'select(.id==4) | .result.content[0].text | fromjson | .caller == "agent:claude"' <<<"$MCP_OUT" >/dev/null \
  || fail "MCP skyget_agents 身份不对：$MCP_OUT"
ok "MCP initialize / tools/list / want / agents 全通（$MID）"

echo "6. CLI agent 路径（--as claude → daemon）"
mkdir -p "$ROOT/out4"
CLI_OUT="$(SKYGET_HOME="$ROOT/cli-home" SKYGET_API="$API" SKYGET_API_TOKEN="$TOKEN" \
  ./target/debug/skyget --json --as claude --daemon \
  "http://127.0.0.1:$SRV_PORT/file.bin" -o "$ROOT/out4" --sha256 "$SHA" \
  2>"$ROOT/cli.err")" || fail "CLI daemon 路径失败：$(cat "$ROOT/cli.err")"
jq -e '.state == "done"' <<<"$CLI_OUT" >/dev/null \
  || fail "CLI daemon 路径没完成：$CLI_OUT / $(cat "$ROOT/cli.err")"
jq -e --arg p "$ROOT/out4/file.bin" '.path == $p' <<<"$CLI_OUT" >/dev/null \
  || fail "CLI 落地路径不对：$CLI_OUT"
[ "$(sha256sum "$ROOT/out4/file.bin" | cut -d' ' -f1)" = "$SHA" ] || fail "CLI 落地 sha 不对"
ok "CLI 把 agent 任务交给 daemon，结果回填到 stdout"

echo "7. 缓存命中也要过 dest 边界（CLI 独立路径）"
mkdir -p "$ROOT/allowed" "$ROOT/outside"
CLI_HOME="$ROOT/cli-home2"
SKYGET_HOME="$CLI_HOME" SKYGET_DEST_ROOTS="$ROOT/allowed" \
  ./target/debug/skyget --json --standalone \
  "http://127.0.0.1:$SRV_PORT/file.bin" -o "$ROOT/allowed" --sha256 "$SHA" \
  >/dev/null 2>"$ROOT/cli-allow.err" || fail "先把内容放进缓存失败：$(cat "$ROOT/cli-allow.err")"
OUT_JSON="$(SKYGET_HOME="$CLI_HOME" SKYGET_DEST_ROOTS="$ROOT/allowed" \
  ./target/debug/skyget --json --standalone \
  "http://127.0.0.1:$SRV_PORT/file.bin" -o "$ROOT/outside" --sha256 "$SHA" \
  2>"$ROOT/cli-deny.err")" && fail "越权落地居然成功了：$OUT_JSON"
jq -e '.error_code == "policy.dest_denied"' <<<"$OUT_JSON" >/dev/null \
  || fail "越权落地没给出 policy.dest_denied：$OUT_JSON / $(cat "$ROOT/cli-deny.err")"
[ ! -e "$ROOT/outside/file.bin" ] || fail "越权落地真的写了文件"
ok "缓存捷径也不能把文件写出允许目录"

echo "8. 缓存 pin（agent 说「我在用」）"
SKYGET_HOME="$ROOT/cli-home" SKYGET_API="$API" SKYGET_API_TOKEN="$TOKEN" \
  ./target/debug/skyget --json pin "$SHA" --ttl 1h --as e2e >/dev/null \
  || fail "pin 失败"
PIN_JSON="$(SKYGET_HOME="$ROOT/cli-home" SKYGET_API="$API" SKYGET_API_TOKEN="$TOKEN" \
  ./target/debug/skyget --json pins)"
# 有 token 时身份以 token 为准：--as e2e 只是自称，台账里记 agent:claude。
jq -e --arg s "$SHA" 'map(select(.sha256==$s and .agent=="agent:claude")) | length == 1' <<<"$PIN_JSON" >/dev/null \
  || fail "pin 列表里没有这条：$PIN_JSON"
SKYGET_HOME="$ROOT/cli-home" SKYGET_API="$API" SKYGET_API_TOKEN="$TOKEN" \
  ./target/debug/skyget --json unpin "$SHA" --as e2e >/dev/null \
  || fail "unpin 失败"
PIN_JSON2="$(SKYGET_HOME="$ROOT/cli-home" SKYGET_API="$API" SKYGET_API_TOKEN="$TOKEN" \
  ./target/debug/skyget --json pins)"
jq -e --arg s "$SHA" 'map(select(.sha256==$s)) | length == 0' <<<"$PIN_JSON2" >/dev/null \
  || fail "unpin 后还留着：$PIN_JSON2"
ok "pin / pins / unpin 全通"

echo "9. /cas 共享缓存直读（含 Range）"
curl -sf -H "Authorization: Bearer $TOKEN" "$API/cas/$SHA" -o "$ROOT/cas.bin" \
  || fail "GET /cas 失败"
[ "$(sha256sum "$ROOT/cas.bin" | cut -d' ' -f1)" = "$SHA" ] || fail "/cas 内容不对"
RCODE="$(curl -s -o "$ROOT/cas.part" -D "$ROOT/cas.h" -w '%{http_code}' \
  -H "Authorization: Bearer $TOKEN" -H 'Range: bytes=0-1023' "$API/cas/$SHA")"
[ "$RCODE" = "206" ] || fail "/cas Range 没回 206：$RCODE"
head -c 1024 "$ROOT/srv/file.bin" | cmp -s - "$ROOT/cas.part" || fail "/cas Range 内容不对"
[ "$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $TOKEN" \
  "$API/cas/0000000000000000000000000000000000000000000000000000000000000000")" = "404" ] \
  || fail "/cas 不存在的内容应 404"
ok "/cas 可整取、可 Range，未知内容 404"

echo "10. /metrics"
MET="$(curl -s -H "Authorization: Bearer $TOKEN" "$API/metrics")"
grep -q '^skyget_up 1$' <<<"$MET" || fail "metrics 缺 skyget_up：$MET"
grep -q '^skyget_jobs{state="running"} ' <<<"$MET" || fail "metrics 缺 running：$MET"
grep -q '^skyget_cache_bytes ' <<<"$MET" || fail "metrics 缺 cache_bytes：$MET"
grep -q '^skyget_build_info{version="' <<<"$MET" || fail "metrics 缺 build_info：$MET"
grep -q '^skyget_uptime_seconds ' <<<"$MET" || fail "metrics 缺 uptime：$MET"
grep -q '^skyget_scheduler_capacity{kind="running"} ' <<<"$MET" || fail "metrics 缺容量：$MET"
grep -q '^skyget_tombstones ' <<<"$MET" || fail "metrics 缺 tombstones：$MET"
grep -q '^skyget_jobs_by_source{source="agent:claude"} ' <<<"$MET" \
  || fail "metrics 缺 per-source 任务数：$MET"
grep -q '^# TYPE skyget_running_by_source gauge$' <<<"$MET" \
  || fail "metrics 缺 per-source 运行数：$MET"
[ "$(curl -s -o /dev/null -w '%{http_code}' "$API/metrics")" = "401" ] \
  || fail "metrics 无 token 应 401"
ok "Prometheus 指标可抓取，且需要 token"

echo "11. skyget top（终端驾驶舱）"
TOP="$(SKYGET_HOME="$ROOT/cli-home" SKYGET_API="$API" SKYGET_API_TOKEN="$TOKEN" \
  ./target/debug/skyget top)"
grep -q '^JOBS ' <<<"$TOP" || fail "top 没输出表头：$TOP"
grep -q '完成' <<<"$TOP" || fail "top 里看不到已完成任务：$TOP"
ok "skyget top 能读 daemon 台账"

echo "12. 安全回归：畸形 sha 与悬空符号链接"
BAD_CODE="$(curl -s -o "$ROOT/bad-sha.json" -w '%{http_code}' \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"url":"http://127.0.0.1:'"$SRV_PORT"'/file.bin","dest":"'"$ROOT/out-bad"'","sha256":"/home/skynet/.ssh/id_ed25519"}' \
  "$API/jobs")"
[ "$BAD_CODE" = "400" ] || fail "绝对路径 sha 应 400，实际 $BAD_CODE：$(cat "$ROOT/bad-sha.json")"
jq -e '.code=="input.bad_sha256"' "$ROOT/bad-sha.json" >/dev/null \
  || fail "畸形 sha 的 code 不对：$(cat "$ROOT/bad-sha.json")"
[ "$(curl -s -o /dev/null -w '%{http_code}' "$API/health")" = "200" ] \
  || fail "畸形 sha 把 daemon 打挂了"
# 多字节 sha 以前会在 &sha[..2] 处 panic；现在必须 400 且 daemon 还活着
BAD_CODE2="$(curl -s -o "$ROOT/bad-sha2.json" -w '%{http_code}' \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"url":"http://127.0.0.1:'"$SRV_PORT"'/file.bin","dest":"'"$ROOT/out-bad"'","sha256":"中"}' \
  "$API/jobs")"
[ "$BAD_CODE2" = "400" ] || fail "多字节 sha 应 400，实际 $BAD_CODE2"
[ "$(curl -s -o /dev/null -w '%{http_code}' "$API/health")" = "200" ] \
  || fail "多字节 sha 把 daemon 打挂了"

# 允许根里放一个悬空符号链接：落地必须避开它，不能跟着写到根外
mkdir -p "$ROOT/allowed2" "$ROOT/outside2"
head -c 1048576 /dev/urandom > "$ROOT/srv/link.bin"
LSHA="$(sha256sum "$ROOT/srv/link.bin" | cut -d' ' -f1)"
ln -s "$ROOT/outside2/target.bin" "$ROOT/allowed2/link.bin"
SKYGET_HOME="$ROOT/cli-home3" SKYGET_ALLOW_PRIVATE=1 SKYGET_DEST_ROOTS="$ROOT/allowed2" \
  ./target/debug/skyget --json --standalone \
  "http://127.0.0.1:$SRV_PORT/link.bin" -o "$ROOT/allowed2" --sha256 "$LSHA" \
  >"$ROOT/link.json" 2>"$ROOT/link.err" || fail "悬空链接场景下载失败：$(cat "$ROOT/link.err")"
[ ! -e "$ROOT/outside2/target.bin" ] || fail "跟着悬空链接写到根外了"
[ -f "$ROOT/allowed2/link (1).bin" ] || fail "没有避开悬空链接另选文件名"
[ "$(sha256sum "$ROOT/allowed2/link (1).bin" | cut -d' ' -f1)" = "$LSHA" ] || fail "避开链接后的文件内容不对"
ok "畸形 sha 被拒且 daemon 存活；悬空符号链接未被跟随"

echo "13. 缓存投毒：块内容被换掉后 daemon 不能秒传坏文件"
BLOB="$ROOT/home/blobs/${SHA:0:2}/$SHA"
[ -f "$BLOB" ] || fail "找不到缓存块：$BLOB"
head -c 4194304 /dev/zero > "$BLOB"
BODY3="$(jq -nc --arg u "http://127.0.0.1:$SRV_PORT/file.bin" --arg d "$ROOT/out6" --arg s "$SHA" \
  '{url:$u,dest:$d,sha256:$s,source:"agent:e2e",idempotency_key:"poison-1"}')"
R3="$(curl -s "${AUTH[@]}" -H 'Content-Type: application/json' -d "$BODY3" "$API/jobs")"
ID3="$(jq -r .id <<<"$R3")"
[ -n "$ID3" ] && [ "$ID3" != "null" ] || fail "投毒任务提交失败：$R3"
for _ in $(seq 1 100); do
  ST3="$(curl -s "${AUTH[@]}" "$API/jobs/$ID3")"
  [ "$(jq -r .state <<<"$ST3")" = "done" ] && break
  [ "$(jq -r .state <<<"$ST3")" = "failed" ] && fail "投毒任务失败：$ST3"
  sleep 0.1
done
[ "$(jq -r .state <<<"$ST3")" = "done" ] || fail "投毒任务没完成：$ST3"
[ "$(jq -r .cached <<<"$ST3")" = "false" ] || fail "被投毒的块不该算缓存命中：$ST3"
[ "$(sha256sum "$ROOT/out6/file.bin" | cut -d' ' -f1)" = "$SHA" ] || fail "投毒后落地内容不对"
[ "$(sha256sum "$BLOB" | cut -d' ' -f1)" = "$SHA" ] || fail "缓存块没有被验证过的内容替换"
ok "被篡改的缓存块被识破并替换，新任务拿到正确文件"

echo "14. 符号链接缓存块：不能变成任意文件读取"
rm -f "$BLOB"
head -c 4194304 /dev/urandom > "$ROOT/secret.bin"
ln -s "$ROOT/secret.bin" "$BLOB"
BODY4="$(jq -nc --arg u "http://127.0.0.1:$SRV_PORT/file.bin" --arg d "$ROOT/out7" --arg s "$SHA" \
  '{url:$u,dest:$d,sha256:$s,source:"agent:e2e",idempotency_key:"symlink-1"}')"
R4="$(curl -s "${AUTH[@]}" -H 'Content-Type: application/json' -d "$BODY4" "$API/jobs")"
ID4="$(jq -r .id <<<"$R4")"
[ -n "$ID4" ] && [ "$ID4" != "null" ] || fail "符号链接任务提交失败：$R4"
for _ in $(seq 1 100); do
  ST4="$(curl -s "${AUTH[@]}" "$API/jobs/$ID4")"
  [ "$(jq -r .state <<<"$ST4")" = "done" ] && break
  [ "$(jq -r .state <<<"$ST4")" = "failed" ] && fail "符号链接任务失败：$ST4"
  sleep 0.1
done
[ "$(sha256sum "$ROOT/out7/file.bin" | cut -d' ' -f1)" = "$SHA" ] || fail "符号链接缓存块导致落地内容不对"
[ -f "$BLOB" ] && [ ! -L "$BLOB" ] || fail "缓存块仍是符号链接"
ok "符号链接缓存块未被跟随，已替换为普通文件"

echo "15. 上传：PUT + tus"
head -c 2097152 /dev/urandom > "$ROOT/upload-root/src.bin"
USHA="$(sha256sum "$ROOT/upload-root/src.bin" | cut -d' ' -f1)"
CLI_ENV=(env SKYGET_HOME="$ROOT/cli-home" SKYGET_API="$API" SKYGET_API_TOKEN="$TOKEN")
PUT_OUT="$("${CLI_ENV[@]}" ./target/debug/skyget --json upload \
  "$ROOT/upload-root/src.bin" "http://127.0.0.1:$UP_PORT/put/src.bin" \
  --protocol put --sha256 "$USHA" --as e2e 2>"$ROOT/up-put.err")" \
  || fail "PUT 上传失败：$(cat "$ROOT/up-put.err")"
jq -e '.state=="done" and .direction=="upload"' <<<"$PUT_OUT" >/dev/null \
  || fail "PUT 上传结果不对：$PUT_OUT"
[ "$(sha256sum "$ROOT/up-srv/src.bin" | cut -d' ' -f1)" = "$USHA" ] || fail "PUT 远端内容不对"
ok "PUT 上传完成，远端 sha 一致"

TUS_OUT="$("${CLI_ENV[@]}" ./target/debug/skyget --json upload \
  "$ROOT/upload-root/src.bin" "http://127.0.0.1:$UP_PORT/tus" \
  --protocol tus --sha256 "$USHA" --as e2e 2>"$ROOT/up-tus.err")" \
  || fail "tus 上传失败：$(cat "$ROOT/up-tus.err")"
jq -e '.state=="done" and .direction=="upload"' <<<"$TUS_OUT" >/dev/null \
  || fail "tus 上传结果不对：$TUS_OUT"
TUS_FILE="$(ls "$ROOT/up-srv"/tus-*.bin 2>/dev/null | head -1)"
[ -n "$TUS_FILE" ] || fail "tus 远端没有文件"
[ "$(sha256sum "$TUS_FILE" | cut -d' ' -f1)" = "$USHA" ] || fail "tus 远端内容不对"
ok "tus 上传完成，远端 sha 一致"

DENY_CODE="$(curl -s -o "$ROOT/up-deny.json" -w '%{http_code}' "${AUTH[@]}" \
  -H 'Content-Type: application/json' \
  -d "{\"path\":\"$ROOT/srv/file.bin\",\"url\":\"http://127.0.0.1:$UP_PORT/put/x\"}" \
  "$API/uploads")"
[ "$DENY_CODE" = "403" ] || fail "根目录外的上传应 403，实际 $DENY_CODE"
jq -e '.code=="policy.upload_source_denied"' "$ROOT/up-deny.json" >/dev/null \
  || fail "根目录外上传的 code 不对：$(cat "$ROOT/up-deny.json")"
ok "上传源目录边界生效（默认/根外均拒绝）"

echo "16. per-agent 落地根"
OUTSIDE="/tmp/skyget-outside-$$"
BODY_OUT="$(jq -nc --arg u "http://127.0.0.1:$SRV_PORT/file.bin" --arg d "$OUTSIDE" \
  '{url:$u,dest:$d,source:"agent:e2e"}')"
DENY_DEST_CODE="$(curl -s -o "$ROOT/deny-dest.json" -w '%{http_code}' \
  "${AUTH[@]}" -H 'Content-Type: application/json' -d "$BODY_OUT" "$API/jobs")"
[ "$DENY_DEST_CODE" = "403" ] || fail "per-agent 根目录外应 403，实际 $DENY_DEST_CODE"
jq -e '.code=="policy.dest_denied"' "$ROOT/deny-dest.json" >/dev/null \
  || fail "per-agent dest 拒绝的 code 不对：$(cat "$ROOT/deny-dest.json")"
ok "claude 的策略只允许 $ROOT 下的落点"

echo "17. 缓存 export / import"
SKYGET_HOME="$ROOT/home" ./target/debug/skyget --json export "$ROOT/cas-out" "$SHA" \
  >"$ROOT/export.json" 2>"$ROOT/export.err" || fail "export 失败：$(cat "$ROOT/export.err")"
jq -e '.blobs==1' "$ROOT/export.json" >/dev/null || fail "export 结果不对：$(cat "$ROOT/export.json")"
[ -f "$ROOT/cas-out/$SHA" ] || fail "导出文件不存在"
SKYGET_HOME="$ROOT/cas-home" ./target/debug/skyget --json import "$ROOT/cas-out" \
  >"$ROOT/import.json" 2>"$ROOT/import.err" || fail "import 失败：$(cat "$ROOT/import.err")"
[ "$(sha256sum "$ROOT/cas-home/blobs/${SHA:0:2}/$SHA" | cut -d' ' -f1)" = "$SHA" ] \
  || fail "导入后的缓存块内容不对"
# 同长度篡改：导入必须拒绝
head -c 4194304 /dev/zero > "$ROOT/cas-out/$SHA"
SKYGET_HOME="$ROOT/cas-home2" ./target/debug/skyget --json import "$ROOT/cas-out" \
  >"$ROOT/import-bad.json" 2>"$ROOT/import-bad.err" && fail "坏块导入居然成功了"
grep -q '内容不符' "$ROOT/import-bad.err" || fail "坏块导入的错误不明确：$(cat "$ROOT/import-bad.err")"
ok "export/import 往返校验通过，坏块被拒绝"

echo "18. 多源回退（说谎的镜像 → 下一个源）"
# 用独立内容，避免直接命中前面已经下好的缓存。
head -c 4194304 /dev/urandom > "$ROOT/srv/multi.bin"
MSHA="$(sha256sum "$ROOT/srv/multi.bin" | cut -d' ' -f1)"
BAD="http://127.0.0.1:$MULTI_PORT/liar-data/multi.bin"
GOOD="http://127.0.0.1:$MULTI_PORT/f/multi.bin"
MULTI_BODY="$(jq -nc --arg u "$BAD" --arg g "$GOOD" --arg d "$ROOT/out8" --arg s "$MSHA" \
  '{url:$u,sources:[$g],dest:$d,sha256:$s,source:"agent:e2e",idempotency_key:"multi-1"}')"
MRESP="$(curl -s "${AUTH[@]}" -H 'Content-Type: application/json' -d "$MULTI_BODY" "$API/jobs")"
MID="$(jq -r .id <<<"$MRESP")"
[ -n "$MID" ] && [ "$MID" != "null" ] || fail "多源任务提交失败：$MRESP"
for _ in $(seq 1 150); do
  MST="$(curl -s "${AUTH[@]}" "$API/jobs/$MID")"
  [ "$(jq -r .state <<<"$MST")" = "done" ] && break
  [ "$(jq -r .state <<<"$MST")" = "failed" ] && fail "多源任务失败：$MST"
  sleep 0.1
done
[ "$(jq -r .state <<<"$MST")" = "done" ] || fail "多源任务没完成：$MST"
[ "$(sha256sum "$ROOT/out8/multi.bin" | cut -d' ' -f1)" = "$MSHA" ] || fail "多源落地内容不对"
[ "$(jq '.sources | length' <<<"$MST")" = "2" ] || fail "sources 没落库：$MST"
[ "$(jq -r .url <<<"$MST")" = "$BAD" ] || fail "换源成功后 url 应仍是主源：$MST"
MEV="$(curl -s "${AUTH[@]}" "$API/jobs/$MID/events?since=0&wait_ms=0")"
jq -e '.events | map(select(.type=="note" and (.data.text|contains("换下一个源")))) | length >= 1' <<<"$MEV" >/dev/null \
  || fail "事件流没有换源记录：$MEV"
NOMulti="$(jq -nc --arg u "$GOOD" --arg d "$ROOT/out9" \
  '{url:$u,sources:["http://127.0.0.1:'"$MULTI_PORT"'/f/multi.bin"],dest:$d}')"
MCODE="$(curl -s -o "$ROOT/multi-nosha.json" -w '%{http_code}' "${AUTH[@]}" \
  -H 'Content-Type: application/json' -d "$NOMulti" "$API/jobs")"
[ "$MCODE" = "400" ] || fail "多源缺 sha 应 400，实际 $MCODE"
jq -e '.code=="input.multi_source_needs_sha"' "$ROOT/multi-nosha.json" >/dev/null \
  || fail "多源缺 sha 的 code 不对：$(cat "$ROOT/multi-nosha.json")"
ok "说谎镜像被识破并换源，最终 sha 一致；缺 sha 的多源被拒"

echo "19. GET /agents（身份 / 策略 / 并发）"
AG="$(curl -s "${AUTH[@]}" "$API/agents")"
jq -e '.registry == true and .caller == "agent:claude"' <<<"$AG" >/dev/null \
  || fail "agents 身份不对：$AG"
jq -e --arg root "$ROOT" \
  '.agents | map(select(.name=="claude" and .source=="agent:claude" and (.jobs > 0) and .max_running == 1 and .policy.dest_roots[0] == $root)) | length == 1' \
  <<<"$AG" >/dev/null || fail "claude 的策略/并发没暴露对：$AG"
for t in "$TOKEN" "$CODEX_TOKEN" "$MASTER_TOKEN"; do
  grep -q "$t" <<<"$AG" && fail "agents 接口泄露了 token：$t"
done
[ "$(curl -s -o /dev/null -w '%{http_code}' "$API/agents")" = "401" ] \
  || fail "agents 无 token 应 401"
# 主 token 是人类管理员：看得到全部 agent，但不冒充任何 agent。
ADMIN_AG="$(curl -s -H "Authorization: Bearer $MASTER_TOKEN" "$API/agents")"
jq -e '.caller == null and (.agents | map(.name) | sort) == ["claude","codex"]' \
  <<<"$ADMIN_AG" >/dev/null || fail "主 token 应看到全部 agent：$ADMIN_AG"
# codex 的 token 只看得到自己（它还没有任务）。
CODEX_AG="$(curl -s -H "Authorization: Bearer $CODEX_TOKEN" "$API/agents")"
jq -e '.caller == "agent:codex" and (.agents | map(.name) == ["codex"])' \
  <<<"$CODEX_AG" >/dev/null || fail "codex 应只看自己：$CODEX_AG"
# 主 token 不能靠请求体 source 冒充 agent 去套别人的策略/上传根。
SPOOF="$(jq -nc --arg u "http://127.0.0.1:$SRV_PORT/file.bin" --arg d "$ROOT/out-spoof" \
  '{url:$u,dest:$d,source:"agent:claude"}')"
SPOOF_CODE="$(curl -s -o "$ROOT/spoof.json" -w '%{http_code}' -H "Authorization: Bearer $MASTER_TOKEN" \
  -H 'Content-Type: application/json' -d "$SPOOF" "$API/jobs")"
[ "$SPOOF_CODE" = "403" ] || fail "主 token 自称 agent 应 403，实际 $SPOOF_CODE"
jq -e '.code=="input.agent_source_requires_token"' "$ROOT/spoof.json" >/dev/null \
  || fail "自称 agent 的 code 不对：$(cat "$ROOT/spoof.json")"
UP_SPOOF="$(jq -nc --arg p "$ROOT/upload-root/src.bin" \
  --arg u "http://127.0.0.1:$UP_PORT/put/spoof.bin" \
  '{path:$p,url:$u,source:"agent:claude"}')"
UP_SPOOF_CODE="$(curl -s -o "$ROOT/up-spoof.json" -w '%{http_code}' -H "Authorization: Bearer $MASTER_TOKEN" \
  -H 'Content-Type: application/json' -d "$UP_SPOOF" "$API/uploads")"
[ "$UP_SPOOF_CODE" = "403" ] || fail "上传自称 agent 应 403，实际 $UP_SPOOF_CODE"
jq -e '.code=="input.agent_source_requires_token"' "$ROOT/up-spoof.json" >/dev/null \
  || fail "上传自称 agent 的 code 不对：$(cat "$ROOT/up-spoof.json")"
ok "身份 / 策略 / 并发可见；agent 只看自己，主 token 看全部且不能冒充 agent"

echo "20. tus Location 跨源拒绝（SSRF / 凭证外发边界）"
cp "$ROOT/srv/file.bin" "$ROOT/upload-root/evil.bin"
EVIL_SHA="$(sha256sum "$ROOT/upload-root/evil.bin" | cut -d' ' -f1)"
EVIL_BODY="$(jq -nc --arg p "$ROOT/upload-root/evil.bin" \
  --arg u "http://127.0.0.1:$EVIL_PORT/tus" --arg s "$EVIL_SHA" \
  '{path:$p,url:$u,protocol:"tus",sha256:$s,source:"agent:e2e",credential_ref:"env:EVIL_TOKEN"}')"
EVIL_RESP="$(curl -s "${AUTH[@]}" -H 'Content-Type: application/json' -d "$EVIL_BODY" "$API/uploads")"
EID="$(jq -r .id <<<"$EVIL_RESP")"
[ -n "$EID" ] && [ "$EID" != "null" ] || fail "tus 恶意 Location 提交失败：$EVIL_RESP"
ESTATE=""
for _ in $(seq 1 100); do
  EST="$(curl -s "${AUTH[@]}" "$API/jobs/$EID")"
  ESTATE="$(jq -r .state <<<"$EST")"
  [ "$ESTATE" = "failed" ] && break
  [ "$ESTATE" = "done" ] && fail "跨源 Location 居然传成功了：$EST"
  sleep 0.1
done
[ "$ESTATE" = "failed" ] || fail "跨源 Location 没失败：$EST"
jq -e '.error_code=="policy.egress_denied"' <<<"$EST" >/dev/null \
  || fail "跨源 Location 的错误码不对：$EST"
[ ! -s "$ROOT/leak.log" ] || fail "跨源 Location 后仍收到请求/凭证：$(cat "$ROOT/leak.log")"
ok "跨源 Location 被拒，且目标端口一个请求都没收到"

echo "21. 预签名 URL 的 API 视图脱敏"
SIGNED_URL="http://127.0.0.1:$SRV_PORT/file.bin?X-Amz-Signature=deadbeef&X-Amz-Credential=AKIA&part=1"
SIGNED_BODY="$(jq -nc --arg u "$SIGNED_URL" --arg d "$ROOT/out-signed" --arg s "$SHA" \
  '{url:$u,dest:$d,sha256:$s,source:"agent:e2e"}')"
SRESP="$(curl -s "${AUTH[@]}" -H 'Content-Type: application/json' -d "$SIGNED_BODY" "$API/jobs")"
SID="$(jq -r .id <<<"$SRESP")"
[ -n "$SID" ] && [ "$SID" != "null" ] || fail "签名 URL 提交失败：$SRESP"
for _ in $(seq 1 100); do
  SST="$(curl -s "${AUTH[@]}" "$API/jobs/$SID")"
  [ "$(jq -r .state <<<"$SST")" = "done" ] && break
  [ "$(jq -r .state <<<"$SST")" = "failed" ] && fail "签名 URL 下载失败：$SST"
  sleep 0.1
done
[ "$(jq -r .state <<<"$SST")" = "done" ] || fail "签名 URL 没下完：$SST"
grep -q 'deadbeef' <<<"$SST" && fail "TaskView 泄露了签名：$SST"
jq -e '.url | (contains("redacted") and (contains("part=1")))' <<<"$SST" >/dev/null \
  || fail "签名脱敏后的 URL 形状不对：$SST"
ok "签名/凭证参数在 API 视图中被替换为 redacted，下载仍用真实 URL 完成"

echo "22. 配置写错时 daemon 拒绝启动（fail closed）"
BAD_PORT=$((23000 + $$ % 500))
set +e
timeout 5s env SKYGET_HOME="$ROOT/bad-home-1" SKYGET_AGENTS=broken \
  ./target/debug/skyget serve --port "$BAD_PORT" >"$ROOT/bad1.log" 2>&1
BAD1=$?
timeout 5s env -u SKYGET_AGENTS SKYGET_HOME="$ROOT/bad-home-2" \
  SKYGET_AGENT_POLICIES="$ROOT/agents.json" \
  ./target/debug/skyget serve --port "$BAD_PORT" >"$ROOT/bad2.log" 2>&1
BAD2=$?
set -e
[ "$BAD1" -ne 0 ] && [ "$BAD1" -ne 124 ] || fail "SKYGET_AGENTS=broken 居然启动了（rc=$BAD1）"
grep -q '没有任何合法' "$ROOT/bad1.log" || fail "坏注册表错误不明确：$(cat "$ROOT/bad1.log")"
[ "$BAD2" -ne 0 ] && [ "$BAD2" -ne 124 ] || fail "有策略无注册表居然启动了（rc=$BAD2）"
grep -q '没有可用的 SKYGET_AGENTS' "$ROOT/bad2.log" || fail "策略无身份错误不明确：$(cat "$ROOT/bad2.log")"
ok "坏 token 注册表 / 有策略无身份都拒绝启动"

echo "23. skyget doctor"
DOC="$(SKYGET_HOME="$ROOT/home" SKYGET_API="$API" SKYGET_API_TOKEN="$TOKEN" \
  ./target/debug/skyget --json doctor)"
jq -e '.ok == true and .daemon.reachable == true and (.identity | contains("agent:claude"))' \
  <<<"$DOC" >/dev/null || fail "doctor JSON 不对：$DOC"
[ "$(wc -l <<<"$DOC")" -eq 1 ] || fail "--json doctor 必须是 stdout 一行 JSON"
jq -e '.policy.upload_roots | length >= 1' <<<"$DOC" >/dev/null \
  || fail "doctor 没读到上传根：$DOC"
SKYGET_HOME="$ROOT/home" SKYGET_API="$API" SKYGET_API_TOKEN="$TOKEN" \
  ./target/debug/skyget doctor | grep -q '结论：可用' || fail "doctor 人话结论不是可用"
set +e
SKYGET_HOME="$ROOT/doctor-down" SKYGET_API=http://127.0.0.1:1 \
  ./target/debug/skyget doctor >"$ROOT/doctor-down.log" 2>&1
DOWN_RC=$?
set -e
[ "$DOWN_RC" -ne 0 ] || fail "daemon 不可达时 doctor 应非 0"
grep -q '连不上' "$ROOT/doctor-down.log" || fail "doctor 没明说连不上：$(cat "$ROOT/doctor-down.log")"
# HTTP 200 但 body 不是 JSON：不能算成功。
python3 scripts/bad-daemon.py "$FAKE_PORT" >"$ROOT/fake.log" 2>&1 &
FAKE_PID=$!
for _ in $(seq 1 30); do curl -sf "http://127.0.0.1:$FAKE_PORT/health" >/dev/null && break; sleep 0.1; done
set +e
SKYGET_HOME="$ROOT/doctor-fake" SKYGET_API="http://127.0.0.1:$FAKE_PORT" \
  ./target/debug/skyget --json doctor >"$ROOT/doctor-fake.json" 2>/dev/null
FAKE_RC=$?
set -e
[ "$FAKE_RC" -ne 0 ] || fail "200-坏 JSON 的 daemon 让 doctor 成功了"
jq -e '.ok == false and ([.checks[] | select(.name=="daemon")][0].detail | contains("不是 JSON"))' \
  "$ROOT/doctor-fake.json" >/dev/null || fail "doctor 没识别 200-坏 JSON：$(cat "$ROOT/doctor-fake.json")"
ok "doctor 能报目录/策略/daemon/身份；不可达/坏 JSON 都非 0"

echo "24. skyget agents"
AGCLI="$(SKYGET_HOME="$ROOT/home" SKYGET_API="$API" SKYGET_API_TOKEN="$TOKEN" \
  ./target/debug/skyget --json agents)"
jq -e '.agents | map(.name) | index("claude") != null' <<<"$AGCLI" >/dev/null \
  || fail "agents --json 看不到 claude：$AGCLI"
SKYGET_HOME="$ROOT/home" SKYGET_API="$API" SKYGET_API_TOKEN="$TOKEN" \
  ./target/debug/skyget agents | grep -q '在跑' || fail "agents 人话输出不对"
ok "终端机群视图能读 /agents"

echo
echo "agent-api-e2e: 24/24 通过"
