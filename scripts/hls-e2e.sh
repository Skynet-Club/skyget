#!/usr/bin/env bash
# HLS 编排回归（不需要真 ffmpeg）：协议白名单必须传给 ffmpeg；受限出网默认拒绝。
set -u

cd "$(dirname "$0")/.." || exit 1
ROOT="$(mktemp -d /tmp/skyget-hls.XXXXXX)"
PORT=$((38200 + $$ % 150))
SRV_PID=""

cleanup() {
  [ -n "$SRV_PID" ] && kill "$SRV_PID" 2>/dev/null
  wait 2>/dev/null
  [ "${KEEP:-0}" = "1" ] && echo "保留现场：$ROOT" || rm -rf "$ROOT"
}
trap cleanup EXIT
fail() { echo "FAIL: $*" >&2; exit 1; }
ok() { echo "  ✓ $*"; }

mkdir -p "$ROOT/srv" "$ROOT/home" "$ROOT/dl"
cat >"$ROOT/srv/stream.m3u8" <<'EOF'
#EXTM3U
#EXT-X-TARGETDURATION:2
#EXTINF:1.0,
seg1.ts
#EXTINF:1.0,
seg2.ts
#EXT-X-ENDLIST
EOF
python3 scripts/testserver.py "$PORT" "$ROOT/srv" >"$ROOT/srv.log" 2>&1 &
SRV_PID=$!
bash scripts/wait-http.sh "http://127.0.0.1:$PORT/" || fail "测试服务器没起来"

echo "1. 开放出网策略：HLS 走 fake-ffmpeg 落地，参数带协议白名单"
OUT=$(SKYGET_HOME="$ROOT/home" SKYGET_ALLOW_PRIVATE=1 \
  SKYGET_FFMPEG="$PWD/scripts/fake-ffmpeg.sh" \
  FAKE_FFMPEG_LOG="$ROOT/args.log" \
  ./target/debug/skyget --json get "http://127.0.0.1:$PORT/f/stream.m3u8" \
  --to "$ROOT/dl" 2>"$ROOT/get.err")
CODE=$?
[ "$CODE" = 0 ] || fail "HLS 下载失败：$OUT $(cat "$ROOT/get.err")"
[ "$(jq -r .state <<<"$OUT")" = "done" ] || fail "HLS 状态不是 done：$OUT"
grep -q '^protocol_whitelist=http,https,tls,tcp$' "$ROOT/args.log" \
  || fail "ffmpeg 没拿到协议白名单：$(cat "$ROOT/args.log")"
OUTFILE=$(find "$ROOT/dl" -name 'stream.mp4' -type f | head -n 1)
[ -n "$OUTFILE" ] || fail "没有落地 stream.mp4"
grep -q '^fake-mp4' "$OUTFILE" || fail "落地文件不是 ffmpeg 的产物"
ok "落地成功且协议白名单正确"

echo "2. 严格出网：HLS 默认拒绝（不启动 ffmpeg）"
OUT2=$(SKYGET_HOME="$ROOT/home" SKYGET_EGRESS=strict \
  SKYGET_FFMPEG="$PWD/scripts/fake-ffmpeg.sh" \
  FAKE_FFMPEG_LOG="$ROOT/args2.log" \
  ./target/debug/skyget --json get "https://1.1.1.1/stream.m3u8" \
  --to "$ROOT/dl2" 2>"$ROOT/get2.err")
CODE2=$?
[ "$CODE2" != 0 ] || fail "严格模式居然接受了 HLS：$OUT2"
[ "$(jq -r .error_code <<<"$OUT2")" = "policy.stream_egress" ] \
  || fail "错误码不对：$OUT2"
[ ! -e "$ROOT/args2.log" ] || fail "被拒绝的任务仍然启动了 ffmpeg"
ok "严格模式拒绝且未启动 ffmpeg"

echo "3. 有 host 白名单但没 opt-in：HLS 仍拒绝"
OUT3=$(SKYGET_HOME="$ROOT/home" SKYGET_ALLOW_PRIVATE=1 \
  SKYGET_ALLOWED_HOSTS=127.0.0.1 SKYGET_FFMPEG="$PWD/scripts/fake-ffmpeg.sh" \
  FAKE_FFMPEG_LOG="$ROOT/args3.log" \
  ./target/debug/skyget --json get "http://127.0.0.1:$PORT/f/stream.m3u8" \
  --to "$ROOT/dl3" 2>"$ROOT/get3.err")
CODE3=$?
[ "$CODE3" != 0 ] || fail "有白名单却接受了 HLS：$OUT3"
[ "$(jq -r .error_code <<<"$OUT3")" = "policy.stream_egress" ] \
  || fail "白名单拒绝错误码不对：$OUT3"
[ ! -e "$ROOT/args3.log" ] || fail "白名单拒绝时仍启动了 ffmpeg"
ok "host 白名单下默认拒绝"

echo "4. 显式 opt-in：本地 HLS 可以跑"
OUT4=$(SKYGET_HOME="$ROOT/home" SKYGET_ALLOW_PRIVATE=1 \
  SKYGET_ALLOWED_HOSTS=127.0.0.1 SKYGET_ALLOW_STREAM_EGRESS=1 \
  SKYGET_FFMPEG="$PWD/scripts/fake-ffmpeg.sh" \
  FAKE_FFMPEG_LOG="$ROOT/args4.log" \
  ./target/debug/skyget --json get "http://127.0.0.1:$PORT/f/stream.m3u8" \
  --to "$ROOT/dl4" 2>"$ROOT/get4.err")
CODE4=$?
[ "$CODE4" = 0 ] || fail "opt-in 后 HLS 仍失败：$OUT4 $(cat "$ROOT/get4.err")"
[ "$(jq -r .state <<<"$OUT4")" = "done" ] || fail "opt-in 后状态不是 done：$OUT4"
grep -q '^protocol_whitelist=http,https,tls,tcp$' "$ROOT/args4.log" \
  || fail "opt-in 后 ffmpeg 没拿到协议白名单"
ok "opt-in 放行且参数正确"

echo "全部通过"
