#!/usr/bin/env python3
"""调度器压测：两个 agent 各 3 个慢任务，验证并发上限与按来源公平。

断言：
  1. 同时在跑（state=running）的任务不超过 SKYGET_MAX_JOBS
  2. 单个 source 同时在跑不超过 SKYGET_MAX_PER_AGENT
  3. 6 个任务最终全部 done，落地 sha 正确
  4. 队列计数在提交后出现过 >0 又归零（说明真的排过队）

用法：python3 scripts/scheduler-loadtest.py
依赖：target/debug/skyget、scripts/testserver.py（本机，不联网）
"""
import hashlib
import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(ROOT, "target/debug/skyget")
SRV = os.path.join(ROOT, "scripts/testserver.py")
MAX_JOBS = 2
MAX_PER_AGENT = 1
N_PER_AGENT = 3


def start(cmd, env):
    return subprocess.Popen(
        cmd, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
    )


def wait_health(base, timeout=10):
    end = time.time() + timeout
    while time.time() < end:
        try:
            with urllib.request.urlopen(base + "/health", timeout=0.5) as r:
                if r.status == 200:
                    return
        except Exception:
            pass
        time.sleep(0.1)
    raise SystemExit("daemon 没起来")


def req(base, token, method, path, body=None):
    data = json.dumps(body).encode() if body is not None else None
    r = urllib.request.Request(base + path, data=data, method=method)
    r.add_header("Authorization", "Bearer " + token)
    if data is not None:
        r.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(r, timeout=10) as resp:
            return resp.status, json.loads(resp.read() or b"{}")
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"{}")


def main():
    tmp = tempfile.mkdtemp(prefix="skyget-load.")
    srv_dir = os.path.join(tmp, "srv")
    home = os.path.join(tmp, "home")
    os.makedirs(srv_dir)
    os.makedirs(home)
    shas = []
    for i in range(6):
        p = os.path.join(srv_dir, f"f{i}.bin")
        with open(p, "wb") as f:
            f.write(os.urandom(2 * 1024 * 1024))
        shas.append(hashlib.sha256(open(p, "rb").read()).hexdigest())

    srv_port, api_port = 18881, 19881
    token = "load-test"
    base = f"http://127.0.0.1:{api_port}"
    env = dict(os.environ)
    env.update(
        {
            "SKYGET_HOME": home,
            "SKYGET_ALLOW_PRIVATE": "1",
            "SKYGET_API_TOKEN": token,
            "SKYGET_MAX_JOBS": str(MAX_JOBS),
            "SKYGET_MAX_QUEUED": "64",
            "SKYGET_MAX_PER_AGENT": str(MAX_PER_AGENT),
        }
    )
    srv = start([sys.executable, SRV, str(srv_port), srv_dir], env)
    daemon = start(
        [BIN, "serve", "--port", str(api_port)], env
    )
    try:
        wait_health(base)
        ids = []
        for i in range(6):
            source = "agent:a" if i < N_PER_AGENT else "agent:b"
            status, v = req(
                base,
                token,
                "POST",
                "/jobs",
                {
                    "url": f"http://127.0.0.1:{srv_port}/slow3/f{i}.bin",
                    "dest": os.path.join(tmp, f"out{i}"),
                    "sha256": shas[i],
                    "source": source,
                },
            )
            if status not in (200, 202):
                raise SystemExit(f"提交失败 {status}: {v}")
            ids.append(v["id"])

        peak = 0
        peak_by_source = {}
        saw_queue = False
        deadline = time.time() + 120
        while time.time() < deadline:
            status, jobs = req(base, token, "GET", "/jobs?limit=100")
            if status != 200:
                raise SystemExit(f"拉台账失败 {status}: {jobs}")
            running = [j for j in jobs if j["state"] == "running"]
            queued = [j for j in jobs if j["state"] == "pending"]
            peak = max(peak, len(running))
            counts = {}
            for j in running:
                counts[j["source"]] = counts.get(j["source"], 0) + 1
                peak_by_source[j["source"]] = max(
                    peak_by_source.get(j["source"], 0), counts[j["source"]]
                )
            if queued:
                saw_queue = True
            if all(j["state"] in ("done", "failed") for j in jobs) and jobs:
                break
            time.sleep(0.15)

        status, jobs = req(base, token, "GET", "/jobs?limit=100")
        assert status == 200
        bad = [j for j in jobs if j["state"] != "done"]
        if bad:
            raise SystemExit(f"有任务没完成：{bad}")
        for i, j in enumerate(sorted(jobs, key=lambda x: x["path"])):
            p = j["path"]
            if not os.path.isfile(p):
                raise SystemExit(f"落地文件不存在：{p}")
            got = hashlib.sha256(open(p, "rb").read()).hexdigest()
            if got != j["sha256"]:
                raise SystemExit(f"落地 sha 不对：{p}")

        print(f"并发峰值 {peak}（上限 {MAX_JOBS}）")
        print(f"按来源峰值 {peak_by_source}（单 agent 上限 {MAX_PER_AGENT}）")
        print(f"观测到排队：{saw_queue}")
        assert peak <= MAX_JOBS, f"并发超限：{peak}"
        assert all(v <= MAX_PER_AGENT for v in peak_by_source.values()), peak_by_source
        assert saw_queue, "6 个慢任务从未排队，说明并发/队列根本没起作用"
        print("scheduler-loadtest: 通过")
    finally:
        for p in (daemon, srv):
            try:
                p.send_signal(signal.SIGTERM)
            except Exception:
                pass
        time.sleep(0.3)
        shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    main()
