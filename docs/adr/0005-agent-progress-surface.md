# ADR 0005 · Agent 面向的进度面

- 状态：已采纳（2026-10-31，0.22.0）
- 相关：`src/bin/skyget.rs`、`src/bin/mcp.rs`、`scripts/detach-e2e.sh`

## 背景

Claude Code、Codex、opencode、hermes 的 TUI 各不相同。为 Claude Code 写一个
statusLine 脚本不能直接给别的 agent 用；把进度刷进对话又会污染上下文。
agent 通过 daemon 后台下载时，stderr 被宿主捕获，终端里的进度条和 OSC 也到不了用户。

## 决策

1. **一个渲染器**：`skyget statusline` 提供 `ansi` / `plain` / `json` 三种输出，
   `--watch` 持续刷新；自动选择 `running > pending > interrupted`，暂停显示 `(paused)`。
2. **宿主只做薄适配**：Claude Code 用 statusLine 配置，tmux 用 `status-right`，
   TUI 插件读 `--format json --watch`，终端用 `--osc` 发 OSC 9;4。
3. **MCP 原生进度**：`skyget_watch` 识别 `params._meta.progressToken`，边跟边发
   `notifications/progress`，不依赖外部进程或状态栏。
4. **来源归属**：`--source <前缀>` 只看某个 agent，标签显示 `Skygeting[claude]:`。

## 后果

- 不再依赖 shell + jq；渲染逻辑只有一份，可单测。
- 不支持状态栏的宿主仍有两条路：JSON 流或 OSC。
- 默认是机器级全局视图；需要隔离的客户端自己加 `--source`。
- 状态栏刷新依赖宿主（Claude Code `refreshInterval`）；有的客户端需要重启才重新挂载。
