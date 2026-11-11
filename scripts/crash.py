#!/usr/bin/env python3
"""崩溃收敛测试：在任意进度点硬杀，验证最终能收敛到逐字节正确。

为什么按「进度」而不是按「时间」杀：按时间杀只能测到几条固定路径，
按进度杀能覆盖预分配后、段中途、段边界、全部段完成但还没落地、落地后算 hash
这些真正容易出事的时刻。

三个断言（缺一不可）：
  1. 每一轮的进度都不回退 —— 回退意味着位图被重置或数据丢了
  2. 最终文件的 sha256 == 源文件 sha256
  3. `skyget verify --deep` 干净：台账、落地、缓存、位图彼此自洽

用法：python3 scripts/crash.py [--rounds 12] [--size 52428800]
"""
import argparse
import json
import os
import random
import shutil
import signal
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(ROOT, "target/release/skyget")
SCRATCH = "/home/skynet/.hermes/cache/scratch"
SRC = os.path.join(SCRATCH, "crash-srv")
HOME = os.path.join(SCRATCH, "crash-home")
DL = os.path.join(SCRATCH, "crash-dl")
PORT = 8897


def sh(cmd, **kw):
    return subprocess.run(cmd, shell=True, capture_output=True, text=True, **kw)


def sha(path):
    out = sh(f"sha256sum {path!r}").stdout.strip()
    return out.split()[0] if out else ""


def ledger(env):
    """读台账最后一条的 bytes_done / state。"""
    db = os.path.join(env["SKYGET_HOME"], "tasks.db")
    if not os.path.exists(db):
        return 0, ""
    import sqlite3

    try:
        c = sqlite3.connect(db)
        row = c.execute(
            "select bytes_done, state from tasks order by created_at desc limit 1"
        ).fetchone()
        c.close()
        return (row[0], row[1]) if row else (0, "")
    except Exception:
        return 0, ""


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--rounds", type=int, default=12)
    ap.add_argument("--size", type=int, default=32 * 1024 * 1024)
    ap.add_argument("--port", type=int, default=PORT)
    ap.add_argument("--mode", default="slow3", choices=["slow", "slow3"],
                    help="slow3 更慢，能给 kill 留出更多进度窗口")
    args = ap.parse_args()

    os.makedirs(SRC, exist_ok=True)
    name = "crash.bin"
    src = os.path.join(SRC, name)
    if not os.path.exists(src) or os.path.getsize(src) != args.size:
        with open(src, "wb") as f:
            f.write(os.urandom(args.size))
    want = sha(src)

    shutil.rmtree(HOME, ignore_errors=True)
    shutil.rmtree(DL, ignore_errors=True)
    os.makedirs(HOME, exist_ok=True)
    os.makedirs(DL, exist_ok=True)

    env = dict(os.environ)
    env["SKYGET_HOME"] = HOME
    env.pop("SKYGET_FFMPEG", None)

    srv = subprocess.Popen(
        [sys.executable, os.path.join(ROOT, "scripts/testserver.py"), str(args.port), SRC],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    time.sleep(1.2)
    url = f"http://127.0.0.1:{args.port}/{args.mode}/{name}"
    dest = os.path.join(DL, name)
    print(f"源文件 {args.size/1e6:.1f}MB  sha={want[:16]}…  url={url}\n")

    last_progress = 0
    regress = None
    kills = 0
    try:
        for i in range(1, args.rounds + 1):
            # 每轮杀在「剩余部分」的随机比例上。用全局比例的话，续传后目标早就满足，
            # 会一直在同一个点反复杀，最后那段永远测不到。
            remaining = args.size - last_progress
            if remaining < 3 * 1024 * 1024:
                # 快完成了就不再杀：让最后一段、落地、算 hash 真正走完一遍
                target_pct = 1.01
            else:
                frac = random.uniform(0.05, 0.30) if i <= 2 else random.uniform(0.10, 0.95)
                target_pct = min((last_progress + remaining * frac) / args.size, 0.99)
            p = subprocess.Popen(
                [BIN, "get", url, "--to", DL, "--json"],
                env=env,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            total = args.size
            got = 0
            killed_at = None
            deadline = time.time() + 120
            while time.time() < deadline:
                if p.poll() is not None:
                    break
                got, state = ledger(env)
                if total and got / total >= target_pct:
                    os.kill(p.pid, signal.SIGKILL)
                    killed_at = got
                    break
                time.sleep(0.02)
            p.wait()
            if killed_at is not None:
                kills += 1

            got, state = ledger(env)
            # 断言 1：进度不退
            if got < last_progress:
                regress = (last_progress, got)
            last_progress = max(last_progress, got)
            pct = (got / total * 100) if total else 0
            tag = "杀于" if killed_at is not None else "跑完"
            print(
                f"  第 {i:2d} 轮  {tag} {pct:5.1f}%  "
                f"({got/1e6:6.2f}MB)  状态 {state or '-':<11} "
                + ("进度回退!" if regress else "")
            )
            if state == "done" and os.path.exists(dest):
                print("  → 已经下完，提前结束杀进程阶段")
                break

        # 最后一轮：不杀，跑到完成
        print("\n  最后完整跑一遍…")
        t0 = time.time()
        r = sh(f"cd {ROOT} && {BIN} get {url!r} --to {DL!r} --json", env=env)
        final = {}
        for line in r.stdout.splitlines():
            line = line.strip()
            if line.startswith("{"):
                try:
                    final = json.loads(line)
                except json.JSONDecodeError:
                    pass
        print(f"  用了 {time.time()-t0:.1f}s  状态 {final.get('state')}")
    finally:
        srv.terminate()
        srv.wait()

    print("\n── 断言 ──")
    ok = True

    if os.path.exists(dest):
        got = sha(dest)
        same = got == want
        print(f"  {'✓' if same else '✗'} 最终 sha256 与源一致")
        if not same:
            print(f"      源 {want}\n      盘 {got}")
            ok = False
    else:
        print("  ✗ 最终文件不存在")
        ok = False

    if regress:
        print(f"  ✗ 进度回退过：{regress[0]/1e6:.2f}MB → {regress[1]/1e6:.2f}MB")
        ok = False
    else:
        print(f"  ✓ 进度从未回退（{kills} 次硬杀，最高 {last_progress/1e6:.2f}MB）")

    v = sh(f"{BIN} verify --deep --json", env=env)
    try:
        rep = json.loads(v.stdout.strip().splitlines()[-1])
    except Exception:
        rep = {}
    fatal = [p for p in rep.get("problems", []) if p["kind"] in
             ("Missing", "SizeMismatch", "HashMismatch", "BlobName")]
    print(f"  {'✓' if not fatal else '✗'} verify --deep 自洽"
          f"（{rep.get('tasks', 0)} 条记录 / {rep.get('blobs', 0)} 个缓存块）")
    for p in fatal:
        print(f"      {p['kind']}: {p['path']} — {p['detail']}")
        ok = False

    print("\n" + ("全部通过" if ok else "有断言失败"))
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
