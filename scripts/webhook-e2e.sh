#!/usr/bin/env bash
# 终态 webhook 验收：done / failed 各投递一次，payload 不含 query token。
set -u

cd "$(dirname "$0")/.." || exit 1
ROOT="$(mktemp -d /tmp/skyget-hook.XXXXXX)"
SRV_PORT=$((39400 + $$ % 150))
API_PORT=$((39600 + $$ % 150))
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
head -c 200000 /dev/urandom >"$ROOT/srv/file.bin"
head -c 4194304 /dev/urandom >"$ROOT/srv/slow.bin"

python3 scripts/testserver.py "$SRV_PORT" "$ROOT/srv" >"$ROOT/srv.log" 2>&1 &
SRV_PID=$!

B="http://127.0.0.1:$SRV_PORT"
SKYGET_HOME="$ROOT/home" SKYGET_ALLOW_PRIVATE=1 \
SKYGET_COMPLETION_WEBHOOK="$B/hook" SKYGET_WEBHOOK_SECRET="test-secret" \
  ./target/debug/skyget serve --port "$API_PORT" >"$ROOT/daemon.log" 2>&1 &
DAEMON_PID=$!
API="http://127.0.0.1:$API_PORT"
export SKYGET_API="$API"
for _ in $(seq 1 60); do
  curl -sf "$API/health" >/dev/null 2>&1 && break
  sleep 0.1
done
curl -sf "$API/health" >/dev/null || fail "daemon 没起来"

wait_hook() {
  for _ in $(seq 1 50); do
    if [ -s "$ROOT/srv/hook.json" ] && jq empty "$ROOT/srv/hook.json" 2>/dev/null; then
      return 0
    fi
    sleep 0.1
  done
  return 1
}

echo "1. 成功任务发 webhook"
OUT=$(./target/debug/skyget --json get "$B/f/file.bin?token=should-not-leak" \
  -o "$ROOT/dl" --as claude 2>/dev/null)
ID=$(jq -r .id <<<"$OUT")
[ -n "$ID" ] && [ "$ID" != "null" ] || fail "下载没返回 id：$OUT"
wait_hook || fail "done 的 webhook 没到"
jq -e --arg id "$ID" '.id == $id and .state == "done"' "$ROOT/srv/hook.json" >/dev/null \
  || fail "done payload 不对：$(cat "$ROOT/srv/hook.json")"
EXPECT_SIG=$(python3 -c 'import hmac,hashlib,sys;print("sha256="+hmac.new(b"test-secret",open(sys.argv[1],"rb").read(),hashlib.sha256).hexdigest())' "$ROOT/srv/hook.json")
GOT_SIG=$(cat "$ROOT/srv/hook.headers" 2>/dev/null)
[ "$EXPECT_SIG" = "$GOT_SIG" ] || fail "webhook HMAC 不符：$GOT_SIG != $EXPECT_SIG"
grep -q 'should-not-leak' "$ROOT/srv/hook.json" && fail "webhook 泄露了 URL query"
grep -qi 'credential\|bearer' "$ROOT/srv/hook.json" && fail "webhook 泄露了凭证字段"
ok "done payload 正确，query/凭证没有外发"
mv "$ROOT/srv/hook.json" "$ROOT/srv/hook-done.json"

echo "2. 失败任务发 webhook"
OUT=$(./target/debug/skyget --json get "$B/deny" -o "$ROOT/dl" --as claude 2>/dev/null)
CODE=$?
[ "$CODE" -ne 0 ] || fail "403 任务居然成功：$OUT"
wait_hook || fail "failed 的 webhook 没到"
jq -e '.state == "failed" and (.error_code | length > 0)' "$ROOT/srv/hook.json" >/dev/null \
  || fail "failed payload 不对：$(cat "$ROOT/srv/hook.json")"
ok "failed payload 正确"
mv "$ROOT/srv/hook.json" "$ROOT/srv/hook-failed.json"

echo "2b. 传输错误的 URL query 不能从 error 文本泄露"
# 连接被拒时 reqwest 的错误会带完整 URL；webhook 的 error 字段必须脱敏。
OUT=$(./target/debug/skyget --json get "http://127.0.0.1:1/x?token=should-not-leak" \
  -o "$ROOT/dl" --as claude 2>/dev/null)
CODE=$?
[ "$CODE" -ne 0 ] || fail "连不上的任务居然成功：$OUT"
wait_hook || fail "传输失败任务的 webhook 没到"
jq -e '.state == "failed"' "$ROOT/srv/hook.json" >/dev/null \
  || fail "传输失败 payload 不对：$(cat "$ROOT/srv/hook.json")"
grep -q 'should-not-leak' "$ROOT/srv/hook.json" \
  && fail "webhook 的 error 字段泄露了 URL query"
ok "error 文本已脱敏"
mv "$ROOT/srv/hook.json" "$ROOT/srv/hook-transport-failed.json"

echo "3. 取消任务发 webhook"
OUT=$(./target/debug/skyget --json get "$B/slow/slow.bin" -o "$ROOT/dl" \
  --as claude --detach 2>/dev/null)
ID=$(jq -r .id <<<"$OUT")
[ -n "$ID" ] && [ "$ID" != "null" ] || fail "detach 没返回 id：$OUT"
curl -s -o /dev/null -X POST "$API/jobs/$ID/cancel"
wait_hook || fail "cancelled 的 webhook 没到"
jq -e --arg id "$ID" '.id == $id and .state == "cancelled"' "$ROOT/srv/hook.json" >/dev/null \
  || fail "cancelled payload 不对：$(cat "$ROOT/srv/hook.json")"
ok "cancelled payload 正确"

echo "全部通过"
