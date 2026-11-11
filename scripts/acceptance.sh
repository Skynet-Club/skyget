#!/usr/bin/env bash
# 一条命令跑完所有可复跑的证据，最后打一张表。
#
#   bash scripts/acceptance.sh          # 全量（几分钟）
#   FAST=1 bash scripts/acceptance.sh   # 跳过 e2e 与性能基线
#
# 判据全部是行为，不看日志措辞：字节对不对、连接数是不是 0、坏块有没有被拒。
# 不联网：所有源都由 bench/throttled_server.py 在 loopback 上提供。
set -uo pipefail

cd "$(dirname "$0")/.." || exit 1
ROOT="$(mktemp -d /tmp/skyget-accept.XXXXXX)"
SKYGET_BIN=${SKYGET_BIN:-./target/release/skyget}
FAST=${FAST:-0}
PIDS=()
RESULTS=()

cleanup() {
	for p in "${PIDS[@]:-}"; do [ -n "$p" ] && kill "$p" 2>/dev/null; done
	wait 2>/dev/null
	[ "${KEEP:-0}" = "1" ] && echo "保留现场：$ROOT" || rm -rf "$ROOT"
}
trap cleanup EXIT

ok() { RESULTS+=("✓|$1|$2"); printf '  ✓ %-22s %s\n' "$1" "$2"; }
bad() { RESULTS+=("✗|$1|$2"); printf '  ✗ %-22s %s\n' "$1" "$2"; }
skip() { RESULTS+=("·|$1|$2"); printf '  · %-22s %s\n' "$1" "$2"; }

free_port() {
	python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()'
}

wait_http() {
	# 只取 1 字节：直接 GET 整个文件会按限速跑满、把就绪探测拖成超时
	for _ in $(seq 60); do curl -sf -m 1 -r 0-0 -o /dev/null "$1" && return 0; sleep 0.1; done
	return 1
}

TOKEN=""
[ -f "$HOME/.config/skynet-dataplane/tokens" ] && \
	TOKEN=$(grep '^main=' "$HOME/.config/skynet-dataplane/tokens" 2>/dev/null | cut -d= -f2)
API=${SKYGET_API:-http://127.0.0.1:7788}

echo "== 1. 运行时"
if "$SKYGET_BIN" --version >/dev/null 2>&1; then
	ok "skyget" "$("$SKYGET_BIN" --version)"
else
	bad "skyget" "跑不起来：$SKYGET_BIN"
fi
if command -v ffmpeg >/dev/null 2>&1 || [ -x "$HOME/.local/opt/ffmpeg/ffmpeg" ]; then
	ok "ffmpeg" "$( ("${SKYGET_FFMPEG:-ffmpeg}" -version 2>/dev/null || "$HOME/.local/opt/ffmpeg/ffmpeg" -version 2>/dev/null) | head -1 | cut -c1-40)"
else
	bad "ffmpeg" "没装：HLS/DASH 会失败"
fi

echo "== 2. 单元测试"
if out=$(cargo test --quiet 2>&1); then
	n=$(grep -oE '[0-9]+ passed' <<<"$out" | awk '{s+=$1} END {print s+0}')
	ok "cargo test" "全过（$n 条）"
else
	bad "cargo test" "有失败，见 cargo test 输出"
fi

if [ "$FAST" = "1" ]; then
	skip "e2e" "FAST=1 跳过"
	skip "性能基线" "FAST=1 跳过"
else
	echo "== 3. 端到端"
	for s in e2e token-scope-e2e hls-e2e; do
		if out=$(bash "scripts/$s.sh" 2>&1); then
			ok "$s" "$(tail -1 <<<"$out" | cut -c1-40)"
		else
			bad "$s" "$(tail -2 <<<"$out" | head -1 | cut -c1-50)"
		fi
	done

	echo "== 4. 性能基线（16MiB，每连接 1MiB/s）"
	P=$(free_port)
	python3 bench/throttled_server.py --port "$P" --size 16777216 --rate 1048576 >/dev/null 2>&1 &
	PIDS+=($!)
	if wait_http "http://127.0.0.1:$P/blob.bin"; then
		t() {
			local t0 t1
			t0=$(date +%s.%N)
			SKYGET_HOME="$ROOT/h" SKYGET_ALLOW_PRIVATE=1 \
				"$SKYGET_BIN" get "http://127.0.0.1:$P/blob.bin" -o "$ROOT/p$1" -j "$1" --json >/dev/null 2>&1
			t1=$(date +%s.%N)
			echo "$t1 - $t0" | bc
		}
		T1=$(t 1); T8=$(t 8)
		if [ "$(echo "$T1 > $T8 * 2" | bc)" = "1" ]; then
			ok "并发加速" "j=1 ${T1}s → j=8 ${T8}s（$(echo "scale=1; $T1/$T8" | bc)x）"
		else
			bad "并发加速" "j=1 ${T1}s，j=8 ${T8}s —— 没有 2 倍差距"
		fi
	else
		bad "并发加速" "限速服务器没起来"
	fi
fi

echo "== 5. 内容寻址与坏块"
P=$(free_port)
python3 bench/throttled_server.py --port "$P" --size 1048576 --rate 0 >/dev/null 2>"$ROOT/s.log" &
PIDS+=($!)
if wait_http "http://127.0.0.1:$P/blob.bin"; then
	SHA=$(head -1 "$ROOT/s.log" | awk '{print $2}')
	# 这里故意不隔离 SKYGET_HOME：要测的是 daemon 真正读的那份 CAS。
	# 用完把这个块删掉（1MiB，内容确定性生成）。
	SKYGET_ALLOW_PRIVATE=1 \
		"$SKYGET_BIN" get "http://127.0.0.1:$P/blob.bin" -o "$ROOT/cas" --json >/dev/null 2>&1
	BLOB="$HOME/.local/share/skynet-download/blobs/${SHA:0:2}/$SHA"
	trap 'rm -f "$BLOB"' EXIT
	if [ -f "$BLOB" ] && [ "$(sha256sum "$BLOB" | cut -d' ' -f1)" = "$SHA" ]; then
		ok "落盘校验" "缓存块 sha 与内容一致"
	else
		bad "落盘校验" "缓存块缺失或 sha 不符"
	fi
	# 坏块：直接改缓存块，再走本地接口（如果 daemon 在跑）
	if [ -n "$TOKEN" ] && curl -sf -m 2 -o /dev/null "$API/health"; then
		cp "$BLOB" "$ROOT/backup"
		printf 'X' | dd of="$BLOB" bs=1 seek=100 conv=notrunc status=none
		code=$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $TOKEN" "$API/cas/$SHA?verify=1")
		cp "$ROOT/backup" "$BLOB"
		if [ "$code" = "409" ]; then
			ok "坏块校验" "篡改后 ?verify=1 返回 409"
		else
			bad "坏块校验" "篡改后返回 $code，期望 409"
		fi
	else
		skip "坏块校验" "daemon 未运行或没有主 token"
	fi
else
	bad "内容寻址" "本地服务器没起来"
fi

echo "== 6. 坏镜像不污染"
P1=$(free_port); P2=$(free_port)
python3 bench/throttled_server.py --port "$P1" --size 8388608 --rate 0 >/dev/null 2>"$ROOT/g.log" &
PIDS+=($!)
python3 bench/throttled_server.py --port "$P2" --size 8388608 --rate 0 --corrupt >/dev/null 2>/dev/null &
PIDS+=($!)
if wait_http "http://127.0.0.1:$P1/blob.bin" && wait_http "http://127.0.0.1:$P2/blob.bin"; then
	SHA=$(head -1 "$ROOT/g.log" | awk '{print $2}')
	SKYGET_HOME="$ROOT/h3" SKYGET_ALLOW_PRIVATE=1 "$SKYGET_BIN" get \
		"http://127.0.0.1:$P1/blob.bin" -o "$ROOT/poison" --sha256 "$SHA" \
		--mirror "http://127.0.0.1:$P2/blob.bin" -j 4 --json >"$ROOT/p.json" 2>/dev/null
	got=$(sha256sum "$ROOT/poison/blob.bin" 2>/dev/null | cut -d' ' -f1)
	if [ "$got" = "$SHA" ]; then
		ok "坏镜像不污染" "回退主源后字节正确"
	else
		bad "坏镜像不污染" "落盘 sha 不符，或压根没落地"
	fi
else
	bad "坏镜像不污染" "服务器没起来"
fi

echo "== 7. 鉴权与身份"
if [ -n "$TOKEN" ] && curl -sf -m 2 -o /dev/null "$API/health"; then
	code=$(curl -s -o /dev/null -w '%{http_code}' "$API/jobs")
	[ "$code" = "401" ] && ok "无 token 拒绝" "401" || bad "无 token 拒绝" "返回 $code，期望 401"
	code=$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $TOKEN" "$API/jobs")
	[ "$code" = "200" ] && ok "主 token 可用" "200" || bad "主 token 可用" "返回 $code"
else
	skip "鉴权" "daemon 未运行或没有主 token"
fi

echo "== 8. 管道里的进度（每 20% 一行，不能被折叠）"
P=$(free_port)
python3 bench/throttled_server.py --port "$P" --size 8388608 --rate 524288 >/dev/null 2>&1 &
PIDS+=($!)
if wait_http "http://127.0.0.1:$P/blob.bin"; then
	n=$(SKYGET_HOME="$ROOT/h4" SKYGET_ALLOW_PRIVATE=1 "$SKYGET_BIN" get \
		"http://127.0.0.1:$P/blob.bin" -o "$ROOT/pg" -j 2 --progress stderr \
		2>&1 1>/dev/null | grep -c '\[' || true)
	# pi 之类的宿主超过 ~7 行就会折叠成「… (N earlier lines)」，等于没进度
	if [ "${n:-0}" -ge 1 ] && [ "${n:-0}" -le 8 ]; then
		ok "管道进度" "${n} 行（不触发折叠）"
	else
		bad "管道进度" "${n} 行，期望 1..8"
	fi
else
	bad "管道进度" "服务器没起来"
fi

echo "== 9. 主源挂掉，镜像顶上"
PA=$(free_port)
PB=$(free_port)
python3 bench/throttled_server.py --port "$PB" --size 8388608 --rate 0 >/dev/null 2>"$ROOT/m.log" &
PIDS+=($!)
if wait_http "http://127.0.0.1:$PB/blob.bin"; then
	SHA=$(head -1 "$ROOT/m.log" | awk '{print $2}')
	SKYGET_HOME="$ROOT/h5" SKYGET_ALLOW_PRIVATE=1 "$SKYGET_BIN" get \
		"http://127.0.0.1:$PA/blob.bin" -o "$ROOT/fo" \
		--mirror "http://127.0.0.1:$PB/blob.bin" --sha256 "$SHA" -j 4 --json \
		>"$ROOT/fo.json" 2>/dev/null
	got=$(sha256sum "$ROOT/fo/blob.bin" 2>/dev/null | cut -d' ' -f1)
	if [ "$got" = "$SHA" ]; then
		ok "多源容错" "主源不可达时用镜像完成"
	else
		bad "多源容错" "没落地或 sha 不符"
	fi
else
	bad "多源容错" "镜像服务器没起来"
fi

echo "== 10. 慢镜像不会拖后腿（按实测速度选源）"
PF=$(free_port)
PS=$(free_port)
python3 bench/throttled_server.py --port "$PF" --size 16777216 --rate 4194304 >/dev/null 2>"$ROOT/f.log" &
PIDS+=($!)
python3 bench/throttled_server.py --port "$PS" --size 16777216 --rate 262144 >/dev/null 2>/dev/null &
PIDS+=($!)
if wait_http "http://127.0.0.1:$PF/blob.bin" && wait_http "http://127.0.0.1:$PS/blob.bin"; then
	SHA=$(head -1 "$ROOT/f.log" | awk '{print $2}')
	t0=$(date +%s.%N)
	SKYGET_HOME="$ROOT/h6" SKYGET_ALLOW_PRIVATE=1 "$SKYGET_BIN" get \
		"http://127.0.0.1:$PF/blob.bin" -o "$ROOT/sp" --sha256 "$SHA" \
		--mirror "http://127.0.0.1:$PS/blob.bin" -j 4 --json >/dev/null 2>&1
	t1=$(date +%s.%N)
	el=$(echo "$t1 - $t0" | bc)
	# 轮流分片会等于慢源独自下完 8 片 ≈ 32s；按速度选源应接近快源的下限 ≈ 4s
	if [ "$(echo "$el < 12" | bc)" = "1" ]; then
		ok "慢镜像不拖后腿" "${el}s（轮流分片需约 32s）"
	else
		bad "慢镜像不拖后腿" "${el}s，太慢，选源没生效"
	fi
else
	bad "慢镜像不拖后腿" "服务器没起来"
fi

echo
echo "================ 汇总 ================"
fail=0
for r in "${RESULTS[@]}"; do
	IFS='|' read -r mark name detail <<<"$r"
	[ "$mark" = "✗" ] && fail=$((fail + 1))
	printf ' %s %-24s %s\n' "$mark" "$name" "$detail"
done
echo "======================================"
if [ "$fail" -gt 0 ]; then
	echo "结论：$fail 项失败"
	exit 1
fi
echo "结论：全部通过"
