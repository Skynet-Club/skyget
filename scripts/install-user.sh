#!/usr/bin/env bash
# 装到用户空间（不需要 sudo）：二进制用**实体文件**，不是指向 target/release 的软链。
#
# 为什么要这个脚本：开发机上常见 `~/.local/bin/skyget -> repo/target/release/skyget`
# 这种软链。它 `cargo clean` 一次、切个分支、甚至 `cargo build` 换个 profile，
# 生产里的 daemon 就会突然指着不存在的文件 —— 而 systemd 只会在下一次重启时报错。
# 发行版（deb）装的是实体文件；这个脚本让没有 sudo 的机器得到同样的结果。
#
# 用法：
#   scripts/install-user.sh                  # 从 target/release 装
#   scripts/install-user.sh <目录>            # 从别的目录装（比如解开的 deb）
#   PREFIX=~/.local scripts/install-user.sh  # 换安装前缀
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FROM="${1:-$ROOT/target/release}"
PREFIX="${PREFIX:-$HOME/.local}"
BIN="$PREFIX/bin"
DOC="$PREFIX/share/doc/skynet-dataplane"

if [ ! -x "$FROM/skyget" ]; then
    echo "找不到 $FROM/skyget：先 cargo build --release --features full，或把解开的 deb 目录传进来。" >&2
    exit 1
fi

install -d "$BIN" "$DOC"

# 六个入口：新名 + 旧名（旧名是兼容入口，和 deb 里一致）
for name in skyget skynet-dataplaned skynet-downloadd skynet-dataplane skynet-download skynet-mcp; do
    if [ ! -x "$FROM/$name" ]; then
        echo "跳过 $name（这个构建里没有）"
        continue
    fi
    # install 会覆盖已有文件，但遇到「软链」时默认写进软链目标 —— 先删掉软链。
    if [ -L "$BIN/$name" ]; then
        rm -f "$BIN/$name"
    fi
    install -m 755 "$FROM/$name" "$BIN/$name"
done

# 文档（deb 里在 /usr/share/doc；用户装就放 ~/.local/share/doc）
for f in README.md README.en.md MANUAL.md CHANGELOG.md SECURITY.md \
         LICENSE AUTHORS.md THIRD_PARTY.md; do
    [ -f "$ROOT/$f" ] && install -m 644 "$ROOT/$f" "$DOC/$f"
done
[ -f "$ROOT/LICENSE" ] && install -m 644 "$ROOT/LICENSE" "$DOC/copyright"
install -d "$DOC/docs/adr" "$DOC/docs/screenshots"
for f in "$ROOT"/docs/*.md; do
    [ -f "$f" ] && install -m 644 "$f" "$DOC/docs/"
done
for f in "$ROOT"/docs/adr/*.md "$ROOT"/docs/screenshots/*.png; do
    [ -f "$f" ] && install -m 644 "$f" "$DOC/docs/$(basename "$(dirname "$f")")/"
done

echo "已装到 $BIN（实体文件，不依赖仓库目录）："
for name in skyget skynet-dataplaned skynet-downloadd skynet-dataplane skynet-download skynet-mcp; do
    [ -x "$BIN/$name" ] || continue
    printf '  %-22s %s 字节\n' "$name" "$(stat -c %s "$BIN/$name")"
done
# 只对确定支持 --version 的那个探一下版本；直接跑 GUI/daemon/MCP 会挂在那里等输入。
# （第一次写这个脚本就因为 `skynet-dataplane --version` 把 GUI 拉起来，卡了 10 分钟。）
printf '  skyget 版本            %s\n' "$(timeout 5 "$BIN/skyget" --version 2>/dev/null | head -1 || echo '(探测超时)')" 
echo
echo "下一步（如果还没配服务）："
echo "  scripts/install-desktop.sh          # 图标 + 桌面入口 + 用户服务"
echo "  systemctl --user restart skynet-downloadd   # 或 skynet-dataplaned，看你用的哪个单元"
echo "  curl -s http://127.0.0.1:7788/health"
