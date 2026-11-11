#!/usr/bin/env bash
# e2e 公共样板。用法：
#
#     set -u
#     source "$(dirname "$0")/lib.sh"
#     e2e_init cli-surface          # 端口、临时目录、计数器、清理钩子
#
# 之后可以直接用：
#   $BIN $DPID                  release 二进制
#   $PORT $D_PORT $API          testserver 端口 / daemon 端口 / daemon 地址
#   $SRC $HOME_DIR $DL          server 素材目录 / daemon 数据根 / 落地目录
#   ok / no / summary           断言与收尾
#   free_port / start_server / start_daemon / wait_health / wait_state / state_of
#
# 为什么要有这个文件：所有 e2e 都复制同一段「取端口、mktemp、ok/no、
# trap、等 health」。复制多了就会各写各的（有的等 health、有的 sleep 1），
# 于是失败原因从「产品坏了」变成「脚本没睡够」。
#
# 约定：**脚本自己不要 trap EXIT**，用 e2e_track 登记进程，lib 负责收拾。

set -u

BIN=./target/release/skyget
DPID=./target/release/skynet-dataplaned

pass=0
fail=0
ok() { pass=$((pass + 1)); printf '  \033[32m✓\033[0m %s\n' "$1"; }
no() { fail=$((fail + 1)); printf '  \033[31m✗\033[0m %s\n' "$1"; }

free_port() {
    python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()'
}

E2E_PIDS=()
E2E_DIRS=()

e2e_track() { E2E_PIDS+=("$1"); }

_e2e_cleanup() {
    local p
    for p in "${E2E_PIDS[@]:-}"; do
        [ -n "$p" ] && kill "$p" 2>/dev/null
    done
    for p in "${E2E_DIRS[@]:-}"; do
        [ -n "$p" ] && rm -rf "$p"
    done
    if [ "${KEEP:-0}" = "1" ]; then
        echo "保留现场：${HOME_DIR:-?}"
    fi
}

# e2e_init <名字>：在仓库根目录下建好一切。脚本必须先 `cd` 到仓库根（或让本函数做）。
e2e_init() {
    local name="${1:-e2e}"
    cd "$(dirname "$0")/.." || exit 1
    trap _e2e_cleanup EXIT

    SRC=$(mktemp -d "/tmp/${name}-srv.XXXXXX")
    HOME_DIR=$(mktemp -d "/tmp/${name}-home.XXXXXX")
    DL=$(mktemp -d "/tmp/${name}-dl.XXXXXX")
    E2E_DIRS=("$SRC" "$HOME_DIR" "$DL")

    PORT=$(free_port)
    D_PORT=$(free_port)
    API="http://127.0.0.1:$D_PORT"

    export SKYGET_HOME="$HOME_DIR"
    export SKYGET_API="$API"
    export SKYGET_ALLOW_PRIVATE=1
    # 外层 shell 可能带着真 daemon 的 token：这些验收跑的是自己那份沙盒，别串味。
    unset SKYGET_API_TOKEN
}

# 起测试服务器（scripts/testserver.py）并等到真的能取文件。
# 用法：start_server <素材目录> [端口]（默认 $PORT）
start_server() {
    local dir="$1" port="${2:-$PORT}"
    python3 scripts/testserver.py "$port" "$dir" >/dev/null 2>&1 &
    local pid=$!
    e2e_track "$pid"
    local i=0
    while [ "$i" -lt 25 ]; do
        curl -sf "http://127.0.0.1:$port/" -o /dev/null 2>/dev/null && return 0
        # `/` 返回 404 也算「起来了」——只要 TCP 在听
        if python3 -c "import socket,sys
s=socket.socket(); s.settimeout(0.2)
sys.exit(0 if s.connect_ex(('127.0.0.1',$port))==0 else 1)"; then
            return 0
        fi
        sleep 0.2
        i=$((i + 1))
    done
    echo "  测试服务器没起来（端口 $port）" >&2
    return 1
}

# 起 daemon 并等 /health。
# 用法：start_daemon [端口] [VAR=VAL ...]
#   有 VAR=VAL 时用 env 前缀传，方便单条调整（如 SKYGET_MAX_PER_HOST=1）。
start_daemon() {
    local port="$1"
    shift || true
    env "$@" "$DPID" --port "$port" >/dev/null 2>&1 &
    local pid=$!
    e2e_track "$pid"
    wait_health "$port" || {
        echo "  daemon 没起来（端口 $port）" >&2
        return 1
    }
}

# 等一个 TCP 端口开始监听（testserver 之外的辅助进程：redirector / hangserver / upload-server）。
# wait_tcp <port> [host]   host 默认 127.0.0.1，IPv6 传 ::1
wait_tcp() {
    local port="$1" host="${2:-127.0.0.1}" i=0
    while [ "$i" -lt 40 ]; do
        python3 -c "import socket,sys
s=socket.socket(socket.AF_INET6 if ':' in '$host' else socket.AF_INET)
s.settimeout(0.2)
sys.exit(0 if s.connect_ex(('$host',$port))==0 else 1)" && return 0
        sleep 0.2
        i=$((i + 1))
    done
    return 1
}

wait_health() {
    local port="$1" i=0
    while [ "$i" -lt 60 ]; do
        curl -sf "http://127.0.0.1:$port/health" >/dev/null 2>&1 && return 0
        sleep 0.2
        i=$((i + 1))
    done
    return 1
}

# 直接问 daemon 要任务状态（CLI 自己的失败 JSON 也有 state=failed，混进来断言会看不准）。
state_of() {
    curl -sf "$API/jobs/$1" 2>/dev/null | python3 -c 'import json,sys
try:
    print(json.load(sys.stdin).get("state", ""))
except Exception:
    print("")'
}

# wait_state <id> <期望状态> [最多几秒]
wait_state() {
    local id="$1" want="$2" secs="${3:-10}" i=0
    while [ "$i" -lt $((secs * 10)) ]; do
        [ "$(state_of "$id")" = "$want" ] && return 0
        sleep 0.1
        i=$((i + 1))
    done
    return 1
}

# job_id <json 文件或 stdin>：从 CLI 的 --json 输出里取 id。
job_id() {
    python3 -c 'import json,sys
try:
    print(json.load(sys.stdin).get("id", ""))
except Exception:
    print("")'
}

summary() {
    echo
    if [ "$fail" -eq 0 ]; then
        printf '\033[1m%d 通过 / 0 失败\033[0m\n' "$pass"
    else
        printf '\033[1m%d 通过 / %d 失败\033[0m\n' "$pass" "$fail"
        exit 1
    fi
}
