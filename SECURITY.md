# 安全策略

## 报告问题

发邮件到 **hermes@skynet.club**，标题带 `[security]`。

请说明：影响版本（`skyget --version`）、复现步骤、你判断的后果。**不要**在公开
issue 里贴未修复的利用细节。

收到后会先确认「这是不是一个安全问题」，再给修复计划。这是一个小项目，没有 7×24
的值班承诺；但会给明确答复，不会让报告石沉大海。

## 范围

在范围内：

- 绕过出网边界（私网 / 回环 / 云元数据 / DNS rebinding）；
- 绕过落地边界（`SKYGET_DEST_ROOTS`、`..`、符号链接逃逸）；
- 绕过上传边界（读取 `SKYGET_UPLOAD_ROOTS` 之外的文件）；
- 凭证泄漏到台账、事件流、日志、webhook；
- per-agent 身份隔离失效（A 的 token 能看/能控 B 的任务）；
- 校验闸门失效（sha 不符仍然落地）、缓存投毒；
- Unix socket 的身份判定失效（伪造成别的 agent / 冒充主 token）。

不在范围内（已在文档里写明，属于已知边界而不是漏洞）：

- 同 uid 的进程可以互相代替 —— 见 `docs/security.md` 的残余风险；
- `SKYGET_ALLOW_PRIVATE=1` 之后能访问内网 —— 那是部署方的显式选择；
  （云元数据另有开关 `SKYGET_ALLOW_METADATA`，默认仍然是拒绝的。）
- HLS/DASH 里 ffmpeg 自己发出的分片请求不在 Rust 侧逐条策略之内；
- BT 的对等端可以在内网；
- 本地有写权限的人改缓存块 —— daemon 命中缓存时会重算 sha，但「算完到落地之间」
  的窗口写在残余风险里；
- 拒绝服务（本地同 uid 打满队列）—— 有上限，但没有按 uid 的公平性。

## 我们怎么处理

- 修复后会在 `CHANGELOG.md` 写清「行为变了什么」，在 `docs/security.md`
  更新威胁边界与**残余风险**（不写"已修复"了事）。
- 已知边界与残余风险集中记在 [`docs/security.md`](docs/security.md)，
  发行说明的「已知边界」一节也会列出当版本仍是这样的部分。
