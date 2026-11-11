#!/usr/bin/env bash
# 清单签名：keygen → sign → install --key 必须闭环；改了内容就必须验不过。
#
# sha256 只能证明「拿到的字节和某个 hash 一致」，证明不了「这个 hash 是发布方给的」。
# 这个验收守的是那条断掉的链。
set -u

# 公共样板都在 scripts/lib.sh（端口/临时目录/ok-no/trap/等 health）。
source "$(dirname "$0")/lib.sh"
e2e_init manifest-sign
# 发布产物目录（私钥、清单、安装落点都在这里）
OUT=$(mktemp -d "/tmp/manifest-sign-out.XXXXXX")
E2E_DIRS+=("$OUT")

mkdir -p "$SRC" "$OUT/bin"
head -c 131072 /dev/urandom > "$SRC/tool.bin"
SHA=$(python3 -c "
import hashlib,sys
print(hashlib.sha256(open('$SRC/tool.bin','rb').read()).hexdigest())")
start_server "$SRC" || exit 1

echo "── 1. keygen：公钥/私钥都拿得到，私钥是 0600 ──"
"$BIN" keygen --out "$OUT/priv.hex" >"$OUT/keygen.txt" 2>&1
PUB=$(grep -oE '公钥  [0-9a-f]{64}' "$OUT/keygen.txt" | awk '{print $2}')
if [ "${#PUB}" = "64" ] && [ -f "$OUT/priv.hex" ]; then
    ok "公钥 64 位十六进制，私钥已写盘"
else
    no "keygen 输出不对：$(cat "$OUT/keygen.txt")"
fi
mode=$(stat -c '%a' "$OUT/priv.hex" 2>/dev/null || stat -f '%Lp' "$OUT/priv.hex" 2>/dev/null)
if [ "$mode" = "600" ]; then
    ok "私钥权限 0600"
else
    no "私钥权限是 $mode，期望 600"
fi

MANIFEST="$OUT/m.json"
cat > "$MANIFEST" <<EOF
{
  "name": "tool",
  "version": "1.0",
  "files": [
    {
      "url": "http://127.0.0.1:$PORT/f/tool.bin",
      "sha256": "$SHA",
      "dest": "$OUT/bin/tool.bin",
      "mode": "755"
    }
  ],
  "bin": "$OUT/bin/tool.bin"
}
EOF

echo "── 2. 没签名却给了公钥：必须拒绝 ──"
if "$BIN" --json install "$MANIFEST" --key "$PUB" >/dev/null 2>"$OUT/err1"; then
    no "未签名的清单带着 --key 竟然通过了"
else
    ok "未签名 + --key → 拒绝（$(head -1 "$OUT/err1" | cut -c1-40)…）"
fi

echo "── 3. 签名后安装：内容校验 + 签名校验一起过 ──"
"$BIN" sign "$MANIFEST" --key "@$OUT/priv.hex" --key-id test-key --in-place >/dev/null 2>&1
if grep -q '"sig"' "$MANIFEST"; then
    ok "签名写回了清单"
else
    no "清单里没有签名"
fi
if "$BIN" --json install "$MANIFEST" --key "$PUB" >"$OUT/install.json" 2>"$OUT/err2"; then
    ok "带签名 + 正确公钥 → 安装成功"
else
    no "安装失败：$(tail -2 "$OUT/err2")"
fi
if [ -f "$OUT/bin/tool.bin" ] && [ "$(wc -c < "$OUT/bin/tool.bin")" = "131072" ]; then
    ok "文件落地，大小正确"
else
    no "文件没落地"
fi

echo "── 4. 有人改了一个字节：签名必须验不过 ──"
python3 - "$MANIFEST" <<'PY'
import json, sys
p = sys.argv[1]
m = json.load(open(p))
m["files"][0]["mode"] = "777"   # 改权限也是改内容
json.dump(m, open(p, "w"), indent=2)
PY
if "$BIN" --json install "$MANIFEST" --key "$PUB" >/dev/null 2>"$OUT/err3"; then
    no "清单被改了，签名却验过了"
else
    ok "改了内容 → 签名校验失败"
fi
if grep -q "签名" "$OUT/err3"; then
    ok "失败原因指向签名（不是含糊的 IO 错误）"
else
    no "失败原因没说清：$(tail -1 "$OUT/err3")"
fi

echo "── 5. 拿错公钥也要验不过 ──"
"$BIN" keygen >"$OUT/other.txt" 2>&1
OTHER=$(grep -oE '公钥  [0-9a-f]{64}' "$OUT/other.txt" | awk '{print $2}')
if "$BIN" --json install "$MANIFEST" --key "$OTHER" >/dev/null 2>&1; then
    no "换了一把公钥竟然验过了"
else
    ok "公钥不匹配 → 拒绝"
fi

echo "── 6. manifest-digest 给出稳定的摘要 ──"
d1=$("$BIN" manifest-digest "$MANIFEST" 2>/dev/null)
d2=$("$BIN" manifest-digest "$MANIFEST" 2>/dev/null)
if [ "${#d1}" = "64" ] && [ "$d1" = "$d2" ]; then
    ok "摘要 64 位且稳定（$d1）"
else
    no "摘要不对：$d1 / $d2"
fi

summary
