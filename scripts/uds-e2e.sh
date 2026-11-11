#!/usr/bin/env bash
# Unix socket 身份：不用分发 token，内核告诉你「谁在调我」。
#
# 验四件事：
# 1. socket 真的建出来了，权限是 0660；
# 2. 通过 socket 调 `/health` 不需要 token；
# 3. 在 `SKYGET_UDS_AGENTS` 里登记过的 uid，任务归属就是那个 agent（没有 token）；
# 4. 同一条 daemon 的 TCP 路径行为不变（有 registry 时仍然要 token）。
set -u

# 公共样板都在 scripts/lib.sh（端口/临时目录/ok-no/trap/等 health）。
source "$(dirname "$0")/lib.sh"
e2e_init uds
SOCK="$HOME_DIR/daemon.sock"

mkdir -p "$SRC"
head -c 262144 /dev/urandom > "$SRC/small.bin"
start_server "$SRC" || exit 1

# 打开 per-agent token 注册表：这样 TCP 这条路必须带 token，
# 而 socket 这条靠 uid，不该需要 token。
ME=$(id -u)
SKYGET_AGENTS="tok_claude=claude" \
SKYGET_UDS_AGENTS="$ME=claude" \
    start_daemon "$D_PORT" || exit 1

echo "── 1. socket 建出来了，权限是 0660 ──"
if [ -S "$SOCK" ]; then
    ok "socket 在 $SOCK"
else
    no "socket 没建出来"
fi
mode=$(stat -c '%a' "$SOCK" 2>/dev/null || stat -f '%Lp' "$SOCK" 2>/dev/null)
if [ "$mode" = "660" ]; then
    ok "权限 0660（组决定谁能连）"
else
    no "权限是 $mode，期望 660"
fi

echo "── 2. socket 上 /health 不需要 token ──"
code=$(curl -s -o /tmp/uds-health.json -w '%{http_code}' --unix-socket "$SOCK" http://localhost/health)
if [ "$code" = "200" ] && grep -q '"ok":true' /tmp/uds-health.json; then
    ok "GET /health → 200"
else
    no "GET /health → $code：$(cat /tmp/uds-health.json)"
fi

echo "── 3. socket 上提交任务：uid 映射成 agent:claude，没有 token ──"
resp=$(curl -s -X POST --unix-socket "$SOCK" http://localhost/jobs \
    -H 'Content-Type: application/json' \
    -d "{\"url\":\"http://127.0.0.1:$PORT/f/small.bin\",\"dest\":\"$DL\"}")
jid=$(printf '%s' "$resp" | python3 -c 'import json,sys
try: print(json.load(sys.stdin)["id"])
except Exception: print("")')
if [ -n "$jid" ]; then
    ok "任务提交成功（$jid）"
else
    no "任务提交失败：$resp"
fi
src=$(curl -s --unix-socket "$SOCK" "http://localhost/jobs/$jid" | python3 -c 'import json,sys
try: print(json.load(sys.stdin)["source"])
except Exception: print("")')
if [ "$src" = "agent:claude" ]; then
    ok "归属是 agent:claude（来自 uid，不是请求体自称）"
else
    no "归属是「$src」，期望 agent:claude"
fi
# 等它下完，确认真的落地了
for _ in $(seq 1 100); do
    st=$(curl -s --unix-socket "$SOCK" "http://localhost/jobs/$jid" | python3 -c 'import json,sys
try: print(json.load(sys.stdin)["state"])
except Exception: print("")')
    [ "$st" = "done" ] && break
    sleep 0.2
done
if [ "$st" = "done" ] && [ -f "$DL/small.bin" ]; then
    ok "任务下完并落地"
else
    no "任务没下完（state=$st）"
fi

echo "── 4. 同一条 daemon 的 TCP 路径没变：有 registry 就必须带 token ──"
code=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$D_PORT/jobs")
if [ "$code" = "401" ]; then
    ok "TCP 不带 token → 401（旧行为不变）"
else
    no "TCP 不带 token → $code，期望 401"
fi

echo "── 5. CLI 用 SKYGET_API=unix:… 直接干活 ──"
SKYGET_API="unix:$SOCK" "$BIN" --json get "http://127.0.0.1:$PORT/f/small.bin" -o "$DL" \
    --name cli.bin --daemon --as claude >/tmp/uds-cli.json 2>/dev/null
if [ -f "$DL/cli.bin" ] && grep -q '"state":"done"' /tmp/uds-cli.json; then
    ok "CLI 经 socket 下完（$DL/cli.bin）"
else
    no "CLI 经 socket 失败：$(tail -2 /tmp/uds-cli.json)"
fi

summary
