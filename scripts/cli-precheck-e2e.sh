#!/usr/bin/env bash
# 预检（`skyget check`）：提交前就知道「会被怎么判」，而且**和真提交同一套规则**。
#
# 两条 daemon：一条严格（默认），一条放开私网 —— 同一个地址在两边的结论必须不同，
# 这才证明判据来自策略，而不是写死。顺带验 completions 和 show --url。
set -u

# 公共样板都在 scripts/lib.sh（端口/临时目录/ok-no/trap/等 health）。
source "$(dirname "$0")/lib.sh"
e2e_init cli-precheck

mkdir -p "$SRC"
head -c 262144 /dev/urandom > "$SRC/small.bin"
STRICT_PORT=$(free_port)
OPEN_PORT=$(free_port)
start_server "$SRC" || exit 1

# 严格 daemon（daemon 默认策略：只放公网）
start_daemon "$STRICT_PORT" SKYGET_ALLOW_PRIVATE= || exit 1
start_daemon "$OPEN_PORT" SKYGET_ALLOW_PRIVATE=1 || exit 1

strict() { SKYGET_API="http://127.0.0.1:$STRICT_PORT" "$BIN" "$@"; }
open_api() { SKYGET_API="http://127.0.0.1:$OPEN_PORT" "$BIN" "$@"; }

echo "── 1. 公网地址：会接受，且逐项列出判据 ──"
out=$(strict check https://example.org/x.iso 2>&1)
case "$out" in
    *"会接受"*) ok "公网直链 → 会接受";;
    *) no "公网直链被判拒绝：$(echo "$out" | tail -2)";;
esac
case "$out" in
    *"✓ protocol"*"✓ dest"*"✓ egress"*) ok "逐项给了 protocol/dest/egress";;
    *) no "逐项结论不全：$out";;
esac

echo "── 2. 回环地址：严格 daemon 拒，放开私网的 daemon 过（同一地址、不同策略）──"
out=$(strict check "http://127.0.0.1:$PORT/f/small.bin" -o "$DL" 2>&1)
case "$out" in
    *policy.egress_denied*) ok "严格 → 403 policy.egress_denied";;
    *) no "严格却没拒：$(echo "$out" | tail -2)";;
esac
case "$out" in
    *SKYGET_ALLOW_PRIVATE*) ok "给了 next_action（说明怎么放行）";;
    *) no "没有 next_action";;
esac
code=0
open_api check "http://127.0.0.1:$PORT/f/small.bin" -o "$DL" >/tmp/precheck-ok.txt 2>&1 || code=$?
if [ "$code" = "0" ] && grep -q "会接受" /tmp/precheck-ok.txt; then
    ok "放开私网 → 会接受（退出码 0）"
else
    no "放开私网也拒了（exit=$code）：$(tail -2 /tmp/precheck-ok.txt)"
fi

echo "── 3. 元数据地址：即便是放开私网的部署，也要能看出它被拒/被放行的依据 ──"
out=$(strict check http://169.254.169.254/latest/meta-data 2>&1)
case "$out" in
    *"169.254.169.254"*) ok "严格 → 明确点名元数据地址";;
    *) no "没点名：$(echo "$out" | tail -2)";;
esac

echo "── 4. 明确不做的类型 / 多源缺 sha：不用联网就拒 ──"
out=$(strict check thunder://QUFodHRw 2>&1)
case "$out" in
    *input.unsupported*) ok "thunder:// → input.unsupported";;
    *) no "thunder 没被拒：$(echo "$out" | tail -2)";;
esac
out=$(strict check https://example.org/a --mirror https://example.org/b 2>&1)
case "$out" in
    *input.multi_source_needs_sha*) ok "多源缺 sha → input.multi_source_needs_sha";;
    *) no "多源缺 sha 没被拒：$(echo "$out" | tail -2)";;
esac

echo "── 5. --json 只有一行，且带 ok/checks ──"
lines=$(strict --json check https://example.org/x.iso 2>/dev/null | wc -l)
payload=$(strict --json check https://example.org/x.iso 2>/dev/null)
if [ "$lines" = "1" ] && printf '%s' "$payload" | grep -q '"checks"'; then
    ok "--json 一行、含 checks"
else
    no "--json 形状不对（$lines 行）"
fi
# 拒绝时也是退出码 1 + 一行
strict --json check http://127.0.0.1:9/x >/tmp/precheck-deny.json 2>/dev/null
[ $? -eq 1 ] && [ "$(wc -l < /tmp/precheck-deny.json)" = "1" ] \
    && ok "拒绝时：退出码 1，stdout 仍是一行" \
    || no "拒绝时的退出码/stdout 不对"

echo "── 6. 真提交的判定和预检一致 ──"
# 预检说「会接受」的地址，真提交就该 202
open_api --json get "http://127.0.0.1:$PORT/f/small.bin" -o "$DL" --as precheck >/tmp/precheck-submit.json 2>/dev/null
if grep -q '"state":"done"' /tmp/precheck-submit.json; then
    ok "预检通过 → 真提交成功"
else
    no "预检通过但提交失败：$(tail -1 /tmp/precheck-submit.json)"
fi
# 预检说「会被拒绝」的，真提交也必须拒绝（同一套规则）
out=$(open_api --json get thunder://QUFodHRw -o "$DL" 2>&1)
printf '%s' "$out" | grep -q 'unsupported\|不支持的地址' \
    && ok "预检拒绝的地址，真提交也拒绝" \
    || no "预检与真提交不一致：$out"

echo "── 7. show --url：按地址查最近一条 ──"
out=$(open_api show --url "http://127.0.0.1:$PORT/f/small.bin" 2>&1)
case "$out" in
    *"状态"*"done"*) ok "show --url 找到记录（done）";;
    *) no "show --url 没找到：$(echo "$out" | head -2)";;
esac
out=$(open_api show --url "https://example.org/never-there" 2>&1)
[ $? -eq 1 ] && case "$out" in
    *"台账里没有"*) ok "没下过的地址 → 明确说没有";;
    *) no "报错不清楚：$out";;
esac

echo "── 8. shell 补全 ──"
for sh in bash zsh fish; do
    n=$("$BIN" completions "$sh" 2>/dev/null | wc -l)
    if [ "$n" -gt 20 ]; then ok "completions $sh（$n 行）"; else no "completions $sh 太短（$n 行）"; fi
done
"$BIN" completions bash 2>/dev/null | grep -q "check" && ok "补全里有 check" || no "补全里没有 check"

summary
