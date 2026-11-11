# 夜班迭代 · 2026-10-31（延续到 08:00）

目标：在 0.22.0 的基础上继续飞轮迭代，优先做**能被测试钉住的安全/可靠性修复**，
而不是堆功能。每一批改动都必须有单测或 e2e 证据；不把已发布的行为弄坏。

## 交付版本

`0.22.1` 是夜班第一批交付；随后在审计/测试中继续发了 `0.22.2`–`0.22.8`
（每批见下文）。最终生产版本是 **0.22.8**，`/health` 返回
`{"ok":true,"version":"0.22.8"}`。

## 产品面新增

- **`GET /statusline`**：给 TUI 插件/看板的一条活跃任务视图；`running > pending >
  interrupted`，同类取最新；支持 `?source=agent:pi`；token 调用方按身份隔离。
- **`GET /jobs?active=1`**：只查活跃任务。状态栏不再从“最近 500 条”里挑，
  跑几小时的长任务不会被后来几百条短任务挤出窗口。
- **HLS/DASH 状态栏秒进度**：`GET /jobs/<id>/seconds` 读最后一个 `progress_secs`
  事件；`skyget statusline` 在 `bytes_total=0` 时按播放秒数画条；JSON 增加
  `seconds_done` / `seconds_total` / `unit=seconds`。
- **MCP `skyget_active`**：agent 不需要 `job_id` 就能拿当前活跃任务（可按来源过滤）。
- **`skyget forget <id>`**：CLI 补上“只删台账记录、不动文件”的入口。
- **同内容 in-flight 等待**：同 sha 已有任务在下载时，一个后来者等待并秒传
  （30 分钟上限、全局单等待槽；更多并发/超时各自下载）。`inflight-e2e.sh` 覆盖。
- **Debian 包**：`packaging/build-deb.sh` 产出含 CLI/daemon/MCP/GUI、用户 systemd
  unit、桌面入口和图标的 `.deb`（只构建；已用 `dpkg-deb -I/-c/-x` 验证）。

## 审计发现并修复

1. **跨 agent 幂等键泄露（安全）**：显式/自然幂等键原来全局共享；B 用 A 的 key
   会拿到 A 的任务 id/状态。现在键带来源前缀，`enqueue` 对归属不同的旧键返回
   409；e2e 覆盖 A/B 同 key 不同任务、同 agent 重试仍命中。
2. **取消/删除后事件流读不到（审计）**：记录删了但事件还在，路由却先看记录回
   404。现在本机无身份/主 token 仍可读事件，per-agent token 无归属证据按 404。
3. **缓存命中被计入每日配额（额度）**：`usage_since` 没过滤 `cached=1`，秒传会
   偷偷占掉当天额度；已修复并加单测。
4. **webhook 错误文本泄露签名 URL（安全）**：reqwest 传输错误的 error 字段带
   完整 URL（含 query）；webhook 和 failed/interrupted 事件统一 `redact_text`。
   URL 先 `redact_url` 再去 query，旧台账里的 `user:pass@` 也不会外发。
5. **webhook 跟随重定向（安全）**：302 到内网/元数据会绕过初始 URL 检查；现在
   客户端不跟随重定向，3xx 按投递失败重试。
6. **清单安装边界（安全）**：清单 URL 原来直接请求，绕过 `check_url`；现在过
   出网策略、限 1MiB 且边读边限流；每个文件必须带 64 位 sha256；先过 dest 边界
   再建父目录；mode 去掉 setuid/setgid/sticky；本地清单必须是普通文件。
7. **配额语法错误 fail-open（配置）**：`SKYGET_AGENT_QUOTAS` 缺 `=` 原来静默跳过
   （等于不限）；现在语法错误全局按 0（所有 agent 全拒）并警告一次，尾逗号仍允许。
8. **per-agent `dest_roots: []` 语义反转（配置）**：空根在兼容语义里是“不限制”，
   写 `[]` 容易被误解成“禁止”；现在解析阶段直接拒绝。
9. **file 凭证权限（安全）**：`$SKYGET_HOME/secrets/` 下 0644 的 token 对本机其他
   用户可见；现在组/其他权限位非 0 直接拒绝，要求 0600。
10. **状态栏查询窗口/索引（体验/性能）**：活跃任务查询、配额求和加索引；
    `/statusline` 只查活跃状态，长任务不再被挤出窗口。

## 其他

- 新数据目录（台账/缓存/分片/secrets）按 0700 创建；已存在的目录不强行改。
- `skyget_failures_total{code=…}` 进 `/metrics`，配合已有的 `skyget_denied_total`。
- webhook 可选 `SKYGET_WEBHOOK_SECRET` HMAC-SHA256 签名（RFC 4231 测试向量，
  webhook-e2e 对实际收到的 body 重算校验）。
- 安全模型 `docs/security.md`：信任边界、SSRF/凭证/路径/配额/完整性、残余风险、
  部署清单；英文 README 增加摘要。
- README 增加和 curl / aria2 / rclone 的能力对比；并发同内容后来者已支持“一个等待并秒传”。

## 验证证据

- `cargo test --features full`：**127 lib + 14 bin + 7 MCP + 1 doc** 全过。
- e2e 15 个脚本全过：core 26/26、restart 5/5、detach、agent-api 24/24、
  agent-integration 6/6、credentials、quota、token-scope、webhook、statusline、
  inflight、upload resume/cancel/recreate、soak。
- `skyget selftest` 12/12；`hostile.py` 5/5。
- `crash.py` 4 次硬杀全过：最终 sha 与源一致、进度从未回退、`verify --deep` 自洽。
- `soak-e2e.sh` 连续 10 轮全过；`restart-e2e.sh` 3 轮全过。
- 生产 daemon：0.22.1，`/health` ok，`/statusline` idle，`/metrics` 正常。

## 需要你决定/手动处理

- `skyget verify` 仍会报 3 条历史 `/tmp` 记录缺文件（不是 0.22 引入）。确认后可用：
  `skyget forget 1a18161d1880495`、
  `skyget forget 1a17645bab64791`、
  `skyget forget 1a1762df426e42f`。
- `/tmp/x/kali-linux-2026.3-installer-amd64.iso` 是缓存的独立副本（占双份空间）；
  确认不需要可直接删除。
- 当前数据目录 `~/.local/share/skynet-download` 是 0775、`tasks.db` 0644；新装会是
  0700。共享主机上建议 `chmod 700 …/skynet-download`、`chmod 600 …/tasks.db`。
- Claude Code 状态栏命令仍是全局 `skyget statusline`（会显示其他 agent 的任务）；
  只想看自己的可加 `--source agent:claude`。

## 补丁 0.22.2（03:40）

夜班复查 0.22.1 的 in-flight 等待时发现两个自查问题，已修复并出补丁：

- **全局等待标志静音了其他任务的卡住提示**。0.22.1 用进程级 `INFLIGHT_WAITER`
  判断“有人在等”，心跳线程据此跳过 45 秒无进度的提示；一个等待者会让所有其他
  真正卡住的任务最长 30 分钟不报“线路卡住”。现在标志按任务传递，只有等待者
  自己跳过提示。
- **等待提示带对端 job id 短前缀**。另一个 agent 的任务对当前 agent 不可见，
  短 id 不该出现在本任务事件流里；提示不再带 id。

验证：新增 `scripts/waiter-stuck-e2e.sh`（B 等 A 时 C 卡住 55 秒）钉住两条行为；
`cargo test --features full` 127 lib + 14 bin + 7 MCP + 1 doc 全过；全部 16 个
e2e（含新脚本）全过。生产 daemon/MCP 已升到 0.22.2，deb 重建为
`dist/skynet-dataplane_0.22.2_amd64.deb`。

## 补丁 0.22.3（04:00）

继续审计配置/清单边界时又发现两处，已修复：

- **清单 sha256 未归一**。`load`/`install` 原样使用清单里的 sha；写大写或带空白时
  校验仍能过（比较忽略大小写），但缓存路径是大写，缓存捷径永远命不中，已在本机的
  同内容会被重下一遍。现在先 `trim` + 转小写，与 API/CLI 一致。
- **per-agent 策略字段名写错被静默忽略**。`Rule` 没开 `deny_unknown_fields`，
  把 `dest_roots` 写成 `dest_root` 时该字段被丢掉，agent 继承全局策略；全局未限制
  落地根时等于悄悄放宽权限。现在配置错误直接拒绝启动。

验证：新增 `load_normalizes_manifest_sha` 单测；用“小写建缓存→停服务器→大写清单
安装”手动验证断网命中缓存；策略字段名写错时 daemon 启动直接报错退出。测试：
`cargo test --features full` **128 lib + 14 bin + 7 MCP + 1 doc** 全过；16 个 e2e
全过；waiters 回归 3 轮、soak/restart 各 5 轮、hostile 5/5、crash 4 次硬杀全过。
生产 daemon/MCP/deb 更新到 0.22.3。

## 补丁 0.22.4（04:05）

部署文档审计发现 systemd 路径没有真正生效：

- **unit 没加载 `~/.config/skynet-dataplane.env`**。文档让运维把
  `SKYGET_AGENTS` / 配额 / 策略写进这个文件，但 unit 里没有 `EnvironmentFile=`，
  服务会静默跑在“本机无鉴权”模式。现在加
  `EnvironmentFile=-%h/.config/skynet-dataplane.env`（文件可不存在），deb 里的
  unit 与旧名 `skynet-downloadd.service` 同步带上。
- **部署文档修正**：per-agent 策略 JSON 必须写成一行（EnvironmentFile 不支持
  跨行值），并补环境文件的创建、0600 权限和重启说明。
- **清理过时的“已知限制”**：`api-v1.md` / `conformance.md` 还写着 per-agent
  token 可读别的 agent 任务；实际 0.22.1 起跨 agent 任务/事件/pin 一律 404，
  token-scope e2e 一直覆盖。已改成当前行为，并把“字节配额未做”更新为缓存容量
  配额未做（字节配额 0.22.0 已有）。“私有源认证未做”更新为“任意自定义头未做；
  Bearer 凭证已支持”。
- **MCP/部署文档**：明确 agent 必须用 `SKYGET_AGENTS` 里自己的 token，不能把主
  token 发给 agent，否则任务隔离/策略/配额都会退化成运维身份。
- **README 旧说法**：“配额还没有按 agent 分开”已删（`SKYGET_AGENT_QUOTAS` 0.22.0
  起支持）；`SKYGET_API_TOKEN` 的环境变量表改成“多 agent 时填自己的 token”。
- **CI 覆盖补全**：README 说“CI 覆盖全部 e2e”，实际少了 agent-api、上传三件套、
  restart、soak；已补进 workflow。

验证：`systemd-analyze --user verify` 通过；用 `systemd-run --user` + 临时
EnvironmentFile 实测单引号 JSON 被正确解析；`cargo test --features full` 128 lib +
14 bin + 7 MCP + 1 doc 全过；core e2e 26/26、statusline e2e 通过；deb 内 unit
确认含 `EnvironmentFile` 且 `ExecStart=/usr/bin/skynet-dataplaned`。生产
daemon/MCP/deb 更新到 0.22.4。

## 补丁 0.22.5（04:45）

安全边界审计发现 HLS/DASH 与文档不一致：

- **ffmpeg 子请求绕过出网策略**。顶层 `.m3u8` 过了 `check_url`，但子清单和分片是
  ffmpeg 自己取的；严格模式/主机白名单下，一个公网播放列表仍能让 ffmpeg 去取
  `127.0.0.1` 或 `169.254.169.254`。`security.md` 却声称“分片 URL 逐条过策略”。
- **修复**：只要策略在限制出网（`allow_private=false` 或配了 `allowed_hosts`），
  HLS/DASH 默认拒绝，错误码 `policy.stream_egress`；确认播放列表可信的部署可设
  `SKYGET_ALLOW_STREAM_EGRESS=1` 显式放行（逃生口，文档写清取舍）。
- **输入协议白名单**：ffmpeg 只允许 `http,https,tls,tcp`，恶意 m3u8 不能用
  `file://` 分段把本机文件读进产物。
- **文档补 BT 残余**：`.torrent` URL 的初始地址过策略，但 librqbit 内部抓取可能
  跟随重定向且不再过策略；BT 仍不是 SSRF 边界。
- 文档同步：security/README/README.en/MANUAL/CLI 环境变量表不再声称分片逐条检查。

验证：新增 `stream_egress_is_fail_closed_under_restrictions` 单测；手动以
`SKYGET_EGRESS=strict` 提交 `https://1.1.1.1/x.m3u8` 得到 `policy.stream_egress`，
加 opt-in 后越过策略门（到“ffmpeg 不存在”）；`cargo test --features full`
**129 lib + 14 bin + 7 MCP + 1 doc** 全过；16 个 e2e 全过。生产 daemon/MCP/deb
更新到 0.22.5。

## 补丁 0.22.6（04:30）

终端注入加固：

- **人类 CLI 输出原本直接打印 agent/源站可控文本**。恶意 agent 可以把 `ESC]52;…`
  塞进 dest/URL，人在终端跑 `skyget list` 时可能触发 OSC 52 写剪贴板、改标题等。
- **修复**：非 JSON 模式下，`list/show/watch/top/pins/agents/verify/install` 以及
  顶层错误的 URL、路径、错误、事件文本先过 `display_safe`（去掉控制字符）。
  JSON/NDJSON 保持原值，字段协议不变。
- 新增 `display_safe_strips_terminal_escapes` 单测；security.md 增加人类输出边界。

验证：`cargo test --features full` **129 lib + 15 bin + 7 MCP + 1 doc** 全过（bin
测试新增 1 条）；core e2e 26/26、statusline e2e 通过；生产 daemon/MCP/deb 更新到
0.22.6。

## 补丁 0.22.7（04:45）

HTTP 解析资源上限：

- **无界 `read_line`**：daemon 用 `BufReader::read_line` 读请求行和每一个头，没有
  长度/数量上限；本机持 token 的 agent 发一条几 GB 的“头”就能把 daemon 内存打满。
  body 有 1MiB 上限、连接有 128 上限，但头部没有。
- **修复**：新增 `read_line_limited`（按字节封顶）和头数量上限；请求行/头行 16KiB、
  头 100 个，超限返回 431 与机器可读错误码。新增 2 个单测（超长拒绝、按换行切分）。

验证：`cargo test --features full` **131 lib + 15 bin + 7 MCP + 1 doc** 全过；
18 个 e2e 全过（新增 `scripts/http-limits-e2e.sh`：超长请求行/头 431、正常 200，
daemon 仍健康；`scripts/hls-e2e.sh`：fake-ffmpeg 验证协议白名单与受限出网拒绝）；
生产 daemon/MCP/deb 更新到 0.22.7。

## 补丁 0.22.8（06:05）

HLS 回归用例 3 暴露出 CLI 的同 URL 快捷键问题：

- **`find_done_by_url` 只按 URL 短路**，不看这次请求的落地目录。`skyget get URL
  --to 新目录` 会直接返回旧任务的结果，新目录里没有文件；也绕过了 HLS 的出网策略
  检查（旧任务在宽松策略下完成，后来的受限调用直接拿到旧结果）。
- **修复**：只有旧任务的父目录与本次目标目录一致时才短路；换目录就正常走引擎
  （命中内容缓存或重新下载），落到新目录。新增 core e2e §2b。
- 验证：core e2e **29/29**（新增 3 项换目录断言）、hls e2e 4 个场景、18 个
  e2e 全过；`cargo test --features full` 131 lib + 15 bin + 7 MCP + 1 doc；
  生产 daemon/MCP/deb 更新到 0.22.8。

## 最终验收快照（06:20）

- 版本链：`v0.22.1` → `v0.22.2` → `v0.22.3` → `v0.22.4` → `v0.22.5` →
  `v0.22.6` → `v0.22.7` → `v0.22.8`（`git tag` 查准确提交）。分支
  `feat/agent-dataplane-v0.21`，工作树干净。
- `cargo test --features full`：**131 lib + 15 bin + 7 MCP + 1 doc** 全过。
- e2e **18 个脚本**全过：core（29/29）、detach、agent-api（24/24）、
  agent-integration（6/6）、download-credential、quota、token-scope、webhook、
  statusline、http-limits、hls（fake-ffmpeg）、inflight、waiter-stuck、
  upload resume/recreate/cancel、restart、soak。
- 稳定性（0.22.7 期间）：soak 100 轮、restart 30 轮、inflight 30 轮、
  statusline 5 轮、waiter-stuck 5 轮；hostile 3 轮 × 5/5；crash 8 次硬杀全过。
- 0.22.8 最终二进制：18 个 e2e 全过，另加 soak 50 轮、restart 20 轮、
  inflight 20 轮全过。
- 备注：一次背靠背 inflight 复跑出现过 1 次 `A failed`，保留现场确认是
  **测试服务器还没 accept 就提交**（`connection refused`），不是产品逻辑；已加
  `scripts/wait-http.sh` 就绪等待，inflight/waiter/hls/statusline 已接入。
- 生产：daemon/MCP/deb 均为 0.22.8；`skyget doctor` 可用；`verify --deep`
  无新增损坏（仍是 3 条历史 `/tmp` 缺文件 + 1 个未共享 Kali 副本）。
- 构建缓存：`cargo clean` 释放过 192.6GiB，根盘从 100% 回到 62%。

## 残余风险（详见 docs/security.md）

DNS rebinding TOCTOU；同 uid 进程可读台账/缓存；`/metrics` 和 `/cas` 是机群级
可见（任何有效 token）；webhook HMAC 是可选、未设置则无签名；BT peers 不受 host
白名单约束；清单签名（minisign/cosign）尚未实现。
