# 天网盒子 0.24 · 发行说明

> 命令、二进制名、systemd 单元、数据目录、`app_id` 全部兼容 0.23；
> 显示名从「天网数据面」改为「天网盒子」，旧名照常工作。
> 契约仍是 **API v1**（`docs/api-v1.md`）。

## 0.24.15：稳定性收尾

这一版没有新协议，只把 0.24.14 的生命周期与发布证据补齐：

- `daemon status` 不再把“socket 文件存在”当成“服务可用”，同时报告真实 pid 与 socket 是否可连。
- `daemon stop` 能停 systemd 服务，也能停 `daemon ensure` 直接拉起的进程；
  进程已死时会清掉残留 pid / 失效 socket。
- 新增真 daemon 生命周期 e2e，覆盖残留恢复、status、直接进程 stop、重复 stop、空闲退出。
- CI 的 skill 检查改用现行 `skill_sync.py`；干净 HOME 不再把“未安装”误报成漂移。
- 测试证据、e2e 数量、README 的“还没做”清单统一到当前实现。

## 平台支持与系统要求

**0.24.15 的唯一正式支持平台是 Ubuntu Linux x86_64。**

- Release 产物：`skynet-dataplane_0.24.15_amd64.deb`；
- 构建与验证环境：Ubuntu 26.04.1 LTS x86_64；
- 二进制最高要求 `GLIBC_2.39`；
- Ubuntu 24.04 LTS 满足 ABI 下限，但本版没有单独做 24.04 发行验收；
- Ubuntu 22.04 及更早版本不能直接运行；
- systemd 用户服务可选，非 systemd 环境可由 `skyget daemon ensure` 直接拉起；
- HLS/DASH 需要额外安装 `ffmpeg`，其它核心能力不依赖 ffmpeg；
- 原生 Windows 10/11 尚未支持；
- macOS 只有源码探索、交叉检查和 CI 配置，没有官方发行物或支持承诺；
- WSL2 中运行 Ubuntu 走 Linux 支持路径，不等于原生 Windows 支持。

Windows/macOS 的目标、当前状态和技术工作见
[平台支持与路线图](platform-support.md)。

## 这一版真正变的是什么

0.24 的主线只有一句：**从「能下载」变成「一台机器上多个 agent 可以放心共用的数据面」。**

| # | 能力 | 一句话 |
|---|---|---|
| 1 | **Unix socket + 内核身份** | `$SKYGET_HOME/daemon.sock`（0660），对端 uid 由内核给，**不用分发 token**；登记过的 uid 直接得到 `agent:<name>`，登记表里没有就直接 401。 |
| 2 | **清单签名（ed25519）** | `sha256` 只证明「字节和某个 hash 一致」，签名才证明「这个 hash 是发布方给的」。`skyget keygen` / `sign` / `manifest-digest` 产签，`install` / `ensure` 验，**fail closed**。 |
| 3 | **缓存水位线** | `SKYGET_CACHE_MAX_BYTES` 之后 daemon 自己把缓存清到低水位。单机数据面的头号故障不是下载慢，是磁盘被缓存吃满。 |
| 4 | **`skyget check` + `POST /check`** | 提交前先问「会被怎么判」，和真提交**共用同一段判定代码**，逐项给出 identity/protocol/mirrors/sha256/credential/dest/upload_source/egress。 |
| 5 | **`skyget ensure`** | 从「下什么」升级成「这台机器该是什么样」：对的不联网不写盘，只增改不删。 |
| 6 | **多源说谎时能救回来** | 校验不符改为逐个源单独重试。**0.23.1 线上版本在主源说谎时会整条失败**，即使旁边有健康镜像。 |
| 7 | **macOS 源码探索** | `/proc` 依赖收进 `src/sys.rs`（macOS 走 libproc）；有交叉检查，但 0.24.15 不提供 macOS 官方包。 |
| 8 | **调度与可观测** | 排队优先级（interactive/normal/batch）、事件流 `phase` 耗时、`check`、`pause/resume/cancel`、`events`、`--timeout`、`--json` 在所有失败路径上都是一行。 |

## 对源站的礼貌（可核对，不是口号）

我们要求自己做一个"源站愿意服务的客户端"。下面每条都能在代码或测试里对上：

| 做法 | 具体数字 / 行为 |
|---|---|
| **并发有上限** | `-j` 默认按体积 **4–16**（不是 32/128）；同一主机**跨任务**共享连接上限（`SKYGET_MAX_PER_HOST`，默认 16） |
| **被限流就退** | 429/503 自动降速（节流阀），**按服务器给的 `Retry-After` 等**；最多等 5 分钟，再长就判定换源/失败，而不是挂死 |
| **UA 诚实** | `skynet-download/<版本>`，不伪装 Chrome；不发伪造的 Referer |
| **不碰登录态** | 不读取浏览器 cookie、不逆向网盘私有 API、不破 DRM、不绕会员 |
| **续传看 ETag** | `url + 大小 + ETag` 任一项变了就**整份重来**，绝不把新文件的尾巴接到旧头上 |
| **校验是闸门** | sha256 不符即失败并丢弃；多源时内容哈希是唯一的裁判 |
| **不无限重试** | 最多 4 次；"源不支持分段/内容变了/校验不符"这类**一次都不重试** |
| **明确拒绝** | `thunder://`、网盘分享页、`ed2k://`、NZB、DRM、YouTube 抓取 —— 不是协议，是商业锁 |

理由很功利：**我们是别人源站的客户端。多一个守规矩的客户端，对方少一次事故；
被拉黑一次，所有用户的下载都会变慢。** 快应该来自分段、多源和自动并发，
不该来自"多开连接硬撞"。

## 已知边界（这一版没做，且不打算假装做了）

1. **优先级只改排队顺序，不抢占**。一个跑到一半的 batch 任务不会被 interactive 打断；
   要限制某 agent 能占几个 worker，用 `SKYGET_AGENT_POLICIES` 的 `max_running`。
   **没有 per-agent 带宽上限**（只有每日字节配额 `SKYGET_AGENT_QUOTAS`）。
2. **`--timeout` 只让 CLI 不等，不取消任务**。分片留着、重跑接着下；daemon 里的任务继续跑。
   不做优雅取消的原因：CLI 的等待可能卡在 DNS / TLS / 长轮询 / 写盘任意一处。
3. **`skyget check` 需要 daemon 在**。它问的是 daemon 那份策略（含 per-agent 覆盖），
   不是本地猜；daemon 不可达时它自己会失败，而不是给你一个"看起来行"的答案。
4. **上传没有 S3 multipart**。`PUT` 与 `tus 1.0`（断点续传）有；S3 multipart + checksum 在路线图上。
5. **签名是「验」，不是「信任根」**。没有 `require_trust` 字段；可信公钥由
   `--key` / `SKYGET_TRUSTED_KEYS` 显式给出。没有配公钥时，清单必须带 sha256 才装。
6. **云元数据单独一个开关**：`SKYGET_ALLOW_PRIVATE=1` 不会连带放开
   `169.254.169.254` / `100.100.100.200` / `fd00:ec2::254`；要放行得设
   `SKYGET_ALLOW_METADATA=1`（错误码 `policy.metadata_denied`）。
7. **平台发布边界**。0.24.15 只提供 Ubuntu Linux x86_64 官方发行物；
   原生 Windows 尚未支持，macOS 只有源码探索与交叉检查，没有官方包或支持承诺。
8. **`verify` 的「无主缓存块」只是信息**，不会自动删（可能对 `--sha256` 秒传仍有用）。
9. **两条一致性测试还挂着「待补」**：连接数上限的压力测试、浏览器 `Origin` 的 socket 级测试
   （代码路径在，压力场景没有）。

## 契约与兼容

- 旧二进制名 `skynet-download` / `skynet-downloadd`、旧 CLI 用法 `--to/--name/--threads`、
  旧数据目录 `~/.local/share/skynet-download`、旧 `app_id` 全部保留。
- 台账：新增 `priority` 列（追加式迁移，老库自动补默认值）。
- 事件流：新增 `phase`、`queued` 里多了 `priority`；老客户端忽略新字段即可。
- `POST /jobs`：新增 `priority`；自然幂等键默认 24h 过期（`SKYGET_IDEM_TTL=0` 回到旧行为）。

## 升级（在本机）

```bash
# 1) 换二进制：有 sudo 用 deb；没有就装到 ~/.local（实体文件，不是软链）
sha256sum -c skynet-dataplane_0.24.15_amd64.deb.sha256
sudo apt install ./skynet-dataplane_0.24.15_amd64.deb
#   或：scripts/install-user.sh

# 2) 重启 daemon —— 水位线、优先级、phase、socket 都是 daemon 侧行为
systemctl --user restart skynet-downloadd.service   # 或 skynet-dataplaned

# 3) 确认
curl -s http://127.0.0.1:7788/health          # {"ok":true,"version":"0.24.15"}
ls -l ~/.local/share/skynet-download/daemon.sock   # 0.24 起会多出这个（0660）
skyget doctor
```

## 验证证据（0.24.15）

```
cargo test --features full        183 单元/二进制 + 1 doc-test
skyget selftest                   12 / 12
27 个 *-e2e.sh + e2e.sh            全 PASS
daemon-lifecycle-e2e.sh           6 / 6
scripts/acceptance.sh             15 / 15 全部通过
scripts/hostile.py                5 / 5
scripts/crash.py --rounds 4       4 次硬杀收敛，进度从不回退
macOS 交叉类型检查                 aarch64-apple-darwin：http ✓ / http+gui ✓（非官方支持）
```

安装路径也验过：`~/.local/bin` 下六个入口是**实体文件**（`readlink /proc/<pid>/exe`
指向 `~/.local/bin/skynet-downloadd`，不是仓库里的 target）；解开的 deb 能直接跑
（`/tmp/x/usr/bin/skyget doctor` → 可用，13 个文件 / 61MB）。

macOS 交叉检查只证明类型和 cfg 对，**不构成 0.24.15 的 macOS 发行支持**。
原生 Windows 当前没有移植或发行物；未来路线见 [platform-support.md](platform-support.md)。

## 产物

| 文件 | 大小 | sha256 |
|---|---|---|
| `skynet-dataplane_0.24.15_amd64.deb` | 19,511,352 | `77818b42dea20692938dcccfc8897c0c17ae3a62fe1a81c62fd63c2c52a298ec` |

包里包含：`skyget` / `skynet-dataplane`（GUI）/ `skynet-dataplaned`（daemon）/
`skynet-mcp`、systemd 用户单元、桌面入口与图标、bash/zsh/fish 补全、
`man skyget` / `man skynet-dataplaned`，以及 `/usr/share/doc/skynet-dataplane/` 下的
README、CHANGELOG、LICENSE、THIRD_PARTY、平台支持与路线图、DEP-5 `copyright`。

校验和同时落在 `dist/SHA256SUMS` 与 `dist/<deb>.sha256`（`packaging/build-deb.sh`
每次构建都会重写）。本发行说明不放进包里 —— 包不可能包含自己的哈希。

## 给 agent 的入口

仓库里带一份 `skyget-transfer` skill（`.claude/skills/skyget-transfer/SKILL.md`），
`integrations/install.sh` 会把它软链给 Claude / Hermes，并往 OpenCode、pi 的配置里加路径。
它是一个**通用** skill：一个文件、一份字节，主流宿主（Claude Code / OpenCode / Codex /
Hermes / pi / Copilot）拿去就能用。正文按能力分三段（MCP / CLI / HTTP），不假设读者有
shell；front-matter 只有 `name` / `description` / `license` 三个通用键。
安装只是把同一份文件放到各宿主的位置（`integrations/skill_sync.py`，CI 比对字节防漂移）。

## 许可证

项目本体 MIT（`LICENSE`）；随仓库分发的修改版 `egui-winit` 是 MIT OR Apache-2.0，
说明见 `THIRD_PARTY.md`。
