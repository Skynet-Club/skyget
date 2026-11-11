<div align="center">

<img src="https://raw.githubusercontent.com/Skynet-Club/skyget/main/assets/icon.png" width="104" alt="天网盒子 · Skynet Dataplane">

# 天网盒子 · Skynet Dataplane

### 一台机器上，所有 AI agent 共用的本机数据面

**可信下载与上传 · 断点续传 · 内容寻址缓存 · 公平调度 · per-agent 限权**

`CLI` · `MCP` · `HTTP API` · `Desktop GUI` · `Linux` · `macOS`

</div>

---

## Agent 缺的不是又一个下载按钮

Agent 真正卡住的，往往是传输开始之后的问题：

- 这个文件真的是我们要的那一份吗？
- 五个 agent 真的需要各自保存一份 20 GB 模型吗？
- 这个下载是谁发起的，最后落到了哪里？
- agent 退出、网络断开或 daemon 被 `kill -9` 之后，任务怎么办？
- 它会不会写到允许目录之外，或者把 `~/.ssh` 里的文件上传出去？
- 六个任务同时抢一台机器时，谁先跑、谁在阻塞、失败原因是哪一个？

`curl` 很适合一个人取一个文件。  
**天网盒子面向的是一台机器、多个 agent、必须可信并可恢复的数据传输。**

它作为本机 daemon 运行，让所有 agent 共用同一本台账、同一份缓存、同一个调度器、
同一套可恢复分片和同一份可观察事件流。

![机群视图](https://raw.githubusercontent.com/Skynet-Club/skyget/main/docs/screenshots/agent-fleet.png)

## 它解决什么

| 现实问题 | 天网盒子的处理 |
|---|---|
| agent 无法证明拿到的字节正确 | SHA-256 是硬闸门；不符即失败并丢弃文件 |
| 十个 agent 下载同一个模型 | 内容寻址缓存全机只存一份；再次获取可以零网络 |
| 客户端中途退出 | 分片位图和 tus 状态可跨 `kill -9` 恢复，任务继续归 daemon 所有 |
| agent 互相踩坏 `.part` | 单写者 daemon、全局调度器、跨进程分片锁 |
| 文件落到任意目录 | 落地根、主机白名单、上传根、云元数据拒绝 |
| 失败原因是一句模糊英文 | 机器可读 `error_code`、`retryable`、`retry_after_ms`、`next_action` |
| 一个 agent 占满整台机器 | per-agent 并发、每日字节配额、优先级队列和公平调度 |
| 现有工具只会 `curl` | `skyget resolve`、curl 兼容入口、HTTP API、11 个 MCP 工具 |

## 一个 daemon，三个常用入口

### 1. 给人和 coding agent 用的 CLI

```bash
skyget get https://example.com/model.safetensors \
  -o ~/models \
  --sha256 <64-char-sha256> \
  --as codex \
  --detach

skyget watch <job-id>
skyget daemon status
```

### 2. 给 Claude Code、Codex、OpenCode、Hermes、pi 等宿主使用的 MCP

```json
{
  "mcpServers": {
    "skynet-dataplane": {
      "command": "skynet-mcp"
    }
  }
}
```

MCP 提供预检、下载、上传、watch、状态、暂停/继续/取消、缓存 pin 和机群视图。
仓库还带一份通用的 `skyget-transfer` skill，供需要明确操作规则的主机直接使用。

### 3. 给其他程序使用的 HTTP API

```http
POST /check
POST /jobs
GET  /jobs/<id>/events
GET  /cas/<sha256>
GET  /health
```

本地 API 有版本化契约和一致性测试。默认只绑回环；Unix socket 可以由内核提供本机身份，
不需要给每个 agent 分发 token。

## 三条不妥协的承诺

1. **校验是结果的一部分。**  
   没有 digest，就不说“已验证”。调用方给出的 SHA-256 是硬闸门，不是装饰。

2. **同一份内容只存一次。**  
   blob 按内容寻址；多个落地目录可以链接同一个已验证 blob，重复获取可以变成缓存命中。

3. **续传由 daemon 负责，不归窗口或 agent 进程。**  
   关窗口、杀掉 CLI、重启 daemon，都不会悄悄把一个几十 GB 的任务从零开始。

多源也建立在这套基础上：返回错误内容的镜像会被拒绝，健康镜像仍能把任务做完。

## 安装

### Linux 发行包

从 Release 附件下载 `skynet-dataplane_0.24.15_amd64.deb`：

```bash
sudo dpkg -i skynet-dataplane_0.24.15_amd64.deb
skyget daemon ensure
skyget doctor
```

### 从源码构建

```bash
git clone https://github.com/Skynet-Club/skyget.git
cd skyget
cargo build --release --features full
scripts/install-user.sh
skyget daemon ensure
skyget doctor
```

`skyget doctor` 会结构化报告版本、目录权限、daemon 可达性、socket 状态、身份模式、
上传策略和落地策略。

## 为可信而设计，而不是只为速度

- **重定向逐跳检查。** 跳向云元数据地址时，在建立连接之前就拒绝。
- **私网和云元数据是两个开关。** 放开公司镜像不会顺带放开凭据端点。
- **凭证只存引用。** `env:NAME` 和受限的 `file:` 路径不会把 secret 写进台账和事件流。
- **上传默认禁止。** `SKYGET_UPLOAD_ROOTS` 必须显式配置，不能任意外传本机文件。
- **对源站礼貌。** 有界并发、遵守 `Retry-After`、UA 诚实、不读取浏览器登录态。
- **边界写在明面上。** 不支持的能力直接说清楚，不用“即将支持”掩盖。

完整边界见 [安全模型](https://github.com/Skynet-Club/skyget/blob/main/docs/security.md)
和 [已知限制](https://github.com/Skynet-Club/skyget/blob/main/docs/release-notes-0.24.md)。

## 证据，而不是口号

0.24.15 发布前实际完成：

- 183 个单元/二进制测试和 1 个 doc-test 全部通过；
- 12 项离线内核自检全部通过；
- 27 个 `*-e2e.sh` 场景加旧 CLI 回归全部通过；
- 多轮 `kill -9` 后进度不回退，最终 SHA-256 一致；
- 恶意服务器谎报 Range、长度、内容和限流时均未产出静默坏文件；
- 真实下载、上传、daemon 重启、Unix socket 身份、签名、配额和 MCP 都跑过；
- macOS 专属代码路径完成交叉检查。

每条对外契约对应的测试见
[conformance.md](https://github.com/Skynet-Club/skyget/blob/main/docs/conformance.md)。

## 明确不假装支持的边界

- 优先级只改排队顺序，不抢占正在运行的任务。
- 目前没有 per-agent 带宽上限；已有每日字节配额和并发上限。
- 上传支持 PUT 和 tus 1.0，尚未实现 S3 multipart。
- 严格出网模式下，HLS/DASH 子请求需要显式 opt-in。
- 不做跨机共享缓存；局域网分发请用 BT 或缓存 export/import。
- 网盘分享抓取、DRM 绕过、`thunder://`、`ed2k://` 和 NZB 明确不支持。

真正需要回答的不是“还能多支持几种协议”，而是：

**另一个 agent 能不能证明拿到了什么、避免重复保存，并在机器或客户端故障后自己恢复？**

## 寻找早期用户与协作者

现在最有价值的是具体场景，而不是泛泛的功能清单：

- 你的 agent 现在如何下载或上传 10–100 GB 的模型、数据集和构建产物？
- 同一台机器上的 Claude Code、Codex、OpenCode 会不会重复下载同一份内容？
- 今天最卡你的是 Hugging Face、S3、公司制品库还是 HLS 鉴权？
- 你会运行共享本机 daemon 之前，必须看到哪一条安全边界？

适合参与的下一批工作：

- 把 Metalink 1.0 接入现有源池
- S3 multipart 上传与远端 checksum 验证
- 连接数上限压力测试与 DNS rebinding 加固
- macOS 打包和桌面集成
- 按 HTTP/MCP 契约实现独立客户端

欢迎提交可复现的 issue。  
“在我机器上失败了”很有价值；  
“地址形态、文件大小、失败事件和错误码都在这里”最有价值。

也可以直接参与：

- [描述你的真实 agent 工作流](https://github.com/Skynet-Club/skyget/issues/1)
- [RFC：Metalink 1.0](https://github.com/Skynet-Club/skyget/issues/2)
- [Good first issue：连接上限压力测试](https://github.com/Skynet-Club/skyget/issues/3)

---

## For English readers

**Skynet Dataplane is the local dataplane every agent on a machine can share.**

It provides SHA-256-gated downloads and uploads, resumable transfers, a
content-addressed cache, fair scheduling, an auditable ledger, and per-agent policy.
One daemon serves a CLI, MCP, HTTP API, and desktop GUI.

The project is built for one machine with many agents: it answers whether the bytes are
trustworthy, who requested them, where they landed, what happens after a client dies,
and which security boundary applies.

We are looking for real agent workflows, independent clients, Metalink support,
S3 multipart upload, and concurrency/security testing.

---

**产物：** `skynet-dataplane_0.24.15_amd64.deb`  
**SHA-256：** `f02ff96cfe4e51511e54652db5a2b296049784ef83725f6e722cdb9f59f62c58`  
**许可证：** MIT
