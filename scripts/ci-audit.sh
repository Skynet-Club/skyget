#!/usr/bin/env bash
# push 之前跑一遍：CI 里引用的每个文件都在、版本号各处一致、工作流本身能解析。
#
# 为什么需要：本仓库还没有 remote，CI 一次都没跑过。第一次 push 才发现
# 「脚本改名了但 workflow 还指着旧名字」是最没必要的一次失败。
set -u
cd "$(dirname "$0")/.." || exit 1

bad=0
say() { printf '  %s %s\n' "$1" "$2"; }
ok() { say '✓' "$1"; }
no() { say '✗' "$1"; bad=$((bad + 1)); }

echo "── 1. 工作流能解析、引用的脚本都在 ──"
python3 - <<'PY'
import os, re, sys

try:
    import yaml
except ImportError:
    print("  ! 没有 pyyaml，跳过 YAML 解析（只做文件存在性检查）")
    yaml = None

size = 0
for wf in sorted(os.listdir(".github/workflows")):
    path = os.path.join(".github/workflows", wf)
    text = open(path, encoding="utf-8").read()
    if yaml:
        try:
            doc = yaml.safe_load(text)
        except Exception as e:
            print(f"  ✗ {wf} 解析失败：{e}")
            size += 1
            continue
        jobs = list((doc or {}).get("jobs", {}))
        print(f"  ✓ {wf}（{len(jobs)} 个 job：{', '.join(jobs)}）")
    # 引用的仓库内文件必须存在
    for m in re.finditer(
        r"((?:scripts|assets|integrations)/[A-Za-z0-9_./-]+\.(?:sh|py|toml|json|md))",
        text,
    ):
        f = m.group(1)
        if not os.path.exists(f):
            print(f"  ✗ {wf} 引用了不存在的文件：{f}")
            size += 1
sys.exit(1 if size else 0)
PY
[ $? -ne 0 ] && bad=$((bad + 1))

echo "── 2. CI 里跑的脚本都可执行 ──"
missing_x=0
# lib.sh 是被 source 的，不需要执行位；其余入口都得能直接跑。
for f in scripts/*-e2e.sh scripts/mac-crosscheck.sh scripts/install-user.sh scripts/install.sh scripts/ci-audit.sh integrations/*.py integrations/*.sh; do
    if [ ! -x "$f" ]; then no "$f 没有执行位"; missing_x=$((missing_x + 1)); fi
done
[ "$missing_x" -eq 0 ] && ok "e2e / 交叉检查 / 安装脚本的执行位齐全"

echo "── 3. 版本号各处一致 ──"
V=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
grep -q "^## $V" CHANGELOG.md && ok "Cargo.toml $V = CHANGELOG 顶部" || no "CHANGELOG 顶部不是 $V"
head -1 docs/conformance.md | grep -qF "$V" && ok "conformance 头 = $V" || no "conformance 头不是 $V"
if [ -f "docs/release-notes-${V%.*}.md" ]; then
    ok "发行说明 docs/release-notes-${V%.*}.md 在"
else
    no "缺发行说明 docs/release-notes-${V%.*}.md"
fi

echo "── 4. 产物与校验和 ──"
if ls dist/*.deb >/dev/null 2>&1; then
    ( cd dist && sha256sum -c ./*.sha256 >/dev/null 2>&1 ) \
        && ok "dist/*.sha256 校验通过" \
        || no "dist 里的 .deb 与 .sha256 不一致"
else
    say '·' "dist 里没有 .deb（还没打包，不算错）"
fi

echo "── 5. 许可证 / 第三方 / 发行说明 ──"
for f in LICENSE THIRD_PARTY.md; do
    [ -f "$f" ] && ok "$f" || no "缺 $f"
done

echo
if [ "$bad" -eq 0 ]; then
    echo "push 前检查：通过"
else
    echo "push 前检查：$bad 项有问题，先修再 push" >&2
    exit 1
fi
