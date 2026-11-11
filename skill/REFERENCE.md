# skyget-transfer · REFERENCE

细节都放这里 —— 它**不进**常驻上下文，宿主（或人）需要时再读。
命令、环境变量、错误码以 `docs/cli.md` / `docs/api-v1.md` 为准；这一页是"够用的一版"。

## Install（一次性动作，不属于 skill 的日常）

```bash
skyget --version || (cd <repo> && scripts/install.sh)   # 一条命令、幂等、--dry-run 可预演
systemctl --user restart skynet-dataplaned              # 或 skynet-downloadd
skyget doctor                                           # 全绿；同时打印 CLI 与 daemon 版本
```

- `scripts/install.sh` 无 sudo：二进制装成**实体文件**到 `~/.local/bin`（不是软链到
  `target/release`，否则 `cargo clean` 就把生产 daemon 指空），加文档、桌面入口、
  用户服务与 agent 接入。`PREFIX=` 换前缀，`--no-service` 跳过 systemd。
- 有 sudo：`sudo dpkg -i skynet-dataplane_<v>_amd64.deb`，再
  `systemctl --user enable --now skynet-dataplaned`。
- 从源码：`cargo build --release --features full && scripts/install.sh`。
- 容器/沙箱：挂 `$SKYGET_HOME/daemon.sock`（0660）进容器，`SKYGET_API=unix:/...`，
  **不要**把 token 拷进去。身份来自内核（uid → `SKYGET_UDS_AGENTS` 里登记的 agent 名）。
- macOS：CLI / daemon / MCP 都能编能跑；GUI 没有托盘（D-Bus 专属），关窗口即退出。

## 命令速查

| 命令 | 作用 |
|---|---|
| `skyget <URL> [-o DIR] [-n NAME] [--sha256 HEX]` | 下载（等价 `skyget get`）；`--sha256` 是硬闸门 |
| `skyget check <URL> [-o DIR] [--sha256 HEX] [--mirror U]` | 预检，不排队不联网；退出 0 = 会接受 |
| `skyget list [-l N] [--mine\|--agents\|--source P]` | 台账；默认 20 条，截断时会明说还有多少 |
| `skyget show <ID 或前几位>` / `show --url <URL>` | 查一条 / 按地址查最近一次 |
| `skyget pause\|resume\|cancel [--as A] <ID>` | 传输控制；暂停保留分片，取消删记录 |
| `skyget retry [--as A] <ID>` | 重跑失败/中断（同记录、同期望 sha） |
| `skyget watch <ID> [--since N]` | 事件流 + 最终结果；断线用同 id 接着读 |
| `skyget events <ID> [--since N] [--follow]` | 纯 NDJSON 事件流，到终态退出 |
| `skyget upload <路径> <URL> --protocol put\|tus --sha256 HEX` | 上传；源必须在 `SKYGET_UPLOAD_ROOTS` |
| `skyget ensure <清单> [--key K] [--require-signature]` | 幂等对账：对的不重下，只增改不删 |
| `skyget install <清单> [--key K]` | 装清单（总是落位） |
| `skyget keygen --out K` / `sign <清单> --key @K --in-place` | 生成/使用 ed25519 签名 |
| `skyget resolve <URL>` | 台账里下过的地址 → 回环 `/cas/<sha256>`（零网络） |
| `skyget curl [curl 参数]` | curl 替身：只接管「单地址 + `-o 文件`」，其余透传真 curl |
| `skyget verify [--deep] [--repair]` / `prune [--max-bytes] [--yes]` | 自检 / 清缓存 |
| `skyget doctor [--json]` / `agents [--json]` / `top` / `statusline` | 体检 / 机群 / 驾驶舱 / 状态栏 |
| `skyget completions bash\|zsh\|fish` | 补全脚本 |

全局：`--json`（stdout 一行 JSON）、`--timeout SECS`（到点不等，**不取消**任务）。

## 错误码（按 `error_code` 分支，不要解析人话）

| our code | 意思 | 可重试 |
|---|---|---|
| `http.*`（404/403/5xx） | 源的问题 | 5xx/429 可，4xx 不可 |
| `network.*` / `http.throttled` | 网不通 / 限流 | 可，按 `retry_after_ms` |
| `part.busy` | 另一个进程在写同一份 `.part` | 可，等 5s |
| `verify.mismatch` | sha 不符，文件已丢弃 | 不可（换源或核 sha） |
| `policy.egress_denied` / `policy.host_denied` | 私网 / 不在白名单 | 不可（部署方放行） |
| `policy.metadata_denied` | 云元数据地址（`169.254.169.254` 等） | 不可；要 `SKYGET_ALLOW_METADATA=1` |
| `policy.dest_denied` | 落地根不允许 | 不可（换目录） |
| `policy.credential_needs_hosts` | 用了凭证但没配白名单 | 不可 |
| `quota.exceeded` | 当日字节配额用尽 | 明天，或找部署方 |
| `job.running` / `job.not_running` | 控制命令与当前状态冲突 | 按 `next_action` |
| `cli.failed` / `cli.usage` / `cli.timeout` | CLI 层失败 / 用法错 / 超时 | 见 `next_action` |

## 环境变量（客户端侧）

| 变量 | 作用 |
|---|---|
| `SKYGET_HOME` | 数据根（台账/缓存/分片/密钥），默认 `~/.local/share/skynet-download` |
| `SKYGET_API` | daemon 地址，默认 `http://127.0.0.1:7788`；`unix:/path` 走 socket |
| `SKYGET_API_TOKEN` | 该 agent 自己的 Bearer token（**永不**写进参数或输出） |
| `SKYGET_ALLOWED_HOSTS` | 出网白名单；用凭证时**必须**配 |
| `SKYGET_ALLOW_PRIVATE` / `SKYGET_ALLOW_METADATA` | 内网 / 云元数据（两个独立开关，后者默认关） |
| `SKYGET_DEST` / `SKYGET_DEST_ROOTS` | 默认落地目录 / 允许的落地根 |
| `SKYGET_UPLOAD_ROOTS` | 允许上传读取的根；默认禁止上传 |
| `SKYGET_AGENT_QUOTAS` / `SKYGET_AGENTS` / `SKYGET_AGENT_POLICIES` | 部署方配置：配额 / token 注册表 / per-agent 策略 |
| `SKYGET_CACHE_MAX_BYTES`（+ `_LOW_WATER` / `_GC_MIN_AGE`） | 缓存水位线，daemon 侧自动清理 |

## 边界（诚实版，不是营销）

- 只接 http/https 直链、`magnet:`/`.torrent`、HLS/DASH。`thunder://`、`ed2k://`、
  网盘分享页、NZB、DRM **明确拒绝** —— 那些不是协议，是商业锁。
- BT v2-only 种子不支持；BT 对等端可以在内网（不受 host 白名单约束）。
- HLS/DASH 的分片由 ffmpeg 自己取，不在 Rust 侧逐条策略之内。
- 优先级只改**排队顺序**，不抢占运行中的任务；没有 per-agent 带宽上限。
- `--timeout` 只让 CLI 不再等；分片留着，重跑接着下。
- 上传没有 S3 multipart；远端没有统一 digest，所以成功只标 `trust=local`。
- 签名是"验"：`--key` / `SKYGET_TRUSTED_KEYS` 显式给公钥，没给就 fail closed。
- 同 uid 的进程本来就能互相代替（能读同一份数据目录）；要隔离请用不同 uid + `SKYGET_UDS_AGENTS`。
- `check` 不联网 → 它**不知道文件多大**，所以不能替你判断"磁盘够不够"。

## 各宿主怎么拿到这份 skill

一个文件，放在宿主自己的位置即可。`scripts/install.sh` 会把同一份字节铺到下面这些地方
（`integrations/skill_sync.py --check` 在 CI 里核对逐字节一致）：

| 宿主 | 它读什么 | 安装方式 |
|---|---|---|
| **Claude Code** | `~/.claude/skills/<name>/SKILL.md`（front-matter `name` + `description`） | 软链该目录 |
| **Hermes** | `~/.hermes/skills/<name>/SKILL.md` + 常驻块在 `~/.hermes/SOUL.md` | 软链 + 块 |
| **OpenCode** | `opencode.json` 的 `skills: [<目录>]` + `~/.config/opencode/AGENTS.md` | 加路径 + 块 |
| **pi** | `~/.pi/agent/settings.json` 的 `skills: [<目录>]` | 加路径 |
| **Codex** | `~/.codex/AGENTS.md`（常驻块） | 写块 |
| **Copilot** | `AGENTS.md`（CLI / coding agent）；VS Code 工作区用 `.github/copilot-instructions.md` | 写块 / `install.sh --repo <dir>` |
| **任意宿主** | `~/AGENTS.md` 的常驻块，或 `~/.local/share/skyget/skill/skyget-transfer/SKILL.md` | 写块 / 自取 |

常驻块 = 同一份文件的正文（去 front-matter 和 H1），由 `integrations/skill_block.py`
生成，所以**不存在第二份手抄内容**。

## 没有 shell、没有网络的宿主怎么办

有些运行时的工具集只有"读文件"（例如只有 `read_file` / `search_files`）。
这类宿主能**理解**这个 skill，但**执行不了** `skyget`。诚实的做法：

1. 说清楚"我需要一个能执行 skyget 的工具"，不要假装完成；
2. 让宿主接 `skynet-mcp`（11 个工具，权限落在它自己的 registry）；
3. 或由它给出"下什么 + 期望 sha256"，交给有 shell 的宿主执行。

能力探测：`GET /health` 的 `capabilities`（`http/hls/manifest/bt/gui`）说明这个 daemon
能干什么；`skyget doctor --json` 说明这台机器配得对不对。

## 出处

- `docs/cli.md`（全部命令与参数）、`docs/api-v1.md`（HTTP 契约 v1）
- `docs/security.md`（威胁边界与残余风险）、`docs/release-notes-0.24.md`（已知边界）
- `docs/conformance.md`（每条契约对应哪个测试）、`CLAUDE.md`（仓库内开发规则）
