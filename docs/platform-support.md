# 平台支持与路线图（0.24.15）

这份文件区分三种状态：

- **正式支持**：有官方发行物，按该平台处理兼容性问题；
- **源码可探索**：代码包含相关分支或能编译，但没有官方发行物与支持承诺；
- **路线图**：计划开发，尚未支持。

“计划”不代表发布日期承诺。

## 当前正式支持

| 平台 | 架构 | 发行物 | 状态 |
|---|---|---|---|
| Ubuntu Linux | x86_64 / `amd64` | `.deb` | **0.24.15 唯一正式支持的平台** |

发行包的运行时事实：

- 构建与验证环境：**Ubuntu 26.04.1 LTS x86_64**；
- 二进制最高需要 `GLIBC_2.39`；
- Ubuntu 24.04 LTS 满足这个 ABI 下限，但 0.24.15 没有在 24.04 上单独做发行验收；
- Ubuntu 22.04 及更早版本使用更低版本的 glibc，**不能直接运行这个包**；
- 发行包声明架构为 `amd64`；ARM64、RISC-V 等架构没有官方包。

Debian 或其它 Ubuntu 衍生发行版可能满足依赖，但它们不属于 0.24.15 的正式支持范围。
在 WSL2 中运行 Ubuntu 属于 Linux 支持路径，不等于原生 Windows 支持。

## 系统要求

- Ubuntu Linux x86_64；
- `glibc >= 2.39`；
- 本地回环端口，默认 `127.0.0.1:7788`；
- 可写的用户数据目录，默认 `~/.local/share/skynet-download`；
- systemd 用户服务可选；不启用时可用 `skyget daemon ensure` 直接拉起 daemon；
- 桌面 GUI 需要 X11 或 Wayland 图形会话；
- HLS/DASH 需要额外安装 `ffmpeg`；直链、BT、上传、MCP 和 daemon 不依赖 ffmpeg；
- 缓存和水位线需要预留足够磁盘空间。未设置 `SKYGET_CACHE_MAX_BYTES` 时不会自动按容量清理。

可以先运行：

```bash
skyget doctor
```

它会检查版本、目录写权限、daemon、Unix socket、身份、上传策略和落地策略。

## 当前不支持

- 原生 Windows 10/11；
- macOS 官方发行包或官方支持承诺；
- Ubuntu ARM64、Windows on ARM、Apple Silicon 官方包；
- 跨机器共享缓存服务；
- 把 daemon 直接暴露到公网。

仓库中包含部分 macOS 条件编译代码、交叉类型检查和 macOS CI 配置，但这只说明源码在
为移植做准备，不等于 0.24.15 已发布 macOS 版本。

## 路线图

### 1. Ubuntu 正式发行线

- 从 Ubuntu 24.04 LTS 基线构建，降低 glibc 下限；
- 提供 Ubuntu 24.04 / 26.04 x86_64 CI 安装与升级测试；
- 补 deb 安装、卸载、升级和用户数据保留测试；
- 生成可复现的 SHA-256、签名和发布证明。

### 2. macOS

目标是提供与 Linux 同等级的 CLI、daemon、MCP、GUI 和原生生命周期集成：

- 官方 Apple Silicon 与 Intel 构建；
- Homebrew 或签名压缩包分发；
- `launchd` 生命周期与升级流程；
- 原生菜单栏图标，替代 Linux D-Bus 托盘；
- 路径、文件锁、进程存活探测和 Unix socket 的真机兼容测试。

当前状态：源码可探索，没有官方发行物。

### 3. Windows 10/11

目标是原生命令行、daemon、MCP 和桌面应用，不要求用户通过 WSL：

- Windows 原生路径、文件锁和进程存活探测；
- 用 Named Pipe 替代 Unix socket 的本机身份通道；
- Windows Service 或用户会话级 daemon 生命周期；
- GUI、托盘、安装器和卸载流程；
- Windows CI 中的下载、续传、上传、重启和并发测试；
- 明确 Defender、代理、长路径和企业证书环境下的行为。

当前状态：未支持，处于架构评估阶段。

### 4. 与平台无关的能力

- Metalink 1.0；
- S3 multipart 上传与远端 checksum；
- `--header` / 私有源鉴权；
- per-agent 带宽与缓存容量配额；
- 连接数压力测试、DNS rebinding 加固；
- HLS/BT 子请求的完整出网边界。

## 如何判断兼容性

遇到平台问题时，请附上：

```bash
cat /etc/os-release
uname -m
ldd --version | head -1
skyget --version
skyget --json daemon status
```

以及安装方式、完整错误码和最小复现步骤。不要在 issue 中粘贴 token、cookie、
预签名 URL 或云凭证。
