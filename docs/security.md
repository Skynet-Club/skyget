# 安全模型与威胁边界

适用对象：`skynet-dataplaned`（旧名 `skynet-downloadd`）+ `skyget`，在一台机器上
为多个 agent 提供下载/上传。目标不是替代 OS 沙箱，而是保证数据面不会变成 agent 的
**confused deputy**：agent 可以提交任务，但不能借数据面读本机秘密、打内网、写任意
路径，或花掉别的 agent 的配额。

这份文档描述**已经实现的边界**和**没有解决的残余风险**。安全结论要能对上代码：
关键位置在 `src/security.rs`、`src/credential.rs`、`src/quota.rs`、`src/api.rs`、
`src/webhook.rs`。

## 1. 信任边界

| 主体 | 信任级别 | 能做什么 | 不能假设什么 |
|---|---|---|---|
| 本机 OS 用户（daemon 的 uid） | 管理员 | 读 `SKYGET_HOME`、改环境、杀进程、直接跑 `skyget` | 不防“本机同 uid 的进程”；那是 OS 权限问题 |
| Agent 进程（持 token） | 不可信 | 提交/查看/控制**自己**的任务 | 可能被 prompt injection 驱动，提交任意 URL/路径 |
| 下载源 / 镜像 | 不可信 | 返回字节 | 不假设内容正确；sha 是唯一硬闸门 |
| HLS 子清单 / 种子 peer | 不可信 | 提供切片/块 | 不假设对端是良性的 |
| Webhook 接收方 | 运维配置，但不默认可信 | 收任务元数据 | 默认无签名（可选 HMAC）；未设置 secret 时 payload 可被伪造/重放 |
| daemon 本身 | 可信代码 | 执行策略、落盘、出网 | 不能假设调用方说的是真话 |

## 2. 网络出站

- **协议**：只接 http/https 直链、`magnet:`/`.torrent`、HLS/DASH（HTTP）。
  不做 ftp/sftp/webdav、网盘分享页抓取、`thunder://`/`ed2k://`、DRM 或会员绕过。
- **URL 形状**：带 `user:pass@` 的地址直接拒绝（`policy.credentials_in_url`），
  避免凭证进日志；预签名 URL 允许，但所有对外视图会脱敏 query。
- **私网/元数据**：daemon 的 `Policy::strict()` 默认拒绝回环、RFC1918、链路本地
  （含 `169.254.169.254`）、CGNAT（含阿里云 `100.100.100.200`）、组播、保留/文档段、
  IPv6 的 `::1`/`fc00::/7`/`fe80::/10`/IPv4-mapped 等。域名会先解析，逐个 IP 检查。
- **云元数据单独一档**：`169.254.169.254` / `100.100.100.200` / `fd00:ec2::254`
  即使在 `SKYGET_ALLOW_PRIVATE=1` 下也**仍然拒绝**（`policy.metadata_denied`）；
  要放行得显式设 `SKYGET_ALLOW_METADATA=1`。理由：开 `ALLOW_PRIVATE` 是为了公司内网
  镜像，不该顺带把「agent 能读云凭证」打开。
- **IPv6 字面量**：按 IP 直接判（不走 DNS）。0.24.6 之前 `host_str()` 带方括号导致
  `to_socket_addrs()` 失败，**所有 IPv6 地址都被误报成「域名解析失败」**。
- **重定向**：每一跳重新走 `check_url`，最多 5 跳；上传明确禁用重定向。
- **Host 白名单**：`SKYGET_ALLOWED_HOSTS` 或 per-agent `allowed_hosts`；支持
  `*.example.com`，`[]` 表示拒绝全部。白名单在每一跳都生效。
- **浏览器来源**：带 `Origin` 的请求一律 403；POST 必须是 `application/json`。
  这是防网页借 `127.0.0.1` 发简单请求产生副作用（CSRF/本地 SSRF）。
- **监听地址**：daemon 固定绑 `127.0.0.1`，不提供 `0.0.0.0` 开关。
  同机容器要读缓存就把数据目录挂进去，或用 `socat`/反代把它接进去；
  跨机分发用 `skyget export/import` 或 BT，不做“带 token 的公共缓存服务器”。

- **Unix socket**：`$SKYGET_HOME/daemon.sock`，权限 `0660`。身份由
  `getsockopt(SO_PEERCRED)`（Linux）/ `getpeereid()`（macOS）给出，**无法被请求体伪造**：
  - daemon 自己的 uid → 等价于主 token（本来就能读整份数据目录）；
  - `SKYGET_UDS_AGENTS` 里登记的 uid/gid → `agent:<name>`，配额/策略/task 隔离照旧；
  - 配了映射表但 uid 不在表里 → `401 auth.unknown_peer`。
  客户端侧 `SKYGET_API=unix:…` 会在进程内起一个回环转发（随机端口 + 随机路径前缀口令），
  daemon 看到的仍然是调用方的 uid。

### 2.1 残余风险

- **同 uid 的进程可以互相代替**：这是 uid 模型的边界，不是新引入的 —— 它们本来就能
  读同一个 `SKYGET_HOME`。真要隔离 agent，请让它们跑在不同 uid 下。
- **IPv6 字面量**：`http://[fd00:ec2::254]/` 这类地址按 IP 直接判（不走 DNS）；
  0.24.6 之前它们会被误报成「域名解析失败」，等于**所有 IPv6 地址都被拒**。
- **DNS rebinding / TOCTOU**：`check_url` 解析一次，reqwest 连接时可能再解析一次；
  两次之间 DNS 可以变。彻底修复需要自定义 resolver 把结果钉死。当前靠 host 白名单
  与主机层出网策略兜底，代码里明确写了这个窗口。
- **BT peer**：`magnet:` 没有 URL 主机可查；BT 的对等端连接不受 host 白名单约束。
  `http(s)://….torrent` 的初始 URL 先过 `check_url`，但 librqbit 内部抓取可能跟随
  重定向，重定向目标不再过策略。BT 是可选 feature，且应当只在隔离网络/可信用途
  下开启，不要把它当 SSRF 边界。
- **Webhook**：初始 URL 过严格出网检查，webhook 是部署方配置，不受下载 host
  白名单限制；客户端**不跟随重定向**（3xx 按失败重试），避免 302 到内网/元数据。
  内网 webhook 需要 `SKYGET_ALLOW_PRIVATE=1`。
- **HLS/DASH 子请求**：子清单/分片由 ffmpeg 发起，daemon 没法逐条套出网策略
  （已用输入协议白名单挡掉 `file://`）。受限模式（严格出网或 host 白名单）默认
  拒绝；`SKYGET_ALLOW_STREAM_EGRESS=1` 是显式信任播放列表的逃生口，开启后
  ffmpeg 的子请求可能打到内网/元数据地址。
- **daemon 日志**：stderr/journal 里的错误可能包含 agent 可控的 URL/路径片段。
  CLI 的人类输出已去控制字符，日志本身没有；在共享主机上看不可信来源的日志时，
  注意终端转义序列（或在隔离终端里看）。

## 3. 身份与授权

- **`SKYGET_AGENTS="tok_claude=claude,tok_pi=pi"`**：token → agent 身份。请求体里的
  `source` 不再可信，台账写入 `agent:<name>`。配置写错（一个合法条目都没有）会
  fail closed，daemon 不启动，而不是退回无鉴权。
- **`SKYGET_API_TOKEN`**：主 token（GUI/运维/监控），有效但**不冒充**任何 agent；
  能看全部任务。
- **两者都没配**：本机无 HTTP 鉴权（兼容旧行为）。能在这台机器上跑进程的人本来就能
  直接跑 `skyget`；但如果这台机器还有别的低权限用户，请务必启用 token。
- **端点**：`/health` 是唯一免鉴权端点；`/metrics`、`/cas`、`/jobs`、`/pins`、
  `/agents`、`/statusline` 在配置了 token 后都要 `Authorization: Bearer …`。
- **隔离**：带 agent token 的调用方只能读/控制自己的任务、事件、pin；
  跨 agent 的 `list`/`get`/`events`/`pause`/`resume`/`retry`/`cancel`/`delete` 一律
  按 404 处理（不泄露“存在但无权”）；`/jobs/changes` 对 token 调用方关闭。
  `/statusline` 同样按 token 过滤。幂等键也按来源隔离：不同 agent 用同一个 key
  不会 dedupe 到对方的任务 id/状态。
- **审计**：`pause`/`resume`/`retry`/`cancel` 事件记录 `by=agent:<name>` 或 `by=api`；
  token 值不会出现在接口、事件或日志里。`/metrics` 暴露
  `skyget_denied_total{kind="auth"|"policy"|"credential"|"quota"}`（进程内累计，
  重启归零），用来发现有人在反复试边界。
- **人类终端输出**：`skyget list/show/watch/top/pins/agents/verify/install` 在非 JSON
  模式下会去掉控制字符（ESC/OSC 等），agent 提供的 URL/路径/错误不能操纵运行命令
  的终端；JSON/NDJSON 保持原值，由调用方按自己的协议处理。
- **本机台账不是 HTTP 边界**：能直接读 `SKYGET_HOME/tasks.db` 的同 uid 进程能看到
  整本台账（含预签名 URL）。需要强多租户时，用不同 OS 用户/容器运行不同 agent，
  而不是只靠 token。
- **`/metrics` 与 `/cas` 是机群级只读面**：`/metrics` 任何有效 token 可读（只有计数、
  缓存大小、每来源任务数/配额，不含 URL/文件名）；`/cas` 是全局内容缓存，任何有效
  token 知道 sha 就能读。需要按 agent 隐藏活动/内容时，不要把这两个端点直接暴露给
  不可信 token（或放在网关后按身份路由）。
- **HTTP 解析上限**：请求行/头行 16KiB、头 100 个（超限 431），POST body 1MiB，
  并发连接 128；防止本机 agent 用超长头/连接耗尽内存。

## 4. 文件系统

- **下载落地**：`SKYGET_DEST_ROOTS` / per-agent `dest_roots`。检查会把已存在的最深
  祖先 canonicalize 后再比较，能发现 `~/Downloads/link-to-etc/passwd` 这类符号链接
  逃逸；悬空链接、最终文件是符号链接都拒绝。
  **空列表 = 不限制**（兼容旧行为）——生产部署必须显式设置。
- **上传读取**：`SKYGET_UPLOAD_ROOTS`，默认空 = **禁止上传**。源必须是允许根目录内
  的普通文件，不能是符号链接。
- **凭证文件**：`file:` 只允许 `$SKYGET_HOME/secrets/` 下、canonicalize 后仍在根内的
  文件；目录 `0700`，文件必须没有组/其他权限位（建议 `0600`），否则直接拒绝
  （`policy.credential_denied`）—— 0644 的 token 对本机其他用户可见。
- **数据目录**：台账、缓存、分片默认在 `$XDG_DATA_HOME/skynet-download`。**新建**的
  数据/缓存/分片/secrets 目录按 0700 创建；已存在的目录不会强行 chmod（部署方可能
  是有意共享），共享主机上请自己用 `umask 077` 和 0700 的 `SKYGET_HOME`。
  当前开发机的 `~/.local` 是 0700，但 `skynet-download/` 本身是 0775、`tasks.db`
  是 0644 —— 这正是需要部署方自己收紧的例子。

## 5. 凭证（私有源）

- **只存引用**：`credential_ref` 是 `env:NAME` 或 `file:PATH`，值只在用时读，
  绝不写进台账、事件、API 返回或 webhook。
- **Env 白名单**：下载 `SKYGET_DOWNLOAD_CREDENTIALS`，上传
  `SKYGET_UPLOAD_CREDENTIALS`；不在白名单里的变量名拒绝。
- **只发给主源**：多源任务的镜像可能是第三方，镜像请求不带 `Authorization`；
  镜像靠 sha 判断内容。HLS/DASH 与种子明确拒绝 `credential_ref`，不静默忽略。
- **必须有 host 白名单**：使用凭证时 `SKYGET_ALLOWED_HOSTS`（或 per-agent
  `allowed_hosts`）为空会被拒绝（`policy.credential_needs_hosts`），防止 agent 把
  token 发到任意公网主机。凭证会跟随策略允许的重定向；白名单越窄越好。
- **脱敏**：URL userinfo、签名类 query（`token`/`signature`/`X-Amz-*` 等）、
  错误文本里的 URL、事件流和 webhook 的 `error` 字段都会脱敏。台账内部保留原始 URL
  供重试，本机同 uid 的读取者仍能看到（见 §3 末）。
- **残余**：token 值存在于 daemon 进程环境；同 uid 可读 `/proc/<pid>/environ`。
  对端 TLS 终端（源站）当然能看到 token。

## 6. 配额与资源

- **调度器**：固定 worker（默认 4）、有界队列（默认 1024）、per-agent 并发上限
  （默认 2，`SKYGET_MAX_PER_AGENT`/策略 `max_running` 覆盖）；任务 panic 不会打死
  worker。队列满回 429 + `retry_after_ms`。
- **每日字节配额**：`SKYGET_AGENT_QUOTAS="claude=10GB"`，按 UTC 自然日统计 `done`
  任务的 `bytes_total`（下载+上传）。下载在探测到总量后、上传在算 hash/传字节前
  检查；超限 `quota.exceeded`，不落盘/不上传。
- **fail closed**：配额值解析失败按 0 处理（该 agent 全拒）；语法错误（缺 `=`、
  空 agent 名）按全局 0 处理（所有 agent 全拒），并在 stderr 警告一次。不当作“不限”。
- **已知取舍**：并发任务可能让日用量超出“一个正在下载的文件”；HLS/DASH 与种子没有
  可靠总字节，无法在开始前精确检查；缓存命中不联网，不计入配额。需要硬上限时，
  用 per-agent 并发 + 配额 + 监控一起，而不是只靠配额。

## 7. 完整性

- **sha256 是硬闸门**：给了 `sha256` 就校验，不符即失败并丢弃文件；多源必须给 sha。
- **缓存捷径也校验**：命中缓存时会核对 blob 内容（`verify_cache`），不对就重新下载。
- **信任级别诚实标注**：`pinned`（调用方给的）/`indexed`（独立校验文件）/
  `tofu-new`/`tofu-ok`（首次/与历史一致）/`torrent`（种子自证）/`none`（没校验）。
  没有 sha 时不会假装“校验通过”。
- **HLS/DASH**：顶层 URL 过出网策略；子清单和分片是 ffmpeg 自己取的，
  **不经过** daemon 的逐条策略。ffmpeg 的输入协议已限制为
  `http,https,tls,tcp`（挡 `file://` 读本机文件）；只要策略本来在限制出网
  （严格模式或配了 host 白名单），默认拒绝 HLS/DASH；确认播放列表可信后才可设
  `SKYGET_ALLOW_STREAM_EGRESS=1` 放行。完成时按 `expected_sha256` 校验（若提供）。
- **种子**：BT v1 infohash 是 SHA-1，不是 SHA-256；`trust=torrent`，不能用它当
  “发布方签名”。
- **清单**：`skyget install` / `ensure` 要求每个文件带 64 位 sha256（没有就拒绝），
  清单 URL 过出网边界，落地路径过 dest 边界，权限只保留 `0o777`（不装 setuid）。
- **清单签名（ed25519）**：清单可带 `signature: {key_id, sig}`，签名覆盖规范化摘要
  （`skyget-manifest-v1` + name/version/排序后的 files(dest,sha256,mode,url)/bin/usage）。
  `--key <hex|id=hex|@file>` 或 `SKYGET_TRUSTED_KEYS` 提供可信公钥。
  **fail closed**：清单带签名但没有可信公钥 → 拒绝；给了公钥但清单没签名 → 拒绝。
  签名用 `skyget keygen` / `skyget sign` 产生（私钥 PKCS#8 DER，写盘 0600）。
  验签失败与「内容被改」是同一个结论：不装。
- **TLS**：使用 reqwest 默认的系统根证书校验，没有证书 pinning。

## 8. Webhook

- 配置：`SKYGET_COMPLETION_WEBHOOK`，只在 daemon 里发，异步，不阻塞任务终态。
- 触发：`done` / `failed` / `interrupted` / `cancelled`；单纯删除记录不发。
- Payload：`id`/`state`/`source`/`direction`/`url`（去掉 query）/`path`/`sha256`/
  `bytes_total`/`error`（脱敏）/`error_code`。
- 投递：5 秒超时，3 次重试；失败只记日志，不影响任务。
  `/metrics` 的 `skyget_webhook_failed_total` 记录最终失败次数。
- **可选 HMAC 签名**：设置 `SKYGET_WEBHOOK_SECRET` 后带
  `X-Skynet-Signature: sha256=<hex>`（对原始 body 的 HMAC-SHA256）；接收方重算验真。
  不设置就是无签名——把 webhook 地址当秘密、只走 HTTPS、在接收端做来源校验/幂等。
- **不跟随重定向**：3xx 按投递失败重试，避免 302 到内网/元数据。
- **不转发凭证**：payload 不含 `Authorization`、token、query 签名。

## 9. 部署检查清单

```bash
# 0) 专用用户 + 收紧 umask/目录
umask 077
export SKYGET_HOME="$HOME/.local/share/skynet-download"

# 1) 身份：per-agent token（强烈建议）
export SKYGET_AGENTS="tok_claude=claude,tok_codex=codex,tok_pi=pi"
# 可选：人类 GUI/监控用主 token
export SKYGET_API_TOKEN="$(openssl rand -hex 32)"

# 2) 边界：落地根、主机白名单、上传根
export SKYGET_DEST_ROOTS="$HOME/Downloads:$HOME/models"
export SKYGET_ALLOWED_HOSTS="huggingface.co,cdn-lfs.huggingface.co"
# 不上传就不要设置 SKYGET_UPLOAD_ROOTS
export SKYGET_AGENT_POLICIES='{"pi":{"dest_roots":["/srv/pi"],"max_running":1}}'

# 3) 配额
export SKYGET_AGENT_QUOTAS="claude=10GB,pi=500MB"

# 4) 凭证（可选；必须同时有 host 白名单）
export SKYGET_DOWNLOAD_CREDENTIALS=HF_TOKEN
export HF_TOKEN=...
mkdir -p "$SKYGET_HOME/secrets" && chmod 700 "$SKYGET_HOME/secrets"
```

验收：

```bash
skyget doctor                 # 看警告，尤其是“来源仍是自称”“落地根未限制”
curl -s http://127.0.0.1:7788/health
curl -s -H "Authorization: Bearer $SKYGET_API_TOKEN" \
  http://127.0.0.1:7788/metrics | grep skyget_
```

主机层再加一道：daemon 只监听 `127.0.0.1`；如果 agent 在容器/别的机器上，通过
受控的本地网关暴露，不要把 7788 直接开到公网。

## 10. 报告问题

发现绕过边界的问题时，不要在公开 issue 里贴 token、签名 URL 或可复现的私有地址。
先私下联系维护者，附最小复现；修复并发布后再公开细节。
