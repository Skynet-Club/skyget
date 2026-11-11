#!/usr/bin/env bash
# 下载凭证验收：Authorization: Bearer 只从白名单 env 取，值不落台账。
#
# 用法：cargo build --bin skyget，再 scripts/download-credential-e2e.sh
set -u

cd "$(dirname "$0")/.." || exit 1
ROOT="$(mktemp -d /tmp/skyget-cred.XXXXXX)"
SRV_PORT=$((32400 + $$ % 200))
API_PORT=$((33400 + $$ % 200))
API2_PORT=$((40400 + $$ % 200))
SRV_PID=""
DAEMON_PID=""
DAEMON2_PID=""

cleanup() {
  [ -n "$DAEMON_PID" ] && kill "$DAEMON_PID" 2>/dev/null
  [ -n "$DAEMON2_PID" ] && kill "$DAEMON2_PID" 2>/dev/null
  [ -n "$SRV_PID" ] && kill "$SRV_PID" 2>/dev/null
  wait 2>/dev/null
  [ "${KEEP:-0}" = "1" ] && echo "保留现场：$ROOT" || rm -rf "$ROOT"
}
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; cat "$ROOT/daemon.log" >&2 2>/dev/null; exit 1; }
ok() { echo "  ✓ $*"; }

mkdir -p "$ROOT/srv" "$ROOT/home" "$ROOT/home2" "$ROOT/dl-ok" "$ROOT/dl-noauth" "$ROOT/dl-denied"
head -c 1048576 /dev/urandom >"$ROOT/srv/auth.bin"
SHA=$(sha256sum "$ROOT/srv/auth.bin" | cut -d' ' -f1)

python3 scripts/testserver.py "$SRV_PORT" "$ROOT/srv" >"$ROOT/srv.log" 2>&1 &
SRV_PID=$!

SKYGET_HOME="$ROOT/home" SKYGET_ALLOW_PRIVATE=1 SKYGET_ALLOWED_HOSTS=127.0.0.1 \
SKYGET_DOWNLOAD_CREDENTIALS=TEST_TOKEN TEST_TOKEN=test-secret \
  ./target/debug/skyget serve --port "$API_PORT" >"$ROOT/daemon.log" 2>&1 &
DAEMON_PID=$!
# 第二个 daemon：没有 host 白名单，带了凭证的提交必须在入队前被拒。
SKYGET_HOME="$ROOT/home2" SKYGET_ALLOW_PRIVATE=1 \
SKYGET_DOWNLOAD_CREDENTIALS=TEST_TOKEN TEST_TOKEN=test-secret \
  ./target/debug/skyget serve --port "$API2_PORT" >"$ROOT/daemon2.log" 2>&1 &
DAEMON2_PID=$!
API="http://127.0.0.1:$API_PORT"
API2="http://127.0.0.1:$API2_PORT"
for _ in $(seq 1 60); do
  curl -sf "$API/health" >/dev/null 2>&1 && break
  sleep 0.1
done
curl -sf "$API/health" >/dev/null || fail "daemon 没起来"
for _ in $(seq 1 60); do
  curl -sf "$API2/health" >/dev/null 2>&1 && break
  sleep 0.1
done
curl -sf "$API2/health" >/dev/null || fail "daemon2 没起来"

B="http://127.0.0.1:$SRV_PORT"

echo "1. 带凭证下载"
out=$(SKYGET_API="$API" ./target/debug/skyget --json get "$B/auth/auth.bin" \
  -o "$ROOT/dl-ok" --sha256 "$SHA" --as claude --daemon \
  --credential-ref env:TEST_TOKEN 2>/dev/null)
code=$?
[ "$code" -eq 0 ] || fail "带凭证下载失败（exit $code）：$out"
jq -e '.state == "done"' <<<"$out" >/dev/null || fail "结果不是 done：$out"
actual=$(sha256sum "$ROOT/dl-ok/auth.bin" 2>/dev/null | cut -d' ' -f1)
[ "$actual" = "$SHA" ] || fail "sha 不符：$actual"
ok "Bearer 凭证生效，文件 sha 正确"

echo "2. 不带凭证应 401"
out=$(SKYGET_API="$API" ./target/debug/skyget --json get "$B/auth/auth.bin" \
  -o "$ROOT/dl-noauth" --as claude --daemon 2>/dev/null)
code=$?
[ "$code" -ne 0 ] || fail "无凭证居然成功：$out"
err=$(jq -r '.error // ""' <<<"$out" 2>/dev/null)
case "$err" in
  *401* | *需要登录*) ok "无凭证失败：$err" ;;
  *) fail "错误不是 401：$out" ;;
esac
[ ! -f "$ROOT/dl-noauth/auth.bin" ] || fail "无凭证却落盘"

echo "3. 不在白名单的 env 被拒"
out=$(SKYGET_API="$API" ./target/debug/skyget --json get "$B/auth/auth.bin" \
  -o "$ROOT/dl-denied" --as claude --daemon \
  --credential-ref env:SECRET_NOT_ALLOWED 2>/dev/null)
code=$?
[ "$code" -ne 0 ] || fail "未授权 env 居然成功：$out"
err=$(jq -r '.error // ""' <<<"$out" 2>/dev/null)
case "$err" in
  *白名单* | *credential_denied* | *凭证*) ok "白名单生效：$err" ;;
  *) fail "错误不是 credential_denied：$out" ;;
esac

echo "4. 台账不出现凭证值"
ids=$(SKYGET_API="$API" ./target/debug/skyget --json list |
  jq -r '.[] | select(.url | contains("/auth/")) | .id')
for id in $ids; do
  body=$(SKYGET_API="$API" ./target/debug/skyget --json show "$id")
  case "$body" in
    *test-secret*) fail "台账泄露了凭证值：$body" ;;
  esac
done
ok "台账只有引用，没有凭证值"

echo "5. 没有 host 白名单时，凭证提交在入队前被拒"
REQ=$(jq -nc --arg u "$B/auth/auth.bin" --arg d "$ROOT/dl-needs-hosts/" \
  '{url:$u, source:"agent:claude", dest:$d, credential_ref:"env:TEST_TOKEN"}')
code=$(curl -s -o "$ROOT/reject.json" -w '%{http_code}' -X POST \
  -H 'Content-Type: application/json' -d "$REQ" "$API2/jobs")
[ "$code" = "403" ] || fail "无白名单的凭证提交返回 $code，期望 403：$(cat "$ROOT/reject.json")"
jq -e '.code == "policy.credential_needs_hosts"' "$ROOT/reject.json" >/dev/null \
  || fail "错误码不对：$(cat "$ROOT/reject.json")"
ok "policy.credential_needs_hosts"

echo "全部通过"
