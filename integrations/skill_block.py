#!/usr/bin/env python3
"""把 skill/SKILL.md 的正文取出来，做成 AGENTS.md / SOUL.md 里那段常驻块。

为什么单独一个文件而不是 heredoc：常驻块的内容必须和 skill **同一份**。
以前它是 install.sh 里手抄的一段，工具升级后就落后了（实测 check/events/pause 全 0 次）。
"""
import sys

text = open(sys.argv[1], encoding="utf-8").read()
end = text.find("\n---", 3)
body = text[end + 4 :].strip() if end > 0 else text
lines = body.splitlines()
if lines and lines[0].startswith("# "):
    lines = lines[1:]
print("\n".join(lines).strip())
