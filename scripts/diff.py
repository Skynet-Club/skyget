#!/usr/bin/env python3
"""差分测试 + 格式裁判。

两件事分开，但都需要：

**差分**：同一个地址，用 curl 独立下一份，与 skyget 下的逐字节比对。
这证明的不是「我认为下对了」，而是「一个独立实现给出了同样的字节」。

**格式裁判**：下回来的东西交给格式自己去判断 —— 视频让 ffprobe 读时长、
压缩包让 tar 列目录、JSON 让解析器加载、PNG 校验魔数和尺寸、可执行文件让它自己报版本。
这些判据都不依赖我写的代码。

用法：python3 scripts/diff.py [--keep]
"""
import argparse
import hashlib
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
HOME = os.path.join(SCRATCH, "diff-home")
DL = os.path.join(SCRATCH, "diff-dl")
REF = os.path.join(SCRATCH, "diff-ref")
PORT = 8898

# 真实外网：小体积、支持 Range、有官方校验
REMOTE = [
    # 国内 CDN，稳定：一个真实的 npm 包（还能顺手校验里面的 package.json）
    ("https://registry.npmmirror.com/typescript/-/typescript-5.4.5.tgz", "npm", "npm 包（国内 CDN）"),
    (
        "https://github.com/BurntSushi/ripgrep/releases/download/14.1.1/ripgrep-14.1.1-x86_64-unknown-linux-musl.tar.gz",
        "tar.gz",
        "ripgrep 发布包（GitHub）",
    ),
]

# 本地矩阵：(路径, 说明, 裁判类型)
LOCAL = [
    ("/f/plain.bin", "支持分段（Range）", None),
    ("/norange/plain.bin", "不支持分段（永远 200）", None),
    ("/redirect/plain.bin", "302 跳转后下载", None),
    ("/cd/small.bin", "Content-Disposition 中文名", None),
    ("/f/tiny.tar.gz", "gzip 压缩包", "tar.gz"),
    ("/f/tiny.mp4", "视频容器", "mp4"),
    ("/f/tiny.png", "PNG 图片", "png"),
    ("/f/tiny.json", "JSON 文本", "json"),
]


def sh(cmd, **kw):
    return subprocess.run(cmd, shell=True, capture_output=True, text=True, **kw)


def sha(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for b in iter(lambda: f.read(1 << 20), b""):
            h.update(b)
    return h.hexdigest()


def judge(kind, path):
    """让格式自己发言。返回 (ok, 说明)。"""
    if kind is None:
        return True, ""
    if kind == "tar.gz":
        r = sh(f"tar -tzf {path!r}")
        n = len([x for x in r.stdout.splitlines() if x.strip()])
        return (r.returncode == 0 and n > 0), f"tar 列出 {n} 个条目"
    if kind == "mp4":
        r = sh(
            f"ffprobe -v error -show_entries format=duration,format_name "
            f"-of default=nw=1 {path!r}"
        )
        ok = r.returncode == 0 and "duration=" in r.stdout
        dur = ""
        for line in r.stdout.splitlines():
            if line.startswith("duration="):
                dur = line.split("=", 1)[1]
        return ok, f"ffprobe 读出时长 {dur}s"
    if kind == "png":
        with open(path, "rb") as f:
            head = f.read(33)
        ok = head[:8] == b"\x89PNG\r\n\x1a\n" and head[12:16] == b"IHDR"
        w = int.from_bytes(head[16:20], "big")
        h = int.from_bytes(head[20:24], "big")
        return ok, f"PNG 头合法，{w}×{h}"
    if kind == "json":
        r = sh(f"python3 -m json.tool {path!r}")
        return r.returncode == 0, "JSON 可解析"
    if kind == "npm":
        # 真正的裁判：把包解开，让 npm 自己的 package.json 说明它是什么
        r = sh(f"tar -xzf {path!r} -O package/package.json")
        if r.returncode != 0:
            return False, "解不开"
        try:
            doc = json.loads(r.stdout)
        except json.JSONDecodeError:
            return False, "package.json 无法解析"
        return True, f"{doc.get('name')}@{doc.get('version')}"
    if kind == "text":
        with open(path, "r", errors="replace") as f:
            first = f.readline().strip()
        return bool(first), f"首行：{first[:40]}"
    return True, ""


def ensure_fixtures():
    """素材自己造：验收脚本不该要求调用方先手工准备一堆文件。

    之前缺素材就打印一句「见 README 的验收一节」—— 第一次跑必然缺，
    等于把人挡在门外，也让这个脚本进不了 CI。
    """
    import tarfile

    os.makedirs(SRC, exist_ok=True)
    want = {
        "plain.bin": lambda: os.urandom(1024 * 1024),
        "small.bin": lambda: os.urandom(300 * 1024),
        "tiny.json": lambda: b'{"name":"skyget","ok":true}\n',
        # 1x1 透明 PNG（合法头 + IHDR）
        "tiny.png": lambda: bytes.fromhex(
            "89504e470d0a1a0a0000000d49484452000000010000000108060000001f15c4"
            "890000000a49444154789c6300010000050001a5f645400000000049454e44ae426082"
        ),
    }
    for name, make in want.items():
        path = os.path.join(SRC, name)
        if not os.path.exists(path):
            with open(path, "wb") as f:
                f.write(make())
    # 视频：有 ffmpeg 就现造一个 0.2 秒的纯色片（没有就不列这条用例）
    mp4 = os.path.join(SRC, "tiny.mp4")
    if not os.path.exists(mp4) and shutil.which("ffmpeg"):
        subprocess.run(
            [
                "ffmpeg", "-y", "-loglevel", "error",
                "-f", "lavfi", "-i", "color=c=0x336699:s=320x240:d=0.2",
                "-pix_fmt", "yuv420p", mp4,
            ],
            capture_output=True,
        )

    # tar.gz：装一个小文件
    tgz = os.path.join(SRC, "tiny.tar.gz")
    if not os.path.exists(tgz):
        payload = os.path.join(SRC, "_tiny.txt")
        with open(payload, "wb") as f:
            f.write(b"hello from skyget diff test\n")
        with tarfile.open(tgz, "w:gz") as tf:
            tf.add(payload, arcname="tiny.txt")
        os.remove(payload)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--keep", action="store_true", help="保留下载产物")
    ap.add_argument(
        "--local",
        action="store_true",
        help="只跑本地用例（不碰外网）：CI 用这个",
    )
    args = ap.parse_args()

    for d in (HOME, DL, REF):
        shutil.rmtree(d, ignore_errors=True)
        os.makedirs(d, exist_ok=True)
    ensure_fixtures()

    env = dict(os.environ)
    env["SKYGET_HOME"] = HOME

    srv = subprocess.Popen(
        [sys.executable, os.path.join(ROOT, "scripts/testserver.py"), str(PORT), SRC],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    time.sleep(1.2)

    local = list(LOCAL)
    # 素材不存在的用例直接跳过并说明 —— 假失败比不测更糟。
    missing = [c[1] for c in local if not os.path.exists(os.path.join(SRC, os.path.basename(c[0])))]
    if missing:
        print(f"跳过 {len(missing)} 条（素材没生成）：{'、'.join(missing)}")
    local = [c for c in local if c[1] not in missing]
    cases = [(f"http://127.0.0.1:{PORT}{p}", desc, judge_kind) for p, desc, judge_kind in local]
    if not args.local:
        cases += [(u, desc, kind) for u, kind, desc in REMOTE]

    rows = []
    try:
        for url, desc, kind in cases:
            name = os.path.basename(url.split("?")[0]) or "out"
            ref = os.path.join(REF, name)

            # 参照实现：curl。--location 跟随跳转，和我们的行为对齐
            c = sh(f"curl -sS --location --max-time 120 -o {ref!r} {url!r}")
            if c.returncode != 0 or not os.path.exists(ref):
                # 参照实现取不到：先看我们能不能取到。两边都不通是环境问题（跳过），
                # 只有我们通才算有信息量。
                r = sh(f"{BIN} get {url!r} --to {DL!r} --json", env=env)
                ours = "done" in r.stdout
                rows.append(
                    (
                        desc,
                        "跳过：网络不可达（两边都没取到）",
                        "" if ours else "环境",
                        "" if ours else "不计入失败",
                        "",
                    )
                )
                continue

            r = sh(f"{BIN} get {url!r} --to {DL!r} --json", env=env)
            got = {}
            for line in r.stdout.splitlines():
                line = line.strip()
                if line.startswith("{"):
                    try:
                        got = json.loads(line)
                    except json.JSONDecodeError:
                        pass
            path = got.get("path", "")
            if got.get("state") != "done" or not path or not os.path.exists(path):
                rows.append((desc, f"skyget 失败：{got.get('error', '')[:40]}", "", "", ""))
                continue

            a, b = sha(ref), sha(path)
            same = a == b
            ok_j, note = judge(kind, path)
            rows.append(
                (
                    desc,
                    "字节一致" if same else f"不一致（curl {a[:8]} / 我们 {b[:8]}）",
                    "格式合格" if ok_j else "格式不合格",
                    note,
                    got.get("sha256", "")[:12],
                )
            )
    finally:
        srv.terminate()
        srv.wait()

    print(f"{'用例':<26} {'结果':<34} {'格式裁判':<12} 说明")
    print("─" * 100)
    bad = 0
    skipped = 0
    for desc, result, fmt, note, h in rows:
        if result.startswith("跳过"):
            skipped += 1
            mark = "–"
        else:
            ok = result.startswith("字节一致") and (fmt in ("格式合格", ""))
            if not ok:
                bad += 1
            mark = "✓" if ok else "✗"
        print(f"{mark} {desc:<24} {result:<34} {fmt:<12} {note}")

    print()
    print(f"{len(rows) - bad - skipped} / {len(rows) - skipped} 通过"
          + (f"，{skipped} 个因网络跳过" if skipped else ""))
    if not args.keep:
        for d in (DL, REF):
            shutil.rmtree(d, ignore_errors=True)
    return 0 if bad == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
