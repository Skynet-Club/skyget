#!/usr/bin/env bash
# 调度面：优先级真的改排队顺序；阶段耗时真的进了事件流。
#
# 这两件事以前都是「有字段、没行为」（`queued` 事件里写着 priority:"normal"，
# 但没有任何东西读它）。所以这里验的是行为，不是字段存在。
set -u

# 公共样板都在 scripts/lib.sh（端口/临时目录/ok-no/trap/等 health）。
source "$(dirname "$0")/lib.sh"
e2e_init scheduling

mkdir -p "$SRC"
# 慢源：每 200ms 只吐 256KB。用来占住唯一的 worker。
head -c 3145728 /dev/urandom > "$SRC/slow.bin"
head -c 262144 /dev/urandom > "$SRC/small.bin"
start_server "$SRC" || exit 1

# 只有一个 worker：排队的顺序才是可观察的。
start_daemon "$D_PORT" SKYGET_MAX_JOBS=1 SKYGET_MAX_PER_AGENT=1 || exit 1

post() { # $1=url $2=priority $3=name
    curl -sf -X POST "$API/jobs" -H 'Content-Type: application/json' \
        -d "{\"url\":\"$1\",\"dest\":\"$DL\",\"name\":\"$3\",\"source\":\"agent:t\",\"priority\":\"$2\"}"
}

id_of() { python3 -c 'import json,sys;print(json.load(sys.stdin)["id"])'; }

echo "── 1. 优先级字段写错就明说 ──"
code=$(curl -s -o /tmp/prio-bad.json -w '%{http_code}' -X POST "$API/jobs" \
    -H 'Content-Type: application/json' \
    -d "{\"url\":\"http://127.0.0.1:$PORT/f/small.bin\",\"dest\":\"$DL\",\"priority\":\"urgent\"}")
if [ "$code" = "400" ] && grep -q "input.bad_priority" /tmp/prio-bad.json; then
    ok "不认识的优先级回 400 input.bad_priority（不静默当 normal）"
else
    no "坏优先级应该 400，实际 $code：$(cat /tmp/prio-bad.json)"
fi

echo "── 2. 排队顺序：interactive 先于 normal 先于 batch ──"
blocker=$(post "http://127.0.0.1:$PORT/slow/slow.bin" normal slow.bin | id_of)
sleep 0.3
batch=$(post "http://127.0.0.1:$PORT/f/small.bin" batch batch.bin | id_of)
normal=$(post "http://127.0.0.1:$PORT/f/small.bin" normal normal.bin | id_of)
inter=$(post "http://127.0.0.1:$PORT/f/small.bin" interactive inter.bin | id_of)

first_ts() { # 任务第一次「探测/定方案」的时刻；没跑到就等
    python3 - "$API" "$1" <<'PY'
import json, sys, time, urllib.request
api, job = sys.argv[1], sys.argv[2]
for _ in range(300):
    with urllib.request.urlopen(f"{api}/jobs/{job}/events?since=0&wait_ms=0") as r:
        ev = json.load(r)["events"]
    for e in ev:
        if e["type"] in ("probing", "planned"):
            print(e["ts"])
            raise SystemExit
    time.sleep(0.1)
print(0)
PY
}

t_i=$(first_ts "$inter")
t_n=$(first_ts "$normal")
t_b=$(first_ts "$batch")
if [ "$t_i" -gt 0 ] && [ "$t_i" -le "$t_n" ] && [ "$t_n" -le "$t_b" ]; then
    ok "开始顺序 interactive → normal → batch（$t_i ≤ $t_n ≤ $t_b）"
else
    no "开始顺序不对：interactive=$t_i normal=$t_n batch=$t_b"
fi

echo "── 3. 阶段耗时进了事件流 ──"
phases=$(python3 - "$API" "$normal" <<'PY'
import json, sys, time, urllib.request
api, job = sys.argv[1], sys.argv[2]
want = {"probe", "transfer", "verify", "land"}
names = set()
for _ in range(300):
    with urllib.request.urlopen(f"{api}/jobs/{job}/events?since=0&wait_ms=0") as r:
        ev = json.load(r)["events"]
    names = {e["data"].get("name") for e in ev if e["type"] == "phase"}
    if want <= names:
        print("yes")
        raise SystemExit
    time.sleep(0.1)
print("missing:" + ",".join(sorted(want - names)))
PY
)
if [ "$phases" = "yes" ]; then
    ok "probe / transfer / verify / land 四个阶段都有耗时"
else
    no "阶段事件不全（$phases）"
fi

echo "── 4. queued 事件里带的是真优先级 ──"
if curl -s "$API/jobs/$inter/events?since=0&wait_ms=0" | grep -q '"priority":"interactive"'; then
    ok "queued 事件如实记录 interactive"
else
    no "queued 事件没记下优先级"
fi

summary
