#!/usr/bin/env bash
# daemon 生命周期：按需拉起、状态可读、直接进程可停、残留 pid/socket 可收尾、空闲退出。
set -u

source "$(dirname "$0")/lib.sh"
e2e_init daemon-lifecycle
if [ -z "${D_PORT:-}" ]; then
    echo "当前沙箱不允许绑定回环端口，无法运行生命周期 e2e" >&2
    exit 1
fi

# e2e 用调试二进制，CI 不需要为了这条用例再编一份 daemon 的 release。
BIN=./target/debug/skyget
DPID=./target/debug/skynet-dataplaned
[ -x "$BIN" ] || { echo "缺 $BIN；先 cargo build --bin skyget --bin skynet-dataplaned" >&2; exit 1; }
[ -x "$DPID" ] || { echo "缺 $DPID；先 cargo build --bin skyget --bin skynet-dataplaned" >&2; exit 1; }

export SKYGET_AUTOSTART=always
unset SKYGET_API_TOKEN
# CI 和开发机可能都装了用户服务。生命周期用例只验「直接拉起」这一档，
# 用假的 systemctl 把环境固定住，避免测试顺手启动 7788 上的生产服务。
mkdir -p "$HOME_DIR/bin"
printf '#!/bin/sh\nexit 1\n' >"$HOME_DIR/bin/systemctl"
chmod +x "$HOME_DIR/bin/systemctl"
export PATH="$HOME_DIR/bin:$PATH"

if [ "$(uname -s)" = Linux ]; then
    PID_FILE="/run/user/$(id -u)/skynet-downloadd-$D_PORT.pid"
else
    PID_FILE="$HOME_DIR/skynet-downloadd-$D_PORT.pid"
fi
mkdir -p "$(dirname "$PID_FILE")"
SOCK="$HOME_DIR/daemon.sock"

echo "── 1. 残留 pid/socket 不妨碍按需拉起 ──"
printf '99999999\n' >"$PID_FILE"
python3 - "$SOCK" <<'PY'
import socket, sys
s = socket.socket(socket.AF_UNIX)
s.bind(sys.argv[1])
s.close()
PY
"$BIN" --json agents >"$HOME_DIR/agents.json" 2>"$HOME_DIR/agents.err"
if [ $? -eq 0 ] && wait_health "$D_PORT"; then
    ok "客户端调用会拉起 daemon，并替换失效 socket"
else
    no "按需拉起失败：$(cat "$HOME_DIR/agents.err" 2>/dev/null)"
fi

echo "── 2. status 同时报告健康、pid 和 socket 可用性 ──"
"$BIN" --json daemon status >"$HOME_DIR/status.json" 2>/dev/null
if python3 - "$HOME_DIR/status.json" <<'PY'
import json, sys
v = json.load(open(sys.argv[1]))
raise SystemExit(0 if v["healthy"] and v["pid_running"] and v["pid"] and v["socket_live"] and not v["socket_stale"] else 1)
PY
then
    ok "status 的 pid_running / socket_live 与实际一致"
else
    no "status 不对：$(cat "$HOME_DIR/status.json")"
fi

echo "── 3. stop 能停直接拉起的进程并清理文件 ──"
DAEMON_PID=$(python3 - "$HOME_DIR/status.json" <<'PY'
import json, sys
print(json.load(open(sys.argv[1]))["pid"])
PY
)
"$BIN" --json daemon stop >"$HOME_DIR/stop.json" 2>/dev/null
if python3 - "$HOME_DIR/stop.json" <<'PY'
import json, sys
v = json.load(open(sys.argv[1]))
raise SystemExit(0 if v["stopped"] and v["by"] == "pid" else 1)
PY
then
    ok "daemon stop 停掉了直接进程 pid $DAEMON_PID"
else
    no "daemon stop 没停掉直接进程：$(cat "$HOME_DIR/stop.json")"
fi
if ! wait_health "$D_PORT" && [ ! -e "$PID_FILE" ] && [ ! -e "$SOCK" ]; then
    ok "health 已下线，pid/socket 都已清理"
else
    no "停止后仍有残留（pid=$([ -e "$PID_FILE" ] && echo yes || echo no), socket=$([ -e "$SOCK" ] && echo yes || echo no)）"
fi

echo "── 4. stop 幂等，并且能单独收拾死 pid/失效 socket ──"
printf '99999999\n' >"$PID_FILE"
python3 - "$SOCK" <<'PY'
import socket, sys
s = socket.socket(socket.AF_UNIX)
s.bind(sys.argv[1])
s.close()
PY
"$BIN" --json daemon stop >"$HOME_DIR/stop2.json" 2>/dev/null
if python3 - "$HOME_DIR/stop2.json" <<'PY' && [ ! -e "$PID_FILE" ] && [ ! -e "$SOCK" ]
import json, sys
v = json.load(open(sys.argv[1]))
raise SystemExit(0 if v["cleaned"] else 1)
PY
then
    ok "重复 stop 可安全执行，并清理死 pid/失效 socket"
else
    no "幂等 stop/残留清理失败：$(cat "$HOME_DIR/stop2.json")"
fi

echo "── 5. SKYGET_IDLE_EXIT 到点后不留现场 ──"
SKYGET_IDLE_EXIT=1 "$BIN" --json agents >"$HOME_DIR/idle-agents.json" 2>/dev/null
# idle 线程每 5 秒检查一次；这 8 秒内不要轮询 /health，否则请求本身会把空闲时钟刷新。
sleep 8
if ! curl -sf "$API/health" >/dev/null 2>&1 && [ ! -e "$PID_FILE" ] && [ ! -e "$SOCK" ]; then
    ok "空闲退出后 health、pid、socket 全部收敛"
else
    no "空闲退出后仍有现场（pid=$([ -e "$PID_FILE" ] && echo yes || echo no), socket=$([ -e "$SOCK" ] && echo yes || echo no)）"
fi

summary
