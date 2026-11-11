//! skyget —— 天网盒子的命令行面。
//!
//! 给 agent 的约定（改形状等于改 API）：
//!   stdout 只放一行最终结果 JSON（--json）；进度事件走 stderr 的 NDJSON。
//!   退出码：0 成功，1 失败。失败原因的 error 字段是人话，能直接说给人听。

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use skyfetch::engine::{clock as human_clock, human};
use skyfetch::model::{new_id, now_ms, Task, STATE_DONE};
use skyfetch::{Engine, Event, Store};

const EXAMPLES: &str = "\
例子：
  skyget https://example.org/big.iso                   下到 ~/Downloads
  skyget https://example.org/x.tgz -o ~/apps -n x.tgz  指定目录和名字
  skyget https://example.org/bin --sha256 <hex>        校验不符就丢弃，不落地
  skyget --json https://… --as claude                  agent：一行结果 + 事件流
  skyget dest ~/视频/天网下载                            以后都下到这儿（不存在会建）
  skyget statusline [--format json --watch]             状态栏 / TUI 插件
  skyget list · skyget show <id 前几位> · skyget verify --deep
  skyget ensure box.json                               对账：已经对的不重下
  skyget keygen --out rel.key · skyget sign box.json --key @rel.key --in-place
  SKYGET_API=unix:$HOME/.local/share/skynet-download/daemon.sock 不用 token";

#[derive(Parser)]
#[command(
    name = "skyget",
    version,
    about = "天网盒子 · 下载、上传、校验、调度",
    override_usage = "skyget <URL> [选项]\n       skyget <COMMAND> [选项]",
    after_help = EXAMPLES
)]
struct Cli {
    /// 机器可读：stdout 只留一行结果 JSON（放哪儿都行）
    #[arg(long, global = true)]
    json: bool,

    /// 这条命令最多等多久（秒）。到点就退出，不再等 ——
    /// 不等 ≠ 取消：已下的分片留着（重跑接着下），daemon 里的任务继续跑。
    #[arg(long, global = true, value_name = "SECS")]
    timeout: Option<u64>,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

/// `skyget daemon …`
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum DaemonAction {
    /// 没在跑就拉起来（systemd 优先，其次直启）；已经在跑就什么都不做
    Ensure,
    /// 在不在跑、谁管的、pid / socket / 版本
    Status,
    /// 停下（systemd 服务优先，其次按 pid）
    Stop,
}

#[derive(Subcommand)]
enum Cmd {
    /// 下一个地址：下完、校验、落地，一次调用拿结果
    Get(GetArgs),
    /// 台账：下过什么、多大、落在哪
    List {
        #[arg(short = 'l', long, default_value_t = 20)]
        limit: usize,
        /// 只看自己下的（没有来源标签的那些）
        #[arg(long)]
        mine: bool,
        /// 只看 agent / 机库这类程序发起的
        #[arg(long)]
        agents: bool,
        /// 按来源前缀筛（cli / human / agent:hangar …）
        #[arg(long)]
        source: Option<String>,
    },
    /// 查一条（id 可以只写前几位）
    Show {
        /// 任务 id（可以只写前几位）
        id: Option<String>,
        /// 按地址查最近的一条记录（省掉从 list 里抄 id 那一步）
        #[arg(long)]
        url: Option<String>,
    },
    /// 把地址换成缓存里的回环地址（命中时零网络；给只会 curl 的调用方用）
    Resolve {
        /// 原地址，要和台账里存的完全一致
        url: String,
        /// 让服务端发之前重新算一遍 sha256（防坏块；慢）
        #[arg(long)]
        verify: bool,
    },
    /// curl 的替身：只接管「一个地址 + 落盘」，其余原样交给真 curl
    Curl {
        /// 原样的 curl 参数（-o / -L / -s / --as 之外一律透传）
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// 按清单装：下载 + 校验 + 落位 + 权限
    Install {
        src: String,
        /// 可信公钥：64 位十六进制 / `id=hex` / `@文件`（可重复）。给了就必须验签
        #[arg(long = "key")]
        key: Vec<String>,
        /// 清单必须带签名（没有就直接失败）
        #[arg(long)]
        require_signature: bool,
    },
    /// 按清单对账：已经正确的文件不联网、不写盘（只增改，不删）
    Ensure {
        src: String,
        #[arg(long = "key")]
        key: Vec<String>,
        #[arg(long)]
        require_signature: bool,
    },
    /// 打印清单的规范化摘要（给外部签名工具用：签这 32 字节）
    ManifestDigest { src: String },
    /// 生成发布用密钥（ed25519）：打印私钥（写进文件保管）和公钥（发给使用方）
    Keygen {
        /// 把私钥写到这个文件（0600）；不给就只打印
        #[arg(long)]
        out: Option<String>,
    },
    /// 给清单签名（把 `signature` 写回清单；默认只打印不修改）
    Sign {
        src: String,
        /// 私钥：十六进制，或 `@文件`
        #[arg(long)]
        key: String,
        #[arg(long = "key-id", default_value = "")]
        key_id: String,
        /// 直接写回清单文件（只对本地文件有效）
        #[arg(long)]
        in_place: bool,
    },
    /// 上传本地文件（默认 HTTP PUT；--protocol tus 可续传）
    Upload {
        /// 本地文件（必须在 SKYGET_UPLOAD_ROOTS 里）
        path: String,
        /// 目标地址
        url: String,
        #[arg(long, default_value = "put")]
        protocol: String,
        /// 本地文件期望的 sha256；不符就不传
        #[arg(long)]
        sha256: Option<String>,
        /// 凭证引用：env:NAME / file:PATH
        #[arg(long)]
        credential_ref: Option<String>,
        #[arg(long = "as", default_value = "cli")]
        as_who: String,
        /// 排队优先级：interactive / normal / batch
        #[arg(long, default_value = "normal", value_parser = ["interactive", "normal", "batch"])]
        priority: String,
    },
    /// 预检：这条地址提交上去会被怎么判（不入队、不联网，和真提交同一套规则）
    Check {
        /// 要检查的地址
        url: String,
        /// 落地目录（默认用 daemon 的默认目录）
        #[arg(short = 'o', long = "out")]
        dest: Option<String>,
        /// 文件名（配合 -o 目录）
        #[arg(short = 'n', long)]
        name: Option<String>,
        /// 期望 sha256（多源时必填）
        #[arg(long)]
        sha256: Option<String>,
        /// 额外镜像（可重复）
        #[arg(long = "mirror")]
        mirrors: Vec<String>,
        /// 上传时的本地源文件（检查 SKYGET_UPLOAD_ROOTS）
        #[arg(long)]
        path: Option<String>,
        /// 凭证引用：env:NAME / file:PATH
        #[arg(long = "credential-ref")]
        credential_ref: Option<String>,
    },
    /// 事件流：按 NDJSON 打印一条任务的事件（--follow 持续跟到终态）
    Events {
        id: String,
        /// 从哪个 seq 之后开始（0 = 从头）
        #[arg(long, default_value_t = 0)]
        since: i64,
        /// 持续跟（像 tail -f），到终态自动退出
        #[arg(long)]
        follow: bool,
    },
    /// 跟一条守护进程里的任务：事件流 + 最终结果（agent 断线后接着读）
    Watch {
        id: String,
        /// 上次读到的 seq；0 = 从头
        #[arg(long, default_value_t = 0)]
        since: i64,
        /// 没有新事件时最多等多久（毫秒）
        #[arg(long, default_value_t = 25_000)]
        wait_ms: u64,
    },
    /// 钉住一份缓存内容（agent 说「我在用，别清」）
    Pin {
        sha256: String,
        /// 保留多久：7d / 12h / 90m；默认 7 天
        #[arg(long, default_value = "7d")]
        ttl: String,
        /// 不自动过期（直到显式 unpin）
        #[arg(long)]
        forever: bool,
        /// 哪个 agent 钉的（记成 agent:<name>）
        #[arg(long = "as", default_value = "cli")]
        as_who: String,
        #[arg(long)]
        note: Option<String>,
    },
    /// 解除 pin；不给 --agent 就去掉所有 agent 的 pin
    Unpin {
        sha256: String,
        #[arg(long = "as")]
        agent: Option<String>,
    },
    /// 列出所有 pin
    Pins,
    /// 机群：daemon 上有哪些 agent、各自能下到哪、正在跑几个
    Agents {
        #[arg(long)]
        json: bool,
    },
    /// 体检：本机目录/写权限/daemon 连通性/身份与策略，部署后用一条命令确认
    Doctor {
        #[arg(long)]
        json: bool,
    },
    /// 导出内容缓存（CI / 离线预热）：不给 sha 就导出全部
    Export {
        dir: String,
        #[arg(value_name = "SHA256")]
        shas: Vec<String>,
    },
    /// 从导出目录导入内容缓存（每个块重新校验）
    Import { dir: String },
    /// 重新排队一条失败/中断的任务（同记录、同期望 sha）
    Retry {
        id: String,
        /// 用这个 agent 的 token 重跑（本机 SKYGET_AGENTS 里要有）
        #[arg(long = "as")]
        as_who: Option<String>,
    },
    /// 暂停：让下载线程自己停，已经下好的分片留着（resume 接着下）
    Pause {
        /// 任务 id（可以只写前几位）
        id: String,
        /// 用这个 agent 的 token 发请求（本机 SKYGET_AGENTS 里要有）
        #[arg(long = "as")]
        as_who: Option<String>,
    },
    /// 继续：接着上次下，一字节都不重下
    Resume {
        id: String,
        #[arg(long = "as")]
        as_who: Option<String>,
    },
    /// 取消：停下并清掉临时分片（已落地的文件不动），这条不会自己回来
    Cancel {
        id: String,
        #[arg(long = "as")]
        as_who: Option<String>,
    },
    /// 只删台账记录，不动盘上文件（任务必须先停下）
    Forget {
        /// 任务 id（可以只写前几位）
        id: String,
        #[arg(long = "as")]
        as_who: Option<String>,
    },
    /// 终端驾驶舱：队列 / 进度 / 来源 / 信任（--watch 持续刷新）
    Top {
        #[arg(long)]
        watch: bool,
        #[arg(long, default_value_t = 1000)]
        interval_ms: u64,
        #[arg(short = 'l', long, default_value_t = 50)]
        limit: usize,
    },
    /// 单行进度条：给状态栏、TUI 插件和其他 agent 用
    Statusline {
        /// 输出格式：ansi（默认）、plain（无颜色）、json（NDJSON）
        #[arg(long, default_value = "ansi")]
        format: String,
        /// 持续刷新，直到没有活跃任务
        #[arg(long)]
        watch: bool,
        /// 刷新间隔（毫秒）
        #[arg(long, default_value_t = 1000)]
        interval_ms: u64,
        /// 指定任务；默认自动选最新的 running/pending
        #[arg(long)]
        job: Option<String>,
        /// 只看某个来源前缀（如 agent:claude）
        #[arg(long)]
        source: Option<String>,
        /// 同时向终端发 OSC 9;4 原生进度
        #[arg(long)]
        osc: bool,
        /// 进度条宽度（格）；0 = 自动（看 COLUMNS，扣掉标签占的格）
        #[arg(long, default_value_t = 0)]
        width: usize,
    },
    /// 自检：台账、落地文件、内容缓存、分片位图是否彼此自洽
    Verify {
        /// 重算 sha256（慢，等于把所有文件读一遍）
        #[arg(long)]
        deep: bool,
        /// 只修安全的：废弃作废的分片位图
        #[arg(long)]
        repair: bool,
    },
    /// 起本地接口，让别的进程也能入队（只绑 127.0.0.1）
    Serve {
        #[arg(short = 'p', long, default_value_t = 7788)]
        port: u16,
    },
    /// 清内容缓存：只删没人在用、而且很久没碰过的（默认 30 天）
    Prune {
        /// 多久没碰过才算旧：30d / 12h / 90m
        #[arg(long, default_value = "30d")]
        older_than: String,
        /// 只清到缓存总量不超过这个值（如 10GB）；受保护/年轻的块仍然不删
        #[arg(long)]
        max_bytes: Option<String>,
        /// 真删（不给就只报计划，不动手）
        #[arg(long)]
        yes: bool,
    },
    /// 下载保存到哪：不给路径就报当前，给了就设成它（不存在会建）
    Dest {
        /// 新目录（可以是还不存在的路径，`~/` 也认）
        path: Option<String>,
        /// 忘掉设置，回到默认（~/Downloads）
        #[arg(long)]
        reset: bool,
    },
    /// 这台机器到底在下什么：总量、命中率、失败分类、来源
    Stats,
    /// 自带体检：本机起一个假服务器，把关键路径全跑一遍（不联网）
    Selftest,
    /// 守护进程生命周期：ensure（按需拉起）/ status / stop
    Daemon {
        #[arg(value_enum)]
        action: DaemonAction,
    },
    /// 生成 shell 补全脚本（bash / zsh / fish / elvish / powershell）
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// 浏览天网机库：先看有什么，再决定拿哪个
    Catalog {
        #[command(subcommand)]
        sub: CatalogCmd,
    },
}

#[derive(clap::Args, Debug)]
struct GetArgs {
    /// 地址（http/https、magnet、.torrent、.m3u8…）
    url: String,
    /// 落地目录（默认 ~/Downloads）
    #[arg(short = 'o', long = "out", visible_alias = "to")]
    out: Option<String>,
    /// 另存名字（默认用服务器给的文件名）
    #[arg(short = 'n', long)]
    name: Option<String>,
    /// 要求内容的 sha256；不符就失败并丢弃
    #[arg(long)]
    sha256: Option<String>,
    /// 从发布方的 .sha256 文件取校验值
    #[arg(long = "sha256-from", visible_alias = "sha256-url")]
    sha256_from: Option<String>,
    /// 下载凭证引用：env:NAME / file:PATH；作为 Authorization: Bearer
    #[arg(long = "credential-ref")]
    credential_ref: Option<String>,
    /// 地址内容变了也接受（正常更新时用）
    #[arg(long)]
    accept_change: bool,
    /// HLS/DASH：分片并行预取后再 remux（默认关闭；不适用的流自动回退）
    #[arg(long)]
    hls_fast: bool,
    /// 进度输出：auto（默认，只在终端里画）/ stderr（管道里也写，一行一条）/ off
    #[arg(long, default_value = "auto")]
    progress: String,
    /// 并发连接数（0 = 按文件大小自动；1 = 不分段，遇到假 Range 源时用）
    #[arg(short = 'j', long = "jobs", visible_alias = "threads", default_value_t = 0)]
    jobs: usize,
    /// 谁发起的，记进台账（写 claude 会记成 agent:claude）
    #[arg(long = "as", default_value = "cli")]
    as_who: String,
    /// 强制交给守护进程跑（默认：来源是 agent 时尝试，连不上再回退）
    #[arg(long)]
    daemon: bool,
    /// 提交后立即返回；任务完全交给 daemon，客户端断开不影响
    #[arg(long, conflicts_with = "standalone")]
    detach: bool,
    /// 强制独立下载，不经过守护进程
    #[arg(long, conflicts_with = "daemon")]
    standalone: bool,
    /// 额外镜像地址（可重复；多源必须有 --sha256）
    #[arg(long = "mirror")]
    mirrors: Vec<String>,
    /// 排队优先级：interactive（有人等着）/ normal / batch（夜间预热）
    #[arg(long, default_value = "normal", value_parser = ["interactive", "normal", "batch"])]
    priority: String,
}

impl GetArgs {
    /// 台账里的来源：`claude` → `agent:claude`，`cli`/`human` 原样
    fn source(&self) -> String {
        source_of(&self.as_who)
    }
}

/// 验签策略（fail closed）：
///
/// - 给了 `--key`（或 `SKYGET_TRUSTED_KEYS`）→ 必须先有签名，再验；
/// - 清单带签名但没有可信公钥 → 报错，不「当作没看见」；
/// - 既没签名也没给 key → 保持旧行为（只有 sha256；文档里明确说明这不够）。
fn check_manifest_trust(
    m: &skyfetch::manifest::Manifest,
    keys: &[String],
    require_signature: bool,
) -> Result<()> {
    let mut parsed: Vec<(String, Vec<u8>)> = Vec::new();
    for k in keys {
        parsed.push(skyfetch::manifest::parse_key(k)?);
    }
    if let Ok(spec) = std::env::var("SKYGET_TRUSTED_KEYS") {
        for item in spec.split(',') {
            if !item.trim().is_empty() {
                parsed.push(skyfetch::manifest::parse_key(item)?);
            }
        }
    }
    if require_signature && m.signature.is_none() {
        anyhow::bail!("清单没有签名，但它被要求必须有（--require-signature）");
    }
    if m.signature.is_some() || (!parsed.is_empty() && require_signature) {
        skyfetch::manifest::verify_signature(m, &parsed)?;
    } else if !parsed.is_empty() && m.signature.is_none() {
        anyhow::bail!("给了可信公钥，但清单没有签名；不能证明这份清单是谁给的");
    }
    Ok(())
}

/// 私钥文件按 0600 建：它一旦泄露，发布链路就整个失去意义。
fn write_private(path: &Path, hex: &str) -> Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| anyhow::anyhow!("写不了私钥文件（{}）：{e}", path.display()))?;
        f.write_all(hex.as_bytes())?;
        f.write_all(b"\n")?;
        return Ok(());
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, format!("{hex}\n"))?;
        Ok(())
    }
}

/// `--as <名字>`：这一次请求用注册表里该 agent 的 token。
///
/// 身份在 daemon 那边只认 token（请求体自称不算），所以本机必须能从
/// `SKYGET_AGENTS` 里找到这个名字 —— 找不到就明确失败，不假装成功。
fn token_for_agent(name: &str) -> Result<String> {
    let spec = std::env::var("SKYGET_AGENTS").map_err(|_| {
        anyhow::anyhow!(
            "--as 需要本机有 SKYGET_AGENTS（token=名字,…）；daemon 的身份只认 token，"
        )
    })?;
    for item in spec.split(',') {
        if let Some((tok, n)) = item.split_once('=') {
            if n.trim() == name.trim() && !tok.trim().is_empty() {
                return Ok(tok.trim().to_string());
            }
        }
    }
    anyhow::bail!(
        "SKYGET_AGENTS 里没有叫「{}」的 token（现有：{}）",
        name.trim(),
        spec.split(',')
            .filter_map(|i| i.split_once('=').map(|(_, n)| n.trim()))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// id 前几位 → 台账里的那条。`show` 和 pause/resume/cancel 共用同一套规则：
/// 完整 id 直接命中；否则按前缀找，唯一才认，多于一条就报「写长一点」。
fn find_task(store: &skyfetch::Store, id: &str) -> Result<skyfetch::Task> {
    let id = id.trim();
    if let Some(t) = store.get(id)? {
        return Ok(t);
    }
    let hits: Vec<_> = store
        .list(1000)?
        .into_iter()
        .filter(|t| t.id.starts_with(id))
        .collect();
    match hits.len() {
        1 => Ok(hits.into_iter().next().unwrap()),
        0 => anyhow::bail!("台账里没有这个任务：{id}"),
        n => anyhow::bail!("id 前缀 {id} 匹配到 {n} 条，写长一点"),
    }
}

/// 传输控制（暂停 / 继续 / 取消）。界面上的按钮走的就是这几个接口 ——
/// 不给 CLI 留一条「只能看不能停」的路。
///
/// `id` 可以只写前几位：先查台账把它补全（和 `show` 一样的手感）。
///
/// 没有 `--by`：事件里记的「谁停的」是 token 绑定的身份，不是调用方自称 ——
/// 加一个能被自己写死的 by 只会让人误以为那是审计依据。
fn control(json: bool, id: &str, verb: &str, as_who: Option<&str>) -> Result<()> {
    let store = skyfetch::Store::open()?;
    let full = find_task(&store, id)?.id;
    let token = match as_who {
        Some(n) if !n.trim().is_empty() => Some(token_for_agent(n)?),
        _ => None,
    };
    // pause → resume 之间要等引擎收摊；按 daemon 给的 retry_after 等，别把 409 甩给人。
    let v = api_json_retry(&format!("/jobs/{full}/{verb}"), 6, token.as_deref())?;
    if json {
        println!("{}", serde_json::to_string(&v)?);
        return Ok(());
    }
    let word = match verb {
        "pause" => "已暂停（已下的分片留着，resume 接着下）",
        "resume" => "已继续（不会从头下）",
        _ => "已取消（临时分片清掉，已落地的文件不动）",
    };
    println!("{word}  {}", &full[..12.min(full.len())]);
    Ok(())
}

fn source_of(who: &str) -> String {
    let v = who.trim();
    if v.is_empty() || v == "cli" || v == "human" || v.contains(':') {
        v.to_string()
    } else {
        format!("agent:{v}")
    }
}

#[derive(Subcommand)]
enum CatalogCmd {
    List {
        #[arg(long)]
        query: Option<String>,
        #[arg(short = 'l', long, default_value_t = 40)]
        limit: usize,
    },
    Show { id: String },
}

/// 这个参数是不是一个下载地址（用来把 `skyget <url>` 补成 `skyget get <url>`）。
///
/// 只认明确的地址形状：选项的值（目录、名字、hash）不会长成这样，所以不会误判。
fn looks_like_url(s: &str) -> bool {
    let l = s.trim().to_ascii_lowercase();
    if l.starts_with("http://") || l.starts_with("https://") || l.starts_with("magnet:") {
        return true;
    }
    let no_q = l.split(['?', '#']).next().unwrap_or(&l);
    [".torrent", ".m3u8", ".mpd"].iter().any(|e| no_q.ends_with(e))
}

/// 从 `Authorization: Bearer xxx` 取出 token；不是这个形状就还给真 curl。
fn bearer_of(header: &str) -> Option<String> {
    let (name, value) = header.split_once(':')?;
    if !name.trim().eq_ignore_ascii_case("authorization") {
        return None;
    }
    let (scheme, tok) = value.trim().split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let tok = tok.trim();
    if tok.is_empty() {
        None
    } else {
        Some(tok.to_string())
    }
}

/// 已知的子命令名从 clap 自己那份定义里取，不手抄。
///
/// 手抄的表漏过 `ensure` / `sign` / `manifest-digest`：它们带 http 地址参数时
/// 会被"隐式 get"误判成 `skyget get ensure <url>`，报一句看不懂的
/// 「unexpected argument」。从 `Cli::command()` 取，加子命令就不必记得改两处。
fn known_subcommands() -> Vec<String> {
    use clap::CommandFactory;
    Cli::command()
        .get_subcommands()
        .map(|c| c.get_name().to_string())
        .collect()
}

/// `skyget <url> …` 等价于 `skyget get <url> …`。
///
/// 判断只看两件事：有没有子命令、有没有地址形状的参数。
/// 没有子命令但有地址 → 在 `argv[1]` 插一个 `get`（不是插在地址前面：
/// 选项写在地址前面时 `skyget -o /tmp <url>` 也要能成立）。
fn with_implicit_get(argv: Vec<std::ffi::OsString>) -> Vec<std::ffi::OsString> {
    let known = known_subcommands();
    let words: Vec<&str> = argv.iter().skip(1).filter_map(|a| a.to_str()).collect();
    if words.iter().any(|w| known.iter().any(|k| k == w)) {
        return argv;
    }
    if !words.iter().any(|w| looks_like_url(w)) {
        return argv;
    }
    let mut out = argv;
    out.insert(1, std::ffi::OsString::from("get"));
    out
}

fn main() {
    let argv = with_implicit_get(std::env::args_os().collect());
    // 用法错误（exit 2）也要能被机器读：`--json` 时除了 stderr 的人话，
    // stdout 再给一行 `cli.usage`。以前参数写错时 agent 什么结构化信息都拿不到。
    let wants_json = argv.iter().any(|a| a == "--json");
    let cli = match Cli::try_parse_from(&argv) {
        Ok(c) => c,
        Err(e) => {
            use clap::error::ErrorKind;
            let kind = e.kind();
            // --help / --version 是「用户要的信息」，不是错误：照 clap 的排版打出去。
            if matches!(
                kind,
                ErrorKind::DisplayHelp
                    | ErrorKind::DisplayVersion
                    | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
            ) {
                let _ = e.print();
                let code = if matches!(kind, ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand) {
                    2
                } else {
                    0
                };
                std::process::exit(code);
            }
            let _ = e.print();
            if wants_json {
                // JSON 里只放一句结论：clap 的整段提示（Usage / 更多信息）留给 stderr，
                // 机器读的那行不该塞一屏排版。
                let full = e.to_string();
                let brief = full
                    .split("\n\nUsage:")
                    .next()
                    .unwrap_or(&full)
                    .trim()
                    .trim_start_matches("error: ")
                    .to_string();
                println!(
                    "{}",
                    serde_json::json!({
                        "state": "failed",
                        "error": brief,
                        "error_code": "cli.usage",
                        "retryable": false,
                        "next_action": "看 `skyget --help` 或 `skyget <子命令> --help`。",
                    })
                );
            }
            std::process::exit(2);
        }
    };
    let json = cli.json;
    if let Some(secs) = cli.timeout {
        // 看门狗：到点直接结束进程。
        // 为什么不「优雅取消」：CLI 的等待可能卡在 DNS、TLS、长轮询、写盘任意一处，
        // 真正的取消只有引擎知道怎么做；而硬退出的代价已经被产品兜住了 ——
        // 分片位图是落盘的，重跑会接着下；daemon 里的任务本来就不受影响。
        let secs = secs.max(1);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(secs));
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "state": "failed",
                        "error": format!("--timeout {secs}s 到点：这条命令不再等了"),
                        "error_code": "cli.timeout",
                        "retryable": true,
                        "next_action": "重跑同一条命令会接着下（分片留着）；daemon 里的任务没被取消。",
                    })
                );
            }
            eprintln!(
                "--timeout {secs}s 到点：不等了。已下的分片留着，重跑接着下；daemon 里的任务继续跑。"
            );
            std::process::exit(1);
        });
    }
    let code = match run(cli) {
        Ok(()) => 0,
        Err(e) => {
            // 顶层错误也可能带源站/agent 可控的 URL 或响应文本；逐行去控制字符，
            // 保留换行，避免终端转义注入。
            let msg = format!("{e:#}");
            for line in msg.lines() {
                eprintln!("{}", display_safe(line));
            }
            // 契约：`--json` 时 stdout 永远是一行 JSON，失败也一样。
            // 以前 CLI 层失败只有 stderr 的中文，agent 得去解析人话才能知道
            // 是「连不上 daemon」还是「参数写错」。
            if json && !REPORTED.load(std::sync::atomic::Ordering::Relaxed) {
                println!(
                    "{}",
                    serde_json::json!({
                        "state": "failed",
                        "error": display_safe(&msg),
                        "error_code": "cli.failed",
                        "retryable": false,
                    })
                );
            }
            1
        }
    };
    std::process::exit(code);
}

fn run(cli: Cli) -> Result<()> {
    let json = cli.json;
    let store = Store::open()?;
    // 什么都不给就看用法（裸地址在 main 里已经被补成 get 了）
    let Some(cmd) = cli.cmd else {
        println!("{EXAMPLES}");
        return Ok(());
    };
    match cmd {
        Cmd::Get(a) => {
            let source = a.source();
            let GetArgs {
                url,
                out,
                name,
                sha256,
                sha256_from,
                credential_ref,
                accept_change,
                jobs,
                daemon,
                detach,
                standalone,
                mirrors,
                hls_fast,
                progress,
                priority,
                ..
            } = a;
            if hls_fast {
                std::env::set_var("SKYGET_HLS_FAST", "1");
            }
            if progress != "auto" {
                if !matches!(progress.as_str(), "stderr" | "off") {
                    anyhow::bail!("--progress 只支持 auto / stderr / off（当前 {progress}）");
                }
                std::env::set_var("SKYGET_PROGRESS", &progress);
            }
            let engine = Engine::new(jobs);
            // 校验值从哪来，决定这次下载的信任级别 —— 别混成一句「校验过了」
            let (expected, trust) = if let Some(u) = sha256_from.as_deref() {
                (
                    fetch_hash(&engine, u).with_context(|| format!("取不到校验文件：{u}"))?,
                    "indexed",
                )
            } else {
                match sha256.as_deref() {
                    Some(h) if !h.trim().is_empty() => (h.trim().to_ascii_lowercase(), "pinned"),
                    _ => (String::new(), ""),
                }
            };
            if !expected.is_empty() && !skyfetch::security::is_sha256_hex(&expected) {
                anyhow::bail!("--sha256 必须是 64 位十六进制（当前 {} 位）", expected.len());
            }
            if !mirrors.is_empty() && expected.is_empty() {
                anyhow::bail!("多源下载必须同时给 --sha256（用来判断哪份内容是对的）");
            }
            for m in &mirrors {
                let l = m.trim().to_ascii_lowercase();
                if !l.starts_with("http://") && !l.starts_with("https://") {
                    anyhow::bail!("--mirror 只支持 http/https 镜像：{m}");
                }
            }
            let credential_ref = credential_ref
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty());
            if let Some(c) = credential_ref.as_deref() {
                if !c.starts_with("env:") && !c.starts_with("file:") {
                    anyhow::bail!("--credential-ref 只支持 env:NAME / file:PATH");
                }
            }
            let task_sources = if mirrors.is_empty() {
                Vec::new()
            } else {
                let mut v = vec![url.clone()];
                for m in &mirrors {
                    let m = m.trim().to_string();
                    if m != url && !v.contains(&m) {
                        v.push(m);
                    }
                }
                v
            };

            // `-o` 永远是**目录**（可能还不存在），文件名由服务器或 `-n` 给。
            // 目录用“以 / 结尾”的写法传给内核，未创建的目录才不会被当成文件名。
            // 没给 -o 就用「用户设过的目录」—— 这里以前直接写死默认目录，
            // 结果是 skyget dest 设完、下载还是落到 ~/Downloads（设了不生效最难查）。
            let dir = out
                .map(|o| PathBuf::from(skyfetch::settings::expand(&o)))
                .unwrap_or_else(skyfetch::settings::dest_dir);
            let dest = match &name {
                Some(n) => dir.join(n).display().to_string(),
                None => {
                    let mut d = dir.display().to_string();
                    if !d.ends_with('/') {
                        d.push('/');
                    }
                    d
                }
            };

            // agent 的入口默认走守护进程：提交、排队、事件流、断线重连都由它管。
            // 人自己用（不带 --as）保持原来的独立下载，不强制装/起 daemon。
            // --daemon / --standalone 显式覆盖；SKYGET_DAEMON=1/0 也可。
            let forced = if standalone {
                Some(false)
            } else if daemon || detach {
                Some(true)
            } else {
                None
            };
            let use_daemon = forced.unwrap_or_else(|| {
                match std::env::var("SKYGET_DAEMON").ok().as_deref() {
                    Some("1") => true,
                    Some("0") => false,
                    _ => source.starts_with("agent:"),
                }
            });
            if use_daemon {
                let dir_arg = dir.display().to_string();
                match run_via_daemon(
                    json,
                    &url,
                    &dir_arg,
                    name.as_deref(),
                    &expected,
                    &source,
                    &mirrors,
                    credential_ref.as_deref(),
                    detach,
                    &priority,
                ) {
                    Ok(DaemonRun::Done) => return Ok(()),
                    Ok(DaemonRun::Unavailable(why)) => {
                        if forced == Some(true) {
                            anyhow::bail!(
                                "守护进程没连上：{why}。先启动 skynet-dataplaned（旧名 skynet-downloadd），或加 --standalone 独立下载。"
                            );
                        }
                        eprintln!(
                            "守护进程没连上（{why}），这次独立下载。加 --daemon 可强制走守护进程。"
                        );
                    }
                    Err(e) => return Err(e),
                }
            }

            // 同一个地址已经下过、文件还在**且就在这次要的目录里**：不再问服务器，
            // 直接把结果还回去。指定了 --name，或 --to 换了目录时不走这条捷径 ——
            // 以前只按 URL 短路，会返回旧路径、把用户明确指定的新目录忽略掉。
            if sha256.is_none() && sha256_from.is_none() && name.is_none() {
                if let Some(prev) = store.find_done_by_url(&url)? {
                    let same_dir = Path::new(&prev.dest).parent() == Some(dir.as_path());
                    if same_dir {
                        let view = prev.view(true, 0);
                        emit(&view, json);
                        return Ok(());
                    }
                }
            }

            let mut task = Task {
                id: new_id(&url),
                direction: skyfetch::model::DIR_DOWNLOAD.to_string(),
                protocol: String::new(),
                credential_ref: credential_ref.clone().unwrap_or_default(),
                sources: task_sources,
                url,
                dest,
                state: skyfetch::model::STATE_PENDING.to_string(),
                bytes_done: 0,
                bytes_total: 0,
                sha256: String::new(),
                error: String::new(),
                error_code: String::new(),
                retryable: false,
                retry_after_ms: 0,
                next_action: String::new(),
                source,
                priority: skyfetch::model::PRIORITY_NORMAL.to_string(),
                expected_sha256: expected,
                pid: 0,
                links_live: 0,
                links_seen: 0,
                trust: trust.to_string(),
                accept_change,
                cached: false,
                rerun: false,
                created_at: now_ms(),
                updated_at: now_ms(),
            };

            let cached_flag = std::sync::atomic::AtomicBool::new(false);
            let cb = make_cb(json, &cached_flag);
            let started = Instant::now();
            // QA 旋钮：SKYGET_QA_CANCEL_MS=1500 → 1.5 秒后按一次「暂停」。
            // 界面上的暂停按钮走的是同一条路（skyfetch::Cancel），这样不用鼠标也能验。
            let cancel = skyfetch::Cancel::new();
            if let Some(ms) = std::env::var("SKYGET_QA_CANCEL_MS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
            {
                let c = cancel.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(ms));
                    c.cancel();
                });
            }
            let result = skyfetch::run_cancellable(&store, &engine, &mut task, &cb, &cancel);

            if !json {
                eprintln!();
            }
            match result {
                Ok(()) => {
                    let cached = cached_flag.load(std::sync::atomic::Ordering::Relaxed);
                    emit(&task.view(cached, started.elapsed().as_millis() as u64), json);
                    Ok(())
                }
                Err(e) => {
                    let mut view = task.view(false, started.elapsed().as_millis() as u64);
                    if view.error.is_empty() {
                        view.error = format!("{e:#}");
                    }
                    // 校验不通过要丢弃文件：盘上没有东西，就别把目标路径当结果报出去
                    if !std::path::Path::new(&view.path).exists() {
                        view.path.clear();
                    }
                    emit(&view, json);
                    Err(e)
                }
            }
        }
        Cmd::Pin {
            sha256,
            ttl,
            forever,
            as_who,
            note,
        } => {
            let ttl_secs = if forever {
                0
            } else {
                skyfetch::prune::parse_age(&ttl)?.as_secs()
            };
            let body = serde_json::json!({
                "sha256": sha256,
                "agent": source_of(&as_who),
                "ttl_secs": ttl_secs,
                "note": note.unwrap_or_default(),
            });
            let v = api_json("POST", "/pins", Some(&body))?;
            if json {
                println!("{}", serde_json::to_string(&v)?);
            } else {
                println!(
                    "已钉住 {}  （{}）",
                    &sha256[..12.min(sha256.len())],
                    if forever {
                        "不过期".to_string()
                    } else {
                        format!("保留 {ttl}")
                    }
                );
            }
            Ok(())
        }
        Cmd::Unpin { sha256, agent } => {
            let path = match agent.as_deref().map(source_of) {
                Some(a) if !a.is_empty() => format!("/pins/{sha256}?agent={a}"),
                _ => format!("/pins/{sha256}"),
            };
            let v = api_json("DELETE", &path, None)?;
            if json {
                println!("{}", serde_json::to_string(&v)?);
            } else {
                println!("已解除 pin {}", &sha256[..12.min(sha256.len())]);
            }
            Ok(())
        }
        Cmd::Top {
            watch,
            interval_ms,
            limit,
        } => {
            let tty = std::io::stdout().is_terminal();
            loop {
                let v = api_json("GET", &format!("/jobs?limit={}", limit.clamp(1, 500)), None)?;
                let jobs = v.as_array().cloned().unwrap_or_default();
                if json {
                    // 给监控/脚本的快照：一次一条 JSON，字段和 `/jobs` 一致，
                    // 另外补一个 summary（省得调用方自己数）。
                    let count = |s: &str| jobs.iter().filter(|j| j["state"] == s).count();
                    println!(
                        "{}",
                        serde_json::json!({
                            "summary": {
                                "total": jobs.len(),
                                "running": count("running"),
                                "queued": count("pending"),
                                "done": count("done"),
                                "failed": count("failed"),
                            },
                            "jobs": jobs,
                        })
                    );
                } else {
                    if watch && tty {
                        print!("\x1b[2J\x1b[H");
                    }
                    print_top(&jobs);
                }
                if !watch {
                    break;
                }
                std::thread::sleep(Duration::from_millis(interval_ms.max(200)));
            }
            Ok(())
        }
        Cmd::Events { id, since, follow } => {
            let store = skyfetch::Store::open()?;
            let full = find_task(&store, &id)?.id;
            let mut since = since;
            let mut reconnects = 0u32;
            loop {
                let wait = if follow { 25_000 } else { 0 };
                let path = format!("/jobs/{full}/events?since={since}&wait_ms={wait}");
                let v = match api_json("GET", &path, None) {
                    Ok(v) => {
                        reconnects = 0;
                        v
                    }
                    // 跟着事件流时 daemon 重启是常态（升级 / 崩溃恢复）：
                    // 连不上就重连，别让 agent 因为一次重启丢掉整条任务。
                    Err(e) if follow && reconnects < 12 && format!("{e:#}").contains("连不上") => {
                        reconnects += 1;
                        let d = Duration::from_millis(500 * reconnects as u64);
                        eprintln!("  守护进程连接中断，{} 毫秒后重连（第 {reconnects}/12 次）", d.as_millis());
                        std::thread::sleep(d);
                        continue;
                    }
                    Err(e) => return Err(e),
                };
                let events = v["events"].as_array().cloned().unwrap_or_default();
                let mut terminal: Option<String> = None;
                for e in &events {
                    if let Some(seq) = e["seq"].as_i64() {
                        since = since.max(seq);
                    }
                    let kind = e["type"].as_str().unwrap_or("");
                    if matches!(kind, "done" | "failed" | "interrupted" | "cancelled") {
                        terminal = Some(kind.to_string());
                    }
                    // 一行一个 JSON：这条命令的输出是给脚本/agent 吃的，不做人类排版。
                    println!("{}", serde_json::to_string(e)?);
                }
                mark_reported();
                if !follow {
                    return Ok(());
                }
                if let Some(kind) = terminal {
                    return if kind == "done" {
                        Ok(())
                    } else {
                        anyhow::bail!("任务终态：{kind}")
                    };
                }
                if events.is_empty() {
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
            }
        }
        Cmd::Statusline {
            format,
            watch,
            interval_ms,
            job,
            source,
            osc,
            width,
        } => run_statusline(
            // 两个开关语义重叠过一次（--json 和 --format json 走不同分支）。
            // 现在只有一条路：--json 就是 --format json 的简写。
            if json { "json" } else { &format },
            watch,
            interval_ms,
            job.as_deref(),
            source.as_deref(),
            osc,
            width,
        ),
        Cmd::Export { dir, shas } => {
            let dest = PathBuf::from(skyfetch::settings::expand(&dir));
            let s = skyfetch::cas::export(&skyfetch::paths::blobs_dir(), &dest, &shas)?;
            if json {
                println!("{}", serde_json::to_string(&s)?);
            } else {
                println!(
                    "导出 {} 块 · {}（{}）",
                    s.blobs,
                    human(s.bytes),
                    dest.display()
                );
            }
            Ok(())
        }
        Cmd::Import { dir } => {
            let src = PathBuf::from(skyfetch::settings::expand(&dir));
            let s = skyfetch::cas::import(&skyfetch::paths::blobs_dir(), &src)?;
            if json {
                println!("{}", serde_json::to_string(&s)?);
            } else {
                println!(
                    "导入 {} 块 · {} · 跳过 {} 块（已存在且校验通过）",
                    s.blobs,
                    human(s.bytes),
                    s.skipped
                );
            }
            Ok(())
        }
        Cmd::Retry { id, as_who } => {
            let full = find_task(&store, &id)?.id;
            let token = match as_who.as_deref() {
                Some(n) if !n.trim().is_empty() => Some(token_for_agent(n)?),
                _ => None,
            };
            let v = api_json_as(
                "POST",
                &format!("/jobs/{full}/retry"),
                Some(&serde_json::json!({})),
                token.as_deref(),
            )?;
            if json {
                println!("{}", serde_json::to_string(&v)?);
            } else {
                println!("已重新排队 {}", &id[..12.min(id.len())]);
            }
            Ok(())
        }
        Cmd::Pause { id, as_who } => control(json, &id, "pause", as_who.as_deref()),
        Cmd::Resume { id, as_who } => control(json, &id, "resume", as_who.as_deref()),
        Cmd::Cancel { id, as_who } => control(json, &id, "cancel", as_who.as_deref()),
        Cmd::Forget { id, as_who } => {
            // 和 show / pause / resume / cancel 一样支持短 id：
            // 只在这里要求 40 位全 id，会让人拿到一句「没有这个任务」——
            // 明明 list 里刚看到它（那行显示的是前 16 位）。
            let full = find_task(&store, &id)?.id;
            let token = match as_who.as_deref() {
                Some(n) if !n.trim().is_empty() => Some(token_for_agent(n)?),
                _ => None,
            };
            let v = api_json_as("DELETE", &format!("/jobs/{full}"), None, token.as_deref())?;
            if json {
                println!("{}", serde_json::to_string(&v)?);
            } else {
                println!(
                    "已删除台账记录 {}（盘上文件不动）",
                    &full[..12.min(full.len())]
                );
            }
            Ok(())
        }
        Cmd::Doctor { json } => doctor(json),
        Cmd::Agents { json } => print_agents(json),
        Cmd::Pins => {
            let v = api_json("GET", "/pins", None)?;
            if json {
                println!("{}", serde_json::to_string(&v)?);
                return Ok(());
            }
            let empty = Vec::new();
            let pins = v.as_array().unwrap_or(&empty);
            if pins.is_empty() {
                println!("没有 pin。");
                return Ok(());
            }
            let now = now_ms();
            println!("{:<14} {:<16} {:<10} {}", "sha256", "agent", "剩余", "说明");
            for p in pins {
                let sha = p["sha256"].as_str().unwrap_or("");
                let agent = p["agent"].as_str().unwrap_or("");
                let exp = p["expires_at"].as_i64().unwrap_or(0);
                let left = if exp == 0 {
                    "不过期".to_string()
                } else if exp <= now {
                    "已过期".to_string()
                } else {
                    let secs = (exp - now) / 1000;
                    if secs >= 86_400 {
                        format!("{}d", secs / 86_400)
                    } else if secs >= 3_600 {
                        format!("{}h", secs / 3_600)
                    } else {
                        format!("{}m", (secs / 60).max(1))
                    }
                };
                println!(
                    "{:<14} {:<16} {:<10} {}",
                    &sha[..14.min(sha.len())],
                    display_safe(agent),
                    left,
                    display_safe(p["note"].as_str().unwrap_or(""))
                );
            }
            Ok(())
        }
        Cmd::Upload {
            path,
            url,
            protocol,
            sha256,
            credential_ref,
            as_who,
            priority,
        } => {
            let proto = protocol.trim().to_ascii_lowercase();
            if proto != "put" && proto != "tus" {
                anyhow::bail!("上传协议只支持 put / tus");
            }
            if let Some(sha) = sha256.as_deref() {
                if !skyfetch::security::is_sha256_hex(sha.trim()) {
                    anyhow::bail!("--sha256 必须是 64 位十六进制");
                }
            }
            let mut body = serde_json::json!({
                "path": path,
                "url": url,
                "protocol": proto,
                "source": source_of(&as_who),
                "priority": priority,
            });
            if let Some(sha) = sha256 {
                body["sha256"] = serde_json::json!(sha.trim().to_ascii_lowercase());
            }
            if let Some(c) = credential_ref {
                body["credential_ref"] = serde_json::json!(c);
            }
            let v = api_json("POST", "/uploads", Some(&body))?;
            let id = v
                .get("id")
                .and_then(|x| x.as_str())
                .ok_or_else(|| anyhow::anyhow!("守护进程没给 id：{v}"))?
                .to_string();
            let state = v.get("state").and_then(|x| x.as_str()).unwrap_or("");
            if matches!(state, "done" | "failed" | "interrupted") {
                let view = fetch_job_view(&api_client()?, &api_base(), &id)?;
                return match finish_daemon_view(json, with_elapsed(view)) {
                    Ok(_) => Ok(()),
                    Err(e) => Err(e),
                };
            }
            let view = watch_job_with(json, &api_client()?, &api_base(), &id, 0, 25_000)?;
            finish_daemon_view(json, with_elapsed(view)).map(|_| ())
        }
        Cmd::Watch { id, since, wait_ms } => {
            let base = api_base();
            let client = api_client()?;
            let view = watch_job_with(json, &client, &base, &id, since, wait_ms)?;
            emit(&view, json);
            Ok(())
        }
        Cmd::Stats => {
            let st = skyfetch::stats::collect(&store)?;
            if json {
                println!("{}", serde_json::to_string(&st)?);
            } else {
                let span = if st.first_ms > 0 && st.last_ms > st.first_ms {
                    let days = (st.last_ms - st.first_ms) as f64 / 86_400_000.0;
                    format!("跨度 {:.1} 天", days)
                } else {
                    String::new()
                };
                println!("台账 {} 条{}", st.tasks, if span.is_empty() { String::new() } else { format!(" · {span}") });
                let mut states: Vec<String> = st
                    .by_state
                    .iter()
                    .map(|(k, v)| format!("{} {}", skyfetch::stats::state_label(k), v))
                    .collect();
                states.sort();
                if !states.is_empty() {
                    println!("  状态    {}", states.join(" · "));
                }
                println!(
                    "  下过    {} · 其中秒传 {} 次（{}，占 {:.0}%）",
                    human(st.done_bytes),
                    st.cached_runs,
                    human(st.cached_bytes),
                    st.saved_ratio() * 100.0
                );
                println!(
                    "  缓存    {} 块 · {}（清缓存：skyget prune）",
                    st.blobs,
                    human(st.blob_bytes)
                );
                if !st.by_source.is_empty() {
                    let mut src: Vec<String> = st
                        .by_source
                        .iter()
                        .map(|(k, v)| format!("{} {}", source_word(k), v))
                        .collect();
                    src.sort();
                    println!("  来源    {}", src.join(" · "));
                }
                if !st.failures.is_empty() {
                    let mut f: Vec<String> = st
                        .failures
                        .iter()
                        .map(|(k, v)| format!("{k} {v}"))
                        .collect();
                    f.sort();
                    println!("  失败    {}", f.join(" · "));
                }
            }
            Ok(())
        }
        Cmd::Selftest => {
            let (ok, checks) = skyfetch::selftest::run()?;
            let passed = checks.iter().filter(|c| c.ok).count();
            if json {
                let rows: Vec<_> = checks
                    .iter()
                    .map(|c| serde_json::json!({"check": c.name, "ok": c.ok, "detail": c.detail}))
                    .collect();
                println!(
                    "{}",
                    serde_json::json!({"passed": passed, "total": checks.len(), "checks": rows})
                );
            } else {
                println!("天网盒子自带体检（不联网，全部在本机跑）\n");
                println!("  {:<28} {:<4} {}", "断言", "结果", "说明");
                println!("  {}", "─".repeat(66));
                for c in &checks {
                    println!(
                        "  {:<28} {:<4} {}",
                        pad_display(&c.name, 28),
                        if c.ok { "✓" } else { "✗" },
                        c.detail
                    );
                }
                println!("\n  {passed} / {} 通过", checks.len());
            }
            if !ok {
                anyhow::bail!("自带体检有问题");
            }
            Ok(())
        }
        Cmd::Dest { path, reset } => {
            if reset {
                let mut cfg = skyfetch::settings::load();
                cfg.dest_dir.clear();
                skyfetch::settings::save(&cfg)?;
            } else if let Some(p) = path {
                let made = skyfetch::settings::set_dest(&p)?;
                eprintln!("以后都下到：{}", made.display());
            }
            // 「现在到底是哪」要说清它是怎么定下来的 —— 不然设了不生效时无从下手。
            let now = skyfetch::settings::dest_dir();
            let why = if std::env::var_os("SKYGET_DEST").is_some() {
                "来自环境变量 SKYGET_DEST（它的优先级最高，会盖住设置）"
            } else if !skyfetch::settings::load().dest_dir.trim().is_empty() {
                "来自设置（skyget dest <路径>）"
            } else {
                "默认值（还没设置过）"
            };
            if json {
                println!(
                    "{}",
                    serde_json::json!({"dest_dir": now.display().to_string(), "why": why})
                );
            } else {
                println!("下载保存到：{}", now.display());
                println!("（{why}）");
            }
            return Ok(());
        }
        Cmd::Prune {
            older_than,
            max_bytes,
            yes,
        } => {
            let age = skyfetch::prune::parse_age(&older_than)?;
            let max = match max_bytes.as_deref() {
                Some(raw) => Some(
                    skyfetch::quota::parse_size(raw)
                        .ok_or_else(|| anyhow::anyhow!("--max-bytes 看不懂：{raw}"))?,
                ),
                None => None,
            };
            let mut plan = skyfetch::prune::plan(&store, age)?;
            if let Some(max) = max {
                plan = skyfetch::prune::cap_to(plan, max);
            }
            let days = age.as_secs() / 86_400;
            if plan.is_empty() {
                if json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "removed": 0, "bytes": 0,
                            "total_bytes": plan.total_bytes, "max_bytes": max,
                        })
                    );
                } else if let Some(max) = max {
                    if plan.total_bytes > max {
                        println!(
                            "缓存 {} 超过上限 {}，但没有可清的：{} 天内的、还有落地文件链着的、台账里还在跑的、被 pin 的，都不动。",
                            human(plan.total_bytes),
                            human(max),
                            days
                        );
                    } else {
                        println!(
                            "缓存 {} 已在上限 {} 以内，不用清。",
                            human(plan.total_bytes),
                            human(max)
                        );
                    }
                } else {
                    println!(
                        "没有可清的：{} 天内的、还有落地文件链着的、台账里还在跑的，都不动。",
                        days
                    );
                }
                return Ok(());
            }
            let after = plan.total_bytes.saturating_sub(plan.blob_bytes());
            if yes {
                let (n, bytes) = skyfetch::prune::apply(&plan)?;
                if json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "removed": n, "bytes": bytes, "dry_run": false,
                            "total_bytes": plan.total_bytes, "after_bytes": after,
                            "max_bytes": max,
                        })
                    );
                } else {
                    println!("清掉  {} 项 · {}", n, human(bytes));
                    if let Some(max) = max {
                        println!("缓存  {} / 上限 {}，清理后 {}", human(plan.total_bytes), human(max), human(after));
                        if after > max {
                            println!("      仍有 {} 受保护/太新，清不到上限", human(after));
                        }
                    }
                }
            } else if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "dry_run": true,
                        "blobs": plan.blobs.len(),
                        "blob_bytes": plan.blob_bytes(),
                        "parts": plan.parts.len(),
                        "part_bytes": plan.part_bytes(),
                        "total_bytes": plan.total_bytes,
                        "after_bytes": after,
                        "max_bytes": max,
                        "kept_linked": plan.kept_linked,
                        "kept_active": plan.kept_active,
                        "kept_young": plan.kept_young,
                    })
                );
            } else {
                if let Some(max) = max {
                    println!("缓存  {} / 上限 {}，清理后 {}", human(plan.total_bytes), human(max), human(after));
                    if after > max {
                        println!("      仍有 {} 受保护/太新，清不到上限", human(after));
                    }
                }
                println!("可清  {} 块内容 · {}", plan.blobs.len(), human(plan.blob_bytes()));
                if !plan.parts.is_empty() {
                    println!(
                        "      {} 个没跑完的分片 · {}",
                        plan.parts.len(),
                        human(plan.part_bytes())
                    );
                }
                if plan.kept_linked > 0 {
                    println!(
                        "      还有落地文件链着的 {} 块：不删（删了会丢秒传）",
                        plan.kept_linked
                    );
                }
                if plan.kept_active > 0 {
                    println!("      台账里还在跑的 {} 块：不删", plan.kept_active);
                }
                if plan.kept_young > 0 {
                    println!("      {} 天内的 {} 块：不删", days, plan.kept_young);
                }
                println!("\n真删加 --yes（不加就只看着）。");
            }
            Ok(())
        }
        Cmd::List {
            limit,
            mine,
            agents,
            source,
        } => {
            // 过滤和计数都在 SQL 里：先取再筛会让人以为台账里只有几条。
            let (tasks, total) = store.list_page(limit, source.as_deref(), mine, agents)?;
            let shown = tasks.len();
            if json {
                let views: Vec<_> = tasks.iter().map(|t| t.view(t.cached, 0)).collect();
                println!("{}", serde_json::to_string(&views)?);
                // 数组形状不能变（契约），截断这件事走 stderr：
                // agent 用 -l 就能拿全，但得先知道「不全」。
                if (total as usize) > shown {
                    eprintln!(
                        "只给了 {shown} 条（台账里共 {total} 条）；加 `-l {total}` 拿全部"
                    );
                }
            } else {
                if tasks.is_empty() {
                    println!("台账是空的。skyget <url> 下第一个。");
                } else {
                    println!(
                        "{} {} {:>9}  {}",
                        pad_display("ID", 16),
                        pad_display("状态", 8),
                        pad_left_display("大小", 9),
                        "落地"
                    );
                }
                for t in &tasks {
                    let size = if t.bytes_done > 0 {
                        human(t.bytes_done.max(t.bytes_total))
                    } else {
                        "—".to_string()
                    };
                    let state = t.state_for_display();
                    let label = if t.dest.is_empty() {
                        skyfetch::security::redact_url(&t.url)
                    } else {
                        t.dest.clone()
                    };
                    println!(
                        "{} {} {:>9}  {}",
                        pad_display(&t.id, 16),
                        pad_display(state_label(&state), 8),
                        size,
                        display_safe(&label)
                    );
                }
                if (total as usize) > shown {
                    println!();
                    println!(
                        "还有 {} 条没显示（共 {total} 条）：`skyget list -l {total}` 看全部",
                        total as usize - shown
                    );
                }
            }
            Ok(())
        }
        Cmd::Check {
            url,
            dest,
            name,
            sha256,
            mirrors,
            path,
            credential_ref,
        } => {
            let mut body = serde_json::json!({"url": url});
            if let Some(d) = dest { body["dest"] = serde_json::json!(d); }
            if let Some(n) = name { body["name"] = serde_json::json!(n); }
            if let Some(s) = sha256 { body["sha256"] = serde_json::json!(s); }
            if !mirrors.is_empty() { body["sources"] = serde_json::json!(mirrors); }
            if let Some(p) = path { body["path"] = serde_json::json!(p); }
            if let Some(c) = credential_ref { body["credential_ref"] = serde_json::json!(c); }
            // 预检会返回 403/400，那正是要看的结果 —— 不能像别的命令那样当失败抛掉。
            let (code, v) = api_json_status("POST", "/check", Some(&body))?;
            if json {
                println!("{}", serde_json::to_string(&v)?);
            } else {
                let checks = v["checks"].as_array().cloned().unwrap_or_default();
                for c in &checks {
                    let ok = c["ok"].as_bool().unwrap_or(false);
                    let name = c["name"].as_str().unwrap_or("?");
                    let detail = c["detail"].as_str().unwrap_or("");
                    println!(
                        "{} {:<11} {}",
                        if ok { "✓" } else { "✗" },
                        name,
                        display_safe(detail)
                    );
                }
                if v["ok"].as_bool().unwrap_or(false) {
                    println!();
                    println!("结论：会接受（落地 {}）", display_safe(v["dest"].as_str().unwrap_or("")));
                } else {
                    println!();
                    println!(
                        "结论：会被拒绝 —— {}（{}）",
                        display_safe(v["error"].as_str().unwrap_or("")),
                        v["code"].as_str().unwrap_or("")
                    );
                    let next = v["next_action"].as_str().unwrap_or("");
                    if !next.is_empty() {
                        println!("下一步：{}", display_safe(next));
                    }
                }
            }
            // 结论已经打出去了（--json 时那一行就是结果）：告诉 main 别再补一行。
            mark_reported();
            if code >= 400 {
                anyhow::bail!("预检不通过（HTTP {code}）");
            }
            Ok(())
        }
        Cmd::Verify { deep, repair } => {
            let rep = skyfetch::verify::run(&store, deep, repair)?;
            if json {
                println!("{}", serde_json::to_string(&rep)?);
            } else {
                if rep.repaired > 0 {
                    println!("已废弃 {} 张作废的分片位图", rep.repaired);
                }
                if rep.problems.is_empty() && rep.orphans.is_empty() {
                    let trust: Vec<String> = rep
                        .trust
                        .iter()
                        .map(|(k, v)| format!("{k} {v}"))
                        .collect();
                    // 「N 条记录」以前只数完成的那批：台账 58 条、这里报 28 条，
                    // 两个数字对不上，读到的人只会以为台账坏了。把总数一并写出来。
                    let total = store
                        .list_page(1, None, false, false)
                        .map(|(_, n)| n)
                        .unwrap_or(rep.tasks as i64);
                    println!(
                        "自洽：{} 条完成记录（台账共 {total} 条）、{} 个缓存块，{}",
                        rep.tasks,
                        rep.blobs,
                        if rep.deep { "hash 已重算" } else { "只查结构（加 --deep 重算 hash）" }
                    );
                    if !trust.is_empty() {
                        println!("校验强度：{}", trust.join(" / "));
                    }
                } else {
                    for p in &rep.problems {
                        println!(
                            "{}  {}  {}\n      {}",
                            if p.kind.fatal() { "✗" } else { "·" },
                            p.kind.label(),
                            display_safe(&p.path),
                            display_safe(&p.detail)
                        );
                    }
                    for o in &rep.orphans {
                        println!("· 无主缓存块  {o}（没有台账记录引用它，不会自动删）");
                    }
                    // 「N 条记录」只数完成的那批：台账 58 条、这里报 28 条，
                    // 两个数字对不上，读到的人只会以为台账坏了。把总数一并写出来。
                    let total = store
                        .list_page(1, None, false, false)
                        .map(|(_, n)| n)
                        .unwrap_or(rep.tasks as i64);
                    println!(
                        "\n{} 条完成记录（台账共 {total} 条）、{} 个缓存块；问题 {} 个（其中致命 {} 个）",
                        rep.tasks,
                        rep.blobs,
                        rep.problems.len(),
                        rep.fatal_count()
                    );
                }
            }
            if !rep.clean() {
                return Err(anyhow::anyhow!("自检发现致命问题"));
            }
            Ok(())
        }
        Cmd::Serve { port } => skyfetch::api::serve(port),
        Cmd::Install {
            src,
            key,
            require_signature,
        } => {
            let engine = Engine::new(5);
            let manifest = skyfetch::manifest::load(&engine, &src)?;
            check_manifest_trust(&manifest, &key, require_signature)?;
            let flag = std::sync::atomic::AtomicBool::new(false);
            let cb = make_cb(json, &flag);
            let done = match skyfetch::manifest::install(&store, &engine, &manifest, &cb) {
                Ok(d) => d,
                // 失败不在命令里打 JSON：统一由 main 打一行（见 main）。
                // 两处都打就会出现两行 stdout，而契约是「只有一行结果」。
                Err(e) => return Err(e),
            };
            if json {
                println!("{}", serde_json::to_string(&done)?);
            } else {
                println!();
                for f in &done.files {
                    println!("装好  {}", display_safe(&f.path));
                    println!("sha256  {}", f.sha256);
                }
                if !done.bin.is_empty() {
                    println!("入口  {}", display_safe(&done.bin));
                }
                if !done.usage.is_empty() {
                    println!("用法  {}", display_safe(&done.usage));
                }
            }
            Ok(())
        }
        Cmd::Ensure {
            src,
            key,
            require_signature,
        } => {
            let engine = Engine::new(5);
            let manifest = skyfetch::manifest::load(&engine, &src)?;
            check_manifest_trust(&manifest, &key, require_signature)?;
            let flag = std::sync::atomic::AtomicBool::new(false);
            let cb = make_cb(json, &flag);
            let done = match skyfetch::manifest::ensure(&store, &engine, &manifest, &cb) {
                Ok(d) => d,
                Err(e) => return Err(e),
            };
            if json {
                println!("{}", serde_json::to_string(&done)?);
            } else {
                println!();
                for f in &done.changed {
                    println!("装好  {}", display_safe(&f.path));
                }
                for f in &done.unchanged {
                    println!("已就绪  {}", display_safe(&f.path));
                }
                if done.changed.is_empty() {
                    println!("没有需要变的：{} 个文件本来就是对的内容。", done.unchanged.len());
                }
                if !done.bin.is_empty() {
                    println!("入口  {}", display_safe(&done.bin));
                }
            }
            Ok(())
        }
        Cmd::ManifestDigest { src } => {
            let engine = Engine::new(5);
            let manifest = skyfetch::manifest::load(&engine, &src)?;
            let digest = skyfetch::manifest::digest_hex(&manifest);
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "name": manifest.name,
                        "version": manifest.version,
                        "digest": digest,
                        "signed": manifest.signature.is_some(),
                    })
                );
            } else {
                println!("{digest}");
                match &manifest.signature {
                    Some(sig) => eprintln!(
                        "（带签名，key_id: {}；用 `skyget install --key …` 验它）",
                        if sig.key_id.is_empty() { "未命名" } else { &sig.key_id }
                    ),
                    None => eprintln!("（没有签名；摘要给外部签名工具用）"),
                }
            }
            Ok(())
        }
        Cmd::Keygen { out } => {
            let (priv_hex, pub_hex) = skyfetch::manifest::generate_key()?;
            match out {
                Some(path) => {
                    let path = skyfetch::settings::expand(&path);
                    if let Some(parent) = std::path::Path::new(&path).parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    write_private(Path::new(&path), &priv_hex)?;
                    println!("私钥  {path}");
                    println!("公钥  {pub_hex}");
                    println!("（公钥发给使用方：skyget install --key {pub_hex} <清单>）");
                }
                None => {
                    println!("{priv_hex}");
                    eprintln!("公钥  {pub_hex}");
                    eprintln!("（私钥只打印这一次：把它写进文件并 chmod 600）");
                }
            }
            Ok(())
        }
        Cmd::Sign {
            src,
            key,
            key_id,
            in_place,
        } => {
            let engine = Engine::new(5);
            let mut manifest = skyfetch::manifest::load(&engine, &src)?;
            let priv_hex = if let Some(p) = key.trim().strip_prefix('@') {
                let text = std::fs::read_to_string(skyfetch::settings::expand(p))
                    .map_err(|e| anyhow::anyhow!("读不到私钥文件 {p}：{e}"))?;
                text.trim().to_string()
            } else {
                key.trim().to_string()
            };
            // 签名前把旧的签名摘掉：签名覆盖的是内容，不是上一份签名。
            manifest.signature = None;
            let sig = skyfetch::manifest::sign_manifest(&manifest, &priv_hex, &key_id)?;
            manifest.signature = Some(skyfetch::manifest::ManifestSignature {
                key_id: key_id.clone(),
                sig,
            });
            let block = serde_json::json!({
                "key_id": key_id,
                "sig": manifest.signature.as_ref().map(|s| s.sig.clone()).unwrap_or_default(),
            });
            if in_place {
                let path = skyfetch::settings::expand(&src);
                let text = serde_json::to_string_pretty(&manifest)?;
                std::fs::write(&path, format!("{text}\n"))
                    .map_err(|e| anyhow::anyhow!("写回清单失败（{path}）：{e}"))?;
                println!("已签名  {path}");
                println!("digest  {}", skyfetch::manifest::digest_hex(&manifest));
            } else if json {
                println!("{}", serde_json::to_string(&block)?);
            } else {
                println!("把这一段加进清单的顶层：");
                println!("{}", serde_json::to_string_pretty(&block)?);
                println!("digest  {}", skyfetch::manifest::digest_hex(&manifest));
            }
            Ok(())
        }
        Cmd::Daemon { action } => {
            let base = api_base();
            match action {
                DaemonAction::Ensure => {
                    let e = skyfetch::daemon::ensure(&base);
                    if json {
                        let v = match &e {
                            skyfetch::daemon::Ensured::Already => serde_json::json!({"state": "already", "base": base}),
                            skyfetch::daemon::Ensured::BySystemd(u) => serde_json::json!({"state": "started", "by": "systemd", "unit": u, "base": base}),
                            skyfetch::daemon::Ensured::Spawned(p) => serde_json::json!({"state": "started", "by": "spawn", "pid": p, "base": base}),
                            skyfetch::daemon::Ensured::Failed(why) => serde_json::json!({"state": "failed", "error": why, "base": base}),
                        };
                        println!("{}", serde_json::to_string(&v)?);
                    } else {
                        match &e {
                            skyfetch::daemon::Ensured::Already => println!("已经在跑了（{base}）"),
                            skyfetch::daemon::Ensured::BySystemd(u) => println!("已通过 systemd 用户服务拉起（{u}）"),
                            skyfetch::daemon::Ensured::Spawned(p) => println!("已直接在后台拉起（pid {p}）"),
                            skyfetch::daemon::Ensured::Failed(why) => println!("起不来：{why}"),
                        }
                    }
                    if matches!(e, skyfetch::daemon::Ensured::Failed(_)) {
                        anyhow::bail!("守护进程起不来");
                    }
                    Ok(())
                }
                DaemonAction::Status => {
                    let health = reqwest::blocking::Client::builder()
                        .timeout(Duration::from_millis(1500))
                        .build()?
                        .get(format!("{}/health", base.trim_end_matches('/')))
                        .send()
                        .ok()
                        .and_then(|r| r.json::<serde_json::Value>().ok());
                    let unit = ["skynet-dataplaned", "skynet-downloadd"].iter().find(|u| {
                        std::process::Command::new("systemctl")
                            .args(["--user", "is-active", u])
                            .output()
                            .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "active")
                            .unwrap_or(false)
                    });
                    let st = skyfetch::daemon::status(&base);
                    if json {
                        println!(
                            "{}",
                            serde_json::json!({
                                "base": base,
                                "healthy": health.is_some(),
                                "health": health,
                                "unit": unit,
                                "pid_file": st.pid_file.display().to_string(),
                                "pid": st.pid,
                                "pid_running": st.pid_running,
                                "socket": st.socket.display().to_string(),
                                "socket_exists": st.socket_exists,
                                "socket_live": st.socket_live,
                                "socket_stale": st.socket_exists && !st.socket_live,
                            })
                        );
                    } else {
                        println!("地址     {base}");
                        match &health {
                            Some(h) => println!("状态     在跑（{}）", h["version"].as_str().unwrap_or("?")),
                            None => println!("状态     没在跑（或连不上）"),
                        }
                        match unit {
                            Some(u) => println!("托管     systemd --user · {u}"),
                            None => println!("托管     没有 systemd 服务在跑（可能是直接启动的）"),
                        }
                        match (st.pid, st.pid_running) {
                            (Some(pid), true) => println!("pid      {pid}（直接进程）"),
                            (Some(pid), false) => {
                                println!("pid      {pid}（已失效，stop 会清理）")
                            }
                            (None, _) => println!("pid      没有 pid 文件"),
                        }
                        println!(
                            "socket   {}（{}）",
                            st.socket.display(),
                            match (st.socket_exists, st.socket_live) {
                                (true, true) => "可用",
                                (true, false) => "残留，连不上",
                                (false, _) => "不在",
                            }
                        );
                    }
                    Ok(())
                }
                DaemonAction::Stop => {
                    let before = skyfetch::daemon::status(&base);
                    let mut by_systemd = false;
                    for u in ["skynet-dataplaned", "skynet-downloadd"] {
                        let ok = std::process::Command::new("systemctl")
                            .args(["--user", "stop", u])
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .status()
                            .map(|s| s.success())
                            .unwrap_or(false);
                        if ok {
                            by_systemd = true;
                            break;
                        }
                    }
                    let direct_pid = skyfetch::daemon::stop(&base)?;
                    let after = skyfetch::daemon::status(&base);
                    let cleaned = (before.pid_running && !after.pid_running)
                        || (before.socket_exists && !after.socket_exists)
                        || before
                            .pid
                            .map(|_| !before.pid_running)
                            .unwrap_or(false);
                    let stopped = by_systemd || direct_pid.is_some();
                    if json {
                        println!(
                            "{}",
                            serde_json::json!({
                                "stopped": stopped,
                                "by": if direct_pid.is_some() {
                                    "pid"
                                } else if by_systemd {
                                    "systemd"
                                } else {
                                    ""
                                },
                                "pid": direct_pid,
                                "cleaned": cleaned,
                            })
                        );
                    } else if direct_pid.is_some() {
                        println!("已停止（直接进程 pid {}）", direct_pid.unwrap());
                    } else if by_systemd {
                        println!("已停止（systemd 服务）");
                    } else if cleaned {
                        println!("没有在跑的守护进程（残留 pid/socket 已清理）");
                    } else {
                        println!("没有在跑的守护进程");
                    }
                    Ok(())
                }
            }
        }
        Cmd::Completions { shell } => {
            use clap::CommandFactory;
            let mut cmd = Cli::command();
            let name = cmd.get_name().to_string();
            clap_complete::generate(shell, &mut cmd, name, &mut std::io::stdout());
            Ok(())
        }
        Cmd::Catalog { sub } => match sub {
            CatalogCmd::List { query, limit } => {
                let list = skyfetch::manifest::catalog_list(query.as_deref(), limit)?;
                if json {
                    println!("{}", serde_json::to_string(&list)?);
                } else if list.is_empty() {
                    println!("机库里没有匹配的条目。");
                } else {
                    for e in &list {
                        println!("{:<22} {:<10} {}", e.id, e.category, e.summary);
                    }
                }
                Ok(())
            }
            CatalogCmd::Show { id } => {
                let list = skyfetch::manifest::catalog_list(Some(&id), 200)?;
                let e = list
                    .into_iter()
                    .find(|e| e.id == id)
                    .ok_or_else(|| anyhow::anyhow!("机库里没有这个条目：{id}"))?;
                if json {
                    println!("{}", serde_json::to_string(&e)?);
                } else {
                    println!("{}  （{}）", e.name, e.category);
                    println!("{}", e.summary);
                    if !e.repo.is_empty() {
                        println!("仓库  {}", e.repo);
                    }
                    if !e.homepage.is_empty() {
                        println!("主页  {}", e.homepage);
                    }
                    for (k, v) in &e.install {
                        println!("{k:<7} {v}");
                    }
                    if !e.install.contains_key("url") {
                        println!("说明  这条没有直链下载地址，得用它自己的安装方式");
                    }
                }
                Ok(())
            }
        },
        Cmd::Resolve { url, verify } => {
            // 只看台账里「下完过、有 sha」的记录：给出去的是缓存地址，不是落地文件，
            // 所以 dest 还在不在不重要。
            let t = store.last_done_for_url(&url)?.ok_or_else(|| {
                anyhow::anyhow!("台账里没有这个地址的完成记录：{url}")
            })?;
            if t.sha256.len() != 64 {
                anyhow::bail!("这条记录的 sha256 不完整（{} 位），拼不出缓存地址", t.sha256.len());
            }
            let blob = skyfetch::paths::blobs_dir()
                .join(&t.sha256[..2])
                .join(&t.sha256);
            if !blob.is_file() {
                anyhow::bail!(
                    "缓存里没有这份内容（可能被 prune 了）：{}；重新下一次即可",
                    t.sha256
                );
            }
            // /cas 是 daemon 提供的；daemon 没起，给出去的地址就是死链 —— 宁可报错。
            let base = api_base();
            let alive = reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(3))
                .build()
                .context("构造探测客户端失败")?
                .get(format!("{}/health", base.trim_end_matches('/')))
                .send()
                .map(|r| r.status().is_success())
                .unwrap_or(false);
            if !alive {
                anyhow::bail!(
                    "daemon 没起（{}/health 不通），/cas 地址不可用：先跑 `skyget serve`",
                    base.trim_end_matches('/')
                );
            }
            let mut cas = format!("{}/cas/{}", base.trim_end_matches('/'), t.sha256);
            if verify {
                cas.push_str("?verify=1");
            }
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "url": cas,
                        "sha256": t.sha256,
                        "bytes": t.bytes_total,
                        "source": t.source,
                        "id": t.id,
                    })
                );
            } else {
                println!("{cas}");
            }
            Ok(())
        }
        Cmd::Curl { args } => {
            // 只接管「一个地址 + 明确落盘」这一种形态。判定刻意保守：
            // 不认识的参数一律透传 —— 猜错一次就是坏掉别人的脚本。
            let mut url: Option<String> = None;
            let mut out: Option<String> = None;
            let mut as_agent: Option<String> = None;
            let mut auth: Option<String> = None;
            let mut silent = false;
            let mut pass = false;

            let mut it = args.iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "-L" | "--location" | "-S" | "--show-error" | "-f" | "--fail" | "-#"
                    | "--progress-bar" => {}
                    "-C" | "--continue-at" => match it.next().map(String::as_str) {
                        // 引擎永远续传，不需要这个开关；别的取值不接管
                        Some("-") => {}
                        _ => {
                            pass = true;
                            break;
                        }
                    },
                    "--retry" => {
                        it.next();
                    }
                    "-s" | "--silent" => silent = true,
                    "-H" | "--header" => match it.next().map(|v| bearer_of(v)) {
                        // 只认 `Authorization: Bearer …`，而且只能有一个。
                        // 其它 header 一律交给真 curl：那不是凭据，接管没好处。
                        Some(Some(tok)) if auth.is_none() => auth = Some(tok),
                        _ => {
                            pass = true;
                            break;
                        }
                    },
                    "-o" | "--output" => match it.next() {
                        Some(v) => out = Some(v.clone()),
                        None => {
                            pass = true;
                            break;
                        }
                    },
                    "--as" => match it.next() {
                        Some(v) => as_agent = Some(v.clone()),
                        None => {
                            pass = true;
                            break;
                        }
                    },
                    _ if a.starts_with("--output=") => {
                        out = Some(a["--output=".len()..].to_string())
                    }
                    _ if a.starts_with('-') => {
                        pass = true;
                        break;
                    }
                    _ => {
                        if url.is_some() {
                            // 多个地址不接管
                            pass = true;
                            break;
                        }
                        url = Some(a.clone());
                    }
                }
            }

            if pass || url.is_none() || out.is_none() || out.as_deref() == Some("-") {
                // `-o -` 在 curl 里是「写 stdout」，不是文件名叫 `-`；这种也不接管。
                // 原样交给真 curl：stdio 直连，退出码原样透出
                let status = std::process::Command::new("curl")
                    .args(&args)
                    .status()
                    .map_err(|e| anyhow::anyhow!("找不到真实的 curl：{e}"))?;
                std::process::exit(status.code().unwrap_or(1));
            }
            let (url, out) = (url.unwrap(), out.unwrap());

            // 不重写下载逻辑：翻译成 get 的入参再走同一条路。
            //
            // 注意 `-o` 的语义差别：curl 的 `-o` 是**文件**，而 skyget 的 `-o` 是**目录**。
            // 早期直接把 `-o out.bin` 传下去，结果生成的是 `out.bin/` 目录再往里面放文件 ——
            // 对 `curl -o` 的脚本来说这是静默的行为改变（文件出现在了别的地方）。
            // 所以这里拆成「目录 + 文件名」两个参数。
            let mut child = std::process::Command::new(std::env::current_exe()?);
            let out_path = std::path::Path::new(&out);
            let (dir, name) = match (out_path.parent(), out_path.file_name()) {
                (Some(d), Some(n)) if !d.as_os_str().is_empty() => (
                    d.display().to_string(),
                    n.to_string_lossy().to_string(),
                ),
                // `-o out.bin`：当前目录 + 文件名
                (_, Some(n)) => (".".to_string(), n.to_string_lossy().to_string()),
                _ => {
                    // 理论上到不了这里（out 非空且不是 `-`）；保守地不接管。
                    anyhow::bail!("--output 的取值看不懂：{out}")
                }
            };
            child.args(["get", &url, "--json", "-o", &dir, "-n", &name]);
            if let Some(agent) = as_agent.as_deref() {
                child.args(["--as", agent]);
            }
            if let Some(tok) = auth.as_deref() {
                // 值只走子进程环境：不进命令行（ps 里看不见）、不进台账（只存引用）。
                const VAR: &str = "SKYGET_CURL_AUTH";
                child.env(VAR, tok);
                // credential::resolve 只认显式列入 SKYGET_DOWNLOAD_CREDENTIALS 的变量名。
                let mut allowed =
                    std::env::var("SKYGET_DOWNLOAD_CREDENTIALS").unwrap_or_default();
                if !allowed.split(',').any(|x| x.trim() == VAR) {
                    if !allowed.is_empty() {
                        allowed.push(',');
                    }
                    allowed.push_str(VAR);
                }
                child.env("SKYGET_DOWNLOAD_CREDENTIALS", allowed);
                let cred_ref = format!("env:{VAR}");
                child.args(["--credential-ref", cred_ref.as_str()]);
            }
            // 结果行是给程序的，替身模式下不吐；进度仍走 stderr（-s 才真的安静）
            child.stdout(std::process::Stdio::null());
            if silent {
                child.stderr(std::process::Stdio::null());
            }
            let status = child.status().context("启动 skyget get 失败")?;
            if !status.success() {
                std::process::exit(status.code().unwrap_or(1));
            }
            Ok(())
        }
        Cmd::Show { id, url } => {
            let t = match (id, url) {
                (Some(id), None) => find_task(&store, &id)?,
                (None, Some(u)) => store.latest_for_url(&u)?.ok_or_else(|| {
                    anyhow::anyhow!("台账里没有这个地址的记录：{u}（换个写法，或者 skyget list 看看）")
                })?,
                (Some(_), Some(_)) => {
                    anyhow::bail!("id 和 --url 只能给一个（id 查一条，--url 查这个地址最近一次）")
                }
                (None, None) => {
                    anyhow::bail!("要给一个任务 id，或者用 --url <地址> 查这个地址最近一次")
                }
            };
            if json {
                println!("{}", serde_json::to_string(&t.view(t.cached, 0))?);
            } else {
                println!("id       {}", t.id);
                println!("状态     {}", t.state_for_display());
                println!(
                    "地址     {}",
                    display_safe(&skyfetch::security::redact_url(&t.url))
                );
                println!("落地     {}", display_safe(&t.dest));
                println!("大小     {}", human(t.bytes_total));
                println!("sha256   {}", t.sha256);
                println!("来源     {}", display_safe(&t.source));
                if t.state != STATE_DONE {
                    println!(
                        "错误     {}",
                        display_safe(&skyfetch::security::redact_text(&t.error))
                    );
                }
            }
            Ok(())
        }
    }
}

/// 从校验文件里取出 hash：兼容 `hash` 单行、`hash  filename`、`SHA256 (x) = hash`。
fn fetch_hash(engine: &Engine, url: &str) -> Result<String> {
    let text = engine
        .client
        .get(url)
        .send()?
        .error_for_status()?
        .text()?;
    for token in text.split(|c: char| c.is_whitespace() || c == '=' || c == '(' || c == ')') {
        let t = token.trim();
        if t.len() == 64 && t.chars().all(|c| c.is_ascii_hexdigit()) {
            return Ok(t.to_ascii_lowercase());
        }
    }
    Err(anyhow::anyhow!("校验文件里没有 sha256（长度 64 的十六进制）"))
}

/// 人类输出里的 URL/路径/错误/事件文本都可能来自不可信来源；去掉控制字符，
/// 防止终端转义序列（OSC 52 写剪贴板、改标题等）被顺带执行。JSON 输出不经过这里。
fn display_safe(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// 终端里中文占两格：按显示宽度补空格，表格才不会歪。
fn pad_display(s: &str, width: usize) -> String {
    let s = display_safe(s);
    let used: usize = s.chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum();
    let mut out = s.to_string();
    for _ in used..width {
        out.push(' ');
    }
    out
}

/// 右对齐版（数值列的表头用）：按显示宽度从左边补空格。
fn pad_left_display(s: &str, width: usize) -> String {
    let s = display_safe(s);
    let used: usize = s.chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum();
    format!("{}{}", " ".repeat(width.saturating_sub(used)), s)
}

/// 台账里的状态说人话（和界面上同一套词）。
fn state_label(state: &str) -> &'static str {
    match state {
        STATE_DONE => "完成",
        "failed" => "失败",
        "running" => "下载中",
        "interrupted" => "中断",
        _ => "排队",
    }
}

/// 来源的短说法（跟界面里那套一致）
fn source_word(s: &str) -> String {
    match s {
        "human" | "" => "人".to_string(),
        "cli" => "命令行".to_string(),
        "api" => "接口".to_string(),
        "manifest" => "清单".to_string(),
        other => match other.strip_prefix("agent:") {
            Some("hangar") => "机库".to_string(),
            Some(x) => x.to_string(),
            None => other.to_string(),
        },
    }
}

fn trust_label(t: &str) -> &'static str {
    match t {
        "pinned" => "你给的 hash",
        "indexed" => "发布方的校验文件",
        "tofu-ok" => "与该地址上次一致",
        "tofu-new" => "首次记录（以后再变会被拒）",
        "torrent" => "种子自证",
        "local" => "本地文件（上传）",
        _ => "未校验",
    }
}

// ── 守护进程客户端（agent 路径） ───────────────────────────────────────
//
// CLI 自己跑引擎是给人用的兜底；agent 默认把任务交给 daemon：
// 进程死了下载不死、并发由全局调度器管、缓存和台账只有一份。
// 这里只用公开的 HTTP 契约，不 import 内核。

enum DaemonRun {
    Done,
    /// 连不上守护进程（不是它拒绝）—— 调用方可以决定回退到独立下载
    Unavailable(String),
}

/// 连不上时要不要顺手把守护进程拉起来。
///
/// 默认开；`SKYGET_AUTOSTART=0` 关掉（给"我只想连远端 daemon"或者排障时用）。
/// 只对回环地址这么干 —— `SKYGET_API` 指到别的机器时，那边的事归那边管。
fn autostart_allowed(base: &str) -> bool {
    let mode = std::env::var("SKYGET_AUTOSTART").unwrap_or_default();
    let mode = mode.trim().to_ascii_lowercase();
    if mode == "0" || mode == "false" || mode == "off" {
        return false;
    }
    let loopback = base.starts_with("http://127.0.0.1:") || base.starts_with("http://localhost:");
    if !loopback {
        return false;
    }
    if mode == "always" {
        return true;
    }
    // 只对**默认端点**（unit 服务的那一个）自动拉起。
    // 否则跑测试/连沙盒端口时，一次瞬断就会把生产 daemon 在 7788 上拉起来 ——
    // 既有副作用，也掩盖了"你连的那个 daemon 没起"这个真实问题。
    let port = base.rsplit(':').next().unwrap_or("").trim_end_matches('/');
    port == "7788" || mode == "always"
}

fn api_base() -> String {
    // `SKYGET_API=unix:/path` 时这里会返回一个进程内回环转发地址（见 skyfetch::uds）。
    // 其它情况下就是环境变量原值，和以前一样。
    skyfetch::uds::base_url()
}

fn api_client() -> Result<reqwest::blocking::Client> {
    api_client_as(None)
}

/// `token = Some(..)` 时用这一把（`--as` 用），否则用环境里的 SKYGET_API_TOKEN。
fn api_client_as(token: Option<&str>) -> Result<reqwest::blocking::Client> {
    let mut headers = reqwest::header::HeaderMap::new();
    let t = match token {
        Some(t) => t.trim().to_string(),
        None => std::env::var("SKYGET_API_TOKEN").unwrap_or_default(),
    };
    if !t.is_empty() {
        let v = reqwest::header::HeaderValue::from_str(&format!("Bearer {t}"))
            .context("token 里有非法字符")?;
        headers.insert(reqwest::header::AUTHORIZATION, v);
    }
    reqwest::blocking::Client::builder()
        // watch 最长长轮询 25 秒，留余量
        .timeout(Duration::from_secs(40))
        .default_headers(headers)
        .build()
        .context("构造接口客户端失败")
}

/// 调接口并把 (状态码, JSON) 一起带回来 —— 4xx **不**当异常。
/// 预检就是这样：403 是结论的一部分，不是命令失败了。
fn api_json_status(
    method: &str,
    path: &str,
    body: Option<&serde_json::Value>,
) -> Result<(u16, serde_json::Value)> {
    let base = api_base();
    let client = api_client()?;
    let url = format!("{}{}", base.trim_end_matches('/'), path);
    let mut rb = match method {
        "GET" => client.get(&url),
        "POST" => client.post(&url),
        other => anyhow::bail!("不支持的 HTTP 方法：{other}"),
    };
    if let Some(b) = body {
        rb = rb.json(b);
    }
    let r = match rb.send() {
        Ok(r) => r,
        // 随 agent 启动：agent 一调命令就把数据盒子带起来，不需要开机自启。
        // 只对默认回环地址做这件事（SKYGET_API 指向别处时，那边由别人负责）。
        Err(e) if autostart_allowed(&base) => {
            match skyfetch::daemon::ensure(&base) {
                skyfetch::daemon::Ensured::Failed(_) => {
                    return Err(e).with_context(|| {
                        format!("连不上守护进程（{base}），自动拉起也失败了")
                    })
                }
                _ => client.post(&url).json(&body.unwrap_or_default()).send()?,
            }
        }
        Err(e) => {
            return Err(e).with_context(|| {
                format!("连不上守护进程（{base}）；先启动 skynet-dataplaned（旧名 skynet-downloadd）")
            })
        }
    };
    let status = r.status().as_u16();
    let text = r.text().unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("返回的不是 JSON（HTTP {status}）：{text}"))?;
    Ok((status, v))
}

/// 对「可重试」的 409 自动等一下再试（只给控制类命令用）。
///
/// `pause` 之后马上 `resume` 是很自然的操作顺序，而 daemon 那边的暂停是**合作式**的：
/// 线程要在下一块数据之间才看到信号，这期间 resume 会拿到 `job.running`
/// （`retryable: true`，建议等 200ms）。CLI 的契约是「一次调用拿结果」，
/// 所以这里按 daemon 自己给的 `retry_after_ms` 等，而不是把 409 甩给用户。
fn api_json_retry(path: &str, tries: usize, token: Option<&str>) -> Result<serde_json::Value> {
    let mut last: Option<anyhow::Error> = None;
    for i in 0..tries.max(1) {
        match api_json_as("POST", path, Some(&serde_json::json!({})), token) {
            Ok(v) => return Ok(v),
            Err(e) => {
                let msg = format!("{e:#}");
                let retryable = msg.contains("\"retryable\":true");
                last = Some(e);
                if !retryable || i + 1 == tries {
                    break;
                }
                // 从错误文本里取 retry_after_ms（没有就 300ms）。
                let wait = msg
                    .split("retry_after_ms\":")
                    .nth(1)
                    .and_then(|rest| {
                        rest.trim_start()
                            .chars()
                            .take_while(|c| c.is_ascii_digit())
                            .collect::<String>()
                            .parse::<u64>()
                            .ok()
                    })
                    .unwrap_or(300)
                    .clamp(50, 2000);
                std::thread::sleep(Duration::from_millis(wait));
            }
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("接口调用失败")))
}

/// 调一个 daemon 接口并把 JSON 带回来。pin/unpin/pins 用。
fn api_json(method: &str, path: &str, body: Option<&serde_json::Value>) -> Result<serde_json::Value> {
    api_json_as(method, path, body, None)
}

fn api_json_as(
    method: &str,
    path: &str,
    body: Option<&serde_json::Value>,
    token: Option<&str>,
) -> Result<serde_json::Value> {
    let base = api_base();
    let client = api_client_as(token)?;
    let url = format!("{}{}", base.trim_end_matches('/'), path);
    let send = |c: &reqwest::blocking::Client| -> Result<reqwest::blocking::Response> {
        let mut rb = match method {
            "GET" => c.get(&url),
            "POST" => c.post(&url),
            "DELETE" => c.delete(&url),
            other => anyhow::bail!("不支持的 HTTP 方法：{other}"),
        };
        if let Some(b) = body {
            rb = rb.json(b);
        }
        Ok(rb.send()?)
    };
    let r = match send(&client) {
        Ok(r) => r,
        // 随 agent 启动：agent 一调命令就把数据盒子带起来，不需要开机自启。
        Err(e) if autostart_allowed(&base) => {
            match skyfetch::daemon::ensure(&base) {
                skyfetch::daemon::Ensured::Failed(_) => {
                    return Err(e).with_context(|| {
                        format!("连不上守护进程（{base}），自动拉起也失败了；检查 skynet-dataplaned")
                    })
                }
                _ => send(&client).with_context(|| format!("守护进程刚拉起，但 {base} 还是不通"))?,
            }
        }
        Err(e) => {
            return Err(e).with_context(|| {
                format!("连不上守护进程（{base}）；先启动 skynet-dataplaned（旧名 skynet-downloadd）")
            })
        }
    };
    let status = r.status();
    let text = r.text().unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("守护进程回了 HTTP {}：{}", status.as_u16(), text);
    }
    serde_json::from_str(&text).with_context(|| format!("返回的不是 JSON：{text}"))
}

/// `skyget agents`：终端版的机群视图。数据与 GUI/MCP 同一个 `/agents`。
fn print_agents(json: bool) -> Result<()> {
    let v = api_json("GET", "/agents", None)?;
    if json {
        println!("{}", serde_json::to_string(&v)?);
        return Ok(());
    }
    let s = &v["scheduler"];
    println!(
        "并发 {} / {} · 排队 {} / {}",
        s["running"].as_u64().unwrap_or(0),
        s["max_running"].as_u64().unwrap_or(0),
        s["queued"].as_u64().unwrap_or(0),
        s["max_queued"].as_u64().unwrap_or(0),
    );
    if !v["registry"].as_bool().unwrap_or(false) {
        println!("（未启用 per-agent token；来源仍是自称，策略不可信）");
    }
    let agents = v["agents"].as_array().cloned().unwrap_or_default();
    if agents.is_empty() {
        println!("还没有 agent 提交过任务。用 `skyget --as <名字>` 或 MCP 接进来。");
        return Ok(());
    }
    for a in agents {
        println!(
            "{:<12} {:<20} {:>4} 条任务  {}/{} 在跑",
            display_safe(a["name"].as_str().unwrap_or("")),
            display_safe(a["source"].as_str().unwrap_or("")),
            a["jobs"].as_u64().unwrap_or(0),
            a["running"].as_u64().unwrap_or(0),
            a["max_running"].as_u64().unwrap_or(0),
        );
        let p = &a["policy"];
        println!("  落地  {}", join_json(&p["dest_roots"], "（未限制）"));
        println!("  主机  {}", join_hosts(&p["allowed_hosts"]));
        println!("  上传  {}", join_json(&p["upload_roots"], "禁止"));
        let used = a["used_today"].as_u64().unwrap_or(0);
        match a["quota_bytes"].as_u64() {
            Some(limit) => println!("  配额  {} / {}（今天）", human(used), human(limit)),
            None => println!("  配额  {}（今天，未限额）", human(used)),
        }
    }
    Ok(())
}

fn join_json(v: &serde_json::Value, empty: &str) -> String {
    let s = v
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str())
                .collect::<Vec<_>>()
                .join(" · ")
        })
        .unwrap_or_default();
    if s.is_empty() {
        empty.to_string()
    } else {
        display_safe(&s)
    }
}

/// `allowed_hosts` 有三种语义：null=不限制，[]=拒绝全部，非空=白名单。
/// 以前把 [] 和 null 都显示成“不限制”，部署方用空数组收紧出网后会被误导。
fn join_hosts(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => "（不限制）".to_string(),
        serde_json::Value::Array(a) if a.is_empty() => "（拒绝全部主机）".to_string(),
        _ => join_json(v, "（拒绝全部主机）"),
    }
}

/// `skyget doctor`：把“部署完到底通没通”做成一条命令。
///
/// 不猜、不读 daemon 的环境变量：本地目录自己探，daemon 的信息从 `/health`
/// 和 `/agents` 取。agent 接入前也可以先跑它，看清楚自己的落地根/上传根。
fn doctor(json: bool) -> Result<()> {
    let home = skyfetch::paths::data_dir();
    let mut checks: Vec<serde_json::Value> = Vec::new();
    let mut add = |name: &str, ok: bool, detail: String| {
        checks.push(serde_json::json!({"name": name, "ok": ok, "detail": detail}));
    };

    add("version", true, skyfetch::version().to_string());
    // HLS/DASH 全靠外部 ffmpeg。没装的话「大片地址」100% 失败，而其它检查全绿 ——
    // 这种「八项全过但功能是坏的」最难查，所以单独报一项。
    let ff = std::env::var("SKYGET_FFMPEG").unwrap_or_else(|_| "ffmpeg".to_string());
    let ff_ok = std::process::Command::new(&ff)
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    add(
        "ffmpeg",
        ff_ok,
        if ff_ok {
            format!("{ff}（HLS/DASH 可用）")
        } else {
            format!("找不到 {ff}：m3u8/mpd 会失败；装一个，或用 SKYGET_FFMPEG 指到可执行文件")
        },
    );
    let created = std::fs::create_dir_all(&home).is_ok();
    let probe = home.join(format!(".doctor-{}", std::process::id()));
    let writable = created
        && std::fs::write(&probe, b"ok").is_ok()
        && std::fs::remove_file(&probe).is_ok();
    add(
        "home_writable",
        writable,
        format!("{}（{}）", home.display(), if writable { "可写" } else { "不可写" }),
    );
    for (name, p) in [
        ("blobs_dir", skyfetch::paths::blobs_dir()),
        ("parts_dir", skyfetch::paths::parts_dir()),
    ] {
        // 目录不存在不是错误：daemon 首次运行会建。信息性检查，不拉低结论。
        add(
            name,
            true,
            format!(
                "{}（{}）",
                p.display(),
                if p.is_dir() { "存在" } else { "未创建，首次运行会建" }
            ),
        );
    }

    // Unix socket：容器/沙箱接入时最常问「为什么连不上」，这里先答。
    // 信息性检查（不拉低结论）：老 daemon 没有 socket 是正常的。
    {
        let sock = skyfetch::uds::socket_path();
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(&sock)
                .ok()
                .map(|m| format!("{:o}", m.permissions().mode() & 0o777))
        };
        #[cfg(not(unix))]
        let mode: Option<String> = None;
        let mapped = std::env::var("SKYGET_UDS_AGENTS")
            .map(|v| v.split(',').filter(|x| !x.trim().is_empty()).count())
            .unwrap_or(0);
        let sock_live = skyfetch::uds::socket_live();
        let detail = if sock.exists() && sock_live {
            format!(
                "{}（权限 {}，uid 映射 {} 个，可用）",
                sock.display(),
                mode.clone().unwrap_or_else(|| "?".into()),
                mapped
            )
        } else if sock.exists() {
            format!(
                "{}（权限 {}，连不上：daemon 未运行留下的残留，下次 ensure 会清理）",
                sock.display(),
                mode.clone().unwrap_or_else(|| "?".into()),
            )
        } else {
            format!(
                "{} 不存在：daemon 0.24 起会自动建；没有它就用 TCP + token（或先重启 daemon）",
                sock.display()
            )
        };
        add("unix_socket", true, detail);
    }

    // 本机环境变量只是兜底；daemon 在线时以它 `/agents` 报的策略为准
    // （doctor 的 shell 不一定有 daemon 的 SKYGET_UPLOAD_ROOTS）。
    let policy = skyfetch::security::Policy::strict();
    let mut dest_roots: Vec<String> = policy
        .dest_roots
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    let mut upload_roots: Vec<String> = policy
        .upload_roots
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    let mut effective_policy: Option<serde_json::Value> = None;

    // daemon：3 秒内必须答话，否则就是没起/端口不对。
    let base = api_base();
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .context("构造 doctor 客户端失败")?;
    let token = std::env::var("SKYGET_API_TOKEN")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let get = |path: &str| {
        let mut rb = client.get(format!("{}{}", base.trim_end_matches('/'), path));
        if let Some(t) = &token {
            rb = rb.bearer_auth(t);
        }
        rb.send()
    };
    let mut daemon = serde_json::json!({"url": base, "reachable": false});
    let mut reachable = false;
    let mut identity = "无 token（本机模式）".to_string();
    match get("/health") {
        Ok(r) if r.status().is_success() => {
            match r.json::<serde_json::Value>() {
                Err(e) => add("daemon", false, format!("{base}/health 返回的不是 JSON：{e}")),
                Ok(v) if v["ok"] != true || !v["version"].is_string() => add(
                    "daemon",
                    false,
                    format!("{base}/health 不符合契约：{v}"),
                ),
                Ok(v) => {
                    reachable = true;
                    daemon["reachable"] = serde_json::json!(true);
                    daemon["version"] = v["version"].clone();
                    add(
                        "daemon",
                        true,
                        format!("{}（{}）", base, v["version"].as_str().unwrap_or("未知版本")),
                    );
                    match get("/agents") {
                        Ok(a) if a.status().is_success() => match a.json::<serde_json::Value>() {
                            Ok(v)
                                if v["agents"].is_array()
                                    && v["policy"].is_object()
                                    && v["scheduler"].is_object() => {
                                let n = v["agents"].as_array().map(|x| x.len()).unwrap_or(0);
                                let registry = v["registry"].as_bool().unwrap_or(false);
                                let caller = v["caller"].as_str().unwrap_or("");
                                identity = if !caller.is_empty() {
                                    format!("{caller}（只能看见自己）")
                                } else if registry {
                                    format!("主 token（可见 {n} 个 agent，registry=true）")
                                } else if token.is_some() {
                                    format!("主 token / 单 token（registry=false，可见 {n} 个来源）")
                                } else {
                                    format!("无 token（本机模式，registry=false，可见 {n} 个来源）")
                                };
                                daemon["registry"] = serde_json::json!(registry);
                                daemon["caller"] = v["caller"].clone();
                                daemon["agents"] = serde_json::json!(n);
                                // 有 caller 时看它自己的策略；主 token 看 daemon 的全局策略。
                                let pol = if caller.is_empty() {
                                    v.get("policy")
                                } else {
                                    v["agents"]
                                        .as_array()
                                        .and_then(|arr| {
                                            arr.iter().find(|a| {
                                                a["source"].as_str() == Some(caller)
                                            })
                                        })
                                        .and_then(|e| e.get("policy"))
                                        .or_else(|| v.get("policy"))
                                };
                                if let Some(p) = pol {
                                    effective_policy = Some(p.clone());
                                }
                                add("identity", true, identity.clone());
                            }
                            Ok(v) => {
                                identity = "daemon /agents 契约不完整".to_string();
                                add(
                                    "identity",
                                    false,
                                    format!("/agents 缺少 agents/policy/scheduler：{v}"),
                                );
                            }
                            Err(e) => {
                                identity = "daemon /agents 返回不是 JSON".to_string();
                                add("identity", false, format!("/agents 返回不是 JSON：{e}"));
                            }
                        },
                        Ok(a) => {
                            let code = a.status().as_u16();
                            let detail = if matches!(code, 401 | 403) {
                                format!("token 被拒绝（HTTP {code}）；检查 SKYGET_API_TOKEN / SKYGET_AGENTS")
                            } else {
                                format!("GET /agents 返回 HTTP {code}；检查 daemon 版本/接口")
                            };
                            add("identity", false, detail);
                        }
                        Err(e) => add("identity", false, format!("查不到 /agents：{e}")),
                    }
                }
            }
        }
        Ok(r) => add(
            "daemon",
            false,
            format!("{} 回了 HTTP {}", base, r.status().as_u16()),
        ),
        Err(e) => add("daemon", false, format!("连不上 {base}：{e}")),
    }

    if let Some(p) = &effective_policy {
        dest_roots = p["dest_roots"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        upload_roots = p["upload_roots"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
    }
    add(
        "dest_policy",
        true,
        if dest_roots.is_empty() {
            "未限制落地根（兼容模式）".to_string()
        } else {
            format!("{} 个落地根", dest_roots.len())
        },
    );
    add(
        "upload_policy",
        true,
        if upload_roots.is_empty() {
            "未配置上传根（上传默认禁止，这是安全默认）".to_string()
        } else {
            format!("{} 个上传根", upload_roots.len())
        },
    );

    let ok = writable && reachable && checks.iter().all(|c| c["ok"] != false);
    // 远端有策略就原样带出（allowed_hosts 可能是 null = 不限制，不能变成 []）；
    // 只有完全没有 daemon 信息时才用本机 env 兜底。
    let policy_json = effective_policy.unwrap_or_else(|| {
        serde_json::json!({
            "dest_roots": dest_roots,
            "upload_roots": upload_roots,
            "allowed_hosts": policy.allowed_hosts,
            "allow_private": policy.allow_private,
        })
    });
    let report = serde_json::json!({
        "ok": ok,
        "version": skyfetch::version(),
        "home": home.display().to_string(),
        "daemon": daemon,
        "identity": identity,
        "policy": policy_json,
        "checks": checks,
    });
    if json {
        // 全局 --json 契约：stdout 只留一行 JSON。doctor 后面还会以非零退出，
        // 所以必须显式标记"结果已经打过了"，否则 main 会再补一行 cli.failed，
        // 脚本 `| jq` 拿到两行就炸。
        println!("{}", serde_json::to_string(&report)?);
        mark_reported();
    } else {
        println!("天网盒子 doctor · {}", skyfetch::version());
        for c in report["checks"].as_array().cloned().unwrap_or_default() {
            let mark = if c["ok"] == true { "✓" } else { "!" };
            println!(
                "  {mark} {:<14} {}",
                c["name"].as_str().unwrap_or(""),
                c["detail"].as_str().unwrap_or("")
            );
        }
        println!("结论：{}", if ok { "可用" } else { "有问题，见上面的 !" });
    }
    if !ok {
        anyhow::bail!("doctor: 有检查未通过");
    }
    Ok(())
}

fn run_via_daemon(
    json: bool,
    url: &str,
    dest_dir: &str,
    name: Option<&str>,
    sha: &str,
    source: &str,
    mirrors: &[String],
    credential_ref: Option<&str>,
    detach: bool,
    priority: &str,
) -> Result<DaemonRun> {
    let base = api_base();
    let client = match api_client() {
        Ok(c) => c,
        Err(e) => return Ok(DaemonRun::Unavailable(format!("{e:#}"))),
    };
    let mut body = serde_json::json!({
        "url": url, "dest": dest_dir, "source": source, "priority": priority,
    });
    if !sha.is_empty() {
        body["sha256"] = serde_json::json!(sha);
    }
    if let Some(n) = name {
        body["name"] = serde_json::json!(n);
    }
    if !mirrors.is_empty() {
        body["sources"] = serde_json::json!(mirrors);
    }
    if let Some(c) = credential_ref {
        body["credential_ref"] = serde_json::json!(c);
    }
    let resp = match client.post(format!("{base}/jobs")).json(&body).send() {
        Ok(r) => r,
        Err(e) => return Ok(DaemonRun::Unavailable(format!("{e}"))),
    };
    let status = resp.status();
    let text = resp.text().unwrap_or_default();
    if status.as_u16() == 401 {
        anyhow::bail!(
            "守护进程要求 token；把 SKYGET_API_TOKEN 设成和 daemon 一样的值。"
        );
    }
    if !status.is_success() {
        anyhow::bail!(
            "守护进程拒绝了这次提交（HTTP {}）：{}",
            status.as_u16(),
            text
        );
    }
    let v: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("守护进程返回的不是 JSON：{text}"))?;
    let id = v
        .get("id")
        .and_then(|x| x.as_str())
        .context("守护进程没给 id")?
        .to_string();
    let deduped = v.get("deduped").and_then(|x| x.as_bool()).unwrap_or(false);
    if !json {
        eprintln!(
            "  交给守护进程：{id}{}",
            if deduped {
                "（已在台账里，直接跟）"
            } else {
                ""
            }
        );
    }

    if detach {
        if json {
            println!("{}", serde_json::to_string(&v)?);
        } else {
            println!("已排队 {id}；任务在 daemon 里跑，客户端可以退出。");
        }
        return Ok(DaemonRun::Done);
    }

    let state = v.get("state").and_then(|x| x.as_str()).unwrap_or("");
    if deduped && matches!(state, "done" | "failed" | "interrupted") {
        let view = fetch_job_view(&client, &base, &id)?;
        return finish_daemon_view(json, with_elapsed(view));
    }
    let view = watch_job_with(json, &client, &base, &id, 0, 25_000)?;
    finish_daemon_view(json, with_elapsed(view))
}

/// 打印最终结果，并把状态翻成退出码：失败了还 exit 0，agent 会以为成功。
fn finish_daemon_view(json: bool, view: skyfetch::TaskView) -> Result<DaemonRun> {
    let ok = view.state == "done";
    emit(&view, json);
    if ok {
        Ok(DaemonRun::Done)
    } else {
        anyhow::bail!(
            "任务 {} 未完成（{}）：{}",
            view.id,
            view.state,
            if view.error.is_empty() {
                "没有错误信息".to_string()
            } else {
                display_safe(&view.error)
            }
        )
    }
}

/// 传输层错误（连不上 / 连接被掐）才值得重连；
/// 4xx/5xx 是服务端给了答复，重连没意义。
fn is_transport_error(e: &reqwest::Error) -> bool {
    e.is_connect() || e.is_timeout() || e.is_request() || e.is_body() || e.is_decode()
}

fn fetch_job_view(
    client: &reqwest::blocking::Client,
    base: &str,
    id: &str,
) -> Result<skyfetch::TaskView> {
    let r = client
        .get(format!("{base}/jobs/{id}"))
        .send()
        .with_context(|| "查任务状态失败")?;
    let status = r.status();
    let text = r.text().unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!(
            "查不到任务 {id}（HTTP {}）：{}。它可能已经被取消/删除。",
            status.as_u16(),
            text
        );
    }
    serde_json::from_str(&text).with_context(|| format!("任务状态不是预期的 JSON：{text}"))
}

fn with_elapsed(mut v: skyfetch::TaskView) -> skyfetch::TaskView {
    if v.elapsed_ms == 0 && v.created_ms > 0 {
        v.elapsed_ms = (now_ms() - v.created_ms).max(0) as u64;
    }
    v
}

fn is_terminal(s: &str) -> bool {
    matches!(s, "done" | "failed" | "interrupted" | "cancelled")
}

fn watch_job_with(
    json: bool,
    client: &reqwest::blocking::Client,
    base: &str,
    id: &str,
    mut since: i64,
    wait_ms: u64,
) -> Result<skyfetch::TaskView> {
    let mut last = Instant::now() - Duration::from_secs(5);
    // 守护进程重启（升级、崩溃恢复）时，客户端不该跟着死：任务在 daemon 里跑着，
    // 这里只是长轮询断了。重连几次，仍连不上才失败并让用户重跑。
    let mut reconnects = 0u32;
    loop {
        let url = format!("{base}/jobs/{id}/events?since={since}&wait_ms={wait_ms}");
        let r = match client.get(&url).send() {
            Ok(r) => {
                if reconnects > 0 {
                    eprintln!("  重新接上了（重连 {reconnects} 次）");
                    reconnects = 0;
                }
                r
            }
            Err(e) if is_transport_error(&e) && reconnects < 12 => {
                reconnects += 1;
                let wait = Duration::from_millis(500 * reconnects as u64);
                eprintln!(
                    "  守护进程连接中断（{}），{} 毫秒后重连（第 {reconnects}/12 次）——任务还在 daemon 里跑",
                    e, wait.as_millis()
                );
                std::thread::sleep(wait);
                continue;
            }
            Err(e) => {
                return Err(e).with_context(|| {
                    "跟事件流失败（任务可能还在跑；重跑同一条命令会接着跟）"
                })
            }
        };
        let status = r.status();
        let text = r.text().unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("事件流 HTTP {}：{}", status.as_u16(), text);
        }
        let v: serde_json::Value = serde_json::from_str(&text)
            .with_context(|| format!("事件流不是预期的 JSON：{text}"))?;
        let events = v
            .get("events")
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default();
        let mut terminal = false;
        for e in &events {
            if let Some(seq) = e.get("seq").and_then(|x| x.as_i64()) {
                since = since.max(seq);
            }
            let kind = e.get("type").and_then(|x| x.as_str()).unwrap_or("");
            if is_terminal(kind) {
                terminal = true;
            }
            print_daemon_event(e, json, &mut last);
        }
        if terminal {
            let view = fetch_job_view(client, base, id)?;
            // 从 seq 0 重放时，历史里可能还留着上一次「暂停/失败」的终态事件。
            // 任务已经 retry/resume 的话，以当前状态为准继续跟，别提前退出。
            if is_terminal(&view.state) {
                return Ok(view);
            }
            continue;
        }
        // 调用方要求不等待时，别空转打爆 CPU。
        if events.is_empty() && wait_ms == 0 {
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

/// 终端进度条。只在 stderr 是 TTY、且不是 `--json` 时使用。
fn progress_bar(done: u64, total: u64, width: usize) -> String {
    let ratio = if total > 0 {
        (done as f64 / total as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let filled = (ratio * width as f64).round() as usize;
    format!("[{}{}]", "█".repeat(filled), "░".repeat(width - filled))
}

/// 总大小未知时不画空条，避免把“不知道”显示成 0%。
fn progress_prefix(done: u64, total: u64, width: usize) -> String {
    if total == 0 {
        return String::new();
    }
    format!(
        "{} {:>3.0}%  ",
        progress_bar(done, total, width),
        done as f64 / total as f64 * 100.0
    )
}

/// 原地刷一行 TTY 进度。`clock` 为真时按播放时长显示。
fn write_progress_line(done: u64, total: u64, clock: bool) {
    let (done_text, total_text) = if clock {
        (
            human_clock(done),
            if total > 0 { human_clock(total) } else { "?".into() },
        )
    } else {
        (human(done), human(total))
    };
    let _ = write!(
        std::io::stderr(),
        "\r  {}{} / {}   ",
        progress_prefix(done, total, 24),
        done_text,
        total_text
    );
    let _ = std::io::stderr().flush();
}

/// 活跃任务：running > pending > interrupted；同类里取最新创建的一条。
fn pick_active(
    jobs: Vec<skyfetch::TaskView>,
    source: Option<&str>,
) -> Option<skyfetch::TaskView> {
    jobs.into_iter()
        .filter(|j| match source {
            Some(s) => j.source.starts_with(s),
            None => true,
        })
        .filter(|j| matches!(j.state.as_str(), "running" | "pending" | "interrupted"))
        .max_by_key(|j| {
            let rank = match j.state.as_str() {
                "running" => 2,
                "pending" => 1,
                _ => 0,
            };
            (rank, j.created_ms)
        })
}

/// 过滤了来源时把来源写进标签，避免“Pi 的任务跑到我这里”的误会。
/// 终端显示宽度：CJK / 全角按 2 格算，否则状态栏会错位。
fn display_width(s: &str) -> usize {
    s.chars()
        .map(|c| match c as u32 {
            0x1100..=0x115F
            | 0x2E80..=0xA4CF
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE30..=0xFE6F
            | 0xFF00..=0xFF60
            | 0xFFE0..=0xFFE6 => 2,
            _ => 1,
        })
        .sum()
}

/// 自动宽度：`COLUMNS` 减去标签和百分比占位，夹在 10..=60。
fn auto_width(label: &str) -> usize {
    let cols = std::env::var("COLUMNS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(80);
    cols.saturating_sub(display_width(label) + 8).clamp(10, 60)
}

fn statusline_label(source: Option<&str>) -> String {
    match source.and_then(|s| s.strip_prefix("agent:")) {
        Some(name) if !name.is_empty() => format!("Skygeting[{name}]: "),
        _ => "Skygeting: ".to_string(),
    }
}

fn statusline_pct(done: u64, total: u64) -> u64 {
    done.saturating_mul(100)
        .checked_div(total)
        .unwrap_or(0)
        .min(100)
}

/// 有进度就至少亮一格，避免低速任务长时间停在 0%。
fn statusline_filled(done: u64, total: u64, width: usize) -> usize {
    let pct = statusline_pct(done, total) as usize;
    pct.saturating_mul(width).div_ceil(100).min(width)
}

fn statusline_suffix(view: &skyfetch::TaskView) -> &'static str {
    match view.state.as_str() {
        "interrupted" => " (paused)",
        "failed" => " (failed)",
        _ => "",
    }
}

/// 有字节就按字节；HLS/DASH 没有总字节时按播放秒数。两者都没有就是 0%。
fn statusline_ratio(view: &skyfetch::TaskView, secs: Option<(u64, u64)>) -> (u64, u64) {
    // 已完成但没有总字节（HLS/多文件种子）：终态就是 100%，不要闪一下 0%。
    if view.state == "done" && view.bytes_total == 0 {
        return (1, 1);
    }
    if view.bytes_total == 0 {
        if let Some((done, total)) = secs {
            if total > 0 {
                return (done, total);
            }
        }
    }
    (view.bytes_done, view.bytes_total)
}

fn statusline_ansi(
    view: &skyfetch::TaskView,
    width: usize,
    label: &str,
    secs: Option<(u64, u64)>,
) -> String {
    let (done, total) = statusline_ratio(view, secs);
    let filled = statusline_filled(done, total, width);
    format!(
        "{label}\x1b[32m{fill}\x1b[0m\x1b[38;2;0;0;0m{pad}\x1b[0m \x1b[32m{}%\x1b[0m{}",
        statusline_pct(done, total),
        statusline_suffix(view),
        fill = "━".repeat(filled),
        pad = "━".repeat(width - filled),
    )
}

fn statusline_plain(
    view: &skyfetch::TaskView,
    width: usize,
    label: &str,
    secs: Option<(u64, u64)>,
) -> String {
    let (done, total) = statusline_ratio(view, secs);
    let filled = statusline_filled(done, total, width);
    format!(
        "{label}{}{} {}%{}",
        "#".repeat(filled),
        "-".repeat(width - filled),
        statusline_pct(done, total),
        statusline_suffix(view)
    )
}

fn statusline_json(
    view: &skyfetch::TaskView,
    width: usize,
    secs: Option<(u64, u64)>,
) -> String {
    let (done, total) = statusline_ratio(view, secs);
    serde_json::json!({
        "job_id": view.id,
        "state": view.state,
        "source": view.source,
        // done/total 保持字节语义；HLS 的秒进度单独给，插件自己选。
        "done": view.bytes_done,
        "total": view.bytes_total,
        "seconds_done": secs.map(|(d, _)| d).unwrap_or(0),
        "seconds_total": secs.map(|(_, t)| t).unwrap_or(0),
        "unit": if view.bytes_total == 0 && total > 0 { "seconds" } else { "bytes" },
        "pct": statusline_pct(done, total),
        "width": width,
    })
    .to_string()
}

/// OSC 9;4：终端标签页/任务栏的原生进度；done 时清掉。
fn osc_statusline(pct: u64, state: &str) -> String {
    let (code, value) = match state {
        "done" => (0, 0),
        "failed" => (2, pct),
        "interrupted" => (4, pct),
        _ => (1, pct),
    };
    format!("\x1b]9;4;{code};{value}\x1b\\")
}

/// 取任务：给了 --job 就只认它；否则在活跃任务里挑一条。
fn statusline_job(
    client: &reqwest::blocking::Client,
    base: &str,
    job: Option<&str>,
    source: Option<&str>,
) -> Option<skyfetch::TaskView> {
    if let Some(id) = job {
        let r = client.get(format!("{base}/jobs/{id}")).send().ok()?;
        if !r.status().is_success() {
            return None;
        }
        return serde_json::from_str(&r.text().ok()?).ok();
    }
    // 只拉活跃任务：不要因为最近几百条已完成记录把长任务挤出窗口。
    let r = client.get(format!("{base}/jobs?active=1&limit=500")).send().ok()?;
    if !r.status().is_success() {
        return None;
    }
    let jobs: Vec<skyfetch::TaskView> = serde_json::from_str(&r.text().ok()?).ok()?;
    pick_active(jobs, source)
}

/// HLS/DASH 没有总字节：从 daemon 的 progress_secs 事件拿秒进度。
fn statusline_seconds(
    client: &reqwest::blocking::Client,
    base: &str,
    id: &str,
) -> Option<(u64, u64)> {
    let r = client
        .get(format!("{base}/jobs/{id}/seconds"))
        .send()
        .ok()?;
    if !r.status().is_success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(&r.text().ok()?).ok()?;
    let done = v["seconds_done"].as_u64()?;
    let total = v["seconds_total"].as_u64()?;
    (total > 0).then_some((done, total))
}

fn emit_statusline(
    format: &str,
    view: &skyfetch::TaskView,
    width: usize,
    label: &str,
    secs: Option<(u64, u64)>,
    osc: bool,
    live_tty: bool,
    live: &mut bool,
) {
    let line = match format {
        "json" => statusline_json(view, width, secs),
        "plain" => statusline_plain(view, width, label, secs),
        _ => statusline_ansi(view, width, label, secs),
    };
    if osc && format != "json" {
        let (done, total) = statusline_ratio(view, secs);
        print!(
            "{}",
            osc_statusline(statusline_pct(done, total), &view.state)
        );
    }
    if live_tty && format != "json" {
        print!("\r{line}\x1b[K");
        let _ = std::io::stdout().flush();
        *live = true;
    } else {
        println!("{line}");
    }
}

fn run_statusline(
    format: &str,
    watch: bool,
    interval_ms: u64,
    job: Option<&str>,
    source: Option<&str>,
    osc: bool,
    width: usize,
) -> Result<()> {
    let format = match format {
        "ansi" | "plain" | "json" => format,
        other => anyhow::bail!("--format 只支持 ansi / plain / json（当前 {other}）"),
    };
    let client = api_client()?;
    let base = api_base();
    let label = statusline_label(source);
    // 宽度要跟着宿主走：写死 40 格在窄状态栏里会被截断，在宽终端里又太短。
    let width = if width == 0 {
        auto_width(&label)
    } else {
        width.clamp(10, 200)
    };
    let live_tty = watch && std::io::stdout().is_terminal();
    let mut last_id = job.map(str::to_string);
    let mut live = false;
    loop {
        let view = match statusline_job(&client, &base, job, source) {
            Some(v) => v,
            None => {
                // 自动模式下任务刚结束：补一帧终态，再退出。
                if let Some(id) = last_id.take() {
                    if let Some(v) = statusline_job(&client, &base, Some(&id), source) {
                        emit_statusline(
                            format, &v, width, &label, None, osc, live_tty, &mut live,
                        );
                    }
                }
                break;
            }
        };
        last_id = Some(view.id.clone());
        // HLS/DASH 的 bytes_total 一直是 0；拿秒进度画条。
        let secs = if view.bytes_total == 0
            && matches!(view.state.as_str(), "running" | "pending")
        {
            statusline_seconds(&client, &base, &view.id)
        } else {
            None
        };
        emit_statusline(format, &view, width, &label, secs, osc, live_tty, &mut live);
        if !watch || is_terminal(&view.state) {
            break;
        }
        std::thread::sleep(Duration::from_millis(interval_ms.max(100)));
    }
    if live {
        println!();
    }
    Ok(())
}

/// 把守护进程的事件翻成旧 CLI 的 NDJSON 形状，agent 的解析代码不用改。
fn print_daemon_event(e: &serde_json::Value, json: bool, last: &mut Instant) {
    let kind = e.get("type").and_then(|x| x.as_str()).unwrap_or("");
    let d = e.get("data").unwrap_or(&serde_json::Value::Null);
    let tty = std::io::stderr().is_terminal();
    match kind {
        "progress" => {
            let gap = if json { 500 } else { 200 };
            if last.elapsed() < Duration::from_millis(gap) {
                return;
            }
            *last = Instant::now();
            let done = d.get("bytes_done").and_then(|x| x.as_u64()).unwrap_or(0);
            let total = d.get("bytes_total").and_then(|x| x.as_u64()).unwrap_or(0);
            if json {
                let _ = writeln!(
                    std::io::stderr(),
                    "{}",
                    serde_json::json!({
                        "event": "progress", "bytes_done": done, "bytes_total": total
                    })
                );
            } else if tty {
                write_progress_line(done, total, false);
            }
        }
        "progress_secs" => {
            let done = d.get("seconds_done").and_then(|x| x.as_u64()).unwrap_or(0);
            let total = d.get("seconds_total").and_then(|x| x.as_u64()).unwrap_or(0);
            if json {
                let _ = writeln!(
                    std::io::stderr(),
                    "{}",
                    serde_json::json!({
                        "event": "progress_secs", "done": done,
                        "bytes_total": 0, "seconds_done": done, "seconds_total": total
                    })
                );
            } else if tty {
                write_progress_line(done, total, true);
            }
        }
        "links" => {
            if json {
                let _ = writeln!(
                    std::io::stderr(),
                    "{}",
                    serde_json::json!({
                        "event": "links",
                        "live": d.get("live").and_then(|x| x.as_u64()).unwrap_or(0),
                        "seen": d.get("seen").and_then(|x| x.as_u64()).unwrap_or(0),
                    })
                );
            }
        }
        "named" => {
            let name = d.get("name").and_then(|x| x.as_str()).unwrap_or("");
            if json {
                let _ = writeln!(
                    std::io::stderr(),
                    "{}",
                    serde_json::json!({"event": "named", "name": name})
                );
            } else if tty {
                let _ = writeln!(std::io::stderr(), "  {name}");
            }
        }
        "note" => {
            let text = d.get("text").and_then(|x| x.as_str()).unwrap_or("");
            if json {
                let _ = writeln!(
                    std::io::stderr(),
                    "{}",
                    serde_json::json!({"event": "note", "text": text})
                );
            } else {
                let _ = writeln!(std::io::stderr(), "  {text}");
            }
        }
        "verifying" => {
            if !json && tty {
                let _ = write!(std::io::stderr(), "\n  校验 sha256…\n");
            }
        }
        "queued" => {
            if json {
                let _ = writeln!(
                    std::io::stderr(),
                    "{}",
                    serde_json::json!({"event": "queued"})
                );
            } else if tty {
                let _ = writeln!(std::io::stderr(), "  已排队，等守护进程调度");
            }
        }
        "failed" if !json && tty => {
            let _ = writeln!(std::io::stderr(), "  失败，等最终结果…");
        }
        _ => {}
    }
}

/// 终端驾驶舱：一屏回答“谁在下什么、到哪了、凭什么信”。
fn print_top(jobs: &[serde_json::Value]) {
    let count = |s: &str| jobs.iter().filter(|j| j["state"] == s).count();
    println!(
        "JOBS {}   running {}  queued {}  done {}  failed {}",
        jobs.len(),
        count("running"),
        count("pending"),
        count("done"),
        count("failed")
    );
    println!(
        "{:<10} {:<8} {:<16} {:<10} {:<10} {}",
        "ID", "STATE", "SOURCE", "TRUST", "PROGRESS", "PATH / ERROR"
    );
    for j in jobs {
        let id = j["id"].as_str().unwrap_or("");
        let state = j["state"].as_str().unwrap_or("");
        let source = trunc(j["source"].as_str().unwrap_or(""), 16);
        let trust = trunc(j["trust"].as_str().unwrap_or(""), 10);
        let done = j["bytes_done"].as_u64().unwrap_or(0);
        let total = j["bytes_total"].as_u64().unwrap_or(0);
        let progress = if total > 0 {
            format!("{:>3.0}%", done as f64 / total as f64 * 100.0)
        } else {
            human(done)
        };
        let tail = if state == "failed" {
            j["error"]
                .as_str()
                .unwrap_or("")
                .lines()
                .next()
                .unwrap_or("")
        } else {
            j["path"].as_str().unwrap_or("")
        };
        println!(
            "{:<10} {:<8} {:<16} {:<10} {:<10} {}",
            &id[..10.min(id.len())],
            top_state_label(state, j["direction"].as_str().unwrap_or("")),
            source,
            trust,
            progress,
            trunc(tail, 60)
        );
    }
}

fn top_state_label(s: &str, direction: &str) -> &'static str {
    let upload = direction == "upload";
    match s {
        "pending" => "排队",
        "running" => {
            if upload {
                "上传中"
            } else {
                "下载中"
            }
        }
        "done" => {
            if upload {
                "已上传"
            } else {
                "完成"
            }
        }
        "failed" => "失败",
        "interrupted" => "中断",
        _ => "?",
    }
}

fn trunc(s: &str, n: usize) -> String {
    let s = display_safe(s);
    if s.chars().count() > n {
        format!("{}…", s.chars().take(n.saturating_sub(1)).collect::<String>())
    } else {
        s
    }
}

/// 这条命令已经往 stdout 打过「最终结果」了（成功或失败都算）。
///
/// main 只在**什么都没打过**时补那一行 `cli.failed`：任务级失败本来就会打
/// `{"state":"failed",…}`，再补一条就成了两行 stdout —— 契约是只有一行。
static REPORTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 已经打过结果了（预检这种「打一行结论再以非零退出」的路径要显式标一下）。
fn mark_reported() {
    REPORTED.store(true, std::sync::atomic::Ordering::Relaxed);
}

fn emit(view: &skyfetch::TaskView, json: bool) {
    REPORTED.store(true, std::sync::atomic::Ordering::Relaxed);
    if json {
        println!("{}", serde_json::to_string(view).unwrap_or_default());
        return;
    }
    if view.state == STATE_DONE {
        if view.direction == skyfetch::model::DIR_UPLOAD {
            println!(
                "已上传  {}  ->  {}  ({})",
                display_safe(&view.path),
                display_safe(&view.url),
                human(view.bytes_total.max(view.bytes_done))
            );
        } else {
            println!(
                "已落地  {}  ({})",
                display_safe(&view.path),
                human(view.bytes_total.max(view.bytes_done))
            );
        }
        println!("sha256  {}  （信任：{}）", view.sha256, trust_label(&view.trust));
        if view.cached {
            println!("说明    这份内容本机早有了，没有重新下载");
        }
    } else {
        // 多行原因（一次几十段失败时）逐行缩进，别让第二行起顶到左边；
        // 每行都过 display_safe，错误文本可能带源站返回的转义序列。
        let err = view
            .error
            .lines()
            .map(display_safe)
            .collect::<Vec<_>>()
            .join("\n      ");
        println!("失败  {err}");
    }
}

/// 进度只走 stderr：stdout 留给最终那行 JSON，管道给 agent 时不掺噪音。
fn make_cb<'a>(
    json: bool,
    cached: &'a std::sync::atomic::AtomicBool,
) -> impl Fn(Event) + Sync + 'a {
    let last = Mutex::new(Instant::now() - Duration::from_secs(5));
    // 管道模式按 20% 一档：pi 之类会把长输出折叠成「… (N earlier lines, ctrl+o)」，
    // 行数一多用户就只看到最后两行，等于没进度。6 行以内不会被折。
    let bucket = Mutex::new(0u64);
    let tty = std::io::stderr().is_terminal();
    // auto（默认）= 只在终端里画；stderr = 管道里也写一行一条；off = 全静音。
    // agent 抓走的 stderr 不是终端，所以默认什么都看不到 —— 这正是要显式开的原因。
    let mode = std::env::var("SKYGET_PROGRESS").unwrap_or_else(|_| "auto".to_string());
    let quiet = mode == "off";
    // 终端里用 \r 原地刷新；管道里只能一行一条，否则会糊成一坨。
    let piped = mode == "stderr" && !tty;
    move |e: Event| {
        if quiet {
            return;
        }
        match e {
            Event::Progress { done, total } => {
                // 管道里的每一行都会留在屏幕上（不像终端里 \r 会覆盖），
                // 所以节奏要放慢、内容要自带条和百分比 —— 200ms 一条等于刷屏。
                let gap = if json { 500 } else if piped { 2000 } else { 200 };
                {
                    let mut l = last.lock().unwrap();
                    if l.elapsed() < Duration::from_millis(gap) {
                        return;
                    }
                    *l = Instant::now();
                }
                if json {
                    let _ = writeln!(
                        std::io::stderr(),
                        "{}",
                        serde_json::json!({
                            "event": "progress",
                            "bytes_done": done,
                            "bytes_total": total
                        })
                    );
                } else if tty {
                    write_progress_line(done, total, false);
                } else if piped {
                    let pct = done.saturating_mul(100).checked_div(total).unwrap_or(0);
                    let mut b = bucket.lock().unwrap();
                    if pct / 20 >= *b {
                        *b = pct / 20 + 1;
                        let _ = writeln!(
                            std::io::stderr(),
                            "  {}{} / {}",
                            progress_prefix(done, total, 20),
                            human(done),
                            human(total)
                        );
                    }
                }
            }
            Event::Planned { total, ranges, threads } => {
                if !json && tty {
                    let _ = writeln!(
                        std::io::stderr(),
                        "  {}{} · {} 个连接",
                        human(total),
                        if ranges { " · 可分段" } else { "" },
                        threads
                    );
                }
            }
            Event::ProgressSecs { done, total } => {
                {
                    let mut l = last.lock().unwrap();
                    if l.elapsed() < Duration::from_millis(if json { 500 } else { 200 }) {
                        return;
                    }
                    *l = Instant::now();
                }
                if json {
                    let _ = writeln!(
                        std::io::stderr(),
                        "{}",
                        serde_json::json!({
                            "event": "progress_secs",
                            "done": done,
                            "bytes_total": 0,
                            "seconds_done": done,
                            "seconds_total": total
                        })
                    );
                } else if tty {
                    write_progress_line(done, total, true);
                }
            }
            Event::Links { live, seen } => {
                if json {
                    let _ = writeln!(
                        std::io::stderr(),
                        "{}",
                        serde_json::json!({
                            "event": "links",
                            "live": live,
                            "seen": seen
                        })
                    );
                }
            }
            Event::Named(n) => {
                if !json && tty {
                    let _ = writeln!(std::io::stderr(), "  {}", display_safe(&n));
                } else if json {
                    let _ = writeln!(
                        std::io::stderr(),
                        "{}",
                        serde_json::json!({"event": "named", "name": n})
                    );
                }
            }
            Event::Verifying => {
                if !json && tty {
                    let _ = write!(std::io::stderr(), "\n  校验 sha256…\n");
                }
            }
            Event::Note(n) => {
                if !json {
                    let _ = writeln!(std::io::stderr(), "  {}", display_safe(&n));
                } else {
                    let _ = writeln!(
                        std::io::stderr(),
                        "{}",
                        serde_json::json!({"event": "note", "text": n})
                    );
                }
            }
            Event::Finished { cached: c } => {
                cached.store(c, std::sync::atomic::Ordering::Relaxed);
            }
            Event::Phase { name, ms } => {
                if json {
                    let _ = writeln!(
                        std::io::stderr(),
                        "{}",
                        serde_json::json!({"event": "phase", "name": name, "ms": ms})
                    );
                }
            }
            Event::Probing => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn argv(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }
    fn strs(v: Vec<OsString>) -> Vec<String> {
        v.iter().map(|s| s.to_string_lossy().to_string()).collect()
    }

    #[test]
    fn host_policy_display_distinguishes_null_from_empty() {
        assert_eq!(
            join_hosts(&serde_json::Value::Null),
            "（不限制）",
            "null = 不限制"
        );
        assert_eq!(
            join_hosts(&serde_json::json!([])),
            "（拒绝全部主机）",
            "空数组 = 全部拒绝，不能显示成不限制"
        );
        assert_eq!(
            join_hosts(&serde_json::json!(["a.com", "*.b.com"])),
            "a.com · *.b.com"
        );
    }

    #[test]
    fn url_shapes_are_recognised() {
        assert!(looks_like_url("https://x/y.zip"));
        assert!(looks_like_url("http://x/y"));
        assert!(looks_like_url("magnet:?xt=urn:btih:abc"));
        assert!(looks_like_url("https://x/a.torrent?token=1"));
        assert!(looks_like_url("https://x/a.m3u8"));
        // 选项的值不该被当成地址
        assert!(!looks_like_url("/tmp/out"));
        assert!(!looks_like_url("claude"));
        assert!(!looks_like_url("1a17609f86b0442"));
    }

    #[test]
    fn bare_url_gets_an_implicit_get() {
        let got = strs(with_implicit_get(argv(&["skyget", "https://x/a.zip"])));
        assert_eq!(got, ["skyget", "get", "https://x/a.zip"]);

        // 选项写在地址前面也要成立
        let got = strs(with_implicit_get(argv(&[
            "skyget", "--json", "-o", "/tmp", "https://x/a.zip",
        ])));
        assert_eq!(got[1], "get");
        assert_eq!(got.len(), 6);
    }

    #[test]
    fn detach_is_recognised_and_conflicts_with_standalone() {
        let cli = Cli::try_parse_from(["skyget", "get", "https://x/a.zip", "--detach"]).unwrap();
        match cli.cmd {
            Some(Cmd::Get(a)) => assert!(a.detach),
            _ => panic!("expected get"),
        }
        assert!(Cli::try_parse_from([
            "skyget",
            "get",
            "https://x/a.zip",
            "--detach",
            "--standalone"
        ])
        .is_err());
    }

    #[test]
    fn subcommands_are_left_alone() {
        for v in [
            vec!["skyget", "list"],
            vec!["skyget", "show", "1a1760"],
            // install 的清单地址长得像 URL，但它有子命令，不能被改写成 get
            vec!["skyget", "install", "https://x/manifest.json"],
            vec!["skyget", "--help"],
            vec!["skyget"],
        ] {
            assert_eq!(strs(with_implicit_get(argv(&v))), v);
        }
    }

    #[test]
    fn progress_bar_is_fixed_width_and_clamped() {
        assert_eq!(progress_bar(0, 100, 10), "[░░░░░░░░░░]");
        assert_eq!(progress_bar(50, 100, 10), "[█████░░░░░]");
        assert_eq!(progress_bar(100, 100, 10), "[██████████]");
        assert_eq!(progress_bar(200, 100, 10), "[██████████]");
        assert_eq!(progress_bar(50, 0, 10), "[░░░░░░░░░░]");
        assert_eq!(progress_bar(1, 3, 10).chars().count(), 12);

        // 总量未知时不画条、不显示假的 0%
        assert_eq!(progress_prefix(50, 0, 10), "");
        assert_eq!(progress_prefix(50, 100, 10), "[█████░░░░░]  50%  ");
    }

    fn task_view(state: &str, created_ms: i64) -> skyfetch::TaskView {
        skyfetch::TaskView {
            id: format!("id-{created_ms}"),
            direction: "download".into(),
            protocol: String::new(),
            url: "https://example.org/x".into(),
            sources: Vec::new(),
            state: state.into(),
            path: String::new(),
            bytes_done: 0,
            bytes_total: 100,
            sha256: String::new(),
            error: String::new(),
            error_code: String::new(),
            retryable: false,
            retry_after_ms: 0,
            next_action: String::new(),
            source: "agent:test".into(),
            priority: "normal".into(),
            cached: false,
            elapsed_ms: 0,
            trust: String::new(),
            created_ms,
            links_live: 0,
            links_seen: 0,
        }
    }

    #[test]
    fn statusline_bar_is_visible_early_and_clamped() {
        assert_eq!(statusline_filled(0, 100, 40), 0);
        assert_eq!(statusline_filled(1, 100, 40), 1);
        assert_eq!(statusline_filled(50, 100, 40), 20);
        assert_eq!(statusline_filled(100, 100, 40), 40);
        assert_eq!(statusline_filled(200, 100, 40), 40);
        assert_eq!(statusline_filled(50, 0, 40), 0);
    }

    #[test]
    fn statusline_formats_agree_on_pct() {
        let mut v = task_view("running", 1);
        v.bytes_done = 25;
        v.bytes_total = 100;

        let ansi = statusline_ansi(&v, 20, "Skygeting: ", None);
        assert!(ansi.contains("25%"));
        assert!(ansi.contains('━'));

        let plain = statusline_plain(&v, 20, "Skygeting: ", None);
        assert!(plain.starts_with("Skygeting: #####"));
        assert!(plain.ends_with("25%"));

        let json = statusline_json(&v, 20, None);
        assert!(json.contains("\"pct\":25"));
        assert!(json.contains("\"done\":25"));
    }

    #[test]
    fn hls_statusline_uses_seconds_when_bytes_are_unknown() {
        let mut v = task_view("running", 1);
        v.bytes_total = 0;
        assert_eq!(statusline_ratio(&v, Some((50, 100))), (50, 100));
        assert_eq!(statusline_ratio(&v, None), (0, 0));

        let mut done = task_view("done", 1);
        done.bytes_total = 0;
        assert_eq!(statusline_ratio(&done, None), (1, 1), "未知总量的完成态是 100%");

        let ansi = statusline_ansi(&v, 20, "Skygeting: ", Some((50, 100)));
        assert!(ansi.contains("50%"), "{ansi}");
        let json = statusline_json(&v, 20, Some((50, 100)));
        assert!(json.contains("\"unit\":\"seconds\""), "{json}");
        assert!(json.contains("\"seconds_done\":50"), "{json}");
        assert!(json.contains("\"pct\":50"), "{json}");
    }

    #[test]
    fn pick_active_prefers_running_then_newest() {
        let picked = pick_active(
            vec![
                task_view("done", 9),
                task_view("pending", 2),
                task_view("running", 1),
            ],
            None,
        )
        .unwrap();
        assert_eq!(picked.id, "id-1");

        let picked =
            pick_active(vec![task_view("pending", 2), task_view("pending", 3)], None).unwrap();
        assert_eq!(picked.id, "id-3");

        // 暂停任务只在没有更活跃的任务时显示。
        let picked = pick_active(
            vec![task_view("interrupted", 5), task_view("pending", 1)],
            None,
        )
        .unwrap();
        assert_eq!(picked.id, "id-1");
        let picked = pick_active(
            vec![task_view("interrupted", 5), task_view("interrupted", 6)],
            None,
        )
        .unwrap();
        assert_eq!(picked.id, "id-6");

        assert!(pick_active(vec![task_view("done", 1)], None).is_none());
    }

    #[test]
    fn statusline_shows_unknown_total_and_paused() {
        let mut v = task_view("running", 1);
        v.bytes_total = 0;
        assert_eq!(statusline_filled(0, 0, 20), 0);
        assert!(statusline_plain(&v, 20, "Skygeting: ", None).ends_with("0%"));

        v.bytes_total = 100;
        v.bytes_done = 50;
        v.state = "interrupted".into();
        assert!(statusline_plain(&v, 20, "Skygeting: ", None).ends_with("(paused)"));
    }

    #[test]
    fn statusline_filters_and_labels_sources() {
        let mut claude = task_view("running", 1);
        claude.source = "agent:claude".into();
        let mut pi = task_view("running", 2);
        pi.source = "agent:pi".into();

        let picked = pick_active(vec![claude, pi], Some("agent:pi")).unwrap();
        assert_eq!(picked.source, "agent:pi");
        assert_eq!(statusline_label(Some("agent:pi")), "Skygeting[pi]: ");
        assert_eq!(statusline_label(None), "Skygeting: ");
    }

    #[test]
    fn osc_sequence_covers_states() {
        assert_eq!(osc_statusline(50, "running"), "\x1b]9;4;1;50\x1b\\");
        assert_eq!(osc_statusline(100, "done"), "\x1b]9;4;0;0\x1b\\");
        assert_eq!(osc_statusline(30, "failed"), "\x1b]9;4;2;30\x1b\\");
        assert_eq!(osc_statusline(30, "interrupted"), "\x1b]9;4;4;30\x1b\\");
    }

    #[test]
    fn table_columns_hold_for_cjk() {
        // 中文按两格算：补齐后视觉宽度一致
        assert_eq!(pad_display("完成", 8).chars().count(), 6); // 2 字 + 4 空格
        assert_eq!(pad_display("done", 8).len(), 8);
        assert_eq!(pad_left_display("大小", 9).chars().count(), 7); // 5 空格 + 2 字
    }

    #[test]
    fn display_safe_strips_terminal_escapes() {
        // 恶意 agent 可以把 ESC/OSC 塞进 dest/URL，列表输出不能执行它。
        assert_eq!(display_safe("a\x1b]52;c;x\x07b"), "a]52;c;xb");
        assert_eq!(display_safe("中文\n路径\t"), "中文路径");
        assert_eq!(display_safe("normal-name"), "normal-name");
    }
}
