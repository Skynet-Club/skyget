#!/usr/bin/env python3
"""Range 服务器 + 每连接限速，用来量「并发连接能不能换速度」。

为什么需要它：网盘和 CDN 的真实脾气不是「总带宽小」，而是
「**单连接**被掐在 N KB/s」。这种情况下速度不由带宽决定，由你开了几条连接决定。
skyget 现有实现是 -j 条连接，所以要先把这件事量出来，再谈优化。

用法：
    python3 bench/throttled_server.py --port 8899 --size 67108864 --rate 2097152

  --rate       每连接限速（字节/秒）。0 = 不限
  --total-rate 服务器总限速（字节/秒）。用来验「总带宽封顶时加连接无效」
  --size       内容大小
  日志（stderr）每行一次请求：RANGE <start>-<end> <bytes> <seconds>
  结束（SIGTERM/SIGINT）时向 stdout 打一行 JSON 汇总。
"""

from __future__ import annotations

import argparse
import hashlib
import pathlib
import json
import re
import signal
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ARGS = None
PAYLOAD = b""
LOCK = threading.Lock()
STATS = {"requests": 0, "bytes": 0, "conns": 0, "max_concurrent": 0, "start": time.time()}
ACTIVE = [0]

RANGE_RE = re.compile(r"bytes=(\d*)-(\d*)")


class Limiter:
    """令牌桶；per-conn 限速和全局限速共用一个 sleep 计算。"""

    def __init__(self) -> None:
        self.lock = threading.Lock()
        self.last = time.time()
        self.allowance = 0.0

    def wait(self, nbytes: int) -> float:
        rate = ARGS.total_rate
        if not rate:
            if ARGS.rate:
                return nbytes / ARGS.rate
            return 0.0
        with self.lock:
            now = time.time()
            self.allowance += (now - self.last) * rate
            self.last = now
            self.allowance = min(self.allowance, rate * 2.0)
            self.allowance -= nbytes
            deficit = -self.allowance
        return max(0.0, deficit / rate)


TOTAL = Limiter()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args) -> None:
        pass

    def _parse_range(self) -> tuple[int, int] | None:
        raw = self.headers.get("Range")
        if not raw:
            return None
        m = RANGE_RE.search(raw)
        if not m:
            return None
        start = int(m.group(1)) if m.group(1) else 0
        end = int(m.group(2)) if m.group(2) else len(PAYLOAD) - 1
        return max(0, start), min(end, len(PAYLOAD) - 1)

    def do_HEAD(self) -> None:
        self.send_response(200)
        self.send_header("Content-Length", str(len(PAYLOAD)))
        self.send_header("Accept-Ranges", "bytes")
        self.send_header("Content-Type", "application/octet-stream")
        self.end_headers()

    def _file_body(self) -> bytes | None:
        """--dir 模式下按路径读文件；不在目录内的一律 404。"""
        if not ARGS.dir:
            return None
        rel = self.path.split("?")[0].lstrip("/")
        target = (pathlib.Path(ARGS.dir) / rel).resolve()
        if not str(target).startswith(str(pathlib.Path(ARGS.dir).resolve())) or not target.is_file():
            return None
        return target.read_bytes()

    def do_GET(self) -> None:
        body = self._file_body()
        if body is not None:
            self.send_response(200)
            self._send(body)
            return
        if ARGS.dir:
            self.send_response(404)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        rng = self._parse_range()
        if rng is None:
            start, end = 0, len(PAYLOAD) - 1
            self.send_response(200)
        else:
            start, end = rng
            self.send_response(206)
            self.send_header("Content-Range", f"bytes {start}-{end}/{len(PAYLOAD)}")
        self._send(PAYLOAD[start : end + 1])

    def _send(self, body: bytes) -> None:
        if ARGS.corrupt:
            body = bytes((b ^ 0xFF) for b in body)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Accept-Ranges", "bytes")
        self.send_header("Content-Type", "application/octet-stream")
        self.end_headers()

        with LOCK:
            STATS["requests"] += 1
            STATS["conns"] += 1
        t0 = time.time()
        sent = 0
        step = 64 * 1024
        with LOCK:
            ACTIVE[0] += 1
            if ACTIVE[0] > STATS["max_concurrent"]:
                STATS["max_concurrent"] = ACTIVE[0]
        try:
            while sent < len(body):
                chunk = body[sent : sent + step]
                pause = TOTAL.wait(len(chunk))
                if pause:
                    time.sleep(pause)
                self.wfile.write(chunk)
                self.wfile.flush()
                sent += len(chunk)
        except (BrokenPipeError, ConnectionResetError):
            pass
        finally:
            with LOCK:
                ACTIVE[0] -= 1
        dur = time.time() - t0
        with LOCK:
            STATS["bytes"] += sent
        sys.stderr.write(f"RANGE {self.path} {sent} {dur:.3f}\n")
        sys.stderr.flush()


def _summary(*_args) -> None:
    with LOCK:
        out = dict(STATS)
    out["wall"] = time.time() - out.pop("start")
    out["mbps"] = round(out["bytes"] / 1e6 / max(out["wall"], 1e-9), 2)
    print(json.dumps(out))
    sys.stdout.flush()
    sys.exit(0)


def main() -> None:
    global ARGS, PAYLOAD
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8899)
    ap.add_argument("--size", type=int, default=64 * 1024 * 1024)
    ap.add_argument("--rate", type=int, default=2 * 1024 * 1024, help="每连接字节/秒")
    ap.add_argument("--total-rate", type=int, default=0, help="全局字节/秒")
    ap.add_argument("--dir", default="", help="改发这个目录里的文件（HLS 片段）")
    ap.add_argument("--corrupt", action="store_true", help="长度对但内容错（模拟坏镜像）")
    ARGS = ap.parse_args()

    # 确定性内容：同样的参数在任何机器上得到同一个 sha256
    seed = hashlib.sha256(b"skyget-bench").digest()
    buf = bytearray()
    while len(buf) < ARGS.size:
        seed = hashlib.sha256(seed).digest()
        buf += seed * 32
    PAYLOAD = bytes(buf[: ARGS.size])
    sys.stderr.write(f"SHA256 {hashlib.sha256(PAYLOAD).hexdigest()}\n")
    sys.stderr.flush()

    signal.signal(signal.SIGTERM, _summary)
    signal.signal(signal.SIGINT, _summary)

    srv = ThreadingHTTPServer(("127.0.0.1", ARGS.port), Handler)
    srv.daemon_threads = True
    srv.serve_forever()


if __name__ == "__main__":
    main()
