#!/usr/bin/env bash
# 一条命令复现「并发连接能不能换速度」的两个 regime。
#
#   bash bench/fanout.sh
#
# 输出一张表：模式 / -j / 墙钟 / MB/s。任何人同一台机器应得到同一量级。
# 内容由 bench/throttled_server.py 确定性生成，全程 loopback，不需要外网。
set -euo pipefail

SIZE=${SIZE:-67108864} # 64 MiB
CONN_RATE=${CONN_RATE:-2097152} # 每连接 2 MiB/s
TOTAL_RATE=${TOTAL_RATE:-4194304} # 全局 4 MiB/s
# 必须现取一个空端口：写死端口会和残留进程串味，量出来的数是别人的。
PORT=${PORT:-$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')}
OUT=$(mktemp -d)
SKYGET=${SKYGET:-skyget}
BIN_DIR=$(cd "$(dirname "$0")" && pwd)

cleanup() {
	[ -n "${SRV_PID:-}" ] && kill -TERM "$SRV_PID" 2>/dev/null || true
	rm -rf "$OUT"
}
trap cleanup EXIT

start_server() {
	python3 "$BIN_DIR/throttled_server.py" --port "$PORT" --size "$SIZE" "$@" 2>>"$OUT/srv.log" &
	SRV_PID=$!
	for _ in $(seq 40); do
		if ! kill -0 "$SRV_PID" 2>/dev/null; then
			echo "server died:" >&2
			tail -5 "$OUT/srv.log" >&2
			return 1
		fi
		curl -sf -o /dev/null -I "http://127.0.0.1:$PORT/blob.bin" && return 0
		sleep 0.1
	done
	echo "server did not become ready" >&2
	return 1
}

stop_server() {
	kill -TERM "$SRV_PID" 2>/dev/null || true
	wait "$SRV_PID" 2>/dev/null || true
	SRV_PID=""
}

# 跑一发，回显 MB/s
run_one() {
	local j=$1 dest="$OUT/o$1"
	local t0 t1
	t0=$(date +%s.%N)
	SKYGET_ALLOW_PRIVATE=1 "$SKYGET" get "http://127.0.0.1:$PORT/blob.bin" \
		-o "$dest" -j "$j" --json >/dev/null 2>&1
	t1=$(date +%s.%N)
	python3 -c "print(f'{$SIZE/1e6/($t1-$t0):.2f}')"
}

printf '%-28s %-4s %8s %10s\n' 模式 -j 墙钟 吞吐
printf '%s\n' '--------------------------------------------------'

start_server --rate "$CONN_RATE"
for j in 1 4 8 16; do
	t0=$(date +%s.%N)
	mbps=$(run_one "$j")
	t1=$(date +%s.%N)
	printf '%-28s %-4s %7.2fs %8s MB/s\n' "每连接限速 2MiB/s" "$j" "$(python3 -c "print($t1-$t0)")" "$mbps"
done
stop_server

start_server --rate 0 --total-rate "$TOTAL_RATE"
for j in 1 16; do
	t0=$(date +%s.%N)
	mbps=$(run_one "$j")
	t1=$(date +%s.%N)
	printf '%-28s %-4s %7.2fs %8s MB/s\n' "全局限速 4MiB/s" "$j" "$(python3 -c "print($t1-$t0)")" "$mbps"
done
stop_server

echo
echo "结论看两段：第一段吞吐随 -j 近似线性 → 瓶颈是单连接；"
echo "第二段加连接不变 → 瓶颈是总带宽，加连接只增加开销。"
