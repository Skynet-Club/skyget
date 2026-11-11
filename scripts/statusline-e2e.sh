#!/usr/bin/env bash
# 状态栏验收：真实 daemon + 慢任务，验证 CLI / HTTP 两条路径与来源过滤。
set -u

cd "$(dirname "$0")/.." || exit 1
ROOT="$(mktemp -d /tmp/skyget-statusline.XXXXXX)"
SRV_PORT=$((35400 + $$ % 150))
API_PORT=$((36400 + $$ % 150))
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
head -c 8388608 /dev/urandom >"$ROOT/srv/slow.bin"
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
OUT=$(./target/debug/skyget --json get "$B/slow/slow.bin" --to "$ROOT/dl" \
  --as claude --detach 2>/dev/null)
ID=$(jq -r .id <<<"$OUT")
[ -n "$ID" ] && [ "$ID" != "null" ] || fail "提交失败：$OUT"

echo "1. CLI statusline 跟随活跃任务"
for _ in $(seq 1 30); do
  JSON=$(./target/debug/skyget statusline --format json 2>/dev/null)
  [ -n "$JSON" ] && break
  sleep 0.1
done
jq -e --arg id "$ID" '.job_id == $id and (.state == "running" or .state == "pending")' \
  <<<"$JSON" >/dev/null || fail "statusline 没选中任务：$JSON"
ok "JSON 选中 $ID"

ANSI=$(./target/debug/skyget statusline 2>/dev/null)
case "$ANSI" in *Skygeting*) ok "ANSI 有标签" ;; *) fail "ANSI 无标签：$ANSI" ;; esac
PLAIN=$(./target/debug/skyget statusline --format plain 2>/dev/null)
case "$PLAIN" in *%) ok "plain 有百分比" ;; *) fail "plain 无百分比：$PLAIN" ;; esac

echo "2. 来源过滤标签"
FILTERED=$(./target/debug/skyget statusline --format plain --source agent:claude 2>/dev/null)
case "$FILTERED" in "Skygeting[claude]: "*) ok "过滤标签正确" ;; *) fail "过滤标签不对：$FILTERED" ;; esac
OTHER=$(./target/debug/skyget statusline --format json --source agent:pi 2>/dev/null)
[ -z "$OTHER" ] || fail "Pi 过滤居然看到 Claude 的任务：$OTHER"
ok "其他来源看不到"

echo "3. HTTP /statusline 与 /seconds"
S=$(curl -s "$API/statusline")
jq -e --arg id "$ID" '.job_id == $id' <<<"$S" >/dev/null || fail "/statusline 不对：$S"
ok "/statusline 一致"
SEC=$(curl -s "$API/jobs/$ID/seconds")
jq -e '.seconds_total >= 0 and .pct >= 0' <<<"$SEC" >/dev/null || fail "/seconds 不对：$SEC"
ok "/seconds 可读"

echo "4. 取消后空闲"
curl -s -o /dev/null -X POST "$API/jobs/$ID/cancel"
sleep 0.4
IDLE=$(./target/debug/skyget statusline --format json 2>/dev/null)
[ -z "$IDLE" ] || fail "取消后 statusline 还输出：$IDLE"
ok "空闲时不输出"

echo "全部通过"
