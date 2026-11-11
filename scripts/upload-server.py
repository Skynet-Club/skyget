#!/usr/bin/env python3
"""最小的上传测试服务器：HTTP PUT + tus 1.0（仅测试用，不联网）。

  PUT    /put/<name>     整文件上传（可带 Authorization: Bearer）
  POST   /tus            创建 tus 上传，返回 Location
  HEAD   /tus/<id>       查 Upload-Offset
  PATCH  /tus/<id>       按 Upload-Offset 追加
  GET    /tus/<id>       取回文件（测试校验用）

用法：python3 scripts/upload-server.py <root-dir> <port> [token]
"""
import os
import secrets
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = os.path.abspath(sys.argv[1])
PORT = int(sys.argv[2])
TOKEN = sys.argv[3] if len(sys.argv) > 3 else ""
SLOW = os.environ.get("UPLOAD_SLOW") == "1"
# 测试用：把创建上传返回的 Location 指到别的地址。
# daemon 必须拒绝不同源的 Location，否则这里就是 SSRF/凭证外发的入口。
TUS_LOCATION = os.environ.get("UPLOAD_TUS_LOCATION", "")
os.makedirs(ROOT, exist_ok=True)
LOCK = threading.Lock()
TUS = {}  # id -> {"path": ..., "size": ..., "offset": ...}


def authed(handler):
    if not TOKEN:
        return True
    return handler.headers.get("Authorization", "") == f"Bearer {TOKEN}"


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def _json(self, code, body=b""):
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if body and self.command != "HEAD":
            self.wfile.write(body)

    def _tus_headers(self, code, offset=None, extra=None):
        self.send_response(code)
        self.send_header("Tus-Resumable", "1.0.0")
        if offset is not None:
            self.send_header("Upload-Offset", str(offset))
        for k, v in (extra or {}).items():
            self.send_header(k, v)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_PUT(self):
        if not authed(self):
            return self._json(401, b'{"error":"auth"}')
        name = os.path.basename(self.path[len("/put/"):])
        if not name or "/" in name:
            return self._json(400, b'{"error":"bad name"}')
        n = int(self.headers.get("Content-Length", "0"))
        dest = os.path.join(ROOT, name)
        with open(dest, "wb") as f:
            left = n
            while left > 0:
                chunk = self.rfile.read(min(left, 1 << 20))
                if not chunk:
                    break
                f.write(chunk)
                left -= len(chunk)
        if left != 0:
            return self._json(400, b'{"error":"short body"}')
        self._json(201, b'{"ok":true}')

    def do_POST(self):
        if not authed(self):
            return self._json(401, b'{"error":"auth"}')
        if self.path != "/tus":
            return self._json(404, b'{"error":"not found"}')
        size = int(self.headers.get("Upload-Length", "0"))
        uid = secrets.token_hex(8)
        path = os.path.join(ROOT, f"tus-{uid}.bin")
        open(path, "wb").close()
        with LOCK:
            TUS[uid] = {"path": path, "size": size, "offset": 0}
        loc = TUS_LOCATION or f"/tus/{uid}"
        self._tus_headers(201, offset=0, extra={"Location": loc})

    def do_HEAD(self):
        if not self.path.startswith("/tus/"):
            return self._json(404, b'{"error":"not found"}')
        uid = self.path[len("/tus/"):]
        st = TUS.get(uid)
        if not st:
            return self._json(404, b'{"error":"not found"}')
        self._tus_headers(200, offset=st["offset"])

    def do_PATCH(self):
        if not authed(self):
            return self._json(401, b'{"error":"auth"}')
        if not self.path.startswith("/tus/"):
            return self._json(404, b'{"error":"not found"}')
        uid = self.path[len("/tus/"):]
        st = TUS.get(uid)
        if not st:
            return self._json(404, b'{"error":"not found"}')
        off = int(self.headers.get("Upload-Offset", "0"))
        if off != st["offset"]:
            return self._json(409, b'{"error":"offset mismatch"}')
        n = int(self.headers.get("Content-Length", "0"))
        # 用 r+b + seek 而不是 append：中断的 PATCH 会在 offset 处留下半截；
        # 重传同一 offset 时应当覆盖，而不是在后面再追加一份。
        with open(st["path"], "r+b") as f:
            f.seek(off)
            left = n
            while left > 0:
                chunk = self.rfile.read(min(left, 1 << 20))
                if not chunk:
                    break
                f.write(chunk)
                left -= len(chunk)
                if SLOW:
                    time.sleep(0.05)
        if left != 0:
            return self._json(400, b'{"error":"short body"}')
        with LOCK:
            st["offset"] = off + (n - left)
            new = st["offset"]
        self._tus_headers(204, offset=new)

    def do_GET(self):
        if not self.path.startswith("/tus/"):
            return self._json(404, b'{"error":"not found"}')
        uid = self.path[len("/tus/"):]
        st = TUS.get(uid)
        if not st:
            return self._json(404, b'{"error":"not found"}')
        data = open(st["path"], "rb").read()
        self.send_response(200)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
