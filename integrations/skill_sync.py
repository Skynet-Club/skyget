#!/usr/bin/env python3
"""把一个通用 skill 装到各宿主 —— **不生成方言，只搬运同一份字节**。

设计原则（这条比脚本本身重要）：
    通用 skill = 一个文件。宿主的差别只在"放在哪、怎么被索引"，不在内容。

所以这里**没有** per-host 重写：`skill/SKILL.md` 原样复制到每个宿主的位置，
`skill.json`（1Agent 注册用）和 AGENTS.md 常驻块都从**同一个文件**派生。

唯一需要迁就的硬约束是 1Agent 的 `description` 1..=60 字 —— 它写进了那个文件的
front-matter（短描述），对 Claude/pi/Hermes 同样合法（额外字段它们会忽略）。
所以一份文件同时满足所有宿主，不需要为谁单独裁剪。

    python3 integrations/skill_sync.py            # 安装/刷新
    python3 integrations/skill_sync.py --check    # 只比对（CI；漂移就 exit 1）
    python3 integrations/skill_sync.py --print    # 打印将要写的内容

安装目标（每个都是同一份字节）：
    ~/.claude/skills/skyget-transfer/              Claude Code
    ~/.hermes/skills/skyget-transfer/              Hermes
    ~/.local/share/skyget/skill/skyget-transfer/   其它宿主 / 手动取用
（OpenCode 与 pi 走各自配置里的 `skills: [<目录>]`，指向仓库里这一份；
 Codex / Copilot / 任意宿主走 ~/AGENTS.md 的常驻块，由 install.sh 写入。）
"""
from __future__ import annotations

import hashlib
import json
import re
import shutil
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
SRC = REPO / "skill/SKILL.md"
REFERENCE = REPO / "skill/REFERENCE.md"

# 主流宿主各自的位置 + 一个"自取"目录（没被显式接进来的宿主从这里拿）。
# 一个文件、一份字节；不做任何 per-host 重写。
TARGETS = [
    Path.home() / ".claude/skills/skyget-transfer",      # Claude Code
    Path.home() / ".hermes/skills/skyget-transfer",      # Hermes
    Path.home() / ".local/share/skyget/skill/skyget-transfer",  # 其它宿主 / 手动取用
]

# 1Agent 的 scan_text 拒收清单（照抄它 src/skill.rs）
FORBIDDEN = [
    "begin private key",
    "ignore previous instructions",
    "ignore all previous",
    "-----begin rsa private key-----",
    "aws_secret_access_key",
]
MAX_BODY_LINES = 90

fail = 0


def bad(msg: str) -> None:
    global fail
    fail += 1
    print(f"  ✗ {msg}", file=sys.stderr)


def split(text: str) -> tuple[dict[str, str], str]:
    if not text.startswith("---\n"):
        raise SystemExit("SKILL.md 缺 front-matter")
    end = text.find("\n---", 3)
    if end < 0:
        raise SystemExit("front-matter 没有闭合")
    meta: dict[str, str] = {}
    for line in text[4:end].splitlines():
        if ":" in line:
            k, v = line.split(":", 1)
            meta[k.strip()] = v.strip()
    body = text[end + 4 :].strip("\n").strip()
    return meta, body


def secret_like(value: str) -> bool:
    for m in re.finditer(r"sk-", value):
        rest = value[m.end():]
        if rest.startswith(("ant-", "proj-", "live-")):
            n = len(re.match(r"[A-Za-z0-9_-]{0,}", rest).group(0))
            if n >= 20:
                return True
        n = len(re.match(r"[A-Za-z0-9_]{0,}", rest).group(0))
        if n >= 16 and any(c.isdigit() for c in rest[:n]):
            return True
    return False


def scan_text(label: str, value: str) -> None:
    for ch in value:
        if not ch.isprintable() and ch not in "\n\r\t":
            bad(f"{label} 含控制字符")
            return
    low = value.lower()
    for marker in FORBIDDEN:
        if marker in low:
            bad(f"{label} 含 1Agent 拒收的用语：{marker}")
            return
    if secret_like(low):
        bad(f"{label} 含看起来像密钥的 token")


def parse_list(value: str) -> list[str]:
    return [x.strip() for x in value.strip("[]").split(",") if x.strip()]


def validate(meta: dict[str, str], body: str) -> None:
    name = meta.get("name", "")
    desc = meta.get("description", "")

    # 通用形态：name + description 是所有主流宿主都认的两个键。
    # description 就是"该不该加载我"的触发语 —— 太短等于没写，太长会被截断。
    if not re.fullmatch(r"[a-z0-9][a-z0-9-]*", name or ""):
        bad(f"name 不合法：{name!r}")
    if not 80 <= len(desc) <= 1024:
        bad(f"description 是 {len(desc)} 字，应该 80..=1024（它是触发语，不是摘要）")
    for key in meta:
        if key not in ("name", "description", "license"):
            bad(f"front-matter 里的 {key} 不是主流宿主通用键，去掉（放进正文）")

    scan_text("description", desc)
    scan_text("body", body)
    lines = len(body.splitlines())
    if lines > MAX_BODY_LINES:
        bad(f"正文 {lines} 行，超过 {MAX_BODY_LINES}（细节该进 REFERENCE.md）")


def build() -> dict[Path, bytes]:
    text = SRC.read_text(encoding="utf-8")
    meta, body = split(text)
    validate(meta, body)
    if fail:
        return {}

    files: dict[Path, bytes] = {}
    for d in TARGETS:
        files[d / "SKILL.md"] = text.encode()          # 同一份字节，不分方言
        files[d / "REFERENCE.md"] = REFERENCE.read_bytes()
    return files


def main() -> int:
    mode = sys.argv[1] if len(sys.argv) > 1 else "--write"
    if mode not in ("--write", "--check", "--print"):
        print(__doc__, file=sys.stderr)
        return 2
    files = build()
    if fail:
        print(f"\n{SRC} 不合规：{fail} 处（先修源文件）", file=sys.stderr)
        return 1

    if mode == "--print":
        for p, data in files.items():
            print(f"===== {p} =====")
            print(data.decode())
        return 0

    drift = []
    for path, data in files.items():
        # 软链场景：目标就是源文件时什么都不用做（零漂移）。
        # 宿主目录本身是软链（Claude/Hermes 由 install.sh 建）时同理。
        if path.is_symlink() or path.parent.is_symlink():
            real = path.resolve()
            if real.exists() and real.read_bytes() == data:
                continue
        old = path.read_bytes() if path.exists() else None
        if old == data:
            continue
        drift.append(path)
        if mode == "--write":
            path.parent.mkdir(parents=True, exist_ok=True)
            if path.is_symlink():
                path.unlink()
            path.write_bytes(data)

    if mode == "--check":
        # CI 从干净 checkout 跑时，宿主安装目录本来就不存在。缺失表示“没装”，
        # 不是漂移；只要仓库内那份软链仍指向真源即可。
        installed_drift = [p for p in drift if p.exists()]
        repo = [
            REPO / ".claude/skills/skyget-transfer/SKILL.md",
            REPO / ".claude/skills/skyget-transfer/REFERENCE.md",
        ]
        for path, data in zip(repo, (SRC.read_bytes(), REFERENCE.read_bytes())):
            if not path.exists() or path.read_bytes() != data:
                installed_drift.append(path)
        if installed_drift:
            print("skill 副本与源不一致（跑 `python3 integrations/skill_sync.py`）：", file=sys.stderr)
            for p in installed_drift:
                print(f"  ✗ {p}", file=sys.stderr)
            return 1
        missing = len(drift) - len([p for p in drift if p.exists()])
        suffix = f"；{missing} 个宿主未安装" if missing else ""
        print(f"  ✓ 仓库视图与已安装副本和 skill/SKILL.md 一致（同一份字节）{suffix}")
        return 0

    for p in drift:
        print(f"  ✓ 写入 {p}")
    if not drift:
        print("  · 已是最新")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
