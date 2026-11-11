#!/usr/bin/env python3
"""一个“HTTP 200 但不是 JSON”的假 daemon：验证 doctor 不把坏响应当成功。"""
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(sys.argv[1])
BODY = b"<html>not a daemon</html>"


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(BODY)))
        self.end_headers()
        self.wfile.write(BODY)


ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
