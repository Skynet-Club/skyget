#!/usr/bin/env bash
# 构建一个可 dpkg -i 的天网盒子包（不改系统，只产出 .deb）。
#
# 用法：packaging/build-deb.sh [--no-build]
# 产物：dist/skynet-dataplane_<version>_<arch>.deb
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
OUT="$ROOT/dist"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
ARCH="$(dpkg --print-architecture)"
PKG="$ROOT/target/deb/skynet-dataplane_${VERSION}_${ARCH}"

if [ "${1:-}" != "--no-build" ]; then
    cargo build --release --features full
fi

for bin in skyget skynet-dataplaned skynet-mcp skynet-dataplane; do
    [ -x "target/release/$bin" ] || {
        echo "缺少 target/release/$bin；先 cargo build --release --features full" >&2
        exit 1
    }
done

rm -rf "$PKG"
mkdir -p "$PKG/DEBIAN" \
    "$PKG/usr/bin" \
    "$PKG/usr/lib/systemd/user" \
    "$PKG/usr/share/applications" \
    "$PKG/usr/share/icons/hicolor/scalable/apps" \
    "$PKG/usr/share/icons/hicolor/256x256/apps" \
    "$PKG/usr/share/doc/skynet-dataplane"

install -m 755 target/release/skyget target/release/skynet-dataplaned \
    target/release/skynet-mcp target/release/skynet-dataplane "$PKG/usr/bin/"
# systemd 用户服务：deb 里不能用 %h/.local/bin，改成 /usr/bin。
sed \
    -e 's|^ExecStart=.*|ExecStart=/usr/bin/skynet-dataplaned --port 7788|' \
    -e 's|^Documentation=.*|Documentation=file:///usr/share/doc/skynet-dataplane/README.md|' \
    packaging/skynet-dataplaned.service >"$PKG/usr/lib/systemd/user/skynet-dataplaned.service"

sed 's|^Exec=.*|Exec=skynet-dataplane|' packaging/skynet-download.desktop \
    >"$PKG/usr/share/applications/skynet-download.desktop"
chmod 644 "$PKG/usr/lib/systemd/user/skynet-dataplaned.service" \
    "$PKG/usr/share/applications/skynet-download.desktop"
install -m 644 packaging/icon.svg \
    "$PKG/usr/share/icons/hicolor/scalable/apps/skynet-download.svg"
if [ -f assets/icon.png ]; then
    install -m 644 assets/icon.png \
        "$PKG/usr/share/icons/hicolor/256x256/apps/skynet-download.png"
fi
# 发行说明不放进包里：它带产物自己的 sha256，而包不可能包含自己的哈希。
# 它在 dist/ 里和 .deb 并排发。
install -m 644 README.md CHANGELOG.md LICENSE THIRD_PARTY.md docs/platform-support.md \
    "$PKG/usr/share/doc/skynet-dataplane/"
# Debian 规矩：/usr/share/doc/<pkg>/copyright 是机器的许可证声明（DEP-5 格式）。
# 它比 LICENSE 多写了 vendored egui-winit 的 MIT OR Apache-2.0 归属。
install -m 644 packaging/copyright "$PKG/usr/share/doc/skynet-dataplane/copyright"

# man 页：`man skyget` / `man skynet-dataplaned` 能直接查到命令与环境变量。
install -d "$PKG/usr/share/man/man1" "$PKG/usr/share/man/man8"
gzip -9nc packaging/skyget.1 > "$PKG/usr/share/man/man1/skyget.1.gz"
gzip -9nc packaging/skynet-dataplaned.8 > "$PKG/usr/share/man/man8/skynet-dataplaned.8.gz"
chmod 644 "$PKG/usr/share/man/man1/skyget.1.gz" "$PKG/usr/share/man/man8/skynet-dataplaned.8.gz"

# shell 补全：运行时 `skyget completions` 能生成，装进包里才是默认就有。
# 三个 shell 各自的标准目录（bash-completion 包会读它）。
COMP_BIN=target/release/skyget
if [ -x "$COMP_BIN" ]; then
    install -d "$PKG/usr/share/bash-completion/completions" \
               "$PKG/usr/share/zsh/vendor-completions" \
               "$PKG/usr/share/fish/vendor_completions.d"
    "$COMP_BIN" completions bash > "$PKG/usr/share/bash-completion/completions/skyget"
    "$COMP_BIN" completions zsh  > "$PKG/usr/share/zsh/vendor-completions/_skyget"
    "$COMP_BIN" completions fish > "$PKG/usr/share/fish/vendor_completions.d/skyget.fish"
    chmod 644 "$PKG/usr/share/bash-completion/completions/skyget" \
              "$PKG/usr/share/zsh/vendor-completions/_skyget" \
              "$PKG/usr/share/fish/vendor_completions.d/skyget.fish"
fi

SIZE="$(du -sk "$PKG" | cut -f1)"
cat >"$PKG/DEBIAN/control" <<EOF
Package: skynet-dataplane
Version: ${VERSION}
Section: net
Priority: optional
Architecture: ${ARCH}
Installed-Size: ${SIZE}
Maintainer: Skynet <hermes@skynet.club>
Depends: libc6, libgcc-s1, libssl3, zlib1g, libzstd1, libx11-6, libxkbcommon0, libwayland-client0, libgl1, libegl1
Description: 天网盒子：单机多 agent 的下载/上传/校验数据面
 常驻 daemon、共享台账与内容寻址缓存、per-agent token/配额/凭证、
 HLS/DASH/BT、MCP 与状态栏。安装后启用用户服务：
 systemctl --user enable --now skynet-dataplaned
EOF

# 当前 shell 的 umask 可能是 077；dpkg 要求包内目录至少 0755。
find "$PKG" -type d -exec chmod 755 {} +
chmod 644 "$PKG/DEBIAN/control"

mkdir -p "$OUT"
DEB="$OUT/skynet-dataplane_${VERSION}_${ARCH}.deb"
dpkg-deb --build --root-owner-group "$PKG" "$DEB" >/dev/null
echo "构建完成：$DEB"
dpkg-deb -I "$DEB" | sed -n '1,16p'

# 产物旁边永远放一份校验和：分发时贴这一行，比让人手算 sha256 靠谱。
DEB="$(ls -1t "$OUT"/skynet-dataplane_*_"$ARCH".deb | head -1)"
if [ -n "$DEB" ]; then
    ( cd "$OUT" && sha256sum "$(basename "$DEB")" > "$(basename "$DEB").sha256" )
    # 汇总文件：发布页贴这一份，比让人在一堆 .sha256 里挑更省事。
    ( cd "$OUT" && sha256sum skynet-dataplane_*.deb > SHA256SUMS )
    echo "sha256: $(cat "$DEB.sha256")"
fi
