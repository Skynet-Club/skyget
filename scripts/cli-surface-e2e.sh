#!/usr/bin/env bash
# CLI 输出面：events / 用法错误 / statusline 两个开关 / export-import 的 JSON 字段。
#
# 这四条以前都有「看起来能用、脚本却读不出东西」的地方：
# 事件流只能靠 watch 的人话排版、参数写错时 stdout 是空的、statusline 有两个 json 开关、
# export/import 的 JSON 字段没写进文档。
set -u

# 公共样板都在 scripts/lib.sh（端口/临时目录/ok-no/trap/等 health）。
source "$(dirname "$0")/lib.sh"
e2e_init cli-surface

mkdir -p "$SRC"
head -c 6291456 /dev/urandom > "$SRC/big.bin"
start_server "$SRC" || exit 1
start_daemon "$D_PORT" SKYGET_MAX_PER_HOST=1 || exit 1

echo "── 1. events：只输出 NDJSON，每行都能 json 解析 ──"
ID=$("$BIN" --json get "http://127.0.0.1:$PORT/f/big.bin" -o "$DL" --detach 2>/dev/null \
    | python3 -c 'import json,sys;print(json.load(sys.stdin).get("id",""))')
[ -n "$ID" ] && ok "任务已提交" || no "提交失败"
"$BIN" events "${ID:0:8}" >/tmp/ev.out 2>/dev/null
if python3 - <<'PY' /tmp/ev.out
import json,sys
lines=[l for l in open(sys.argv[1]) if l.strip()]
assert lines, "没有输出"
for l in lines: json.loads(l)
print(len(lines))
PY
then
    ok "每一行都是合法 JSON（$(wc -l < /tmp/ev.out) 行）"
else
    no "有不是 JSON 的行"
fi
grep -q '"type":"queued"' /tmp/ev.out && ok "含 queued 事件" || no "没有 queued"
# 人话排版不该混进来
grep -qE '^(排队|已落地|sha256)' /tmp/ev.out && no "混进了人话输出" || ok "没有混人话排版"

echo "── 2. events --follow：跟到 done 退出 0 ──"
"$BIN" events "${ID:0:8}" --follow >/tmp/ev-follow.out 2>/dev/null
code=$?
last=$(tail -1 /tmp/ev-follow.out | python3 -c 'import json,sys;print(json.load(sys.stdin).get("type",""))' 2>/dev/null)
[ "$code" = "0" ] && [ "$last" = "done" ] && ok "退出 0，最后一条是 done" || no "退出码 $code / 最后一条 $last"

echo "── 3. 用法错误：退出码 2，--json 时 stdout 一行 cli.usage ──"
"$BIN" --json retry >/tmp/usage.json 2>/dev/null
code=$?
lines=$(wc -l < /tmp/usage.json)
if [ "$code" = "2" ] && [ "$lines" = "1" ] && grep -q '"cli.usage"' /tmp/usage.json; then
    ok "缺参数 → exit 2 + 一行 cli.usage"
else
    no "缺参数时 exit=$code、$lines 行"
fi
"$BIN" --json frobnicate >/tmp/usage2.json 2>/dev/null
[ "$?" = "2" ] && grep -q '"cli.usage"' /tmp/usage2.json \
    && ok "未知子命令 → exit 2 + cli.usage" || no "未知子命令的处理不对"
# --help 不是错误
"$BIN" --help >/dev/null 2>&1 && ok "--help 退出 0" || no "--help 退出码不对"
"$BIN" --version >/tmp/v.txt 2>&1 && grep -q "$("$BIN" --version)" /tmp/v.txt \
    && ok "--version 打到 stdout 且退出 0" || no "--version 不对"

echo "── 4. statusline：--json 与 --format json 是同一份输出 ──"
# 上面那条 6MB 早就下完了，statusline 空闲时什么都不打（这是设计）。
# 重新起一条慢任务，才有得比。
"$BIN" --json get "http://127.0.0.1:$PORT/slow/big.bin" -o "$DL" --name slow.bin --detach >/dev/null 2>&1
for _ in $(seq 1 30); do
    "$BIN" statusline --format json 2>/dev/null | grep -q . && break
    sleep 0.2
done
a=$("$BIN" statusline --json 2>/dev/null)
b=$("$BIN" statusline --format json 2>/dev/null)
if [ -n "$a" ] && [ "$a" = "$b" ]; then
    ok "两者一致（$(printf '%s' "$a" | head -c 60)…）"
else
    no "两者不一致：json=[$a] format=[$b]"
fi
c=$("$BIN" statusline --format plain 2>/dev/null)
[ -n "$c" ] && [ "$c" != "$a" ] && ok "plain 走另一条路（$(printf '%s' "$c" | head -c 40)…）" || no "plain 不对：$c"

echo "── 5. export / import 的 --json 字段 ──"
exp=$(mktemp -d)
"$BIN" --json export "$exp" >/tmp/exp.json 2>/dev/null
if python3 - <<'PY' /tmp/exp.json
import json,sys
d=json.load(open(sys.argv[1]))
assert set(d) == {"blobs","bytes","skipped"}, d
print(d)
PY
then
    ok "export --json 只有 blobs/bytes/skipped"
else
    no "export --json 字段不对：$(cat /tmp/exp.json)"
fi
imp=$(mktemp -d)
cp -r "$exp"/. "$imp"/ 2>/dev/null
"$BIN" --json import "$imp" >/tmp/imp.json 2>/dev/null
if python3 - <<'PY' /tmp/imp.json
import json,sys
d=json.load(open(sys.argv[1]))
assert set(d) == {"blobs","bytes","skipped"}, d
assert d["blobs"] == 0, d      # 同一份缓存导进来：全跳过
assert d["skipped"] >= 1, d
print(d)
PY
then
    ok "import --json 同形状，且跳过已存在的块"
else
    no "import --json 不对：$(cat /tmp/imp.json)"
fi
rm -rf "$exp" "$imp"

echo "── 6. --timeout：到点就不等（不是取消） ──"
# hangserver 只发响应头、不发正文：没有 --timeout 时这里会一直等下去。
HANG_PORT=$(free_port)
python3 scripts/hangserver.py "$HANG_PORT" >/dev/null 2>&1 &
e2e_track $!
i=0
while [ "$i" -lt 25 ]; do
    python3 -c "import socket,sys
s=socket.socket(); s.settimeout(0.2)
sys.exit(0 if s.connect_ex(('127.0.0.1',$HANG_PORT))==0 else 1)" && break
    sleep 0.2
    i=$((i + 1))
done
t0=$(date +%s)
"$BIN" --timeout 2 get "http://127.0.0.1:$HANG_PORT/stall.bin" -o "$DL" --json     >/tmp/timeout.json 2>/dev/null
code=$?
took=$(( $(date +%s) - t0 ))
if [ "$code" = "1" ] && grep -q '"cli.timeout"' /tmp/timeout.json && [ "$took" -le 6 ]; then
    ok "2 秒到点退出（实际 ${took}s，exit 1，cli.timeout）"
else
    no "超时没生效：exit=$code、${took}s、$(head -c 120 /tmp/timeout.json)"
fi
[ "$(wc -l < /tmp/timeout.json)" = "1" ] && ok "超时也只打一行 JSON" || no "超时打了多行"

summary
