#!/usr/bin/env bash
# URL 形态：那些「看起来能下、其实一步就踩空」的写法。
#
# 这一批里两条是**安全**用例，不是功能用例：
#   · 302 打向元数据地址（169.254.169.254）必须在**那一跳**被拒；
#   · file:// / data: 必须拒绝，而不是把本机文件当"下载"。
# 其余是解析细节：IPv6 字面量、#fragment、查询串里带 &/=/%。
source "$(dirname "$0")/lib.sh"
e2e_init url-forms

mkdir -p "$SRC"
head -c 262144 /dev/urandom > "$SRC/plain.bin"
SHA=$(python3 -c "
import hashlib
print(hashlib.sha256(open('$SRC/plain.bin','rb').read()).hexdigest())")

start_server "$SRC" || exit 1

# 一个只发 Location 的跳转服务器（它不访问目标，所以指到元数据地址是安全的）
REDIR_PORT=$(free_port)
python3 scripts/redirector.py "$REDIR_PORT" "http://169.254.169.254/latest/meta-data" >/dev/null 2>&1 &
e2e_track $!
wait_tcp "$REDIR_PORT" || { echo "  跳转服务器没起来" >&2; exit 1; }

run() { "$BIN" --json get "$1" -o "$DL" ${2:-} 2>/dev/null; }
state() { printf '%s' "$1" | python3 -c 'import json,sys
try: print(json.load(sys.stdin).get("state",""))
except Exception: print("")'; }
code() { printf '%s' "$1" | python3 -c 'import json,sys
try: print(json.load(sys.stdin).get("error_code",""))
except Exception: print("")'; }

echo "── 1. 正常地址：基线（后面几条都拿它当对照）──"
out=$(run "http://127.0.0.1:$PORT/f/plain.bin" "--name plain.bin")
[ "$(state "$out")" = "done" ] && ok "基线能下完" || no "基线就失败了：$(printf '%s' "$out" | head -c 160)"

echo "── 2. #fragment：不该被发给服务器 ──"
out=$(run "http://127.0.0.1:$PORT/f/plain.bin#section-2" "--name frag.bin")
if [ "$(state "$out")" = "done" ] && [ "$(sha256sum "$DL/frag.bin" | cut -d' ' -f1)" = "$SHA" ]; then
    ok "带 #fragment 的地址正常下载（fragment 没进请求行）"
else
    no "带 fragment 失败：$(printf '%s' "$out" | head -c 160)"
fi

echo "── 3. 查询串里带 & = % ：要原样传给服务器，不能截断 ──"
out=$(run "http://127.0.0.1:$PORT/f/plain.bin?a=1&b=x%20y&c=%2F" "--name query.bin")
if [ "$(state "$out")" = "done" ] && [ "$(sha256sum "$DL/query.bin" | cut -d' ' -f1)" = "$SHA" ]; then
    ok "查询串里的 & / % 没把地址拆坏"
else
    no "查询串被拆坏了：$(printf '%s' "$out" | head -c 160)"
fi

echo "── 4. IPv6 字面量 http://[::1]:PORT/ ──"
V6_PORT=$(free_port)
python3 scripts/testserver.py "$V6_PORT" "$SRC" "::1" >/dev/null 2>&1 &
e2e_track $!
if wait_tcp "$V6_PORT" "::1"; then
    out=$(run "http://[::1]:$V6_PORT/f/plain.bin" "--name v6.bin")
    if [ "$(state "$out")" = "done" ] && [ "$(sha256sum "$DL/v6.bin" | cut -d' ' -f1)" = "$SHA" ]; then
        ok "IPv6 字面量能下（方括号与端口都解析对了）"
    else
        no "IPv6 失败：$(printf '%s' "$out" | head -c 200)"
    fi
else
    no "本机没有 ::1，跳过不算通过（请在有 IPv6 的机器上跑）"
fi

echo "── 5. file:// 必须拒绝（不能把本机文件当下载）──"
SECRET="$HOME_DIR/secret.txt"
echo "do-not-download-this" > "$SECRET"
out=$(run "file://$SECRET" "--name leaked.bin")
if [ "$(state "$out")" = "failed" ] && [ ! -f "$DL/leaked.bin" ]; then
    ok "file:// 被拒，没落地（$(code "$out")）"
else
    no "file:// 竟然被处理了：$(printf '%s' "$out" | head -c 160)"
fi

echo "── 6. data: 必须拒绝 ──"
out=$(run "data:text/plain,hello" "--name data.bin")
[ "$(state "$out")" = "failed" ] && ok "data: 被拒（$(code "$out")）" || no "data: 竟然被处理了"

echo "── 7. 302 到公网/同机地址：跟随（基线证明跳转本身是支持的）──"
out=$(run "http://127.0.0.1:$PORT/redirect/plain.bin" "--name jump.bin")
if [ "$(state "$out")" = "done" ] && [ "$(sha256sum "$DL/jump.bin" | cut -d' ' -f1)" = "$SHA" ]; then
    ok "302 被跟随，字节一致"
else
    no "302 跟丢了：$(printf '%s' "$out" | head -c 160)"
fi

echo "── 8. 302 打向元数据地址：必须在那一跳被拒（安全）──"
t0=$(date +%s)
out=$("$BIN" --json --timeout 20 get "http://127.0.0.1:$REDIR_PORT/anything" -o "$DL" \
    --name meta.bin 2>/dev/null)
took=$(( $(date +%s) - t0 ))
c=$(code "$out")
if [ "$(state "$out")" = "failed" ] && [ "$c" = "policy.metadata_denied" ]; then
    ok "重定向到 169.254.169.254 被策略拒绝（${took}s，code=$c）"
elif [ "$(state "$out")" = "failed" ] && [ "$c" = "network.timeout" ]; then
    no "没有在策略层拒绝，而是去连了（${took}s 超时）——这是策略漏洞"
else
    no "结果不符合预期：$(printf '%s' "$out" | head -c 200)"
fi
[ "$took" -le 10 ] && ok "拒绝是即时的（${took}s，没有等连接超时）" || no "拒绝太慢（${took}s）"

echo "── 9. 同一条 302 链路：check 也要给出同样的结论 ──"
# check 问的是 daemon 的策略（lib 已经把 SKYGET_ALLOW_PRIVATE=1 导出了）。
start_daemon "$D_PORT" || exit 1
out=$("$BIN" --json check "http://127.0.0.1:$REDIR_PORT/anything" 2>/dev/null)
# check 只看顶层地址（预检不联网），所以这里期望 egress 通过；
# 真正拦下第二跳的是下载时的策略。这条断言是「预检不假装知道跳转」。
if printf '%s' "$out" | grep -q '"ok":true'; then
    ok "预检对顶层地址放行（它不跟跳转，如实不假装）"
else
    no "预检对顶层地址给了别的结论：$(printf '%s' "$out" | head -c 160)"
fi

echo "── 10. 台账里不应该出现 secret 文件的内容指纹 ──"
if grep -q "$(sha256sum "$SECRET" | cut -d' ' -f1)" "$HOME_DIR/tasks.db" 2>/dev/null; then
    no "file:// 的内容进了台账"
else
    ok "file:// 没有被读进任何记录"
fi

summary
