# ADR 0006 · 凭证与每日配额

- 状态：已采纳（2026-10-31，0.22.0）
- 相关：`src/credential.rs`、`src/quota.rs`、`src/api.rs`、`src/engine.rs`

## 背景

agent 需要私有源（Hugging Face、S3、内部 API），运维需要按 agent 限制用量。
两件事都不能靠“把 secret 写进台账”或“相信请求体里的自称”来实现。

## 决策

1. **凭证只存引用**：`env:NAME` / `file:PATH`，值只在用时读、绝不落台账。
   上传和下载各有白名单：`SKYGET_UPLOAD_CREDENTIALS` / `SKYGET_DOWNLOAD_CREDENTIALS`；
   `file:` 只允许 `$SKYGET_HOME/secrets/` 下的文件。
2. **下载凭证只发给主源**：镜像可能是第三方，不能把 token 带过去；多源任务里
   非主源请求不带认证头。HLS/DASH 与种子明确拒绝 `--credential-ref`，不静默忽略。
   **用凭证必须有显式 host 白名单**（`SKYGET_ALLOWED_HOSTS` / per-agent
   `allowed_hosts`），否则提交被拒 `policy.credential_needs_hosts` —— 防止 agent
   把 token 发到任意公网主机。
3. **配额在知道总量后检查**：`SKYGET_AGENT_QUOTAS`（十进制单位）按 UTC 自然日
   统计 `done` 任务的 `bytes_total`（下载+上传）；下载在探测到总量后、上传在
   算 hash/传字节前检查，超限 `quota.exceeded` 且不落盘/不上传。缓存命中不联网，
   不计入。
4. **配置写错 fail closed**：配额值解析失败按 0 处理（全拒），不当作“不限”。
5. **可观测**：`/agents` 与 `skyget agents` 返回 `quota_bytes` / `used_today`。

## 后果

- 台账和日志里没有 secret；泄露面只剩本机进程环境或 secrets 目录。
- 没有 per-agent token 时，来源仍是自称，配额和策略都可能被绕过；生产部署应启用
  `SKYGET_AGENTS`（见 `docs/deploy.md`）。
- 并发任务可能让日用量超出上限“一个正在下载的文件”；这是有意的取舍，避免为了
  精确预留引入全局锁。HLS/DASH 与种子没有可靠的总字节，配额无法在开始前精确检查；
  直链是前置精确检查。
