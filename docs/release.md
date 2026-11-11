# 发布与验收清单

每次发版按这个顺序走；任何一步不过就停下修，不跳步。

## 1. 准备

- [ ] 工作树干净：`git status --short` 为空。
- [ ] `Cargo.toml` 版本、`CHANGELOG.md` 顶部条目、文档里的版本一致。
- [ ] 行为变化写进 `CHANGELOG.md`；契约变化同步 `docs/api-v1.md` 和
      `docs/conformance.md`（新增承诺必须加一行测试证据）。
- [ ] 平台矩阵、官方发行物、系统依赖和未来平台路线图与
      `docs/platform-support.md` 一致；官方支持范围不能靠“源码可编”暗示。
- [ ] 安全边界变化同步 `docs/security.md`（包括残余风险，不许只写“已修复”）。
- [ ] `LICENSE` / `THIRD_PARTY.md` 在；deb 里有 `usr/share/doc/skynet-dataplane/copyright`。
- [ ] `docs/release-notes-<版本>.md` 写了「已知边界」和产物 sha256（`sha256sum dist/*.deb`）。

## 2. 构建

```bash
cargo build --features full            # e2e 用的是 target/debug，必须先重建
cargo build --release --features full
```

> 坑：`cargo test` / `cargo check` **不会**重建普通的 `target/debug/skyget`。
> 如果只跑了测试就直接跑 e2e，测到的是上一次的二进制。今晚就踩过一次。

## 3. 测试

```bash
cargo test --features full             # 单元 + bin + MCP + doc
./target/release/skyget selftest       # 12 条内核行为
```

逐个跑（CI 也跑同一组，共 28 个）：

```bash
for s in scripts/e2e.sh scripts/detach-e2e.sh scripts/agent-api-e2e.sh \
         scripts/agent-integration-e2e.sh scripts/statusline-e2e.sh \
         scripts/http-limits-e2e.sh scripts/hls-e2e.sh scripts/inflight-e2e.sh \
         scripts/waiter-stuck-e2e.sh scripts/download-credential-e2e.sh \
         scripts/quota-e2e.sh scripts/token-scope-e2e.sh scripts/webhook-e2e.sh \
         scripts/upload-resume-e2e.sh scripts/upload-recreate-e2e.sh \
         scripts/upload-cancel-e2e.sh scripts/restart-e2e.sh \
         scripts/daemon-lifecycle-e2e.sh scripts/soak-e2e.sh \
         scripts/uds-e2e.sh scripts/manifest-sign-e2e.sh \
         scripts/cache-gc-e2e.sh scripts/scheduling-e2e.sh \
         scripts/cli-control-e2e.sh scripts/cli-precheck-e2e.sh \
         scripts/cli-surface-e2e.sh scripts/curl-shim-e2e.sh \
         scripts/url-forms-e2e.sh; do
  bash "$s" || { echo "FAIL $s"; break; }
  sleep 1                              # 让上一个 daemon 完全释放端口
done
```

```bash
python3 scripts/hostile.py             # 坏服务器矩阵 5/5（CI 已跑）
python3 scripts/diff.py --local        # 差分 + 格式裁判（本地用例，CI 已跑）
python3 scripts/crash.py --rounds 4    # 任意进度硬杀后收敛
```

公共样板在 `scripts/lib.sh`（端口/临时目录/ok-no/trap/等 health）：
新脚本 `source` 它就行，别再复制那 30 行 —— 复制多了就会各写各的等待逻辑。

平台：0.24.15 的正式发行目标是 **Ubuntu Linux x86_64**。发布的 deb 在
Ubuntu 26.04.1 LTS 上构建和验证，二进制要求 glibc 2.39+。`.github/workflows/ci.yml`
里的 `macos` job 只服务未来移植和源码兼容性，不代表 0.24.15 已正式支持 macOS；
原生 Windows 尚未支持。完整边界见 `docs/platform-support.md`。

> 坑：外层 shell 里如果带着 `SKYGET_API_TOKEN`，e2e 起的 daemon 会启用鉴权，
> 而脚本自己的 curl 不带 token → 一堆 `auth.required` 假失败。
> 跑之前 `unset SKYGET_API_TOKEN`（新脚本里已经自己 unset）。

发布前再做一轮重复：

```bash
for i in $(seq 1 30); do bash scripts/soak-e2e.sh || exit 1; done
for i in $(seq 1 10); do bash scripts/restart-e2e.sh || exit 1; done
```

> 坑：e2e 脚本按 `$$` 取端口，背靠背跑偶尔会撞上上一个 daemon 尚未释放的端口。
> 循环里 `sleep 1`；偶发 `Address already in use` 先单独重跑，不要直接改代码。
> 测试服务器用 `scripts/wait-http.sh` 等 accept 就绪；inflight/statusline 早期曾因
> 服务器还没起来就提交，分别把 A 判成 `connection refused` / 状态栏看不到任务
> （都不是产品 bug）。

## 3.5 push 之前：macOS 交叉类型检查

```bash
bash scripts/mac-crosscheck.sh
```

本机没有 macOS SDK，但可以把 `aarch64-apple-darwin` 的 std 装上、用一层假 cc
丢掉 Mach-O 专属参数，让 `cargo check` 走完 **只在 macOS 上编译的分支**
（`sys.rs` 的 libproc、`uds.rs` 的 getpeereid、tray 的非 Linux 桩、GUI 的 cfg）。

它证明「类型和 cfg 对」，**不证明链接和真机行为**；`--features bt` 过不去
（librqbit → aws-lc 的 aarch64-apple 汇编要真 Apple 汇编器）。
因此它是**未来 macOS 移植的源码检查**，不是 0.24.15 的官方平台认证。
正式支持矩阵与路线图见 `docs/platform-support.md`。

push 前再跑两条冒烟（都很便宜，能挡住最尴尬的两种情况）：

```bash
skyget check https://example.org/x.iso        # 预检链路通（daemon 在）
skyget check http://169.254.169.254/          # 必须非 0，结论是 policy.metadata_denied
```

产出后核对校验和（构建脚本每次重写 `.sha256`）：

```bash
cd dist && sha256sum -c *.sha256
```

## 4. 打包与部署

```bash
bash packaging/build-deb.sh
dpkg-deb -f dist/skynet-dataplane_<版本>_amd64.deb Version
cat dist/skynet-dataplane_<版本>_amd64.deb.sha256   # 构建脚本每次都会重写

# dist/ 只留当前版本：旧包全部删掉（202MB → 19MB 就是这么来的）。
# 留着它们只会让「下错版本」变成一个真实的可能，而历史版本该在 tag/release 里找。
ls dist/*.deb | grep -v "$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)" | xargs -r rm -f
dpkg-deb --fsys-tarfile dist/skynet-dataplane_<版本>_amd64.deb \
  | tar -xO ./usr/lib/systemd/user/skynet-dataplaned.service | grep -E 'EnvironmentFile|ExecStart'
```

没有 sudo 时（开发机常见）走用户空间安装 —— 装的是**实体文件**：

```bash
scripts/install-user.sh                 # ~/.local/bin 下六个入口 + ~/.local/share/doc
PREFIX=/opt/skynet scripts/install-user.sh   # 换前缀
```

> 不要用「软链到 `target/release`」当安装：`cargo clean` / 切分支 / 换 profile 都会让
> 生产 daemon 指向不存在的文件，而 systemd 只会在下一次重启时才报错。
> 想核对现在跑的是哪个文件：`readlink /proc/$(systemctl --user show -p MainPID --value skynet-downloadd)/exe`。

产物自检（不需要 root，也不动系统）：

```bash
dpkg-deb -x dist/skynet-dataplane_<版本>_amd64.deb /tmp/x
/tmp/x/usr/bin/skyget --version && /tmp/x/usr/bin/skyget doctor
find /tmp/x -type f | wc -l        # 13 个文件 / 约 61MB
```

生产 daemon 当前是手动进程（不是 systemd）。

> 升级到 0.24 后**必须重启 daemon**：水位线、优先级、`phase` 事件、Unix socket
> 都是 daemon 侧行为，光换 CLI 不会生效（`skyget doctor` 会把两边的版本都打出来）。

```bash
PID=$(pgrep -f '^/home/skynet/.local/bin/skynet-downloadd --port 7788$')
kill "$PID"
(nohup setsid /home/skynet/.local/bin/skynet-downloadd --port 7788 \
  >>/tmp/skynet-downloadd.log 2>&1 &)
install -m 0755 target/release/skynet-mcp ~/.local/bin/skynet-mcp
curl -s http://127.0.0.1:7788/health
ls -l ~/.local/share/skynet-download/daemon.sock   # 0.24 起会多出这个（0660）
./target/release/skyget doctor
```

> 进程名 `skynet-downloadd` 超过 15 字符，`pgrep -x` 匹配不到；用锚定的
> `pgrep -af '^…$'` 或显式 pid。

## 5. 收尾

- [ ] `git add ... && git commit`，提交信息写清改了什么/为什么。
       **push 之后不要再 `--amend`**：amend 会重写历史，共享分支上只能靠 force push
       收场（本地裸仓库那次就是这么撞上的）。要改就追加一个提交。
- [ ] 打 annotated tag：`git tag -a vX.Y.Z -m "..."`，确认 `git tag --points-at HEAD`。
- [ ] 更新记忆里的生产版本/启动方式（如果变了）。
- [ ] `skyget verify --deep`：确认没有**新增**损坏；历史记录按用户决定处理。
- [ ] 根盘空间：`target/` 长期增长用 `cargo clean`（会删构建产物，先重建 release；
      运行中的 daemon 不受影响，但符号链接会短暂失效）。

## 6. 回滚

- 代码：`git checkout <上一个 tag>` 并重建；数据目录不动。
- daemon：kill 当前进程，用上一个 release 二进制重启；台账/缓存向后兼容。
- 数据库：不要手改 `tasks.db`；用 `skyget forget` / `retry` / `cancel` 的接口。
