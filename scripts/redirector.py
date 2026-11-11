#!/usr/bin/env python3
"""只做一件事的跳转服务器：把任何请求 302 到指定地址。

用途：测「重定向的每一跳都要重新过出网策略」这条承诺。

    python3 scripts/redirector.py <port> <目标URL>

它**不会**去访问目标，也不检查目标是否可达 —— 它只负责发 Location。
所以用它指向 `http://169.254.169.254/latest/meta-data` 是安全的：
如果 skyget 真的跟过去了，说明策略漏了；如果没跟，报错会是 policy.egress_denied
而不是连接超时。这正是这条用例要区分的东西。
"""
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(sys.argv[1])
TARGET = sys.argv[2]


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def _go(self, body: bool):
        self.send_response(302)
        self.send_header("Location", TARGET)
        self.send_header("Content-Length", "0")
        self.end_headers()
        if body:
            # 有些客户端只在 GET 上跟随跳转；这里什么都不写。
            pass

    def do_GET(self):
        self._go(True)

    def do_HEAD(self):
        self._go(False)


if __name__ == "__main__":
    print(f"redirecting :{PORT} → {TARGET}", flush=True)
    ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
