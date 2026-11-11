#!/usr/bin/env bash
# 一条命令装完：二进制 + 文档 + 桌面入口 + 用户服务 + agent 接入（skill / MCP）。
#
# 为什么要有它：装 skyget 本来要跑三个脚本（install-user → install-desktop →
# integrations/install），而 agent 的 skill 里写三段安装指令就等于每次都让它做选择题。
# 这里把三步串成一条，幂等，可预演。
#
#   scripts/install.sh              # 真装（每一步都自己判断要不要动）
#   scripts/install.sh --dry-run    # 只说什么会变
#   scripts/install.sh --no-service # 只装二进制和 agent 接入，不碰 systemd
#   PREFIX=/opt/skynet scripts/install.sh
set -uo pipefail
cd "$(dirname "$0")/.." || exit 1

DRY=0
SERVICE=1
for a in "$@"; do
    case "$a" in
        --dry-run) DRY=1 ;;
        --no-service) SERVICE=0 ;;
        -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
        *) echo "未知参数：$a" >&2; exit 2 ;;
    esac
done

step() { printf '\n\033[1m== %s\033[0m\n' "$*"; }
run() {
    if [ "$DRY" = 1 ]; then
        printf '  会执行：%s\n' "$*"
    else
        "$@"
    fi
}

# 0) 前置：二进制得先编出来（用户空间安装不替你编译）
if [ ! -x target/release/skyget ]; then
    echo "先构建：cargo build --release --features full" >&2
    exit 1
fi

step "1. 二进制与文档（实体文件，不依赖仓库目录）"
run bash scripts/install-user.sh

step "2. 桌面入口、图标、用户服务"
if [ "$SERVICE" = 1 ]; then
    run bash scripts/install-desktop.sh
else
    echo "  跳过（--no-service）"
fi

step "3. agent 接入（skill + MCP 注册）"
run bash integrations/install.sh $([ "$DRY" = 1 ] && echo --dry-run)

step "4. 自检"
if [ "$DRY" = 1 ]; then
    echo "  （预演结束，什么都没写）"
    exit 0
fi
V=$("$HOME/.local/bin/skyget" --version 2>/dev/null || echo "skyget 未安装")
H=$(curl -s --max-time 3 http://127.0.0.1:7788/health 2>/dev/null || echo '{"ok":false}')
printf '  %-10s %s\n' "CLI" "$V"
printf '  %-10s %s\n' "daemon" "$H"
printf '  %-10s %s\n' "socket" "$([ -S "$HOME/.local/share/skynet-download/daemon.sock" ] && echo "在" || echo "不在（daemon 未运行或版本 < 0.24）")"
echo
echo "下一步（如果 daemon 没在跑）："
echo "  systemctl --user enable --now skynet-dataplaned   # 或 skynet-downloadd，看你用哪个单元"
