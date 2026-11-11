#!/usr/bin/env bash
# 每日配额验收：SKYGET_AGENT_QUOTAS="claude=1MB"。
# 已用 400k + 400k → 800k；1.5MB 被拒；100k + 100k 到 1MB；
# 再来 1KB 必须被拒，且失败任务不落盘。
set -u

cd "$(dirname "$0")/.." || exit 1
ROOT="$(mktemp -d /tmp/skyget-quota.XXXXXX)"
SRV_PORT=$((34400 + $$ % 300))
API_PORT=$((35400 + $$ % 300))
UP_PORT=$((36400 + $$ % 300))
SRV_PID=""
UP_PID=""
DAEMON_PID=""

cleanup() {
  [ -n "$DAEMON_PID" ] && kill "$DAEMON_PID" 2>/dev/null
  [ -n "$SRV_PID" ] && kill "$SRV_PID" 2>/dev/null
  [ -n "$UP_PID" ] && kill "$UP_PID" 2>/dev/null
  wait 2>/dev/null
  [ "${KEEP:-0}" = "1" ] && echo "保留现场：$ROOT" || rm -rf "$ROOT"
}
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; cat "$ROOT/daemon.log" >&2 2>/dev/null; exit 1; }
ok() { echo "  ✓ $*"; }

mkdir -p "$ROOT/srv" "$ROOT/home" "$ROOT/src" "$ROOT/up"
for spec in "small1 400000" "small2 400000" "big 1500000" "tiny1 100000" "tiny2 100000" "tiny3 1000"; do
  set -- $spec
  head -c "$2" /dev/urandom >"$ROOT/srv/$1.bin"
done
head -c 1000 /dev/urandom >"$ROOT/src/up.bin"

python3 scripts/testserver.py "$SRV_PORT" "$ROOT/srv" >"$ROOT/srv.log" 2>&1 &
SRV_PID=$!
python3 scripts/upload-server.py "$ROOT/up" "$UP_PORT" >"$ROOT/up.log" 2>&1 &
UP_PID=$!

SKYGET_HOME="$ROOT/home" SKYGET_ALLOW_PRIVATE=1 SKYGET_AGENT_QUOTAS="claude=1MB" \
  SKYGET_AGENTS="tok_quota=claude" \
  SKYGET_UPLOAD_ROOTS="$ROOT/src" \
  ./target/debug/skyget serve --port "$API_PORT" >"$ROOT/daemon.log" 2>&1 &
DAEMON_PID=$!
API="http://127.0.0.1:$API_PORT"
for _ in $(seq 1 60); do
  curl -sf "$API/health" >/dev/null 2>&1 && break
  sleep 0.1
done
curl -sf "$API/health" >/dev/null || fail "daemon 没起来"

B="http://127.0.0.1:$SRV_PORT"
run_ok() {
  local file=$1 dir=$2 name=$3 out code
  out=$(SKYGET_API="$API" SKYGET_API_TOKEN="tok_quota" ./target/debug/skyget --json get "$B/f/$file" \
    -o "$ROOT/$dir" --as claude --daemon 2>/dev/null)
  code=$?
  [ "$code" -eq 0 ] || fail "$name 应该成功（exit $code）：$out"
  jq -e '.state == "done"' <<<"$out" >/dev/null || fail "$name 不是 done：$out"
  [ -f "$ROOT/$dir/$file" ] || fail "$name 没落盘"
  ok "$name 通过"
}
run_quota() {
  local file=$1 dir=$2 name=$3 out code err
  out=$(SKYGET_API="$API" SKYGET_API_TOKEN="tok_quota" ./target/debug/skyget --json get "$B/f/$file" \
    -o "$ROOT/$dir" --as claude --daemon 2>/dev/null)
  code=$?
  [ "$code" -ne 0 ] || fail "$name 应该被配额拒绝：$out"
  err=$(jq -r '.error_code // ""' <<<"$out" 2>/dev/null)
  [ "$err" = "quota.exceeded" ] || fail "$name 不是 quota.exceeded：$out"
  [ ! -f "$ROOT/$dir/$file" ] || fail "$name 被拒却落盘"
  ok "$name 被拒（quota.exceeded）"
}

echo "1. 先下 400k + 400k"
run_ok small1.bin dl1 small1
run_ok small2.bin dl2 small2

echo "2. 1.5MB 超过剩余额度"
run_quota big.bin dl3 big

echo "3. 100k + 100k 正好到 1MB"
run_ok tiny1.bin dl4 tiny1
run_ok tiny2.bin dl5 tiny2

echo "4. 再 1KB 必须被拒"
run_quota tiny3.bin dl6 tiny3

echo "5. /agents 显示配额和今日用量"
agents=$(curl -sf -H "Authorization: Bearer tok_quota" "$API/agents")
jq -e '.agents[] | select(.name=="claude") | .quota_bytes == 1000000 and .used_today > 0' \
  <<<"$agents" >/dev/null || fail "agents 没显示配额/用量：$agents"
ok "agents 显示 quota_bytes / used_today"

echo "6. /metrics 暴露配额"
metrics=$(curl -sf -H "Authorization: Bearer tok_quota" "$API/metrics")
grep -q 'skyget_agent_quota_bytes{agent="claude"} 1000000' <<<"$metrics" \
  || fail "metrics 缺 quota：$metrics"
grep -q 'skyget_agent_used_today_bytes{agent="claude"}' <<<"$metrics" \
  || fail "metrics 缺 used_today：$metrics"
ok "metrics 暴露 quota / used_today"

echo "7. 上传也受同一个日额度限制"
out=$(SKYGET_API="$API" SKYGET_API_TOKEN="tok_quota" ./target/debug/skyget --json upload "$ROOT/src/up.bin" \
  "http://127.0.0.1:$UP_PORT/put/up.bin" --as claude 2>/dev/null)
code=$?
[ "$code" -ne 0 ] || fail "超额上传居然成功：$out"
err=$(jq -r '.error_code // ""' <<<"$out" 2>/dev/null)
[ "$err" = "quota.exceeded" ] || fail "上传错误不是 quota.exceeded：$out"
[ ! -f "$ROOT/up/up.bin" ] || fail "超额上传却到达服务器"
ok "上传被配额拒绝，服务器没有收到文件"

echo "全部通过"
