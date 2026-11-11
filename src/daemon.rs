//! 天网盒子 · 守护进程入口。
//!
//! 下载不归窗口管：窗口只是它的一个前端，关了、崩了、重开，任务照跑。
//! 这条是产品承诺的一部分 —— 关窗口不该等于「把正在下的大文件掐掉」。
//!
//! 新主名是 `skynet-dataplaned`；`skynet-downloadd` 作为旧名继续可用。
//! 两个名字共用同一份 pid 文件、同一个端口和同一本台账，不能各起一个。

use std::io::Write;
use std::path::PathBuf;

/// pid 文件**按端口分**：一个用户跑两个守护进程（比如一个默认目录、一个便携目录）
/// 是合理需求，不该被一个全局 pid 文件挡住。
///
/// 文件名保留 `skynet-downloadd-*`：升级后新旧二进制必须认同一个 pid，
/// 否则 `skynet-dataplaned` 和 `skynet-downloadd` 会各起一份、抢同一个端口。
fn pid_path(port: u16) -> PathBuf {
    crate::sys::pid_file(&format!("skynet-downloadd-{port}"))
}

/// 那个 pid 是不是我们的进程（而不是 pid 被别人重用了）。
///
/// 只看「进程还活着」会误判：机器重启、进程崩过之后，pid 文件里那个号很容易
/// 落到一个完全无关的进程身上，于是守护进程永远起不来、还一口咬定「已经有一个在跑」。
fn pid_is_ours(pid: i32) -> bool {
    // 精确匹配可执行文件名，不能用前缀：`skynet-dataplane`（GUI）也包含
    // `skynet-dataplane`，会把界面进程误认成守护进程。
    crate::sys::pid_is_binary(pid as i64, &["skynet-dataplaned", "skynet-downloadd"])
}

/// 已经有一个在跑就返回它的 pid。
fn claim(port: u16) -> Option<i32> {
    let path = pid_path(port);
    let mut stale = false;
    if let Ok(text) = std::fs::read_to_string(&path) {
        if let Ok(pid) = text.trim().parse::<i32>() {
            if pid > 0 && unsafe { libc::kill(pid, 0) } == 0 && pid_is_ours(pid) {
                return Some(pid);
            }
        }
        stale = true;
    }
    if stale {
        let _ = std::fs::remove_file(&path);
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&path, std::process::id().to_string());
    None
}

/// 给"空闲退出"用：pid 文件路径（按端口算不出来时用默认端口）。
///
/// 空闲退出的线程拿不到 `port` 变量（它在 `api::serve` 里），而 pid 文件是按端口命名的；
/// 这里用环境里能拿到的端口兜底，拿不到就 7788。
pub fn pid_path_for_idle() -> std::path::PathBuf {
    let port: u16 = std::env::args()
        .skip_while(|a| a != "--port" && a != "-p")
        .nth(1)
        .and_then(|v| v.parse().ok())
        .or_else(|| {
            std::env::args()
                .find_map(|a| a.strip_prefix("--port=").and_then(|v| v.parse().ok()))
        })
        .unwrap_or(7788);
    pid_path(port)
}

fn release(port: u16) {
    let _ = std::fs::remove_file(pid_path(port));
}

/// daemon 的 pid/socket 现场。`pid_running` 只认真正的 daemon 可执行文件，
/// 防止 pid 被系统复用后把一个无关进程当成守护进程。
#[derive(Debug, Clone)]
pub struct ListenerState {
    pub pid_file: PathBuf,
    pub pid: Option<i32>,
    pub pid_running: bool,
    pub socket: PathBuf,
    pub socket_exists: bool,
    pub socket_live: bool,
}

fn port_from_base(base: &str) -> u16 {
    url::Url::parse(base)
        .ok()
        .and_then(|u| u.port_or_known_default())
        .unwrap_or(7788)
}

fn listener_state(port: u16) -> ListenerState {
    let pid_file = pid_path(port);
    let pid = std::fs::read_to_string(&pid_file)
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .filter(|p| *p > 0);
    let pid_running = pid
        .map(|p| crate::sys::pid_alive(p as i64) && pid_is_ours(p))
        .unwrap_or(false);
    let socket = crate::uds::socket_path();
    ListenerState {
        pid_file,
        pid,
        pid_running,
        socket_exists: socket.exists(),
        socket_live: crate::uds::socket_live(),
        socket,
    }
}

/// 给 CLI 显示 daemon 状态；只读，不清理现场。
pub fn status(base: &str) -> ListenerState {
    listener_state(port_from_base(base))
}

/// 清理已经确认无效的 pid/socket。活着的 socket 一律保留，避免误伤同机另一个
/// daemon（socket 路径不按端口分）。
fn remove_stale_artifacts(port: u16) {
    let st = listener_state(port);
    if !st.pid_running {
        let _ = std::fs::remove_file(&st.pid_file);
    }
    if st.socket_exists && !st.socket_live {
        let _ = std::fs::remove_file(&st.socket);
    }
}

/// 停下按 pid 记录的直接拉起进程。找不到活进程时也会清掉死 pid/失效 socket，
/// 所以 `skyget daemon stop` 也是 `kill -9` 之后的收尾入口。
pub fn stop(base: &str) -> anyhow::Result<Option<i32>> {
    let port = port_from_base(base);
    let st = listener_state(port);
    let mut stopped = None;
    if st.pid_running {
        if let Some(pid) = st.pid {
            let rc = unsafe { libc::kill(pid, libc::SIGTERM) };
            if rc != 0 {
                return Err(std::io::Error::last_os_error())
                    .map_err(|e| anyhow::anyhow!("停不下守护进程 pid {pid}：{e}"));
            }
            for _ in 0..50 {
                if !(crate::sys::pid_alive(pid as i64) && pid_is_ours(pid)) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            if crate::sys::pid_alive(pid as i64) && pid_is_ours(pid) {
                anyhow::bail!("守护进程 pid {pid} 收到 SIGTERM 后 5 秒仍未退出");
            }
            stopped = Some(pid);
        }
    }
    remove_stale_artifacts(port);
    Ok(stopped)
}

/// 跑守护进程；`--port` / `-p` / `--port=NNNN`，默认 127.0.0.1:7788。
pub fn run() -> anyhow::Result<()> {
    // Linux 的进程名（comm）只有 15 字节：`skynet-downloadd` 会被内核截成
    // `skynet-download` —— 和界面的进程名一字不差。后果是 `pkill -x skynet-download`、
    // `pgrep -x skynet-download` 这种最自然的写法分不清谁是谁：
    // 用户想关窗口，顺手把守护进程也杀了，正在下的任务全断。
    // 所以这里自己把名字改短、改成不一样的一个。
    set_comm("skygetd");

    let mut port = 7788u16;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--port" | "-p" => {
                i += 1;
                port = args
                    .get(i)
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| anyhow::anyhow!("--port 后面要跟端口号"))?;
            }
            "--help" | "-h" => {
                println!("skynet-dataplaned [--port 7788]（旧名 skynet-downloadd）");
                return Ok(());
            }
            other => {
                if let Some(rest) = other.strip_prefix("--port=") {
                    port = rest
                        .parse()
                        .map_err(|_| anyhow::anyhow!("端口号不对：{rest}"))?;
                } else {
                    anyhow::bail!("不认识的参数：{other}（只认 --port）");
                }
            }
        }
        i += 1;
    }

    if let Some(pid) = claim(port) {
        println!("已经有一个在跑了（pid {pid}，端口 {port}）。");
        return Ok(());
    }
    remove_stale_artifacts(port);

    // 退出时把 pid 文件与 socket 清掉，否则下次启动会以为还有人在跑、
    // 或者留下一个连不上的 socket 文件。SIGTERM 默认是直接终止进程、不展开栈，
    // 所以不能只靠 Drop：这里装一个只做 unlink（asnyc-signal-safe）的处理函数。
    install_cleanup_handlers(port);

    // 退出时（正常返回 / panic 展开）再兜一次
    let _guard = Guard { port };

    eprintln!("天网盒子守护进程 · 127.0.0.1:{port}");
    flush();
    spawn_cache_gc();
    let r = crate::api::serve(port);
    release(port);
    r
}

/// 缓存水位线守护线程。
///
/// 单机数据盒子的头号故障模式不是下载慢，是“磁盘被缓存吃满”。
/// `skyget prune` 已经能把该清的清掉，但它要人记得手动跑 —— 而 agent 不会记得。
///
/// 配置（全在 daemon 侧，不由 agent 决定）：
/// - `SKYGET_CACHE_MAX_BYTES`：上限，认 `10GB` / `500MB` / 纯数字；不设 = 不自动清
/// - `SKYGET_CACHE_LOW_WATER`：清到多低，默认上限的 90%（留着别每 5 分钟清一次）
/// - `SKYGET_CACHE_GC_MIN_AGE`：最小年龄，默认 1h —— 刚下完的不算“没人要”
/// - `SKYGET_CACHE_GC_INTERVAL`：多久看一次，默认 5m
fn spawn_cache_gc() {
    let Some(max) = std::env::var("SKYGET_CACHE_MAX_BYTES")
        .ok()
        .and_then(|v| crate::quota::parse_size(v.trim()))
        .filter(|v| *v > 0)
    else {
        return;
    };
    let low = std::env::var("SKYGET_CACHE_LOW_WATER")
        .ok()
        .and_then(|v| crate::quota::parse_size(v.trim()))
        .unwrap_or(max / 10 * 9);
    let low = low.min(max);
    let min_age = std::env::var("SKYGET_CACHE_GC_MIN_AGE")
        .ok()
        .and_then(|v| crate::prune::parse_age(&v).ok())
        .unwrap_or(std::time::Duration::from_secs(3_600));
    let interval = std::env::var("SKYGET_CACHE_GC_INTERVAL")
        .ok()
        .and_then(|v| crate::prune::parse_age(&v).ok())
        .unwrap_or(std::time::Duration::from_secs(300))
        .max(std::time::Duration::from_secs(10));

    let spawned = std::thread::Builder::new()
        .name("cache-gc".into())
        .spawn(move || {
            loop {
                std::thread::sleep(interval);
                // 台账单独开一条连接：WAL 允许读写并发，不用抢请求线程那把锁。
                let Ok(store) = crate::Store::open() else {
                    continue;
                };
                match crate::prune::enforce_budget(&store, max, low, min_age) {
                    Ok(ev) => {
                        crate::metrics::cache_after_gc(ev.total_after);
                        if ev.blobs > 0 {
                            eprintln!(
                                "缓存超过 {}，清掉 {} 块 / {}，现在 {}（保护 pin、落地链接和 {} 小时内新建的）",
                                crate::engine::human(max),
                                ev.blobs,
                                crate::engine::human(ev.bytes),
                                crate::engine::human(ev.total_after),
                                min_age.as_secs() / 3600,
                            );
                            crate::metrics::cache_evicted(ev.blobs as u64, ev.bytes);
                        }
                    }
                    Err(e) => eprintln!("缓存清扫失败：{e:#}"),
                }
            }
        });
    if spawned.is_err() {
        eprintln!("起不了缓存清扫线程（继续跑，只是缓存不会被自动清）");
    }
}

/// 守护进程是谁拉起来的。给 CLI / GUI 报告用，也决定"要不要等它"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ensured {
    /// 本来就在跑（健康检查通过）
    Already,
    /// 交给 systemd 用户服务启动（推荐：策略/环境来自 unit，不来自调用方）
    BySystemd(String),
    /// systemd 不可用，直接在本机 spawn 了一个（活不过注销）
    Spawned(i32),
    /// 起不来
    Failed(String),
}

/// 确保守护进程在跑：先问 `/health`，不通就 `systemctl --user start`，
/// 再不行才直接 spawn 同目录的二进制。
///
/// 这是"随 agent 启动"的核心原语：agent 一跑 `skyget …` 或 MCP 工具，
/// 这里就把数据盒子带起来，不需要开机自启。
///
/// 优先 systemd 的原因很实在：**策略与凭证来自 unit 的 EnvironmentFile**，
/// 而不是当前这个 agent 的环境 —— 否则 agent 自己设个 `SKYGET_ALLOW_PRIVATE=1`
/// 就能把自己拉起的数据盒子放宽。systemd 不可用时才退化成直启，并在返回值里说清楚。
pub fn ensure(base: &str) -> Ensured {
    if health_ok(base) {
        return Ensured::Already;
    }
    for unit in ["skynet-dataplaned", "skynet-downloadd"] {
        let ok = std::process::Command::new("systemctl")
            .args(["--user", "start", unit])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok && wait_health(base) {
            return Ensured::BySystemd(unit.to_string());
        }
    }
    let Ok(exe) = std::env::current_exe() else {
        return Ensured::Failed("拿不到自己的路径".into());
    };
    let Some(dir) = exe.parent() else {
        return Ensured::Failed("拿不到自己的目录".into());
    };
    for name in ["skynet-dataplaned", "skynet-downloadd"] {
        let cand = dir.join(name);
        if !cand.is_file() {
            continue;
        }
        let port = port_from_base(base).to_string();
        if let Ok(child) = std::process::Command::new(&cand)
            .arg("--port")
            .arg(&port)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            if wait_health(base) {
                return Ensured::Spawned(child.id() as i32);
            }
        }
    }
    Ensured::Failed(format!("起不来：{base}/health 一直不通"))
}

fn health_ok(base: &str) -> bool {
    reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_millis(800))
        .build()
        .ok()
        .and_then(|c| c.get(format!("{}/health", base.trim_end_matches('/'))).send().ok())
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

fn wait_health(base: &str) -> bool {
    for _ in 0..25 {
        if health_ok(base) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(120));
    }
    false
}

/// 收到 SIGTERM/SIGINT 时清理两个文件再退出。
///
/// 只做 `unlink` + `_exit`：POSIX 明确把 `unlink` 列在 async-signal-safe 列表里，
/// 所以在信号处理函数里调用它是安全的；不在这里加锁、不分配内存、不写日志。
fn install_cleanup_handlers(port: u16) {
    use std::sync::OnceLock;
    // 处理函数必须是 `extern "C"`，也就拿不到闭包 —— 路径只能放进静态区。
    static PID: OnceLock<std::ffi::CString> = OnceLock::new();
    static SOCK: OnceLock<std::ffi::CString> = OnceLock::new();
    let _ = PID.set(
        std::ffi::CString::new(pid_path(port).display().to_string()).unwrap_or_default(),
    );
    let _ = SOCK.set(
        std::ffi::CString::new(crate::uds::socket_path().display().to_string())
            .unwrap_or_default(),
    );

    extern "C" fn on_term(_sig: libc::c_int) {
        unsafe {
            if let Some(p) = PID.get() {
                libc::unlink(p.as_ptr());
            }
            if let Some(s) = SOCK.get() {
                libc::unlink(s.as_ptr());
            }
            libc::_exit(0);
        }
    }
    unsafe {
        libc::signal(libc::SIGTERM, on_term as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_term as *const () as libc::sighandler_t);
    }
}

struct Guard {
    port: u16,
}
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = release(self.port);
        remove_stale_artifacts(self.port);
    }
}

/// 改进程名（comm）。失败就算了 —— 这是为了方便认，不影响下载。
fn set_comm(name: &str) {
    crate::sys::set_process_name(name);
}

fn flush() {
    let _ = std::io::stdout().flush();
}
