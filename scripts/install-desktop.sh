#!/usr/bin/env bash
# 把「天网盒子」装进桌面：图标（多尺寸）+ .desktop 入口 + 守护进程用户服务。
#
# 用法：scripts/install-desktop.sh [可执行文件路径] [--with-service]
#
# 默认**不设开机自启**：服务装好但 disabled，agent 一跑 skyget / MCP 就按需拉起
# （`skyget daemon ensure`）。要传统常驻就加 --with-service。
# 默认用 ~/.local/bin/skynet-dataplane；旧名 skynet-download 仍可作参数传入。
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WITH_SERVICE=0
BIN_ARG=""
for a in "$@"; do
	case "$a" in
	--with-service) WITH_SERVICE=1 ;;
	*) [ -z "$BIN_ARG" ] && BIN_ARG="$a" ;;
	esac
done
BIN="${BIN_ARG:-$HOME/.local/bin/skynet-dataplane}"
# 从仓库直接装：默认位置没有就复制刚构建的 GUI 过去；都没有才退回旧名。
if [ ! -x "$BIN" ] && [ -x "$ROOT/target/release/skynet-dataplane" ]; then
    install -d "$HOME/.local/bin"
    install -m 755 "$ROOT/target/release/skynet-dataplane" "$BIN"
fi
if [ ! -x "$BIN" ] && [ -x "$HOME/.local/bin/skynet-download" ]; then
    BIN="$HOME/.local/bin/skynet-download"
fi
ICONS="$HOME/.local/share/icons/hicolor"
APPS="$HOME/.local/share/applications"
SVG="$ROOT/packaging/icon.svg"
PNG="$ROOT/assets/icon.png"

if [ ! -x "$BIN" ]; then
    echo "找不到可执行文件：$BIN" >&2
    echo "先 cargo build --release --features full，再建软链或传路径过来。" >&2
    exit 1
fi

# ── 图标：有 rsvg-convert 就按屏幕尺寸各来一份，没有就只放 256 ──
if command -v rsvg-convert >/dev/null && [ -f "$SVG" ]; then
    for s in 16 24 32 48 64 128 256 512; do
        install -d "$ICONS/${s}x${s}/apps"
        rsvg-convert -w "$s" -h "$s" "$SVG" -o "$ICONS/${s}x${s}/apps/skynet-download.png"
    done
    echo "图标：8 个尺寸已装到 $ICONS"
else
    install -d "$ICONS/256x256/apps"
    install -m 644 "$PNG" "$ICONS/256x256/apps/skynet-download.png"
    echo "图标：只装了 256×256（缺 rsvg-convert）"
fi

# ── 守护进程：新主名优先；旧服务名迁移一次，避免两个服务抢同一个端口 ──
DAEMON_DIR="$HOME/.local/bin"
install -d "$HOME/.config/systemd/user"
# systemd unit 的 ExecStart 是 ~/.local/bin/skynet-dataplaned；从仓库直接装时
# 把刚构建的 daemon 也放到那里，否则会出现「服务已启用但可执行文件不存在」。
if [ ! -x "$DAEMON_DIR/skynet-dataplaned" ] && [ -x "$ROOT/target/release/skynet-dataplaned" ]; then
    install -d "$DAEMON_DIR"
    install -m 755 "$ROOT/target/release/skynet-dataplaned" "$DAEMON_DIR/skynet-dataplaned"
    echo "守护进程：已安装 $DAEMON_DIR/skynet-dataplaned"
fi
if [ -x "$ROOT/target/release/skynet-dataplaned" ] || [ -x "$DAEMON_DIR/skynet-dataplaned" ]; then
    if systemctl --user is-enabled skynet-downloadd >/dev/null 2>&1; then
        systemctl --user disable --now skynet-downloadd 2>/dev/null || true
        echo "守护进程：已停用旧服务 skynet-downloadd，迁移到新名"
    fi
    install -m 644 "$ROOT/packaging/skynet-dataplaned.service" "$HOME/.config/systemd/user/"
    systemctl --user daemon-reload 2>/dev/null || true
    if [ "$WITH_SERVICE" = 1 ]; then
        systemctl --user enable --now skynet-dataplaned 2>/dev/null \
            && echo "守护进程：已装成用户服务并开机自启（--with-service）" \
            || echo "守护进程：服务已装，但启用失败（手动：systemctl --user enable --now skynet-dataplaned）"
    else
        # 默认：装服务但**不开机自启**。agent 一调 skyget / MCP 就按需拉起
        # （客户端里的 daemon ensure），空闲 SKYGET_IDLE_EXIT 秒后自己退。
        systemctl --user disable skynet-dataplaned 2>/dev/null || true
        echo "守护进程：服务已装，未设开机自启（由 agent 按需拉起；常驻用 --with-service）"
    fi
elif [ -x "$ROOT/target/release/skynet-downloadd" ] || [ -x "$DAEMON_DIR/skynet-downloadd" ]; then
    install -m 644 "$ROOT/packaging/skynet-downloadd.service" "$HOME/.config/systemd/user/"
    systemctl --user daemon-reload 2>/dev/null || true
    if [ "$WITH_SERVICE" = 1 ]; then
        systemctl --user enable --now skynet-downloadd 2>/dev/null \
            && echo "守护进程：旧服务已启用并开机自启（建议尽快切到 skynet-dataplaned）"
    else
        systemctl --user disable skynet-downloadd 2>/dev/null || true
        echo "守护进程：旧服务已装，未设开机自启（由 agent 按需拉起）"
    fi
else
    echo "守护进程：没找到 skynet-dataplaned / skynet-downloadd，跳过（界面连不上时自己会尝试拉起来）"
fi

# ── 桌面入口 ──
# 文件名保留 skynet-download.desktop，upgrade 时原地更新，不在应用列表里留两个图标。
install -d "$APPS"
sed "s|^Exec=.*|Exec=$BIN|" "$ROOT/packaging/skynet-download.desktop" > "$APPS/skynet-download.desktop"
chmod +x "$APPS/skynet-download.desktop"
echo "入口：$APPS/skynet-download.desktop（Exec=$BIN）"

update-desktop-database "$APPS" 2>/dev/null || true
gtk-update-icon-cache -f -t "$ICONS" 2>/dev/null || true
echo "完成。应用列表里搜「天网盒子」即可；也可以 gtk-launch skynet-download。"
