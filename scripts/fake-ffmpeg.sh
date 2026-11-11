#!/usr/bin/env bash
# 测试替身：不联网、不转码，只按引擎给的参数写一个产物并报进度。
# 用来在没有 ffmpeg 的机器上验证 HLS 编排（协议白名单、参数、落地）。
set -u
out=""
prev=""
protocols=""
for a in "$@"; do
    if [ "$prev" = "-progress" ]; then
        break
    fi
    if [ "$prev" = "-protocol_whitelist" ]; then
        protocols="$a"
    fi
    if [ "$a" = "-progress" ]; then
        out="$prev"
    fi
    prev="$a"
done
if [ -n "${FAKE_FFMPEG_LOG:-}" ]; then
    printf 'protocol_whitelist=%s\nout=%s\n' "$protocols" "$out" >"$FAKE_FFMPEG_LOG"
fi
[ -n "$out" ] || { echo "fake-ffmpeg: 没找到输出文件参数" >&2; exit 2; }
printf 'fake-mp4' >"$out" || exit 1
printf 'out_time=00:00:01.000000\nprogress=continue\nout_time=00:00:02.000000\nprogress=end\n'
