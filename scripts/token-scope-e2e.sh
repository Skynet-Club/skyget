#!/usr/bin/env bash
# token 隔离验收：两个 agent 的 token，各自只能看见/控制自己的任务。
set -u

cd "$(dirname "$0")/.." || exit 1
ROOT="$(mktemp -d /tmp/skyget-scope.XXXXXX)"
SRV_PORT=$((37400 + $$ % 200))
API_PORT=$((38400 + $$ % 200))
TOK_A="tok-a-$$"
TOK_B="tok-b-$$"
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

mkdir -p "$ROOT/srv" "$ROOT/home" "$ROOT/dl"
head -c 8388608 /dev/urandom >"$ROOT/srv/file.bin"

python3 scripts/testserver.py "$SRV_PORT" "$ROOT/srv" >"$ROOT/srv.log" 2>&1 &
SRV_PID=$!

SKYGET_HOME="$ROOT/home" SKYGET_ALLOW_PRIVATE=1 \
SKYGET_AGENTS="$TOK_A=claude,$TOK_B=pi" \
  ./target/debug/skyget serve --port "$API_PORT" >"$ROOT/daemon.log" 2>&1 &
DAEMON_PID=$!
API="http://127.0.0.1:$API_PORT"
for _ in $(seq 1 60); do
  curl -sf "$API/health" >/dev/null 2>&1 && break
  sleep 0.1
done
curl -sf "$API/health" >/dev/null || fail "daemon 没起来"
export SKYGET_API="$API"

B="http://127.0.0.1:$SRV_PORT"
OUT=$(SKYGET_API_TOKEN="$TOK_A" ./target/debug/skyget --json get "$B/slow/file.bin" \
  -o "$ROOT/dl" --as claude --detach 2>/dev/null)
ID=$(jq -r .id <<<"$OUT")
[ -n "$ID" ] && [ "$ID" != "null" ] || fail "detach 没返回 id：$OUT"

echo "1. B 看不到 A 的任务"
LIST_B=$(curl -s -H "Authorization: Bearer $TOK_B" "$API/jobs?limit=50")
jq -e --arg id "$ID" 'map(select(.id == $id)) | length == 0' <<<"$LIST_B" >/dev/null \
  || fail "B 的 /jobs 看到了 A 的任务：$LIST_B"
ok "/jobs 已按 token 过滤"

echo "2. B 读/跟/取消 A 的任务都失败"
code=$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $TOK_B" \
  "$API/jobs/$ID")
[ "$code" = "404" ] || fail "B GET A 的任务返回 $code，期望 404"
ok "GET 被拒"
if SKYGET_API_TOKEN="$TOK_B" ./target/debug/skyget watch "$ID" --wait-ms 0 >/dev/null 2>&1; then
  fail "B 居然能 watch A 的任务"
fi
ok "watch 被拒"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST \
  -H "Authorization: Bearer $TOK_B" "$API/jobs/$ID/cancel")
[ "$code" = "404" ] || fail "B cancel A 的任务返回 $code，期望 404"
ok "cancel 返回 404"

echo "3. A 仍然能看/控制自己的任务"
code=$(curl -s -o "$ROOT/a.json" -w '%{http_code}' -H "Authorization: Bearer $TOK_A" \
  "$API/jobs/$ID")
[ "$code" = "200" ] || fail "A GET 自己的任务返回 $code"
jq -e --arg id "$ID" '.id == $id' "$ROOT/a.json" >/dev/null \
  || fail "A 的 GET 不对：$(cat "$ROOT/a.json")"
LIST_A=$(curl -s -H "Authorization: Bearer $TOK_A" "$API/jobs?limit=50")
jq -e --arg id "$ID" 'map(select(.id == $id)) | length == 1' <<<"$LIST_A" >/dev/null \
  || fail "A 的 /jobs 看不到自己的任务：$LIST_A"
ok "owner 读写正常"

echo "4. pin 也按 token 隔离"
PIN_SHA=$(printf 'ab%.0s' $(seq 1 32))
SKYGET_API_TOKEN="$TOK_A" ./target/debug/skyget pin "$PIN_SHA" --as claude >/dev/null \
  || fail "A pin 失败"
PINS_B=$(SKYGET_API_TOKEN="$TOK_B" ./target/debug/skyget --json pins 2>/dev/null)
jq -e --arg sha "$PIN_SHA" 'map(select(.sha256 == $sha)) | length == 0' <<<"$PINS_B" >/dev/null \
  || fail "B 看到了 A 的 pin：$PINS_B"
if SKYGET_API_TOKEN="$TOK_B" ./target/debug/skyget unpin "$PIN_SHA" --as pi >/dev/null 2>&1; then
  fail "B 解掉了 A 的 pin"
fi
PINS_A=$(SKYGET_API_TOKEN="$TOK_A" ./target/debug/skyget --json pins 2>/dev/null)
jq -e --arg sha "$PIN_SHA" 'map(select(.sha256 == $sha)) | length == 1' <<<"$PINS_A" >/dev/null \
  || fail "A 看不到自己的 pin：$PINS_A"
SKYGET_API_TOKEN="$TOK_A" ./target/debug/skyget unpin "$PIN_SHA" --as claude >/dev/null \
  || fail "A unpin 失败"
ok "pin 创建/列表/解除都按 token 隔离"

echo "5. /statusline 也按 token 隔离"
OUT2=$(SKYGET_API_TOKEN="$TOK_A" ./target/debug/skyget --json get "$B/slow3/file.bin" \
  -o "$ROOT/dl" --as claude --detach 2>/dev/null)
ID2=$(jq -r .id <<<"$OUT2")
[ -n "$ID2" ] && [ "$ID2" != "null" ] || fail "statusline 测试任务没提交：$OUT2"
sleep 0.3
SA=$(curl -s -H "Authorization: Bearer $TOK_A" "$API/statusline")
jq -e --arg id "$ID2" '.job_id == $id and (.state == "running" or .state == "pending")' \
  <<<"$SA" >/dev/null || fail "A 的 /statusline 没看到自己的任务：$SA"
SB=$(curl -s -H "Authorization: Bearer $TOK_B" "$API/statusline")
jq -e --arg id "$ID2" '.job_id != $id' <<<"$SB" >/dev/null \
  || fail "B 的 /statusline 看到了 A 的任务：$SB"
curl -s -o /dev/null -X POST -H "Authorization: Bearer $TOK_A" "$API/jobs/$ID2/cancel"
ok "A 能看到自己的活跃任务，B 看不到"

echo "6. 幂等键按 token 隔离"
BODY=$(jq -n --arg u "$B/f/file.bin" --arg d "$ROOT/dl/shared" \
  '{url:$u, dest:$d, idempotency_key:"shared-key"}')
submit_job() {
  curl -s -w '\n%{http_code}' -H "Authorization: Bearer $1" \
    -H 'Content-Type: application/json' -d "$BODY" "$API/jobs"
}
RESP_A=$(submit_job "$TOK_A")
CODE_A=${RESP_A##*$'\n'}
JSON_A=${RESP_A%$'\n'*}
IDA=$(jq -r .id <<<"$JSON_A")
[ "$CODE_A" = "202" ] && [ -n "$IDA" ] && [ "$IDA" != "null" ] \
  || fail "A 提交失败：$RESP_A"
RESP_B=$(submit_job "$TOK_B")
CODE_B=${RESP_B##*$'\n'}
JSON_B=${RESP_B%$'\n'*}
IDB=$(jq -r .id <<<"$JSON_B")
[ "$CODE_B" = "202" ] && [ -n "$IDB" ] && [ "$IDB" != "$IDA" ] \
  || fail "B 同 key 居然拿到了 A 的任务：$RESP_A / $RESP_B"
RESP_A2=$(submit_job "$TOK_A")
CODE_A2=${RESP_A2##*$'\n'}
JSON_A2=${RESP_A2%$'\n'*}
[ "$CODE_A2" = "200" ] && [ "$(jq -r .id <<<"$JSON_A2")" = "$IDA" ] \
  || fail "A 自己重试没有命中同一条：$RESP_A2"
curl -s -o /dev/null -X POST -H "Authorization: Bearer $TOK_A" "$API/jobs/$IDA/cancel"
curl -s -o /dev/null -X POST -H "Authorization: Bearer $TOK_B" "$API/jobs/$IDB/cancel"
ok "不同 token 同 key 不 dedupe，同 token 重试仍命中"

echo "7. 鉴权失败计入 /metrics"
code=$(curl -s -o /dev/null -w '%{http_code}' "$API/jobs")
[ "$code" = "401" ] || fail "无 token 的 /jobs 返回 $code，期望 401"
M=$(curl -s -H "Authorization: Bearer $TOK_A" "$API/metrics")
N=$(awk -F'[{} ]+' '$1=="skyget_denied_total" && $2=="kind=\"auth\"" {print $3}' <<<"$M")
[ "${N:-0}" -ge 1 ] || fail "auth 拒绝计数没增加：$(grep skyget_denied <<<"$M")"
ok "skyget_denied_total{kind=\"auth\"} 已计数"

# 收尾：A 自己取消，避免留下后台任务
curl -s -o /dev/null -X POST -H "Authorization: Bearer $TOK_A" "$API/jobs/$ID/cancel"

echo "8. 配额没有 token 身份绑定 → 拒绝启动"
BAD_PORT=$((39400 + $$ % 200))
timeout 5 env SKYGET_HOME="$ROOT/home-bad" SKYGET_AGENT_QUOTAS="claude=1000" \
  ./target/debug/skyget serve --port "$BAD_PORT" >"$ROOT/bad.log" 2>&1
code=$?
[ "$code" = "124" ] && fail "配额没有注册表时 daemon 竟然起来了（被 timeout 杀掉）"
[ "$code" -ne 0 ] || fail "配额没有注册表时 daemon 退出码是 0"
grep -q "SKYGET_AGENT_QUOTAS" "$ROOT/bad.log" || fail "报错里没提 SKYGET_AGENT_QUOTAS：$(cat "$ROOT/bad.log")"
ok "配额无注册表被拒绝启动"

echo "全部通过"
