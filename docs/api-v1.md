# 天网盒子 · 本地接口契约（0.24 现行版，契约仍为 v1）

这份文档是**契约**，不是使用说明。改字段形状 = 改 API；改语义 = 改 API。
客户端实现请以这里为准，别依赖某次 `GET` 的偶然输出。

> 定位：守护进程是这台机器上**唯一**的下载写者。CLI、GUI、MCP、机库、
> 别的 agent 都通过它提交任务、读事件、取消任务。`skyget` 人类路径仍可独立下载，
> agent 路径（`--as`）默认提交给 daemon。

## 1. 地址与鉴权

- 默认：`http://127.0.0.1:7788`（只绑回环）。
- 换端口：daemon `--port 7799`；客户端 `SKYGET_API=http://127.0.0.1:7799`。
- 鉴权（可组合）：
  - **多 agent（推荐）**：`SKYGET_AGENTS="tok_claude=claude,tok_codex=codex"`。
    每个 token 绑定一个 agent 身份；daemon 用身份覆盖请求体里的 `source`，
    台账和公平队列不再相信自称。名字只允许字母/数字/`-_.`，最长 32。
  - **主 token（人类管理员 / GUI / 运维）**：`SKYGET_API_TOKEN=…`。启用 agent 注册表后
    它仍然有效，但没有 agent 身份 —— 能看全部 agent、能操作全部任务，台账来源仍按请求体
    （适合本机 GUI/MCP；不要把它发给不可信 agent）。
  - **单 token（兼容）**：只设 `SKYGET_API_TOKEN`、不设 `SKYGET_AGENTS`，所有调用方共用。
  - 除 `GET /health` 外都要带 `Authorization: Bearer <token>`；否则
    `401 {"code":"auth.required"}`。CLI / GUI / MCP 读 `SKYGET_API_TOKEN`。
- 跨机器：目前请自己套 SSH 隧道；不要直接把端口暴露到公网。

## 2. 任务对象（TaskView）

`GET /jobs`、`GET /jobs/<id>`、CLI `--json` 返回同一种形状。
`GET /jobs?active=1` 只返回 `running`/`pending`/`interrupted`（按创建时间倒序），
用于状态栏/看板：老的长任务不会因为最近几百条短任务被挤出窗口。
`url` / `sources` / `error` / 事件 data 里的预签名参数（`X-Amz-Signature`、
`X-Goog-Signature`、`token=`、`secret=` 等）会被替换为 `[redacted]`：台账内部
保留真实值供重试，但**接口不把签名当公开信息**。本地 SQLite 文件仍含真实 URL
（同 uid 可读，属于本机信任边界；需要更强隔离时把 daemon 放进独立容器）。

| 字段 | 类型 | 含义 |
|---|---|---|
| `id` | string | 任务 id（40 位十六进制，时间有序） |
| `direction` | string | `download` / `upload` |
| `protocol` | string | 上传：`put` / `tus`；下载为空 |
| `url` | string | 下载主源地址 / 上传目标地址 |
| `sources` | string[] | 这次任务可用的镜像（多源时包含主 url） |
| `state` | string | `pending` / `running` / `done` / `failed` / `interrupted` |
| `path` | string | **最终落地文件路径**；失败/未落地时为空 |
| `bytes_done` | u64 | 已下字节 |
| `bytes_total` | u64 | 总字节；未知为 0 |
| `sha256` | string | 内容指纹 |
| `error` | string | 人话失败原因（可直接转述） |
| `error_code` | string | 机器可读分类，见 §6 |
| `retryable` | bool | 重试有没有意义 |
| `retry_after_ms` | u64 | 建议等多久再试；0 = 不建议自动重试 |
| `next_action` | string | 下一步做什么，一句话 |
| `source` | string | `human` / `cli` / `api` / `agent:<name>` / `manifest`。启用 `SKYGET_AGENTS`/per-agent 策略后，`agent:<name>` 只能由该 agent 的 token 产生；主 token 或无身份调用方用 `agent:` 会被 `403 input.agent_source_requires_token` 拒绝 |
| `priority` | string | 排队优先级 `interactive`/`normal`/`batch`；老 daemon 不回这个字段，按 `normal` 看 |
| `cached` | bool | 这份内容本机早有，一字节没走网络 |
| `elapsed_ms` | u64 | 本次耗时 |
| `trust` | string | `pinned` / `indexed` / `tofu-new` / `tofu-ok` / `torrent` / `none` |
| `created_ms` | i64 | 创建时刻（毫秒） |
| `links_live` / `links_seen` | u32 | 已连接 / 发现的对等端（BT）；直链时 seen=0 |

## 2.5 传输与身份

两种传输，同一个契约：

| 传输 | 身份来源 | 说明 |
|---|---|---|
| `127.0.0.1:<port>`（默认） | `Authorization: Bearer <token>` | 老路径，行为完全不变 |
| Unix socket `$SKYGET_HOME/daemon.sock`（0660） | 内核 `SO_PEERCRED` 给的 uid/gid | 不用分发 token；daemon 自己的 uid = 主 token 等价，登记的 uid → `agent:<name>` |

socket 上的请求**不需要** Bearer。配了 `SKYGET_UDS_AGENTS` 而 uid 不在表里 → `401 auth.unknown_peer`。
放在 `0660` 文件上的权限是部署方的边界（组决定谁能连）。
CLI / MCP 用 `SKYGET_API=unix:<path>` 即可，客户端会在本进程内起一个带口令前缀的回环转发，
对端 uid 仍然是调用方自己的 uid —— 身份不会被中转放大。

## 2.6 预检：`POST /check`

不排队、不联网，只回答「这条提交会被怎么判」。它和 `POST /jobs` **共用同一段判定代码**
（`preflight`），所以预演说行、真提交就不会说不行。

```http
POST /check
{"url": "https://example.org/x.iso", "dest": "~/models", "sha256": "9f2c…"}

→ 200 {"ok": true, "url": "…", "dest": "/home/u/models/", "source": "api",
       "checks": [{"name":"protocol","ok":true,"detail":"http(s) 直链"}, …]}
→ 403 {"ok": false, "code": "policy.egress_denied", "error": "策略拒绝：…",
       "next_action": "如果是内网/回环源，需要部署方设 SKYGET_ALLOW_PRIVATE=1 …",
       "checks": [{"name":"egress","ok":false,"detail":"…"}, …]}
```

- 检查项：`identity`（不能自称 agent）、`protocol`、`mirrors`（多源必须 pin sha）、
  `sha256`、`credential`、`dest`、`upload_source`（给 `path` 时）、`egress`
  （**只有预检做**：真提交故意不在入队时查网，域名暂时解析失败该按网络错误重试）。
- 判定用的是**调用方身份对应的策略**（token → agent → per-agent 覆盖），和 `/jobs` 一致。
- 上传任务把本地路径放 `path`，会按 `SKYGET_UPLOAD_ROOTS` 判。

## 3. 提交任务

```http
POST /jobs
Content-Type: application/json
Authorization: Bearer <token>

{
  "url": "https://example.org/model.gguf",
  "sha256": "9f2c…7a41",
  "dest": "~/models",
  "name": "llama-3.1-8b-q4.gguf",
  "source": "agent:claude",
  "idempotency_key": "run-7f3/model"
}
```

| 字段 | 必填 | 说明 |
|---|---|---|
| `url` | 是 | http/https、magnet、.torrent、.m3u8/.mpd |
| `sources` | 否 | 额外 http(s) 镜像（去重）。给了镜像就必须同时给 `sha256`；主 url 也会作为一个源 |
| `sha256` | 建议 | 硬门槛：不符即失败、文件丢弃；多源时必填 |
| `dest` | 否 | **目录**（不带尾斜杠也按目录）；默认 `~/Downloads`。如果它已经是一个文件，则保留该完整路径 |
| `name` | 否 | 文件名（只能是一段，不能带路径）；给了就把 dest 当目录 |
| `source` | 否 | 台账里的来源；默认 `api`，最长 64 字节。配了 `SKYGET_AGENTS` 时被 token 身份覆盖 |
| `idempotency_key` | 否 | 幂等键；不给则用 `(url, dest, sha256)` 自然键。**键按来源隔离**：不同 agent 用同一个 key 不会拿到对方的任务 |
| `accept_change` | 否 | 地址内容变了也接受（和 CLI `--accept-change` 同义；只对本次生效） |
| `priority` | 否 | `interactive` / `normal`（默认）/ `batch`。**只影响排队顺序**：调度器先挑优先级高的来源，同优先级仍按「谁跑得少」轮转。并发上限由配额/调度上限决定，优先级不能突破它。写错回 `400 input.bad_priority` |

返回：

```json
{
  "id": "01a18863f23e0003a2e400000000f222d08662f8",
  "state": "queued",
  "events": "/jobs/01a18863…/events",
  "priority": "normal",
  "running": 2,
  "queued": 5
}
```

同一个自然幂等键命中一条**早已结束**的旧任务（默认超过 24h，`SKYGET_IDEM_TTL`）时，
不再 dedupe：新建一条任务，并在响应里多给一个 `previous_job`（上一次那条的 id），
`deduped` 为 `false`。显式传 `idempotency_key` 的语义不变。

- 新任务：`202 Accepted`。
- 同一幂等键/自然键重复提交：`200 OK`，返回**同一条**任务，并带 `"deduped": true`。
  失败的任务也返回同一条；要重跑用 `/jobs/<id>/retry`。
- `401` 鉴权失败；`403` 落地路径被策略拒绝；`429` 队列满（带 `retry_after_ms`）；
  `503` daemon 正在退出。
- **多源**：主 url + `sources` 依次尝试。网络/限流错误保留已下分片；内容不符/HTTP
  错误会清掉该任务的位图再换下一个源。最终仍以 `sha256` 为硬门槛。多源缺 sha
  返回 `400 input.multi_source_needs_sha`。

**dest 语义**：`dest` 默认是目录。要指定文件名用 `name`，不要靠尾斜杠。
`dest` 已经是一个文件时保留完整路径（兼容旧调用）。

## 3.5 上传

```http
POST /uploads
Content-Type: application/json

{
  "path": "~/outputs/checkpoint.safetensors",
  "url": "https://upload.example.org/files/checkpoint",
  "protocol": "tus",
  "sha256": "…",
  "credential_ref": "env:UPLOAD_TOKEN",
  "source": "agent:claude",
  "idempotency_key": "run-7f3/upload"
}
→ 202 {"id": "…", "state": "queued", "events": "/jobs/<id>/events"}
```

| 字段 | 必填 | 说明 |
|---|---|---|
| `path` | 是 | 本地源文件；必须在 `SKYGET_UPLOAD_ROOTS` 内 |
| `url` | 是 | PUT 目标 / tus 创建端点 |
| `protocol` | 否 | `put`（默认）或 `tus` |
| `sha256` | 否 | 本地文件期望指纹；不符就不上传 |
| `credential_ref` | 否 | `env:NAME`（需在 `SKYGET_UPLOAD_CREDENTIALS` 白名单）或 `file:PATH`（只能在 `SKYGET_HOME/secrets/` 下） |
| `source` / `idempotency_key` | 否 | 同下载 |

- 上传任务复用同一张台账、事件流、调度器和幂等语义；`TaskView.direction=upload`，
  `path` 是本地源文件，`url` 是远端目标。
- **默认禁止上传**：`SKYGET_UPLOAD_ROOTS` 为空时一律 `403 policy.upload_source_denied`。
- 目标地址走与下载相同的出网策略。
- 凭证值不落台账；只存引用。env 变量名必须显式列入 `SKYGET_UPLOAD_CREDENTIALS`，
  否则 agent 可以用 `env:AWS_SECRET_ACCESS_KEY` 把本机秘密当 token 发出去。
- `tus` 的 `Location`/offset 存在 `parts/tus-<job>.json`（0600），daemon 重启后自动续传；
  完成后删除。**Location 必须与上传地址同源**且通过出网策略；服务器想把后续
  HEAD/PATCH 指到别的主机/端口会被 `policy.egress_denied` 拒绝（防止带 Bearer
  凭证和文件内容打内网/元数据地址）。远端对象 404/410 时清 sidecar 并重建上传一次。
- 上传同样受取消和超时约束：每块/整请求按最低速度算超时，避免一个不读 body 的
  服务器长期占住全局 worker。阻塞中的 HTTP 请求无法被 signal 立即打断，
  取消会等当前请求超时或完成（已知残余）。
- 远端校验：PUT/tus 都没法保证远端内容 hash（协议没有统一回执）；任务成功表示
  “远端接受了全部字节”，`trust=local`，不假装是内容校验。S3 multipart + checksum 在路线图上。

## 4. 事件流

```http
GET /jobs/<id>/events?since=<seq>&wait_ms=<0-25000>&limit=<1-1000>
```

返回：

```json
{
  "job_id": "01a18863…",
  "events": [
    {"seq": 1, "ts": 1793289598011, "type": "queued", "data": {"source": "agent:claude"}},
    {"seq": 2, "ts": 1793289598123, "type": "planned", "data": {"bytes_total": 5284823040, "ranges": true, "threads": 5}},
    {"seq": 3, "ts": 1793289599011, "type": "progress", "data": {"bytes_done": 1073741824, "bytes_total": 5284823040}}
  ],
  "next": 3
}
```

- `seq` 在**每个任务内**从 1 连续递增。
- `since` 是**排他**的：只回 `seq > since`；客户端把返回的 `next` 存起来。
- `wait_ms > 0` 时长轮询：没有新事件就等，最多这么久（上限 25 秒）。
- 未知 id 且没有历史事件：`404`。
- 任务被删后事件仍保留（审计）；`GET /jobs/<id>` 会 404，事件流还能读
  （本机无身份 / 主 token；per-agent token 没有记录可用于归属校验，按 404 处理）。

事件类型：

| type | data |
|---|---|
| `queued` | `{source, priority}` |
| `probing` | `{}` |
| `planned` | `{bytes_total, ranges, threads}` |
| `progress` | `{bytes_done, bytes_total}` |
| `progress_secs` | `{seconds_done, seconds_total}` |
| `phase` | `{name, ms}`：`probe`（探测/首字节）、`transfer`（传输）、`verify`（算 sha + 结算 trust）、`land`（硬链接落地）、`cache-hit`（秒传命中）。用来回答「慢在哪」，不是总耗时（总耗时看 `elapsed_ms`） |
| `links` | `{live, seen}` |
| `named` | `{name}` |
| `note` | `{text}` |
| `verifying` | `{}` |
| `done` | `{cached}` |
| `failed` | `{error, code, retryable, retry_after_ms, bytes_done}` |
| `interrupted` | 同上 |
| `paused` | `{by}` |
| `cancelled` | `{by}` |

进度事件最多每秒一条；状态事件不丢。

## 4.5 缓存 pin

agent 用 pin 表达“这份内容我在用，prune 别清”。pin 按 `(sha256, agent)` 去重。

```http
POST /pins
{
  "sha256": "9f2c…",
  "agent": "agent:claude",
  "ttl_secs": 3600,
  "note": "model in use"
}
→ 201 {"sha256":"…","agent":"agent:claude","note":"…","created_at":…,"expires_at":…}
```

- `ttl_secs` 不填 = 7 天；`0` = 不过期（直到显式 unpin）。
- `GET /pins` → `Pin[]`（含已过期，调用方按 `expires_at` 判断）。
- `DELETE /pins/<sha>?agent=…` → `200`；不带 `agent` 去掉所有 agent 的 pin；没有则 `404`。
- `skyget prune` 不会清被有效 pin 保护的内容（`kept_pinned`）。
- CLI：`skyget pin <sha> --ttl 7d --as claude`、`skyget unpin <sha>`、`skyget pins`。

## 4.6 共享缓存直读

```http
GET /cas/<sha256>
Authorization: Bearer <token>
Range: bytes=0-1023        # 可选，单段
```

- 命中：`200` + `application/octet-stream` + `Content-Length` + `ETag: "<sha256>"`。
- 带 Range：`206` + `Content-Range: bytes a-b/size`；不满足：`416` + `Content-Range: bytes */size`。
- 缓存里没有：`404 {"code":"cas.not_found"}`。
- 只认 64 位十六进制；路径不会穿越。
- 用途：容器/局域网另一台机器拿同一个 token 直接从宿主缓存取，不重复走公网。
  `http://<host>:7788/cas/<sha256>` 本身就是一个支持 Range 的源。
- 任意自定义头仍不支持；私有源的 `Authorization: Bearer` 请用
  `skyget get --credential-ref env:NAME|file:PATH`（daemon 端要配
  `SKYGET_DOWNLOAD_CREDENTIALS` 和主机白名单），其他头用网关。`--header` 在路线图上。

## 4.7 增量同步（界面/GUI 用）

```http
GET /jobs/changes?since=<上次返回的 now>
{"now": 1793289600000, "items": [TaskView…], "deleted": ["<job_id>…"]}
```

- `since=0` 返回最近 500 条全量，`deleted` 为空。
- `since>0` 返回 `updated_at > since` 的任务（最多 500）和删除墓碑。
- `now` 是下次要带回的游标，取本批实际返回行/墓碑的最大时间戳；任务被截断时停在第 500 条，
  不会把没返回的行跳过。
- 删除墓碑落在 SQLite（`tombstones` 表），**daemon 重启后对 `since>0` 的客户端仍然可见**；
  默认保留 7 天，可用 `SKYGET_TOMBSTONE_RETENTION_DAYS` 调整。

## 4.7.5 秒进度（HLS/DASH）

```http
GET /jobs/<id>/seconds
Authorization: Bearer <token>
{"id":"…","unit":"seconds","seconds_done":420,"seconds_total":1800,"pct":23}
```

- 取该任务最后一个 `progress_secs` 事件；HLS/DASH 没有可靠总字节时，状态栏用它画条。
- token 调用方只能读自己的任务（跨 agent 404），与事件流一致。
- 没有秒进度时返回 `seconds_done=0` / `seconds_total=0` / `pct=0`。

## 4.8 状态栏（TUI 插件用）

```http
GET /statusline[?source=agent:pi]
Authorization: Bearer <token>
{"job_id":"01a18e…","state":"running","source":"agent:pi",
 "done":1234,"total":4096,"seconds_done":0,"seconds_total":0,"unit":"bytes","pct":30}
```

- 返回一条“最值得显示”的活跃任务：`running` > `pending` > `interrupted`（暂停），
  同类取最新创建；判断用的是展示状态（进程没了的 `running` 会按 `interrupted` 处理）。
- `done`/`total` 始终是字节；HLS/DASH 没有总字节时用 `seconds_done`/`seconds_total`
  计算 `pct`，并把 `unit` 标成 `seconds`，插件不用再请求一次。
- 没有活跃任务：`200 {"job_id":"","state":"idle","source":"","done":0,"total":0,
  "seconds_done":0,"seconds_total":0,"unit":"bytes","pct":0}`。
- `source` 是前缀过滤；带 agent token 的调用方只能看到自己的任务，主 token / 本机无鉴权
  调用可见全部。
- 插件按自己的刷新间隔轮询即可；不用拉 `GET /jobs?limit=500` 再自己实现选择规则。

## 5. 传输控制

| 方法/路径 | 作用 | 失败语义 |
|---|---|---|
| `POST /jobs/<id>/pause` | 暂停；已下分片保留 | 非本进程跑：`409` |
| `POST /jobs/<id>/resume` | 继续；分片接着用 | `404` |
| `POST /jobs/<id>/retry` | 重跑失败/中断任务（同记录、同期望 hash） | `404` |
| `POST /jobs/<id>/cancel` | 停止 + 清临时分片 + 删记录 | `404` |
| `DELETE /jobs/<id>` | 只删记录，盘上文件不动 | `404` |

`pause`/`cancel` 对排队中的任务也有效；排队任务被取消后不会再写回台账。

## 6. 失败分类（error_code）

调用方按 `error_code` 分支，**不要解析 `error` 的中文**。

| code | retryable | 含义 |
|---|---|---|
| `part.busy` | true | 另一个进程正在下同一地址 |
| `verify.mismatch` | false | sha256 不符，文件已丢弃 |
| `source.changed` | false | 同一地址内容变了（TOFU） |
| `source.range_lied` | false | 服务器假支持 Range；用 `--jobs 1` |
| `source.no_range` | false | 服务器不支持分段 |
| `http.throttled` | true | 429 / 503 |
| `http.auth` | false | 401 / 403，需要登录/授权 |
| `http.not_found` | false | 404 / 410 |
| `http.server` | true | 5xx |
| `network.timeout` | true | 超时 |
| `network.reset` | true | 连接被重置/中途断开 |
| `network.connect` | true | 连不上/DNS 失败 |
| `disk.full` | false | 磁盘满 |
| `disk.permission` | false | 目标目录无写权限 |
| `policy.egress_denied` | false | 地址指向私网/元数据，被策略拒绝 |
| `policy.dest_denied` | false | 落地路径不在允许根目录 |
| `policy.host_denied` | false | 域名不在白名单 |
| `policy.credentials_in_url` | false | URL 里带账号密码 |
| `queue.full` | true | 队列满 |
| `job.paused` | true | 已暂停 |
| `job.not_found` | false | 没有这条任务 |
| `job.external` | false | 另一个进程在跑，本服务停不了它 |
| `job.running` | false | 已经在跑，不能重复启动 |
| `daemon.restarted` | true | 守护进程重启导致中断/排队丢失 |
| `input.unsupported` | false | 不支持的地址/协议 |
| `input.bad_sha256` | false | sha256 不是 64 位十六进制 |
| `input.multi_source_needs_sha` | false | 给了多源镜像但没给 sha256 |
| `input.bad_dest` | false | dest/name 形状不对 |
| `input.bad_source` | false | source 超过 64 字节 |
| `input.bad_protocol` | false | 上传协议不是 put/tus |
| `input.bad_credential_ref` | false | credential_ref 形状不对 |
| `policy.upload_source_denied` | false | 上传默认关闭 / 源文件不在允许根目录 |
| `policy.credential_denied` | false | 凭证引用被白名单/secrets 目录拒绝 |
| `input.bad_json` | false | 请求体不是合法 JSON |
| `input.bad_url` | false | url 为空 |
| `internal.panic` | false | 任务内部 panic（程序 bug） |
| `internal` | false | 兜底 |

HTTP 层错误统一用 `{"error":"人话","code":"...","retryable":bool,"retry_after_ms":u64}`。
（0.19 里仍有少数分支只带 `error`，见 `docs/conformance.md` 的待办；不要依赖缺省字段。）

## 7. 策略与限制

daemon 默认是 **strict 出网策略**：

- 拒绝私有 / 回环 / 链路本地 / CGNAT（含 `169.254.169.254`、`100.100.100.200`）/ 组播 / 文档段。
- 每一跳重定向都重新检查。
- `SKYGET_ALLOW_PRIVATE=1` 显式放开私有地址（本地测试、内网镜像）。
- `SKYGET_ALLOWED_HOSTS=example.org,*.corp.net` 额外做域名白名单。
- `SKYGET_DEST_ROOTS=~/Downloads:~/models` 限制落地根目录（含 `..` 与符号链接逃逸检查）。
- daemon 命中缓存时会重算 sha256（`verify_cache`）；符号链接缓存块一律拒绝。
  CLI 独立下载默认不重算，`SKYGET_VERIFY_CACHE=1` 打开。
- `SKYGET_UPLOAD_ROOTS`：允许上传读取的本地根目录（冒号分隔）；**默认空 = 禁止上传**。
- `SKYGET_UPLOAD_CREDENTIALS`：允许 `credential_ref=env:NAME` 读取的环境变量名白名单
  （逗号分隔）；默认空。`file:PATH` 只允许 `SKYGET_HOME/secrets/` 下的文件。
- `SKYGET_AGENT_POLICIES=/path/agents.json`：per-agent 覆盖上面三项（以及
  `allowed_hosts`），未写的字段继承全局 strict；文件读不了/格式不对 daemon 拒绝启动。

  ```json
  {
    "claude": {
      "dest_roots": ["~/models", "~/Downloads"],
      "allowed_hosts": ["huggingface.co", "*.hf.co"],
      "upload_roots": ["~/outputs"],
      "max_running": 1
    }

  }
  ```

  - 名字必须和 `SKYGET_AGENTS` 里的 name 一致。
  - 身份来自 token，所以 agent 不能靠改请求体绕过自己的策略。
  - `max_running` 是每个 agent 同时跑几个任务；不填用全局 `SKYGET_MAX_PER_AGENT`。
    带宽/容量配额还没有按 agent 分开。

并发限制（环境变量）：

| 变量 | 默认 | 含义 |
|---|---|---|
| `SKYGET_MAX_JOBS` | 4 | 同时跑的任务数 |
| `SKYGET_MAX_QUEUED` | 1024 | 排队上限；满了回 429 |
| `SKYGET_MAX_PER_AGENT` | 2 | 单个 source 最多同时占几个 worker |
| `SKYGET_MAX_CONNS` | 128 | 接口连接上限（长轮询也算） |
| `SKYGET_PROGRESS_RETENTION_DAYS` | 30 | 进度事件保留天数；0 = 不清理 |
| `SKYGET_EVENT_RETENTION_DAYS` | 90 | 全部事件保留天数；0 = 不清理 |
| `SKYGET_TOMBSTONE_RETENTION_DAYS` | 7 | 删除墓碑保留天数；0 = 不清理 |

## 7.5 监控

```http
GET /metrics
Authorization: Bearer <token>
```

Prometheus 文本格式（`text/plain; version=0.0.4`）。包含：

- `skyget_up`
- `skyget_build_info{version=…}` / `skyget_uptime_seconds`
- `skyget_jobs{state="running"|"queued"}`
- `skyget_jobs_total{state=…}`（台账里的历史状态）
- `skyget_scheduler_capacity{kind="running"|"queued"}`（配置上限）
- `skyget_sources`（台账里出现过的来源数）
- `skyget_jobs_by_source{source=…}` / `skyget_running_by_source{source=…}`
  （只对内置来源和注册表里的 agent 单独建序列，其余自称来源合并为 `source="other"`）
- `skyget_cache_bytes` / `skyget_cache_blobs`
- `skyget_failures_total{code="http.404"|"policy.egress_denied"|…}`（历史失败任务按
  `error_code` 的计数，`code` 来自固定分类，不是调用方任意字符串）
- `skyget_download_bytes_total` / `skyget_cache_saved_bytes_total`
- `skyget_events_total` / `skyget_pins` / `skyget_tombstones`
- `skyget_denied_total{kind="auth"|"policy"|"credential"|"quota"}`（进程内累计，
  重启归零；400/404/429 不计入）
- `skyget_webhook_failed_total`（webhook 最终投递失败次数）

`/health` 是唯一免鉴权的端点；`/metrics` 和其他接口一样要 token。

```http
GET /health
→ 200 {"ok": true, "version": "0.24.8",
       "capabilities": ["http", "hls", "manifest", "bt", "gui"]}
```

`capabilities` 是这个**构建**真的编进去的面（`http` 恒在；`hls`/`manifest` 随 http；
`bt`、`gui` 按 feature 出现）。agent 可以先探测再决定走哪条路，不必读 skill。

## 7.6 谁在用这台数据盒子

```http
GET /agents
Authorization: Bearer <token>
```

```json
{
  "registry": true,
  "caller": "agent:claude",
  "policy": {
    "dest_roots": ["/srv/agents"],
    "allowed_hosts": ["huggingface.co"],
    "upload_roots": [],
    "allow_private": false
  },
  "scheduler": {"max_running": 4, "max_queued": 1024, "running": 1, "queued": 0},
  "agents": [
    {
      "name": "claude",
      "source": "agent:claude",
      "jobs": 12,
      "running": 1,
      "max_running": 2,
      "policy": {
        "dest_roots": ["/srv/agents/claude"],
        "allowed_hosts": ["huggingface.co"],
        "upload_roots": ["/srv/agents/claude/out"],
        "allow_private": false
      }
    }
  ]
}
```

- **token 永远不出现在响应里**；身份是 daemon 从 Bearer 解析出来的。
- 用 per-agent token 调用时只返回自己（`caller`）；旧单 token / 本机无鉴权时返回全部。
- `max_running` 是调度器里**实际生效**的上限（per-agent 策略 > 全局默认）。
- 顶层 `policy`：使用主 token/无身份时是 daemon 全局策略；使用 agent token 时只返回
  该 agent 实际生效的策略（不泄露别人的根路径/白名单）。agent 条目里的 `policy` 同理。
- 台账里出现过、但已经不在注册表里的 agent 也会列出（历史任务仍然可解释）。
- 没有 `SKYGET_AGENTS` 时 `registry:false`，名字来自台账 `source` 和策略文件。

## 8. 客户端

- CLI：`skyget --json --as claude --daemon <url> -o <dir> --sha256 <hex>`
  （agent 来源默认就走 daemon；`--standalone` 强制独立下载）。
- `skyget watch <id> [--since N]`：断线后接着读事件。
- `skyget retry <id>`：重跑失败/中断任务（同记录、同期望 sha）。
- `skyget top [--watch]`：终端驾驶舱（队列 / 进度 / 来源 / 信任 / 落点）。
- `skyget agents [--json]`：终端机群视图（`GET /agents`）。
- `skyget upload <path> <url> [--protocol put|tus] [--sha256 …] [--credential-ref …] --as <agent>`：
  上传；之后可用 `skyget watch <id>` 跟进度。
- `skyget export <dir> [sha…]` / `skyget import <dir>`：内容缓存搬运，双方都重算 sha256。
- `skyget doctor [--json]`：目录写权限 / 策略 / daemon 版本与连通性 / token 身份体检。
- MCP：`skynet-mcp`，工具 `skyget_want` / `skyget_upload` / `skyget_watch` /
  `skyget_status` / `skyget_retry` / `skyget_cancel` / `skyget_pin` / `skyget_unpin` /
  `skyget_agents`。
- GUI：同一个接口，自动带 `SKYGET_API_TOKEN`。

## 9. 还没做（别假装有）

- 上传的 S3 multipart（SigV4/预签名分片）与远端 checksum 回执；tus/PUT 已有。
- Metalink 清单解析（`sources` 数组已支持多源回退）。
- 多源同时下载（当前是顺序回退，不是分段并行）。
- 更多签名算法（当前是 ed25519，`skyget keygen` / `skyget sign` 产签）与
  多签（同一份清单签两把钥匙）。
- 分片级 Merkle、range 取、坏块修复。
- 缓存 quota（pin 已有，但没有按 agent 的容量配额/自动 lease）。
- 任意自定义 `--header`；私有源 Bearer 认证已可用（`--credential-ref`）。
  `/cas` 目前要调用方自己带 token。
- 上传中的阻塞式 HTTP 请求不能被 cancel 立即打断（只能等单请求超时）。
- API/事件已脱敏，但 SQLite 台账内仍保存真实预签名 URL（同 uid 可读）。
- per-agent token 已经有**任务/事件/pin 读隔离**（跨 agent 一律 404），但它不是
  本机文件系统隔离：能直接读 SQLite 台账的本机用户仍能看到全部记录；`/metrics` 和
  `/cas` 仍是机群级（任何有效 token）。需要强多租户时，一台 daemon 只给一个租户，
  或把 daemon 放进独立容器/账号。
- `/v1` 前缀；现阶段路径不带版本号，但字段变化会按契约处理。
