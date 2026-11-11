#!/usr/bin/env python3
"""信任级别：hash 从哪来，决定了这次下载能防住什么。

五条断言，重点是第 2 条和第 5 条 —— 它们证明「没有可信 hash 时，我们能做的只有记录变化」。

  1. 首次下载（没给 hash）        → trust=tofu-new，记下 hash
  2. 同一地址的内容被换了          → 拒绝，并明确说「内容变了」而不是「校验失败」
  3. 加 --accept-change           → 接受更新，重新记录
  4. 用发布方的 .sha256 文件      → trust=indexed
  5. 校验文件里的 hash 是错的      → 拒绝并丢弃

TOFU（trust on first use）能挡住的是「同一个地址悄悄换了东西」，挡不住「第一次给的就是假的」——
后者只能靠签名或独立索引。这里不假装比这更强。

用法：python3 scripts/trust.py
"""
import json
import os
import shutil
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(ROOT, "target/release/skyget")
SCRATCH = "/home/skynet/.hermes/cache/scratch"
SRC = os.path.join(SCRATCH, "trust-srv")
HOME = os.path.join(SCRATCH, "trust-home")
DL = os.path.join(SCRATCH, "trust-dl")
PORT = 8894


def sh(cmd, env=None):
    return subprocess.run(cmd, shell=True, capture_output=True, text=True, env=env)


def run(env, url, extra=""):
    r = sh(f"{BIN} get {url!r} --to {DL!r} --json {extra}", env=env)
    doc = {}
    for line in r.stdout.splitlines():
        line = line.strip()
        if line.startswith("{"):
            try:
                doc = json.loads(line)
            except json.JSONDecodeError:
                pass
    return doc


def main():
    os.makedirs(SRC, exist_ok=True)
    target = os.path.join(SRC, "t.bin")
    # 每轮都从干净内容开始，避免上一轮的结果影响判断
    with open(target, "wb") as f:
        f.write(os.urandom(3_000_000))
    for extra in ("t.bin.sha256", "bad.sha256"):
        p = os.path.join(SRC, extra)
        if os.path.exists(p):
            os.remove(p)

    shutil.rmtree(HOME, ignore_errors=True)
    shutil.rmtree(DL, ignore_errors=True)
    os.makedirs(DL, exist_ok=True)
    env = dict(os.environ)
    env["SKYGET_HOME"] = HOME

    srv = subprocess.Popen(
        [sys.executable, os.path.join(ROOT, "scripts/testserver.py"), str(PORT), SRC],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    time.sleep(1.2)
    U = f"http://127.0.0.1:{PORT}/f/t.bin"

    checks = []
    try:
        d = run(env, U, "--name a.bin")
        checks.append(
            ("首次下载（没给 hash）→ tofu-new", d.get("state") == "done" and d.get("trust") == "tofu-new",
             d.get("trust", "") or d.get("error", "")[:50])
        )

        with open(target, "ab") as f:
            f.write(b"tampered")
        d = run(env, U, "--name b.bin")
        ok = d.get("state") == "failed" and "内容变了" in d.get("error", "")
        checks.append(
            ("地址内容被换 → 拒绝，且说清是「变了」", ok, d.get("error", "")[:60])
        )

        d = run(env, U, "--name b.bin --accept-change")
        checks.append(
            ("--accept-change → 接受更新并重新记录", d.get("state") == "done" and d.get("trust") == "tofu-new",
             d.get("trust", "") or d.get("error", "")[:50])
        )

        want = sh(f"sha256sum {target!r}").stdout.split()[0]
        with open(os.path.join(SRC, "t.bin.sha256"), "w") as f:
            f.write(f"{want}  t.bin\n")
        d = run(env, U, f"--name c.bin --sha256-url {U}.sha256")
        checks.append(
            ("发布方校验文件 → indexed", d.get("state") == "done" and d.get("trust") == "indexed",
             d.get("trust", "") or d.get("error", "")[:50])
        )

        with open(os.path.join(SRC, "bad.sha256"), "w") as f:
            f.write("0" * 64 + "  t.bin\n")
        d = run(env, U, f"--name d.bin --sha256-url http://127.0.0.1:{PORT}/f/bad.sha256")
        left = os.path.exists(os.path.join(DL, "d.bin"))
        ok = d.get("state") == "failed" and "校验不符" in d.get("error", "") and not left
        checks.append(
            ("错的校验文件 → 拒绝且不落地", ok, d.get("error", "")[:60])
        )
    finally:
        srv.terminate()
        srv.wait()

    print(f"{'断言':<36} {'结果':<6} 说明")
    print("─" * 88)
    bad = 0
    for name, ok, note in checks:
        if not ok:
            bad += 1
        print(f"{name:<36} {'✓' if ok else '✗':<6} {note}")
    print(f"\n{len(checks) - bad} / {len(checks)} 通过")
    for d in (HOME, DL):
        shutil.rmtree(d, ignore_errors=True)
    return 0 if bad == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
