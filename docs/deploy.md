# 部署与策略

面向“一台机器、多个 agent、长期运行”的部署说明。只跑单次下载可以跳过。

> **平台边界：** 0.24.15 只正式支持 Ubuntu Linux x86_64。发布包为 `amd64`，
> 在 Ubuntu 26.04.1 LTS 上构建和验证，要求 glibc 2.39+。Ubuntu 22.04 及更早版本
> 不能直接运行；原生 Windows 尚未支持；macOS 只有源码探索，没有官方包。
> 见 [平台支持与路线图](platform-support.md)。

## 1. 安装

```bash
cargo build --release --features http,bt
install -Dm755 target/release/skyget          ~/.local/bin/skyget
install -Dm755 target/release/skynet-dataplaned ~/.local/bin/skynet-dataplaned
install -Dm755 target/release/skynet-mcp     ~/.local/bin/skynet-mcp
```

需要界面时再编 `--features full`，装 `skynet-dataplane`（GUI）。
旧名 `skynet-downloadd` / `skynet-download` 仍然可用。

Ubuntu 也可以直接构建安装包：

```bash
packaging/build-deb.sh            # 调 cargo build --release --features full
sha256sum -c dist/skynet-dataplane_*_amd64.deb.sha256
sudo apt install ./dist/skynet-dataplane_*_amd64.deb
systemctl --user enable --now skynet-dataplaned
```

包内包含 `skyget` / `skynet-dataplaned` / `skynet-mcp` / `skynet-dataplane`、
用户级 systemd unit、桌面入口和图标。只产出包不安装：去掉 `sudo apt install` 即可，
用 `dpkg-deb -x` 检查内容。

升级时先停止用户服务，再安装新包并重建 daemon：

```bash
skyget daemon stop
sudo apt install ./skynet-dataplane_<version>_amd64.deb
systemctl --user daemon-reload
skyget daemon ensure
skyget doctor
```

卸载程序不会删除台账、缓存、分片和配置：

```bash
sudo apt remove skynet-dataplane
# 如需一并清理用户数据，先确认路径再手动处理：
# ~/.local/share/skynet-download
# ~/.config/skynet-dataplane.env
```

## 2. systemd（用户服务）

仓库自带 `packaging/skynet-dataplaned.service`：

```bash
mkdir -p ~/.config/systemd/user
cp packaging/skynet-dataplaned.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now skynet-dataplaned
journalctl --user -u skynet-dataplaned -f
```

数据默认在 `$XDG_DATA_HOME/skynet-download/`：

```text
tasks.db     台账（SQLite）
blobs/       内容寻址缓存，同一份内容全机只存一份
parts/       未完成分片（.part + 位图）
secrets/     file: 凭证允许放这里
```

用 `SKYGET_HOME` 可以把整份数据搬到别的盘。

## 3. 身份：每个 agent 一个 token

unit 通过 `EnvironmentFile=-%h/.config/skynet-dataplane.env` 读取这份文件
（不存在也能启动；里面有 token，权限设 600）：

```bash
install -m 600 /dev/null ~/.config/skynet-dataplane.env
```

```ini
# ~/.config/skynet-dataplane.env
SKYGET_API_TOKEN=legacy-shared-token
SKYGET_AGENTS=tok_claude=claude,tok_codex=codex,tok_pi=pi
```

改完 `systemctl --user restart skynet-dataplaned`；`systemctl --user show-environment`
看不到这些值，它们只注入服务进程。

- `SKYGET_AGENTS` 开启后，daemon 从 `Authorization: Bearer <token>` 解析身份，
  请求体里的 `source` 不再可信。
- 带 token 的客户端只能看到/控制自己的任务：`/jobs`、`/jobs/<id>`、事件流、
  pause/resume/retry/cancel/delete 都按身份过滤；pin 的创建/列表/解除也只限
  自己；`/jobs/changes` 只对本机无 token 模式开放（GUI 用）。
- 这是 HTTP 面的隔离：能直接读 `SKYGET_HOME` 的本机用户仍能看到整本台账，
  那属于本机管理员权限，不是 agent 权限。
- 客户端设置该 agent 自己的 token：CLI 用 `SKYGET_API_TOKEN`，MCP 用
  `env.SKYGET_API_TOKEN`。不要把主 token 发给 agent，否则身份隔离/策略/配额都会
  退化成运维身份。
- `skyget agents` 会显示身份、并发、落地根、主机、上传根和今日配额；
  没有启用 token 时它会明确提示“来源仍是自称”。

## 4. 每 agent 策略

```ini
# 必须写成一行：EnvironmentFile 不支持跨行 JSON
SKYGET_AGENT_POLICIES='{"claude": {"dest_roots": ["~/models"], "allowed_hosts": ["huggingface.co", "*.hf.co"], "max_running": 2}, "pi": {"dest_roots": ["/srv/pi"], "upload_roots": [], "max_running": 1}}'
```

未写的字段（以及 `null`）继承全局 `SKYGET_DEST_ROOTS` / `SKYGET_ALLOWED_HOSTS` /
`SKYGET_UPLOAD_ROOTS`。`allowed_hosts` 的 `[]` = 拒绝全部；要“不限制”请显式
把全局 `SKYGET_ALLOWED_HOSTS` 也留空——per-agent 没有单独的“放开”开关。
`dest_roots: []` 会被拒绝（空根在兼容语义里是“不限制”，不是禁止）；要禁止落地
就给一个不可能命中的目录。

## 5. 配额与凭证

```ini
SKYGET_AGENT_QUOTAS="claude=10GB,pi=500MB"
SKYGET_DOWNLOAD_CREDENTIALS=HF_TOKEN
SKYGET_ALLOWED_HOSTS=huggingface.co,cdn-lfs.huggingface.co
HF_TOKEN=hf_xxx
SKYGET_COMPLETION_WEBHOOK="https://hooks.example.com/skynet"
```

`SKYGET_COMPLETION_WEBHOOK` 在任务终态（done/failed/interrupted）异步 POST 一次 JSON：
`id` / `state` / `source` / `direction` / `url`（去掉 query）/ `path` / `sha256` /
`bytes_total` / `error` / `error_code`。投递失败重试 3 次，只记 journal，不影响任务；
webhook 本身也走出网策略，内网地址需要 `SKYGET_ALLOW_PRIVATE=1`；它是部署方配置，
不受下载用的 `SKYGET_ALLOWED_HOSTS` 限制。取消会发 `cancelled`；单纯删除记录不发。

设置 `SKYGET_WEBHOOK_SECRET` 后，请求带
`X-Skynet-Signature: sha256=<hex>`（对原始 body 的 HMAC-SHA256）；接收方用同一
secret 重算即可验真。不设置就是无签名；webhook 不跟随重定向。

- 配额按 UTC 自然日统计 `done` 任务字节数（下载+上传）；超限 `quota.exceeded`，不落盘。
- `--credential-ref env:HF_TOKEN` 只在主源上发 `Authorization: Bearer`；
  HLS/DASH 和种子明确拒绝。
- **启用凭证必须同时配置 `SKYGET_ALLOWED_HOSTS`（或 per-agent `allowed_hosts`）**，
  否则提交会在入队前被拒（`policy.credential_needs_hosts`）—— 防止 agent 把 token
  发到任意公网主机。
- `file:` 凭证只能放在 `$SKYGET_HOME/secrets/`；目录 `0700`，文件必须没有组/其他
  权限位（`chmod 600`），否则会被拒绝。

## 5.3 生命周期：跟着 agent 起，而不是跟着开机起

三种模式，按场景选一种：

| 模式 | 怎么开 | 适合 |
|---|---|---|
| **随 agent 起**（默认） | 装服务但 `disable`；agent 一调 `skyget` / MCP 就按需拉起 | 开发者机器、笔记本：没人用的时候不该常驻 |
| **传统常驻** | `scripts/install-desktop.sh --with-service`（`enable --now`，配 `linger`） | 服务器、被 cron/外部系统直接调 HTTP 的场景 |
| **手动** | `skyget daemon ensure` / `stop` / `status` | 排障、演示 |

机制（随 agent 起这一档）：

```bash
skyget daemon ensure      # 没在跑就拉起：先 systemctl --user start，再退化为直启
skyget daemon status      # 在不在跑、谁托管、pid 是否真实、socket 是否真能连
skyget daemon stop        # systemd 服务或直接进程都能停；顺手清死 pid/失效 socket
```

- **客户端自带兜底**：`skyget` 的任何接口调用、以及 `skynet-mcp`，连不上时都会调
  `daemon ensure`（`SKYGET_AUTOSTART=0` 可关，且只对回环地址生效）。
- **宿主启动钩子**（更早热身）：`integrations/install.sh` 会往
  `~/.claude/settings.json` 的 `hooks.SessionStart` 写一条
  `skyget daemon ensure`；其它宿主没有钩子也没关系，靠上面的客户端兜底。
- **空闲退出**：`SKYGET_IDLE_EXIT=<秒>`（默认关）。满足三个条件才退：
  这段时间没请求、调度器里没有跑/排队的任务、台账里没有活任务。
- **崩溃收尾**：`kill -9` 无法运行任何清理代码，所以死 pid 和 socket 文件可能留下。
  `daemon ensure` 会替换它们，`daemon status` 会把 socket 标成“残留，连不上”，
  `daemon stop` 可以单独清理；读到文件存在不再等于“服务活着”。
- **多个 agent 共用一个 daemon**：谁先起谁拉起，后起的 `ensure` 看到健康就什么都不做。
  **不要**让每个 agent 起自己的子进程 —— 那会撞端口、把缓存和台账拆开，而且先退出的
  那个会把别人的 daemon 一起带走。
- **策略从哪来**：自动拉起优先走 systemd（`EnvironmentFile` 里的
  `SKYGET_ALLOW_*` / `ALLOWED_HOSTS` / token），**不继承调用方 agent 的环境** ——
  否则 agent 自己设个 `SKYGET_ALLOW_PRIVATE=1` 就能把数据面放宽。
  systemd 不可用时的直启会继承环境，这一档只当兜底。

> 服务端提醒：如果没人跑 CLI、只有外部系统直接 `curl :7788`，按需拉起**不会**发生
> （没有人触发它）—— 那种部署请用 `--with-service`。

## 5.4 Unix socket 身份（推荐）

不想给每个 agent 发 token，就让它连 socket：

```ini
# daemon 侧
SKYGET_UDS_AGENTS=1001=claude,1002=codex
```

```bash
# agent 侧（容器里把 socket 挂进去即可，不需要任何 secret）
export SKYGET_API=unix:/home/skynet/.local/share/skynet-download/daemon.sock
skyget --json get https://… -o ~/models
```

- socket 权限 `0660`：把 agent 加进同一个组，或让它跑成表里登记的 uid；
- daemon 自己的 uid 连上来等价于主 token（能看全部任务），其它 uid 只看自己的；
- 配了 `SKYGET_UDS_AGENTS` 而 uid 不在表里 → 直接 401，不会悄悄放行。

## 5.5 磁盘水位线与优先级

单机上最常见的故障不是下载慢，是**磁盘被缓存吃满**。daemon 可以自己兜住：

```ini
SKYGET_CACHE_MAX_BYTES=50GB      # 超过就把缓存清到低水位；不设 = 不自动清
SKYGET_CACHE_LOW_WATER=45GB      # 默认是上限的 90%
SKYGET_CACHE_GC_MIN_AGE=1h       # 刚下完的不算“没人要”
SKYGET_CACHE_GC_INTERVAL=5m      # 多久看一次（下限 10s）
```

门槛和 `skyget prune` 完全一致：被 pin 的、还有落地文件链着的（`nlink > 1`）、
台账里活着的、比最小年龄年轻的，一块都不删 —— 宁可超预算也不删刚下完的东西。

排队优先级由调用方给（`priority` / `--priority`），部署方不需要配；但要清楚它的
边界：**只改排队顺序，不改并发上限**。要限制某个 agent 能占几个 worker，用
`SKYGET_AGENT_POLICIES` 的 `max_running`。

## 6. 健康检查

```bash
skyget doctor
curl -sf http://127.0.0.1:7788/health
curl -s  http://127.0.0.1:7788/metrics | head
```

`/agents` 是 JSON 的机群视图；`/metrics` 是 Prometheus 文本格式，配额会以
`skyget_agent_quota_bytes` / `skyget_agent_used_today_bytes` 两个 gauge 暴露。

## 7. 升级

```bash
cargo build --release --features http,bt
install -Dm755 target/release/skyget ~/.local/bin/skyget
install -Dm755 target/release/skynet-dataplaned ~/.local/bin/skynet-dataplaned
systemctl --user restart skynet-dataplaned
skyget doctor
```

台账在启动时自动迁移；未完成分片会保留，重试接着下。

## 8. 边界

- 只接 http/https 直链、magnet/.torrent、HLS/DASH（HTTP）。
- 不做网盘分享页抓取、`thunder://`/`ed2k://`、DRM 或会员绕过。
- 私有源请走官方 API / 用户授权，用 `--credential-ref` 传凭证。

完整的信任边界、SSRF/凭证/路径控制和**残余风险**（DNS rebinding、同 uid 读台账、
webhook 无签名等）见 [安全模型](security.md)。
