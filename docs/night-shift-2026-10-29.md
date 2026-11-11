# 夜班接管 · 2026-10-29 23:46 → 2026-10-30 08:00

目标：按接管方案把 P0「agent 级 daemon」做成可验证的增量，飞轮循环：
需求验证 → 开发 → 审计 → 测试 → 评估 → 迭代。

不追求一晚上实现整张蓝图；追求的是：**每一批改动都有验收证据，且不把现有产品弄坏。**

## 今晚的验收清单（P0）

- [ ] A1 跨进程分片锁：同一 URL 的 `.part` 只能有一个写者；第二个进程得到明确错误，不踩坏文件。
- [ ] A2 任务 id：时间有序 + 随机/序号，1 万次生成无碰撞；同 URL 不再靠 16 位 hash 碰运气。
- [ ] A3 机读错误：`code` / `retryable` / `retry_after_ms` / `next_action` 落库、进 JSON；分类有单测。
- [ ] A4 安全基线：SSRF（私网/元数据默认拒绝）、dest 根目录约束、API Bearer token 可选强制。
- [ ] A5 幂等提交：`Idempotency-Key` 或自然键 `(url,dest,sha256)` 重复提交返回同一 job。
- [ ] A6 全局调度：daemon 最多 N 个并发 job，队列有上限，满时 429 + `retry_after_ms`。
- [ ] A7 事件流：事件落库，`GET /jobs/<id>/events?since=` 可重放、可长轮询。
- [ ] A8 MCP：最小 stdio JSON-RPC，`want` / `watch` / `status` 三个工具。
- [ ] A9 审计：`cargo test`、`cargo clippy`、`skyget selftest` 全绿；写评估报告。

## 开工前基线（23:50）

- `cargo test --lib --bins --no-default-features --features http`：30 lib + 4 bin 通过。
- `cargo test`（默认）会编译 GUI 示例而失败 —— 示例没有按 feature 门控，记为 A0。
- 仓库干净，master，最后提交 c453908。

## 进度日志

（每完成一批，追加一行：时间 / 改了什么 / 证据 / 下一步）

### 23:51 · P0 第一批（正确性 / 错误契约 / 安全）
- A0 示例按 feature 门控：`cargo test` 不再被 GUI 示例挡住。
- A1 跨进程分片锁 `src/lock.rs`：`flock(LOCK_EX|LOCK_NB)`，HTTP/HLS/BT/`discard_parts` 都接上；
  第二个写者得到 `PartBusy`（`part.busy`，可重试）。锁文件故意不删（unlink/flock 竞态）。
- A2 `new_id` 改成时间前缀 + pid + 序号 + 48 位随机；1 万次生成无碰撞单测。
- A3 机读错误：`error_code/retryable/retry_after_ms/next_action` 落库、进 JSON；分类函数有单测。
- A4 安全：`src/security.rs`。严格档默认拒绝私网/回环/链路本地/CGNAT/元数据；
  dest 根目录约束（含 `..` 与符号链接逃逸）；API 可选 Bearer token；GUI 自动带 token。
  残余：DNS rebinding 的 TOCTOU 窗口，文档里写明。
- 证据：`cargo test --lib --bins --no-default-features --features http` 40+ 通过；
  `skyget selftest` 12/12（锁文件不算临时分片）。

### 23:56 · P0 第二批（幂等 / 调度 / 事件 / MCP）
- A5 幂等：`idempotency` 表 + `INSERT OR IGNORE` 原子占用；显式键或 `(url,dest,sha)` 自然键；
  8 线程抢同一键只有 1 个成功；删除任务释放键。
- A6 调度：`src/scheduler.rs`。固定 worker、按来源公平（默认单 agent 最多 2/4）、
  有界队列（满 429 + retry_after_ms）；队列满时新任务删记录释放幂等键，恢复任务标中断。
- A7 事件：`events` 表，per-job seq；`GET /jobs/<id>/events?since=&wait_ms=` 长轮询可重放；
  进度 1 秒一条，状态/错误/取消都落事件。
- A8 MCP：`src/bin/mcp.rs`，stdio JSON-RPC；工具 `skyget_want/watch/status/cancel`；
  自己不下载，全部转给守护进程。协议单测 4 条。

### 00:02 · P0 收尾 + 对外契约
- 接口连接上限 `SKYGET_MAX_CONNS=128`（长轮询也不能无限开线程）；小栈 512KB。
- `POST /jobs/<id>/retry`；MCP 增加 `skyget_retry`。
- CLI agent 路径：`skyget --as <agent>` / `--daemon` 提交给 daemon，事件流翻译回旧 NDJSON，
  stdout 仍只有一行结果；`skyget watch <id>` 可断线续读；连不上按来源决定回退或报错。
- 端到端 `scripts/agent-api-e2e.sh`：鉴权 / 幂等 / 事件 / 缓存命中 / MCP / CLI daemon 6 项全过。
- 抓到并修掉：`GET /jobs/<id>` 的 cached 写死 false；缓存命中 path 报目录不报文件；
  `early_dest` 与 `dest_is_dir` 判据不一致（目录没尾斜杠时把目录变成文件）。
- 文档：`docs/api-v1.md`（TaskView / 事件 / 错误码 / 策略 / 限制 / 客户端）；
  README 增加 0.19 小节；CHANGELOG 0.19.0；版本号 0.19.0。
- 测试：49 lib + 4+4 bin；clippy 只剩原有风格告警。
- 下一步：等独立审计结果 → 修；然后视时间做 P1（上传 / 多源 / 缓存 pin）。

### 00:20 · 证据链
- `cargo test --all-features --lib --bins`：61 + 4 + 4 = 69 通过。
- `scripts/e2e.sh`（release 0.19）：26/26，通过的是独立 CLI 的旧行为（未回归）。
- `scripts/hostile.py`：5/5（假 Range / 单连接逃生 / 谎报数据 / 带 hash 拒绝 / 503 退避）。
- `scripts/crash.py --rounds 6 --size 16MB --mode slow3`：4 次硬杀，进度不回退、最终 sha 一致、
  `verify --deep` 干净。
- `scripts/agent-api-e2e.sh`：6/6（鉴权 / 幂等 / 事件 / 缓存 / MCP / CLI daemon）。

### 00:18 · P1 共享缓存 + 监控
- 缓存 pin：`pins` 表 + `POST/GET/DELETE /pins`；CLI `skyget pin|pins|unpin`；
  MCP `skyget_pin/unpin`；`prune` 新增 `kept_pinned`。端到端 §8。
- 共享缓存直读 `GET /cas/<sha256>`：单段 Range（206/416）、ETag、404；
  端到端 §9（整取 sha 一致 + 前 1KB Range 一致）。
- `GET /metrics` Prometheus 文本（jobs/cache/bytes/events/pins），端到端 §10。
- 排队状态机：`RunHandle{gen,queued,cancelled}`；排队 pause→resume 复位同一队列项，
  cancel 后不可复活；DELETE 运行中任务回 409（不再被线程写回）。单测 3 条。
- 安全补丁：缓存捷径现在先过 `check_dest`（否则可用缓存里的 sha 写到允许目录外）；
  API 拒绝带 `Origin` 的请求、POST 必须 `application/json`（浏览器 CSRF/本地 SSRF）。
- 证据：57 lib + 4 + 4；`agent-api-e2e.sh` 10/10。

### 00:22 · 调度器真实压测
- `scripts/scheduler-loadtest.py`：2 worker、单 agent 上限 1、两个 agent 各 3 个慢任务。
- 实测：并发峰值 2（上限 2）、按来源峰值各 1、观测到排队、6 个任务全部落地且 sha 正确。

### 00:23 · 独立审计后修复（审计报告 24 条，按严重度消化）
- **sha256 边界**：API/MCP/CLI 全部只接受 64 位十六进制；`blob_of` 拒绝非 hex/穿越/多字节，
  修掉「任意本地文件被缓存捷径复制进 dest」和 `&sha[..2]` panic。
- **dest 边界**：API/CLI/引擎统一展开 `~`/相对路径；保留尾斜杠的目录语义；
  `check_dest` 拒绝悬空符号链接与最终文件链接；`unique_path`/`pick_dest`/`land`
  全部改用 `symlink_metadata`，落地前拒绝跟随链接。
- **生命周期**：`save_if_exists` 让终态只更新已存在的行（取消/删除后不复活）；
  cancel 未知 id 回 404、外部进程回 409；`forget` 对外部 pid 回 409；
  reconcile 只处理 pid 已死/本进程的 running，并把 pending 孤儿标中断 + 写事件。
- **幂等/并发**：先落库再 claim；8 线程同键只落 1 条任务；dispatch 的 check+insert
  放进同一把锁（并发 resume 只起 1 个引擎）。
- **panic 隔离**：scheduler `catch_unwind`；JobGuard 在 unwind 时清 handle、
  把 running/pending 标成 `internal.panic`。
- **MCP**：job_id 只接受 40 位十六进制（挡住 `x/../../jobs` 路径穿越）；
  畸形 sha 在 MCP 层就拒绝。
- **其它**：`/jobs/changes` 游标改为最后一条实际返回的 updated_at（不再用墙钟）；
  strict 策略尊重 `SKYGET_ALLOWED_HOSTS`；token 统一 trim；IPv6 增加 NAT64/6to4/Teredo/
  site-local/IPv4-compatible 拦截；HLS 子播放列表走同一 client+策略；http `.torrent`
  先过 check_url；DNS/策略拒绝的错误码修正；CLI daemon 失败退出码非 0；
  TaskView.path 只在 done 时有值；API/MCP 支持 accept_change。
- 证据：82 lib + 4 + 6；`agent-api-e2e.sh` 12/12；`restart-e2e.sh` 4/4。
- 仍欠（已记录，不在今晚硬塞）：每 agent 身份 token、上传、多源、S3 签名、
  ffmpeg 分片出网边界、BT peer 出口、events 保留策略、`/jobs/changes` 墓碑持久化。

### 00:36 · 上传（PUT / tus）+ agent 身份
- 上传 MVP：`src/upload.rs`。HTTP PUT / tus 1.0（创建、HEAD、PATCH、断点续传）；
  共用台账/事件/调度/幂等；`Task.direction/protocol/credential_ref` 落库。
- 安全：`SKYGET_UPLOAD_ROOTS` 默认空 = 禁止上传；凭证 env 白名单
  `SKYGET_UPLOAD_CREDENTIALS`，file: 只允许 `SKYGET_HOME/secrets/`（挡住
  `env:AWS_SECRET_ACCESS_KEY` / `file:/etc/shadow` 外传）；目标走同一套出网策略。
- 语义诚实：PUT/tus 没有统一远端 digest 回执，成功标 `trust=local`，不假装校验通过。
- CLI `skyget upload`、MCP `skyget_upload`、`skyget retry`；`TaskView` 新增 direction/protocol；
  `verify` 跳过上传（源文件被清掉不算台账坏）。
- agent 身份：`src/agents.rs`，`SKYGET_AGENTS="tok=name"`；token 身份覆盖请求体 source；
  没配时退回单 token 兼容模式。公平队列/台账归属不再靠自称。
- 端到端：`agent-api-e2e.sh` 15/15（PUT/tus/源目录边界 + 之前的 12 项）；
  `upload-resume-e2e.sh` 3/3（16MB 处 kill -9，tus 续传，远端 sha 一致）；
  `restart-e2e.sh` 4/4；单测 87 lib + 4 + 6。
- 还没做：S3 multipart / 远端 digest、per-agent 策略（根目录/白名单/配额）、多源、签名。

### 00:47 · per-agent 策略 + 多源回退 + 缓存搬运
- per-agent 能力：`SKYGET_AGENT_POLICIES`（JSON）按 agent 覆盖 dest_roots / allowed_hosts /
  upload_roots / max_running；未写字段继承全局 strict；策略文件坏了拒绝启动（fail closed）。
  `AgentPolicies::concurrency_limits` 直接喂给调度器的 per-source 上限。
- 多源回退：`NewJob.sources` / CLI `--mirror`。主源 + 镜像依次跑同一 `run_one`；
  网络/限流类失败保留分片，内容不符/HTTP 硬错误清位图再换源（`Note` 事件“换下一个源”）；
  有固定 sha 时多源共享 `sha-<前16>` 的 `.part`，位图与已下分段跨源复用。
  多源必须带 sha（否则不同镜像的字节无法证明是同一份内容）。
- 缓存 export/import：`skyget export <dir> [sha…]` / `skyget import <dir>`；
  两端都重算 sha256，坏块/长度不符拒绝；临时文件 + rename 落地。
- 修：缓存命中的 `land` 在目标父目录不存在时 ENOENT（多源 e2e 的 out8 暴露）；
  现在 `land` 先 `create_dir_all(parent)`。
- 证据：`cargo test --all-features --lib --bins` 91 + 4 + 6；release `--features full` 构建过；
  `agent-api-e2e.sh` **18/18**（新增 per-agent 根目录、CAS 往返、说谎镜像回退、多源缺 sha）；
  `restart-e2e.sh` 4/4；`upload-resume-e2e.sh` 3/3；`selftest` 12/12；
  `e2e.sh` 26/26；`hostile.py` 5/5；`crash.py` 6 轮通过；`scheduler-loadtest.py` 通过。

### 00:50 · 全量回归（审计前基线）
- 上面九组测试在同一次构建上全部重跑通过；release 二进制 0.20.0。
- 下一轮：对上传 / 多源 / 身份策略 / CAS / DB 迁移做独立审计，按严重度修；
  然后决定剩余时间投 GUI 驾驶舱还是 Metalink / UDS。

### 00:53 · 运维耐久：墓碑落库 + 保留策略 + 指标
- `/jobs/changes` 的删除墓碑从进程内 Vec 移到 SQLite `tombstones` 表；
  `Store::remove` 在同一个事务里删任务、释放幂等键、写墓碑。
  修掉「daemon 重启后删除对 since>0 客户端不可见」——这会让 GUI 把删掉的行挂回来。
- 保留：启动清一次 + 每小时维护线程；进度事件 30 天 / 全部事件 90 天 / 墓碑 7 天，
  环境变量可调，0=永不清理。以前只有启动时清进度事件，长跑 daemon 事件表无限增长。
- `/metrics` 增加 build_info / uptime / scheduler_capacity / sources / tombstones。
- 证据：单测 93 lib + 4 + 6（新增墓碑跨重开、保留先吃进度后吃状态）；
  `restart-e2e.sh` 5/5（新增 kill -9 后墓碑仍可见）；`cargo test --all-features` 全绿。

### 00:55 · `GET /agents`：把「谁在用」变成契约
- 新端点：agent 名字 / 来源 / 任务数 / 正在跑 / **实际生效**的并发上限 /
  dest_roots / allowed_hosts / upload_roots / allow_private；调度器总容量。
- 安全：响应里绝不带 token；per-agent token 调用只返回自己（caller），
  旧单 token / 本机无鉴权才看全部。名字来自注册表 + 策略文件 + 台账历史（已删 token 的
  历史任务仍能解释）。
- 实现：`Registry::names`、`AgentPolicies::names/max_running`、`Scheduler::limit_of/
  running_by_source`、`Store::source_counts`；`route_as` 多一个 registry 参数。
- 证据：94 lib + 4 + 6；`agent-api-e2e.sh` **19/19**（新增 §19：身份/策略/并发、
  token 不外泄、无 token 401）。

> 00:59–05:58 主要花在第二轮审计、上传/多源修复、GUI 机群实现和反复回归；
> 下面按收尾批次合并记录。

### 05:59 · 第二轮独立审计 → 修复 8 条 + 1 条部分
审计覆盖上传 / 多源 / 身份策略 / CAS / DB 迁移，共 9 条发现。按严重度：

- **（高）tus Location SSRF/凭证外发**：Location 以前直接 `base.join` 后用于
  HEAD/PATCH，不校验。服务器（白名单内即可）能把带 Bearer 和文件内容的请求指到
  `169.254.169.254`/任意域名。现在强制**同源 + 出网策略**，拒绝码
  `policy.egress_denied`；sidecar 里的旧 Location 也重新校验。
- **（高）多源共享位图信任坏源**：以前多源 + pin sha 共用 `sha-<前16>.part`。
  源 A 写入内容错误的段后网络失败，镜像补缺，整文件永远校验不过；重试又从坏源开始。
  现在分片按**源 URL** 隔离，换源从零开始；单源续传不变。清理按所有 sources 走。
- **（中高）tus 404/410 永久卡死**：HEAD 失败被 `unwrap_or(offset)` 吞掉，PATCH 永远 404。
  现在 HEAD/PATCH/收尾 HEAD 区分 404/410，清 sidecar 重建一次。
  新增 `scripts/upload-recreate-e2e.sh`：kill daemon → 重启上传服务器（状态清空）→
  重启 daemon → 自动重建 → 远端 sha 一致，4/4。
- **（中高）上传取消后幽灵复活**：初始/中段全量 `store.save` 把删掉的行插回；
  sha256 不可取消；取消后还写 sidecar。现在改 `save_if_exists`、分块哈希检查取消、
  PATCH 后取消不写 sidecar。
- **（中）上传无单请求超时**：按 32 KiB/s 下限计算每个 PUT/PATCH 的超时（2 分钟～6 小时）。
  阻塞式请求仍不能被 cancel 立即打断，记为残余。
- **（中）预签名 URL 进台账/API**：TaskView.url/sources/error 与事件 data 里的
  签名/令牌参数替换 `[redacted]`；CLI show/list 同步。SQLite 内仍存真实 URL（同 uid
  可读），API 侧不再外泄。新增 e2e §21。
- **（中低）身份/策略绕过**：`SKYGET_AGENTS` 全非法时以前静默 fail-open，现在拒绝启动；
  有 policies 无 registry 也拒绝启动。主 token/无身份不能用请求体 `source=agent:x`
  套别人策略，返回 `input.agent_source_requires_token`（e2e §19 覆盖）。
- **（中低）Closed 分支留下 pending + 占幂等键**：与队列满一致，新任务删行释放键，
  恢复任务标 interrupted。
- **（低）CAS export 吞 rename 失败**：目标同名目录现在明确报错且不写 manifest。
- 证据（本轮）：99 lib + 4 + 6；`agent-api-e2e.sh` 20/20；上传重建 4/4；
  全量回归见下一节。仍未修：阻塞请求中的即时取消、DB 内真实 URL 的磁盘隔离、
  S3 multipart/远端 digest。

### 05:59 · GUI 机群视图
- 顶栏「机群 N」切换到 agent 驾驶舱；每张卡显示任务数、正在跑/实际上限、
  并发占用条、落地根/允许主机/上传根。数据来自 `GET /agents`，2 秒刷新，
  只在视图打开时请求重绘。
- `SKYGET_GUI_VIEW=fleet` 启动即打开；用界面自带 `SKYGET_SHOT` 自截图验证。
- 第一版截图抓到布局 bug：卡片塞在 `ui.horizontal` 里，宽度缩成剩余值，
  长路径一字一行。改为纵向流 + Frame 直接占满行。
- 第二张实拍（两个 agent、各自 1/1 在跑、策略可见）存
  `docs/screenshots/agent-fleet.png`，README 0.20 节引用。

### 06:05 · 收尾小项（回归后追加）
- `skyget doctor [--json]`：目录可写/blobs/parts/策略/daemon 版本/连通性/token 身份。
  第一版只读本机 env，e2e 立刻抓到“daemon 有上传根、doctor 说没有”——改为优先读
  `/agents` 的远端策略；`/agents` 顶层新增 policy（agent token 只暴露自己的有效策略）。
- `/metrics` 增加按来源拆分的 `skyget_jobs_by_source` / `skyget_running_by_source`。
- MCP 新增 `skyget_agents`；CLI 新增 `skyget agents`（终端机群视图）。
- GUI 详情卡“落地”为空时显示“（还没落定）”，不再像界面坏了。
- 上传 `source=agent:x` 冒充同样在 `/uploads` 上拒绝（e2e §19）。
- e2e 增加 §22 fail-closed 启动、§23 doctor、§24 agents；最终 **24/24**。

### 06:07 · 上传取消 e2e + 迁移/事件脱敏测试
- 新 `scripts/upload-cancel-e2e.sh`：慢速 tus 传 16MB 时 cancel，
  6 秒窗口内记录始终 404、sidecar 不重生、事件有 cancelled —— 3/3。
  这条钉住审计第 4 条（全量 save 幽灵复活 + 取消后写 sidecar）。
- store 新增：0.18 老表迁移测试（缺 pid/trust/direction 等列仍能读、默认 download、
  新列可写回）；事件 data 里预签名 URL 落库前脱敏测试。
- `scripts/soak-e2e.sh`：40 并发 / 4 worker / 立刻取消 10 个。结果：30 个完成
  全部落地且 sha 一致、10 个取消记录 404、台账无 running/pending、同幂等键 dedupe。
  取消与完成会竞速（7/10 在 cancel 到达前已完成，文件保留是契约允许的），
  脚本不断言“取消一定没落地”。

### 06:12 · 全量回归（14/14 通过，最终）
- 单测 **101 lib + 4 bin + 6 MCP**（最终源码，`cargo test --all-features`）。
- release 全量构建、GUI 构建通过。
- `agent-api-e2e.sh` **24/24**（最终源码；多出的 §22 fail-closed、§23 doctor、
  §24 skyget agents）。全量回归那一轮的基线是 23/23，追加功能后单独重跑 24/24。
- `restart-e2e.sh` **5/5**（含删除墓碑跨重启）。
- `upload-resume-e2e.sh` **3/3**；`upload-recreate-e2e.sh` **4/4**（远端 404 重建）；
  `upload-cancel-e2e.sh` **3/3**（取消不复活）。
- `soak-e2e.sh` 通过：40 并发 / 4 worker / 10 取消，30 个完成全部落地 sha 一致，
  台账 30 条且无 running/pending 幽灵。
- `skyget selftest` **12/12**；旧 `e2e.sh` **26/26**；`hostile.py` **5/5**；
  `crash.py` 6 轮 6 次硬杀（最高 13.11MB，进度不回退、最终 sha 一致）；
  调度压测通过（峰值 2/上限 2，单 agent 1，观测到排队）。
- 最终 14 项 `FAILURES=0`；回归后又用最新源码重跑单测 101 lib + 4 + 6、
  agent e2e 24/24（doctor/agents 的一行 JSON 与 scoping 变更）。

### 06:17 · 第三轮审计（收尾面）→ 修 7 条
只审 doctor / agents / 脱敏 / 指标 / 单镜像 / 清理，独立审计确认：

- **（高）脱敏截断泄漏**：`redact_text` 在 `,`/`;`/`)` 处切 URL，切掉的后半段
  （含 `X-Amz-Signature`）被原样拼回；大写 `HTTPS://` 完全漏扫。现在按空白/全角标点
  切边界、尾部 ASCII 标点剥离后原样保留、scheme 大小写不敏感、解析失败走 `&`/`;`
  分段兜底；新增逗号路径、分号参数、大写 scheme、畸形 URL 四组单测。
- **（中）单镜像被忽略**：`run_inner` 用 `sources.len() > 1`，而 model.rs 契约允许
  `sources` 不含主 url（只有一个镜像）。改为非空即多源；新增 `has_mirrors` 单测。
- **（中）doctor 假阳性**：`/health`/`/agents` 200 但 body 是 HTML/缺字段时，
  以前 `unwrap_or({})` 算成功。现在契约校验失败即 `ok=false`；新增假 daemon
  （200 HTML）e2e，`--json` 退出非 0。
- 多源成功后 `task.url` 恢复主源（e2e §18 断言）；清理分片遇到 PartBusy 继续清其它源。
- `allowed_hosts` 三态：`null`=不限制、`[]`=拒绝全部；CLI/GUI 不再都显示“不限制”。
- `/metrics` 来源标签收敛：内置来源 + 注册表 agent，其余进 `other`，防基数膨胀。
- §20 改为真证明：Location 指向记录请求的端口、带真实的 Bearer 凭证，
  失败后断言记录端口一个请求都没有；§19 检查三个 token 都不出现在响应。
- 证据：**103 lib + 5 CLI + 6 MCP**；`agent-api-e2e.sh` **24/24**（含新断言）。

### 06:18 · 最终回归（第三轮修复后，14/14）
- **103 lib + 5 CLI + 6 MCP** 单测；release 全量 + GUI 构建。
- `agent-api-e2e.sh` **24/24**（含脱敏边界、单镜像、doctor 坏 JSON、
  SSRF 记录器、三个 token 不泄漏）。
- restart 5/5；upload resume 3/3、recreate 4/4、cancel 3/3；soak 通过。
- selftest 12/12；legacy 26/26；hostile 5/5；crash 多轮硬杀通过；
  scheduler-loadtest 通过（峰值 2/上限 2）。
- **FAILURES=0**。这是收尾时的最终证据。

### 07:24 · 品牌分层：天网数据面 / Skynet Dataplane
- 产品名从「天网下载器」升级为「天网数据面」；`skyget` 命令、app_id、数据目录、
  托盘图标名、MCP 配置全部保持兼容。
- 新主二进制：`skynet-dataplane`（GUI）、`skynet-dataplaned`（daemon）；
  旧名 `skynet-download` / `skynet-downloadd` 是同代码的兼容入口，共用 pid 文件/
  端口/台账，新旧混跑只会有 1 个实例。
- 守护进程入口抽到 `skyfetch::daemon::run()`，GUI 入口抽到 `skyfetch::gui::run()`；
  两个 bin 只是薄壳，避免双份逻辑。
- systemd 新 unit `skynet-dataplaned.service`；安装脚本优先装新服务、发现旧服务先停用；
  桌面文件原地更新，不产生双图标。
- GUI 标题/托盘/CLI 帮助/启动日志/README/MANUAL/API 契约统一新名；重拍
  `docs/screenshots/agent-fleet.png`（标题栏已是「天网数据面」）。
- 验证：新旧 daemon 互相识别单实例（双向手工测试）；agent e2e 24/24；
  改名后完整 14 项回归 **FAILURES=0**（103 lib + 5 CLI + 6 MCP）；
  默认 http 构建同时产出 `skynet-dataplaned` 和 `skynet-downloadd`。
  项目版本 **0.21.0**；截图 `docs/screenshots/agent-fleet.png` 已换成新标题栏。

### 07:42 · 修托盘「退出」无反应（0.21.1）
- 根因：窗口最小化/收进托盘后（Wayland 尤其）eframe 不再出帧，
  `Tray::fire` 只发通道 + `request_repaint`，GUI 永远不处理 `ViewportCommand::Close`；
  托盘和 Dock 留在原地，桌面等应用退出时鼠标转圈。
- 修复：托盘「退出」先让 D-Bus 菜单回调返回，再由独立线程 100ms 后结束 GUI 进程；
  GUI 只是前端，daemon 里的下载/上传不受影响。退出前释放单实例 pid，写 `gui.log`。
- 验证（真实 D-Bus，不是只编译）：
  `busctl --user call <SNI> /MenuBar com.canonical.dbusmenu Event 3 clicked ...`
  → 调用返回成功、GUI 进程退出、`/run/user/1000/skynet-download.pid` 消失。
- 版本 0.21.1。

## 最终评估（给接管人的一句话总结）

**它现在就是「天网数据面」—— 一个“单机多 agent 的数据面”**，不是又一个下载按钮：

- 适合：一台机器上多个 agent 抢带宽/缓存/台账；大文件下载与上传；需要断点续传、
  内容校验、幂等重试、可审计/可限权的场景；人类和 agent 共用一份状态。
- 已经比普通下载器强的点：单写者台账、per-agent token/策略、有界公平调度、
  事件重放、崩溃对账、内容寻址缓存、tus 续传、可机读错误、MCP/CLI/GUI 同一契约。
- 还不是“任意规模的 AI 基础设施”：没有跨机器 relay、对象存储后端、
  S3 multipart/远端 digest、Metalink、签名/来源证明、per-agent 字节配额、UDS/TLS。
- per-agent token 目前是归属/公平/策略边界，不是任务读隔离（签名 URL 已脱敏）；
  强多租户需要一台 daemon 一个租户或独立容器。
- 下一步优先级（按对 agent 的实际价值）：
  1. S3 multipart + 远端 checksum（上传的可验证闭环）；
  2. 下载自定义请求头 / 私库鉴权（Hugging Face 等）；
  3. per-agent 字节/带宽配额（共享机器上的公平性从“并发”走到“流量”）；
  4. UDS + systemd socket activation（不暴露 TCP 端口）；
  5. Metalink / 分段并行多源 / 签名清单。
- 回归期间发现并修：GUI 机群卡片宽度（截图验收）。
- 结论：本轮改动没有破坏 0.19 已有行为；所有新增契约都有对应证据（见 conformance）。
