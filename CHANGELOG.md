# 变更记录

只记「行为变了」的事，不记内部整理。版本号按 0.x 走，改动可能不向后兼容，
但每次都会写明白旧用法还能不能用。

## 0.24.15

**稳定性收尾：生命周期现场不再含糊**

- `skyget daemon status` 现在同时报 pid 文件、pid 是否真的是 daemon、
  socket 是否存在、是否真的能连。崩溃后的死 pid / 失效 socket 不再看起来像正常状态。
- `skyget daemon stop` 补齐了之前只写在帮助里的“其次按 pid”：systemd 没托管时，
  现在会停掉 `daemon ensure` 直接拉起的进程；没有活进程时会清掉死 pid / 失效 socket。
  SIGTERM 后 5 秒仍未退出则明确报错，不误删现场。
- 新增 `scripts/daemon-lifecycle-e2e.sh`：残留状态恢复、status、直接进程 stop、
  重复 stop、空闲退出共 6 项，全部走真 daemon。
- CI 改为实际调用 `integrations/skill_sync.py --check`，并覆盖 `main` / `master`
  两种分支名；干净 HOME 下“宿主未安装”不再被误判为 skill 漂移。
- 单元/二进制测试统一为 183 条，另有 1 条 doc-test；e2e 脚本当前为 27 个
  `*-e2e.sh` 场景加 `scripts/e2e.sh` 旧 CLI 回归。

## 0.24.14

**生命周期：跟着 agent 起，而不是跟着开机起**

默认不再设开机自启。三种模式：

- **随 agent 起（默认）**：`scripts/install-desktop.sh` 只装服务、`disable` 开机自启；
  agent 一调 `skyget` / MCP 就按需拉起（`skyget daemon ensure`）。
  `integrations/install.sh` 还会往 Claude Code 的 `hooks.SessionStart` 写一条
  `skyget daemon ensure`，让 agent 一启动就热身。
- **传统常驻**：`scripts/install-desktop.sh --with-service`（`enable --now`）。
- **手动**：新增 `skyget daemon ensure|status|stop`。

配套：

- **客户端兜底**：`api_json` 连不上时自动 `daemon ensure`（`SKYGET_AUTOSTART=0` 关闭，
  只对回环地址生效）；GUI 复用同一份 `daemon::ensure`。
- **空闲退出**：`SKYGET_IDLE_EXIT=<秒>`。无请求 + 调度器空 + 台账无活任务才退，
  退出前清 socket 与 pid 文件。
- **策略不跟调用方走**：自动拉起优先用 systemd（环境来自 `EnvironmentFile`），
  不继承 agent 自己的环境，避免 agent 把 `SKYGET_ALLOW_PRIVATE` 之类放宽。
- 文档 `docs/deploy.md` 新增「5.3 生命周期」，写明三种模式、为什么默认不开机自启、
  以及"每个 agent 起自己的子进程"为什么是错的。

## 0.24.13

**重启/常驻测试抓到的四个问题**

- **pid 文件丢后缀（0.24.0 的回归）**：`sys::pid_file()` 在 Linux 上返回
  `/run/user/1000/skynet-downloadd-7788`（没有 `.pid`），于是按老名字读 pid 的脚本失效，
  旧文件也没人清 —— 实测 `/run/user` 下攒了 **102 个**。现在统一补回 `.pid`，
  并加了单测钉住。
- **`systemctl stop` 之后 pid 文件与 socket 还在**：SIGTERM 默认直接终止进程、不展开栈，
  `Drop` 里的清理根本不会跑。现在装一个只做 `unlink`（async-signal-safe）的
  SIGTERM/SIGINT 处理函数，退出后两个文件都干净；重启时 socket 也重建。
- **`skyget watch` / `events --follow` 会跟着 daemon 重启一起死**：任务明明在 daemon 里
  继续跑，客户端却报错退出。现在传输层错误自动重连（最多 12 次，0.5s 递增），
  接上后打印「重新接上了（重连 N 次）」；4xx 这种有答复的失败不重连。
- **崩溃循环判定误伤正常重启**：以前"10 分钟内启动 ≥3 次"就算循环，运维升级连着
  重启三次就会让所有在途任务停在「中断」。现在看**上一轮的存活时长** ——
  连续 3 次都活不到 60 秒才算循环；正常跑过再重启不受影响。

## 0.24.12

**礼貌条款：按服务器说的退避**

- 429/503 不再固定等 5 秒：**读 `Retry-After`**（秒），按它等；上限夹到 5 分钟
  （再长就该换源/失败，而不是把任务挂死）。没有这个头时退回原来的节奏。
  探测、分段、单连接三条路径都接上了；`retry_after_ms` 与 `next_action` 也带上服务器给的值。
- 这既是"不把自家 IP 撞进黑名单"，也是给别人源站减负 —— 理由写进了
  `docs/philosophy.md` 第 11 条。

**文档：把"礼貌"写成可核对的条款**

新增一节「对源站的礼貌（可核对）」，出现在
[发行公告](docs/announcement-0.24.md)、[发行说明](docs/release-notes-0.24.md)、
[产品介绍](docs/product-intro.md)、[设计哲学](docs/philosophy.md)、
[协议](docs/protocols.md) 与 README：

| 做法 | 数字 |
|---|---|
| 并发上限 | `-j` 4–16（按体积）；`SKYGET_MAX_PER_HOST` 默认 16，跨任务共享 |
| 限流退避 | 429/503 → `Retry-After`，≤300s；无该头则 429 退 1.5s×n / 503 退 0.3s×n |
| 重试 | 最多 4 次；不支持分段/内容变了/校验不符 → **不重试** |
| 身份 | UA `skynet-download/<版本>`；不伪造 Referer；不读浏览器 cookie |
| 续传 | `url + 大小 + ETag` 三项全等才复用位图 |
| 拒绝 | `thunder://`、网盘分享页、`ed2k://`、NZB、DRM、站点抓取 |

## 0.24.11

**skill 面向主流宿主通用，不再为一单一宿主调整形态**

- front-matter 只留 `name` / `description` / `license` —— 这是 Claude Code、OpenCode、
  Codex、Hermes、pi、Copilot 都认的两个键（外加一个无害的 license）。
  `description` 恢复成完整的触发语（80..=1024 字），不再被 60 字上限压缩。
- `description` 之外**不再有任何宿主专用字段**；谁需要额外元数据，自己从正文取。
- 安装目标改成主流宿主：`~/.claude/skills/`（Claude Code）、`~/.hermes/skills/`、
  `~/.local/share/skyget/skill/`（其它宿主自取）；OpenCode / pi 走各自配置里的
  `skills: [<目录>]`；Codex / Copilot / 任意读 `AGENTS.md` 的宿主走常驻块。
- `integrations/install.sh --repo DIR` 现在会额外写
  `DIR/.github/copilot-instructions.md`（同一份正文，Copilot VS Code 工作区级）。
- 删除 1Agent 的 `skill.json` 派生与 `~/.local/share/1agent/...` 目标：
  产品不为某一宿主改变形态。
- `integrations/skill_sync.py --check` 仍然比对逐字节一致（CI 里跑）。

## 0.24.10

**skill 变成真正的一份通用文件**

上一版是"一个源 → 各宿主生成的方言"，那还是三份东西。现在：

- **一个文件** `skill/SKILL.md`（含 front-matter）就是通用 skill。宿主差别只在"放在哪"，
  不在内容：`skill_sync.py` 把**同一份字节**搬到各宿主位置（Claude/Hermes 用软链，
  1Agent 用拷贝 + 派生的 `skill.json`），`--check` 比对字节而不是比对语义。
- front-matter 采用**最严宿主的限制**：`description` 31 字（1Agent 要求 1..=60），
  同时带 `version/platforms/scope/requires_tools/risk`（其它宿主忽略不认识的字）。
  代价是触发语不能再写长；补偿方式是正文第一段就是 `## When to use`。
- 正文按**能力**分三段（MCP / CLI / HTTP），不假设读者有 shell：任何 agent 读一份文件，
  只走自己能走的那段。正文英文 + 中文产品名，命令与错误码一律代码体。
- `AGENTS.md` / `SOUL.md` 的常驻块也从**同一个文件**取（`integrations/skill_block.py`
  去掉 front-matter 和 H1），不再有第二份手抄内容。
- 删掉 `skill/{body.md,core.md,skyget-transfer.toml}` 与 `integrations/skill_gen.py`：
  真源只有一个。CI 检查改为 `skill_sync.py --check`。

**为什么要这样**：上一版的两份手写副本已经漂移（AGENTS 块里 `check`/`events`/`pause`/
`--timeout` 出现次数为 0）。通用性不是"给每个宿主写一份"，而是"一份文件谁都能用"。

## 0.24.9

**给 agent 的 skill 变成"生成物"**

- 同一份知识以前有**两份手写副本**（`.claude/skills/.../SKILL.md` 与
  `integrations/install.sh` 里的 AGENTS.md 块），实测已经漂移：块里
  `check` / `events` / `pause` / `--timeout` 出现次数为 0。
- 现在只有一个真源 `skill/`（`skyget-transfer.toml` 元数据 + `core.md` 常驻块 +
  `body.md` 正文 + `REFERENCE.md` 长尾），由 `integrations/skill_gen.py` 生成：
  - `.claude/skills/skyget-transfer/{SKILL.md,REFERENCE.md}`（长描述，Claude/Hermes/pi/OpenCode）
  - `~/.local/share/1agent/skills/skyget-transfer/{SKILL.md,skill.json,REFERENCE.md}`
    （≤60 字描述 + 1Agent 字段 + content_sha256）
  - AGENTS.md 块改用 `skill/core.md`（不再手抄）
- 生成器自带校验：正文行数上限、`description_short` 1..=60、1Agent 的 scan_text 规则
  （控制字符 / 注入用语 / 像密钥的 token）、`content_sha256` 自洽。
- CI 增加 `python3 integrations/skill_gen.py --check`：生成物与源不一致就失败。

**尺寸**

- `SKILL.md` 从 226 行 / 11 KB 收到 74 行 / 4.2 KB（常驻上下文少 ~62%），
  细节挪进 `REFERENCE.md`（同目录，按需读）。
- 正文按**能力**分层（MCP / CLI / HTTP），不再假设读者有 shell。

**`GET /health` 自述能力**

```json
{"ok": true, "version": "0.24.9", "capabilities": ["http", "hls", "manifest", "bt", "gui"]}
```

宿主不读 skill 也能先探测再决定走哪条路；`bt` / `gui` 按构建 feature 出现。契约加法，
老客户端忽略新字段即可。

## 0.24.8

发行文稿（不改程序行为）：

- 新增 **`docs/announcement-0.24.md`**：可直接贴到 release 页面的公告 ——
  一句话定位、这一版做了什么、升级三步、兼容性、已知边界、产物、许可证。
- **DEP-5 格式的 `packaging/copyright`** 进包（`/usr/share/doc/skynet-dataplane/copyright`）：
  除 MIT 正文外，写明 vendored `egui-winit` 是 MIT OR Apache-2.0 及其归属。
  LICENSE 文件本身仍然单独装一份。
- **man 页**：`man skyget`、`man skynet-dataplaned`（含环境变量、FILES、退出码、
  信号），装到 `/usr/share/man/man{1,8}` 并 gzip。
- **`vendor/egui-winit/PATCH-NOTICE.md`**：这份内置代码是改过的上游、改了什么、
  许可证归属在哪、上游修好后怎么删。
- **`SECURITY.md`**：报告渠道、范围内/范围外（明确把「已知边界」列进"不是漏洞"，
  避免把设计取舍当成 0day 处理）。
- README 的文档索引补上公告 / 许可证 / 第三方 / 安全策略。
- **修掉重装后还显示旧名的地方**：桌面入口 `Name=天网数据面`、systemd 单元的
  `Description`、OpenCode 插件的描述，全部对齐到「天网盒子」（英文仍是 Skynet Dataplane）。
- `skyget-transfer` skill 按 0.24 重写：安装/升级、`check` 预检、`events`、
  `pause/resume/cancel`、签名清单、`--timeout`、`--json` 失败契约、Unix socket、
  缓存水位线；并修掉三处会误导 agent 的说法（`--as` 的位置、token 是否隔离、
  多源是并行还是顺序）。

## 0.24.7

**MCP**
- 新增 **`skyget_check`**：提交前预检（`POST /check`），返回 `ok` + 逐项
  `checks[{name,ok,detail}]`；被拒时带 `code` / `next_action`。
  **403 也是答案**，不像其他接口那样当异常抛掉 —— agent 才拿得到原因。
  MCP 现在 11 个工具。

**打包**
- deb 里带 shell 补全：`/usr/share/bash-completion/completions/skyget`、
  `/usr/share/zsh/vendor-completions/_skyget`、`/usr/share/fish/vendor_completions.d/skyget.fish`
  （构建时由 `skyget completions` 生成，不是手抄的）。

**GUI**
- **窄窗口自动收列**：可用宽度不够时先隐藏「速率（近 1 分钟）」波形，再隐藏
  「线路」列并把宽度让给名称。之前固定列宽合计 ~868 + 右栏 316 > 最小窗宽 980，
  13 寸笔记本上把详情卡打开会把进度条切掉一截。
- **键盘导航**：`↑`/`↓` 换行、`Home`/`End` 跳首尾、`Enter` 打开详情
  （和空格/Delete 一样，输入框有焦点时不抢键）。
- **字号从 9 档收到 6 档**（微 10.5 / 小 11.5 / 正文 12 / 标题 13.5 / 大标题 17 / 等宽 11.5）。
  差 0.5px 的两档肉眼分不出，却让「这行该多小」每次都得重新决定。
- 验收旋钮 `SKYGET_GUI_SIZE=980x520`：这台机器的合成器吞掉 XTEST，窄布局只能这样看。

## 0.24.6

**修的两个真 bug（都是新写的 URL/curl 用例逼出来的）**

- **IPv6 字面量全被拒**：`check_url` 拿 `host_str()`（IPv6 带方括号 `[::1]`）去
  `to_socket_addrs()`，必然解析失败 —— 任何 `http://[::1]/…` 都报「域名解析失败」。
  现在按 `url::Host` 分支，IP 字面量不查 DNS。`scripts/url-forms-e2e.sh` §4 守着。
- **`skyget curl -o 文件` 生成的是目录**：`-o` 在 curl 里是文件、在 skyget 里是目录，
  早期直接把参数透下去，结果文件跑进了 `out.bin/` 里 —— 对脚本是静默的行为改变。
  现在拆成「目录 + `-n` 文件名」。`scripts/curl-shim-e2e.sh` §1/§3 守着（与真 curl 字节比对）。

**安全加固**

- **云元数据不再是 `SKYGET_ALLOW_PRIVATE` 的附赠品**：新增 `SKYGET_ALLOW_METADATA`
  （默认关），`169.254.169.254` / `100.100.100.200` / `fd00:ec2::254` 即使放开私网
  也仍然拒绝，错误码 `policy.metadata_denied`。开私网是为了内网镜像，不该顺带
  把「agent 能读云凭证」也打开。

**新的验收（都在 CI 里）**

- `scripts/url-forms-e2e.sh`（11 项）：IPv6、`#fragment`、查询串里的 `&`/`%`、
  `file://`/`data:` 拒绝、302 跟随、**302 → 元数据地址必须即时被拒**。
- `scripts/curl-shim-e2e.sh`（12 项）：接管/透传边界、与真 curl 字节一致、
  `-H Authorization` → `credential-ref env:SKYGET_CURL_AUTH` 且 token 不落台账、
  没配 `SKYGET_ALLOWED_HOSTS` 时拒绝发送、`-s`、`--as`。
- CI 补跑 `scripts/hostile.py` 与 `scripts/diff.py --local`（后者新增 `--local`：
  只跑本地用例，素材自己造，缺 ffmpeg 就跳过视频那条）。

## 0.24.5

发行工程，不改行为：

- 补 **LICENSE**（MIT）与 **THIRD_PARTY.md**（说明 vendored `egui-winit` 是
  MIT OR Apache-2.0、改了什么、上游修好后怎么删）。
- 新增 **`docs/release-notes-0.24.md`**：这一版的故事、**已知边界 8 条**、
  升级步骤、验证证据、产物 sha256。
- deb 现在带 `LICENSE` / `THIRD_PARTY.md` / 发行说明，并按 Debian 规矩放
  `usr/share/doc/skynet-dataplane/copyright`。
- 新增 **`scripts/install-user.sh`**：没有 sudo 时把六个入口装成**实体文件**
  （不是软链到 `target/release`）+ 文档；`PREFIX=` 可换前缀。软链安装的坑见 `docs/release.md`。
- 新增 **`scripts/mac-crosscheck.sh`**：本机用假 cc 对 `aarch64-apple-darwin`
  做 `cargo check`（http / http+gui），push 前就能发现 macOS 专属分支的编译错误。
  它证明类型与 cfg 对，**不**证明链接和真机行为 —— 那仍然只有 CI 的 macos job。

## 0.24.4

- **`--timeout <秒>`（全局）**：到点直接退出，退出码 1、`--json` 时 `cli.timeout`。
  语义是「这条命令不再等」而不是取消：分片留着、重跑接着下，daemon 里的任务继续跑。
- **`skyget --json top`** 真的输出 JSON 了（以前打的是人话表格）：`{summary, jobs}`，
  一次快照，给监控用。
- 文档补齐三个「只有代码」的字段面：`statusline --format json`、`top --json`、`stats --json`。
- **e2e 公共样板收进 `scripts/lib.sh`**：端口/临时目录/ok-no/trap/等 health 一套。
  我这轮写的 7 个脚本（cli-control / cli-precheck / cli-surface / cache-gc /
  scheduling / uds / manifest-sign）已迁移，头部从 ~30 行降到 3 行。

## 0.24.3

- **`skyget events <id>`**：事件流专用命令，只输出 NDJSON（一行一个事件，原样）。
  `--follow` 像 `tail -f`，到 `done` 退出 0、到失败/中断/取消退出 1。
  （`watch` 仍然负责人话排版 + 最终结果。）
- **用法错误也能机读**：`--json` 时参数写错/子命令不认识除了 stderr 的人话，
  stdout 还给一行 `cli.usage`；退出码仍是 2。`--help` / `--version` 保持 stdout + 退出 0。
- **`statusline --json` 与 `--format json` 合并**：只剩一条路（以前两个开关走不同分支，
  容易以为其中一个管别的）。
- 文档补上 `export/import --json` 的字段表（`blobs` / `bytes` / `skipped`）。
- 新增 `scripts/cli-surface-e2e.sh`（events / 用法错误 JSON / statusline 等价 / export-import 字段）。

## 0.24.2

**预检**
- 新增 `skyget check <URL>` / `POST /check`：**不入队、不联网**地告诉你「这条提交会被怎么判」。
  和 `POST /jobs` 共用同一段 `preflight`（同一份实现，所以预演不会说谎），逐项给出
  `identity / protocol / mirrors / sha256 / credential / dest / upload_source / egress`
  的结论，被拒时带 `code` + `next_action`。判定用调用方身份对应的策略（含 per-agent 覆盖）。
- 出网判定只在预检里做：真提交仍按原设计不在入队时查网（域名暂时解析失败该按网络错误重试）。

**命令**
- `skyget completions bash|zsh|fish|elvish|powershell`（新增 `clap_complete` 依赖）。
- `skyget show --url <URL>`：按地址查最近一条记录。
- 控制类命令（`pause/resume/cancel/retry/forget`）新增 `--as <agent>`：
  用注册表里该 agent 的 token 发这一次请求（本机 `SKYGET_AGENTS` 里没有就明确失败）。
- `retry` 也支持短 id（和 pause/resume/cancel/show 一致）。

**验收**
- 新增 `scripts/cli-precheck-e2e.sh`（18 项：同一地址在严格/放开私网两条 daemon 上结论不同、
  逐项判据、`--json` 一行、预检与真提交一致、`show --url`、三种 shell 补全）。

## 0.24.1

**命令行**
- **`pause` / `resume` / `cancel` 进了 CLI**：以前只有 HTTP 接口、GUI 按钮和 MCP 有，
  命令行只能看不能停，而 `forget` 又要求「任务必须先停下」。id 可只写前几位；
  `pause` 之后立刻 `resume` 会按 daemon 的 `retry_after_ms` 等一下再发。
- **`--json` 的失败路径**：CLI 层失败（连不上 daemon / id 不存在 / 清单读不到）现在
  也在 stdout 给一行 `{"state":"failed","error":…,"error_code":"cli.failed"}`。
  以前只有 stderr 的中文，agent 得解析人话。`install`/`ensure` 里那两处重复打印
  也一并删掉（否则会出现两行 stdout，违反「只有一行结果」）。
- **`list` 不再静默截断**：默认 20 条，被截断时明确写「还有 N 条没显示（共 M 条）」；
  `--json` 时提示走 stderr，stdout 仍是数组。`--mine/--agents/--source` 的筛选
  也挪进 SQL —— 以前是「先取 20 条再筛」，`--mine` 可能只剩 3 条。
- **`verify` 的计数不再自相矛盾**：那句「N 条记录」其实只数完成的那批
  （台账 58 条、它报 28 条），现在写成「28 条完成记录（台账共 58 条）」。
- **隐式 `get` 的子命令表不再手抄**：手抄那份漏了 `ensure` / `sign` /
  `manifest-digest`，带 http 地址参数时会被误判成 `skyget get ensure <url>`。
  现在从 clap 自己那份定义里取。
- `forget` 也支持短 id（以前只认 40 位全 id，传前缀会得到一句「没有这个任务」，
  而 `list` 里显示的就是前 16 位）。
- 对**已经结束**的任务 `pause`：回 409 `job.not_running` + 「这条已经结束（done）」，
  不再是误导人的「这条不是这个服务在跑」。
- 新增 `scripts/cli-control-e2e.sh`（10 项：短 id、暂停留分片、继续、`--json` 形状、
  取消删记录、完成态暂停的文案、未知 id）。

## 0.24.0

**平台**
- **macOS 可用**：`/proc` 依赖收进 `src/sys.rs`（Linux 走 `/proc`，macOS 走 libproc），
  CLI / daemon / MCP 在 macOS 上能编能跑；CI 增加 `macos-latest`（单测 + selftest + 真实下载/续传）。
  GUI 托盘仍是 Linux 专属（D-Bus），macOS 上退化为「关窗口即退出」。
- `sha256sum` 在验收脚本里换成可移植的 `sha()`（macOS 只有 `shasum -a 256`）。

**身份**
- **Unix socket + 内核身份**：daemon 额外监听 `$SKYGET_HOME/daemon.sock`（`0660`）。
  对端身份来自 `SO_PEERCRED`（Linux）/ `getpeereid()`（macOS），**不需要 token、
  也不能被请求体伪造**：daemon 自己的 uid = 主 token 等价，`SKYGET_UDS_AGENTS`
  里登记的 uid/gid → `agent:<name>`（配额/策略/隔离照旧），登记了映射表却没这个
  uid → `401 auth.unknown_peer`。
- 客户端：`SKYGET_API=unix:/path/to/daemon.sock`（CLI / MCP 都认）。实现是进程内
  回环转发 + 随机路径前缀口令，对端 uid 仍是调用方自己的，现有 `reqwest` 调用一处没改。
- TCP 路径行为完全不变（有 registry 时仍然必须带 token）。

**缓存**
- **水位线自动清扫**：daemon 按 `SKYGET_CACHE_MAX_BYTES` 定期把缓存清到
  `SKYGET_CACHE_LOW_WATER`（默认 90%），门槛与 `skyget prune` 一致
  （pin / 落地链接 / 活任务 / `SKYGET_CACHE_GC_MIN_AGE` 默认 1h 都保护）。
  `/metrics` 新增 `skyget_cache_bytes` 与两个 evicted 计数器。不设上限 = 行为不变。
- 新增 `scripts/cache-gc-e2e.sh`：真下载 + 真删文件，验证「清旧块、留链接、留刚下完的」。

**调度**
- **优先级**：`priority: interactive|normal|batch`（CLI `--priority`，上传也支持）。
  只改排队顺序；同优先级仍按「谁跑得少」轮转，不破坏公平，也不突破并发上限。
  写错回 `400 input.bad_priority`。`TaskView` / `queued` 事件带 `priority`。
- 新增 `scripts/scheduling-e2e.sh`（顺序 + 阶段事件 + 坏输入）。

**可观测**
- 事件流新增 `phase {name, ms}`：`probe` / `transfer` / `verify` / `land` / `cache-hit`。
  回答「慢在哪」，总耗时仍看 `elapsed_ms`。

**幂等**
- 自然幂等键（未显式给 `idempotency_key`）默认 24h 过期：过期后重新执行，
  响应多带 `previous_job`。`SKYGET_IDEM_TTL=0` 回到旧行为（永远 dedupe）。
  显式 `idempotency_key` 语义不变。

**清单签名**
- 清单可带 `signature: {key_id, sig}`（ed25519，覆盖规范化摘要）。`install` / `ensure`
  用 `--key <hex|id=hex|@file>` 或 `SKYGET_TRUSTED_KEYS` 验；**fail closed**：
  清单带签名却没有可信公钥 → 拒绝，给了公钥但清单没签名 → 也拒绝。
- 新增 `skyget keygen`（打印公钥、私钥写盘 0600）、`skyget sign`（`--in-place` 写回）、
  `skyget manifest-digest`（给外部签名工具用）。
- 新增 `scripts/manifest-sign-e2e.sh`：keygen → sign → install → 改一个字节必须失败。

**清单**
- 新增 `skyget ensure <清单>`：按清单**对账**而不是重装。每个文件先比 sha256，
  对的不联网、不写盘、不动权限；不对的按 `install` 的方式取。
  只增改、不删 —— 清单外的文件原样留着。JSON 输出区分 `changed` / `unchanged`。

**文档**
- 修掉 README 与 `docs/security.md` 的矛盾：daemon 只绑 `127.0.0.1`，跨机共享缓存
  请用 `export/import` 或 BT，不再声称「局域网带 token 就能取」。


## 0.23.1

- **多源按实测速度选源**，不再按分片号轮流分。轮流分的毛病：一个慢但活着的镜像会
  分走一半分片，把整体拖到最慢那个的水平。现在的规则是「还没样本的源先各打一片
  （探索），之后全给测出来最快的」；同一源连续失败两次本轮不再用。
  实测（16MiB，快源 4MiB/s + 慢源 256KiB/s，16 个分片）：**32s → 4.8s**，
  快源 12 次请求 / 慢源 4 次。`scripts/acceptance.sh` 第 10 项守着。

## 0.23.0

一次把「多 agent 共用一台机器」这条路走完的版本。旧用法全部可用。

**下载**
- **多源并行**：`--mirror` 从「顺序重试」变成「分片轮转」。必须 pin `--sha256`
  （分片来自不同主机，唯一能证明拼装正确的是内容哈希）。单段失败会换下一个源重试；
  整份校验不符时清掉分片、**改用主源重试一次**（坏镜像实测：长度对、内容错）。
  两个源都坏仍然 fail closed。实测 48MiB/双源/各封顶 4MiB/s：10.07s → 6.07s。
- **`-j` 默认改为 0 = 按体积自动**（`clamp(大小/8MiB, 4, 16)`）。旧默认 5 在大文件上
  白扔 2.8 倍（256MiB：26.7s → 9.5s）。显式 `-j N` 行为不变。
- **`SKYGET_MAX_PER_HOST`**（默认 16）：同一主机跨任务的连接上限。3 任务并发实测
  峰值从 10 条降到 4 条。
- **`SKYGET_ADAPT=1`**（默认关）：吞吐不再增长时自动退连接。两种源效果相反
  （单连接封顶 +5% 耗时，总带宽封顶 −10%），按多数场景默认关闭。
- **`--hls-fast`**（默认关）：分片并行预取后再交给 ffmpeg remux。直播 / 加密 /
  master playlist / BYTERANGE 自动回退到直连。46MB/30 段/每连接 2MiB/s：
  12.08s → 3.23s，产物 sha256 完全相同。

**取用**
- 新增 `skyget resolve <URL>`：台账里下过的地址 → 回环 `/cas/<sha256>`，零网络。
- 新增 `skyget curl`：接管「单地址 + `-o`」，其余参数透传给真 curl。
  `-H "Authorization: Bearer …"` 映射成 `--credential-ref`（token 只进子进程环境，
  台账只存引用），仍受 `SKYGET_ALLOWED_HOSTS` 约束。
- `/cas/<sha>?verify=1`（或 `SKYGET_VERIFY_CAS=1`）：发之前重算 sha256，
  不符返回 409 `cas.corrupt`。默认不校验，保住 46% 秒传的速度。

**进度**
- `--progress auto|stderr|off`。管道里也输出，每 20% 一行（超过 ~7 行多数 TUI 会折叠）。
- `statusline --width 0` 按 `COLUMNS` 自适应，CJK 按 2 格算。
- MCP `skyget_watch` 返回 `done` / `bytes_done` / `bytes_total`，`events` 只留最近 20 条。
  实测（pi）此前会把上下文刷到 95.7%，现在一次调用即可收尾。
- MCP 进度通知改为百分比 + 预排好的一行（`skyget ━━━━━━━━━─ 90% 9.0MB/10.0MB`）。

**安全**
- 设了 `SKYGET_AGENT_QUOTAS` 但没有 `SKYGET_AGENTS` 时**拒绝启动**（与 per-agent 策略同规），
  否则配额靠自称就能绕过。

**工程**
- `bench/`：可控限速服务器 + `fanout.sh`，离线复跑「连接数换速度」的两种 regime。
- `integrations/`：五端接入说明、OpenCode TUI 插件、幂等安装脚本。
- `scripts/acceptance.sh`：一条命令 14 项证据，约一分钟，不联网。

## 0.22.8

- **修复 CLI 重复 URL 快捷键忽略 `--to`**：同一个地址下过之后，`skyget get URL
  --to 新目录` 以前会直接返回旧任务的路径和结果，新目录里什么都没有（还绕过了
  出网/HLS 策略检查）。现在只有**落地目录与上次一致**才走无网络捷径；显式换目录
  会正常下载/命中缓存并落到新目录。e2e §2b 覆盖。

## 0.22.7

- **HTTP 请求行/头行加上限**：每行 16KiB、最多 100 个头，超限返回 431
  （`http.line_too_large` / `http.header_too_large` / `http.too_many_headers`）。
  此前用无界 `read_line`，本机 agent 可以发超长请求头把 daemon 内存打满。
  body 1MiB 和连接数 128 的上限保持不变。`scripts/http-limits-e2e.sh` 做线路级回归。
- 新增 `scripts/hls-e2e.sh` + `scripts/fake-ffmpeg.sh`：没有真 ffmpeg 也能验证
  HLS 编排会传 `-protocol_whitelist`、受限出网默认拒绝且不启动 ffmpeg。
- 新增 `docs/release.md` 发布/验收清单：构建顺序、18 个 e2e、打包、部署、回滚，
  以及今晚踩过的 `target/debug` 不重建、端口未释放、`cargo clean` 等坑。

## 0.22.6

- **人类 CLI 输出去掉终端控制字符**：`skyget list / show / watch / top / pins /
  agents / verify / install` 以及顶层错误在非 JSON 模式下对 agent/源站可控的 URL、
  落地路径、错误和事件文本做 `display_safe`，防止 ESC/OSC（例如 OSC 52 写剪贴板、
  改标题）在运行命令的终端里执行。JSON/NDJSON 输出保持原值，协议字段不变。

## 0.22.5

- **修复 HLS/DASH 绕过出网边界**：子清单和分片是 ffmpeg 自己取的，daemon 的
  `check_url` 拦不到；以前严格模式或配了主机白名单时，一个公网播放列表仍能让
  ffmpeg 去取 `127.0.0.1` / `169.254.169.254`。现在只要策略在限制出网就拒绝
  HLS/DASH（`policy.stream_egress`），错误信息说明原因。确认播放列表可信的部署
  可显式设置 `SKYGET_ALLOW_STREAM_EGRESS=1` 放行——这是一个明确的安全取舍。
- **限制 ffmpeg 输入协议**为 `http,https,tls,tcp`，恶意播放列表里的 `file://`
  分段读不到本机文件。
- 文档同步：`security.md` 的残余风险、README/MANUAL/CLI 环境变量表不再声称
  “分片 URL 逐条过出网策略”。

## 0.22.4

- **systemd 用户服务读取 `~/.config/skynet-dataplane.env`**。unit 增加
  `EnvironmentFile=-%h/.config/skynet-dataplane.env`，与部署文档一致；以前文档让
  运维把 token/配额/策略写进这个文件，但 unit 根本没加载，服务会静默跑在
  “本机无鉴权”模式。文件不存在仍可启动，权限建议 600。旧名
  `packaging/skynet-downloadd.service` 同步修复。
- **部署文档修正**：per-agent 策略的 JSON 必须写成一行（EnvironmentFile 不支持
  跨行值），并补上环境文件的创建/权限/重启说明。
- **文档修正**：`docs/api-v1.md` / `docs/conformance.md` 里“per-agent token 不做
  读隔离”的旧说法已删除；实际跨 agent 的任务/事件/pin 一律 404（主 token 与本机
  无鉴权模式除外）。“私有源认证未做”也改成“任意自定义头未做，Bearer 凭证已支持”。
  MCP/部署文档补上“agent 必须用自己的 token，别用主 token”；README 里“配额还没有
  按 agent 分开”的旧说法也改为 `SKYGET_AGENT_QUOTAS` 已支持。
- **CI 覆盖补全**：补上 agent-api、上传 resume/recreate/cancel、restart、soak，
  README 的“CI 覆盖全部 e2e”现在属实。

## 0.22.3

- **清单 sha256 归一**：`load` 与 `install` 都先 `trim` + 转小写再建任务。
  清单里写大写（或带空白）的 64 位十六进制原本能装成功，但缓存路径对不上，
  已在本机的同内容会被重下一遍；现在与 API/CLI 一致走缓存。
- **per-agent 策略字段名写错拒绝启动**：`Rule` 加 `deny_unknown_fields`。
  以前把 `dest_roots` 写成 `dest_root` 会被静默忽略，该 agent 继承全局策略；
  在全局未限制落地根时，这等于把限制悄悄放宽。现在配置错误直接失败。

## 0.22.2

- **修复：等待同内容任务时，全局静音了所有任务的卡住提示**。0.22.1 用一个
  进程级标志判断“有人在等”，心跳线程据此跳过 45 秒无进度的提示；只要有一个任务
  在等（最长 30 分钟），其他真正卡住的下载也不会报“线路卡住”。现在标志按任务
  传递，只有等待者自己跳过提示，别的任务照常报。
- **等待提示不再带对端 job id**。另一个 agent 的任务对当前 agent 不可见，短 id
  也不该出现在本任务的事件流里；提示只说明“同内容已有任务在下载”。

## 0.22.1

- **`GET /statusline`**：给 TUI 插件/看板轮询的单任务视图，返回
  `job_id/state/source/done/total/pct`。选择规则与 CLI 一致（running > pending >
  interrupted，同类取最新）；`?source=agent:pi` 前缀过滤；带 agent token 的调用方
  只看得到自己的任务；空闲返回 `state:"idle"`。不必再拉 `/jobs?limit=500` 自己挑。
  接口契约见 `docs/api-v1.md` §4.8。
- **webhook / 失败事件的错误文本脱敏**：reqwest 传输错误会带完整 URL（含签名
  query）；`error` 字段统一走 `redact_text`，不再把 token 从错误文本漏给 webhook
  或事件流。webhook 的 `url` 也先 `redact_url` 再去 query，旧台账里的 `user:pass@`
  同样不会外发。`scripts/webhook-e2e.sh` 增加「连接被拒 + query 凭证」路径的断言。
- **`docs/security.md`**：把信任边界、SSRF/凭证/路径控制、配额、完整性和
  残余风险（DNS rebinding、同 uid 读台账、webhook 无签名等）写成一份可核对的
  安全模型，附部署检查清单。
- **拒绝计数进 `/metrics`**：`skyget_denied_total{kind="auth"|"policy"|"credential"|
  "quota"}` 与 `skyget_webhook_failed_total`。HTTP 401/403 与任务运行期失败都计数，
  重启归零；token-scope e2e 增加断言。
- **README**：与 curl / aria2 / rclone 的能力对比表；明确“并发同一内容的 in-flight
  合并尚未做”（先完成的进缓存，后来的命中缓存），不把缓存说成并发去重。
- **修复：取消/删除后事件流读不到**。记录删了但事件还在，路由却先看记录并回 404；
  现在本机无身份 / 主 token 仍可读事件（审计），per-agent token 按 404 隔离。
  `upload-cancel-e2e` 覆盖。
- **修复：缓存命中被计入配额**。`usage_since` 过滤 `cached=1`，秒传不再偷偷占掉
  当天额度；与配额文档和“缓存命中不联网、不计入”的承诺一致。新增单测。
- **`skyget forget <id>`**：CLI 补上只删台账记录（不动文件）的入口，用于清理
  `verify` 报“缺文件”的历史记录；运行/排队中的任务仍要求先 `cancel`。
- **新数据目录按 0700 创建**（台账/缓存/分片/secrets）；已存在的目录不强行改。
  `file:` 凭证可以直接放进自动创建好的 `$SKYGET_HOME/secrets/`。
- **配额语法错误 fail closed**：`SKYGET_AGENT_QUOTAS` 里缺 `=` / 空 agent 名时，
  不再静默跳过（那会等于不限），而是所有 agent 配额按 0 处理并在 stderr 警告一次；
  尾逗号仍然允许。
- **失败分类进 `/metrics`**：`skyget_failures_total{code="http.404"|…}` 按
  `error_code` 统计历史失败任务，配 `skyget_denied_total` 一起做看板。
- **`GET /statusline` 只查活跃任务**：不再从“最近 500 条”里挑；跑了几小时的老任务
  不会被后来几百条短任务挤出窗口，界面不会误报空闲。`GET /jobs?active=1` 提供同样
  过滤，`skyget statusline` 已改用它。
- **台账索引**：`tasks(state, created_at)` 与 `tasks(source, state, updated_at)`。
  状态栏每秒轮询、配额每次提交都走索引，不再全表扫。
- **清单安装边界收紧**：清单 URL 过私网/元数据检查；每个文件必须带 64 位 sha256；
  落地路径先过 dest 边界再建父目录；mode 去掉 setuid/setgid/sticky 位。
- **幂等键按来源隔离**：不同 agent 用同一个显式 key 时，不再 dedupe 到对方的任务
  id/状态；同 agent 重试仍然命中。升级后旧格式的键不再命中（一次性）。
  显式 key 最长 128 字节，防止用超长键撑大索引。
- **per-agent `dest_roots: []` 直接拒绝**：空数组在兼容语义里是“不限制”，
  容易被误读成“禁止落地”；继承请省略，禁止请给一个不可能命中的根。
- **file 凭证权限检查**：`$SKYGET_HOME/secrets/` 下的凭证文件不能有组/其他权限位
  （0644 直接拒绝，要求 0600），避免 token 被本机其他用户读取。
- **webhook 不跟随重定向**：3xx 按投递失败重试，堵住“初始 URL 合法、302 到
  内网/元数据”的 SSRF 路径。
- **webhook 可选 HMAC**：`SKYGET_WEBHOOK_SECRET` 设置后，请求带
  `X-Skynet-Signature: sha256=<hex>`（RFC 4231 测试向量验证），接收方可验真；
  不设置保持无签名。
- **HLS/DASH 状态栏按秒进度**：新增 `GET /jobs/<id>/seconds`（最后一个
  `progress_secs` 事件），`skyget statusline` 在 `bytes_total=0` 时用它画条；
  JSON 增加 `seconds_done` / `seconds_total` / `unit=seconds`。
- **Debian 包**：`packaging/build-deb.sh` 产出包含 CLI/daemon/MCP/GUI、用户
  systemd unit、桌面入口和图标的 `.deb`；不改系统，只构建。
- **MCP `skyget_active`**：直接读 `GET /statusline`，agent 不需要 `job_id` 就能
  拿到当前最值得显示的任务（可按 `source` 过滤）；空闲返回 `state=idle`。
- **同内容 in-flight 等待**：同一 sha 已有任务在下载时，最多一个后来者等待并秒传
  （30 分钟上限，全局单等待槽；对端失败/取消/超时则自己下）。等待期间不误报
  “线路卡住”。`scripts/inflight-e2e.sh` 覆盖。
- **修复 `skyget list/show --json` 的 `cached` 写死 false**：台账里 `cached=1` 的
  秒传记录现在如实显示。

## 0.22.0

- **`skyget get --detach`**：提交后立即返回，任务完全归 daemon；客户端断开或
  被 SIGKILL 不影响下载。`--json` 只输出提交结果（含 `id`），后续用 `watch` 看进度。
- 新增 `scripts/detach-e2e.sh`：验证 detach 与客户端被杀后任务继续到 `done`。
- **`skyget statusline` 增强**：`--source <前缀>` 只看某个 agent，标签显示
  `Skygeting[claude]:`；`--osc` 发 OSC 9;4，终端标签页原生进度；JSON 增加 `source`。
- **MCP 进度通知**：`skyget_watch` 识别 `params._meta.progressToken`，边跟边发
  `notifications/progress`；Claude Code / Codex / opencode 可在自己的 TUI 里原生显示进度。
- **下载凭证**：`skyget get --credential-ref env:NAME|file:PATH` 以
  `Authorization: Bearer` 取私有源；daemon 用 `SKYGET_DOWNLOAD_CREDENTIALS` 白名单，
  凭证值不落台账。**启用凭证必须配置 `SKYGET_ALLOWED_HOSTS`（或 per-agent
  `allowed_hosts`）**，否则提交被拒 `policy.credential_needs_hosts`。
  `scripts/download-credential-e2e.sh` 覆盖 401 / 白名单 / host 守卫 / 不落值。
- **每 agent 每日数据配额（下载+上传）**：`SKYGET_AGENT_QUOTAS="claude=10GB,pi=500MB"`，
  按 UTC 自然日统计 `done` 任务字节数；超限失败 `quota.exceeded`，不落盘/不上传。
  `scripts/quota-e2e.sh` 覆盖边界（正好 1MB 通过，再多 1KB 拒绝）。
  配额可见：`/agents` 与 `skyget agents` 增加 `quota_bytes` / `used_today`；
  `/metrics` 暴露 `skyget_agent_quota_bytes` / `skyget_agent_used_today_bytes`。
- **CI**：GitHub Actions 跑 `cargo test`、`skyget selftest` 与全部 e2e
  （内核 / detach / MCP / 凭证 / 配额 / token / webhook）。
- **控制事件审计**：`pause` / `resume` / `retry` / `cancel` 事件记录
  `by=agent:<name>`（有 token）或 `api`。
- **token 任务隔离**：带 token 的客户端只能读/控制自己的任务；跨 agent 的
  `list` / `get` / `events` / `pause` / `cancel` / `delete` 返回 404，
  `/jobs/changes` 对 token 客户端关闭。pin 的创建/列表/解除也只限自己。
  `scripts/token-scope-e2e.sh` 用两个 token 验证任务与 pin 隔离。
- **产品手册**：MANUAL.md 同步 `--detach`、`statusline`、凭证、配额与已知限制。
- **终态 webhook**：`SKYGET_COMPLETION_WEBHOOK` 在 done/failed/interrupted/cancelled
  时异步 POST；payload 不含凭证、URL 去掉 query；`scripts/webhook-e2e.sh` 覆盖
  成功/失败/取消三条路径。
- **缓存上限**：`skyget prune --max-bytes 10GB` 在年龄门槛之上按总量预算清，
  从最老的开始删；pin / 落地链接 / 年轻块仍然保护。
- **English quick start**：`README.en.md`，覆盖安装、daemon、MCP、状态栏、
  凭证、配额和验收命令。
- **文档**：`docs/deploy.md`（systemd / token / 策略 / 配额 / 凭证）与
  ADR 0005（进度面）、ADR 0006（凭证与配额）。

## 0.21.3

- **CLI/TUI 进度条**：`skyget get` / `skyget watch` 在 stderr 是终端时显示
  24 格单行进度条、百分比和已下/总量；HLS 这类按播放时长报进度的任务同样有
  进度条。总量未知时只显示字节/时长，不画空条假装 0%。
- 进度仍然只走 stderr；`--json` 或 stderr 不是终端时保持原样（NDJSON / 无进度），
  stdout 仍只有最终一行 JSON，不影响 agent 解析。
- **修：暂停后重试，`skyget watch <id>` 会被历史里的旧「暂停」事件骗退**。
  现在重放到历史终态时会再查一次当前状态；任务已经 resume/retry 就继续跟，
  不再打印旧错误后立刻退出。
- Agent 接入规则补充：用户提出下载/上传就立即走 skyget，不必等用户点名；
  `curl` 只用于查官方目录/HEAD/校验值等元数据。
- **`skyget statusline`**：单行进度条，支持 `ansi` / `plain` / `json` 三种输出，
  `--watch` 持续刷新；默认跟随最新 `running`/`pending` 任务，暂停显示 `(paused)`，
  空闲时隐藏。Claude Code 配置见 README。
- 版本 0.21.3；命令与 API 契约不变。

## 0.21.2

- **修：启动时鼠标转圈约 12 秒**。eframe 默认带 `wgpu`，在这台 Mesa 上首次
  创建 Vulkan 管线时 `[vkcf] Analysis` 线程阻塞约 8.5 秒，窗口映射被拖到 12 秒。
  GUI 改用 **glow/OpenGL** 渲染（`default-features=false` + `glow`）；
  实测 `App::new` 8ms、首帧 17ms，进程里没有 Vulkan 线程。
- **单实例改为 `flock` + `/proc` 兜底**：以前 pid 文件被误删/旧构建残留时，
  会同时出现两个 GUI 进程（Dock/托盘看起来像旧图标）。现在：
  - flock 是真正的互斥，并发启动只有一个能拿到；
  - 启动新构建时若发现旧的 GUI 二进制已被替换（`/proc/<pid>/exe` 带 deleted），
    结束旧进程再接管，不再并排出现两个窗口；
  - pid 文件丢失时扫 `/proc` 同名的 GUI，避免重复实例。
- **Agent 接入落地**：仓库根 `CLAUDE.md` 明确大文件/续传/校验/上传必须走
  skyget，小请求不要用；`.claude/skills/skyget-transfer/SKILL.md` 给出 MCP 与
  CLI 的完整调用工作流。新增 `scripts/agent-integration-e2e.sh`：真 MCP stdio
  `want/status/watch/dedupe` + CLI 同台账/共享缓存，5/5 通过。
- 版本 0.21.2。

## 0.21.1

- **修：托盘右键「退出」没反应**。窗口最小化/收进托盘后（尤其 Wayland）eframe
  不再出帧，托盘命令只 `request_repaint` 的叫醒方式可能永远等不到下一帧，
  `ViewportCommand::Close` 就不会被处理，表现是托盘和 Dock 都还在、鼠标转圈几秒。
  现在托盘「退出」先让 D-Bus 菜单回调返回，再由独立线程在 100ms 后结束 GUI 进程
  （下载/上传都在 daemon 里，不受影响），退出前释放单实例 pid；
  `gui.log` 会留一行「托盘/快捷键：退出 GUI」。
  实测：通过 DBusMenu 触发「退出」→ 调用正常返回、进程退出、pid 文件消失。
- 同一路径也用于 GUI 内部收到 `TrayCmd::Quit` 的兜底，不再依赖隐藏窗口的下一帧。
- `scripts/install-desktop.sh` 从仓库直接运行时，会把 `target/release` 的
  `skynet-dataplane` / `skynet-dataplaned` 安装到 `~/.local/bin`（已有同名文件则沿用），
  避免出现「新 systemd unit 已启用、但 ExecStart 二进制不存在」。
- **修：新 GUI 连旧 daemon 时 footer 报 `error decoding response body`**。
  `TaskView` 的 0.19 新字段（`error_code/retryable/retry_after_ms/next_action`）
  现在 `#[serde(default)]`，新客户端能读旧 daemon 的响应；启动时若 major/minor
  版本不一致会提示「重启或升级 skynet-dataplaned」，而不是把 reqwest 解码错误
  直接甩给用户。
- 桌面入口 `StartupNotify=false`：eframe/winit 不发启动完成信号，保留 `true` 会让
  GNOME 在启动时显示几秒转圈/等待光标。
- 版本 0.21.1；命令与 API 契约不变。

## 0.21.0

**改名：天网下载器 → 天网数据面 / Skynet Dataplane。**
「下载器」只是入口动作，不是产品角色；名字对齐到“单机多 agent 的数据搬运与治理层”。

- 新主二进制：`skynet-dataplane`（GUI）、`skynet-dataplaned`（守护进程）。
- **旧名继续可用**：`skynet-download` / `skynet-downloadd` 是同一份程序的兼容入口；
  两个名字共用一个 pid 文件、一个端口、一本台账，不能各起一份。
- `skyget` 命令行、`skynet-mcp`、`app_id`/`WM_CLASS`、数据目录（`~/.local/share/skynet-download`）、
  托盘图标名全部不动 —— 已安装的桌面入口、MCP 配置、缓存和历史台账不用迁移。
- GUI 标题、托盘标题、CLI 帮助、启动日志、README/MANUAL/API 契约统一用「天网数据面」。
- systemd：新增 `packaging/skynet-dataplaned.service`；旧 unit 保留。
  `scripts/install-desktop.sh` 优先装新服务，并在发现旧服务时停用旧服务再迁移，
  避免两个服务抢 7788。桌面文件仍叫 `skynet-download.desktop`（原地更新，不产生双图标）。
- GUI 自动拉起守护进程时新名优先、旧名兜底；单实例检测按可执行文件名精确匹配，
  `skynet-dataplane` 不会误认成 `skynet-dataplaned`。

## 0.20.0

**上传（PUT / tus）**：下载和上传共用同一本台账、事件流、调度器和策略。

- `POST /uploads`：`path`（本地源文件）+ `url`（目标）+ `protocol`（`put` / `tus`）
  + `sha256` + `credential_ref`；CLI `skyget upload`，MCP `skyget_upload`。
- **HTTP PUT**：流式上传；**tus 1.0**：创建/HEAD/PATCH，断点续传。
  `kill -9` daemon 后重启会从远端 `Upload-Offset` 接着传；完成后清掉 sidecar。
- **默认禁止上传**：`SKYGET_UPLOAD_ROOTS` 为空即 403 `policy.upload_source_denied`。
- **凭证不落台账**：只存引用。`env:NAME` 必须在 `SKYGET_UPLOAD_CREDENTIALS` 白名单里；
  `file:PATH` 只允许 `SKYGET_HOME/secrets/` 下（canonicalize 后比较）。
  这挡住了「用 `env:AWS_SECRET_ACCESS_KEY` / `file:/etc/shadow` 借上传外传秘密」。
- 目标地址走与下载相同的出网策略；任务成功标 `trust=local`（远端收完了字节，
  但协议没有统一 digest 回执，不假装是内容校验）。
- `Task` / `TaskView` / 台账新增 `direction`、`protocol`、`credential_ref`（老库默认 download）。
- **多源回退**：`POST /jobs` 的 `sources`（CLI `--mirror`）。主源 + 镜像依次尝试；
  网络/限流保留**该源自己的**分片，内容不符清掉所有源再换；最终 sha256 仍是硬门槛。
  多源必须带 sha。分片按源 URL 存，不再按内容 sha 共享：没有逐段哈希时，
  跨源复用等于信任上一个源写下的坏字节（独立审计发现，已修）。
  Metalink 清单解析、分段级并行多源仍未做。
- **审计后的上传/多源修复**：
  - tus `Location` 必须与上传地址同源并过出网策略；否则服务器可以用 Location
    把 Bearer 凭证和文件内容指到元数据/攻击者地址（SSRF/外发）。
  - tus 远端对象 404/410 时清 sidecar、重建上传一次；服务器重启/上传过期不再永久卡死。
  - 上传取消：初始/中段写库改 `save_if_exists`，sha256 分块检查取消，取消后不再写
    tus sidecar —— 任务不会在取消几秒后以 interrupted 幽灵复活。
  - 上传请求按“最低 32 KiB/s”计算单请求超时，不读 body 的服务器不会占满 worker 6 小时。
    （阻塞式 HTTP 请求仍不能被取消信号立即打断，记为残余。）
  - `SKYGET_AGENTS` 设置但没有任何合法条目时拒绝启动（不再静默退回无鉴权）；
    有 per-agent 策略但没有 token 注册表也拒绝启动。
  - 主 token / 无身份调用方不能用请求体 `source=agent:<name>` 套用别人的策略；
    返回 `input.agent_source_requires_token`。
  - 队列满/进程退出（Closed）时新任务删除记录并释放幂等键，不再留下永不启动的 pending。
  - CAS export 的 rename 失败不再被吞掉；目标同名目录会明确报错，不写 manifest。
  - **URL 脱敏**：`TaskView.url/sources/error` 和事件 data 中的签名/令牌参数替换为
    `[redacted]`；下载/重试仍用台账里的真实 URL。SQLite 本地文件仍是同 uid 可读，
    需要更强隔离时把 daemon 放进独立容器/账号。
- **第三轮审计后的收尾修复**：
  - 脱敏不能按 `,`/`;`/`)` 截断 URL：截断后的剩余文本会绕过敏感参数替换。
    现在只按空白/全角标点切边界，尾部 ASCII 标点剥离后原样保留；scheme 大小写
    不敏感；解析失败的 URL 走 `&`/`;` 分段兜底替换。
  - `sources` 只含一个镜像（不含主 url）时现在会真正进入多源回退；
    以前 `len() > 1` 会让 SDK 调用方的单镜像被静默忽略。
  - 多源成功时 `task.url` 恢复为主源，台账不再把镜像（可能是过期签名 URL）
    写成任务地址。
  - 清理多源分片时逐个源尝试；某个源被在途请求锁住不会让其它源漏清。
  - `skyget doctor`：`/health` 或 `/agents` 返回 HTTP 200 但不是合法 JSON/契约时
    判失败，不再假“可用”；身份文案区分主 token / 单 token / 无 token；
    `--json` 保证 stdout 一行；`allowed_hosts` 区分 `null`（不限制）与 `[]`（拒绝全部），
    CLI/GUI 同步修正。
  - `/metrics` 的 per-source 标签只保留内置来源与注册表里的 agent，其余收敛为
    `other`，防止调用方用随机 `source` 把 Prometheus 基数打爆。
  - §20 的 SSRF 断言改为：Location 指向一个**记录请求**的端口，并携带真实
    Bearer 凭证；任务失败后断言该端口日志为空。e2e 不再只靠错误码自证。
- **缓存 export/import**：`skyget export <dir> [sha…]` / `skyget import <dir>`；
  导出/导入都重算 sha256，坏块拒绝；用于 CI 预热和离线搬运。
- **agent 身份与能力**：`SKYGET_AGENTS="tok_claude=claude,…"` token→agent 注册表；
  daemon 用 token 身份覆盖请求体 `source`，公平队列/台账不再相信自称。
  `SKYGET_AGENT_POLICIES` 按 agent 覆盖 dest_roots / allowed_hosts / upload_roots /
  max_running，未写字段继承全局 strict；文件坏了拒绝启动。没配时退回单 token 兼容模式。
  `SKYGET_API_TOKEN` 保留为主 token：启用注册表后人类 GUI/运维仍能看全部、操作全部，
  但没有 agent 身份（不会冒充某个 agent；适合本机，不要发给不可信 agent）。
  per-agent token 是归属/公平/策略边界，**不是读隔离**：任一有效 token 仍可读别的
  agent 的任务元数据/事件（签名 URL 已脱敏）；强多租户请一台 daemon 一个租户。
- **删除墓碑落库**：`/jobs/changes` 的 `deleted` 以前只存在进程内存里，daemon 一重启，
  删除对 `since>0` 的增量客户端就不可见了（界面会把删掉的行挂回来）。现在删除与墓碑
  在同一个 SQLite 事务里写；`restart-e2e.sh` §5 用 kill -9 验证跨重启仍可见。
- **保留策略**：启动时清理 + 每小时维护线程。进度事件 30 天、全部事件 90 天、
  删除墓碑 7 天；`SKYGET_{PROGRESS,EVENT,TOMBSTONE}_RETENTION_DAYS=0` 可关掉。
  以前只在启动时清一次，长期运行的 daemon 事件表只增不减。
- **监控补齐**：`skyget_build_info`、`skyget_uptime_seconds`、
  `skyget_scheduler_capacity`、`skyget_sources`、`skyget_tombstones`，
  以及按来源拆开的 `skyget_jobs_by_source` / `skyget_running_by_source`。
- **`GET /agents` + MCP `skyget_agents`**：这台 daemon 上谁在用、能用到哪、现在跑几个。返回 agent 名字 /
  任务数 / 正在跑 / 实际生效的并发上限 / dest_roots / allowed_hosts / upload_roots，
  **不返回 token**；per-agent token 只能看见自己。Cockpit 和运维排查用。
- **`skyget doctor [--json]`**：一条命令体检 —— 数据目录可写、blobs/parts 存在、
  落地/上传/主机策略、daemon 版本与连通性、当前 token 身份。daemon 不可达/身份被拒
  退出码非 0；本地目录/策略仍照常报。部署后、接入前先跑它。
- **`skyget agents [--json]`**：终端机群视图，和 GUI/MCP 共用 `/agents`；
  无头服务器上也能看每个 agent 的任务数/并发/落地根/主机/上传根。
- **GUI 机群视图**：顶栏「机群 N」切到 agent 驾驶舱 —— 每个 agent 一张卡：
  任务数、正在跑/上限、并发占用条、落地根、允许主机、上传根。
  `SKYGET_GUI_VIEW=fleet` 可启动即打开；数据来自 `/agents`，2 秒刷新一次，
  只在视图打开时重绘（不破坏空闲零帧）。

## 0.19.0

**agent 级守护进程**：从「一台机器一个下载器」到「一台机器一个数据面」。契约见
[docs/api-v1.md](docs/api-v1.md)。

- **agent 默认经守护进程**：`skyget --as claude …` 提交给 daemon（连不上回退并说明）；
  `--daemon` / `--standalone` 可强制。人类不带 `--as` 时行为不变。
  好处：agent 的 shell 死了下载不死；并发由全局调度器管；台账/缓存只有一份。
- **幂等提交**：显式 `idempotency_key`，或 `(url,dest,sha256)` 自然键。重复提交返回
  同一条任务（`deduped:true`）；失败任务用 `POST /jobs/<id>/retry` 重跑。
- **事件流**：`GET /jobs/<id>/events?since=&wait_ms=`，per-job seq，可重放、可长轮询；
  `skyget watch <id>` 断线接着读。
- **全局调度**：daemon 默认 4 并发 / 排队上限 1024 / 单 agent 最多 2 个；队列满回
  429 + `retry_after_ms`。环境变量 `SKYGET_MAX_JOBS` / `SKYGET_MAX_QUEUED` /
  `SKYGET_MAX_PER_AGENT` / `SKYGET_MAX_CONNS`。
- **机读错误**：`error_code` / `retryable` / `retry_after_ms` / `next_action` 落库并进
  `--json`；分类见契约 §6。旧的 `error` 人话字段保留。
- **安全边界（行为变化）**：daemon 默认拒绝私网/回环/链路本地/CGNAT/云元数据地址，
  重定向逐跳检查；`SKYGET_ALLOW_PRIVATE=1` 显式放开。`SKYGET_DEST_ROOTS` 限制落地目录；
  `SKYGET_API_TOKEN` 可选强制 Bearer。API 的 `dest` 默认按**目录**处理（要文件名用 `name`）。
  人类 CLI 独立下载仍是宽松策略。
- **跨进程分片锁**：同一地址的 `.part`/位图只能有一个写者；第二个进程得到
  `part.busy`（可重试）。锁文件 `.lock` 会留在 parts 目录（故意不删）。
- **任务 id**：改成时间前缀 + pid + 序号 + 48 位随机，40 位十六进制；不再是
  「毫秒 + URL 前 2 字节」。
- **MCP**：新增 `skynet-mcp`（stdio），工具 `skyget_want` / `skyget_watch` /
  `skyget_status` / `skyget_retry` / `skyget_cancel` / `skyget_pin` / `skyget_unpin`。
- **缓存 pin**：`POST /pins` / `GET /pins` / `DELETE /pins/<sha>`；CLI
  `skyget pin|pins|unpin`；`prune` 跳过被有效 pin 保护的内容（`kept_pinned`）。
  `ttl_secs` 默认 7 天，0 = 不过期。
- **共享缓存直读**：`GET /cas/<sha256>`，支持单段 Range（206/416），带 token；
  容器/局域网可直接从宿主缓存取，不必重复走公网。
- **监控**：`GET /metrics`，Prometheus 文本；队列/缓存/字节/事件/pin，带 token。
- **终端驾驶舱**：`skyget top [--watch]`，无头服务器也能看队列/来源/信任/落点。
- **独立审计后的一轮安全/正确性修复**：
  - sha256 全边界只接受 64 位十六进制；`blob_of` 拒绝穿越/多字节，修掉「任意本地文件
    被缓存捷径复制进 dest」与切片 panic。
  - dest 统一展开 `~`/相对路径并保留目录语义；拒绝悬空符号链接和最终文件链接，
    `unique_path`/`pick_dest`/`land` 不再跟随链接。
  - 终态写库改为 `save_if_exists`（取消/删除后不复活）；cancel 未知 id 404、
    外部进程 409；重启 reconcile 处理 pending 孤儿并写事件。
  - 并发同幂等键只落一条任务；并发 resume 只起一个引擎；scheduler panic 隔离 +
    JobGuard 把 `internal.panic` 写进台账/事件。
  - MCP job_id 只接受 40 位十六进制；`/jobs/changes` 游标不再用墙钟；
    strict 策略尊重 `SKYGET_ALLOWED_HOSTS`；token 统一 trim；IPv6 封 NAT64/6to4/
    Teredo/site-local/IPv4-compatible；HLS 子播放列表和 http `.torrent` 也过出网策略。
  - CLI daemon 路径失败退出码非 0；`TaskView.path` 只在 done 时有值。
  - **缓存完整性**：daemon 命中缓存重算 sha256；被篡改的块由验证过的下载替换，
    符号链接块拒绝。人类 CLI 默认保留秒传速度，`SKYGET_VERIFY_CACHE=1` 打开。
- 修：`GET /jobs/<id>` 把 `cached` 写死成 false；缓存命中的 `path` 报成目录而不是文件；
  `early_dest` 与 `dest_is_dir` 判据不一致；一个跨毫秒会偶发失败的 `sanitize_name` 测试；
  `cargo test` 在 http-only 构建下被 GUI 示例挡住。

## 0.18.3

- 「保存目录」那个控件挪到 **「下载」按钮右边**（原来在地址框左边）：读完「下什么」接着就是「下到哪」。
- 去掉它的悬停背景块：只在悬停/展开时把图标和文字提亮，顶栏不添色块。

## 0.18.2

- **修：「下载保存到」按钮点了没反应**。点下去之后，界面在同一帧里做了两件事：
  打开弹层，然后判定「这一下点在弹层外面」→ 立刻关掉。所以看起来像按钮坏的。
  把「该不该关」抽成纯函数，显式排除三种情况（打开它的那一帧 / 点在弹层里 /
  点在开关本身），并先用两条单测把「同一帧不许关」钉住。
- 弹层不再每帧抢输入框焦点（原来点「选目录…」后焦点会被立刻抢回去，手感发黏）。
- 多了个入口：**Ctrl+D** 打开/收起「下载保存到」—— 鼠标出问题时也有路走。
- 目录按钮点一下会往 `gui.log` 记一行（无条件记）。以后「点了没反应」这种报告，
  能一步分清是「事件没进来」还是「逻辑没走对」。

## 0.18.1

- 换新 logo（天网俱乐部的下载器图标）。重画的是矢量源，按 16/24/32/48/64/128/256/512
  八档重新渲染，顶栏标题、任务栏、托盘、桌面图标四处都换。

## 0.18.0

- **下载保存到哪，可以自己定了**（以前写死 `~/Downloads`）。
  - 界面：地址栏左边多了一个「文件夹 + 目录名」，点开能改路径、新建目录、调系统选择器。
    改完只影响以后的落点，已经下好的文件不会被搬走。
  - 命令行：`skyget dest` 报当前，`skyget dest <路径>` 设成它（不存在会建），`skyget dest --reset` 回默认。
  - 存到 `settings.json`；优先级 `SKYGET_DEST` 环境变量 > 设置 > 默认。
  - 设的时候会真写一个临时文件再删掉：只读目录在当时就报出来，而不是等落地失败。
- 修：`skyget get` 不给 `-o` 时直接用了写死的默认目录，导致 `skyget dest` 设了不生效
  （这种「设了没反应」最难查，是拿一次真下载发现的）。

## 0.17.0

稳定性一轮（每条都有实测，不是「应该更稳了」）：

- **崩溃/断电后自己接着下**。以前 kill -9 或断电之后，台账里那批任务永远停在「下载中」——
  没有线程在下它们，进度条冻住、也不报错，比失败难查。现在守护进程启动时先对账：
  状态是「下载中」但没有线程的，一律标成「中断」并写清原因；最多两条自动接着下。
  十分钟内反复崩的，不自动接（否则变成「崩了重启、重启就崩」）。
  实测：下到 53.9MB 时 kill -9 → systemd 拉起来 → 日志「上次没下完的 1 条已经标成中断」
  → 3 秒后完成 102393700 字节，文件在。
- **卡死的源不再是「永远下载中」**。给每条请求加 90 秒上限（1MB 一段，等于 11KB/s 的下限），
  45 秒一个字节没进就在界面上说一句「这条线路多半卡住了」。实测那个 GitHub 源：
  以前挂一小时不动，现在给出「连不上这个地址：client error (Connect): operation timed out」。
- **失败时保留「已经下到多少」**。以前失败一律显示 0 B / 110MB，盘上明明有 24MB 分片，
  用户只会以为白下了。
- **进程名分得清了**。`skynet-downloadd` 的进程名被内核截成 `skynet-download`，和界面一字不差 ——
  `pkill -x skynet-download` 会连带杀掉守护进程，正在下的全断。现在守护进程叫 `skygetd`。
- **空闲不再白画**。心跳以前无条件写一遍 `updated_at`，一个卡住的任务会让界面以为「有变化」，
  每秒重画两次；增量轮询也改成「没变化就什么都不做」。实测：空闲 CPU 0.60% → 0.35%，
  请求重绘的原因从 `remote.rs:149` 变成「没有」。

## 0.16.2

代码审查（含静态扫描）之后修的五个问题：

- **同一条任务能被派发两次**：连点「继续」、或脚本重试提交，会起两个线程写同一份 `.part`
  和同一张位图 —— 文件直接烂掉。现在 `running` 里已有该 id 就回 409。
- 提交失败后界面那行占位永远停在「排队」。现在提交失败会把占位行撤掉并说原因。
- `prune` 用 `list(1000)` 再筛活跃任务：台账超过一千条时，排在后面的活跃任务会漏掉，
  它引用的缓存块会被当成没人用。改成按状态直接查（`Store::active`）。
- 启动路径上拉起守护进程（`systemctl … start` + 轮询）会拖慢「双击到看见窗口」。
  挪到后台线程，窗口立刻出来。
- 状态栏的缓存占用每帧遍历一遍缓存目录。改成 5 秒算一次。

## 0.16.1

- **增量轮询**：界面每 700ms 只拉变动（`GET /jobs/changes?since=`），不再整份搬台账；
  删除通过服务端墓碑告知（只留最近 500 条）。
- 修：`route()` 先切掉 `?` 再匹配，导致 `since` / `limit` 从来没生效过（静默退化成全量）。
- 修：提交后列表出现两条（界面占位 id ≠ 守护进程真 id），现在占位行改成真 id。
- 修：pid 文件按端口分，允许一个用户跑两个守护进程。

## 0.16.0

- **拆出守护进程 `skynet-downloadd`**：下载不随窗口走。关窗口、界面崩了、重开，
  下载都不受影响。窗口变成它的前端（本地接口 127.0.0.1:7788）。
- 界面里「自己下」那套整条删掉（下载线程 / 任务通道 / 本地台账巡检）。
- 修：单实例判断只看「pid 活着」，机器重启后 pid 被重用会让守护进程永远起不来。
  现在核对 `/proc/<pid>/cmdline`。
- 装上 systemd 用户服务（开机自起、崩了有人拉、日志进 journal）。

## 0.15.1

- 接口上的传输控制：`POST /jobs/<id>/pause|resume|cancel`、`DELETE /jobs/<id>`，
  跟界面那五个按钮一一对应。对不是本进程跑的任务 pause 回 409。
- 修：`if let Some(x) = mutex.lock()...get(k)` 把锁守卫带进块内，块里再 lock 就是自死锁 ——
  取消操作会把整个接口卡死。

## 0.15.0

- `skyget stats`：总量、秒传命中率、失败分类（网不通 / 被限流 / 其他）、按来源分组。
- 台账新增 `cached` 列（老库自动迁移，历史记录当作「不是秒传」，不猜）。

## 0.14.3

- `prune` 的「多久没碰过」改成取 mtime 与 atime 较新者；命中缓存时 touch 一下 mtime。
  以前只看 mtime，天天在用的块和第 31 天没人理的块一样老 —— 清掉的是常用的那个。

## 0.14.2

- `skyget selftest` 增加两条：暂停后台账字节数必须真实；缓存块被改一个字节要被查出。

## 0.14.1

- 失败先分类再决定救不救：网不通 / 被限流 → 退避重试两次（2s、4s）；源不支持分段、
  内容变了、校验不符 → 不重试，直接给结论。所有分段倒在同一件事上只给一句话，
  不再逐段念二十行。

## 0.14.0

- **`skyget selftest`**：进程内起一个能演的 HTTP 服务，五秒跑完九条关键路径，不联网。
  它抓出三个真 bug：暂停时进度退回 0、探测遇 429 直接判死、有 sha 却还要先问服务器。

## 0.13.4

- 「凭什么可信」分级修正：`--sha256-from` 记 `indexed`（值来自发布方），不是 `pinned`。
- 界面显示秒传（「本机早有这份内容，一字节没下」）。
- 接口：`POST /jobs` 回 202；请求体超 1MiB 回 413 并说明字节数（以前静默截断）。

## 0.13.3

- 重试不再重下、不再攒 `(1)` 副本；内容本机早有就一个请求都不发（102MB 从 10.8s → 0.12s）。
- 修：`fs::copy` 源和目标同一个 inode（落地文件本来就是缓存块的硬链接）会把文件清成 0 字节。
- 修：心跳线程 `sleep(900ms)` 让每次收尾白等 0.9 秒（秒传任务里这就是全部耗时）。

## 0.13.2 / 0.13.1

- 失败原因一段一行、按段号排序，装在能滚的终端框里；老记录的长句也会被拆开显示。
- 列表行的悬停提示不再把几十行错误塞进去盖住整张表。

## 0.13.0

- 详情卡里五个播放器式控件：暂停 / 继续 / 取消 / 重试 / 删除（按状态变灰，位置不跳）。
- 内核：合作式停止信号（`Cancel`），主动停记「中断」而不是「失败」。

## 0.12.1

- `skyget <url>` 就是下载（不用再写 `get`）；`-o/--out`、`-n/--name`、`-j/--jobs`、`--as`
  短选项；`--json` 放哪儿都行。
- 台账按来源筛：`--mine` / `--agents` / `--source`；界面顶栏「全部来源 / 只看我的」。
- `skyget prune`：只清没人在用、很久没碰过的缓存块。
