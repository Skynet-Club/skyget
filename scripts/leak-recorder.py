#!/usr/bin/env python3
"""记录所有到达的请求（方法/路径/是否带 Authorization）到日志文件。

只给 SSRF/凭证外发测试用：把它放在 tus Location 指向的“攻击者”端口上，
断言日志为空，证明确实一个 HEAD/PATCH 都没发出去，而不只是错误码碰巧对。
"""
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

LOG = sys.argv[1]
PORT = int(sys.argv[2])


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def _handle(self):
        auth = "yes" if self.headers.get("Authorization") else "no"
        with open(LOG, "a") as f:
            f.write(f"{self.command} {self.path} auth={auth}\n")
        self.send_response(404)
        self.send_header("Content-Length", "0")
        self.end_headers()

    do_GET = _handle
    do_HEAD = _handle
    do_POST = _handle
    do_PUT = _handle
    do_PATCH = _handle
    do_DELETE = _handle


ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
