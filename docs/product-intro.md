# 天网盒子 · 产品介绍

> 单机多 agent 的数据盒子：**下载、上传、校验、缓存、调度、限权**。
> 人类看得见，agent 用得动。

> 对外产品名是「天网盒子 / Skynet Data Box」。`skynet-dataplane` 和
> `skynet-dataplaned` 仅是为了兼容已有脚本保留的内部二进制名。
> 项目署名与品牌归属按 **Skynet-Club** 统筹，最终署名以双方共同确认为准。

## 一句话

**下载速度在 2026 年不是问题；问题是取完之后。** 拿到的东西对不对、存了几份、谁装的、
装到哪、下次还要不要重下；多个 agent 同时取数据时，谁先跑、能写到哪、失败了怎么接着来。
这一段，现有下载器、下载站、网盘全部留白 —— 而 agent 恰好卡在这里。

## 给谁用

- **一台机器上跑多个 agent 的人**（Claude Code / Codex / OpenCode / Hermes / pi / 自研）。
- **要把模型、数据集、镜像、构建产物可靠地搬进搬出**的团队。
- **需要回答"这个文件是谁下的、对不对、下次还要不要重下"**的运维。

一个人、一条命令、下一个文件 —— curl 更合适，不用装这个。

## 它解决什么

| 你原本的问题 | 它怎么做 |
|---|---|
| 下完不知道对不对 | 下完必算 sha256；给了期望值就是硬闸门，不符即失败并**丢弃**文件 |
| 断了要重下 | 段位图落盘，`kill -9` 之后最多丢一两段；上传（tus）也一样 |
| 十个 agent 抢带宽、互相踩 `.part` | 常驻守护进程 + 全局调度 + 每 agent 并发上限 + 跨进程分片锁 |
| 同一个模型存了五份 | 内容寻址缓存：全机一份，落地用硬链接；再取一次可能**零网络** |
| 不知道谁把那 20GB 放这的 | 台账：来源、hash、落地路径、时间；事件流可重放 |
| agent 能把文件写到任何地方 | 落地根、主机白名单、上传根、每日配额，都在进程内拒绝 |
| 失败原因是一句英文 | 机器可读 `error_code` + 人话 `next_action`，不用解析日志 |
| 只会 `curl` 的工具接不进来 | `skyget resolve` 给回环地址；`skyget curl` 当替身；MCP 11 个工具 |

## 三个入口，一个内核

- **命令行**：`skyget`（下载/上传/预检/台账/自检/体检）。
- **界面**：`skynet-dataplane` —— 列表、进度、失败原因、机群视图（谁在用、各自能写到哪）。
- **接口**：HTTP（契约 v1）+ `skynet-mcp`（Claude Code / Codex / OpenCode / pi 等直接当工具用）。

三者共享同一本台账、同一份缓存、同一个调度器。关掉窗口，下载继续。

## 三条不妥协的事

1. **校验是一等公民** —— 不符即失败、文件丢弃；没有 hash 就不说"校验通过"。
2. **内容寻址** —— 同一份内容全机只存一份；秒传命中是零网络。
3. **断点续传真能续** —— 位图落盘、跨进程加锁，不是"失败就重下"。

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

## 它不做的事（明确拒绝，并说明原因）

- `thunder://`、`ed2k://`、网盘分享页、NZB、DRM —— 不是协议，是商业锁。
- BT v2-only 种子（v1 与 hybrid 可以）。
- 跨机共享缓存 —— 局域网分发用 BT 或 `export/import`。
- 不做 `curl | sh` 安装，也不发 npm 包（与"常驻 daemon + 原生二进制"的模型错配）。

已知边界（9 条，含"优先级不抢占""没有带宽上限""上传没有 S3 multipart"等）见
[发行说明](release-notes-0.24.md)。

## 平台与系统要求

**0.24.15 只正式支持 Ubuntu Linux x86_64。**

- 发行包是 `amd64`；
- 构建和验证环境是 Ubuntu 26.04.1 LTS；
- 二进制要求 `glibc >= 2.39`，Ubuntu 22.04 及更早版本不能直接运行；
- Ubuntu 24.04 满足 ABI 下限，但本版没有单独做 24.04 发行验收；
- 原生 Windows 尚未支持；macOS 目前只有源码探索和交叉检查，没有官方包；
- HLS/DASH 需要额外安装 `ffmpeg`；其它核心能力不依赖 ffmpeg。

原生 Windows 10/11 和 macOS 的目标、技术工作与当前状态见
[平台支持与路线图](platform-support.md)。

## 安装（两种，都不用编译）

```bash
# 有 sudo：发行包
sha256sum -c skynet-dataplane_0.24.15_amd64.deb.sha256
sudo apt install ./skynet-dataplane_0.24.15_amd64.deb
systemctl --user daemon-reload && systemctl --user enable --now skynet-dataplaned

# 没有 sudo：用户空间（装成实体文件，不是软链到构建目录）
scripts/install.sh            # 二进制 + 文档 + 桌面入口 + 用户服务 + agent 接入
```

从源码：`cargo build --release --features full && scripts/install.sh`。

装完三条命令确认：

```bash
skyget --version                              # skyget 0.24.15
curl -s http://127.0.0.1:7788/health          # {"ok":true,"version":"0.24.15","capabilities":[...]}
skyget doctor                                 # 目录/写权限/daemon/身份/策略 全绿
```

> 换二进制后**必须重启 daemon**：缓存水位线、优先级、`phase` 事件、Unix socket
> 都是 daemon 侧行为。

## 给 agent 读的契约

- 每个调用：stdout **一行 JSON**（失败也是），退出码 `0/1/2` 可分支。
- 失败带 `error_code` + `retryable` + `retry_after_ms` + `next_action`。
- 提交前可 `skyget check <URL>`：不排队、不联网，和真提交共用同一段判定。
- 通用 skill：`skyget-transfer`（一个文件，Claude Code / OpenCode / Codex / Hermes /
  pi / Copilot 与任何读 `AGENTS.md` 的宿主通用）。

## 证据

不是截图，是可复跑的东西：

- 单元/二进制测试 183 条，另有 1 条 doc-test；`skyget selftest` 12 条不联网的内核行为断言。
- 27 个 `*-e2e.sh` 场景加 `scripts/e2e.sh` 旧 CLI 回归（真 daemon、真 HTTP 源、
  真 token、真事件流、真 MCP）：分段、续传、
  校验丢弃、多源说谎、限流退避、断线重连、上传续传、per-agent 隔离、配额、webhook、
  Unix socket 身份、清单签名、缓存水位线、优先级、URL 形态、curl 替身……
- [`docs/conformance.md`](conformance.md)：每条对外承诺对应哪个测试。

## 许可证

**MIT**（[LICENSE](../LICENSE)）。随源码分发的修改版 `egui-winit` 是 MIT OR Apache-2.0，
归属与改动见 [THIRD_PARTY.md](../THIRD_PARTY.md)。
安全问题的报告方式见 [SECURITY.md](../SECURITY.md)。
