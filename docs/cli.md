# skyget 命令行参考（0.24）

`skyget` 是天网盒子的命令行客户端。它既能独立下载，也能把任务交给守护进程，
与 GUI / MCP / HTTP 共用同一本台账、同一份内容缓存和同一套策略。

> 本文与 `skyget <命令> --help` 同源校正；两者冲突时以 `--help` 为准。
> HTTP 契约见 [api-v1.md](api-v1.md)，产品说明见 [../MANUAL.md](../MANUAL.md)。

## 0. 全局约定

```text
skyget <URL> [选项]          # 裸地址 = skyget get <URL>
skyget <COMMAND> [选项]
```

| 约定 | 说明 |
|---|---|
| `--json` | 可放在任意位置；除 `serve` 这类常驻命令外，stdout 只留一行结果 JSON，过程/错误走 stderr。**失败也是这一行**：`{"state":"failed","error":…,"error_code":"cli.failed","retryable":false}` |
| `--timeout <秒>` | 全局。这条命令最多等多久，到点直接退出（1 / `cli.timeout`）；**不等 ≠ 取消**：分片留着、重跑接着下，daemon 里的任务继续跑 |
| 进度显示 | stderr 是终端且没用 `--json` 时，显示单行实时进度条；`--json` 时 stderr 改为 NDJSON 事件 |
| 退出码 | `0` = 成功；`1` = 任意失败（包括任务最终不是 `done`）；`2` = 用法错（clap 解析失败，不给 JSON） |
| `--as <名字>` | 来源记为 `agent:<名字>`；默认 `cli` |
| agent 默认路径 | 带 `--as` 时优先提交给 daemon；连不上按来源回退或报错 |
| `--daemon` | 强制交给 daemon |
| `--standalone` | 强制独立下载，不经过 daemon |
| 数据根 | 默认 `~/.local/share/skynet-download`，可用 `SKYGET_HOME` 整体搬走 |

## 1. 命令总表

| 命令 | 作用 |
|---|---|
| `get` | 下载、校验、落地，一次调用拿结果 |
| `check` | 预检：这条地址提交上去会被怎么判（不入队、不联网，和真提交同一套规则） |
| `completions` | 生成 bash / zsh / fish 补全脚本 |
| `resolve` | 把已下过的地址换成回环缓存地址（命中时零网络） |
| `curl` | curl 的替身：接管「单地址 + `-o 文件`」，其余参数透传给真 curl（`-o` 按 curl 语义当**文件**，不是目录） |
| `upload` | 上传本地文件（PUT / tus） |
| `events` | 事件流：按 NDJSON 打印一条任务的事件（`--follow` 跟到终态） |
| `watch` | 跟 daemon 里的一条任务：事件流 + 最终结果 |
| `retry` | 重新排队失败/中断任务（同记录、同期望 sha） |
| `pause` / `resume` / `cancel` | 暂停 / 继续 / 取消 daemon 里的任务（id 可只写前几位） |
| `forget` | 只删台账记录，盘上文件不动 |
| `list` / `show` | 查台账 |
| `pin` / `unpin` / `pins` | 缓存 pin（「这份内容我在用」） |
| `export` / `import` | 内容缓存搬运，双方都重算 sha256 |
| `prune` | 清没人在用、很久没碰过的缓存块 |
| `verify` | 台账 / 落地文件 / 缓存 / 位图自洽检查 |
| `agents` | 机群：agent 身份、策略、并发 |
| `doctor` | 目录 / 策略 / daemon / 身份体检 |
| `top` | 终端驾驶舱 |
| `statusline` | 单行进度条（状态栏 / TUI 插件） |
| `stats` | 总量、秒传命中率、失败分类、来源 |
| `serve` | 前台起 daemon（通常交给 systemd） |
| `selftest` | 不联网的内核行为自检 |
| `dest` | 查看/设置默认保存目录 |
| `install` | 按清单下载 + 校验 + 落位 + 权限 |
| `ensure` | 按清单**对账**：已经正确的文件不联网、不写盘（只增改，不删） |
| `catalog` | 浏览天网机库 |

## 2. 下载

```bash
skyget <URL>                         # 默认下到 ~/Downloads
skyget get <URL> -o ~/models -n model.safetensors
```

| 选项 | 说明 |
|---|---|
| `-o, --out <目录>` | 落地目录（别名 `--to`） |
| `-n, --name <名字>` | 另存名字；默认用服务器给的文件名 |
| `--sha256 <hex>` | 内容必须匹配；不符就失败并丢弃 |
| `--sha256-from <URL>` | 从发布方的 `.sha256` 文件取期望值（别名 `--sha256-url`） |
| `--accept-change` | 地址内容变了也接受（正常更新时用） |
| `--priority <P>` | 排队优先级：`interactive`（有人等着）/ `normal`（默认）/ `batch`（夜间预热）；只改排队顺序，不改并发上限 |
| `-j, --jobs <N>` | 并发连接数；**默认 0 = 按体积自动**（`clamp(大小/8MiB, 4, 16)`）；`1` = 不分段（对付假 Range 源） |
| `--hls-fast` | HLS/DASH：分片并行预取后再 remux。默认关闭；直播/加密/master playlist 自动回退 |
| `--progress <auto\|stderr\|off>` | 进度输出。`auto`（默认）只在终端里画；`stderr` 在管道里也写（每 20% 一行）；`off` 全静音 |
| `--as <名字>` | 来源身份；默认 `cli` |
| `--daemon` / `--standalone` | 强制 daemon / 强制独立 |
| `--detach` | 提交给 daemon 后立即返回；任务在 daemon 里跑，客户端可退出 |
| `--credential-ref <env:NAME\|file:PATH>` | 下载凭证；作为 `Authorization: Bearer`，值不落台账；仅 http/https 直链，且必须配置 host 白名单 |
| `--mirror <URL>` | 额外镜像，可重复；多源必须同时给 `--sha256` |

示例：

```bash
# 人类：大文件 + 校验
skyget https://example.org/big.iso --sha256 <hex> -o ~/Downloads

# agent：提交给 daemon，断线后用 watch 接着看
skyget --json --as claude https://example.org/model.bin --sha256 <hex> -o /srv/models

# 多源回退：主源 + 镜像
skyget get https://a.example/x -o /srv/out \
  --mirror https://b.example/x --sha256 <hex>
```

### 已下过的内容：`resolve`

```bash
skyget resolve <URL>          # → http://127.0.0.1:7788/cas/<sha256>
curl -o out.bin "$(skyget resolve <URL>)"
```

命中时**零网络**（46% 的台账户下载属于这种情况）。地址不在台账里会 exit 1 ——
它是缓存查询，不是下载器。`--verify` 会让服务端发之前重算一遍 sha256（慢，防坏块）。

### curl 的替身：`curl`

```bash
skyget curl -L -o out.bin <URL>
```

只接管「一个地址 + `-o`」这一种形态。`-X` / `-d` / `-T` / 多个地址 / `-o -` /
非 Authorization 的 `-H` 全部原样交给真 curl。`-H "Authorization: Bearer …"` 会映射成
`--credential-ref`（token 只进子进程环境，台账只存引用），并且仍受
`SKYGET_ALLOWED_HOSTS` 约束。

## 3. 上传

```bash
skyget upload <本地文件> <目标URL> [选项]
```

| 选项 | 说明 |
|---|---|
| `--protocol put` | 默认；一次 HTTP PUT，小文件/内部接口 |
| `--protocol tus` | tus 1.0；分块 PATCH，可断点续传 |
| `--sha256 <hex>` | 本地文件期望指纹；不符就不上传 |
| `--credential-ref <ref>` | `env:NAME` 或 `file:PATH`；只存引用，不落台账 |
| `--as <名字>` | 来源身份；默认 `cli` |

限制与语义：

- 源文件必须在 `SKYGET_UPLOAD_ROOTS` 内；默认空 = **禁止上传**。
- `env:NAME` 的变量名必须在 `SKYGET_UPLOAD_CREDENTIALS` 白名单里。
- `file:PATH` 只允许 `SKYGET_HOME/secrets/` 下的文件。
- 目标走与下载相同的出网策略；tus `Location` 还必须与上传地址同源。
- PUT/tus 没有统一远端 digest，成功标 `trust=local`，不假装内容校验通过。

```bash
skyget upload ./checkpoint.safetensors https://upload.example.org/files/x \
  --protocol tus --sha256 <hex> --credential-ref env:UPLOAD_TOKEN --as claude
```

## 4. 任务与事件

```bash
skyget events <id 或前几位> [--since N] [--follow]
skyget watch <id> [--since N] [--wait-ms 25000]
skyget pause|resume|cancel [--as <agent>] <id 或前几位>
skyget retry [--as <agent>] <id>
skyget forget [--as <agent>] <id 或前几位>
skyget list [-l 20] [--mine | --agents | --source <前缀>]
skyget show <id 或前几位> | skyget show --url <URL>
```

- `watch` 打印事件流，断线后用 `--since` 接着读；最终结果会转成退出码。
- `events` 是同一个事件流，但**只输出 NDJSON**（一行一个事件，原样），不掺人话排版：
  `skyget events <id> --since 42 > events.ndjson`。`--follow` = 像 `tail -f` 一样跟，
  到 `done` 退出 0，到 `failed/interrupted/cancelled` 退出 1。
- `pause` / `resume` / `cancel` 和界面上的那三个按钮走同一组接口：
  - `pause` = 合作式停下，已下的分片留着；`resume` 接着下，一字节不重下；
  - `cancel` = 停下 + 清临时分片 + **删掉这条记录**（已落地的文件不动）；
  - id 可以只写前几位；`pause` 之后立刻 `resume` 会按 daemon 给的
    `retry_after_ms` 等一下再发（引擎收摊需要一点时间），这是 CLI 侧的一次重试。
- `retry` 只对失败/中断任务有意义；复用原记录、原期望 sha 和已下分片。
- `forget` 只删台账记录（`DELETE /jobs/<id>`），**盘上文件不动**；还在跑/排队中
  先 `cancel`（`skyget cancel <id>`，以前只能从界面或 HTTP 接口做）。
  适合清理 `verify` 报“缺文件”的历史记录（例如 /tmp 已被清掉）。
- `list --mine` 只看没有来源标签的记录；`--agents` 只看程序发起的；
  `--source agent:claude` 按前缀筛。筛选在**查询里**做（不是先取 20 条再筛），
  所以 `--mine` 拿到的就是「我的最近 20 条」。
- `list` 默认 20 条；被截断时会明确写「还有 N 条没显示（共 M 条）：`skyget list -l M` 看全部」。
  `--json` 时这行提示走 stderr，stdout 仍是一个数组（形状不变）。
- `show` 支持 id 前缀；`--url <地址>` 直接查这个地址最近一条记录（省掉从 list 抄 id）。
- 用法错误（参数写错、子命令不认识）退出码 **2**；`--json` 时 stdout 仍给一行
  `{"state":"failed","error":"…","error_code":"cli.usage","next_action":"…"}`，
  人话提示（含 Usage）在 stderr。`--help` / `--version` 照常打到 stdout、退出 0。
- `--as <agent>` 用在控制类命令上 = **用该 agent 的 token 发这一次请求**
  （身份在 daemon 那边只认 token）。本机 `SKYGET_AGENTS` 里没有这个名字就明确失败。
- `pause` / `resume` / `retry` / `cancel` 写的事件带 `by`：有 token 时是
  `agent:<name>`，否则是 `api`。

### 机器可读的形状（`--json`）

三个「只给人看」的命令现在都有稳定字段，脚本不用再解析表格：

**`skyget statusline --format json`**（或 `--json`，两者等价）

| 字段 | 含义 |
|---|---|
| `job_id` / `state` / `source` | 当前跟的任务、它的状态、来源 |
| `done` / `total` / `pct` | 已下字节 / 总字节（未知为 0）/ 百分比（`total` 未知时按 0 算，别拿它当进度） |
| `unit` | `bytes` 或 `seconds`（切片流按播放时长报进度） |
| `seconds_done` / `seconds_total` | 只有 `unit=seconds` 时非 0 |
| `width` | 这条进度条按多少格画的（TUI 里要自己截断时用） |

没有活跃任务时**什么都不输出**（不是空 JSON）—— 状态栏插件据此判断「可以清掉这一格」。

**`skyget --json top [-l N]`**：一次快照，不是流。

```json
{"summary": {"total": 12, "running": 1, "queued": 0, "done": 9, "failed": 2},
 "jobs": [ …同 /jobs 的 TaskView 字段… ]}
```

**`skyget --json stats`**：`tasks` / `by_state` / `by_source` / `done_bytes` / `cached_runs` /
`cached_bytes` / `blobs` / `blob_bytes` / `failures`（人话分类）/ `failures_by_code`（机器码）/
`first_ms` / `last_ms`（台账时间跨度）。

### 超时：`--timeout <秒>`（全局）

```bash
skyget --timeout 300 get <URL> --daemon     # 5 分钟后不再等
skyget --timeout 60 watch <id>
```

到点会**直接退出**（退出码 1，`--json` 时给 `cli.timeout`），语义写清楚：

- **不等 ≠ 取消**。已下的分片留在盘上，重跑同一条命令会接着下；
- daemon 里的任务**不受影响**，只是这条 CLI 不等了；
- 为什么不做「优雅取消」：CLI 的等待可能卡在 DNS / TLS / 长轮询 / 写盘任意一处，
  真正的取消只有引擎知道怎么做；而硬退出的代价已经被产品兜住（位图落盘、可续传）。

### 状态栏 / TUI 插件：`skyget statusline`

```bash
skyget statusline [--format ansi|plain|json] [--watch]   # --json 等价于 --format json [--interval-ms 1000] [--width N]

`--width 0`（默认）按 `COLUMNS` 自适应，扣掉标签占的格后夹在 10..60。
                  [--job <id>] [--source <前缀>] [--osc] [--width 40]
```

- 默认自动选最新的 `running`/`pending` 任务；暂停任务显示 `(paused)`，没有任务时什么都不输出。
- `ansi`：绿色 `━` 进度 + 纯黑轨道，给状态栏；`plain`：`#`/`-`，无颜色；
  `json`：每行一个对象（`job_id` / `state` / `source` / `done` / `total` /
  `seconds_done` / `seconds_total` / `unit` / `pct` / `width`），给 TUI 插件自己渲染。
- HLS/DASH 没有总字节：自动用 `/jobs/<id>/seconds` 的播放秒数算 pct（`unit=seconds`）；
  普通直链仍按字节。
- `--source agent:claude` 只看某个来源；过滤时标签显示 `Skygeting[claude]:`。
- `--osc` 同时向终端发 OSC 9;4，标签页/任务栏显示原生进度。
- `--watch` 在终端里原地刷新；`--format json --watch` 输出 NDJSON，直到任务结束。

## 5. 缓存与内容

```bash
skyget pin <sha256> --ttl 7d --as claude [--note "model in use"]
skyget pin <sha256> --forever
skyget pins [--json]
skyget unpin <sha256> [--as claude]

skyget export <目录> [sha…]
skyget import <目录>
skyget prune --older-than 30d [--max-bytes 10GB] [--yes]
skyget verify [--deep] [--repair]
```

- `pin` 按 `(sha256, agent)` 去重；默认 7 天，`--forever` = 不过期。
- `prune` 默认只报计划；`--yes` 才真删；有效 pin 保护的内容不会被清。
- `--max-bytes` 在年龄门槛之上再按总量预算清：从最老的开始，直到缓存不超过上限；
  受 pin / 落地文件链接 / 年龄保护的块仍然不动。
- `export/import` 两端都重算 sha256；坏块拒绝。
  `--json` 输出同一个形状（`cas::Summary`）：

  | 字段 | 含义 |
  |---|---|
  | `blobs` | 实际搬运的块数（导出：写出的块；导入：新落地的块） |
  | `bytes` | 这些块的总字节 |
  | `skipped` | 跳过数（导出：源里没有的 sha；导入：已存在且校验通过） |
- `verify --deep` 会重算所有落地文件/缓存块的 sha；`--repair` 只修安全项（作废位图）。

## 6. 机群与运维

```bash
skyget agents [--json]   # 有哪些 agent、并发、落地根、主机、上传根、今日配额用量
skyget doctor [--json]   # 目录写权限、策略、daemon 版本/连通性、token 身份
skyget top [--watch] [--interval-ms 1000] [--limit 50]
skyget stats [--json]
skyget serve [--port 7788]
skyget selftest [--json]
```

`skyget doctor` 的退出码：本地目录/daemon/身份任一关键项失败即 `1`；
`--json` 仍只在 stdout 输出一行完整报告。

`skyget serve` 是前台守护进程；生产环境用 systemd：

```bash
systemctl --user enable --now skynet-dataplaned
journalctl --user -u skynet-dataplaned -f
```

## 7. 其他

```bash
skyget dest [路径] [--reset]   # 查看/设置默认保存目录
skyget install <清单> [--key <公钥>] [--require-signature]
skyget ensure  <清单> [--key <公钥>] [--require-signature]   # 幂等对账：只补缺/换错的
skyget keygen --out ~/.config/skynet/release.key   # ed25519：打印公钥、私钥写盘(0600)
skyget sign <清单> --key @release.key --key-id rel-1 --in-place
skyget manifest-digest <清单>    # 规范化摘要（给外部签名工具签这 32 字节）
skyget catalog list
skyget catalog show <名字>
```

`catalog` 需要天网机库目录；默认位置可用 `SKYGET_CATALOG` 指到 `catalog.db`。

## 8. 环境变量

| 变量 | 作用 |
|---|---|
| `SKYGET_HOME` | 数据根（台账/缓存/分片/密钥） |
| `SKYGET_API` | daemon 地址，默认 `http://127.0.0.1:7788`；写 `unix:/path/to/daemon.sock` 时走 Unix socket（身份由内核给，不用 token） |
| `SKYGET_UDS_AGENTS` | **daemon 侧**：`uid:name` 或 `gid:name` 的映射，如 `1001=claude,1002=codex`。登记了的 uid 连 socket 即得到该 agent 身份（配额/策略/隔离照旧）；配了这张表又没登记的 uid 会被 401 拒绝 |
| `SKYGET_API_TOKEN` | 客户端 Bearer token；单 token 模式=主 token，多 agent 时填该 agent 自己的 token |
| `SKYGET_AGENTS` | `token=name,...` per-agent token 注册表 |
| `SKYGET_AGENT_POLICIES` | per-agent 策略 JSON 文件 |
| `SKYGET_AGENT` | MCP 默认 agent 名 |
| `SKYGET_DAEMON` | `1`/`0` 覆盖 agent 默认走 daemon 的行为 |
| `SKYGET_ALLOW_PRIVATE` | 放开私网/回环（默认拒绝）。**不包含云元数据**：那要另一个开关，见下 |
| `SKYGET_ALLOW_METADATA` | 放开云元数据地址（169.254.169.254 / 100.100.100.200 / `fd00:ec2::254`）。默认拒绝，且**不跟着 `ALLOW_PRIVATE` 走** |
| `SKYGET_EGRESS` | `strict` 强制严格出网策略 |
| `SKYGET_ALLOW_STREAM_EGRESS` | 受限出网下仍允许 HLS/DASH（ffmpeg 子请求不受逐条策略；确认播放列表可信） |
| `SKYGET_ALLOWED_HOSTS` | 允许的 host 白名单，逗号分隔 |
| `SKYGET_DEST` / `SKYGET_DEST_ROOTS` | 默认保存目录 / 允许落地根 |
| `SKYGET_UPLOAD_ROOTS` | 允许上传读取的本地根；空 = 禁止上传 |
| `SKYGET_UPLOAD_CREDENTIALS` | 允许上传引用的 `env:NAME` 凭证名，逗号分隔 |
| `SKYGET_DOWNLOAD_CREDENTIALS` | 允许下载引用的 `env:NAME` 凭证名，逗号分隔；空 = 禁止 |
| `SKYGET_AGENT_QUOTAS` | 每 agent 每天数据字节上限（下载+上传），如 `claude=10GB,pi=500MB`；`unlimited` = 不限；写错 fail closed（值错该 agent 全拒，语法错全局全拒） |
| `SKYGET_COMPLETION_WEBHOOK` | 任务终态 POST 的 webhook URL；payload 不含凭证，URL 去掉 query |
| `SKYGET_WEBHOOK_SECRET` | 可选；设置后 webhook 带 `X-Skynet-Signature: sha256=…` |
| `SKYGET_VERIFY_CACHE` | 人类 CLI 命中缓存时也重算 sha |
| `SKYGET_MAX_JOBS` / `SKYGET_MAX_QUEUED` | daemon 并发 / 队列上限 |
| `SKYGET_MAX_PER_AGENT` / `SKYGET_MAX_CONNS` | 单来源并发 / 连接上限 |
| `SKYGET_CACHE_MAX_BYTES` | **缓存水位线**（daemon）：超过就自动清到低水位；`10GB` / `500MB` / 纯字节；不设 = 不自动清 |
| `SKYGET_CACHE_LOW_WATER` | 清到多低，默认上限的 90%（避免每轮都触发） |
| `SKYGET_CACHE_GC_MIN_AGE` | 最小年龄，默认 `1h`：刚下完的块不算「没人要」 |
| `SKYGET_CACHE_GC_INTERVAL` | 多久检查一次，默认 `5m`（下限 10s） |
| `SKYGET_TRUSTED_KEYS` | 可信清单公钥：`id=hex` 或 `hex`，逗号分隔（和 `--key` 等价） |
| `SKYGET_IDEM_TTL` | 自然幂等键（未显式给 `idempotency_key`）的有效期，默认 `24h`；`0` = 永不过期（旧行为） |
| `SKYGET_{PROGRESS,EVENT,TOMBSTONE}_RETENTION_DAYS` | 事件/墓碑保留天数，0 = 不清理 |
| `SKYGET_CATALOG` | 机库 `catalog.db` 路径 |
| `SKYGET_FFMPEG` | ffmpeg 可执行文件路径（HLS 用） |
| `SKYGET_GUI_VIEW` | GUI 启动视图；`fleet` = 直接打开机群 |

## 9. MCP 工具

`skynet-mcp` 是 stdio MCP server，自己不下载，只转发给 daemon：

| 工具 | 作用 |
|---|---|
| `skyget_check` | 提交前预检（不排队、不联网）：会被接受还是被拒、卡在哪一项 |
| `skyget_want` | 提交下载任务 |
| `skyget_upload` | 提交上传任务 |
| `skyget_watch` | 跟事件流 |
| `skyget_status` | 查任务 |
| `skyget_active` | 当前活跃任务（`GET /statusline`；TUI/状态栏用） |
| `skyget_cancel` | 取消任务 |
| `skyget_retry` | 重跑失败/中断任务 |
| `skyget_pin` / `skyget_unpin` | 缓存 pin |
| `skyget_agents` | 看当前身份能看见的机群 |

`skyget_watch` 支持 MCP 进度通知：请求的 `params._meta.progressToken` 会原样出现在
`notifications/progress` 里，`progress`/`total` 是字节数或秒数。客户端
（Claude Code / Codex / opencode）可以据此在自己的 TUI 里画进度条。

最小配置：

```json
{
  "mcpServers": {
    "skynet-dataplane": {
      "command": "/home/you/.local/bin/skynet-mcp",
      "env": {
        "SKYGET_API": "http://127.0.0.1:7788",
        "SKYGET_API_TOKEN": "该 agent 自己的 token（SKYGET_AGENTS 里的值）；没设就不填"
      }
    }
  }
}
```

多 agent 主机不要把主 token 发给 agent：每个 agent 用 `SKYGET_AGENTS` 里自己的
token，才能拿到任务隔离、per-agent 策略和配额；主 token 是给 GUI/运维用的。

## 10. HTTP 接口速查

完整字段/错误码见 [api-v1.md](api-v1.md)。

| 方法与路径 | 作用 |
|---|---|
| `GET /health` | 存活与版本（唯一免鉴权） |
| `POST /jobs` | 提交下载任务（幂等） |
| `POST /uploads` | 提交上传任务 |
| `GET /jobs` / `GET /jobs/<id>` | 台账 / 单条 |
| `GET /jobs/<id>/events?since=&wait_ms=` | 事件流 + 长轮询 |
| `POST /jobs/<id>/pause\|resume\|retry\|cancel` | 传输控制 |
| `DELETE /jobs/<id>` | 只删记录，不动文件 |
| `GET /jobs/changes?since=` | 增量同步（含删除墓碑） |
| `POST/GET/DELETE /pins` | 缓存 pin |
| `GET /cas/<sha256>` | 从共享缓存直读（支持 Range） |
| `GET /metrics` | Prometheus |
| `GET /statusline?source=` | 当前活跃任务（TUI 插件轮询） |
| `GET /agents` | 机群身份/策略/并发 |

## 11. 常见工作流

> Agent 何时该用 skyget、何时不该用，以及 MCP/CLI 的选择，见
> [`../CLAUDE.md`](../CLAUDE.md) 和
> [`.claude/skills/skyget-transfer/SKILL.md`](../.claude/skills/skyget-transfer/SKILL.md)。

### 人类：大文件下载

```bash
skyget https://example.org/dataset.tar --sha256 <hex> -o ~/datasets
```

### agent：提交后断线重连

```bash
skyget --json --as claude https://example.org/model.bin --sha256 <hex> -o /srv/models
# 拿到 id 后
skyget watch <id> --since 0
```

### 多 agent 共用一台机器

```bash
export SKYGET_AGENTS="tok_claude=claude,tok_codex=codex"
export SKYGET_AGENT_POLICIES=/etc/skynet/policies.json
skynet-dataplaned
skyget doctor
skyget agents
```

### CI：预热/搬运缓存

```bash
skyget export ./cas-cache <sha…>
skyget import ./cas-cache
```

### 上传大文件（tus）

```bash
skyget upload ./big.bin https://upload.example.org/files/big \
  --protocol tus --sha256 <hex> --as claude
```

## 12. 兼容名

| 新主名 | 旧名（仍可用） |
|---|---|
| `skynet-dataplane`（GUI） | `skynet-download` |
| `skynet-dataplaned`（daemon） | `skynet-downloadd` |
| `skyget` | 不变 |
| `skynet-mcp` | 不变 |

旧名与新名共用 pid 文件、端口和台账，不会各起一份。
