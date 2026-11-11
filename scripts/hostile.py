#!/usr/bin/env python3
"""坏服务器矩阵：验证「宁可失败，也不静默产出坏文件」。

四种服务器行为，断言各不相同（区别本身就是结论）：

  1. /liar/      谎报范围（Content-Range 与请求不符）→ **必须失败**
  2. 同一个源改 --threads 1（单连接逃生口）        → **必须成功且内容正确**
  3. /liar-data/ 谎报数据（头部全对，字节错）       → 无 hash 时**检测不到**（诚实的边界）
  4. 同上但带 --sha256                            → **必须失败并丢弃文件**
  5. /flaky/     每三次请求回一次 503              → **退避重试后成功**

第 3 条不是缺陷，是这类失败的本质：中间层把字节换了但头部自称一致，
任何下载器都无法自证内容 —— 唯一的防线是可信的 hash（--sha256 或 BT 的 piece hash）。

用法：python3 scripts/hostile.py
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
SRC = os.path.join(SCRATCH, "diff-srv")
HOME = os.path.join(SCRATCH, "hostile-home")
DL = os.path.join(SCRATCH, "hostile-dl")
PORT = 8895


def sh(cmd, env=None):
    return subprocess.run(cmd, shell=True, capture_output=True, text=True, env=env)


def sha(path):
    out = sh(f"sha256sum {path!r}").stdout
    return out.split()[0] if out else ""


def run(env, url, extra=""):
    shutil.rmtree(HOME, ignore_errors=True)
    shutil.rmtree(DL, ignore_errors=True)
    os.makedirs(DL, exist_ok=True)
    r = sh(f"{BIN} get {url!r} --to {DL!r} --json {extra}", env=env)
    doc = {}
    for line in r.stdout.splitlines():
        line = line.strip()
        if line.startswith("{"):
            try:
                doc = json.loads(line)
            except json.JSONDecodeError:
                pass
    dest = os.path.join(DL, "mid.bin")
    return doc, dest


def main():
    # 素材自己造：验收脚本不该要求调用方先手工准备一个 4MB 随机文件，
    # 少了就直接说「缺素材」等于把人挡在门外（第一次跑必然缺）。
    os.makedirs(SRC, exist_ok=True)
    src = os.path.join(SRC, "mid.bin")
    if not os.path.exists(src):
        with open(src, "wb") as f:
            f.write(os.urandom(4 * 1024 * 1024))
    want = sha(src)

    env = dict(os.environ)
    env["SKYGET_HOME"] = HOME

    srv = subprocess.Popen(
        [sys.executable, os.path.join(ROOT, "scripts/testserver.py"), str(PORT), SRC],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    time.sleep(1.2)
    B = f"http://127.0.0.1:{PORT}"

    checks = []
    try:
        # 1. 谎报范围就该失败
        doc, dest = run(env, f"{B}/liar/mid.bin")
        checks.append(
            (
                "谎报范围 · 5 线程 → 应当失败",
                doc.get("state") == "failed"
                and ("不是我要的那一段" in doc.get("error", "")),
                doc.get("error", "")[:70],
            )
        )

        # 2. 单连接逃生口：必须成功且内容正确
        doc, dest = run(env, f"{B}/liar/mid.bin", "--threads 1")
        ok = doc.get("state") == "done" and os.path.exists(dest) and sha(dest) == want
        checks.append(("谎报范围 · --threads 1 → 单连接成功且内容正确", ok, doc.get("error", "")[:70]))

        # 3. 谎报数据：没有 hash 时检测不到（记录现状，不判失败）
        doc, dest = run(env, f"{B}/liar-data/mid.bin")
        detected = doc.get("state") == "failed"
        checks.append(
            (
                "谎报数据 · 无 hash → 检测不到（边界，非缺陷）",
                True,
                "已如实记录：下载器无法自证内容" if not detected else "意外地被检测到了",
            )
        )

        # 4. 带 hash 就必须挡住
        doc, dest = run(env, f"{B}/liar-data/mid.bin", f"--sha256 {want}")
        leftovers = [f for f in os.listdir(DL)] if os.path.isdir(DL) else []
        checks.append(
            (
                "谎报数据 · 带 --sha256 → 失败且不落地",
                doc.get("state") == "failed"
                and "校验不符" in doc.get("error", "")
                and not leftovers,
                doc.get("error", "")[:70],
            )
        )

        # 5. 503 闪烁：退避重试后应当成功
        doc, dest = run(env, f"{B}/flaky/mid.bin")
        ok = doc.get("state") == "done" and os.path.exists(dest) and sha(dest) == want
        checks.append(("503 闪烁 · 退避重试 → 成功且内容正确", ok, doc.get("error", "")[:70]))
    finally:
        srv.terminate()
        srv.wait()

    print(f"{'断言':<44} {'结果':<6} 说明")
    print("─" * 96)
    bad = 0
    for name, ok, note in checks:
        if not ok:
            bad += 1
        print(f"{name:<44} {'✓' if ok else '✗':<6} {note}")

    print(f"\n{len(checks) - bad} / {len(checks)} 通过")
    for d in (HOME, DL):
        shutil.rmtree(d, ignore_errors=True)
    return 0 if bad == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
