#!/usr/bin/env bash
# 天网盒子 · 端到端验收
#
# 一条命令证明内核还对：分段、续传、校验、内容寻址、错误人话。
# 用法：scripts/e2e.sh            （自己起测试服务器，跑完自己收）
set -u

cd "$(dirname "$0")/.." || exit 1
BIN=./target/release/skyget
SRC=${SKYGET_E2E_SRC:-/home/skynet/.hermes/cache/scratch/skyget-srv}
HOME_DIR=${SKYGET_E2E_HOME:-/home/skynet/.hermes/cache/scratch/e2e-home}
DL=${SKYGET_E2E_DL:-/home/skynet/.hermes/cache/scratch/e2e-dl}
PORT=8899
B="http://127.0.0.1:$PORT"
export SKYGET_HOME="$HOME_DIR"

pass=0
fail=0
ok() { pass=$((pass + 1)); printf '  \033[32m✓\033[0m %s\n' "$1"; }
no() { fail=$((fail + 1)); printf '  \033[31m✗\033[0m %s\n' "$1"; }
check() { if [ "$2" = "$3" ]; then ok "$1"; else no "$1（期望 $3，实际 $2）"; fi; }
has() { case "$2" in *"$3"*) ok "$1" ;; *) no "$1（没找到「$3」，实际：$2）" ;; esac; }

j() { python3 -c "import json,sys; d=json.load(sys.stdin); print(d.get('$1',''))"; }

# macOS 没有 `sha256sum`（那是 coreutils），只有 `shasum -a 256`。
# 验收脚本本身也是产品的一部分，得能在两个平台上给出同一个答案。
sha() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    else
        shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

cleanup() { [ -n "${SRV:-}" ] && kill "$SRV" 2>/dev/null; }
trap cleanup EXIT

prep() {
    rm -rf "$HOME_DIR" "$DL"
    mkdir -p "$HOME_DIR" "$DL"
}

# 服务器和测试数据
mkdir -p "$SRC"
[ -f "$SRC/big.bin" ] || head -c 52428800 /dev/urandom > "$SRC/big.bin"
[ -f "$SRC/small.bin" ] || head -c 3000000 /dev/urandom > "$SRC/small.bin"
BIG=$(sha "$SRC/big.bin")
SMALL=$(sha "$SRC/small.bin")
python3 scripts/testserver.py $PORT "$SRC" >/dev/null 2>&1 &
SRV=$!
sleep 1.5
curl -sf -o /dev/null -H 'Range: bytes=0-0' "$B/f/small.bin" || {
    echo "测试服务器没起来"; exit 1; }

echo "── 1. 分段下载与校验 ──"
prep
out=$($BIN get "$B/f/big.bin" --to "$DL" --json 2>/dev/null)
check "50MB 下完" "$(echo "$out" | j state)" "done"
check "sha256 与源文件一致" "$(sha "$DL/big.bin")" "$BIG"
check "报告里的 sha256 也一致" "$(echo "$out" | j sha256)" "$BIG"
check "字节数准确" "$(echo "$out" | j bytes_total)" "52428800"

echo "── 2. 重复地址不走网络 ──"
t0=$(date +%s%N)
out=$($BIN get "$B/f/big.bin" --to "$DL" --json 2>/dev/null)
ms=$((($(date +%s%N) - t0) / 1000000))
check "标记为 cached" "$(echo "$out" | j cached)" "True"
[ "$ms" -lt 800 ] && ok "秒回（${ms}ms）" || no "太慢：${ms}ms"

echo "── 3. 内容寻址：同内容不同地址只存一份 ──"
prep
$BIN get "$B/f/small.bin" --to "$DL" --json >/dev/null 2>&1
out=$($BIN get "$B/norange/small.bin" --to "$DL" --json 2>/dev/null)
check "命中内容缓存" "$(echo "$out" | j cached)" "True"
n=$(find "$HOME_DIR/blobs" -type f | wc -l)
check "blob 只有一份" "$n" "1"
check "内容正确" "$(sha "$DL/small.bin")" "$SMALL"

echo "── 2b. 明确换 --to：必须落到新目录 ──"
DL2="${DL}-alt"
rm -rf "$DL2"
mkdir -p "$DL2"
out=$($BIN get "$B/f/small.bin" --to "$DL2" --json 2>/dev/null)
check "状态 done" "$(echo "$out" | j state)" "done"
check "文件在新目录" "$(sha "$DL2/small.bin")" "$SMALL"
has "报告路径指向新目录" "$(echo "$out" | j path)" "$DL2"

echo "── 4. 服务器不支持分段 ──"
out=$($BIN get "$B/norange/small.bin" --to "$DL" --name "single.bin" --json 2>/dev/null)
check "单连接也下得完" "$(echo "$out" | j state)" "done"
check "单连接内容正确" "$(sha "$DL/single.bin")" "$SMALL"

echo "── 5. 失败要说人话 ──"
out=$($BIN get "$B/deny" --json 2>/dev/null); code=$?
check "403 退出码非零" "$code" "1"
has "403 说需要登录" "$(echo "$out" | j error)" "需要登录"
out=$($BIN get "$B/missing" --json 2>/dev/null)
has "404 说不存在" "$(echo "$out" | j error)" "不存在"
out=$($BIN get "thunder://QUFodHRw" --json 2>/dev/null)
has "thunder:// 直说类型" "$(echo "$out" | j error)" "thunder"
note=$($BIN get "$B/html" --to "$DL" --json 2>&1 >/dev/null | grep -c '网页')
check "网页给出提示" "$note" "1"

echo "── 6. 校验是硬门槛 ──"
prep
out=$($BIN get "$B/f/small.bin" --to "$DL" --sha256 "$SMALL" --json 2>/dev/null)
check "期望值正确时通过" "$(echo "$out" | j state)" "done"
out=$($BIN get "$B/f/small.bin" --to "$DL" --name wrong.bin \
    --sha256 0000000000000000000000000000000000000000000000000000000000000000 --json 2>/dev/null)
check "期望值不符时失败" "$(echo "$out" | j state)" "failed"
has "失败原因说得清" "$(echo "$out" | j error)" "校验不符"
[ -f "$DL/wrong.bin" ] && no "坏文件被留下了" || ok "坏文件已丢弃"

echo "── 7. 断点续传（下到一半硬杀）──"
prep
$BIN get "$B/slow3/big.bin" --to "$DL" --json >/dev/null 2>&1 &
pid=$!
sleep 6
kill -9 "$pid" 2>/dev/null
wait "$pid" 2>/dev/null
meta=$(find "$HOME_DIR/parts" -name '*.json' | head -1)
done=$(python3 -c "import json;d=json.load(open('$meta'));print(sum(d['done']))" 2>/dev/null || echo 0)
[ "$done" -gt 0 ] && ok "位图记下了 $done 段" || no "位图没记下任何进度"
out=$($BIN get "$B/slow3/big.bin" --to "$DL" --json 2>&1)
has "续传时告诉人「接着上次下」" "$out" "接着上次下"
check "续传后内容仍然正确" "$(sha "$DL/big.bin")" "$BIG"

echo "── 8. 位图不能是废纸 ──"
# 下完后再跑一次：place 会把 .part 搬走，位图必须已清，不能产出全零文件
left=$(find "$HOME_DIR/parts" -name '*.json' | wc -l)
check "完成后不留位图" "$left" "0"

echo "── 9. 台账 ──"
n=$($BIN list --json | python3 -c "import json,sys;print(len(json.load(sys.stdin)))")
[ "$n" -gt 0 ] && ok "台账有 $n 条记录" || no "台账是空的"
id=$($BIN list --json | python3 -c "import json,sys;print(json.load(sys.stdin)[0]['id'])")
$BIN show "$id" --json | grep -q "$id" && ok "单条可查" || no "单条查不到"

echo
printf '\033[1m%d 通过 / %d 失败\033[0m\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
