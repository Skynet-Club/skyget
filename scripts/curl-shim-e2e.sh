#!/usr/bin/env bash
# `skyget curl`：curl 的替身。这一层坏了最伤人 —— 用户的脚本会以为下载器把命令搞坏了。
#
# 它只接管「一个地址 + -o 落盘」，其余原样交给真 curl。所以这里两头都要测：
#   · 该接管的：字节与真 curl 一致、走 skyget（台账有记录）、续传、凭证映射；
#   · 该透传的：-o -、POST、多地址……一律交给真 curl，行为必须和 curl 一样。
set -u
source "$(dirname "$0")/lib.sh"
e2e_init curl-shim

mkdir -p "$SRC"
head -c 1048576 /dev/urandom > "$SRC/plain.bin"
SHA=$(python3 -c "import hashlib;print(hashlib.sha256(open('$SRC/plain.bin','rb').read()).hexdigest())")
start_server "$SRC" || exit 1

echo "── 1. 接管：skyget curl -L -o 落盘，字节与源一致 ──"
"$BIN" curl -L -o "$DL/a.bin" "http://127.0.0.1:$PORT/f/plain.bin" 2>/dev/null
if [ -f "$DL/a.bin" ] && [ "$(sha256sum "$DL/a.bin" | cut -d' ' -f1)" = "$SHA" ]; then
    ok "落盘且 sha 一致"
else
    no "没落盘或内容不对"
fi

echo "── 2. 它是真的走了 skyget（台账里有一条），不是偷偷调了 curl ──"
n=$("$BIN" --json list -l 5 2>/dev/null | python3 -c 'import json,sys;print(len(json.load(sys.stdin)))')
[ "${n:-0}" -ge 1 ] && ok "台账里有 ${n} 条记录" || no "台账是空的（说明没经过 skyget）"

echo "── 3. 与真 curl 逐字节一致（独立实现比对）──"
if command -v curl >/dev/null; then
    curl -sS -L -o "$DL/ref.bin" "http://127.0.0.1:$PORT/f/plain.bin" 2>/dev/null
    if [ "$(sha256sum "$DL/ref.bin" | cut -d' ' -f1)" = "$(sha256sum "$DL/a.bin" | cut -d' ' -f1)" ]; then
        ok "和 curl 下出来的字节相同"
    else
        no "和 curl 的字节不同"
    fi
else
    no "本机没有 curl，这一条没法比"
fi

echo "── 4. 凭证：-H \"Authorization: Bearer …\" 映射成 credential-ref ──"
export SKYGET_ALLOWED_HOSTS=127.0.0.1
"$BIN" curl -L -o "$DL/auth.bin" \
    -H "Authorization: Bearer test-secret" \
    "http://127.0.0.1:$PORT/auth/plain.bin" 2>/dev/null
if [ -f "$DL/auth.bin" ] && [ "$(sha256sum "$DL/auth.bin" | cut -d' ' -f1)" = "$SHA" ]; then
    ok "带 Bearer 的私有源下成功"
else
    no "带 Bearer 的下载失败了"
fi

echo "── 5. 凭证的值不能落台账（只留 env:SKYGET_CURL_AUTH）──"
if grep -q "test-secret" "$HOME_DIR/tasks.db" 2>/dev/null; then
    no "台账里出现了 token 明文"
else
    ok "台账里没有 token 明文"
fi
if grep -q "env:SKYGET_CURL_AUTH" "$HOME_DIR/tasks.db" 2>/dev/null; then
    ok "只记了凭证引用 env:SKYGET_CURL_AUTH"
else
    no "没找到凭证引用（映射可能没生效）"
fi

echo "── 6. 没配 SKYGET_ALLOWED_HOSTS 时，凭证必须被拒（不能把 token 发出去）──"
unset SKYGET_ALLOWED_HOSTS
out=$("$BIN" --json curl -L -o "$DL/auth2.bin" \
    -H "Authorization: Bearer test-secret" \
    "http://127.0.0.1:$PORT/auth/plain.bin" 2>&1)
# 替身模式下 stdout 是空的（结果行不吐），所以看退出码 + 文件
if [ ! -f "$DL/auth2.bin" ]; then
    ok "拒绝发送（没有落地文件）"
else
    no "竟然下成功了：$out"
fi

echo "── 7. 透传：-o - （写 stdout）交给真 curl ──"
body=$("$BIN" curl -sS -o - "http://127.0.0.1:$PORT/f/plain.bin" 2>/dev/null | head -c 16 | xxd -p | tr -d '\n')
head16=$(head -c 16 "$SRC/plain.bin" | xxd -p | tr -d '\n')
[ -n "$body" ] && [ "$body" = "$head16" ] && ok "-o - 原样写到 stdout" || no "-o - 行为不对（$body vs $head16）"

echo "── 8. 透传：-d（POST）交给真 curl ──"
code=$(curl -s -o /dev/null -w '%{http_code}' -d "x=1" "http://127.0.0.1:$PORT/f/plain.bin")
"$BIN" curl -sS -d "x=1" -o "$DL/post.txt" "http://127.0.0.1:$PORT/f/plain.bin" 2>/dev/null
if [ -f "$DL/post.txt" ] && [ "$(cat "$DL/post.txt")" = "no" ]; then
    ok "POST 交给了真 curl（拿到 testserver 的 404 响应体，http=$code）"
else
    no "POST 透传不对：$(head -c 80 "$DL/post.txt" 2>/dev/null)"
fi

echo "── 9. 失败：404 的退出码非零（和 curl 的约定一致）──"
"$BIN" curl -S -o "$DL/missing.bin" "http://127.0.0.1:$PORT/missing" 2>/dev/null
code=$?
[ "$code" -ne 0 ] && ok "404 退出码非零（$code）" || no "404 竟然退出 0"

echo "── 10. -s：安静（stderr 不该有进度）──"
err=$("$BIN" curl -s -o "$DL/quiet.bin" "http://127.0.0.1:$PORT/f/plain.bin" 2>&1 >/dev/null | head -c 200)
[ -z "$err" ] && ok "-s 时 stderr 为空" || no "-s 时仍有输出：$err"

echo "── 11. --as：来源记成 agent:<名字> ──"
"$BIN" curl --as shim-test -o "$DL/as.bin" "http://127.0.0.1:$PORT/f/plain.bin" 2>/dev/null
src=$("$BIN" --json list -l 3 2>/dev/null | python3 -c '
import json,sys
d=json.load(sys.stdin)
print(next((t["source"] for t in d if t.get("path","").endswith("as.bin")), ""))')
[ "$src" = "agent:shim-test" ] && ok "来源是 agent:shim-test" || no "来源不对：$src"

summary
