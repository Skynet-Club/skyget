#!/usr/bin/env bash
# CLI 的传输控制：pause / resume / cancel 必须能用短 id、走 daemon、状态真的变。
#
# 这三个命令以前只存在于 HTTP 接口、GUI 按钮和 MCP 里 —— 命令行只能看不能停，
# 而 `skyget forget` 又要求「任务必须先停下」。这个验收守的就是那条缺口。
set -u

# 公共样板都在 scripts/lib.sh（端口/临时目录/ok-no/trap/等 health）。
source "$(dirname "$0")/lib.sh"
e2e_init cli-control

mkdir -p "$SRC"
# 24MB / slow（每 200ms 一个 256KB 片段）+ 单主机只许一条连接 ≈ 20 秒：
# 暂停/继续/取消才有窗口。用 6MB 时任务在两条命令之间就下完了（踩过）。
head -c 25165824 /dev/urandom > "$SRC/big.bin"
start_server "$SRC" || exit 1

start_daemon "$D_PORT" SKYGET_MAX_PER_HOST=1 || exit 1

# 读状态直接问 daemon：CLI 自己的失败 JSON 也有 state=failed，混进来断言会看不出来。
state_of() {
    curl -sf "$API/jobs/$1" 2>/dev/null | python3 -c 'import json,sys
try:
    print(json.load(sys.stdin).get("state", ""))
except Exception:
    print("")'
}

wait_state() { # $1=id $2=期望状态 $3=最多等几秒
    local i=0
    while [ "$i" -lt $(( ${3:-10} * 10 )) ]; do
        [ "$(state_of "$1")" = "$2" ] && return 0
        sleep 0.1
        i=$((i + 1))
    done
    return 1
}

echo "── 1. 提交一条慢任务（--detach）──"
out=$("$BIN" --json get "http://127.0.0.1:$PORT/slow/big.bin" -o "$DL" --as cli-test --detach 2>/dev/null)
ID=$(printf '%s' "$out" | python3 -c 'import json,sys;print(json.load(sys.stdin).get("id",""))')
SHORT=${ID:0:8}
if [ -n "$ID" ]; then ok "已提交 ${SHORT}…"; else no "提交失败：$out"; fi
wait_state "$ID" running 10 && ok "任务在跑" || no "任务没跑起来（$(state_of "$ID")）"

echo "── 2. skyget pause <短 id> ──"
"$BIN" pause "$SHORT" >/dev/null 2>&1
if wait_state "$ID" interrupted 10; then
    ok "暂停生效（interrupted）"
else
    no "暂停没生效（$(state_of "$ID")）"
fi

echo "── 3. 暂停后分片还在（不该清掉）──"
parts=$(ls -1 "$HOME_DIR/parts" 2>/dev/null | grep -c '\.part$' || true)
[ "${parts:-0}" -ge 1 ] && ok "分片还在（$parts 个 .part）" || no "暂停把分片清掉了"

echo "── 4. skyget resume <短 id> ──"
"$BIN" resume "$SHORT" >/dev/null 2>&1
if wait_state "$ID" running 10; then
    ok "继续生效（running）"
else
    no "继续没生效（$(state_of "$ID")）"
fi

echo "── 5. 三个命令的 --json 都是一行、带任务 id ──"
bad=""
outfile=$(mktemp)
for verb in pause resume; do
    # 写文件再数行：`$(...)` 会吃掉结尾换行，`printf | wc -l` 数出来永远是 0（踩过）
    if ! "$BIN" --json "$verb" "$SHORT" >"$outfile" 2>/dev/null; then
        bad="$bad $verb(命令失败)"
        continue
    fi
    lines=$(wc -l <"$outfile")
    grep -q '"id"' "$outfile" || bad="$bad $verb(没有 id)"
    [ "$lines" = "1" ] || bad="$bad $verb($lines 行)"
done
rm -f "$outfile"
[ -z "$bad" ] && ok "pause / resume 的 --json 形状正确" || no "--json 形状不对：$bad"

echo "── 6. skyget cancel：记录消失、文件不动 ──"
"$BIN" cancel "$SHORT" >/dev/null 2>&1
if wait_state "$ID" "" 10; then
    ok "取消后记录消失（daemon 查不到）"
else
    no "取消后记录还在（$(state_of "$ID")）"
fi
if [ -f "$DL/big.bin" ]; then
    no "取消后不该有落地的文件"
else
    ok "没有落地文件（取消不掉半成品）"
fi

echo "── 7. 对已经结束的任务暂停：要说清是哪个状态，不能甩「不是这个服务在跑」──"
# 上一步已经删了记录，这里用一个真下完的任务来试
"$BIN" --json get "http://127.0.0.1:$PORT/f/big.bin" -o "$DL" --as cli-test 2>/dev/null >/dev/null
DONE_ID=$("$BIN" --json list -l 1 2>/dev/null | python3 -c 'import json,sys;d=json.load(sys.stdin);print(d[0]["id"] if d else "")')
msg=$("$BIN" pause "${DONE_ID:0:8}" 2>&1)
case "$msg" in
    *"已经结束"*) ok "对完成的任务暂停 → 「已经结束（done）」（$msg）";;
    *"不是这个服务在跑"*) no "还是那句误导人的「不是这个服务在跑」";;
    *) no "报错不清楚：$msg";;
esac

echo "── 8. 未知 id 要说人话 ──"
err=$("$BIN" pause zzzzzzzz 2>&1)
case "$err" in
    *"台账里没有"*) ok "未知 id → 台账里没有这个任务";;
    *) no "未知 id 的报错不对：$err";;
esac

summary
