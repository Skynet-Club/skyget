#!/usr/bin/env bash
# 从矢量源重新生成全套图标。
#
#   scripts/make-icons.sh [源.svg]
#
# 不给参数就用 assets/icon.svg。
#
# 为什么要有这个脚本：logo 换一次要动 assets/icon.png（顶栏 + 窗口图标，编进二进制里）
# 和 hicolor 八档（桌面 / 任务栏 / 托盘），一共九处。手敲九次迟早漏一处，
# 而且漏的那一处只有「托盘还是旧图标」这种很难注意到的症状。
#
# 两个坑写在这里，省得下次再踩：
#   1. 源 svg 不是正方形（238.554×242.546）。直接 -w 512 -h 512 会被硬拉，
#      纵向压掉 1.7%，箭头看着是歪的。所以先按宽度渲染，再放到正方形画布上居中。
#   2. 图标贴边会显得「被裁掉了」。四周留 6% 边距，小尺寸下也不至于顶到边。
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SRC="${1:-$ROOT/assets/icon.svg}"
OUT="$(mktemp -d)"
trap 'rm -rf "$OUT"' EXIT

[ -f "$SRC" ] || { echo "找不到矢量源：$SRC"; exit 1; }
command -v rsvg-convert >/dev/null || { echo "缺 rsvg-convert（apt install librsvg2-bin）"; exit 1; }
python3 -c "import PIL" 2>/dev/null || { echo "缺 python3-pil（apt install python3-pil）"; exit 1; }

echo "源：$SRC"
rsvg-convert -w 1024 "$SRC" -o "$OUT/big.png"

python3 - "$OUT" <<'PY'
import sys
from PIL import Image

out = sys.argv[1]
im = Image.open(f"{out}/big.png").convert("RGBA")
w, h = im.size
side = int(max(w, h) * 1.12)          # 6% 边距
canvas = Image.new("RGBA", (side, side), (0, 0, 0, 0))
canvas.paste(im, ((side - w) // 2, (side - h) // 2), im)
canvas.save(f"{out}/master.png")
for s in (16, 24, 32, 48, 64, 128, 256, 512):
    canvas.resize((s, s), Image.LANCZOS).save(f"{out}/{s}.png")
print(f"渲染完成：{w}x{h} → 方画布 {side}x{side} → 八个尺寸")
PY

cp "$OUT/256.png" "$ROOT/assets/icon.png"
echo "assets/icon.png ← 256×256（编进二进制：顶栏 + 窗口图标）"

ICONS="$HOME/.local/share/icons/hicolor"
for s in 16 24 32 48 64 128 256 512; do
    install -Dm644 "$OUT/$s.png" "$ICONS/${s}x${s}/apps/skynet-download.png"
done
echo "hicolor ← 八档（桌面 / 任务栏 / 托盘）"

if command -v gtk-update-icon-cache >/dev/null; then
    gtk-update-icon-cache -f -t "$ICONS" >/dev/null 2>&1 || true
    echo "图标缓存已刷新"
fi

cat <<'TIP'

下一步：重编（icon.png 是 include_bytes! 进去的，不重编还是旧图标）
    cargo build --release --features full
再重开界面。托盘图标按名字取 hicolor，重开就换过来了。
TIP
