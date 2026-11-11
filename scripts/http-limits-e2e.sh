#!/usr/bin/env bash
# 0.22.7 回归：超长请求行/头必须回 431，而不是把 daemon 读爆；正常请求仍然 200。
set -u

cd "$(dirname "$0")/.." || exit 1
ROOT="$(mktemp -d /tmp/skyget-http-limits.XXXXXX)"
PORT=$((39000 + $$ % 150))
DAEMON_PID=""

cleanup() {
  [ -n "$DAEMON_PID" ] && kill "$DAEMON_PID" 2>/dev/null
  wait 2>/dev/null
  [ "${KEEP:-0}" = "1" ] && echo "保留现场：$ROOT" || rm -rf "$ROOT"
}
trap cleanup EXIT
fail() { echo "FAIL: $*" >&2; cat "$ROOT/daemon.log" >&2 2>/dev/null; exit 1; }
ok() { echo "  ✓ $*"; }

mkdir -p "$ROOT/home"
SKYGET_HOME="$ROOT/home" ./target/debug/skyget serve --port "$PORT" >"$ROOT/daemon.log" 2>&1 &
DAEMON_PID=$!
for _ in $(seq 1 100); do
  curl -sf "http://127.0.0.1:$PORT/health" >/dev/null 2>&1 && break
  sleep 0.1
done
curl -sf "http://127.0.0.1:$PORT/health" >/dev/null || fail "daemon 没起来"

echo "1. 超长请求行 / 头行 / 头数量"
PORT="$PORT" python3 - <<'PY' || exit 1
import os
import socket

port = int(os.environ["PORT"])


def first_line(payload):
    s = socket.create_connection(("127.0.0.1", port), timeout=5)
    s.sendall(payload)
    data = b""
    try:
        while b"\r\n" not in data:
            chunk = s.recv(4096)
            if not chunk:
                break
            data += chunk
    finally:
        s.close()
    return data.split(b"\r\n", 1)[0]


cases = [
    ("请求行 20KiB", b"GET /" + b"a" * 20000 + b" HTTP/1.1\r\n\r\n", b"431"),
    ("头行 20KiB", b"GET /health HTTP/1.1\r\nX: " + b"a" * 20000 + b"\r\n\r\n", b"431"),
    ("101 个头", b"GET /health HTTP/1.1\r\n" + b"X: a\r\n" * 101 + b"\r\n", b"431"),
    ("正常健康检查", b"GET /health HTTP/1.1\r\n\r\n", b"200"),
]
for name, payload, want in cases:
    got = first_line(payload)
    if want not in got:
        print(f"FAIL {name}: 期望 {want.decode()}，实际 {got[:60]!r}")
        raise SystemExit(1)
    print(f"  ✓ {name} → {want.decode()}")
PY

echo "2. daemon 仍然活着"
curl -sf "http://127.0.0.1:$PORT/health" >/dev/null || fail "超长请求之后 daemon 不健康"
ok "健康检查正常"
echo "全部通过"
