#!/usr/bin/env bash
# macOS 的本地交叉类型检查（不需要 Mac 机器）。
#
# 用途：在 push 之前确认「只在 macOS 上编译的分支」真的能编译 ——
# `src/sys.rs` 的 libproc 分支、`src/uds.rs` 的 getpeereid 分支、
# `src/tray.rs` 的非 Linux 桩、以及 GUI 在 macOS 上的 cfg。
#
# 这是 `cargo check`（不链接、不产出二进制）：证明**类型与 cfg 对**，
# 不证明符号能链上、更不证明真机能跑。真机证据仍然只有 CI 的 macos job。
#
# 为什么需要假 cc：本机没有 macOS SDK，而依赖里几个 crate（rusqlite/ring）
# 的 build script 会用 `-arch arm64`、`-mmacosx-version-min=…`、`-gfull`
# 这类 Mach-O 专属参数调 cc。这里用一层壳把这些参数丢掉，让宿主 cc 生成
# 对象文件（cargo check 不链接，所以 ELF 对象没影响）。
#
# 限制：`--features bt` 过不去 —— librqbit 拉到 aws-lc，它的 aarch64-apple 汇编
# 需要真正的 Apple 汇编器。macOS 上的 BT 构建只能交给 CI。
set -u
cd "$(dirname "$0")/.." || exit 1

TARGET=aarch64-apple-darwin
CC_SHIM="$(mktemp -d)/mac-cc"
cat > "$CC_SHIM" <<'EOF'
#!/bin/bash
# 丢掉 Mach-O 专属参数，其余原样交给宿主 cc。
args=()
skip=0
for a in "$@"; do
    if [ "$skip" = 1 ]; then skip=0; continue; fi
    case "$a" in
        -arch) skip=1 ;;
        -mmacosx-version-min=*) ;;
        -gfull|-gdwarf-2|-mno-omit-leaf-frame-pointer) ;;
        *) args+=("$a") ;;
    esac
done
exec cc "${args[@]}"
EOF
chmod +x "$CC_SHIM"

if ! rustup target list --installed | grep -qx "$TARGET"; then
    echo "先装目标：rustup target add $TARGET"
    exit 1
fi

fail=0
for feats in "" "http,gui"; do
    label=${feats:-默认（http）}
    echo "── cargo check --target $TARGET --features ${feats:-<默认>} ──"
    if CC_aarch64_apple_darwin="$CC_SHIM" cargo check --quiet --target "$TARGET" ${feats:+--features "$feats"}; then
        echo "  ✓ $label 通过"
    else
        echo "  ✗ $label 失败"
        fail=1
    fi
done

echo
if [ "$fail" -eq 0 ]; then
    echo "macOS 分支类型检查通过（http / http+gui）。"
    echo "注意：这不等于「Mac 上能跑」—— 链接与真机行为由 CI 的 macos job 负责。"
else
    echo "macOS 分支有编译错误，先修再 push。" >&2
    exit 1
fi
