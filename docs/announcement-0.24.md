# 天网盒子 0.24 · 发行公告

> 一台机器上多个 agent 共用的数据盒子：下载、上传、校验、缓存、调度、限权。
> 人类看得见，agent 用得动。

> 产品名为「天网盒子 / Skynet Data Box」；`skynet-dataplane` / `skynet-dataplaned`
> 是兼容用的内部二进制名。项目署名与品牌归属按 **Skynet-Club** 统筹，
> 最终署名以双方共同确认为准。

## 一句话

**下载速度在 2026 年不是问题；问题是取完之后** —— 拿到的东西对不对、存了几份、
谁装的、装到哪、下次还要不要重下；以及多个 agent 同时取数据时，谁先跑、能写到哪、
失败了怎么接着来。这一整段，现有下载器、下载站和网盘全部留白，而 agent 恰好卡在这里。

## 这一版做了什么

- **Unix socket + 内核身份**：`$SKYGET_HOME/daemon.sock`（0660），对端 uid 由内核给，
  **不用分发 token**。登记过的 uid 直接得到 `agent:<name>`，没登记的直接 401。
- **清单签名（ed25519）**：`sha256` 只证明「字节和某个 hash 一致」，签名才证明
  「这个 hash 是发布方给的」。`skyget keygen / sign / manifest-digest` 产签，
  `install / ensure` 验，**fail closed**。
- **缓存水位线**：`SKYGET_CACHE_MAX_BYTES` 之后 daemon 自己把缓存清到低水位。
  单机数据盒子的头号故障不是下载慢，是磁盘被缓存吃满。
- **提交前预检**：`skyget check <URL>` / `POST /check` / MCP `skyget_check`，
  不入队、不联网，告诉你「会被接受还是被拒、卡在哪一项」——
  和真提交**共用同一段判定**，所以预演不会说谎。
- **`skyget ensure <清单>`**：从「下什么」升级成「这台机器该是什么样」。
  对的不联网不写盘，只增改不删。
- **队列优先级 + 阶段耗时**：`interactive / normal / batch`；事件流多一种 `phase`
  （探测 / 传输 / 校验 / 落地），回答「慢在哪」。
- **命令行补齐了「把事做完」的那半**：`pause/resume/cancel`、`events`、`--timeout`、
  `show --url`、`completions`、`--json` 在所有失败路径上都是一行。
- **macOS 源码探索**：`/proc` 依赖收进一层薄封装，并有交叉类型检查；0.24.15
  仍不提供 macOS 官方发行物或支持承诺。
- **修了两个线上会咬人的真 bug**：主源说谎时多源下载整条失败（0.23.1 复现）；
  `skyget curl -o <文件>` 生成的是目录。

## 升级

```bash
# 有 sudo：
sha256sum -c skynet-dataplane_0.24.15_amd64.deb.sha256
sudo apt install ./skynet-dataplane_0.24.15_amd64.deb
# 没有 sudo（开发机常见）：装成实体文件到 ~/.local
scripts/install-user.sh

systemctl --user restart skynet-downloadd   # 或 skynet-dataplaned
curl -s http://127.0.0.1:7788/health        # {"ok":true,"version":"0.24.15"}
```

**必须重启 daemon**：水位线、优先级、`phase` 事件、Unix socket 都是 daemon 侧行为，
只换 CLI 不会生效。`skyget doctor` 会同时打出两边的版本。

## 兼容性

命令、二进制名（含旧名 `skynet-download` / `skynet-downloadd`）、systemd 单元、
数据目录（`~/.local/share/skynet-download`）、`app_id` 全部不变；
HTTP 契约仍是 **v1**，只做了加法（`priority`、`phase`、`/check`、`previous_job`）。
台账新增 `priority` 列，追加式迁移，老库自动补默认值。

平台兼容性：

| 平台 | 0.24.15 状态 |
|---|---|
| Ubuntu Linux x86_64 | **唯一正式支持**；包为 `amd64`，验证环境 Ubuntu 26.04.1 |
| Ubuntu 24.04 LTS | 满足 `glibc >= 2.39` 下限，但本版未单独发行验收 |
| Ubuntu 22.04 及更早 | 不支持，glibc 版本过低 |
| 原生 Windows 10/11 | 尚未支持，规划中 |
| macOS | 源码可探索，没有官方包或支持承诺 |

完整系统要求、安装/卸载/升级注意事项和 Windows/macOS 路线图见
[平台支持与路线图](platform-support.md)。

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

## 已知边界

完整 9 条见 [发行说明](release-notes-0.24.md)。最短版本：

- 优先级只改**排队顺序**，不抢占正在跑的；没有 per-agent 带宽上限（只有每日字节配额）。
- `--timeout` 只让 CLI 不等，**不取消任务**（分片留着、重跑接着下）。
- `skyget check` 需要 daemon 在：它问的是 daemon 那份策略。
- 上传没有 S3 multipart（`PUT` / `tus` 有）；签名是「验」，没有 `require_trust` 字段。
- `SKYGET_ALLOW_PRIVATE=1` **不会**连带放开云元数据；要单独设 `SKYGET_ALLOW_METADATA=1`。
- 0.24.15 不提供 macOS 官方发行物；原生 Windows 尚未支持。

## 产物

| 文件 | 大小 | sha256 |
|---|---|---|
| `skynet-dataplane_0.24.15_amd64.deb` | 19,643,758 | `7b97e06bb5bac98e0a268b143bb9a442cef3986e8b88e9fa81ec4e185047dd5e` |

包里：`skyget` / `skynet-dataplane`（GUI）/ `skynet-dataplaned`（daemon）/ `skynet-mcp`、
systemd 用户单元、桌面入口与图标、bash/zsh/fish 补全、`man skyget` / `man skynet-dataplaned`、
`/usr/share/doc/skynet-dataplane/` 下的 README / MANUAL / SECURITY / CHANGELOG /
LICENSE / AUTHORS / THIRD_PARTY / 完整 `docs/`（平台支持、路线图、ADR、截图）/
DEP-5 copyright。

## 许可证

MIT（[LICENSE](../LICENSE)）。随源码分发的修改版 `egui-winit` 是 MIT OR Apache-2.0，
改动与归属见 [THIRD_PARTY.md](../THIRD_PARTY.md) 和
`vendor/egui-winit/PATCH-NOTICE.md`。
安全问题怎么报：见 [SECURITY.md](../SECURITY.md)。
