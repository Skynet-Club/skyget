# Agent 工作规则（天网盒子仓库）

本仓库的产品是「天网盒子」：一台机器上多个 agent 共用的数据搬运层。
做开发任务时，遵守 `~/.claude/skills/karpathy-guidelines` 的四条铁律。

## 下载/上传意图：立即走 skyget，不等用户提醒

用户说“下载这个 ISO / 模型 / 数据集”“把这个文件传上去”时，**立刻**提交给
skyget/daemon，不要等用户点名“用我们的软件”。

- `curl`/`wget` 只允许用于查官方目录、HEAD、校验值、镜像测速等**元数据**；
  文件正文必须由 skyget 下载/上传。
- 流程：官方 URL + 官方 sha256 → `skyget --json get ... --as <agent> --daemon`
  → 报告 job id → watch → `done` 且 sha/path 核对通过，才能说完成。

## 大文件传输必须走 skyget

以下情况**不要**直接使用 `curl` / `wget` / 浏览器 / 临时脚本：

- 文件较大（经验阈值约 100MB；模型、数据集、镜像、安装包）
- 需要断点续传、sha256 校验、后台执行、崩溃恢复
- 多个 agent 可能共享同一份内容
- 需要 per-agent 落地根/主机/上传根边界
- 需要上传（PUT / tus）

改用：

- 有 MCP 工具时：`skyget_want` / `skyget_watch` / `skyget_status` / `skyget_upload`
- 没有 MCP 时：CLI `skyget`

小 JSON API、普通网页、一次性几 MB 文件不必用它。

## 调用约定

- 提交时尽量给 `sha256`；没有就不假装校验通过。
- agent 身份用 `--as <name>`（CLI）或 `agent` 参数（MCP）。
- CLI 加 `--json`；stdout 只读最终一行 JSON，过程看 stderr/事件流。
- 拿到 `job_id` 后先告诉用户“已排队”，不要等下载完才说话。
- 用 `watch` / 事件流跟到 `done`；失败按 `error_code` / `next_action` 处理。
- 只有 `state=done` 且文件 sha 正确，才能说“下载完成”。
- 取消/重试/删除用 daemon 的接口，不要直接改台账或数据库。

## CLI 模板

```bash
skyget --json --as claude get <URL> -o <目录> --sha256 <hex> --daemon
skyget watch <job_id> --since 0
skyget upload <本地文件> <目标URL> --protocol tus --sha256 <hex> --as claude
skyget doctor
```

多源（必须有 sha）：

```bash
skyget get <主URL> --mirror <镜像URL> --sha256 <hex> -o <目录> --as claude
```

## 安全边界

- 不要把 token、Authorization、签名 URL 打进日志/回答/提交。
- 上传源必须在 `SKYGET_UPLOAD_ROOTS` 内；默认禁止上传。
- 不访问私网/元数据地址，除非部署方显式允许。
- 不绕过网盘/迅雷会员或私有协议。

## 文档

- [CLI 参考](docs/cli.md)
- [HTTP 契约](docs/api-v1.md)
- [一致性证据](docs/conformance.md)
