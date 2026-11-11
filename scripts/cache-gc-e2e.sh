#!/usr/bin/env bash
# 缓存水位线：超过上限、块已经没人要了 → daemon 自己清掉；
# 还链着落地文件的、刚下完的，一块都不动。
#
# 这是单机数据盒子最容易出的事故（磁盘被缓存吃满），所以验收必须是真的下载 + 真的杀文件，
# 而不是只测一遍 prune 的单元函数。
set -u

# 公共样板都在 scripts/lib.sh（端口/临时目录/ok-no/trap/等 health）。
source "$(dirname "$0")/lib.sh"
e2e_init cache-gc

blob_bytes() {
    python3 - "$HOME_DIR/blobs" <<'PY'
import os, sys
root = sys.argv[1]
total = 0
for d, _, files in os.walk(root):
    for f in files:
        total += os.path.getsize(os.path.join(d, f))
print(total)
PY
}

mkdir -p "$SRC"
# 两个 2MiB 文件：合起来超过 3MB 上限，但单个不超。
head -c 2097152 /dev/urandom > "$SRC/a.bin"
head -c 2097152 /dev/urandom > "$SRC/b.bin"
start_server "$SRC" || exit 1
if ! curl -sf "http://127.0.0.1:$PORT/f/b.bin" -o /dev/null; then
    echo "测试服务器没起来（端口 $PORT），这个验收跑不了"
    exit 1
fi

SKYGET_CACHE_MAX_BYTES=3MB \
SKYGET_CACHE_GC_MIN_AGE=0s \
SKYGET_CACHE_GC_INTERVAL=10s \
    start_daemon "$D_PORT" || exit 1

echo "── 1. 两块都落地：总量超过上限，但它们都还链着落地文件 ──"
"$BIN" --json get "http://127.0.0.1:$PORT/f/a.bin" -o "$DL" --daemon --as gc >/dev/null 2>&1
"$BIN" --json get "http://127.0.0.1:$PORT/f/b.bin" -o "$DL" --daemon --as gc >/dev/null 2>&1
before=$(blob_bytes)
if [ "$before" -ge 4194304 ]; then
    ok "缓存里有两块（$before 字节）"
else
    no "缓存里应该有两块，实际 $before 字节"
fi

# 等一轮清扫（间隔 10s）：它们有落地文件链着，一块都不该少。
sleep 12
after_linked=$(blob_bytes)
if [ "$after_linked" -eq "$before" ]; then
    ok "落地文件还在：缓存一块没动（$after_linked 字节）"
else
    no "不该清有落地文件的块：$before → $after_linked"
fi

echo "── 2. 删掉一份落地文件后，它变成“没人要的旧块” ──"
rm -f "$DL/a.bin"
for _ in $(seq 1 40); do
    now=$(blob_bytes)
    [ "$now" -lt "$before" ] && break
    sleep 1
done
now=$(blob_bytes)
if [ "$now" -lt "$before" ]; then
    ok "超过上限后自动清掉了旧块（$before → $now 字节）"
else
    no "超过上限了却没清（仍然是 $now 字节）"
fi
if [ "$now" -le 3145728 ]; then
    ok "清到了上限以下（$now ≤ 3MB）"
else
    no "还在上限之上：$now"
fi

echo "── 3. 还链着落地文件的那一块不能被误删 ──"
if [ -f "$DL/b.bin" ] && [ "$(wc -c < "$DL/b.bin")" -eq 2097152 ]; then
    ok "用户文件完好（b.bin 2MiB）"
else
    no "用户文件被动了"
fi

summary
