#!/usr/bin/env python3
"""本地测试服务器：把下载器需要面对的现实演一遍。

    /f/<name>       正常文件：支持 Range（206），可分段
    /norange/<name> 假装不支持 Range（永远 200 整条），测单连接回退
    /slow/<name>    每 200ms 才吐 256KB，测中断续传
    /deny           403（测「需要登录」这类人话错误）
    /html           一个网页（测「这不是文件」的提示）
    /missing        404

用法：python3 scripts/testserver.py 8899 <目录>
"""
import os
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = sys.argv[2] if len(sys.argv) > 2 else "/tmp/skyget-test"
PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 8899
# 第三个参数是绑定地址，默认 127.0.0.1。给 `::1` 就能测 IPv6 字面量
# （`http://[::1]:PPORT/f/x`）—— 那是 URL 解析里最容易出错的一种写法。
HOST = sys.argv[3] if len(sys.argv) > 3 else "127.0.0.1"


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    flaky_count = 0

    def log_message(self, *a):
        pass

    def do_HEAD(self):
        self._serve(head=True)

    def do_GET(self):
        self._serve(head=False)

    def do_POST(self):
        path = self.path.split("?")[0]
        if path == "/hook":
            length = int(self.headers.get("Content-Length", "0"))
            body = self.rfile.read(length)
            # 先写 header 再写 body：e2e 看到 hook.json 时 header 已经就绪。
            with open(os.path.join(ROOT, "hook.headers"), "w") as f:
                f.write(self.headers.get("X-Skynet-Signature", ""))
            with open(os.path.join(ROOT, "hook.json"), "wb") as f:
                f.write(body)
            self.send_response(200)
            self.send_header("Content-Length", "2")
            self.end_headers()
            self.wfile.write(b"ok")
            return
        self._plain(404, b"no")

    def _serve(self, head):
        path = self.path.split("?")[0]
        # 需要 Bearer 的源：验证下载凭证确实进了请求头
        if path.startswith("/auth/"):
            if self.headers.get("Authorization") != "Bearer test-secret":
                return self._plain(401, b"auth required")
            path = "/f/" + os.path.basename(path[len("/auth/"):])
        if path == "/deny":
            return self._plain(403, b"no")
        if path == "/missing":
            return self._plain(404, b"no")
        if path.startswith("/flaky/"):
            # 每三次请求回一次 503：真实世界里限流/灰度/过载都长这样
            H.flaky_count += 1
            if H.flaky_count % 3 == 0:
                return self._plain(503, b"busy")
            mode_flaky, name = "range", os.path.basename(path[len("/flaky/"):])
            path = "/f/" + name

        if path == "/html":
            body = b"<html><body>this is a page, not a file</body></html>"
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            if not head:
                self.wfile.write(body)
            return

        # 302 跳转：验证客户端跟随重定向后取到的字节与源一致
        if path.startswith("/redirect/"):
            target = "/f/" + os.path.basename(path[len("/redirect/"):])
            self.send_response(302)
            self.send_header("Location", target)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return

        mode, name = "range", ""
        for prefix, m in (
            ("/f/", "range"),
            ("/norange/", "norange"),
            ("/slow/", "slow"),
            ("/slow3/", "slow3"),
            ("/cd/", "cdname"),
            ("/liar/", "liar"),
            ("/liar-data/", "liar_data"),
        ):
            if path.startswith(prefix):
                mode, name = m, os.path.basename(path[len(prefix):])
                break
        else:
            return self._plain(404, b"no")

        fp = os.path.join(ROOT, name)
        if not os.path.isfile(fp):
            return self._plain(404, b"no")
        size = os.path.getsize(fp)
        start, end = 0, size - 1
        rng = self.headers.get("Range")
        partial = False
        # 半吊子 Range：探测时老实（HEAD 与 bytes=0-0 都正常），正式分段时错位 ——
        # 声称给的是请求的那一段，实际永远从 0 开始吐。真实世界里部分 CDN / 反代
        # 的范围归一化 bug 就是这个形态，而且它只在多段下载时才暴露。
        # 两种「假分段」，可检测性完全不同：
        #   /liar/      谎报范围：Content-Range 永远写 0 开头 → 客户端能发现不一致
        #   /liar-data/ 谎报数据：Content-Range 老实写请求范围，但字节从 0 开始发
        #               → 头部完全自洽，只有端到端 hash 能发现
        if mode in ("liar", "liar_data") and rng and not head and rng != "bytes=0-0":
            try:
                spec = rng[6:].split("-")
                s = int(spec[0])
                e = int(spec[1]) if len(spec) > 1 and spec[1] else size - 1
            except ValueError:
                return self._plain(400, b"bad range")
            length = e - s + 1
            self.send_response(206)
            self.send_header("Content-Type", "application/octet-stream")
            self.send_header("Content-Length", str(length))
            self.send_header("Accept-Ranges", "bytes")
            if mode == "liar":
                # 谎报范围：声称永远是从 0 开始的那一段
                self.send_header("Content-Range", f"bytes 0-{length-1}/{size}")
            else:
                # 谎报数据：头部完全如实，只有字节是错的
                self.send_header("Content-Range", f"bytes {s}-{e}/{size}")
            self.send_header("ETag", f'"{name}-{size}"')
            self.end_headers()
            with open(fp, "rb") as f:
                left = length
                while left > 0:
                    chunk = f.read(min(262144, left))
                    if not chunk:
                        break
                    try:
                        self.wfile.write(chunk)
                    except (BrokenPipeError, ConnectionResetError):
                        return
                    left -= len(chunk)
            return

        if rng and mode in ("range", "slow", "slow3", "cdname") and rng.startswith("bytes="):
            spec = rng[6:].split("-")
            try:
                start = int(spec[0]) if spec[0] else 0
                end = int(spec[1]) if len(spec) > 1 and spec[1] else size - 1
            except ValueError:
                return self._plain(400, b"bad range")
            if start >= size:
                return self._plain(416, b"bad range")
            partial = True

        length = end - start + 1
        self.send_response(206 if partial else 200)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Content-Length", str(length))
        self.send_header("Accept-Ranges", "bytes" if mode != "norange" else "none")
        self.send_header("ETag", f'"{name}-{size}"')
        if mode == "cdname":
            # 中文名字走 RFC 5987 那种写法
            self.send_header(
                "Content-Disposition",
                "attachment; filename=\"fallback.bin\"; filename*=UTF-8''%E4%B8%AD%E6%96%87-%E6%B5%8B%E8%AF%95.bin",
            )
        if partial:
            self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
        self.end_headers()
        if head:
            return

        with open(fp, "rb") as f:
            f.seek(start)
            left = length
            while left > 0:
                chunk = f.read(min(262144, left))
                if not chunk:
                    break
                try:
                    self.wfile.write(chunk)
                except (BrokenPipeError, ConnectionResetError):
                    return
                left -= len(chunk)
                if mode == "slow":
                    time.sleep(0.2)
                elif mode == "slow3":
                    time.sleep(1.0)

    def _plain(self, code, body):
        self.send_response(code)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        try:
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            pass


if __name__ == "__main__":
    import socket

    os.makedirs(ROOT, exist_ok=True)
    if ":" in HOST:
        class V6(ThreadingHTTPServer):
            address_family = socket.AF_INET6

        srv = V6((HOST, PORT), H)
    else:
        srv = ThreadingHTTPServer((HOST, PORT), H)
    print(f"serving {ROOT} on {HOST}:{PORT}", flush=True)
    srv.serve_forever()
