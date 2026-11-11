#!/usr/bin/env python3
"""并发 soak：40 个任务 / 4 worker，立刻取消 10 个，其余必须完成且 sha 正确。
用法：soak.py <api> <token> <srv-url> <sha> <dest-root>"""
import concurrent.futures as cf
import json
import os
import sys
import time
import urllib.error
import urllib.request

API, TOKEN, SRV, SHA, DEST = sys.argv[1:6]
N = 40
CANCEL = 10


def call(method, path, body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(
        API + path, data=data, method=method,
        headers={"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"},
    )
    try:
        with urllib.request.urlopen(req, timeout=20) as r:
            return r.status, json.loads(r.read() or b"{}")
    except urllib.error.HTTPError as e:
        try:
            return e.code, json.loads(e.read() or b"{}")
        except Exception:
            return e.code, {}


def submit(i):
    code, v = call("POST", "/jobs", {
        "url": f"{SRV}/slow3/f{i}.bin",
        "dest": os.path.join(DEST, f"out{i}"),
        "sha256": SHA,
        "source": "agent:soak",
        "idempotency_key": f"soak-{i}",
    })
    return i, code, v


with cf.ThreadPoolExecutor(max_workers=N) as ex:
    results = list(ex.map(submit, range(N)))

accepted = [(i, v["id"]) for i, code, v in results if code in (200, 202) and v.get("id")]
assert len(accepted) == N, f"只收下 {len(accepted)}/{N}: {results[:3]}"
print(f"提交 {N} 条，重复提交同一 idempotency_key 检查…")
dups = [submit(0) for _ in range(3)]
assert all(code == 200 and v.get("deduped") for _, code, v in dups), dups
print("  幂等 OK：重复提交返回同一条")

# 让一批真的跑起来，再取消前 10 条
# 立刻取消，抢在慢速源完成之前
for i, jid in accepted[:CANCEL]:
    code, v = call("POST", f"/jobs/{jid}/cancel", {})
    assert code == 200 and v.get("state") == "cancelled", (i, code, v)
print(f"取消 {CANCEL} 条")

# 等剩余完成
deadline = time.time() + 60
pending = {jid for _, jid in accepted[CANCEL:]}
while pending and time.time() < deadline:
    for jid in list(pending):
        code, v = call("GET", f"/jobs/{jid}")
        if code == 404:
            raise SystemExit(f"未取消的任务不见了：{jid}")
        if v.get("state") in ("done", "failed", "interrupted"):
            pending.discard(jid)
            if v.get("state") != "done":
                raise SystemExit(f"任务未完成：{jid} {v}")
    time.sleep(0.1)
assert not pending, f"超时未完成 {len(pending)} 条"

# 文件与台账
landed = 0
for i in range(N):
    p = os.path.join(DEST, f"out{i}", f"f{i}.bin")
    if i < CANCEL:
        # 取消与完成可能竞速：取消语义是“停止 + 删记录”，已完成的文件允许在。
        landed += os.path.exists(p)
    else:
        assert os.path.exists(p), f"完成任务没落地：{p}"
        import hashlib
        h = hashlib.sha256(open(p, "rb").read()).hexdigest()
        assert h == SHA, f"sha 不对：{p}"
print(f"文件检查 OK：{N-CANCEL} 个完成任务落地正确；{CANCEL} 个取消中 {landed} 个在取消前已完成")

# 取消的记录必须 404；台账里没有 running 幽灵
for _, jid in accepted[:CANCEL]:
    code, _ = call("GET", f"/jobs/{jid}")
    assert code == 404, f"取消的记录没删：{jid}"
code, jobs = call("GET", "/jobs?limit=500")
assert code == 200
assert not [j for j in jobs if j["state"] in ("running", "pending")], "台账里还有 running/pending 幽灵"
print(f"台账 OK：{len(jobs)} 条，没有 running/pending 幽灵")
print("soak: PASS")
