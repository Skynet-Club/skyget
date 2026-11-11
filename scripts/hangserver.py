#!/usr/bin/env python3
"""接受请求、发完响应头后不再发正文：给“45 秒卡住提示”的回归测试用。"""
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(sys.argv[1])


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self):
        self.hang()

    def do_HEAD(self):
        self.hang()

    def hang(self):
        self.send_response(200)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Content-Length", "1048576")
        self.end_headers()
        try:
            time.sleep(120)
        except Exception:
            pass

    def log_message(self, *args):
        pass


ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
