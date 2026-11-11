//! 本地 HTTP 接口：让别的进程（另一个 agent、编辑器、脚本）也能入队，
//! 而不只是 CLI 和那个窗口。
//!
//! 只绑 127.0.0.1，没有鉴权 —— 这台机器上能跑程序的进程本来就能直接跑 `skyget`，
//! 加一层 token 只是自我安慰。要跨机器用，自己套 SSH 隧道。
//!
//! ```text
//! GET  /health          → {"ok":true,"version":"..."}
//! GET  /jobs            → 台账最近 50 条
//! GET  /jobs/<id>       → 单条（含实时状态：跑着的任务由进程号判断）
//! POST /jobs            → {"url":"...","sha256":"...","dest":"..."} → {"id":"..."}
//! ```
//!
//! POST 立刻返回 id，下载在后台线程里跑；进度用 GET /jobs/<id> 轮询。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::agents::{AgentPolicies, Registry};
use crate::engine::Engine;
use crate::model::{new_id, now_ms, Task};
use crate::scheduler::{Scheduler, SubmitError};
use crate::store::Store;

#[derive(Deserialize)]
struct NewJob {
    url: String,
    /// 额外镜像。多源必须有 pin 的 sha256（否则无法判断哪份内容才是对的）。
    #[serde(default)]
    sources: Vec<String>,
    #[serde(default)]
    sha256: String,
    #[serde(default)]
    dest: String,
    #[serde(default)]
    source: String,
    /// 调用方自己生成的幂等键。不给就用 `(url, dest, sha256)` 自然键。
    #[serde(default)]
    idempotency_key: String,
    /// 可选文件名。给了就把 dest 当目录；不给则 dest 是目录（不带尾斜杠也认）。
    #[serde(default)]
    name: String,
    /// 地址内容变了也接受（正常更新时用）。和 CLI 的 --accept-change 同义。
    #[serde(default)]
    accept_change: bool,
    /// 下载凭证引用：env:NAME / file:PATH；只存引用，值不落台账。
    #[serde(default)]
    credential_ref: String,
    /// 排队优先级：interactive / normal / batch。默认 normal。
    ///
    /// 只影响排队顺序，不影响并发上限 —— 别把它当成「插队抢光别人的槽」，
    /// 那是配额的事。
    #[serde(default)]
    priority: String,
}

/// 一个任务这一次运行的句柄。
///
/// `queued` 区分「还在队列里」和「worker 已经拿走」；`cancelled` 区分
/// 「暂停（可复位）」和「取消（不可复活）」；`gen` 防止旧队列项在退出时
/// 把同名任务的新运行从表里删掉（排队任务暂停后 resume 会起新的一次运行）。
#[derive(Clone)]
struct RunHandle {
    cancel: crate::engine::Cancel,
    gen: u64,
    queued: bool,
    cancelled: bool,
}

/// 正在跑 / 排队中的任务 → 它的停止信号。暂停 / 取消要能按 id 找到它。
type Running = Arc<std::sync::Mutex<std::collections::HashMap<String, RunHandle>>>;

static RUN_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn finish_run(running: &Running, id: &str, gen: u64) {
    let mut m = running.lock().unwrap();
    if m.get(id).map(|h| h.gen) == Some(gen) {
        m.remove(id);
    }
}

/// worker 真正开始跑时调用：标记「已出队」，并看一眼有没有被暂停/取消。
/// 与 resume 的复位共用同一把表锁，避免「刚复位又被旧队列项退出」的竞态。
fn mark_started(running: &Running, id: &str, gen: u64) -> bool {
    let mut m = running.lock().unwrap();
    match m.get_mut(id) {
        Some(h) if h.gen == gen => {
            h.queued = false;
            !h.cancelled && !h.cancel.cancelled()
        }
        _ => false,
    }
}

/// job 执行期间的 RAII 守卫：正常结束、panic、提前返回都走同一条收尾路径。
///
/// scheduler 用 `catch_unwind` 保证 worker 活着，但 panic 的任务还会留下
/// 「running 表里有 handle、台账里是 running」的脏账。Drop 在 unwind 时也会执行，
/// 所以这里补：清 handle；如果台账还是 pending/running，标成 internal.panic。
struct JobGuard {
    running: Running,
    store: Arc<Store>,
    id: String,
    gen: u64,
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        finish_run(&self.running, &self.id, self.gen);
        let Ok(Some(mut t)) = self.store.get(&self.id) else {
            return;
        };
        if t.state == crate::model::STATE_RUNNING || t.state == crate::model::STATE_PENDING {
            t.state = crate::model::STATE_FAILED.to_string();
            t.error = "任务内部 panic（这是程序 bug，不是源的问题）".to_string();
            t.error_code = "internal.panic".to_string();
            t.retryable = false;
            t.retry_after_ms = 0;
            t.next_action = "把这条错误和日志一起报给维护者。".to_string();
            t.pid = 0;
            t.updated_at = now_ms();
            let _ = self.store.save(&t);
            let _ = self.store.append_event(
                &self.id,
                "failed",
                &serde_json::json!({
                    "error": &t.error, "code": "internal.panic", "retryable": false,
                }),
            );
        }
        crate::webhook::notify(&self.store, &self.id);
    }
}

/// 把一次「排队中暂停」复位。只有还在队列里、且没被取消的句柄能复位。
fn resume_queued(running: &Running, id: &str, gen: u64) -> bool {
    let mut m = running.lock().unwrap();
    match m.get_mut(id) {
        Some(h) if h.gen == gen && h.queued && !h.cancelled => {
            h.cancel.reset();
            true
        }
        _ => false,
    }
}

fn pid_alive(pid: i64) -> bool {
    crate::sys::pid_alive(pid)
}

/// 有 token 的调用方只能碰自己的任务；本机无 token 模式保持全可见。
fn owns(agent: Option<&str>, task: &crate::model::Task) -> bool {
    match agent {
        None => true,
        Some(name) => task.source == format!("agent:{name}") || task.source == name,
    }
}

fn job_not_found() -> (u16, serde_json::Value) {
    (
        404,
        serde_json::json!({"error": "没有这个任务", "code": "job.not_found"}),
    )
}

/// HTTP 请求行/头行的硬上限。`read_line` 无界读，一个本机 agent 发一条
/// 几 GB 的「头」就能把 daemon 内存打满；这里按字节封顶。
const MAX_HTTP_LINE: usize = 16 * 1024;
/// 头数量上限；单行有界，总量也就有界。
const MAX_HTTP_HEADERS: usize = 100;

fn read_line_limited<R: BufRead>(r: &mut R, limit: usize) -> std::io::Result<Option<String>> {
    let mut out: Vec<u8> = Vec::new();
    loop {
        let buf = r.fill_buf()?;
        if buf.is_empty() {
            if out.is_empty() {
                return Ok(None);
            }
            break;
        }
        let (take, newline) = match buf.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (buf.len(), false),
        };
        out.extend_from_slice(&buf[..take]);
        r.consume(take);
        if out.len() > limit {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "HTTP 行太长",
            ));
        }
        if newline {
            break;
        }
    }
    String::from_utf8(out)
        .map(Some)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "HTTP 头不是 UTF-8"))
}

/// 连接计数守卫：handler 退出（正常/报错/客户端断开）时减一。
struct ConnGuard {
    conns: Arc<std::sync::atomic::AtomicUsize>,
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.conns
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// 收拾上一次的残局：台账说「下载中/排队」，但这次刚起来，队列和线程都没了。
///
/// 调度队列只在内存里，所以重启后 **pending 也一定是孤儿** —— 不处理的话
/// 它会永远显示「排队」，而且幂等键一直指向这条不会动的记录。
/// 全部改成「中断」并写清为什么；返回这些任务，调用方决定要不要自动接着下。
fn reconcile(store: &Store) -> Vec<Task> {
    let mut out = Vec::new();
    let active = match store.active() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("收拾残局时读台账失败：{e}（这次就不整理了）");
            return out;
        }
    };
    for mut t in active {
        let was_queued = t.state == crate::model::STATE_PENDING;
        if t.state != crate::model::STATE_RUNNING && !was_queued {
            continue;
        }
        // 另一个进程还在跑这条（人类 CLI standalone）：别把它改成中断，
        // 更别自动 resume —— 那会和它抢同一份分片/台账行。
        if t.state == crate::model::STATE_RUNNING
            && t.pid > 0
            && t.pid != std::process::id() as i64
            && pid_alive(t.pid)
        {
            continue;
        }
        t.state = crate::model::STATE_INTERRUPTED.to_string();
        t.error = if was_queued {
            "守护进程重启过：这条还在队列里，队列没有保留 —— 重跑会接着下。".to_string()
        } else {
            "守护进程重启过：这条上次正在下，已经中断 —— 重跑会接着下。".to_string()
        };
        t.error_code = "daemon.restarted".to_string();
        t.retryable = true;
        t.retry_after_ms = 0;
        t.next_action = "继续/重试即可；已下的分片会保留。".to_string();
        t.pid = 0;
        t.updated_at = crate::model::now_ms();
        if let Err(e) = store.save(&t) {
            eprintln!("记「{}」中断失败：{e}", t.id);
            continue;
        }
        // 事件流也要留下这一笔：进程重启导致的「中断」和用户暂停
        // 必须能分得清，否则事后复盘只看到状态，不知道是谁按的停。
        let _ = store.append_event(
            &t.id,
            "interrupted",
            &serde_json::json!({
                "error": &t.error,
                "code": "daemon.restarted",
                "queued": was_queued,
                "retryable": true,
            }),
        );
        out.push(t);
    }
    if !out.is_empty() {
        eprintln!("上次没下完的 {} 条已经标成中断", out.len());
    }
    out
}

/// 这次重启是不是落在「刚崩完又崩」的循环里（十分钟内第三次以上）。
/// 是不是在"崩溃循环"里：**连续几次都活得很短**才算。
///
/// 以前只看"10 分钟内启动了几次"：运维升级时连着重启三次就会误判，
/// 于是所有在途任务都停在「中断」，还得人手 `retry`。真正的崩溃循环特征是
/// **每次启动几十秒内就死**，所以这里改成看**上一轮的存活时长**。
fn crash_looping() -> bool {
    const WINDOW_SECS: i64 = 600;
    /// 存活不足这个秒数就算"短命"（正常跑起来后重启不会命中）。
    const SHORT_RUN_SECS: i64 = 60;
    /// 连续几次短命才认定是循环。
    const SHORT_RUNS: usize = 3;

    let path = crate::paths::data_dir().join("boots.txt");
    let now = crate::model::now_ms() / 1000;
    let mut boots: Vec<i64> = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().parse::<i64>().ok())
        .filter(|t| now - t < WINDOW_SECS)
        .collect();
    boots.sort_unstable();

    // 相邻两次启动的间隔 ≈ 上一轮活了多久（最新一轮还没结束，用 now 估）。
    let mut stamps = boots.clone();
    stamps.push(now);
    let short_runs = stamps
        .windows(2)
        .rev()
        .take(SHORT_RUNS)
        .filter(|w| w[1] - w[0] < SHORT_RUN_SECS)
        .count();
    let looping = short_runs == SHORT_RUNS;

    boots.push(now);
    let text: String = boots
        .iter()
        .rev()
        .take(20)
        .map(|t| format!("{t}\n"))
        .collect();
    let _ = std::fs::create_dir_all(crate::paths::data_dir());
    let _ = std::fs::write(&path, text);
    looping
}

/// 最近一次收到请求的时刻（毫秒）。`handle` 一进来就更新，用来判断"还有没有人在用"。
static LAST_REQUEST: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

fn spawn_idle_exit(store: Arc<Store>, scheduler: Arc<Scheduler>) {
    let Some(idle) = std::env::var("SKYGET_IDLE_EXIT")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
    else {
        return;
    };
    let idle_ms = (idle as i64) * 1000;
    // 启动时刻算一次"活动"，给 agent 刚拉起就查状态的场景留出窗口。
    LAST_REQUEST.store(now_ms(), Ordering::Relaxed);
    let spawned = std::thread::Builder::new()
        .name("idle-exit".into())
        .spawn(move || loop {
            std::thread::sleep(Duration::from_secs(5));
            let last = LAST_REQUEST.load(Ordering::Relaxed);
            let quiet = last > 0 && now_ms() - last > idle_ms;
            let (running, queued) = scheduler.stats();
            if !quiet || running > 0 || queued > 0 {
                continue;
            }
            // 台账里还有活任务（比如 CLI 独立起的）就不退，免得两个写者打架。
            if store.active().map(|v| !v.is_empty()).unwrap_or(true) {
                continue;
            }
            eprintln!(
                "闲置 {} 秒、也没有在跑的任务，守护进程退出（SKYGET_IDLE_EXIT）",
                idle
            );
            // `process::exit` 不展开栈，SignalGuard/Drop 都不会跑：自己收摊。
            let _ = std::fs::remove_file(crate::uds::socket_path());
            let _ = std::fs::remove_file(crate::daemon::pid_path_for_idle());
            std::process::exit(0);
        });
    if spawned.is_err() {
        eprintln!("起不了空闲退出线程（继续跑）");
    }
}

/// 进程启动时刻，`/metrics` 的 uptime 用。
static SERVE_STARTED: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// `SKYGET_*_RETENTION_DAYS` → 毫秒；0 或负数 = 永不清理（None）。
fn retention_ms(var: &str, default_days: i64) -> Option<i64> {
    let days = std::env::var(var)
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(default_days)
        .max(0);
    (days > 0).then_some(days.saturating_mul(86_400_000))
}

fn prune_retained(
    store: &Store,
    progress_ms: Option<i64>,
    event_ms: Option<i64>,
    tomb_ms: Option<i64>,
) {
    if let Some(ms) = progress_ms {
        match store.prune_progress_events(ms) {
            Ok(n) if n > 0 => eprintln!("清理了 {n} 条过期进度事件"),
            _ => {}
        }
    }
    if let Some(ms) = event_ms {
        match store.prune_old_events(ms) {
            Ok(n) if n > 0 => eprintln!("清理了 {n} 条过期事件"),
            _ => {}
        }
    }
    if let Some(ms) = tomb_ms {
        match store.prune_tombstones(ms) {
            Ok(n) if n > 0 => eprintln!("清理了 {n} 条过期删除墓碑"),
            _ => {}
        }
    }
}

pub fn serve(port: u16) -> Result<()> {
    let store = Arc::new(Store::open()?);
    SERVE_STARTED.get_or_init(std::time::Instant::now);
    // 保留期：进度事件 30 天、其它事件 90 天、删除墓碑 7 天。
    // 0 = 永不清理。以前只在启动时清一次，daemon 连续跑几个月事件表只增不减。
    let progress_ms = retention_ms("SKYGET_PROGRESS_RETENTION_DAYS", 30);
    let event_ms = retention_ms("SKYGET_EVENT_RETENTION_DAYS", 90);
    let tomb_ms = retention_ms("SKYGET_TOMBSTONE_RETENTION_DAYS", 7);
    prune_retained(&store, progress_ms, event_ms, tomb_ms);
    {
        // 每小时一轮；维护失败只打日志，不影响请求处理。
        let store = store.clone();
        std::thread::Builder::new()
            .name("skyget-maint".into())
            .spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_secs(3600));
                prune_retained(&store, progress_ms, event_ms, tomb_ms);
            })
            .ok();
    }
    let running: Running = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    // 本机回环上"能跑程序的人本来就能跑 skyget"，所以默认不强制鉴权。
    // 但一旦把端口转发、放进容器、或让不可信 agent 调，就必须有 token：
    // 设了 SKYGET_API_TOKEN 就全接口强制 Bearer（/health 除外）。
    // trim：systemd EnvironmentFile / Docker secret 常带尾随换行，
    // 不 trim 的话 daemon 期望的 token 和客户端发的会永远差一个字节。
    let token: Option<Arc<String>> = std::env::var("SKYGET_API_TOKEN")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(Arc::new);
    // token → agent 身份。设了它就用它，旧单 token 只作为兼容回退。
    let registry = Registry::from_env()?.map(Arc::new);
    // 可选的 per-agent 能力覆盖；文件读不了/格式不对就拒绝启动（fail closed）。
    let agent_policies = AgentPolicies::from_env()?.map(Arc::new);
    // 策略按 agent 名生效；没有 token 注册表时名字只能靠请求体自称，
    // 等于谁都能挑一份更宽的策略。这种配置拒绝启动，而不是假装安全。
    if agent_policies.is_some() && registry.is_none() {
        anyhow::bail!(
            "设置了 SKYGET_AGENT_POLICIES 但没有可用的 SKYGET_AGENTS；\
             per-agent 策略必须绑定 token 身份，否则请求体里的 source 可以冒充任何 agent。"
        );
    }
    // 配额也是按 agent 名算的，同样只能靠 token 定死。没有注册表时，
    // 冒用别人的名字就能烧掉对方的额度，换一个名字还能拿一份全新额度 ——
    // 和策略一样，这种配置拒绝启动，而不是假装在限额。
    if !crate::quota::configured().is_empty() && registry.is_none() {
        anyhow::bail!(
            "设置了 SKYGET_AGENT_QUOTAS 但没有可用的 SKYGET_AGENTS；\
             配额必须绑定 token 身份，否则请求体里的 source 可以冒用或绕开额度。"
        );
    }
    // 并发上限、公平队列、有界队列。详见 scheduler.rs。
    let scheduler = Scheduler::from_env();
    if let Some(ap) = &agent_policies {
        for (source, n) in ap.concurrency_limits() {
            scheduler.set_limit(&source, n);
        }
    }
    // 空闲退出：没人连、队列空、也没活任务时，自己收摊。
    // 「随 agent 启动」必须配上这个 —— 否则 agent 一跑就把一个 daemon 留在机器上，
    // 等于换了个方式做常驻。`SKYGET_IDLE_EXIT=<秒>`，0/未设 = 不退出。
    spawn_idle_exit(store.clone(), scheduler.clone());

    // 每个连接一个线程，长轮询还会占住线程 25 秒。没有上限的话，
    // 一个失控的 agent 开 1000 个 watch 就能把进程的线程数打满。
    let conns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let max_conns: usize = std::env::var("SKYGET_MAX_CONNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(128);

    // 先把上一次留下的残局收拾干净：崩溃/断电/被 kill -9 之后，台账里会有一批
    // 状态还是「下载中」、但没有任何线程在下它们的任务。界面上就会永远显示在跑、
    // 进度不动 —— 比失败更难看懂（失败至少告诉你怎么办）。
    let orphans = reconcile(&store);

    let listener = TcpListener::bind(("127.0.0.1", port))
        .with_context(|| format!("端口 {port} 绑不上（是不是已经有一个在跑？）"))?;
    eprintln!(
        "天网盒子接口：http://127.0.0.1:{port}{}",
        if let Some(reg) = &registry {
            format!("（agent token 注册表：{} 个）", reg.len())
        } else if token.is_some() {
            "（已启用 Bearer 鉴权）".to_string()
        } else {
            String::new()
        }
    );

    // 重启后自己接着下：只捡崩溃那一刻正在跑的那几条（最多两条），
    // 而且要先确认自己不是在崩溃循环里 —— 否则会变成「崩了重启、重启就崩」。
    if !orphans.is_empty() {
        if crash_looping() {
            eprintln!(
                "十分钟内已经重启过好几次了，这次不自动接着下（{} 条等你自己点继续）",
                orphans.len()
            );
        } else {
            for t in orphans.into_iter().take(2) {
                let (code, _v) =
                    dispatch(t, &store, &running, &scheduler, true, agent_policies.as_ref());
                if code != 202 {
                    eprintln!("自动接着下没成功（{code}）");
                }
            }
        }
    }

    let ctx = Ctx {
        store,
        running,
        token,
        registry,
        agent_policies,
        scheduler,
        conns,
        max_conns,
    };

    // Unix socket 和 TCP 一起跑：老的 127.0.0.1 路径完全不变，新的 socket 提供
    // 「不用分发 token 的身份」。socket 起不来不影响 TCP（反之亦然）。
    let uds_ctx = ctx.clone();
    let _ = std::thread::Builder::new()
        .name("uds".into())
        .spawn(move || serve_uds(uds_ctx));

    for stream in listener.incoming() {
        match stream {
            Ok(s) => ctx.spawn(s, PeerId::Absent),
            Err(e) => eprintln!("接受连接失败：{e}"),
        }
    }
    Ok(())
}

/// Unix socket 监听：身份来自内核（`SO_PEERCRED`），不需要 token。
fn serve_uds(ctx: Ctx) {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};

    let path = crate::uds::socket_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // 已经有一个活着的 daemon 占着这个 socket：别抢，也别删（删了会让它的客户端失联）。
    if UnixStream::connect(&path).is_ok() {
        eprintln!(
            "unix socket 已被另一个守护进程占用：{}（这个进程只提供 TCP）",
            path.display()
        );
        return;
    }
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("unix socket 绑不上（{}）：{e}", path.display());
            return;
        }
    };
    // 0660：组决定「谁能连」。比 0666 少一类事故，比 0600 多一个可控的共享方式。
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o660));

    let me = unsafe { libc::getuid() };
    let mapped = std::env::var("SKYGET_UDS_AGENTS")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);
    eprintln!(
        "天网盒子接口（unix）：{}{}",
        path.display(),
        if mapped {
            "（uid 身份映射已启用）"
        } else {
            ""
        }
    );

    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            continue;
        };
        let peer = crate::uds::peer_of(&stream);
        let id = match peer {
            Some(p) => match crate::uds::agent_for(p) {
                // 部署方显式登记了就按登记的算 —— 单用户机器上「不用 token 也有身份」
                // 正是这条的价值。代价写在这里：同一个 uid 的进程本来就能互相代替
                // （它们能读同一份台账），真要隔离请用不同 uid。
                Some(name) => PeerId::Agent(name),
                // 没登记、但就是 daemon 自己的用户：等价于主 token，能看全部。
                None if p.uid == me => PeerId::Local,
                // 明确登记了映射却没这个 uid：拒绝，而不是悄悄降级成「无鉴权」。
                None if mapped => {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let mut s = stream;
                    let _ = write_json(
                        &mut s,
                        401,
                        serde_json::json!({
                            "error": "这个 uid 没有在 SKYGET_UDS_AGENTS 里登记",
                            "code": "auth.unknown_peer",
                        }),
                    );
                    continue;
                }
                None => PeerId::Absent,
            },
            None => PeerId::Absent,
        };
        ctx.spawn(stream, id);
    }
}

/// 一条已经建立的连接。TCP（回环）和 Unix socket 走同一套请求处理。
///
/// 只抽象「读 / 写 / 复制 / 读超时」四件事：HTTP 的解析、鉴权、路由一行都不用分叉。
trait Conn: Read + Write + Send {
    fn dup(&self) -> std::io::Result<Box<dyn Conn>>;
    fn set_read_timeout(&self, d: Option<Duration>) -> std::io::Result<()>;
    fn set_write_timeout(&self, d: Option<Duration>) -> std::io::Result<()>;
}

impl Conn for TcpStream {
    fn dup(&self) -> std::io::Result<Box<dyn Conn>> {
        Ok(Box::new(self.try_clone()?))
    }
    fn set_read_timeout(&self, d: Option<Duration>) -> std::io::Result<()> {
        TcpStream::set_read_timeout(self, d)
    }
    fn set_write_timeout(&self, d: Option<Duration>) -> std::io::Result<()> {
        TcpStream::set_write_timeout(self, d)
    }
}

impl Conn for std::os::unix::net::UnixStream {
    fn dup(&self) -> std::io::Result<Box<dyn Conn>> {
        Ok(Box::new(self.try_clone()?))
    }
    fn set_read_timeout(&self, d: Option<Duration>) -> std::io::Result<()> {
        std::os::unix::net::UnixStream::set_read_timeout(self, d)
    }
    fn set_write_timeout(&self, d: Option<Duration>) -> std::io::Result<()> {
        std::os::unix::net::UnixStream::set_write_timeout(self, d)
    }
}

/// 调用方身份：谁在连我。
///
/// TCP 上只有一个答案（`Absent`，只能靠 bearer token）；Unix socket 上内核会告诉我们
/// 对端 uid —— 那是没法伪造的，所以同 uid 等价于主 token，登记的 uid 得到 agent 身份。
#[derive(Clone, Debug)]
enum PeerId {
    /// 没有内核层面的身份（TCP，或拿不到 peer cred）
    Absent,
    /// daemon 自己的 uid：等价于主 token（本来就能读整本台账）
    Local,
    /// 在 `SKYGET_UDS_AGENTS` 里登记的 agent
    Agent(String),
}

/// 请求处理需要的共享状态。两个监听器（TCP、UDS）共用同一份。
#[derive(Clone)]
struct Ctx {
    store: Arc<Store>,
    running: Running,
    token: Option<Arc<String>>,
    registry: Option<Arc<Registry>>,
    agent_policies: Option<Arc<AgentPolicies>>,
    scheduler: Arc<Scheduler>,
    conns: Arc<std::sync::atomic::AtomicUsize>,
    max_conns: usize,
}

impl Ctx {
    /// 起一个请求线程。连接数满了就明确回 503，而不是让线程数无限涨。
    fn spawn<C: Conn + 'static>(&self, mut conn: C, peer: PeerId) {
        use std::sync::atomic::Ordering;
        let now = self.conns.fetch_add(1, Ordering::Relaxed) + 1;
        if now > self.max_conns {
            self.conns.fetch_sub(1, Ordering::Relaxed);
            let _ = write_json(
                &mut conn,
                503,
                serde_json::json!({
                    "error": "连接数已满", "code": "server.busy",
                    "retryable": true, "retry_after_ms": 1000,
                }),
            );
            return;
        }
        let guard = ConnGuard { conns: self.conns.clone() };
        let store = self.store.clone();
        let running = self.running.clone();
        let token = self.token.clone();
        let registry = self.registry.clone();
        let agent_policies = self.agent_policies.clone();
        let scheduler = self.scheduler.clone();
        // 小栈：请求处理不递归、不跑下载；长轮询占用的是等待时间，不是栈。
        let spawned = std::thread::Builder::new()
            .stack_size(512 * 1024)
            .spawn(move || {
                let _guard = guard;
                if let Err(e) = handle(
                    Box::new(conn),
                    peer,
                    store,
                    running,
                    token,
                    registry,
                    agent_policies,
                    scheduler,
                ) {
                    eprintln!("请求处理失败：{e:#}");
                }
            });
        if spawned.is_err() {
            eprintln!("起不了请求线程（连接数太多）");
        }
    }
}

fn handle(
    mut s: Box<dyn Conn>,
    peer: PeerId,
    store: Arc<Store>,
    running: Running,
    token: Option<Arc<String>>,
    registry: Option<Arc<Registry>>,
    agent_policies: Option<Arc<AgentPolicies>>,
    scheduler: Arc<Scheduler>,
) -> Result<()> {
    LAST_REQUEST.store(now_ms(), Ordering::Relaxed);
    s.set_read_timeout(Some(Duration::from_secs(15))).ok();
    let mut reader = BufReader::new(s.dup()?);

    let line = match read_line_limited(&mut reader, MAX_HTTP_LINE) {
        Ok(Some(l)) => l,
        Ok(None) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
            write_json(
                &mut s,
                431,
                serde_json::json!({"error": "请求行太长", "code": "http.line_too_large"}),
            )?;
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("/").to_string();

    // 读头，拿 Content-Length、Authorization、Content-Type、Origin
    let mut len = 0usize;
    let mut auth: Option<String> = None;
    let mut ctype = String::new();
    let mut browser_origin = false;
    let mut range: Option<String> = None;
    let mut header_count = 0usize;
    loop {
        let h = match read_line_limited(&mut reader, MAX_HTTP_LINE) {
            Ok(Some(h)) => h,
            Ok(None) => break,
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                write_json(
                    &mut s,
                    431,
                    serde_json::json!({"error": "请求头太长", "code": "http.header_too_large"}),
                )?;
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        if h == "\r\n" || h == "\n" {
            break;
        }
        header_count += 1;
        if header_count > MAX_HTTP_HEADERS {
            write_json(
                &mut s,
                431,
                serde_json::json!({"error": "请求头太多", "code": "http.too_many_headers"}),
            )?;
            return Ok(());
        }
        // 头名大小写不敏感，但头的**值**必须原样保留 —— 之前把整行
        // 转小写再比，`Bearer` 变成 `bearer`，token 永远对不上。
        if let Some((name, value)) = h.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            if name == "content-length" {
                len = value.trim().parse().unwrap_or(0);
            }
            if name == "authorization" {
                auth = Some(value.trim().to_string());
            }
            if name == "content-type" {
                ctype = value.trim().to_ascii_lowercase();
            }
            if name == "origin" {
                browser_origin = true;
            }
            if name == "range" {
                range = Some(value.trim().to_string());
            }
        }
    }

    // 浏览器防线：网页可以往 127.0.0.1 发「简单请求」，不需要读响应，
    // 副作用就已经发生了（借 daemon 当 SSRF/写文件代理）。两条一起用：
    // 1. 带 Origin 的一律拒绝（curl/CLI/MCP 不发这个头）；
    // 2. POST 必须是 application/json —— 跨源 JSON 会触发预检，
    //    而本服务不答 CORS，浏览器就发不出真正的请求。
    if browser_origin {
        crate::metrics::deny(crate::metrics::Kind::Policy);
        write_json(
            &mut s,
            403,
            serde_json::json!({
                "error": "不接受浏览器来源的请求", "code": "policy.browser_origin",
            }),
        )?;
        return Ok(());
    }
    if method == "POST" && len > 0 && !ctype.starts_with("application/json") {
        crate::metrics::deny(crate::metrics::Kind::Policy);
        write_json(
            &mut s,
            415,
            serde_json::json!({
                "error": "POST 的 Content-Type 必须是 application/json",
                "code": "input.content_type",
            }),
        )?;
        return Ok(());
    }

    // /health 不鉴权：界面/监控要先能问"活着吗"，而它不泄露任何任务信息。
    let clean_path = path.split('?').next().unwrap_or(&path);
    let mut auth_agent: Option<String> = None;
    // 内核给的身份比任何 self-claim 都硬：Unix socket 上的 uid 不能被请求体伪造。
    let peer_authorized = match &peer {
        PeerId::Local => true,
        PeerId::Agent(name) => {
            auth_agent = Some(name.clone());
            true
        }
        PeerId::Absent => false,
    };
    if clean_path != "/health" {
        let bearer = auth
            .as_deref()
            .and_then(|h| h.strip_prefix("Bearer "))
            .unwrap_or("");
        // agent token → 身份；`SKYGET_API_TOKEN` 是主 token（人类 GUI / 运维），
        // 即使启用了 per-agent 注册表也仍然有效，但**不**冒充任何 agent。
        // 以前 registry 一开主 token 就失效，GUI 和监控会被锁在门外。
        let mut authorized = peer_authorized;
        if let Some(reg) = &registry {
            if let Some(name) = reg.name_for(bearer) {
                auth_agent = Some(name.to_string());
                authorized = true;
            }
        }
        if !authorized {
            if let Some(want) = token.as_deref() {
                authorized = auth.as_deref() == Some(format!("Bearer {want}").as_str());
            }
        }
        // 既没有 registry 也没有主 token = 本机无鉴权（兼容旧行为）。
        if !authorized && (registry.is_some() || token.is_some()) {
            let payload = serde_json::json!({
                "error": "需要有效的 Bearer token（agent token 见 SKYGET_AGENTS，主 token 见 SKYGET_API_TOKEN）",
                "code": "auth.required",
            });
            crate::metrics::deny(crate::metrics::Kind::Auth);
            write_json(&mut s, 401, payload)?;
            return Ok(());
        }
    }

    // 内容缓存直读：局域网/容器从宿主缓存取，不重复走公网。支持 Range。
    // 放在鉴权之后：没 token 的进程不能把缓存当公共文件服务器。
    if method == "GET" && clean_path.starts_with("/cas/") {
        // 默认只发字节（46% 的命中走这条路，每次都重新哈希等于把秒传变慢）。
        // 但坏块会被所有 agent 无差别吃掉，所以给一个显式的校验开关。
        let verify = path.split_once('?').map(|(_, q)| {
            q.split('&').any(|kv| kv == "verify=1" || kv == "verify=true")
        }).unwrap_or(false)
            || std::env::var("SKYGET_VERIFY_CAS").map(|v| v == "1").unwrap_or(false);
        return serve_cas(&mut *s, clean_path, range.as_deref(), verify);
    }
    // Prometheus 文本指标；同样在鉴权之后。
    if method == "GET" && clean_path == "/metrics" {
        return serve_metrics(&mut *s, &store, &scheduler, registry.as_deref());
    }
    // 上限 1MiB：超过就明确拒绝。以前是截断到 1MiB 继续解析 —— 收到的 JSON 少一截，
    // 报出来的错还指向「缺字段」，等于让人去查一个不存在的 bug。
    const MAX_BODY: usize = 1 << 20;
    if len > MAX_BODY {
        let payload = serde_json::json!({
            "error": format!("请求体太大：{len} 字节，上限 {MAX_BODY}"),
        });
        write_json(&mut s, 413, payload)?;
        return Ok(());
    }
    let mut body = vec![0u8; len];
    if len > 0 {
        reader.read_exact(&mut body)?;
    }

    let (code, payload) = route_as(
        &method,
        &path,
        &body,
        &store,
        &running,
        &scheduler,
        auth_agent.as_deref(),
        agent_policies.as_ref(),
        registry.as_deref(),
    );
    note_response_denial(code, &payload);
    write_json(&mut s, code, payload)
}

/// 把 HTTP 面的拒绝响应归到一个固定标签；具体原因仍在响应体里。
///
/// 只统计 401/403：400/404/429 大多是输入或容量问题，不是安全边界事件。
fn note_response_denial(code: u16, payload: &serde_json::Value) {
    use crate::metrics::{deny, Kind};
    let c = payload.get("code").and_then(|v| v.as_str()).unwrap_or("");
    match code {
        401 => deny(Kind::Auth),
        _ if c == "quota.exceeded" => deny(Kind::Quota),
        403 if c.starts_with("auth.") => deny(Kind::Auth),
        403 if c.starts_with("policy.credential") => deny(Kind::Credential),
        403 => deny(Kind::Policy),
        _ => {}
    }
}

/// Prometheus label 值转义：来源名是调用方给的，可能带引号/反斜杠。
fn prom_label(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

/// 来源标签收敛：只给内置来源和**注册表里的 agent** 单独建时间序列，
/// 其它自称来源统一进 `other`。否则任何调用方用随机 `source` 提交任务，
/// 都能让 Prometheus 基数无限膨胀。
fn metric_source(source: &str, known: &std::collections::HashSet<String>) -> String {
    const BUILTIN: &[&str] = &["human", "cli", "api", "manifest"];
    if BUILTIN.contains(&source) || known.contains(source) {
        source.to_string()
    } else {
        "other".to_string()
    }
}

/// `GET /metrics`：Prometheus 文本格式。基础设施团队接监控用。
fn serve_metrics(
    s: &mut dyn Conn,
    store: &Arc<Store>,
    scheduler: &Arc<Scheduler>,
    registry: Option<&Registry>,
) -> Result<()> {
    let st = match crate::stats::collect(store) {
        Ok(st) => st,
        Err(e) => {
            write_json(
                s,
                500,
                serde_json::json!({"error": format!("{e:#}")}),
            )?;
            return Ok(());
        }
    };
    let (running, queued) = scheduler.stats();
    let (max_running, max_queued) = scheduler.capacity();
    let events = store.event_count().unwrap_or(0);
    let pins = store.pins().map(|p| p.len()).unwrap_or(0);
    let tombstones = store.tombstone_count().unwrap_or(0);
    let uptime = SERVE_STARTED
        .get()
        .map(|t| t.elapsed().as_secs())
        .unwrap_or(0);

    let mut out = String::new();
    out.push_str("# HELP skyget_up 守护进程活着\n# TYPE skyget_up gauge\nskyget_up 1\n");
    out.push_str(&format!(
        "# HELP skyget_build_info 构建版本\n# TYPE skyget_build_info gauge\nskyget_build_info{{version=\"{}\"}} 1\n",
        crate::version()
    ));
    out.push_str(&format!("skyget_uptime_seconds {uptime}\n"));
    out.push_str("# HELP skyget_jobs 调度器里的任务\n# TYPE skyget_jobs gauge\n");
    out.push_str(&format!("skyget_jobs{{state=\"running\"}} {running}\n"));
    out.push_str(&format!("skyget_jobs{{state=\"queued\"}} {queued}\n"));
    for (state, n) in &st.by_state {
        out.push_str(&format!("skyget_jobs_total{{state=\"{state}\"}} {n}\n"));
    }
    out.push_str("# HELP skyget_scheduler_capacity 调度器上限\n# TYPE skyget_scheduler_capacity gauge\n");
    out.push_str(&format!(
        "skyget_scheduler_capacity{{kind=\"running\"}} {max_running}\n"
    ));
    out.push_str(&format!(
        "skyget_scheduler_capacity{{kind=\"queued\"}} {max_queued}\n"
    ));
    out.push_str("# HELP skyget_sources 台账里的来源数\n# TYPE skyget_sources gauge\n");
    out.push_str(&format!("skyget_sources {}\n", st.by_source.len()));
    // 按来源拆开：公平队列/配额出问题时，先看是谁在占。
    // 标签先收敛，避免调用方用随机 source 制造无限时间序列。
    let known: std::collections::HashSet<String> = registry
        .map(|r| {
            r.names()
                .into_iter()
                .map(|n| format!("agent:{n}"))
                .collect()
        })
        .unwrap_or_default();
    let mut jobs_by: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    for (source, n) in &st.by_source {
        *jobs_by.entry(metric_source(source, &known)).or_default() += n;
    }
    let mut running_by: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    for (source, n) in scheduler.running_by_source() {
        *running_by.entry(metric_source(&source, &known)).or_default() += n;
    }
    out.push_str("# HELP skyget_jobs_by_source 每个来源在台账里的任务数\n# TYPE skyget_jobs_by_source gauge\n");
    for (source, n) in &jobs_by {
        out.push_str(&format!(
            "skyget_jobs_by_source{{source=\"{}\"}} {n}\n",
            prom_label(source)
        ));
    }
    out.push_str("# HELP skyget_running_by_source 每个来源正在跑的任务数\n# TYPE skyget_running_by_source gauge\n");
    for (source, n) in &running_by {
        out.push_str(&format!(
            "skyget_running_by_source{{source=\"{}\"}} {n}\n",
            prom_label(source)
        ));
    }
    let quotas = crate::quota::configured();
    out.push_str("# HELP skyget_agent_quota_bytes 每 agent 每日字节上限\n# TYPE skyget_agent_quota_bytes gauge\n");
    for (name, limit) in &quotas {
        if let Some(limit) = limit {
            out.push_str(&format!(
                "skyget_agent_quota_bytes{{agent=\"{}\"}} {limit}\n",
                prom_label(name)
            ));
        }
    }
    out.push_str("# HELP skyget_agent_used_today_bytes 每 agent 今日已完成字节\n# TYPE skyget_agent_used_today_bytes gauge\n");
    for (name, _) in &quotas {
        let used = crate::quota::used_today(store, &format!("agent:{name}")).unwrap_or(0);
        out.push_str(&format!(
            "skyget_agent_used_today_bytes{{agent=\"{}\"}} {used}\n",
            prom_label(name)
        ));
    }
    out.push_str("# HELP skyget_failures_total 按 error_code 统计的失败任务\n# TYPE skyget_failures_total gauge\n");
    for (code, n) in &st.failures_by_code {
        out.push_str(&format!(
            "skyget_failures_total{{code=\"{}\"}} {n}\n",
            prom_label(code)
        ));
    }
    out.push_str("# HELP skyget_cache_bytes 内容缓存占用\n# TYPE skyget_cache_bytes gauge\n");
    out.push_str(&format!("skyget_cache_bytes {}\n", st.blob_bytes));
    out.push_str(&format!("skyget_cache_blobs {}\n", st.blobs));
    out.push_str("# HELP skyget_download_bytes_total 完成下载的总字节\n# TYPE skyget_download_bytes_total counter\n");
    out.push_str(&format!("skyget_download_bytes_total {}\n", st.done_bytes));
    out.push_str("# HELP skyget_cache_saved_bytes_total 秒传省下的字节\n# TYPE skyget_cache_saved_bytes_total counter\n");
    out.push_str(&format!("skyget_cache_saved_bytes_total {}\n", st.cached_bytes));
    out.push_str(&format!("skyget_events_total {events}\n"));
    out.push_str(&format!("skyget_pins {pins}\n"));
    out.push_str(&format!("skyget_tombstones {tombstones}\n"));
    crate::metrics::render(&mut out);

    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        out.len(),
        out
    );
    s.write_all(resp.as_bytes())?;
    s.flush().ok();
    Ok(())
}

/// `GET /cas/<sha>`：从内容缓存直读，支持单段 Range。
///
/// 这是“缓存变成共享资产”的入口：容器/局域网另一台机器拿同一个 token
/// 就能从这里取，不必再走公网。只读、只认 64 位十六进制，路径不可穿越。
fn serve_cas(s: &mut dyn Conn, path: &str, range: Option<&str>, verify: bool) -> Result<()> {
    let sha = path
        .trim_start_matches("/cas/")
        .trim_end_matches('/')
        .to_ascii_lowercase();
    if !valid_sha(&sha) {
        write_json(
            s,
            400,
            serde_json::json!({
                "error": "sha256 必须是 64 位十六进制", "code": "input.bad_sha256",
            }),
        )?;
        return Ok(());
    }
    let blob = crate::paths::blobs_dir().join(&sha[..2]).join(&sha);
    // symlink_metadata：不接受符号链接块（否则 /cas 会变成任意文件读取）。
    let meta = match std::fs::symlink_metadata(&blob) {
        Ok(m) if m.is_file() => m,
        _ => {
            write_json(
                s,
                404,
                serde_json::json!({"error": "缓存里没有这份内容", "code": "cas.not_found"}),
            )?;
            return Ok(());
        }
    };
    // 校验在读之前：宁可给一个 409，也不要把一份对不上的字节发出去。
    if verify {
        match crate::engine::sha256_file(&blob) {
            Ok(got) if got == sha => {}
            Ok(got) => {
                write_json(
                    s,
                    409,
                    serde_json::json!({
                        "error": "缓存块校验不通过：内容与文件名对不上，已拒绝发送。重新下一次即可。",
                        "code": "cas.corrupt",
                        "expected": sha,
                        "actual": got,
                    }),
                )?;
                return Ok(());
            }
            Err(e) => {
                write_json(
                    s,
                    500,
                    serde_json::json!({
                        "error": format!("读缓存块失败：{e:#}"), "code": "cas.read_failed",
                    }),
                )?;
                return Ok(());
            }
        }
    }
    let size = meta.len();
    let (code, start, end) = match range {
        None => (200u16, 0u64, size.saturating_sub(1)),
        Some(h) => match parse_range(h, size) {
            Some((a, b)) => (206, a, b),
            None => {
                let resp = format!(
                    "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{size}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                s.write_all(resp.as_bytes())?;
                return Ok(());
            }
        },
    };
    let len = if size == 0 { 0 } else { end - start + 1 };
    let mut f = std::fs::File::open(&blob)
        .with_context(|| format!("打开缓存块失败：{}", blob.display()))?;
    use std::io::Seek;
    f.seek(std::io::SeekFrom::Start(start))?;

    let head = if code == 206 {
        format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Type: application/octet-stream\r\nContent-Length: {len}\r\nContent-Range: bytes {start}-{end}/{size}\r\nAccept-Ranges: bytes\r\nETag: \"{sha}\"\r\nConnection: close\r\n\r\n"
        )
    } else {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {len}\r\nAccept-Ranges: bytes\r\nETag: \"{sha}\"\r\nConnection: close\r\n\r\n"
        )
    };
    s.write_all(head.as_bytes())?;
    s.set_write_timeout(Some(Duration::from_secs(120))).ok();
    // 流式拷贝，不把大文件读进内存。
    std::io::copy(&mut f.take(len), s)?;
    s.flush().ok();
    Ok(())
}

/// 单段 Range：`bytes=a-b` / `bytes=a-` / `bytes=-n`。多段直接拒绝（416）。
fn parse_range(h: &str, size: u64) -> Option<(u64, u64)> {
    if size == 0 {
        return None;
    }
    let v = h.trim().strip_prefix("bytes=")?;
    if v.contains(',') {
        return None;
    }
    let (a, b) = v.split_once('-')?;
    if a.trim().is_empty() {
        let n: u64 = b.trim().parse().ok()?;
        if n == 0 {
            return None;
        }
        let n = n.min(size);
        return Some((size - n, size - 1));
    }
    let start: u64 = a.trim().parse().ok()?;
    let end: u64 = if b.trim().is_empty() {
        size.saturating_sub(1)
    } else {
        b.trim().parse().ok()?
    };
    if start >= size || start > end {
        return None;
    }
    Some((start, end.min(size - 1)))
}

/// 状态码要有说人话的 reason：202 写成 "Error" 会让调用方以为失败了。
fn write_json<W: Write + ?Sized>(s: &mut W, code: u16, payload: serde_json::Value) -> Result<()> {
    let body_txt = payload.to_string();
    let reason = match code {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let resp = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body_txt}",
        body_txt.as_bytes().len(),
    );
    s.write_all(resp.as_bytes())?;
    s.flush().ok();
    Ok(())
}

/// 测试/无身份调用的入口。
#[cfg(test)]
fn route(
    method: &str,
    path: &str,
    body: &[u8],
    store: &Arc<Store>,
    running: &Running,
    scheduler: &Arc<Scheduler>,
) -> (u16, serde_json::Value) {
    route_as(method, path, body, store, running, scheduler, None, None, None)
}

/// `agent` = 从 token 解析出的身份；有值时覆盖请求体里的 `source`。
fn route_as(
    method: &str,
    path: &str,
    body: &[u8],
    store: &Arc<Store>,
    running: &Running,
    scheduler: &Arc<Scheduler>,
    agent: Option<&str>,
    agent_policies: Option<&Arc<AgentPolicies>>,
    registry: Option<&Registry>,
) -> (u16, serde_json::Value) {
    // 查询串不能先切掉：since / limit 都在问号后面。
    // 之前这里先 split('?') 再匹配，于是 `?since=…` 永远读不到，增量退化成全量。
    let full = path.to_string();
    let path = path.split('?').next().unwrap_or(path);
    match (method, path) {
        // 能力自述：宿主不必读 skill 就能知道这个 daemon 能干什么。
        // 「没有 shell 的 agent」也能先探测再决定（见 skill/REFERENCE.md）。
        ("GET", "/health") => (
            200,
            serde_json::json!({
                "ok": true,
                "version": crate::version(),
                "capabilities": crate::sdk::capabilities(),
            }),
        ),
        // 给 TUI 插件/看板轮询的单任务视图：不必拉 500 条 /jobs 再自己挑。
        ("GET", "/statusline") => statusline_view(store, agent, query(&full, "source").as_deref()),
        ("GET", "/pins") => match store.pins() {
            Ok(pins) => {
                let pins: Vec<_> = pins
                    .into_iter()
                    .filter(|p| match agent {
                        Some(name) => p.agent == format!("agent:{name}"),
                        None => true,
                    })
                    .collect();
                (200, serde_json::json!(pins))
            }
            Err(e) => (500, serde_json::json!({"error": format!("{e:#}")})),
        },
        ("POST", "/pins") => pin_create(body, store, agent),
        (m, p) if m == "DELETE" && p.starts_with("/pins/") => {
            pin_delete(&full, &p["/pins/".len()..], store, agent)
        }
        ("GET", "/agents") => agents_view(store, scheduler, registry, agent_policies, agent),
        ("GET", "/jobs/changes") => {
            if agent.is_some() {
                (
                    403,
                    serde_json::json!({
                        "error": "带 token 的客户端不能用全量 changes；请用 /jobs 或 /jobs/<id>。",
                        "code": "auth.scoped_endpoint",
                    }),
                )
            } else {
                changes(&full, store)
            }
        }
        // 事件流必须排在 `/jobs/<id>` 前面：否则 `/jobs/x/events` 会被当成 id=x/events。
        (m, p) if m == "GET" && p.starts_with("/jobs/") && p.ends_with("/events") => {
            let id = p
                .trim_start_matches("/jobs/")
                .trim_end_matches("/events")
                .trim_end_matches('/');
            match store.get(id) {
                Ok(Some(t)) if owns(agent, &t) => events(&full, id, store),
                Ok(Some(_)) => job_not_found(),
                // 记录已删但事件保留（cancel/delete 的审计线索）。只有主 token /
                // 本机无身份能读：per-agent token 没有记录可用于归属校验，按 404。
                Ok(None) if agent.is_none() => events(&full, id, store),
                Ok(None) => job_not_found(),
                Err(e) => (500, serde_json::json!({"error": format!("{e:#}")})),
            }
        }
        // HLS/DASH 的进度是“秒”，不是字节；状态栏用它画条。
        (m, p) if m == "GET" && p.starts_with("/jobs/") && p.ends_with("/seconds") => {
            let id = p
                .trim_start_matches("/jobs/")
                .trim_end_matches("/seconds")
                .trim_end_matches('/');
            seconds_progress(id, store, agent)
        }
        ("GET", "/jobs") => {
            // `active=1`：只查活跃状态，长任务不会被最近几百条短任务挤出窗口。
            let active_only = query(&full, "active").as_deref() == Some("1");
            let listed = if active_only {
                store.active_tasks(jobs_limit(&full))
            } else {
                store.list(jobs_limit(&full))
            };
            match listed {
                Ok(list) => {
                    let views: Vec<_> = list
                        .iter()
                        .filter(|t| owns(agent, t))
                        .map(|t| t.view(t.cached, 0))
                        .collect();
                    (200, serde_json::json!(views))
                }
                Err(e) => (500, serde_json::json!({"error": format!("{e:#}")})),
            }
        }
        (m, p) if m == "GET" && p.starts_with("/jobs/") => {
            let id = &p["/jobs/".len()..];
            match store.get(id) {
                // cached 是台账里的事实，不是调用方猜的：接口要如实带出去。
                // 以前这里写死 false，第二个命中缓存的任务在接口里看起来像重新下了一遍。
                Ok(Some(t)) if owns(agent, &t) => {
                    (200, serde_json::json!(t.view(t.cached, 0)))
                }
                Ok(Some(_)) => job_not_found(),
                Ok(None) => (404, serde_json::json!({"error": "没有这个任务"})),
                Err(e) => (500, serde_json::json!({"error": format!("{e:#}")})),
            }
        }
        // 预检：不入队、不联网，只回答「这条提交会被怎么判」。
        ("POST", "/check") => check(body, agent, agent_policies, registry),
        ("POST", "/jobs") => submit(
            body,
            store,
            running,
            scheduler,
            agent,
            agent_policies,
            registry,
        ),
        ("POST", "/uploads") => upload(
            body,
            store,
            running,
            scheduler,
            agent,
            agent_policies,
            registry,
        ),
        // 传输控制：跟界面上那五个按钮一一对应，别人（脚本、别的 agent）也要能按
        (m, p) if m == "POST" && p.ends_with("/pause") => {
            let by = actor(agent);
            with_id(p, "/pause", |id| pause(id, store, running, agent, &by))
        }
        (m, p) if m == "POST" && p.ends_with("/resume") => {
            let by = actor(agent);
            with_id(p, "/resume", |id| {
                resume(id, store, running, scheduler, agent_policies, agent, &by)
            })
        }
        // 失败任务的重跑：语义和 resume 一样（同一记录、同一期望 hash），
        // 单独留一个名字是给 agent 的意图用 —— “重试失败”比“继续中断”好懂。
        (m, p) if m == "POST" && p.ends_with("/retry") => {
            let by = actor(agent);
            with_id(p, "/retry", |id| {
                resume(id, store, running, scheduler, agent_policies, agent, &by)
            })
        }
        (m, p) if m == "POST" && p.ends_with("/cancel") => {
            let by = actor(agent);
            with_id(p, "/cancel", |id| cancel(id, store, running, agent, &by))
        }
        (m, p) if m == "DELETE" && p.starts_with("/jobs/") => {
            with_id(p, "", |id| forget(id, store, running, agent))
        }
        _ => (
            404,
            serde_json::json!({"error": "不认识这个路径", "code": "http.not_found"}),
        ),
    }
}

/// `GET /agents`：这台 daemon 上“谁在用、能用到哪、现在跑几个”。
///
/// 故意不返回 token —— 身份是 daemon 从 token 解析出来的，token 本身不出任何接口。
/// 用 per-agent token 调用时只看得到自己；旧单 token / 无鉴权（本机）能看到全部。
fn agents_view(
    store: &Arc<Store>,
    scheduler: &Arc<Scheduler>,
    registry: Option<&Registry>,
    agent_policies: Option<&Arc<AgentPolicies>>,
    caller: Option<&str>,
) -> (u16, serde_json::Value) {
    let counts = match store.source_counts() {
        Ok(v) => v,
        Err(e) => return (500, serde_json::json!({"error": format!("{e:#}")})),
    };
    let running: std::collections::HashMap<String, usize> =
        scheduler.running_by_source().into_iter().collect();

    let mut names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    if let Some(r) = registry {
        names.extend(r.names());
    }
    if let Some(ap) = agent_policies {
        names.extend(ap.names());
    }
    // 台账里的来源也列出来：token 轮换/注册表被删掉之后，历史任务仍然要能解释。
    for (src, _) in &counts {
        if let Some(name) = src.strip_prefix("agent:") {
            names.insert(name.to_string());
        }
    }
    if let Some(only) = caller {
        names.retain(|n| n == only);
    }

    let base = crate::security::Policy::strict();
    let agents: Vec<serde_json::Value> = names
        .into_iter()
        .map(|name| {
            let source = format!("agent:{name}");
            let p = agent_policies
                .map(|ap| ap.for_agent(&name, &base))
                .unwrap_or_else(|| base.clone());
            let jobs = counts
                .iter()
                .find(|(s, _)| *s == source)
                .map(|(_, n)| *n)
                .unwrap_or(0);
            serde_json::json!({
                "name": name,
                "source": source,
                "jobs": jobs,
                "running": running.get(&source).copied().unwrap_or(0),
                "max_running": scheduler.limit_of(&source),
                "quota_bytes": crate::quota::limit_for(&source),
                "used_today": crate::quota::used_today(store, &source).unwrap_or(0),
                "policy": {
                    "dest_roots": p.dest_roots.iter().map(|x| x.display().to_string()).collect::<Vec<_>>(),
                    "allowed_hosts": p.allowed_hosts,
                    "upload_roots": p.upload_roots.iter().map(|x| x.display().to_string()).collect::<Vec<_>>(),
                    "allow_private": p.allow_private,
                },
            })
        })
        .collect();
    // daemon 的策略（per-agent 未覆盖时继承全局）。doctor / cockpit 需要知道
    // “daemon 实际认的上传根/落地根”，而不是自己 shell 里的环境变量。
    // agent token 只暴露它自己的有效策略，不把别人的根路径/白名单带出去。
    let exposed = match (caller, agent_policies) {
        (Some(name), Some(ap)) => ap.for_agent(name, &base),
        _ => base.clone(),
    };
    let base_policy = serde_json::json!({
        "dest_roots": exposed.dest_roots.iter().map(|x| x.display().to_string()).collect::<Vec<_>>(),
        "allowed_hosts": exposed.allowed_hosts,
        "upload_roots": exposed.upload_roots.iter().map(|x| x.display().to_string()).collect::<Vec<_>>(),
        "allow_private": exposed.allow_private,
    });
    let (max_running, max_queued) = scheduler.capacity();
    let (running_now, queued) = scheduler.stats();
    (
        200,
        serde_json::json!({
            "registry": registry.is_some(),
            "caller": caller.map(|a| format!("agent:{a}")),
            "policy": base_policy,
            "scheduler": {
                "max_running": max_running,
                "max_queued": max_queued,
                "running": running_now,
                "queued": queued,
            },
            "agents": agents,
        }),
    )
}

/// 增量：`GET /jobs/changes?since=<ms>`。
///
/// 回 `{"now": 时间戳, "items": [变过的], "deleted": [删掉的 id]}`。
/// 客户端下次把 `now` 当 `since` 带回来；`since=0` 等于全量（断线重连时用）。
fn changes(path: &str, store: &Arc<Store>) -> (u16, serde_json::Value) {
    let since = path
        .split("since=")
        .nth(1)
        .and_then(|v| v.split('&').next())
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0)
        .max(0);

    // since=0 = 首次全量：要的是**最近的** 500 条，不是最旧的 500 条。
    // 游标用这批里最大的 updated_at，而不是当前时间 —— 后者在批量变动
    // 超过 500 条时会把没返回的行永久跳过。
    if since == 0 {
        return match store.list(500) {
            Ok(list) => {
                let now = list.iter().map(|t| t.updated_at).max().unwrap_or(0);
                let items: Vec<_> = list.iter().map(|t| t.view(t.cached, 0)).collect();
                (
                    200,
                    serde_json::json!({"now": now, "items": items, "deleted": []}),
                )
            }
            Err(e) => (500, serde_json::json!({"error": format!("{e:#}")})),
        };
    }

    match store.since(since, 500) {
        Ok(list) => {
            let items: Vec<_> = list.iter().map(|t| t.view(t.cached, 0)).collect();
            // 墓碑从台账读，不从进程内 Vec 读：daemon 重启后删除仍然可见。
            let deleted_pairs: Vec<(String, i64)> =
                store.tombstones_since(since, 500).unwrap_or_default();
            let deleted: Vec<String> = deleted_pairs.iter().map(|(id, _)| id.clone()).collect();
            let task_now = list.last().map(|t| t.updated_at).unwrap_or(since);
            let tomb_now = deleted_pairs.iter().map(|(_, ts)| *ts).max().unwrap_or(since);
            // 任务列表被 LIMIT 截断时，游标只能停在第 500 条上；
            // 否则剩下的任务会被墓碑时间戳带着一起跳过。
            let now = if list.len() >= 500 {
                task_now
            } else {
                task_now.max(tomb_now)
            };
            (
                200,
                serde_json::json!({"now": now, "items": items, "deleted": deleted}),
            )
        }
        Err(e) => (500, serde_json::json!({"error": format!("{e:#}")})),
    }
}

#[derive(Deserialize)]
struct NewPin {
    sha256: String,
    #[serde(default)]
    agent: String,
    /// 不填 = 7 天；0 = 不过期（直到显式 unpin）
    ttl_secs: Option<u64>,
    #[serde(default)]
    note: String,
}

fn valid_sha(s: &str) -> bool {
    crate::security::is_sha256_hex(s)
}

/// 钉住一份缓存内容。agent 用它表达「这个模型我在用，prune 别清」。
fn pin_create(
    body: &[u8],
    store: &Arc<Store>,
    agent: Option<&str>,
) -> (u16, serde_json::Value) {
    let p: NewPin = match serde_json::from_slice(body) {
        Ok(p) => p,
        Err(e) => {
            return (
                400,
                serde_json::json!({"error": format!("请求体要是 JSON：{e}")}),
            )
        }
    };
    let sha = p.sha256.trim().to_ascii_lowercase();
    if !valid_sha(&sha) {
        return (
            400,
            serde_json::json!({
                "error": "sha256 必须是 64 位十六进制", "code": "input.bad_sha256",
            }),
        );
    }
    // 有 token 时身份以 token 为准，请求体里的 agent 只是自称。
    let agent = match agent {
        Some(name) => format!("agent:{name}"),
        None if p.agent.trim().is_empty() => "api".to_string(),
        None => p.agent.trim().to_string(),
    };
    // 默认 7 天，避免 agent 忘了 unpin 把缓存永久钉死；0 才是「直到显式解除」。
    let ttl = p.ttl_secs.unwrap_or(7 * 86_400);
    match store.pin(&sha, &agent, ttl, &p.note) {
        Ok(pin) => (201, serde_json::json!(pin)),
        Err(e) => (500, serde_json::json!({"error": format!("{e:#}")})),
    }
}

fn pin_delete(
    full: &str,
    sha: &str,
    store: &Arc<Store>,
    caller: Option<&str>,
) -> (u16, serde_json::Value) {
    let sha = sha.trim().trim_end_matches('/').to_ascii_lowercase();
    if !valid_sha(&sha) {
        return (
            400,
            serde_json::json!({
                "error": "sha256 必须是 64 位十六进制", "code": "input.bad_sha256",
            }),
        );
    }
    // 有 token 时只能解自己的 pin；`?agent=` 和“解全部”都不再可用。
    let agent = match caller {
        Some(name) => format!("agent:{name}"),
        None => query(full, "agent").unwrap_or_default(),
    };
    let (removed, all) = if agent.trim().is_empty() {
        (store.unpin_all(&sha).unwrap_or(false), true)
    } else {
        (store.unpin(&sha, &agent).unwrap_or(false), false)
    };
    if removed {
        (
            200,
            serde_json::json!({"sha256": sha, "unpinned": true, "all_agents": all}),
        )
    } else {
        (404, serde_json::json!({"error": "没有这条 pin"}))
    }
}

/// `GET /jobs/<id>/events?since=<seq>&wait_ms=<n>&limit=<n>`。
///
/// `since` = 上次读到的 seq（0 = 从头）。`wait_ms>0` 时做长轮询：
/// 没有新事件就最多等这么久，再一次返回 —— 比 SSE 简单，agent 用一个
/// 普通 HTTP 客户端就能实现「断线重连 + 接着读」。
fn events(full: &str, id: &str, store: &Arc<Store>) -> (u16, serde_json::Value) {
    if id.is_empty() || id.contains('/') {
        return (
            400,
            serde_json::json!({"error": "路径里要有任务 id：/jobs/<id>/events"}),
        );
    }
    let since: i64 = query(full, "since")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0)
        .max(0);
    let wait_ms: u64 = query(full, "wait_ms")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0)
        .min(25_000);
    let limit: usize = query(full, "limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(200)
        .clamp(1, 1000);

    // 任务不存在、也没有历史事件：这是打错了 id，不该回一个永远空的流。
    if since == 0
        && store.get(id).ok().flatten().is_none()
        && store.last_event_seq(id).unwrap_or(0) == 0
    {
        return (404, serde_json::json!({"error": "没有这个任务"}));
    }

    let deadline = std::time::Instant::now() + Duration::from_millis(wait_ms);
    loop {
        match store.events_since(id, since, limit) {
            Ok(list) => {
                let next = list.last().map(|e| e.seq).unwrap_or(since);
                if !list.is_empty() || wait_ms == 0 || std::time::Instant::now() >= deadline {
                    return (
                        200,
                        serde_json::json!({
                            "job_id": id,
                            "events": list,
                            "next": next,
                        }),
                    );
                }
            }
            Err(e) => {
                return (500, serde_json::json!({"error": format!("{e:#}")}));
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// `GET /jobs/<id>/seconds`：HLS/DASH 的秒进度（最后一个 progress_secs 事件）。
/// 字节进度未知时，状态栏靠这个画条；token 调用方仍然只能看自己的任务。
fn seconds_progress(
    id: &str,
    store: &Arc<Store>,
    agent: Option<&str>,
) -> (u16, serde_json::Value) {
    if id.is_empty() || id.contains('/') {
        return (
            400,
            serde_json::json!({"error": "路径里要有任务 id：/jobs/<id>/seconds"}),
        );
    }
    match store.get(id) {
        Ok(Some(t)) if owns(agent, &t) => {}
        Ok(Some(_)) | Ok(None) => return job_not_found(),
        Err(e) => return (500, serde_json::json!({"error": format!("{e:#}")})),
    }
    let (done, total) = latest_seconds(store, id);
    let pct = done
        .saturating_mul(100)
        .checked_div(total)
        .unwrap_or(0)
        .min(100);
    (
        200,
        serde_json::json!({
            "id": id,
            "unit": "seconds",
            "seconds_done": done,
            "seconds_total": total,
            "pct": pct,
        }),
    )
}

/// 从完整路径里取一个查询参数。`full` 要包含 `?`。
fn query(full: &str, name: &str) -> Option<String> {
    let q = full.split_once('?')?.1;
    q.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        if k == name {
            Some(v.to_string())
        } else {
            None
        }
    })
}

/// `GET /statusline[?source=agent:pi]`：给 TUI 插件/看板轮询的活跃任务。
///
/// 选择规则与 CLI `skyget statusline` 一致：running > pending > interrupted，
/// 同类取最新；token 调用方只看得到自己的任务。没有活跃任务时返回
/// `state: "idle"`，插件不必把 404 当正常状态处理。
fn statusline_view(
    store: &Arc<Store>,
    agent: Option<&str>,
    source: Option<&str>,
) -> (u16, serde_json::Value) {
    // 只查活跃状态：老的长任务不会被后来几百条短任务挤出最近窗口。
    let list = match store.active_tasks(2000) {
        Ok(l) => l,
        Err(e) => return (500, serde_json::json!({"error": format!("{e:#}")})),
    };
    let active = list
        .iter()
        .filter(|t| owns(agent, t))
        .filter(|t| source.is_none_or(|s| t.source.starts_with(s)))
        .filter_map(|t| {
            let state = t.state_for_display();
            matches!(state.as_str(), "running" | "pending" | "interrupted")
                .then_some((t, state))
        })
        .max_by_key(|(t, state)| {
            let rank = match state.as_str() {
                "running" => 2,
                "pending" => 1,
                _ => 0,
            };
            (rank, t.created_at)
        });
    match active {
        Some((t, _)) => {
            let v = t.view(t.cached, 0);
            // 普通直链有总字节，不必每秒再扫一遍事件表。
            let (sd, st) = if v.bytes_total == 0 {
                latest_seconds(store, &v.id)
            } else {
                (0, 0)
            };
            let use_secs = v.bytes_total == 0 && st > 0;
            let (done, total) = if use_secs {
                (sd, st)
            } else {
                (v.bytes_done, v.bytes_total)
            };
            let pct = done
                .saturating_mul(100)
                .checked_div(total)
                .unwrap_or(0)
                .min(100);
            (
                200,
                serde_json::json!({
                    "job_id": v.id,
                    "state": v.state,
                    "source": v.source,
                    "done": v.bytes_done,
                    "total": v.bytes_total,
                    "seconds_done": sd,
                    "seconds_total": st,
                    "unit": if use_secs { "seconds" } else { "bytes" },
                    "pct": pct,
                }),
            )
        }
        None => (
            200,
            serde_json::json!({
                "job_id": "",
                "state": "idle",
                "source": "",
                "done": 0,
                "total": 0,
                "seconds_done": 0,
                "seconds_total": 0,
                "unit": "bytes",
                "pct": 0,
            }),
        ),
    }
}

/// 该任务最后一次 `progress_secs` 的秒进度。事件被保留策略清掉时返回 (0,0)。
fn latest_seconds(store: &Store, id: &str) -> (u64, u64) {
    let last = store.last_event_seq(id).unwrap_or(0);
    if last == 0 {
        return (0, 0);
    }
    for e in store
        .events_since(id, last.saturating_sub(256), 256)
        .unwrap_or_default()
        .into_iter()
        .rev()
    {
        if e.kind == "progress_secs" {
            return (
                e.data["seconds_done"].as_u64().unwrap_or(0),
                e.data["seconds_total"].as_u64().unwrap_or(0),
            );
        }
    }
    (0, 0)
}

/// GET /jobs?limit=200 里的 limit（默认 50，最多 500）
fn jobs_limit(path: &str) -> usize {
    path.split("limit=")
        .nth(1)
        .and_then(|v| v.split('&').next())
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(50)
        .clamp(1, 500)
}

/// 从 `/jobs/<id>/pause` 这种路径里切出 id
fn with_id(path: &str, suffix: &str, f: impl FnOnce(&str) -> (u16, serde_json::Value)) -> (u16, serde_json::Value) {
    let rest = path.trim_start_matches("/jobs/");
    let id = if suffix.is_empty() {
        rest
    } else {
        rest.strip_suffix(suffix).unwrap_or(rest)
    };
    if id.is_empty() || id.contains('/') {
        return (
            400,
            serde_json::json!({
                "error": "路径里得有任务 id：/jobs/<id>/pause",
                "code": "input.bad_id",
            }),
        );
    }
    f(id)
}

/// 暂停：让下载线程自己停，已下的留着（下次接着下）。
/// 控制事件的审计身份：有 token 就是 agent:<name>，否则 api。
fn actor(agent: Option<&str>) -> String {
    agent
        .map(|a| format!("agent:{a}"))
        .unwrap_or_else(|| "api".to_string())
}

fn pause(
    id: &str,
    store: &Arc<Store>,
    running: &Running,
    agent: Option<&str>,
    by: &str,
) -> (u16, serde_json::Value) {
    match store.get(id) {
        Ok(Some(t)) if !owns(agent, &t) => return job_not_found(),
        Ok(None) if agent.is_some() => return job_not_found(),
        _ => {}
    }
    let h = running.lock().unwrap().get(id).cloned();
    match h {
        Some(h) if h.cancelled => (
            409,
            serde_json::json!({"error": "这条已经取消了", "code": "job.cancelled"}),
        ),
        Some(h) => {
            h.cancel.cancel();
            let _ = store.append_event(id, "paused", &serde_json::json!({"by": by}));
            // 还在排队、没被 worker 拿到的任务，引擎没机会写「中断」——
            // 这里替它写，否则它会一直显示「排队」。
            if let Ok(Some(mut t)) = store.get(id) {
                if t.state == crate::model::STATE_PENDING {
                    t.state = crate::model::STATE_INTERRUPTED.to_string();
                    t.error = "已暂停（还没开始跑）".to_string();
                    t.error_code = "job.paused".to_string();
                    t.retryable = true;
                    t.next_action = "继续即可；已下的分片会接着用。".to_string();
                    let _ = store.save(&t);
                }
            }
            (202, serde_json::json!({"id": id, "state": "stopping"}))
        }
        // 没有在跑的句柄。先看这条到底是什么状态 —— 以前一律回
        // 「不是这个服务在跑」，而任务其实早就下完了，那句话会把人引向
        // 「是不是有两个 daemon」这种不存在的问题。
        None => {
            let st = match store.get(id).ok().flatten() {
                Some(t) => t.state_for_display(),
                None => {
                    return (
                        404,
                        serde_json::json!({"error": "没有这个任务", "code": "job.not_found"}),
                    )
                }
            };
            (
                409,
                serde_json::json!({
                    "error": format!("这条已经结束（{st}），没有可暂停的"),
                    "code": "job.not_running",
                    "retryable": false,
                    "state": st,
                    "next_action": "要重跑就 `skyget retry <id>`；要清理记录就 `skyget forget <id>`。",
                }),
            )
        }
    }
}

/// 继续：从台账取回原始条件（落地路径、期望 sha256）再跑一次。
fn resume(
    id: &str,
    store: &Arc<Store>,
    running: &Running,
    scheduler: &Arc<Scheduler>,
    agent_policies: Option<&Arc<AgentPolicies>>,
    agent: Option<&str>,
    by: &str,
) -> (u16, serde_json::Value) {
    let mut task = match store.get(id) {
        Ok(Some(t)) => t,
        Ok(None) => return (404, serde_json::json!({"error": "没有这个任务"})),
        Err(e) => return (500, serde_json::json!({"error": format!("{e:#}")})),
    };
    if !owns(agent, &task) {
        return job_not_found();
    }

    // 还在队列里的任务：pause 只是把 cancel 置位，job 并没有结束。
    // 直接复位信号让它继续排队，不要另起一条（另一条会和旧队列项抢同一个 id）。
    let h = running.lock().unwrap().get(id).cloned();
    if let Some(h) = h {
        if h.cancelled {
            return (
                409,
                serde_json::json!({"error": "这条已经取消了", "code": "job.cancelled"}),
            );
        }
        if h.queued && resume_queued(running, id, h.gen) {
            task.state = crate::model::STATE_PENDING.to_string();
            task.error.clear();
            task.error_code.clear();
            task.retryable = false;
            task.retry_after_ms = 0;
            task.next_action.clear();
            task.updated_at = now_ms();
            let _ = store.save(&task);
            let _ = store.append_event(
                id,
                "queued",
                &serde_json::json!({"resumed": true, "by": by}),
            );
            return (
                202,
                serde_json::json!({"id": id, "state": "queued", "resumed": true}),
            );
        }
        // 正在跑（或者刚出队、还没收尾）：让调用方稍后再试，别起第二个写者。
        return (
            409,
            serde_json::json!({
                "error": "这条正在跑；要重来先 pause，等它停下再 resume",
                "code": "job.running",
                "retryable": true,
                "retry_after_ms": 200,
            }),
        );
    }

    dispatch(task, store, running, scheduler, true, agent_policies)
}

/// 取消：停掉并清掉临时分片，记录也删掉（已落地的文件不动）。
fn cancel(
    id: &str,
    store: &Arc<Store>,
    running: &Running,
    agent: Option<&str>,
    by: &str,
) -> (u16, serde_json::Value) {
    let task = store.get(id).ok().flatten();
    if let Some(t) = &task {
        if !owns(agent, t) {
            return job_not_found();
        }
    } else if agent.is_some() {
        return job_not_found();
    }
    // 注意：`if let Some(c) = running.lock()...` 会把那把锁的守卫**活着带进 if 块**，
    // 里面再 running.lock() 就是自己等自己 —— 整个服务当场卡死（实测过）。
    // 所以先 clone 出来，守卫在语句结束时就释放。
    let handle = running.lock().unwrap().get(id).cloned();
    if task.is_none() && handle.is_none() {
        return (
            404,
            serde_json::json!({"error": "没有这个任务", "code": "job.not_found"}),
        );
    }
    if handle.is_none() {
        if let Some(t) = &task {
            // 不是本进程在跑，但另一个进程（人类 CLI）还活着：删记录会被它写回来。
            if t.pid > 0 && t.pid != std::process::id() as i64 && pid_alive(t.pid) {
                return (
                    409,
                    serde_json::json!({
                        "error": "这条是另一个进程在跑，停不了它；去那边停",
                        "code": "job.external",
                        "retryable": false,
                    }),
                );
            }
        }
    }
    if let Some(h) = handle {
        // 先标记「已取消」再置信号：resume 看到 cancelled 就不会把队列项复活。
        {
            let mut m = running.lock().unwrap();
            if let Some(x) = m.get_mut(id) {
                if x.gen == h.gen {
                    x.cancelled = true;
                }
            }
        }
        h.cancel.cancel();
        // 等下载线程收摊（最多 5 秒），再删记录 —— 否则线程最后那次写库会把它写回来。
        // 排队中的任务可能 5 秒内轮不到；它的队列项出队时会看到 cancelled 并退出。
        for _ in 0..50 {
            if !running.lock().unwrap().contains_key(id) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    if let Some(t) = &task {
        if t.direction == crate::model::DIR_UPLOAD {
            crate::upload::discard_state(&t.id);
        } else {
            let _ = crate::engine::discard_task_parts(t);
        }
    }
    // 取消是「意图」，不是故障：事件流里要留下这一笔，事后能分清
    //「用户不要了」和「下载失败」。
    let _ = store.append_event(id, "cancelled", &serde_json::json!({"by": by}));
    // 记录马上要删，webhook 用内存里的那份发。
    if let Some(mut t) = task.clone() {
        t.state = "cancelled".to_string();
        crate::webhook::notify_task(&t);
    }
    // remove() 同时写删除墓碑（同一事务），/jobs/changes 才不会漏掉这一条。
    let _ = store.remove(id);
    (200, serde_json::json!({"id": id, "state": "cancelled"}))
}

/// 只删记录，不动盘上的文件。
///
/// 还在跑/排队的不许直接删：线程最后那次写库会把记录写回来（复活幽灵）。
/// 要删先 cancel。
fn forget(
    id: &str,
    store: &Arc<Store>,
    running: &Running,
    agent: Option<&str>,
) -> (u16, serde_json::Value) {
    match store.get(id) {
        Ok(Some(t)) if !owns(agent, &t) => return job_not_found(),
        Ok(None) if agent.is_some() => return job_not_found(),
        _ => {}
    }
    if running.lock().unwrap().contains_key(id) {
        return (
            409,
            serde_json::json!({
                "error": "这条还在跑/排队；先 cancel 再删记录",
                "code": "job.running",
                "retryable": false,
            }),
        );
    }
    // 另一个进程（CLI standalone）可能在跑：删行会被它终态写回。
    if let Ok(Some(t)) = store.get(id) {
        if t.pid > 0 && t.pid != std::process::id() as i64 && pid_alive(t.pid) {
            return (
                409,
                serde_json::json!({
                    "error": "这条是另一个进程在跑；先让它停下",
                    "code": "job.external",
                    "retryable": false,
                }),
            );
        }
        if t.direction == crate::model::DIR_UPLOAD {
            crate::upload::discard_state(id);
        }
    }
    match store.remove(id) {
        Ok(true) => (200, serde_json::json!({"id": id, "forgotten": true})),
        Ok(false) => (404, serde_json::json!({"error": "没有这个任务"})),
        Err(e) => (500, serde_json::json!({"error": format!("{e:#}")})),
    }
}

/// 取某个 agent 的有效策略；没有 per-agent 配置就用全局 strict。
fn policy_for(
    agent: Option<&str>,
    agent_policies: Option<&Arc<AgentPolicies>>,
) -> crate::security::Policy {
    let base = crate::security::Policy::strict();
    match (agent, agent_policies) {
        (Some(name), Some(ap)) => ap.for_agent(name, &base),
        _ => base,
    }
}

/// 提交前的检查输入。`POST /jobs` 和 `POST /check` 共用同一份 ——
/// 「预演」和「真提交」必须是同一套判定，否则预演会说谎。
struct Want<'a> {
    url: &'a str,
    dest: &'a str,
    name: &'a str,
    sha256: &'a str,
    sources: &'a [String],
    credential_ref: &'a str,
    /// 请求体自称的来源（只用来挡住「没有身份却自称 agent」）
    source: &'a str,
    /// 上传源路径（下载任务为空）
    path: &'a str,
    /// 排队优先级（空 = normal）
    priority: &'a str,
    /// 幂等键（预检传空）
    idempotency_key: &'a str,
}

struct Prepared {
    url: String,
    dest: String,
    sha256: String,
    mirrors: Vec<String>,
    source: String,
    /// 逐项结论，`POST /check` 会原样回给调用方
    checks: Vec<serde_json::Value>,
}

fn checked(checks: &mut Vec<serde_json::Value>, name: &'static str, ok: bool, detail: impl Into<String>) {
    checks.push(serde_json::json!({"name": name, "ok": ok, "detail": detail.into()}));
}

fn deny(
    checks: &mut Vec<serde_json::Value>,
    name: &'static str,
    code: &'static str,
    error: String,
) -> (u16, serde_json::Value) {
    checked(checks, name, false, error.clone());
    let status = status_for(code);
    (
        status,
        serde_json::json!({
            "ok": false,
            "error": error,
            "code": code,
            "next_action": next_action_for(code),
            "checks": checks,
        }),
    )
}

/// 拒绝时的 HTTP 状态：和重构前 `submit` 的行为保持一致
/// （自称 agent 和策略拒绝都是 403，输入形状问题是 400）。
fn status_for(code: &str) -> u16 {
    match code {
        "input.agent_source_requires_token" => 403,
        _ if code.starts_with("policy.") => 403,
        _ => 400,
    }
}

/// 失败之后该干什么，一句话。预检和提交共用。
fn next_action_for(code: &str) -> &'static str {
    match code {
        "input.agent_source_requires_token" => "用该 agent 自己的 token 提交（或用 unix socket 的 uid 身份）。",
        "input.multi_source_needs_sha" => "多源必须 pin sha256：分片来自不同主机，只有内容哈希能证明拼装正确。",
        "input.bad_sha256" => "sha256 要 64 位十六进制。",
        "input.bad_dest" => "dest 是目录，文件名用 name 给（name 不能带路径）。",
        "input.unsupported" => "只接 http/https 直链、magnet、.torrent、.m3u8/.mpd；网盘/迅雷不做。",
        "policy.egress_denied" | "policy.host_denied" => {
            "如果是内网/回环源，需要部署方设 SKYGET_ALLOW_PRIVATE=1 或 SKYGET_ALLOWED_HOSTS。"
        }
        "policy.metadata_denied" => {
            "云元数据地址默认拒绝（可能泄露凭据）；确需访问要显式设 SKYGET_ALLOW_METADATA=1。"
        }
        "policy.dest_denied" => "换一个落地目录；允许的根由部署方的 SKYGET_DEST_ROOTS 决定。",
        "policy.credential_needs_hosts" => "先配 SKYGET_ALLOWED_HOSTS（或 per-agent allowed_hosts），凭证才允许发出。",
        "policy.upload_source_denied" => "上传默认关闭；由部署方用 SKYGET_UPLOAD_ROOTS 圈定可读目录。",
        _ => "",
    }
}

/// 提交前的全部判定。`deep = true` 时额外做出网判定（`/check` 用；
/// 真提交故意不做出网前置检查 —— 域名暂时解析失败该按网络错误重试，
/// 不该在提交那一刻判死）。
fn preflight(
    w: &Want<'_>,
    agent: Option<&str>,
    agent_policies: Option<&Arc<AgentPolicies>>,
    registry: Option<&Registry>,
    deep: bool,
) -> Result<Prepared, (u16, serde_json::Value)> {
    let mut checks: Vec<serde_json::Value> = Vec::new();

    // 1) 来源自称：启用身份/策略后不允许没有 token 却写 agent:
    if agent.is_none()
        && (registry.is_some() || agent_policies.is_some())
        && w.source.trim().starts_with("agent:")
    {
        return Err(deny(
            &mut checks,
            "identity",
            "input.agent_source_requires_token",
            "不能自称 agent；请用该 agent 的 token 提交".into(),
        ));
    }
    if w.source.len() > 64 {
        return Err(deny(
            &mut checks,
            "identity",
            "input.bad_source",
            "source 最长 64 字节".into(),
        ));
    }
    if w.idempotency_key.len() > 128 {
        return Err(deny(
            &mut checks,
            "identity",
            "input.bad_idempotency_key",
            "idempotency_key 最长 128 字节".into(),
        ));
    }
    let prio = w.priority.trim().to_ascii_lowercase();
    if !prio.is_empty()
        && !matches!(
            prio.as_str(),
            crate::model::PRIORITY_INTERACTIVE
                | crate::model::PRIORITY_NORMAL
                | crate::model::PRIORITY_BATCH
        )
    {
        return Err(deny(
            &mut checks,
            "priority",
            "input.bad_priority",
            "priority 只认 interactive / normal / batch".into(),
        ));
    }
    checked(&mut checks, "identity", true, format!("来源 {}", if w.source.is_empty() { "(默认)" } else { w.source }));
    checked(
        &mut checks,
        "priority",
        true,
        if prio.is_empty() { "normal（默认）".to_string() } else { prio.clone() },
    );

    // 2) 地址
    let url = w.url.trim().to_string();
    if url.is_empty() {
        return Err(deny(&mut checks, "protocol", "input.bad_url", "url 不能为空".into()));
    }
    let lower = url.to_ascii_lowercase();
    let kind = if lower.starts_with("http://") || lower.starts_with("https://") {
        "http"
    } else if lower.starts_with("magnet:") || lower.ends_with(".torrent") {
        "bt"
    } else if crate::hls::is_stream(&url) {
        "stream"
    } else {
        return Err(deny(
            &mut checks,
            "protocol",
            "input.unsupported",
            format!("不支持的地址类型：{}", url.chars().take(24).collect::<String>()),
        ));
    };
    checked(
        &mut checks,
        "protocol",
        true,
        match kind {
            "http" => "http(s) 直链".to_string(),
            "bt" => "BitTorrent（种子自身有 piece hash；不受 host 白名单约束）".to_string(),
            _ => "切片流（分片由 ffmpeg 取，见 security.md 的边界）".to_string(),
        },
    );

    // 3) 镜像
    let mut mirrors: Vec<String> = Vec::new();
    for s in w.sources {
        let s = s.trim();
        if s.is_empty() {
            continue;
        }
        let l = s.to_ascii_lowercase();
        if !l.starts_with("http://") && !l.starts_with("https://") {
            return Err(deny(
                &mut checks,
                "mirrors",
                "input.unsupported",
                "sources 只支持 http/https 镜像".into(),
            ));
        }
        if !mirrors.iter().any(|m| m == s) {
            mirrors.push(s.to_string());
        }
    }
    let sha = w.sha256.trim().to_ascii_lowercase();
    if !mirrors.is_empty() && sha.is_empty() {
        return Err(deny(
            &mut checks,
            "mirrors",
            "input.multi_source_needs_sha",
            "多源下载必须同时给 sha256（用来判断哪份内容是对的）".into(),
        ));
    }
    checked(
        &mut checks,
        "mirrors",
        true,
        if mirrors.is_empty() {
            "单源".to_string()
        } else {
            format!("{} 个镜像 + 主源", mirrors.len())
        },
    );

    // 4) 校验值形状
    if !sha.is_empty() && !valid_sha(&sha) {
        return Err(deny(
            &mut checks,
            "sha256",
            "input.bad_sha256",
            "sha256 必须是 64 位十六进制".into(),
        ));
    }
    checked(
        &mut checks,
        "sha256",
        true,
        if sha.is_empty() {
            "没有给（不会被校验，只记 tofu 信任级）".to_string()
        } else {
            "格式正确，落地后一定比对".to_string()
        },
    );

    // 5) 凭证引用
    let cred = w.credential_ref.trim();
    if !cred.is_empty() && !cred.starts_with("env:") && !cred.starts_with("file:") {
        return Err(deny(
            &mut checks,
            "credential",
            "input.bad_credential_ref",
            "credential_ref 只支持 env:NAME / file:PATH".into(),
        ));
    }

    // 6) 落地目录
    let dest_raw = if w.dest.is_empty() {
        crate::settings::dest_dir().display().to_string()
    } else {
        crate::settings::expand(w.dest)
    };
    let dest = match normalize_dest(&dest_raw, w.name) {
        Ok(d) => d,
        Err(e) => {
            return Err(deny(&mut checks, "dest", "input.bad_dest", e));
        }
    };
    let policy = policy_for(agent, agent_policies);
    if !cred.is_empty() && policy.allowed_hosts.is_none() {
        return Err(deny(
            &mut checks,
            "credential",
            "policy.credential_needs_hosts",
            "使用 credential_ref 前必须配置 SKYGET_ALLOWED_HOSTS 或 per-agent allowed_hosts".into(),
        ));
    }
    checked(
        &mut checks,
        "credential",
        true,
        if cred.is_empty() {
            "没有用凭证".to_string()
        } else {
            format!("{cred}（值不落台账）")
        },
    );
    if let Err(e) = crate::security::check_dest(&dest, &policy) {
        let code = e
            .downcast_ref::<crate::security::Denied>()
            .map(|d| d.code)
            .unwrap_or("policy.dest_denied");
        return Err(deny(&mut checks, "dest", code, format!("{e:#}")));
    }
    checked(&mut checks, "dest", true, dest.clone());

    // 7) 上传源（只有上传任务有）
    if !w.path.trim().is_empty() {
        if let Err(e) = crate::security::check_upload_source(w.path, &policy.upload_roots) {
            let code = e
                .downcast_ref::<crate::security::Denied>()
                .map(|d| d.code)
                .unwrap_or("policy.upload_source_denied");
            return Err(deny(&mut checks, "upload_source", code, format!("{e:#}")));
        }
        checked(&mut checks, "upload_source", true, w.path.to_string());
    }

    // 8) 出网（只有预检做）
    if deep && kind == "http" {
        if let Err(e) = crate::security::check_url(&url, &policy) {
            let code = e
                .downcast_ref::<crate::security::Denied>()
                .map(|d| d.code)
                .unwrap_or("policy.egress_denied");
            return Err(deny(&mut checks, "egress", code, format!("{e:#}")));
        }
        checked(&mut checks, "egress", true, host_of(&url));
    }

    let source = match agent {
        Some(name) => format!("agent:{name}"),
        None if w.source.is_empty() => "api".to_string(),
        None => w.source.to_string(),
    };
    Ok(Prepared {
        url,
        dest,
        sha256: sha,
        mirrors,
        source,
        checks,
    })
}

fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_else(|| "?".to_string())
}

/// `POST /check`：**不**入队、不联网，只回答「这条提交会被怎么判」。
///
/// 和 `/jobs` 共用 `preflight`：预演和真提交必须是同一套判定，
/// 否则预演只会给人假的安心（或者假的恐慌）。
fn check(
    body: &[u8],
    agent: Option<&str>,
    agent_policies: Option<&Arc<AgentPolicies>>,
    registry: Option<&Registry>,
) -> (u16, serde_json::Value) {
    #[derive(serde::Deserialize, Default)]
    struct CheckBody {
        #[serde(default)]
        url: String,
        #[serde(default)]
        dest: String,
        #[serde(default)]
        name: String,
        #[serde(default)]
        sha256: String,
        #[serde(default)]
        sources: Vec<String>,
        #[serde(default)]
        credential_ref: String,
        #[serde(default)]
        source: String,
        #[serde(default)]
        path: String,
        #[serde(default)]
        priority: String,
        #[serde(default)]
        idempotency_key: String,
    }
    let b: CheckBody = match serde_json::from_slice(body) {
        Ok(b) => b,
        Err(e) => {
            return (
                400,
                serde_json::json!({
                    "ok": false,
                    "error": format!("请求体要是 JSON：{e}"),
                    "code": "input.bad_json",
                }),
            )
        }
    };
    let w = Want {
        url: &b.url,
        dest: &b.dest,
        name: &b.name,
        sha256: &b.sha256,
        sources: &b.sources,
        credential_ref: &b.credential_ref,
        source: &b.source,
        path: &b.path,
        priority: &b.priority,
        idempotency_key: &b.idempotency_key,
    };
    match preflight(&w, agent, agent_policies, registry, true) {
        Ok(p) => (
            200,
            serde_json::json!({
                "ok": true,
                "url": p.url,
                "dest": p.dest,
                "source": p.source,
                "checks": p.checks,
            }),
        ),
        Err((code, mut v)) => {
            if let Some(o) = v.as_object_mut() {
                o.insert("ok".into(), serde_json::json!(false));
            }
            (code, v)
        }
    }
}

fn submit(
    body: &[u8],
    store: &Arc<Store>,
    running: &Running,
    scheduler: &Arc<Scheduler>,
    agent: Option<&str>,
    agent_policies: Option<&Arc<AgentPolicies>>,
    registry: Option<&Registry>,
) -> (u16, serde_json::Value) {
    let job: NewJob = match serde_json::from_slice(body) {
        Ok(j) => j,
        Err(e) => {
            return (
                400,
                serde_json::json!({
                    "error": format!("请求体要是 JSON：{e}"),
                    "code": "input.bad_json",
                }),
            )
        }
    };
    // 所有提交前的判定都在 preflight 里（`POST /check` 用的是同一份）：
    // 少一份实现，就少一次「预演说行、真提交说不行」。
    let w = Want {
        url: &job.url,
        dest: &job.dest,
        name: &job.name,
        sha256: &job.sha256,
        sources: &job.sources,
        credential_ref: &job.credential_ref,
        source: &job.source,
        path: "",
        priority: &job.priority,
        idempotency_key: &job.idempotency_key,
    };
    let p = match preflight(&w, agent, agent_policies, registry, false) {
        Ok(p) => p,
        Err((code, v)) => return (code, v),
    };
    let primary_url = p.url.clone();
    let dest = p.dest.clone();
    let want_sha = p.sha256.clone();
    let cred = job.credential_ref.trim().to_string();
    let source = p.source.clone();
    let task_sources = if p.mirrors.is_empty() {
        Vec::new()
    } else {
        let mut v = vec![primary_url.clone()];
        for m in &p.mirrors {
            if *m != primary_url {
                v.push(m.clone());
            }
        }
        v
    };

    // 幂等：同一个意图只落成一条任务。agent 重试、脚本重放、用户连点，
    // 不该变成 N 条记录、N 个下载线程。
    let idem_key = if job.idempotency_key.trim().is_empty() {
        format!("{source}:{}", natural_key(&primary_url, &dest, &want_sha))
    } else {
        format!("idem:{source}:{}", job.idempotency_key.trim())
    };
    let task = Task {
        id: new_id(&primary_url),
        direction: crate::model::DIR_DOWNLOAD.to_string(),
        protocol: String::new(),
        credential_ref: cred.to_string(),
        sources: task_sources,
        url: primary_url,
        dest: dest.clone(),
        state: crate::model::STATE_PENDING.to_string(),
        bytes_done: 0,
        bytes_total: 0,
        sha256: String::new(),
        error: String::new(),
        error_code: String::new(),
        retryable: false,
        retry_after_ms: 0,
        next_action: String::new(),
        source,
        expected_sha256: want_sha,
        pid: 0,
        links_live: 0,
        links_seen: 0,
        trust: String::new(),
        priority: {
            let p = job.priority.trim().to_ascii_lowercase();
            if matches!(p.as_str(), "interactive" | "normal" | "batch") {
                p
            } else {
                crate::model::PRIORITY_NORMAL.to_string()
            }
        },
        accept_change: job.accept_change,
        cached: false,
        rerun: false,
        created_at: now_ms(),
        updated_at: now_ms(),
    };
    enqueue(task, idem_key, store, running, scheduler, agent_policies)
}

/// 幂等键命中已有任务：同来源返回 dedupe，不同来源拒绝（防旧键/手工键泄露）。
fn existing_job(existing: &crate::model::Task, source: &str) -> (u16, serde_json::Value) {
    if existing.source != source {
        return (
            409,
            serde_json::json!({
                "error": "这个幂等键已被另一个来源占用",
                "code": "job.conflict",
                "retryable": true,
            }),
        );
    }
    (
        200,
        serde_json::json!({
            "id": existing.id,
            "state": existing.state_for_display(),
            "deduped": true,
            "events": format!("/jobs/{}/events", existing.id),
        }),
    )
}

/// 落行 + 原子占幂等键 + 入调度。下载和上传共用同一套幂等语义。
fn enqueue(
    task: Task,
    idem_key: String,
    store: &Arc<Store>,
    running: &Running,
    scheduler: &Arc<Scheduler>,
    agent_policies: Option<&Arc<AgentPolicies>>,
) -> (u16, serde_json::Value) {
    // 自然键（`auto:` / `auto-up:`）命中的是一条**早已结束**的旧任务时，
    // 它不该再一直被 dedupe 回去：`skyget get <url> -o ~/models` 这种
    // 「每天定时拉一次，看有没有新版本」的 bot 会永远拿回上周那条 done。
    // 显式 `idempotency_key` 的语义不变（那是调用方说「这就是同一个意图」）。
    let mut previous_job: Option<String> = None;
    if let Some(existing) = store.find_idem(&idem_key).unwrap_or(None) {
        if stale_natural_key(&existing, &idem_key) {
            let _ = store.drop_idem(&idem_key);
            previous_job = Some(existing.id.clone());
        } else {
            return existing_job(&existing, &task.source);
        }
    }
    // 先把任务行落库，再占幂等键：反过来会出现
    // 「A 占了键但还没插行 → B 以为键是陈旧的、删掉重占」的窗口，
    // 两个请求就会各落一条任务。先插行后，B claim 失败时一定能 find 到 A 的任务。
    if let Err(e) = store.save(&task) {
        return (500, serde_json::json!({"error": format!("写台账失败：{e:#}")}));
    }

    // 原子占键：两个并发重试只有一个能占住，另一个拿回同一条任务。
    match store.claim_idem(&idem_key, &task.id) {
        Ok(true) => {}
        Ok(false) => {
            if let Some(existing) = store.find_idem(&idem_key).unwrap_or(None) {
                // 自己这条是输家：删掉，别在台账里留一条没有幂等键的孤儿。
                let _ = store.remove(&task.id);
                return existing_job(&existing, &task.source);
            }
            // 陈旧键：对应的任务已经被删了，清掉再占一次。
            let _ = store.drop_idem(&idem_key);
            if !store.claim_idem(&idem_key, &task.id).unwrap_or(false) {
                let _ = store.remove(&task.id);
                return (
                    409,
                    serde_json::json!({
                        "error": "同一个幂等键正在被另一个请求占用",
                        "code": "job.conflict",
                        "retryable": true,
                    }),
                );
            }
        }
        Err(e) => {
            let _ = store.remove(&task.id);
            return (500, serde_json::json!({"error": format!("{e:#}")}));
        }
    }
    let (code, mut payload) = dispatch(task, store, running, scheduler, false, agent_policies);
    if let Some(prev) = previous_job {
        // 告诉调用方「这次是新任务，上一次是这条」——它才能判断该不该覆盖本地文件。
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("previous_job".to_string(), serde_json::json!(prev));
            obj.insert("deduped".to_string(), serde_json::json!(false));
        }
    }
    (code, payload)
}

/// 自然幂等键命中的旧任务是否已经“过期”。
///
/// 只有自然键（调用方没给 key，由 `(url,dest,sha)` 生成）会过期：
/// 那是“同一个请求”，不是“同一个意图”。TTL 默认 24h，
/// `SKYGET_IDEM_TTL=0` 关掉（回到“永远 dedupe”的旧行为）。
fn stale_natural_key(existing: &crate::model::Task, idem_key: &str) -> bool {
    let ttl: i64 = std::env::var("SKYGET_IDEM_TTL")
        .ok()
        .and_then(|v| crate::prune::parse_age(v.trim()).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(86_400);
    stale_natural_key_at(existing, idem_key, ttl, now_ms())
}

/// 纯函数版：`ttl_secs <= 0` = 关掉（永远 dedupe）。
fn stale_natural_key_at(
    existing: &crate::model::Task,
    idem_key: &str,
    ttl_secs: i64,
    now: i64,
) -> bool {
    if !idem_key.starts_with("auto:") && !idem_key.starts_with("auto-up:") {
        return false;
    }
    if ttl_secs <= 0 {
        return false;
    }
    let terminal = matches!(
        existing.state.as_str(),
        crate::model::STATE_DONE
            | crate::model::STATE_FAILED
            | crate::model::STATE_INTERRUPTED
            | "cancelled"
    );
    terminal && now - existing.updated_at > ttl_secs * 1_000
}

/// 引擎事件 → 事件流里的 (type, data, 是否限流)。
fn event_payload(e: &crate::engine::Event) -> (&'static str, serde_json::Value, bool) {
    use crate::engine::Event::*;
    match e {
        Probing => ("probing", serde_json::json!({}), false),
        Planned {
            total,
            ranges,
            threads,
        } => (
            "planned",
            serde_json::json!({"bytes_total": total, "ranges": ranges, "threads": threads}),
            false,
        ),
        Progress { done, total } => (
            "progress",
            serde_json::json!({"bytes_done": done, "bytes_total": total}),
            true,
        ),
        ProgressSecs { done, total } => (
            "progress_secs",
            serde_json::json!({"seconds_done": done, "seconds_total": total}),
            true,
        ),
        Links { live, seen } => (
            "links",
            serde_json::json!({"live": live, "seen": seen}),
            false,
        ),
        Phase { name, ms } => ("phase", serde_json::json!({"name": name, "ms": ms}), false),
        Note(text) => ("note", serde_json::json!({"text": text}), false),
        Named(name) => ("named", serde_json::json!({"name": name}), false),
        Verifying => ("verifying", serde_json::json!({}), false),
        Finished { cached } => ("done", serde_json::json!({"cached": cached}), false),
    }
}

/// 把 API 的 `dest` / `name` 规整成引擎认识的形状。
///
/// 规则只有两条：
/// - 没给 name：dest 是目录；除非它已经是一个**文件**（保留完整文件路径）。
/// - 给了 name：dest 是目录，文件名必须是不带路径的一段。
fn normalize_dest(dest: &str, name: &str) -> Result<String, String> {
    let dest = dest.trim();
    if dest.is_empty() {
        return Err("dest 不能为空".into());
    }
    let name = name.trim();
    if name.is_empty() {
        if dest.ends_with('/') || std::path::Path::new(dest).is_file() {
            Ok(dest.to_string())
        } else {
            Ok(format!("{}/", dest.trim_end_matches('/')))
        }
    } else {
        if name == "." || name == ".." || name.contains('/') || name.contains('\\') {
            return Err("name 只能是文件名，不能带路径".into());
        }
        if std::path::Path::new(dest).is_file() {
            return Err("dest 是一个文件；给 name 时 dest 必须是目录".into());
        }
        Ok(format!("{}/{}", dest.trim_end_matches('/'), name))
    }
}

#[derive(Deserialize)]
struct NewUpload {
    path: String,
    url: String,
    #[serde(default)]
    protocol: String,
    #[serde(default)]
    sha256: String,
    #[serde(default)]
    credential_ref: String,
    #[serde(default)]
    source: String,
    #[serde(default)]
    idempotency_key: String,
}

/// `POST /uploads`：把本地文件送到远端。默认协议 `put`，可显式 `tus`。
fn upload(
    body: &[u8],
    store: &Arc<Store>,
    running: &Running,
    scheduler: &Arc<Scheduler>,
    agent: Option<&str>,
    agent_policies: Option<&Arc<AgentPolicies>>,
    registry: Option<&Registry>,
) -> (u16, serde_json::Value) {
    let job: NewUpload = match serde_json::from_slice(body) {
        Ok(j) => j,
        Err(e) => {
            return (
                400,
                serde_json::json!({
                    "error": format!("请求体要是 JSON：{e}"), "code": "input.bad_json",
                }),
            )
        }
    };
    if job.url.trim().is_empty() {
        return (
            400,
            serde_json::json!({"error": "url 不能为空", "code": "input.bad_url"}),
        );
    }
    let target = job.url.trim().to_ascii_lowercase();
    if !target.starts_with("http://") && !target.starts_with("https://") {
        return (
            400,
            serde_json::json!({
                "error": "上传目标只支持 http/https", "code": "input.unsupported",
            }),
        );
    }
    if job.source.len() > 64 {
        return (
            400,
            serde_json::json!({"error": "source 最长 64 字节", "code": "input.bad_source"}),
        );
    }
    if job.idempotency_key.len() > 128 {
        return (
            400,
            serde_json::json!({
                "error": "idempotency_key 最长 128 字节",
                "code": "input.bad_idempotency_key",
            }),
        );
    }
    // 同一个边界：没有 agent 身份时不能自称 agent，否则会套用别人的上传根。
    if agent.is_none()
        && (registry.is_some() || agent_policies.is_some())
        && job.source.trim().starts_with("agent:")
    {
        return (
            403,
            serde_json::json!({
                "error": "不能自称 agent；请用该 agent 的 token 提交",
                "code": "input.agent_source_requires_token",
            }),
        );
    }
    let protocol = if job.protocol.trim().is_empty() {
        crate::model::PROTO_PUT.to_string()
    } else {
        job.protocol.trim().to_ascii_lowercase()
    };
    if protocol != crate::model::PROTO_PUT && protocol != crate::model::PROTO_TUS {
        return (
            400,
            serde_json::json!({
                "error": "上传协议只支持 put / tus", "code": "input.bad_protocol",
            }),
        );
    }
    let want_sha = job.sha256.trim().to_ascii_lowercase();
    if !want_sha.is_empty() && !valid_sha(&want_sha) {
        return (
            400,
            serde_json::json!({
                "error": "sha256 必须是 64 位十六进制", "code": "input.bad_sha256",
            }),
        );
    }
    let cred = job.credential_ref.trim();
    if !cred.is_empty() && !cred.starts_with("env:") && !cred.starts_with("file:") {
        return (
            400,
            serde_json::json!({
                "error": "credential_ref 只支持 env:NAME / file:PATH",
                "code": "input.bad_credential_ref",
            }),
        );
    }
    // 源文件边界：根目录为空就是禁止上传；这里就拒绝，不占队列位。
    let policy = policy_for(agent, agent_policies);
    let source_path = match crate::security::check_upload_source(&job.path, &policy.upload_roots) {
        Ok(p) => p,
        Err(e) => {
            let code = e
                .downcast_ref::<crate::security::Denied>()
                .map(|d| d.code)
                .unwrap_or("policy.upload_source_denied");
            return (
                403,
                serde_json::json!({"error": format!("{e:#}"), "code": code}),
            );
        }
    };
    let source_abs = source_path.display().to_string();
    // 来源以 token 为准；幂等键同样按来源隔离。
    let source = match agent {
        Some(name) => format!("agent:{name}"),
        None if job.source.is_empty() => "api".to_string(),
        None => job.source.clone(),
    };
    // 命名空间区分方向：同一个显式 key 不该把下载和上传合成一条任务。
    let idem_key = if job.idempotency_key.trim().is_empty() {
        format!(
            "{source}:auto-up:{}",
            natural_key(&job.url, &source_abs, &want_sha)
        )
    } else {
        format!("idem:up:{source}:{}", job.idempotency_key.trim())
    };

    let task = Task {
        id: new_id(&source_abs),
        direction: crate::model::DIR_UPLOAD.to_string(),
        protocol,
        credential_ref: cred.to_string(),
        sources: Vec::new(),
        url: job.url.clone(),
        dest: source_abs,
        state: crate::model::STATE_PENDING.to_string(),
        bytes_done: 0,
        bytes_total: 0,
        sha256: String::new(),
        error: String::new(),
        error_code: String::new(),
        retryable: false,
        retry_after_ms: 0,
        next_action: String::new(),
        source,
        expected_sha256: want_sha,
        pid: 0,
        links_live: 0,
        links_seen: 0,
        trust: String::new(),
        priority: crate::model::PRIORITY_NORMAL.to_string(),
        accept_change: false,
        cached: false,
        rerun: false,
        created_at: now_ms(),
        updated_at: now_ms(),
    };
    enqueue(task, idem_key, store, running, scheduler, agent_policies)
}

/// 自然幂等键：同一来源、同一 URL、同一落地位置、同一期望 hash = 同一个意图。
/// 调用方会在前面再拼 `source`：键必须按 agent 隔离，不能把别人的任务 dedupe 回来。
/// 跨 agent 的同内容复用走内容缓存（第一份完成后秒传），不是共享任务 id。
fn natural_key(url: &str, dest: &str, sha256: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(url.as_bytes());
    h.update(b"\n");
    h.update(dest.as_bytes());
    h.update(b"\n");
    h.update(sha256.trim().to_ascii_lowercase().as_bytes());
    format!("auto:{:x}", h.finalize())
}

/// 把任务交给后台线程，并登记它的停止信号（暂停 / 取消按 id 找它）。
fn dispatch(
    mut task: Task,
    store: &Arc<Store>,
    running: &Running,
    scheduler: &Arc<Scheduler>,
    rerun: bool,
    agent_policies: Option<&Arc<AgentPolicies>>,
) -> (u16, serde_json::Value) {
    let id = task.id.clone();
    task.state = crate::model::STATE_PENDING.to_string();
    task.rerun = rerun;
    task.updated_at = now_ms();
    let source = task.source.clone();
    let task_priority = if task.priority.is_empty() {
        crate::model::PRIORITY_NORMAL.to_string()
    } else {
        task.priority.clone()
    };
    // 每个任务按自己的 agent 取策略：同一台机器上 claude 和 codex 的
    // 落地根/域名白名单/上传根可以不同。
    let engine_policy = match agent_policies {
        Some(ap) => ap.for_source(&source, &crate::security::Policy::strict()),
        None => crate::security::Policy::strict(),
    };
    let cancel = crate::engine::Cancel::new();
    let gen = RUN_GEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // 检查 + 插入必须在同一把锁里：两个并发 resume 都先 contains_key=false
    // 再各自 insert，会起两个引擎写同一份文件/台账。
    {
        let mut m = running.lock().unwrap();
        if m.contains_key(&id) {
            return (
                409,
                serde_json::json!({
                    "error": "这条已经在跑了（要重新开始先 pause 或 cancel）",
                    "code": "job.running",
                    "retryable": false,
                }),
            );
        }
        m.insert(
            id.clone(),
            RunHandle {
                cancel: cancel.clone(),
                gen,
                queued: true,
                cancelled: false,
            },
        );
    }
    // 先落库再入队：界面/接口立刻能看见「排队」，而不是等 worker 真的开始跑。
    if let Err(e) = store.save(&task) {
        finish_run(running, &id, gen);
        // 新任务的幂等键指向一条写不进去的行：清掉，让调用方干净重试。
        if !rerun {
            let _ = store.remove(&id);
        }
        return (500, serde_json::json!({"error": format!("写台账失败：{e:#}")}));
    }
    let store2 = store.clone();
    let running2 = running.clone();
    let id2 = id.clone();
    let job = Box::new(move || {
        // 真正出队了：标记 queued=false，并看一眼有没有被暂停/取消。
        // 与 resume 的复位共用表锁，避免旧队列项把新一次运行从表里删掉。
        if !mark_started(&running2, &id2, gen) {
            finish_run(&running2, &id2, gen);
            return;
        }
        // 从这里开始，无论正常/panic/提前返回，都由 guard 收尾。
        let _guard = JobGuard {
            running: running2.clone(),
            store: store2.clone(),
            id: id2.clone(),
            gen,
        };
        // daemon 是多 agent 通道：默认只放行公网地址（可用 SKYGET_ALLOW_PRIVATE=1 放开）。
        // per-agent 策略在这个任务自己的 engine 上生效。
        // 连接数交给引擎按体积定（0 = 自动）：写死 5 会让经 daemon 的大文件
        // 永远拿不到并行的那部分带宽 —— 而 agent 走的就是 daemon 这条路。
        let engine = Engine::with_options(0, engine_policy, true);
        // 引擎事件落库：agent 断线重连靠 seq 接着读，人复盘靠它还原过程。
        // 进度最多每秒一条，别让 SQLite 变成高频写入的受害者。
        let store_ev = store2.clone();
        let id_ev = id2.clone();
        let last_progress =
            std::sync::Mutex::new(std::time::Instant::now() - Duration::from_secs(10));
        let cb = move |e: crate::engine::Event| {
            let (kind, data, throttled) = event_payload(&e);
            if throttled {
                let mut last = last_progress.lock().unwrap();
                if last.elapsed() < Duration::from_secs(1) {
                    return;
                }
                *last = std::time::Instant::now();
            }
            let _ = store_ev.append_event(&id_ev, kind, &data);
        };
        let _ = if task.direction == crate::model::DIR_UPLOAD {
            crate::upload::run(&store2, &engine, &mut task, &cb, &cancel)
        } else {
            crate::engine::run_cancellable(&store2, &engine, &mut task, &cb, &cancel)
        };
        // 最终状态：done 已由 Finished 事件覆盖；失败/中断补一条带错误码的。
        // 先看记录还在不在：取消路径可能已经把这条删了，别给幽灵写事件。
        if (task.state == crate::model::STATE_FAILED
            || task.state == crate::model::STATE_INTERRUPTED)
            && store2.get(&id2).ok().flatten().is_some()
        {
            let _ = store2.append_event(
                &id2,
                &task.state,
                &serde_json::json!({
                    // 事件流也会被 agent 写进日志；错误文本里的 URL query 同样是凭证。
                    "error": crate::security::redact_text(&task.error),
                    "code": task.error_code,
                    "retryable": task.retryable,
                    "retry_after_ms": task.retry_after_ms,
                    "bytes_done": task.bytes_done,
                }),
            );
        }
        // guard 在这里 drop，负责 finish_run + panic 收尾。
    });
    let prio = crate::model::priority_rank(&task_priority);
    match scheduler.submit_with(&source, prio, job) {
        Ok(()) => {
            // 收下了、后台在跑 = 202 Accepted。回 200 会让调用方以为已经下完了。
            let _ = store.append_event(&id, "queued", &serde_json::json!({
                "source": source,
                "priority": task_priority,
            }));
            let (running_now, queued) = scheduler.stats();
            (
                202,
                serde_json::json!({
                    "id": id, "state": "queued",
                    "events": format!("/jobs/{id}/events"),
                    "priority": task_priority,
                    "running": running_now, "queued": queued,
                }),
            )
        }
        Err(SubmitError::Full) => {
            finish_run(running, &id, gen);
            if rerun {
                // 恢复的任务别删：它还有已下的分片，标成中断等队列空。
                if let Ok(Some(mut t)) = store.get(&id) {
                    t.state = crate::model::STATE_INTERRUPTED.to_string();
                    t.error = "队列已满，这条还没开始跑".to_string();
                    t.error_code = "queue.full".to_string();
                    t.retryable = true;
                    t.retry_after_ms = 5_000;
                    t.next_action = "等一会儿再继续；已下的分片会保留。".to_string();
                    let _ = store.save(&t);
                }
            } else {
                // 新任务：删记录并释放幂等键，让调用方用同一个键干净重试。
                let _ = store.remove(&id);
            }
            (
                429,
                serde_json::json!({
                    "error": "队列已满", "code": "queue.full",
                    "retryable": true, "retry_after_ms": 5000,
                }),
            )
        }
        Err(SubmitError::Closed) => {
            finish_run(running, &id, gen);
            // 和队列满一样：留一条永远不会启动的 pending 行 + 占住的幂等键，
            // 客户端拿同一个 key 重试只会 dedupe 到这条幽灵。新任务删干净。
            if rerun {
                if let Ok(Some(mut t)) = store.get(&id) {
                    t.state = crate::model::STATE_INTERRUPTED.to_string();
                    t.error = "守护进程正在退出，这条还没开始跑".to_string();
                    t.error_code = "service.closing".to_string();
                    t.retryable = true;
                    t.retry_after_ms = 1000;
                    t.next_action = "守护进程起来后再继续；已下的分片会保留。".to_string();
                    let _ = store.save(&t);
                }
            } else {
                let _ = store.remove(&id);
            }
            (
                503,
                serde_json::json!({
                    "error": "正在退出，暂不收新任务", "code": "service.closing",
                    "retryable": true, "retry_after_ms": 1000,
                }),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    fn harness(tag: &str) -> (Arc<Store>, Running, Arc<Scheduler>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("skyget-api-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Arc::new(Store::open_at(&dir.join("tasks.db")).unwrap());
        let running: Running = Arc::new(Mutex::new(HashMap::new()));
        let scheduler = Scheduler::new(1, 8, 1);
        (store, running, scheduler, dir)
    }

    #[test]
    fn health_reports_version_and_capabilities() {
        let (store, running, scheduler, _dir) = harness("health");
        let (code, v) = route("GET", "/health", b"", &store, &running, &scheduler);
        assert_eq!(code, 200);
        assert_eq!(v["ok"], true);
        assert!(v["version"].as_str().unwrap_or("").starts_with('0'));
        let caps = v["capabilities"].as_array().expect("必须有 capabilities");
        assert!(caps.iter().any(|c| c == "http"), "{v}");
        // 构建里编了什么就该报什么：bt/gui 是可选的，不硬编码。
        assert_eq!(caps.len(), crate::sdk::capabilities().len());
    }

    #[test]
    fn check_reports_which_policy_item_denied() {
        let (store, running, scheduler, _dir) = harness("check-deny");
        // 127.0.0.1 + 严格出网（daemon 默认）：必拒，且原因要指到 egress
        let body = serde_json::to_vec(&serde_json::json!({"url": "http://127.0.0.1:9/x"})).unwrap();
        let (code, v) = route("POST", "/check", &body, &store, &running, &scheduler);
        assert_eq!(code, 403, "{v}");
        assert_eq!(v["ok"], false);
        assert_eq!(v["code"], "policy.egress_denied", "{v}");
        assert!(v["next_action"].as_str().unwrap_or("").contains("SKYGET_ALLOW_PRIVATE"));
        let checks = v["checks"].as_array().unwrap();
        assert!(checks.iter().any(|c| c["name"] == "protocol" && c["ok"] == true));
        assert!(checks.iter().any(|c| c["name"] == "egress" && c["ok"] == false));
    }

    #[test]
    fn check_refuses_unsupported_schemes_without_touching_the_network() {
        let (store, running, scheduler, _dir) = harness("check-scheme");
        let body = serde_json::to_vec(&serde_json::json!({"url": "thunder://QUFodHRw"})).unwrap();
        let (code, v) = route("POST", "/check", &body, &store, &running, &scheduler);
        assert_eq!(code, 400, "{v}");
        assert_eq!(v["code"], "input.unsupported");
    }

    #[test]
    fn check_validates_priority_too() {
        // 预检和提交共用 preflight：交给 `/check` 的字段如果校验不全，
        // 预演就会说「行」而真提交说「不行」。
        let (store, running, scheduler, _dir) = harness("check-prio");
        let body = serde_json::to_vec(&serde_json::json!({
            "url": "https://example.org/x",
            "priority": "urgent",
        }))
        .unwrap();
        let (code, v) = route("POST", "/check", &body, &store, &running, &scheduler);
        assert_eq!(code, 400, "{v}");
        assert_eq!(v["code"], "input.bad_priority");
    }

    #[test]
    fn check_requires_sha_for_mirrors() {
        let (store, running, scheduler, _dir) = harness("check-mirror");
        let body = serde_json::to_vec(&serde_json::json!({
            "url": "http://127.0.0.1/a",
            "sources": ["http://127.0.0.1/b"],
        }))
        .unwrap();
        let (code, v) = route("POST", "/check", &body, &store, &running, &scheduler);
        assert_eq!(code, 400, "{v}");
        assert_eq!(v["code"], "input.multi_source_needs_sha");
    }

    #[test]
    fn check_applies_upload_roots() {
        let (store, running, scheduler, _dir) = harness("check-upload");
        // 没有 SKYGET_UPLOAD_ROOTS：上传一律拒绝（安全默认）
        let body = serde_json::to_vec(&serde_json::json!({
            "url": "http://127.0.0.1/x",
            "path": "/etc/passwd",
        }))
        .unwrap();
        let (code, v) = route("POST", "/check", &body, &store, &running, &scheduler);
        assert_eq!(code, 403, "{v}");
        assert_eq!(v["code"], "policy.upload_source_denied", "{v}");
    }

    #[test]
    fn a_finished_natural_key_only_dedupes_within_the_ttl() {
        let mut t = crate::model::Task {
            id: "x".into(),
            state: crate::model::STATE_DONE.into(),
            updated_at: 1_000_000,
            ..Default::default()
        };
        let key = "auto:abc";
        // 24h TTL：23 小时后仍然 dedupe，25 小时后放行。
        assert!(!stale_natural_key_at(&t, key, 86_400, 1_000_000 + 23 * 3600_000));
        assert!(stale_natural_key_at(&t, key, 86_400, 1_000_000 + 25 * 3600_000));
        // 显式幂等键永远不过期：那是调用方说「这就是同一个意图」。
        assert!(!stale_natural_key_at(
            &t,
            "idem:agent:x:run-1",
            86_400,
            1_000_000 + 365 * 86_400_000
        ));
        // 还在跑的不能用“过期”把它变成第二条。
        t.state = crate::model::STATE_RUNNING.into();
        assert!(!stale_natural_key_at(&t, key, 86_400, 1_000_000 + 999 * 86_400_000));
        // TTL=0 = 关掉（旧行为）。
        t.state = crate::model::STATE_DONE.into();
        assert!(!stale_natural_key_at(&t, key, 0, i64::MAX / 2));
    }

    #[test]
    fn http_line_reader_caps_length() {
        let exact = vec![b'x'; MAX_HTTP_LINE];
        let mut r = std::io::BufReader::new(std::io::Cursor::new(exact));
        assert!(read_line_limited(&mut r, MAX_HTTP_LINE).unwrap().is_some());

        let too_long = vec![b'x'; MAX_HTTP_LINE + 1];
        let mut r = std::io::BufReader::new(std::io::Cursor::new(too_long));
        assert!(read_line_limited(&mut r, MAX_HTTP_LINE).is_err());
    }

    #[test]
    fn http_line_reader_stops_at_newline() {
        let data = b"GET / HTTP/1.1\r\nHost: x\r\n\r\n";
        let mut r = std::io::BufReader::new(&data[..]);
        assert_eq!(
            read_line_limited(&mut r, MAX_HTTP_LINE).unwrap().as_deref(),
            Some("GET / HTTP/1.1\r\n")
        );
        assert_eq!(
            read_line_limited(&mut r, MAX_HTTP_LINE).unwrap().as_deref(),
            Some("Host: x\r\n")
        );
        assert_eq!(
            read_line_limited(&mut r, MAX_HTTP_LINE).unwrap().as_deref(),
            Some("\r\n")
        );
        assert!(read_line_limited(&mut r, MAX_HTTP_LINE).unwrap().is_none());
    }

    #[test]
    fn dest_is_a_directory_by_default_and_name_is_a_basename_only() {
        assert_eq!(normalize_dest("/tmp/out", "").unwrap(), "/tmp/out/");
        assert_eq!(normalize_dest("/tmp/out/", "").unwrap(), "/tmp/out/");
        assert_eq!(normalize_dest("/tmp/out", "x.bin").unwrap(), "/tmp/out/x.bin");
        assert!(normalize_dest("/tmp/out", "../x.bin").is_err());
        assert!(normalize_dest("/tmp/out", "a/b").is_err());

        let f = std::env::temp_dir().join(format!("skyget-dest-file-{}", std::process::id()));
        std::fs::write(&f, b"x").unwrap();
        let s = f.display().to_string();
        assert_eq!(normalize_dest(&s, "").unwrap(), s, "已有文件路径要保留");
        assert!(normalize_dest(&s, "name").is_err(), "文件路径不能再挂 name");
        let _ = std::fs::remove_file(f);
    }

    #[test]
    fn cas_range_parser_handles_open_suffix_and_invalid() {
        assert_eq!(parse_range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(parse_range("bytes=100-", 1000), Some((100, 999)));
        assert_eq!(parse_range("bytes=-100", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=0-9999", 1000), Some((0, 999)));
        assert_eq!(parse_range("bytes=1000-", 1000), None);
        assert_eq!(parse_range("bytes=5-4", 1000), None);
        assert_eq!(parse_range("bytes=0-1,5-6", 1000), None, "多段不支持");
        assert_eq!(parse_range("items=0-1", 1000), None);
        assert_eq!(parse_range("bytes=0-1", 0), None);
    }

    #[test]
    fn job_guard_turns_a_panic_into_internal_panic_and_clears_the_handle() {
        let (store, running, _scheduler, dir) = harness("guard");
        let mut t = Task::new("http://127.0.0.1/x", &dir.join("out").display().to_string());
        t.id = "job-panic".into();
        t.state = crate::model::STATE_RUNNING.to_string();
        store.save(&t).unwrap();
        running.lock().unwrap().insert(
            "job-panic".into(),
            RunHandle {
                cancel: crate::engine::Cancel::new(),
                gen: 9,
                queued: false,
                cancelled: false,
            },
        );
        {
            let _g = JobGuard {
                running: running.clone(),
                store: store.clone(),
                id: "job-panic".into(),
                gen: 9,
            };
            // 模拟 panic 展开：guard 在这里 drop
        }
        let got = store.get("job-panic").unwrap().unwrap();
        assert_eq!(got.state, crate::model::STATE_FAILED);
        assert_eq!(got.error_code, "internal.panic");
        assert!(!running.lock().unwrap().contains_key("job-panic"));
        assert!(store
            .events_since("job-panic", 0, 10)
            .unwrap()
            .iter()
            .any(|e| e.kind == "failed" && e.data["code"] == "internal.panic"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reconcile_marks_queued_orphans_interrupted() {
        let (store, _running, _scheduler, dir) = harness("reconcile");
        let mut t = Task::new("http://127.0.0.1/x", &dir.join("out").display().to_string());
        t.id = "job-queued-orphan".into();
        t.state = crate::model::STATE_PENDING.to_string();
        store.save(&t).unwrap();

        let out = reconcile(&store);
        assert_eq!(out.len(), 1);
        let got = store.get(&t.id).unwrap().unwrap();
        assert_eq!(got.state, crate::model::STATE_INTERRUPTED);
        assert_eq!(got.error_code, "daemon.restarted");
        assert!(got.retryable);
        let events = store.events_since(&t.id, 0, 10).unwrap();
        let last = events.last().expect("要留下 interrupted 事件");
        assert_eq!(last.kind, "interrupted");
        assert_eq!(last.data["queued"], true);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn concurrent_resume_of_the_same_id_only_starts_one_engine() {
        let (store, running, _unused, dir) = harness("dispatch-race");
        let scheduler = Scheduler::new(1, 8, 1);
        let (hold_tx, hold_rx) = std::sync::mpsc::channel::<()>();
        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        let hold_rx = Arc::new(Mutex::new(hold_rx));
        let rx = hold_rx.clone();
        scheduler
            .submit(
                "blocker",
                Box::new(move || {
                    let _ = started_tx.send(());
                    let _ = rx.lock().unwrap().recv();
                }),
            )
            .unwrap();
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("blocker 没开跑");

        let mut t = Task::new("http://user:pass@127.0.0.1/x", &dir.join("out").display().to_string());
        t.id = "job-race".into();
        let mut handles = Vec::new();
        for _ in 0..2 {
            let store = store.clone();
            let running = running.clone();
            let scheduler = scheduler.clone();
            let task = t.clone();
            handles.push(std::thread::spawn(move || {
                dispatch(task, &store, &running, &scheduler, true, None).0
            }));
        }
        let codes: Vec<u16> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(codes.iter().filter(|c| **c == 202).count(), 1, "{codes:?}");
        assert_eq!(codes.iter().filter(|c| **c == 409).count(), 1, "{codes:?}");
        let _ = hold_tx.send(());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn upload_rejects_bad_protocol_sha_and_unconfigured_roots() {
        let (store, running, scheduler, dir) = harness("upload-bad");
        let post = |body: serde_json::Value| {
            let bytes = serde_json::to_vec(&body).unwrap();
            route(
                "POST",
                "/uploads",
                &bytes,
                &store,
                &running,
                &scheduler,
            )
        };
        let (code, v) = post(serde_json::json!({
            "path": "/tmp/x", "url": "https://example.org/u", "protocol": "ftp"
        }));
        assert_eq!(code, 400);
        assert_eq!(v["code"], "input.bad_protocol");

        let (code, v) = post(serde_json::json!({
            "path": "/tmp/x", "url": "https://example.org/u", "sha256": "../etc/passwd"
        }));
        assert_eq!(code, 400);
        assert_eq!(v["code"], "input.bad_sha256");

        let (code, v) = post(serde_json::json!({
            "path": "/tmp/x", "url": "https://example.org/u"
        }));
        assert_eq!(code, 403);
        assert_eq!(v["code"], "policy.upload_source_denied", "默认必须禁止上传");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn concurrent_same_idempotency_key_creates_exactly_one_task() {
        let (store, running, scheduler, dir) = harness("concurrent-idem");
        let body = Arc::new(
            serde_json::to_vec(&serde_json::json!({
                "url": "http://user:pass@127.0.0.1/x",
                "dest": dir.join("out").display().to_string(),
                "source": "agent:test",
                "idempotency_key": "same-key",
            }))
            .unwrap(),
        );
        let mut handles = Vec::new();
        for _ in 0..8 {
            let store = store.clone();
            let running = running.clone();
            let scheduler = scheduler.clone();
            let body = body.clone();
            handles.push(std::thread::spawn(move || {
                route("POST", "/jobs", &body, &store, &running, &scheduler).0
            }));
        }
        let codes: Vec<u16> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(codes.iter().filter(|c| **c == 202).count(), 1, "{codes:?}");
        assert_eq!(codes.iter().filter(|c| **c == 200).count(), 7, "{codes:?}");
        let list = store.list(10).unwrap();
        assert_eq!(list.len(), 1, "同一个幂等键只能有一条任务：{list:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn submit_expands_tilde_before_checking_or_landing() {
        let (store, running, scheduler, dir) = harness("expand");
        let body = serde_json::to_vec(&serde_json::json!({
            "url": "http://user:pass@127.0.0.1/x",
            "dest": "~/skyget-expand-test",
            "source": "agent:test",
        }))
        .unwrap();
        let (code, v) = route("POST", "/jobs", &body, &store, &running, &scheduler);
        assert_eq!(code, 202, "{v}");
        let id = v["id"].as_str().unwrap();
        let t = store.get(id).unwrap().unwrap();
        let home = crate::paths::home_dir();
        assert!(
            t.dest.starts_with(&home.display().to_string()),
            "dest 没展开：{}",
            t.dest
        );
        assert!(t.dest.ends_with('/'), "目录语义丢了尾斜杠：{}", t.dest);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unauthenticated_callers_cannot_claim_an_agent_source() {
        let (store, running, scheduler, dir) = harness("spoof-source");
        let ap = Arc::new(
            AgentPolicies::parse(r#"{"claude":{"dest_roots":["/tmp"]}}"#).unwrap(),
        );
        let body = serde_json::to_vec(&serde_json::json!({
            "url": "http://127.0.0.1:1/x",
            "dest": dir.join("out").display().to_string(),
            "source": "agent:claude",
        }))
        .unwrap();
        // 有 per-agent 策略但没有 token 身份：source 是自称，必须拒绝。
        let (code, v) = route_as(
            "POST",
            "/jobs",
            &body,
            &store,
            &running,
            &scheduler,
            None,
            Some(&ap),
            None,
        );
        assert_eq!(code, 403, "{v}");
        assert_eq!(v["code"], "input.agent_source_requires_token");
        assert!(store.list(10).unwrap().is_empty());

        // 旧的“谁也没配”模式继续允许自称来源（兼容行为）。
        let (code, _v) = route(
            "POST",
            "/jobs",
            &body,
            &store,
            &running,
            &scheduler,
        );
        assert_eq!(code, 202, "没有身份/策略配置时保留旧的自称语义");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn metrics_collapse_unregistered_sources_to_one_series() {
        let mut known = std::collections::HashSet::new();
        known.insert("agent:claude".to_string());
        assert_eq!(metric_source("agent:claude", &known), "agent:claude");
        assert_eq!(metric_source("human", &known), "human");
        assert_eq!(metric_source("agent:random", &known), "other");
        assert_eq!(metric_source("some-random-source", &known), "other");
    }

    #[test]
    fn statusline_endpoint_picks_latest_active_and_scopes_to_token() {
        let (store, running, scheduler, dir) = harness("statusline");
        let mut pending = Task::new("http://127.0.0.1/p", &dir.join("p").display().to_string());
        pending.id = "sl-pending".into();
        pending.source = "agent:claude".into();
        pending.state = crate::model::STATE_PENDING.into();
        pending.created_at = 10;
        store.save(&pending).unwrap();

        let mut run = Task::new("http://127.0.0.1/r", &dir.join("r").display().to_string());
        run.id = "sl-run".into();
        run.source = "agent:pi".into();
        run.state = crate::model::STATE_RUNNING.into();
        run.bytes_done = 30;
        run.bytes_total = 100;
        run.created_at = 20;
        store.save(&run).unwrap();

        let (code, v) = route("GET", "/statusline", b"", &store, &running, &scheduler);
        assert_eq!(code, 200);
        assert_eq!(v["job_id"], "sl-run", "running 优先于 pending: {v}");
        assert_eq!(v["pct"], 30);
        assert_eq!(v["unit"], "bytes");

        let (_, v) = route(
            "GET",
            "/statusline?source=agent:claude",
            b"",
            &store,
            &running,
            &scheduler,
        );
        assert_eq!(v["job_id"], "sl-pending", "source 前缀过滤: {v}");

        let (_, v) = route_as(
            "GET",
            "/statusline",
            b"",
            &store,
            &running,
            &scheduler,
            Some("claude"),
            None,
            None,
        );
        assert_eq!(v["job_id"], "sl-pending", "token 只能看到自己的任务: {v}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn deleted_jobs_keep_their_events_for_audit() {
        let (store, running, scheduler, dir) = harness("events-after-delete");
        let mut t = Task::new("http://127.0.0.1/x", &dir.join("out").display().to_string());
        t.id = "gone".into();
        store.save(&t).unwrap();
        store
            .append_event("gone", "cancelled", &serde_json::json!({"by": "api"}))
            .unwrap();
        store.remove("gone").unwrap();

        let (code, v) = route(
            "GET",
            "/jobs/gone/events?since=0",
            b"",
            &store,
            &running,
            &scheduler,
        );
        assert_eq!(code, 200, "{v}");
        assert!(
            !v["events"].as_array().unwrap().is_empty(),
            "取消后事件应可审计：{v}"
        );

        // per-agent token 没有记录可用于归属校验，不能读别人的历史事件。
        let (code, _) = route_as(
            "GET",
            "/jobs/gone/events?since=0",
            b"",
            &store,
            &running,
            &scheduler,
            Some("pi"),
            None,
            None,
        );
        assert_eq!(code, 404);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn denial_counter_classifies_response_codes() {
        use crate::metrics::{count, Kind};
        let a = count(Kind::Auth);
        note_response_denial(401, &serde_json::json!({"code": "auth.required"}));
        assert!(count(Kind::Auth) > a);

        let c = count(Kind::Credential);
        note_response_denial(
            403,
            &serde_json::json!({"code": "policy.credential_needs_hosts"}),
        );
        assert!(count(Kind::Credential) > c);

        let p = count(Kind::Policy);
        note_response_denial(403, &serde_json::json!({"code": "policy.dest_denied"}));
        assert!(count(Kind::Policy) > p);
    }

    #[test]
    fn seconds_endpoint_reports_hls_progress_and_scopes_token() {
        let (store, running, scheduler, dir) = harness("hls-seconds");
        let mut t = Task::new("https://example.test/x.m3u8", &dir.join("x.mp4").display().to_string());
        t.id = "hls-job".into();
        t.source = "agent:claude".into();
        t.state = crate::model::STATE_RUNNING.into();
        store.save(&t).unwrap();
        store
            .append_event(
                "hls-job",
                "progress_secs",
                &serde_json::json!({"seconds_done": 5, "seconds_total": 10}),
            )
            .unwrap();
        store
            .append_event(
                "hls-job",
                "progress_secs",
                &serde_json::json!({"seconds_done": 7, "seconds_total": 10}),
            )
            .unwrap();

        let (code, v) = route(
            "GET",
            "/jobs/hls-job/seconds",
            b"",
            &store,
            &running,
            &scheduler,
        );
        assert_eq!(code, 200, "{v}");
        assert_eq!(v["seconds_done"], 7);
        assert_eq!(v["seconds_total"], 10);
        assert_eq!(v["pct"], 70);

        // /statusline 也要直接给出 HLS 的秒进度，插件不用再请求一次。
        let (code, s) = route("GET", "/statusline", b"", &store, &running, &scheduler);
        assert_eq!(code, 200, "{s}");
        assert_eq!(s["job_id"], "hls-job");
        assert_eq!(s["unit"], "seconds");
        assert_eq!(s["pct"], 70);

        let (code, _) = route_as(
            "GET",
            "/jobs/hls-job/seconds",
            b"",
            &store,
            &running,
            &scheduler,
            Some("pi"),
            None,
            None,
        );
        assert_eq!(code, 404);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn jobs_active_filter_excludes_finished_tasks() {
        let (store, running, scheduler, dir) = harness("jobs-active");
        let mut done = Task::new("http://127.0.0.1/d", &dir.join("d").display().to_string());
        done.id = "job-done".into();
        done.state = crate::model::STATE_DONE.into();
        store.save(&done).unwrap();
        let mut run = Task::new("http://127.0.0.1/r", &dir.join("r").display().to_string());
        run.id = "job-run".into();
        run.state = crate::model::STATE_RUNNING.into();
        store.save(&run).unwrap();

        let (code, v) = route(
            "GET",
            "/jobs?active=1&limit=50",
            b"",
            &store,
            &running,
            &scheduler,
        );
        assert_eq!(code, 200);
        let ids: Vec<&str> = v
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|x| x["id"].as_str())
            .collect();
        assert!(ids.contains(&"job-run"), "{v}");
        assert!(!ids.contains(&"job-done"), "{v}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn statusline_endpoint_reports_idle_when_nothing_is_active() {
        let (store, running, scheduler, dir) = harness("statusline-idle");
        let mut done = Task::new("http://127.0.0.1/d", &dir.join("d").display().to_string());
        done.id = "sl-done".into();
        done.state = crate::model::STATE_DONE.into();
        store.save(&done).unwrap();

        let (code, v) = route("GET", "/statusline", b"", &store, &running, &scheduler);
        assert_eq!(code, 200);
        assert_eq!(v["state"], "idle");
        assert_eq!(v["job_id"], "");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn agents_view_is_scoped_to_the_caller_and_never_exposes_tokens() {
        let (store, running, scheduler, dir) = harness("agents-view");
        let mut t = Task::new("http://127.0.0.1/a", &dir.join("a").display().to_string());
        t.id = "job-agent".into();
        t.source = "agent:claude".into();
        store.save(&t).unwrap();
        let mut h = Task::new("http://127.0.0.1/b", &dir.join("b").display().to_string());
        h.id = "job-human".into();
        h.source = "human".into();
        store.save(&h).unwrap();

        let ap = Arc::new(
            AgentPolicies::parse(r#"{"claude":{"dest_roots":["/tmp/allowed"],"max_running":1}}"#)
                .unwrap(),
        );
        let (code, v) = route_as(
            "GET",
            "/agents",
            b"",
            &store,
            &running,
            &scheduler,
            None,
            Some(&ap),
            None,
        );
        assert_eq!(code, 200, "{v}");
        assert_eq!(v["registry"], false);
        assert!(v["policy"].is_object(), "doctor 需要 daemon 的实际策略：{v}");
        let list = v["agents"].as_array().unwrap();
        assert_eq!(list.len(), 1, "只列 agent 来源，human 不算：{v}");
        assert_eq!(list[0]["name"], "claude");
        assert_eq!(list[0]["source"], "agent:claude");
        assert_eq!(list[0]["jobs"], 1);
        assert_eq!(list[0]["policy"]["dest_roots"][0], "/tmp/allowed");
        assert!(v.to_string().find("tok_").is_none(), "接口不能带 token");

        // per-agent token 只能看见自己。codex 没有任务，也仍然有身份条目。
        let (_, only) = route_as(
            "GET",
            "/agents",
            b"",
            &store,
            &running,
            &scheduler,
            Some("codex"),
            Some(&ap),
            None,
        );
        assert_eq!(only["caller"], "agent:codex");
        assert_eq!(only["agents"].as_array().unwrap().len(), 0);

        let (_, me) = route_as(
            "GET",
            "/agents",
            b"",
            &store,
            &running,
            &scheduler,
            Some("claude"),
            Some(&ap),
            None,
        );
        assert_eq!(me["agents"].as_array().unwrap().len(), 1);
        assert_eq!(
            me["policy"]["dest_roots"][0], "/tmp/allowed",
            "agent token 的顶层策略应是它自己的有效策略"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn changes_cursor_advances_to_the_last_returned_row_not_wall_clock() {
        let (store, running, scheduler, dir) = harness("changes");
        let mut a = Task::new("http://127.0.0.1/a", &dir.join("a").display().to_string());
        a.id = "job-a".into();
        a.created_at = 100;
        a.updated_at = 100;
        store.save(&a).unwrap();
        let mut b = Task::new("http://127.0.0.1/b", &dir.join("b").display().to_string());
        b.id = "job-b".into();
        b.created_at = 200;
        b.updated_at = 200;
        store.save(&b).unwrap();

        let (code, v) = route(
            "GET",
            "/jobs/changes?since=0",
            b"",
            &store,
            &running,
            &scheduler,
        );
        assert_eq!(code, 200);
        assert_eq!(v["now"], 200, "全量游标应是这批里最大的 updated_at，不是当前时间");
        assert_eq!(v["items"].as_array().unwrap().len(), 2);

        a.updated_at = 300;
        store.save(&a).unwrap();
        let (code, v) = route(
            "GET",
            "/jobs/changes?since=200",
            b"",
            &store,
            &running,
            &scheduler,
        );
        assert_eq!(code, 200);
        assert_eq!(v["now"], 300);
        assert_eq!(v["items"].as_array().unwrap().len(), 1);
        assert_eq!(v["items"][0]["id"], "job-a");

        let (code, v) = route(
            "GET",
            "/jobs/changes?since=300",
            b"",
            &store,
            &running,
            &scheduler,
        );
        assert_eq!(code, 200);
        assert_eq!(v["items"].as_array().unwrap().len(), 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn token_callers_only_see_and_control_their_own_jobs() {
        let (store, running, scheduler, dir) = harness("scope");
        let mut a = Task::new("http://127.0.0.1/a", &dir.join("a").display().to_string());
        a.id = "job-a".into();
        a.source = "agent:claude".into();
        store.save(&a).unwrap();
        let mut b = Task::new("http://127.0.0.1/b", &dir.join("b").display().to_string());
        b.id = "job-b".into();
        b.source = "agent:pi".into();
        store.save(&b).unwrap();

        let (code, v) = route_as(
            "GET",
            "/jobs",
            b"",
            &store,
            &running,
            &scheduler,
            Some("claude"),
            None,
            None,
        );
        assert_eq!(code, 200);
        let items = v.as_array().unwrap();
        assert_eq!(items.len(), 1, "{v}");
        assert_eq!(items[0]["id"], "job-a");

        for path in ["/jobs/job-b", "/jobs/job-b/events?since=0&wait_ms=0"] {
            let (code, v) = route_as(
                "GET",
                path,
                b"",
                &store,
                &running,
                &scheduler,
                Some("claude"),
                None,
                None,
            );
            assert_eq!(code, 404, "{path}: {v}");
        }
        let (code, v) = route_as(
            "POST",
            "/jobs/job-b/cancel",
            b"",
            &store,
            &running,
            &scheduler,
            Some("claude"),
            None,
            None,
        );
        assert_eq!(code, 404, "{v}");
        let (code, _) = route_as(
            "GET",
            "/jobs/changes?since=0",
            b"",
            &store,
            &running,
            &scheduler,
            Some("claude"),
            None,
            None,
        );
        assert_eq!(code, 403);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn control_events_record_token_actor() {
        let (store, running, scheduler, dir) = harness("actor");
        let mut t = Task::new(
            "http://127.0.0.1/x",
            &dir.join("out").display().to_string(),
        );
        t.id = "job-actor".into();
        t.source = "agent:claude".into();
        t.state = crate::model::STATE_RUNNING.to_string();
        store.save(&t).unwrap();
        running.lock().unwrap().insert(
            "job-actor".into(),
            RunHandle {
                cancel: crate::engine::Cancel::new(),
                gen: 1,
                queued: false,
                cancelled: false,
            },
        );
        let (code, v) = route_as(
            "POST",
            "/jobs/job-actor/pause",
            b"",
            &store,
            &running,
            &scheduler,
            Some("claude"),
            None,
            None,
        );
        assert_eq!(code, 202, "{v}");
        let events = store.events_since("job-actor", 0, 10).unwrap();
        let paused = events
            .iter()
            .find(|e| e.kind == "paused")
            .expect("缺 paused 事件");
        assert_eq!(paused.data["by"], "agent:claude");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cancel_unknown_id_is_404_and_external_process_is_409() {
        let (store, running, scheduler, dir) = harness("cancel-ext");
        let (code, v) = route(
            "POST",
            "/jobs/nope/cancel",
            b"",
            &store,
            &running,
            &scheduler,
        );
        assert_eq!(code, 404, "{v}");
        assert_eq!(v["code"], "job.not_found");
        assert_eq!(store.last_event_seq("nope").unwrap(), 0, "不该给不存在的任务写事件");

        let mut child = std::process::Command::new("sleep").arg("2").spawn().unwrap();
        let mut t = Task::new("http://127.0.0.1/x", &dir.join("out").display().to_string());
        t.id = "job-external".into();
        t.state = crate::model::STATE_RUNNING.to_string();
        t.pid = child.id() as i64;
        store.save(&t).unwrap();
        let (code, v) = route(
            "POST",
            "/jobs/job-external/cancel",
            b"",
            &store,
            &running,
            &scheduler,
        );
        assert_eq!(code, 409, "{v}");
        assert_eq!(v["code"], "job.external");
        assert!(store.get("job-external").unwrap().is_some(), "外部进程的任务不能被删");
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn token_callers_only_manage_their_own_pins() {
        let (store, running, scheduler, dir) = harness("pins-scope");
        let sha1 = "11".repeat(32);
        let sha2 = "22".repeat(32);

        // 带 claude token 创建 pin；请求体自称 pi 应被忽略。
        let body = serde_json::json!({"sha256": sha1, "agent": "pi"}).to_string();
        let (code, v) = route_as(
            "POST",
            "/pins",
            body.as_bytes(),
            &store,
            &running,
            &scheduler,
            Some("claude"),
            None,
            None,
        );
        assert_eq!(code, 201, "{v}");

        store.pin(&sha2, "agent:pi", 3600, "").unwrap();
        let (code, v) = route_as(
            "GET",
            "/pins",
            b"",
            &store,
            &running,
            &scheduler,
            Some("claude"),
            None,
            None,
        );
        assert_eq!(code, 200);
        let pins = v.as_array().unwrap();
        assert_eq!(pins.len(), 1, "{v}");
        assert_eq!(pins[0]["agent"], "agent:claude");

        // claude 删不掉 pi 的 pin。
        let (code, _) = route_as(
            "DELETE",
            &format!("/pins/{sha2}?agent=agent:pi"),
            b"",
            &store,
            &running,
            &scheduler,
            Some("claude"),
            None,
            None,
        );
        assert_eq!(code, 404);
        assert!(store.pins().unwrap().iter().any(|p| p.sha256 == sha2));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pins_can_be_created_listed_and_removed() {
        let (store, running, scheduler, dir) = harness("pins");
        let sha = "cd".repeat(32);
        let body = serde_json::to_vec(&serde_json::json!({
            "sha256": sha,
            "agent": "agent:claude",
            "ttl_secs": 3600,
            "note": "model in use",
        }))
        .unwrap();
        let (code, v) = route("POST", "/pins", &body, &store, &running, &scheduler);
        assert_eq!(code, 201, "{v}");
        assert_eq!(v["sha256"], sha);
        assert_eq!(v["agent"], "agent:claude");

        let (code, list) = route("GET", "/pins", b"", &store, &running, &scheduler);
        assert_eq!(code, 200);
        assert_eq!(list.as_array().unwrap().len(), 1);

        let (code, v) = route(
            "DELETE",
            &format!("/pins/{sha}?agent=agent:claude"),
            b"",
            &store,
            &running,
            &scheduler,
        );
        assert_eq!(code, 200, "{v}");
        assert!(store.pins().unwrap().is_empty());

        let bad = serde_json::to_vec(&serde_json::json!({"sha256": "not-a-sha"})).unwrap();
        let (code, _) = route("POST", "/pins", &bad, &store, &running, &scheduler);
        assert_eq!(code, 400);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn queued_pause_then_resume_reuses_the_same_run() {
        let running: Running = Arc::new(Mutex::new(HashMap::new()));
        let cancel = crate::engine::Cancel::new();
        let gen = 42;
        running.lock().unwrap().insert(
            "job-q".into(),
            RunHandle {
                cancel: cancel.clone(),
                gen,
                queued: true,
                cancelled: false,
            },
        );

        // pause：只置信号，不标记 cancelled
        cancel.cancel();
        assert!(!mark_started(&running, "job-q", gen), "暂停的队列项不该开跑");
        // 注意 mark_started 已经把 queued 置 false；真实流程里 pause 在出队前。
        // 这里重新模拟「还没出队」的状态，验证 resume 能复位。
        running.lock().unwrap().insert(
            "job-q".into(),
            RunHandle {
                cancel: cancel.clone(),
                gen,
                queued: true,
                cancelled: false,
            },
        );
        assert!(resume_queued(&running, "job-q", gen));
        assert!(!cancel.cancelled(), "resume 必须把暂停信号复位");
        assert!(mark_started(&running, "job-q", gen), "复位后同一个队列项能继续跑");
        finish_run(&running, "job-q", gen);
        assert!(!running.lock().unwrap().contains_key("job-q"));
    }

    #[test]
    fn cancelled_run_cannot_be_resumed_even_if_still_queued() {
        let running: Running = Arc::new(Mutex::new(HashMap::new()));
        let cancel = crate::engine::Cancel::new();
        let gen = 43;
        running.lock().unwrap().insert(
            "job-c".into(),
            RunHandle {
                cancel: cancel.clone(),
                gen,
                queued: true,
                cancelled: true,
            },
        );
        cancel.cancel();
        assert!(!resume_queued(&running, "job-c", gen));
        assert!(!mark_started(&running, "job-c", gen));
        // 旧 generation 的退出不能删掉新 generation 的句柄
        finish_run(&running, "job-c", gen + 1);
        assert!(running.lock().unwrap().contains_key("job-c"));
        finish_run(&running, "job-c", gen);
        assert!(!running.lock().unwrap().contains_key("job-c"));
    }

    #[test]
    fn forget_refuses_a_job_that_is_still_queued_or_running() {
        let (store, running, _scheduler, dir) = harness("forget");
        let mut t = Task::new("http://127.0.0.1/x", &dir.join("out").display().to_string());
        t.id = "job-live".into();
        store.save(&t).unwrap();
        running.lock().unwrap().insert(
            "job-live".into(),
            RunHandle {
                cancel: crate::engine::Cancel::new(),
                gen: 1,
                queued: true,
                cancelled: false,
            },
        );
        let (code, v) = route(
            "DELETE",
            "/jobs/job-live",
            b"",
            &store,
            &running,
            &_scheduler,
        );
        assert_eq!(code, 409, "{v}");
        assert_eq!(v["code"], "job.running");
        assert!(store.get("job-live").unwrap().is_some());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn queue_full_returns_429_and_releases_the_idempotency_key() {
        let (store, running, _unused, dir) = harness("queuefull");
        let scheduler = Scheduler::new(1, 1, 1);
        // 占住唯一的 worker，再占住唯一的队列位 —— 这样下一条必然 Full。
        let (hold_tx, hold_rx) = std::sync::mpsc::channel::<()>();
        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        let hold_rx = Arc::new(Mutex::new(hold_rx));
        let rx = hold_rx.clone();
        scheduler
            .submit(
                "blocker",
                Box::new(move || {
                    let _ = started_tx.send(());
                    let _ = rx.lock().unwrap().recv();
                }),
            )
            .unwrap();
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("blocker 没开跑");
        scheduler.submit("filler", Box::new(|| {})).unwrap();

        let body = serde_json::to_vec(&serde_json::json!({
            "url": "http://127.0.0.1/x",
            "dest": dir.join("out").display().to_string(),
            "source": "agent:test",
            "idempotency_key": "queue-test",
        }))
        .unwrap();
        let (code, v) = route("POST", "/jobs", &body, &store, &running, &scheduler);
        assert_eq!(code, 429, "{v}");
        assert_eq!(v["code"], "queue.full");
        assert_eq!(v["retryable"], true);
        // 队列满的新任务不该留在台账里，也不该占着幂等键 —— 否则重试会拿到幽灵。
        assert!(store
            .find_idem("idem:agent:test:queue-test")
            .unwrap()
            .is_none());
        assert!(store.list(10).unwrap().is_empty());

        let _ = hold_tx.send(());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn overly_long_idempotency_key_is_rejected() {
        let (store, running, scheduler, dir) = harness("idem-long");
        let body = serde_json::to_vec(&serde_json::json!({
            "url": "http://127.0.0.1/x",
            "dest": dir.join("out").display().to_string(),
            "idempotency_key": "x".repeat(129),
        }))
        .unwrap();
        let (code, v) = route("POST", "/jobs", &body, &store, &running, &scheduler);
        assert_eq!(code, 400, "{v}");
        assert_eq!(v["code"], "input.bad_idempotency_key");
        assert!(store.list(10).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn idempotency_key_is_scoped_per_agent() {
        let (store, running, scheduler, dir) = harness("idem-scope");
        let body = serde_json::to_vec(&serde_json::json!({
            "url": "http://127.0.0.1/x",
            "dest": dir.join("out").display().to_string(),
            "idempotency_key": "shared-key",
        }))
        .unwrap();
        let submit_as = |name: &str| {
            route_as(
                "POST",
                "/jobs",
                &body,
                &store,
                &running,
                &scheduler,
                Some(name),
                None,
                None,
            )
        };
        let (code, a) = submit_as("claude");
        assert_eq!(code, 202, "{a}");
        let (code, b) = submit_as("pi");
        assert_eq!(code, 202, "{b}");
        assert_ne!(a["id"], b["id"], "不同 agent 同 key 不能 dedupe 到一起：{a} / {b}");
        let (code, a2) = submit_as("claude");
        assert_eq!(code, 200, "{a2}");
        assert_eq!(a2["id"], a["id"], "同 agent 重试仍要命中");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn same_intent_returns_the_same_job_and_events_are_replayable() {
        let (store, running, scheduler, dir) = harness("idem");
        let out = dir.join("out");
        let body = serde_json::to_vec(&serde_json::json!({
            "url": "http://user:pass@127.0.0.1/x",
            "dest": out.display().to_string(),
            "source": "agent:test",
            "idempotency_key": "run-1",
        }))
        .unwrap();

        let (code, v) = route("POST", "/jobs", &body, &store, &running, &scheduler);
        assert_eq!(code, 202, "{v}");
        let id = v["id"].as_str().unwrap().to_string();
        assert_eq!(v["state"], "queued");

        // 同一个幂等键再提交：必须是同一条，不是第二条下载。
        let (code2, v2) = route("POST", "/jobs", &body, &store, &running, &scheduler);
        assert_eq!(code2, 200, "{v2}");
        assert_eq!(v2["id"], id);
        assert_eq!(v2["deduped"], true);

        // 工作线程会因「URL 里带账号密码」被安全边界拒绝；等它把 failed 写进事件流。
        let path = format!("/jobs/{id}/events?since=0&wait_ms=0");
        let mut events = Vec::new();
        for _ in 0..100 {
            let (code, ev) = route("GET", &path, b"", &store, &running, &scheduler);
            assert_eq!(code, 200, "{ev}");
            events = ev["events"].as_array().cloned().unwrap_or_default();
            if events.iter().any(|e| e["type"] == "failed") {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(events.iter().any(|e| e["type"] == "queued"), "事件流要从 queued 开始：{events:?}");
        let failed = events
            .iter()
            .find(|e| e["type"] == "failed")
            .unwrap_or_else(|| panic!("没有 failed 事件：{events:?}"));
        assert_eq!(failed["data"]["code"], "policy.credentials_in_url");

        // since= 从中间接着读，不能重复前面的。
        let after = route(
            "GET",
            &format!("/jobs/{id}/events?since=1&wait_ms=0"),
            b"",
            &store,
            &running,
            &scheduler,
        );
        assert_eq!(after.0, 200);
        let after_seq: Vec<i64> = after.1["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["seq"].as_i64().unwrap())
            .collect();
        assert!(after_seq.iter().all(|s| *s > 1));

        let _ = std::fs::remove_dir_all(dir);
    }
}
