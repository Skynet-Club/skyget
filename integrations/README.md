# 各 UI 的进度显示

数据只有一个来源：`GET /statusline`（1 秒一次，本地回环，开销可忽略）。
各 UI 的差别只在"能挂在哪里"，不在"画什么"。

## 状态

| 承载面 | 接入点 | 状态 |
|---|---|---|
| Claude Code | `statusLine` 命令 → `skyget statusline`（ansi） | ✅ 在用 |
| OpenCode | TUI 插件，`prompt.footer.status` 槽 | ✅ 已注册（渲染待 TUI 验证） |
| pi | 无状态栏钩子；用 CLI + `--progress stderr` | ✅ 可用（不是常驻条） |
| OpenClaw TUI | 未安装，机制未验证 | ⬜ |
| deepseek harness Web UI | cordis 插件 + `packages/client/ui-session` 状态区 | ⬜ |
| 任意 CLI / TUI（兜底） | 子进程读 NDJSON | ✅ 可用 |

## 给 agent 装 skill：各家机制不一样

同一个 `SKILL.md`（在 `.claude/skills/skyget-transfer/`），五个宿主的加载方式不同：

| Agent | 机制 | 装法 |
|---|---|---|
| Claude Code | `~/.claude/skills/<name>/SKILL.md` | 软链 |
| Hermes | `~/.hermes/skills/<name>/SKILL.md`（同格式） | 软链 |
| OpenCode | 配置 `skills: ["<路径>"]` | 写 json |
| pi | 配置 `skills: ["<路径>"]` | 写 json（或启动时 `--skill <路径>`） |
| Codex | **没有原生 skill** | 只能靠 `AGENTS.md` 指令块 |

`install.sh` 会按上表逐家安装；Codex 那条到头就是指令块，不存在"装 skill"。

## 一条命令装好（幂等）

```bash
bash integrations/install.sh --dry-run   # 预演，什么都不写
bash integrations/install.sh             # 写：指令块 + skill 软链 + token + MCP
bash integrations/install.sh --no-mcp    # 只写指令块和 token
```

- 指令块用 `<!-- >>> skyget >>> -->` 包住，重复跑只替换段内内容，不会重复追加；
- 认早期版本写的标记（`<!-- >>> skyget: 大文件传输 >>> -->`），会就地升级；
- 改动前自动备份到 `~/.local/state/skyget/install-backup/<时间戳>/`；
- token 只在缺失时生成，已有就绝不动。

**为什么需要它**：这些配置散在 10+ 个文件里。手改过一次之后，换机器、或者某个
agent 自己改了配置，就只能靠回忆。这个脚本把"该长什么样"写死。

## 数据契约

```bash
curl -s -H "Authorization: Bearer $SKYGET_API_TOKEN" \
  http://127.0.0.1:7788/statusline
```

```json
{"job_id":"…","state":"running","pct":21,"done":1835008,"total":8388608,
 "source":"api","unit":"bytes","seconds_done":0,"seconds_total":0,"width":60}
```

没有活跃任务时 `state` 为 `idle`；HLS/DASH 流的 `total` 为 0，改用
`seconds_done` / `seconds_total`。

## OpenCode

本地插件在 `integrations/opencode-skyget-status/`。`opencode plugin add` 只接受
npm / git 规格，本地路径要写进 `~/.config/opencode/cli.json`：

```json
{ "plugins": ["/home/skynet/skynet-download/integrations/opencode-skyget-status"] }
```

确认已加载：`opencode plugin list` 会显示 `local` + 该路径。

**别靠 footer 判断"装没装上"** —— 空闲时不显示进度是正常行为，容易误判。
插件每秒写一次存活标记：

```bash
cat ~/.local/state/skyget/opencode-plugin.alive
# 2026-11-04T08:21:19Z state=idle pct=0
```

mtime 不更新 = 没加载或没轮询；内容里 `http 401` = token 没读到；
`err …` = 数据盒子连不上。这一行能区分三种完全不同的故障。

token 优先取 `SKYGET_API_TOKEN`，没有就直接读
`~/.config/skynet-dataplane/tokens` 里的 `main=` —— TUI 从哪个 shell 启动不由插件决定，
指望环境变量必然会踩空（第一次就是 401）。

## pi：只能是一行行滚动的文本

实测结论（2026-11-04，pi 0.x）：

- **没有状态栏钩子**。`~/.pi/agent/settings.json` 的 schema 里只有 `tuiMode` 和
  `fullscreenScrollbar`，没有 `statusLine` 之类的口子。
- 底部那条**通栏绿条是 pi 自己的「工具运行中」指示器**，任何工具调用都会有，
  和 skyget 无关。MCP 的 `message` 最多成为它里面的一行副标题。
- 所以 pi 的上限是「一行行滚动的文本进度」，做不到 OpenCode / Claude 那种常驻条。

在 pi 里用这个组合：

```bash
skyget get <URL> -o <dir> --progress stderr
```

**两个坑**：

1. **不要接管道**。`skyget … | tail -40` 会把输出缓冲到进程退出 ——
   整场下载只有一根绿条，零信息。实测吃过一次。
2. **MCP 的 `skyget_watch` 在 pi 里不划算**：pi 不按 `done` 提前收，会连续轮询，
   上下文几轮就冲到 95%。CLI + `--progress stderr` 反而更省。

## 兜底：任何宿主都能用

```bash
skyget statusline --format json --watch      # NDJSON，一行一次，空闲自动退出
skyget statusline --format plain --watch     # 人类可读的一行
```

在一个空 pane 里跑它，等价于给任何 TUI 加了一条状态栏。这是目前唯一
零适配、五端通用的方式。
