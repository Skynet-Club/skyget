# ADR 0002 · agent 驱动的安全边界

- 状态：已采纳（2026-10-30，0.19.0）
- 相关：`src/security.rs`、`src/api.rs`、`src/engine.rs`

## 背景

下载器同时拥有**网络**和**文件系统**两个原语。人自己用时，「能在这台机器上跑程序」
基本等于「本来就能干这些」；agent 来驱动时不是这样：

- 被 prompt injection 的 agent 可以让下载器去取 `169.254.169.254`（云元数据）、
  内网服务、`file://` 或带账号密码的地址。
- agent 可以把 `dest` 指到 `~/.ssh/authorized_keys`，或利用 `..` / 符号链接逃逸。
- 一个失控的 agent 可以开几百个连接/长轮询，把 daemon 的线程和队列打满。

这不是部署环境的问题，是产品必须在进程内拒绝的事。下载器不能变成 agent 的
confused deputy（混淆代理）。

## 决策

1. **两档默认值**：
   - `Policy::default()` 放行私有地址 —— CLI 人类路径、单测、本地测试服务器用；
   - `Policy::strict()` 只放行公网 —— daemon 这条多 agent 通道默认用。
2. **出网**：拒绝私网/回环/链路本地/CGNAT（含 169.254.169.254、100.100.100.200）/
   组播/文档段；每一跳重定向重新检查；URL 带 userinfo 直接拒绝；域名白名单可选。
3. **落地**：`SKYGET_DEST_ROOTS` 非空时，路径必须落在允许根目录内；先 canonicalize
   已存在的最深祖先再比较，挡住 `..` 和符号链接逃逸。
4. **鉴权**：`SKYGET_API_TOKEN` 设了就全接口强制 Bearer（`/health` 除外）；
   CLI / GUI / MCP 自动带同一个 token。
5. **资源上限**：`SKYGET_MAX_JOBS` / `SKYGET_MAX_QUEUED` / `SKYGET_MAX_PER_AGENT` /
   `SKYGET_MAX_CONNS`；队列满回 429 + `retry_after_ms`。
6. **逃生口是显式的**：`SKYGET_ALLOW_PRIVATE=1`、`SKYGET_ALLOWED_HOSTS`、
   `SKYGET_DEST_ROOTS` 都由部署方设置，不由 agent 自己决定。

## 已知残余风险（不假装解决）

- **DNS rebinding 的 TOCTOU**：检查时解析一次，连接时 reqwest 再解析一次；
  中间被换成内网 IP 仍可能绕过。彻底解决要自定义 resolver 把解析结果钉死。
- **HLS / ffmpeg**：只检查顶层 m3u8；ffmpeg 自己去取的分片不在 Rust 侧边界内。
- **BT**：peers 本来就可能在内网（LAN 种子），出口策略不适用；种子级风险靠 infohash/内容校验。
- **上传（未实现）**：未来必须同时限制**可读**的本地路径（`SKYGET_UPLOAD_ROOTS`），
  否则 agent 可以把任意本地文件外传 —— 这是比 SSRF 更严重的一侧。
- **凭证/身份**：`SKYGET_AGENTS` 可以给每个 agent 一个 token，台账和公平队列用
  token 绑定的身份；`SKYGET_AGENT_POLICIES` 可按 agent 覆盖落地根、域名白名单、
  上传根。没配时仍是单 token + 自称 `source`。**配额（并发/带宽/容量）还没有按
  agent 分开**，目前只有全局调度上限。
- **缓存校验与落地之间的本地 TOCTOU**：daemon 在缓存命中时重算 sha256，但从
  “算完 hash”到“硬链接/拷贝落地”之间，同机有写权限的进程仍可能换块。
  彻底解决要把 open + verify + link 绑在同一个 fd/文件锁上；这一版先挡住
  静态投毒和符号链接块，并把窗口写在这里。
## 0.22 更新

- 带 token 的客户端只能读/控制自己的任务（`/jobs`、`/jobs/<id>`、事件、
  pause/resume/retry/cancel/delete）和自己的 pin；`/jobs/changes` 只对本机
  无 token 模式开放。
- 每日字节配额已按 agent（`SKYGET_AGENT_QUOTAS`，下载+上传）；带宽/容量配额仍未做。

- **HLS 的 ffmpeg 分片**：Rust 侧只校验子播放列表；ffmpeg 自己取的分片仍在
  策略之外，边界见上。

## 后果

- agent 取公网模型/工具链零配置即可用；内网源需要部署方显式放行。
- 人类 CLI 独立下载保持宽松，避免把现有工作流一刀切断。
- 任何新增协议/传输都必须先回答：它的寻址、续传、校验来源、出口策略是什么。
