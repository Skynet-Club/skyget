//! 下载引擎：探测 → 分段 → 续传 → 校验 → 内容寻址落地。
//!
//! 三条不妥协的事：
//! 1. 断点续传落到盘上（`.part` + 段位图），进程被杀也能接着下；
//! 2. 完成时一定算 sha256；调用方给了期望值就一定比对，不符判失败；
//! 3. 内容按 hash 存一份（`blobs/<xx>/<hash>`），同内容只下一次、只存一份。

use std::collections::{HashMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use reqwest::blocking::Client;
use reqwest::header::{
    ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG, RANGE,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::lock::PartLock;
use crate::model::{now_ms, Task, STATE_DONE, STATE_FAILED, STATE_RUNNING};
use crate::paths;
use crate::store::Store;

const MIN_CHUNK: u64 = 1 << 20; // 1 MiB：再小就只剩请求开销
const MAX_CHUNK: u64 = 32 << 20; // 32 MiB
const MAX_TRIES: usize = 4;
const BUF: usize = 64 << 10;

#[derive(Debug, Clone)]
pub enum Event {
    /// 正在问服务器这个地址是什么
    Probing,
    /// 定好了分段方案
    Planned {
        total: u64,
        ranges: bool,
        threads: usize,
    },
    Progress {
        done: u64,
        total: u64,
    },
    /// 切片流按播放时长报进度（字节数最后一刻才知道，按字节画就是在骗人）
    ProgressSecs {
        done: u64,
        total: u64,
    },
    /// 线路：BT 是 已连接/发现 的对等端；直链是分段连接数（seen = 0）
    Links {
        live: u32,
        seen: u32,
    },
    /// 一个阶段的耗时（毫秒）。给 agent/运维回答「慢在哪」：
    /// 探测（DNS/TTFB）、传输、校验、落地。不是总耗时 —— 总耗时看 elapsed_ms。
    Phase { name: &'static str, ms: u64 },
    /// 给人看的一句提示（不是错误）
    Note(String),
    /// 真正的名字定下来了（BT 的种子文件名不是内容名，拿到 metadata 才知道）
    Named(String),
    /// 正在算 hash
    Verifying,
    /// 完成；cached = 这份内容本机早就有了（没重新下载）
    Finished {
        cached: bool,
    },
}

pub struct Engine {
    pub threads: usize,
    pub client: Client,
    /// 出网 / 落地的边界。CLI 默认宽松，daemon 用 strict。
    pub policy: crate::security::Policy,
    /// 命中缓存时是否重算 sha256。
    ///
    /// 缓存块是 `blobs/<sha>` 的普通文件，名字不等于内容 —— 同机的另一个 agent
    /// 可以把它换成别的内容。daemon 是多 agent 共享的信任边界，所以默认校验；
    /// 人类 CLI 保持秒传速度，需要时用 `SKYGET_VERIFY_CACHE=1` 打开。
    pub verify_cache: bool,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new(5)
    }
}

impl Engine {
    pub fn new(threads: usize) -> Self {
        let verify = std::env::var("SKYGET_VERIFY_CACHE")
            .map(|v| v == "1")
            .unwrap_or(false);
        Self::with_options(threads, crate::security::Policy::from_env(), verify)
    }

    /// 多 agent 通道用的引擎：只放行公网 + 缓存命中必须重算 sha。
    pub fn strict(threads: usize) -> Self {
        Self::with_options(threads, crate::security::Policy::strict(), true)
    }

    pub fn with_options(
        threads: usize,
        policy: crate::security::Policy,
        verify_cache: bool,
    ) -> Self {
        let redirect_policy = policy.clone();
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            // 读超时：**没有这一条，一个卡住的源能让任务永远显示「下载中」**。
            // 实测过：连接建立成功、五个分段都挂着、一个字节也不来，
            // 于是一小时后界面上还是「下载中 24.0MB / 110.4MB」，进度条不动、
            // 也不报错 —— 比失败难查得多（失败至少告诉你下一步做什么）。
            // 90 秒还没把这一段（1MB）收完 = 这一次就算卡住，交给上面那层去分类重试。
            // 1MB / 90 秒 ≈ 11KB/s 的下限，比这更慢的源本来就该报出来而不是挂着。
            // （blocking 客户端没有单独的读超时，只能给整条请求兜个上限。）
            .timeout(Duration::from_secs(90))
            .user_agent(concat!("skynet-download/", env!("CARGO_PKG_VERSION")))
            // 重定向不能绕过边界：每一跳都按同一套策略检查。
            // 以前只检查初始地址，302 到 169.254.169.254 就绕过去了。
            .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                if attempt.previous().len() >= redirect_policy.max_redirects {
                    return attempt.error("重定向次数过多");
                }
                match crate::security::check_url(attempt.url().as_str(), &redirect_policy) {
                    Ok(_) => attempt.follow(),
                    Err(e) => attempt.error(format!("{e:#}")),
                }
            }))
            .build()
            .expect("构造 HTTP 客户端失败");
        Self {
            // 0 是「按体积自己定」，要原样留住；其它值仍旧夹在 1..=32。
            threads: if threads == 0 { 0 } else { threads.clamp(1, 32) },
            client,
            policy,
            verify_cache,
        }
    }
}

// ── 暂停 / 取消 ─────────────────────────────────────────────────────────

/// 用户主动按了停：这是**意图**，不是故障。
///
/// 调用方靠它区分「下不下来」和「我不想下了」——前者记 failed，后者记 interrupted，
/// 而且不能把已下的分片当垃圾清掉（下次接着下）。
#[derive(Debug)]
pub struct Stopped;

/// 同一内容并发只允许一个等待者；更多后来者直接各自下载，不把 worker 全占住。
static INFLIGHT_WAITER: AtomicBool = AtomicBool::new(false);

struct WaiterGuard;
impl Drop for WaiterGuard {
    fn drop(&mut self) {
        INFLIGHT_WAITER.store(false, Ordering::Release);
    }
}

/// 只标记「这个任务在等」，心跳据此不报线路卡住。全局槽不能拿来判断：
/// 那会让一个等待者把所有其他任务的卡住提示一起静音。
struct WaitingGuard<'a>(&'a AtomicBool);
impl Drop for WaitingGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// 等一个正在跑的同 sha 任务把 blob 落进缓存。最多等 30 分钟；
/// 对端失败/取消、自己取消、或超时就返回（随后按正常路径下载）。
fn wait_for_inflight(
    store: &Store,
    expected: &str,
    self_id: &str,
    cancel: &Cancel,
    cb: &(dyn Fn(Event) + Sync),
    waiting: &AtomicBool,
) -> Result<()> {
    if INFLIGHT_WAITER
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return Ok(());
    }
    let _guard = WaiterGuard;
    let peer = store
        .active_tasks(2000)
        .unwrap_or_default()
        .into_iter()
        .find(|t| {
            t.id != self_id
                && t.direction == crate::model::DIR_DOWNLOAD
                && t.state_for_display() == "running"
                && t.expected_sha256.eq_ignore_ascii_case(expected)
        });
    let Some(peer) = peer else {
        return Ok(());
    };
    waiting.store(true, Ordering::Release);
    let _waiting = WaitingGuard(waiting);
    // 不报对端 job id：另一个 agent 的任务对当前 agent 不可见，id 只会造成越权信息泄露。
    cb(Event::Note(
        "同内容已有任务在下载，等它完成后直接秒传".into(),
    ));
    let deadline = Instant::now() + Duration::from_secs(30 * 60);
    while Instant::now() < deadline {
        if cancel.cancelled() {
            return Err(anyhow::Error::new(Stopped));
        }
        if blob_of(expected).is_some() {
            return Ok(());
        }
        match store.get(&peer.id) {
            Ok(Some(p)) if p.state_for_display() == "running" => {}
            // 对端结束/失败/取消：没 blob 就自己下，别永远等。
            _ => return Ok(()),
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Ok(())
}

impl std::fmt::Display for Stopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "已按暂停")
    }
}

impl std::error::Error for Stopped {}

/// 停止信号：界面按「暂停」时调用 `cancel()`，下载线程在每块数据之间自己看一眼。
///
/// 合作式而不是杀线程——被杀的那一刻如果正在 `write_at`，位图和盘上的字节就对不上了。
#[derive(Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    /// 不需要暂停的调用方（CLI、时钟）拿这个：永远不响
    pub fn never() -> Self {
        Self::new()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// 撤销暂停：只用于「还在队列里、没被 worker 拿走」的任务。
    /// 已经在下载的任务不该被复位 —— 那会让它继续跑，违背取消的意图。
    pub fn reset(&self) {
        self.0.store(false, Ordering::Relaxed);
    }

    pub fn cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// 跑完一个任务：成功 Ok（task 已置 done），失败 Err（task 已置 failed，error 是给人看的话）。
pub fn run(
    store: &Store,
    engine: &Engine,
    task: &mut Task,
    cb: &(dyn Fn(Event) + Sync),
) -> Result<()> {
    run_cancellable(store, engine, task, cb, &Cancel::never())
}

/// 同上，但可以在中途喊停（界面上的「暂停」走这条）。
pub fn run_cancellable(
    store: &Store,
    engine: &Engine,
    task: &mut Task,
    cb: &(dyn Fn(Event) + Sync),
    cancel: &Cancel,
) -> Result<()> {
    task.state = STATE_RUNNING.to_string();
    task.pid = std::process::id() as i64;
    task.updated_at = now_ms();
    let _ = store.save(task);

    // 心跳：每秒把已下字节写进台账。没有这个，「界面看见 agent 在下载」就只是启动那一刻的
    // 快照 —— CLI/接口起的任务在窗口里会一直显示 0 字节。
    let live = Arc::new(AtomicU64::new(0));
    let live_total = Arc::new(AtomicU64::new(0));
    let links_live = Arc::new(AtomicU32::new(0));
    let links_seen = Arc::new(AtomicU32::new(0));
    // 只有本任务在等同内容任务时为 true；不能共用全局槽，否则会静音别人的卡住提示。
    let waiting = Arc::new(AtomicBool::new(false));
    let beat_id = task.id.clone();
    // 心跳线程用「可被打断的等」而不是 sleep：收尾时立刻叫醒它，
    // 否则每次下载结束都要白等它睡满 900ms（秒传/缓存命中的任务里，这 0.9 秒就是全部耗时）。
    let (beat_tx, beat_rx) = std::sync::mpsc::channel::<()>();
    let beat_rx = std::sync::Mutex::new(beat_rx);
    // 网络类失败自己再试两次：退避 2 秒、4 秒。已下的分片会接着用，
    // 所以「重试」不会把已经下好的几百兆重来一遍。
    let mut attempt = 0u32;
    let mut wait = Duration::from_secs(2);
    // 卡住检测用：上一次看到的字节数、从什么时候开始没动、说过没有
    let mut last_seen = 0u64;
    let mut stuck_since = std::time::Instant::now();
    let mut said_stuck = false;
    let outcome = loop {
        attempt += 1;
        let r = std::thread::scope(|s| {
            s.spawn(|| loop {
                let r = beat_rx.lock().unwrap().recv_timeout(Duration::from_millis(900));
                match r {
                    Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                }
                let now_bytes = live.load(Ordering::Relaxed);
                let _ = store.save_progress(
                    &beat_id,
                    now_bytes,
                    live_total.load(Ordering::Relaxed),
                    links_live.load(Ordering::Relaxed),
                    links_seen.load(Ordering::Relaxed),
                );
                // 卡住要说话：45 秒一个字节没进，先告诉人「在等」，别让人盯着不动的条猜。
                // 只有本任务在等同内容的任务时不算“线路卡住”，把计时器往前走。
                if waiting.load(Ordering::Relaxed) {
                    last_seen = now_bytes;
                    stuck_since = std::time::Instant::now();
                    said_stuck = false;
                } else if now_bytes != last_seen {
                    last_seen = now_bytes;
                    stuck_since = std::time::Instant::now();
                    said_stuck = false;
                } else if !said_stuck && stuck_since.elapsed() > Duration::from_secs(45) {
                    said_stuck = true;
                    cb(Event::Note(
                        "45 秒没有一个字节进来，多半是这条线路卡住了（请求会在 90 秒自己超时，然后重试）"
                            .to_string(),
                    ));
                }
            });
            let r = run_inner(
                store,
                engine,
                task,
                cb,
                &live,
                &live_total,
                &links_live,
                &links_seen,
                cancel,
                &waiting,
            );
            let _ = beat_tx.send(());
            r
        });
        match r {
            Ok(v) => break Ok(v),
            Err(e) => {
                let kind = classify_failure(&format!("{e:#}"));
                // 主动停、以及「重试也没用」的错，直接收工
                let retriable = matches!(kind, Failure::Network | Failure::Throttle);
                if !retriable || attempt >= 3 || cancel.cancelled() {
                    break Err(e);
                }
                cb(Event::Note(format!(
                    "{}，{:.0} 秒后自动重试（第 {attempt} 次）",
                    if kind == Failure::Network {
                        "网络不通"
                    } else {
                        "被服务器限流"
                    },
                    wait.as_secs_f32()
                )));
                std::thread::sleep(wait);
                wait *= 2;
            }
        }
    };

    match outcome {
        Ok(cached) => {
            task.state = STATE_DONE.to_string();
            task.cached = cached; // 落库：事后才算得出「这台机器省了多少网络」
            task.error.clear();
            task.error_code.clear();
            task.retryable = false;
            task.retry_after_ms = 0;
            task.next_action.clear();
            task.updated_at = now_ms();
            // 终态只更新已存在的行：取消/删除可能已经发生在另一个进程里，
            // INSERT ON CONFLICT 会把删掉的任务写回来。
            if !store.save_if_exists(task).unwrap_or(false) {
                return Err(anyhow!("任务记录已被删除（可能在另一处取消）"));
            }
            cb(Event::Finished { cached });
            Ok(())
        }
        Err(e) if e.downcast_ref::<Stopped>().is_some() => {
            // 主动停：记成「中断」，已下的分片留着，下次接着下
            task.state = crate::model::STATE_INTERRUPTED.to_string();
            // 盘上已经有多少就是多少：写 0 的话，「中断」那行会显示成没下过
            task.bytes_done = live.load(Ordering::Relaxed).max(task.bytes_done);
            task.error = "已暂停，继续下载不会从头来".to_string();
            task.error_code = "job.paused".to_string();
            task.retryable = true;
            task.retry_after_ms = 0;
            task.next_action = "继续即可；已下的分片会接着用。".to_string();
            task.updated_at = now_ms();
            let _ = store.save_if_exists(task);
            Err(e)
        }
        Err(e) => {
            let msg = format!("{e:#}");
            task.state = STATE_FAILED.to_string();
            // 失败也要如实记下「已经下到多少」：曾经这里留着内存里那个 0，
            // 界面上就成了「失败 · 0 B / 110MB」，可盘上明明有 24MB 分片 ——
            // 用户看到这个数字只会以为白下了。
            task.bytes_done = live.load(Ordering::Relaxed).max(task.bytes_done);
            task.links_live = 0;
            task.links_seen = 0;
            task.error = msg.clone();
            let info = classify_error(&e);
            crate::metrics::note_error_code(info.code);
            task.error_code = info.code.to_string();
            task.retryable = info.retryable;
            task.retry_after_ms = info.retry_after_ms;
            task.next_action = info.next_action.to_string();
            task.updated_at = now_ms();
            let _ = store.save_if_exists(task);
            Err(anyhow!(msg))
        }
    }
}

/// 多源入口：单源直接走 `run_one`，多源按顺序回退。
#[cfg_attr(not(feature = "bt"), allow(unused_variables))]
/// sources 可以只放镜像（不含主 url）；只要非空就进多源路径。
/// 以前用 `len() > 1`，SDK 调用方按 model.rs 的文档给一个镜像时会被静默忽略。
fn has_mirrors(task: &Task) -> bool {
    !task.sources.is_empty()
}

fn run_inner(
    store: &Store,
    engine: &Engine,
    task: &mut Task,
    cb: &(dyn Fn(Event) + Sync),
    live: &Arc<AtomicU64>,
    live_total: &Arc<AtomicU64>,
    links_live: &Arc<AtomicU32>,
    links_seen: &Arc<AtomicU32>,
    cancel: &Cancel,
    waiting: &AtomicBool,
) -> Result<bool> {
    if has_mirrors(task) {
        return run_multi(
            store, engine, task, cb, live, live_total, links_live, links_seen, cancel, waiting,
        );
    }
    run_one(
        store, engine, task, cb, live, live_total, links_live, links_seen, cancel, waiting, &[],
    )
}

/// 依次尝试镜像。每个源用自己的 `.part`（按 URL），不跨源复用未校验的分片：
/// 网络失败保留该源自己的进度；内容说谎/HTTP 硬错误清掉所有源的分片再换源。
fn run_multi(
    store: &Store,
    engine: &Engine,
    task: &mut Task,
    cb: &(dyn Fn(Event) + Sync),
    live: &Arc<AtomicU64>,
    live_total: &Arc<AtomicU64>,
    links_live: &Arc<AtomicU32>,
    links_seen: &Arc<AtomicU32>,
    cancel: &Cancel,
    waiting: &AtomicBool,
) -> Result<bool> {
    // 主 url 也算一个源；去重但保序。
    let primary = task.url.clone();
    let mut sources: Vec<String> = Vec::new();
    for s in std::iter::once(primary.clone()).chain(task.sources.iter().cloned()) {
        if !s.trim().is_empty() && !sources.contains(&s) {
            sources.push(s);
        }
    }
    // 只有 pin 了 sha 才敢跨源拼：分片会来自不同主机，唯一能证明「拼起来是对的」
    // 就是内容哈希。没有 pin 就老老实实一个一个试，那是唯一安全的做法。
    if sources.len() > 1 && crate::security::is_sha256_hex(&task.expected_sha256) {
        task.url = primary.clone();
        let out = run_one(
            store, engine, task, cb, live, live_total, links_live, links_seen, cancel, waiting,
            &sources[1..],
        );
        task.url = primary.clone();
        let Err(e) = out else {
            return out;
        };
        // 分片来自多个源时，「校验不符」十有八九是某个镜像在说谎（长度对、内容错）。
        //
        // 以前这里只「改用主源重试一次」—— 如果**说谎的正好是主源**，那一次必然也失败，
        // 哪怕旁边就有一个好镜像。所以现在逐个源单独试：混合拼装已经不可信了，
        // 唯一能证明「这个源给的是对的内容」的办法就是拿它自己整份跑一遍。
        if format!("{e:#}").contains("校验不符") {
            cb(Event::Note(
                "多源校验不符：可能有源给了错的内容。清掉分片，逐个源单独重试".to_string(),
            ));
            let _ = discard_task_parts(task);
            let mut last_err = e;
            for (i, src) in sources.iter().enumerate() {
                if cancel.cancelled() {
                    task.url = primary.clone();
                    bail!(Stopped);
                }
                task.url = src.clone();
                match run_one(
                    store, engine, task, cb, live, live_total, links_live, links_seen, cancel,
                    waiting, &[],
                ) {
                    Ok(v) => {
                        task.url = primary.clone();
                        return Ok(v);
                    }
                    Err(one) => {
                        if cancel.cancelled() || one.downcast_ref::<Stopped>().is_some() {
                            task.url = primary.clone();
                            return Err(one);
                        }
                        // 事件流里要能看出「换源发生过」：否则只看到一次校验失败，
                        // 看不出后面是靠另一个源救回来的。
                        if i + 1 < sources.len() {
                            let first = format!("{one:#}").lines().next().unwrap_or("").to_string();
                            cb(Event::Note(format!(
                                "源 {}/{} 校验不符（{}），换下一个源",
                                i + 1,
                                sources.len(),
                                first
                            )));
                        }
                        last_err = one;
                    }
                }
            }
            task.url = primary.clone();
            return Err(last_err);
        }
        return Err(e);
    }

    let mut last: Option<anyhow::Error> = None;
    for (i, src) in sources.iter().enumerate() {
        if cancel.cancelled() {
            task.url = primary;
            bail!(Stopped);
        }
        task.url = src.clone();
        match run_one(
            store, engine, task, cb, live, live_total, links_live, links_seen, cancel, waiting, &[],
        ) {
            Ok(v) => {
                // 无论最后是谁成功，台账里的 url 都保留主源：重试时仍先试主源，
                // 也不会把镜像（可能是过期签名 URL）写成任务地址。
                task.url = primary;
                return Ok(v);
            }
            Err(e) => {
                if cancel.cancelled() || e.downcast_ref::<Stopped>().is_some() {
                    task.url = primary;
                    return Err(e);
                }
                let kind = classify_failure(&format!("{e:#}"));
                let retryable = matches!(kind, Failure::Network | Failure::Throttle);
                if i + 1 < sources.len() {
                    let first = format!("{e:#}").lines().next().unwrap_or("").to_string();
                    cb(Event::Note(format!(
                        "源 {}/{} 失败（{}），换下一个源",
                        i + 1,
                        sources.len(),
                        first
                    )));
                    // 内容/策略/HTTP 这类错误：共享位图里可能已经有这个源的坏块，
                    // 清掉再让下一个源从零开始。网络类错误保留已下的分片。
                    if !retryable {
                        let _ = discard_task_parts(task);
                    }
                    last = Some(e);
                    continue;
                }
                task.url = primary;
                return Err(e);
            }
        }
    }
    task.url = primary;
    Err(last.unwrap_or_else(|| anyhow!("没有可用的源")))
}

#[cfg_attr(not(feature = "bt"), allow(unused_variables))]
#[allow(clippy::too_many_arguments)]
fn run_one(
    store: &Store,
    engine: &Engine,
    task: &mut Task,
    cb: &(dyn Fn(Event) + Sync),
    live: &Arc<AtomicU64>,
    live_total: &Arc<AtomicU64>,
    links_live: &Arc<AtomicU32>,
    links_seen: &Arc<AtomicU32>,
    cancel: &Cancel,
    waiting: &AtomicBool,
    mirrors: &[String],
) -> Result<bool> {
    // `~` / 相对路径先统一成绝对路径：check_dest 和真正写盘必须看同一条路径。
    // API/CLI 也会展开，但 SDK 调用方可能直接给相对路径；这里兜一层。
    // `settings::expand` 会去掉尾斜杠，而尾斜杠是“这是目录”的信号 —— 补回来，
    // 否则 `dest=/tmp/out/` 会变成 `/tmp/out`，被当成文件写到目录本身。
    let phase_start = std::time::Instant::now();
    let expanded = normalize_task_dest(&task.dest);
    if !expanded.is_empty() {
        task.dest = expanded;
    }

    // 凭证只用于 http/https 直链；切片流和种子不走认证头，明确拒绝而不是静默忽略。
    if !task.credential_ref.trim().is_empty() {
        let lower = task.url.to_ascii_lowercase();
        let is_bt = lower.starts_with("magnet:") || lower.ends_with(".torrent");
        if crate::hls::is_stream(&task.url) || is_bt {
            bail!("--credential-ref 目前只支持 http/https 直链；HLS/DASH 与种子不走认证头。");
        }
    }
    // 种子与切片流是另外两种下载方式，落地与校验跟直链共用同一份代码
    if crate::hls::is_stream(&task.url) {
        return run_stream(store, engine, task, cb, cancel);
    }
    #[cfg(feature = "bt")]
    if crate::bt::is_bt(&task.url) {
        // `.torrent` 走 http(s) 时由 librqbit 去抓：这一跳也必须过出网策略，
        // 否则 `http://169.254.169.254/...?...x=.torrent` 就绕过了 check_url。
        if task.url.to_ascii_lowercase().starts_with("http") {
            crate::security::check_url(&task.url, &engine.policy)?;
        }
        return run_torrent(engine, task, cb, live, live_total, links_live, links_seen, cancel);
    }
    // 没编 BT 时遇到种子地址：说清楚是「这个构建不支持」，别让人以为链接坏了
    #[cfg(not(feature = "bt"))]
    {
        let l = task.url.to_ascii_lowercase();
        if l.starts_with("magnet:") || l.ends_with(".torrent") {
            bail!("这个构建没编入种子支持（--features bt）。magnet 与 .torrent 需要它。");
        }
    }
    // 落地边界必须先于缓存捷径：命中缓存不联网，但**仍然会往 dest 写文件**。
    // 以前 check_dest 在缓存捷径之后，agent 可以拿一个已经在缓存里的 sha256
    // 把内容写到允许目录之外（比如 ~/.ssh/authorized_keys）。
    crate::security::check_dest(&task.dest, &engine.policy)?;

    // 同一份内容已经有人在下载：先等它落进缓存，再走下面的秒传捷径。
    // 只允许一个等待者（全局一个槽），避免所有 worker 都在等同一个大文件。
    if crate::security::is_sha256_hex(&task.expected_sha256)
        && blob_of(&task.expected_sha256).is_none()
    {
        wait_for_inflight(store, &task.expected_sha256, &task.id, cancel, cb, waiting)?;
    }

    // 内容就在本机缓存里（sha 对得上）：一个网络请求都不发 —— 连探测也不发。
    // 以前没有 sha 的时候只能「同一个地址下过」才命中；有 sha 却还要问一遍服务器，
    // 于是「秒传」也要几百毫秒、断网就失败。
    if !task.expected_sha256.is_empty() {
        if let Some(blob) = blob_of(&task.expected_sha256) {
            if engine.verify_cache && !blob_matches(&blob, &task.expected_sha256) {
                // 缓存块内容和文件名里的 sha 不符（被改过/投毒）：
                // 不信任这次秒传，继续走正常下载；`place` 会用验证过的内容替换它。
                cb(Event::Note(
                    "缓存块内容和文件名里的 sha 不符（可能被改过），这次不秒传，重新下载".into(),
                ));
            } else {
            let dest = early_dest(task);
            let size = fs::metadata(&blob).map(|m| m.len()).unwrap_or(0);
            cb(Event::Planned {
                total: size,
                ranges: true,
                threads: 1,
            });
            cb(Event::Verifying);
            land_existing(&blob, &dest)?;
            // path 必须报**最终文件**，不是目录：缓存捷径以前只落了文件、
            // 没把 dest 改写成 dest/文件名，接口和 agent 看到的是目录。
            task.dest = dest.display().to_string();
            task.sha256 = task.expected_sha256.clone();
            task.bytes_done = size;
            task.bytes_total = size;
            // 信任级别由调用方说明（pinned / indexed），这里只在它没说的时候兜个底 ——
            // 覆盖成 pinned 等于把「来自发布方」和「我自己钉的」说成同一件事
            if task.trust.is_empty() {
                task.trust = "pinned".to_string();
            }
            task.updated_at = now_ms();
            cb(Event::Progress {
                done: size,
                total: size,
            });
            // Finished 由 run() 按 Ok(true) 统一发，这里不重复
            return Ok(true);
            }
        }
    }

    check_scheme(&task.url)?;
    // 出网边界先过：被拒绝的地址不该先建锁文件、更不该先碰盘。
    let lower = task.url.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        crate::security::check_url(&task.url, &engine.policy)?;
    }
    // 凭证只发给主源；镜像可能是第三方，不能把 token 带过去。
    let credential =
        crate::credential::resolve(&task.credential_ref, "SKYGET_DOWNLOAD_CREDENTIALS")?;
    if credential.is_some() && engine.policy.allowed_hosts.is_none() {
        bail!(
            "使用下载凭证前必须配置 SKYGET_ALLOWED_HOSTS（或 per-agent allowed_hosts），\
             否则 token 可能被发到任意公网主机。"
        );
    }
    let auth = if task.sources.iter().any(|s| s == &task.url) {
        None
    } else {
        credential.as_deref()
    };

    // 分片按 URL 指纹存，不按任务 id：任务 id 每次都新生成，靠它续传等于每次从头下。
    // 也正因为按 URL 存，跨进程会撞同一份 `.part` / 位图 —— 先拿锁，再做探测。
    // 拿不到锁说明另一个进程正在下同一个地址：直接给它一个可重试的结论，
    // 绝不让两个写者一起动同一份位图。
    let part = paths::parts_dir().join(format!("{}.part", part_key_for(task)));
    fs::create_dir_all(paths::parts_dir()).ok();
    let _part_lock = PartLock::acquire(&part, &task.url)?;

    // 重定向每一跳由客户端的 redirect policy 再查（初始地址上面已经查过）。
    cb(Event::Probing);
    // 有镜像时把每个源都探一遍：只把「有响应」的源放进分片池，挂掉的不占分片；
    // 主源挂了也能靠镜像的探测结果继续 —— 否则镜像等于白配。
    let mut live_sources: Vec<String> = Vec::new();
    let mut chosen: Option<Probe> = None;
    let mut first_err: Option<anyhow::Error> = None;
    for src in std::iter::once(task.url.clone()).chain(mirrors.iter().cloned()) {
        if live_sources.contains(&src) {
            continue;
        }
        match probe(&engine.client, &src, auth, cb) {
            Ok(p) => {
                live_sources.push(src);
                if chosen.is_none() {
                    chosen = Some(p);
                }
            }
            Err(e) => {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
    }
    let probe = match chosen {
        Some(p) => p,
        None => match first_err {
            Some(e) => return Err(e),
            None => anyhow::bail!("没有可用的源"),
        },
    };
    if cancel.cancelled() {
        bail!(Stopped);
    }
    if let Some(note) = probe.warning.clone() {
        cb(Event::Note(note));
    }

    let dir = dest_dir(task);
    fs::create_dir_all(&dir).with_context(|| format!("建不了落地目录：{}", dir.display()))?;
    // 调用方指定了文件名就用它的（--name / 显式路径），否则听服务器的
    let explicit_name = {
        let p = Path::new(&task.dest);
        if dest_is_dir(task) {
            None
        } else {
            p.file_name().map(|s| s.to_string_lossy().to_string())
        }
    };
    let name = sanitize_name(
        &explicit_name
            .or_else(|| probe.filename.clone())
            .unwrap_or_else(|| name_from_url(&task.url)),
    );
    // 重试 / 继续：落回台账里记着的那个文件。
    // 以前每次重试都走 unique_path，于是同一份东西在盘上攒成 "(1)"、"(2)" 副本。
    let dest = pick_dest(task, &dir, &name);
    task.dest = dest.display().to_string();

    // 内容本机早就有了（按 sha 命中缓存）：直接落地，一个字节都不走网络。
    // 这条以前只在「同一个地址下过」时命中；带 --sha256 的调用每次都白下一遍 102MB。
    let want = task.expected_sha256.clone();
    if !want.is_empty() {
        if let Some(blob) = blob_of(&want) {
            if engine.verify_cache && !blob_matches(&blob, &want) {
                cb(Event::Note(
                    "缓存块内容和文件名里的 sha 不符（可能被改过），这次不秒传，重新下载".into(),
                ));
            } else {
            cb(Event::Verifying);
            let size = fs::metadata(&blob).map(|m| m.len()).unwrap_or(0);
            land_existing(&blob, &dest)?;
            task.dest = dest.display().to_string();
            task.sha256 = want.clone();
            task.bytes_done = size;
            task.bytes_total = size.max(task.bytes_total);
            // 信任级别由调用方说明（pinned / indexed），这里只在它没说的时候兜个底 ——
            // 覆盖成 pinned 等于把「来自发布方」和「我自己钉的」说成同一件事
            if task.trust.is_empty() {
                task.trust = "pinned".to_string();
            }
            task.updated_at = now_ms();
            cb(Event::Progress {
                done: size,
                total: size,
            });
            // Finished 由 run() 按 Ok(true) 统一发，这里不重复
            return Ok(true);
            }
        }
    }

    let total = probe.total.unwrap_or(0);
    // 配额在知道总量后、真正下载前检查；缓存命中不联网，不走这里。
    if total > 0 {
        crate::quota::check(store, &task.source, total)?;
    }
    task.bytes_total = total;
    live_total.store(total, Ordering::Relaxed);

    // `-j 0` = 按体积自己定：小文件开一堆连接只是浪费，大文件才值得并行。
    let threads = if engine.threads == 0 {
        auto_threads(total)
    } else {
        engine.threads
    };
    cb(Event::Phase { name: "probe", ms: phase_start.elapsed().as_millis() as u64 });
    let transfer_start = std::time::Instant::now();
    links_live.store(if probe.ranges { threads as u32 } else { 1 }, Ordering::Relaxed);
    // threads=1 表示「别分段」—— 遇到 Range 支持有问题的源，这是唯一的逃生口
    let cached = if probe.ranges && total >= MIN_CHUNK * 2 && threads > 1 {
        segmented(engine, task, &probe, total, threads, &part, live, cb, cancel, auth, &live_sources)?
    } else {
        single(engine, task, &probe, total, &part, live, cb, cancel, auth)?
    };
    // 半截的东西绝不能往下走去算 hash 和落地
    if cancel.cancelled() {
        bail!(Stopped);
    }

    cb(Event::Phase { name: "transfer", ms: transfer_start.elapsed().as_millis() as u64 });
    // 收尾：算 hash、校验、内容寻址落地
    let verify_start = std::time::Instant::now();
    cb(Event::Verifying);
    let sha = sha256_file(&part)?;
    // 落地之前先把「凭什么信这份内容」结算清楚
    settle_trust(store, task, &sha, Some(&part))?;
    cb(Event::Phase { name: "verify", ms: verify_start.elapsed().as_millis() as u64 });
    let land_start = std::time::Instant::now();
    let size = fs::metadata(&part).map(|m| m.len()).unwrap_or(total);
    let cached = place(&part, &dest, &sha, engine.verify_cache)? || cached;
    cb(Event::Phase { name: "land", ms: land_start.elapsed().as_millis() as u64 });
    // 内容已经进了缓存，位图没用了；留着它下次就会被误信
    let _ = fs::remove_file(part.with_extension("json"));

    task.sha256 = sha;
    task.bytes_done = size;
    task.bytes_total = size;
    task.updated_at = now_ms();
    Ok(cached)
}

/// 凭什么信这份内容 —— 在落地之前结算，写进任务的 trust 字段。
///
/// 这一层不做密码学，只做「把来源说清楚」：
///   pinned  = 调用方给了 sha256（他从别处求证的，责任在他）
///   indexed = 从独立校验文件取到的 hash
///   tofu-ok = 与这个地址上次取到的内容一致
///   tofu-new= 第一次见，把这个 hash 记下来
///   torrent = 种子自证（piece hash），不需要外部 hash
fn settle_trust(store: &Store, task: &mut Task, sha: &str, part: Option<&Path>) -> Result<()> {
    if !task.expected_sha256.is_empty() {
        if !sha.eq_ignore_ascii_case(&task.expected_sha256) {
            if let Some(p) = part {
                let _ = fs::remove_file(p);
            }
            bail!(
                "校验不符：期望 {}…，实际 {}…。文件已丢弃。",
                short(&task.expected_sha256),
                short(sha)
            );
        }
        if task.trust.is_empty() {
            task.trust = "pinned".into();
        }
        return Ok(());
    }

    match store.last_done_for_url(&task.url)? {
        Some(prev) if !prev.sha256.is_empty() && !prev.sha256.eq_ignore_ascii_case(sha) => {
            if task.accept_change {
                task.trust = "tofu-new".into();
                return Ok(());
            }
            if let Some(p) = part {
                let _ = fs::remove_file(p);
            }
            bail!(
                "这个地址的内容变了：上次 {}…，这次 {}…。\
                 可能是正常更新，也可能是有人在换东西 —— 确认要更新就加 --accept-change。",
                short(&prev.sha256),
                short(sha)
            );
        }
        Some(prev) if prev.sha256.eq_ignore_ascii_case(sha) => {
            task.trust = "tofu-ok".into();
        }
        _ => {
            task.trust = "tofu-new".into();
        }
    }
    Ok(())
}

// ── 切片流（HLS / DASH）────────────────────────────────────────────────

fn run_stream(
    store: &Store,
    engine: &Engine,
    task: &mut Task,
    cb: &(dyn Fn(Event) + Sync),
    cancel: &Cancel,
) -> Result<bool> {
    cb(Event::Probing);
    // 切片流只检查顶层 m3u8；ffmpeg 自己去取的分片不在这一层能拦。
    crate::security::check_dest(&task.dest, &engine.policy)?;
    crate::security::check_url(&task.url, &engine.policy)?;
    let stream_opt_in = std::env::var("SKYGET_ALLOW_STREAM_EGRESS")
        .map(|v| v == "1")
        .unwrap_or(false);
    if !crate::security::stream_egress_allowed(&engine.policy, stream_opt_in) {
        return Err(anyhow::Error::new(crate::security::Denied {
            code: "policy.stream_egress",
            why: "HLS/DASH 由 ffmpeg 取子清单和分片，这些子请求不经过 daemon 的出网策略；\
                  当前策略限制了出网（严格模式或 host 白名单），已拒绝。确认播放列表可信后，\
                  可设置 SKYGET_ALLOW_STREAM_EGRESS=1 显式放行。"
                .into(),
        }));
    }
    let dir = dest_dir(task);
    fs::create_dir_all(&dir).with_context(|| format!("建不了落地目录：{}", dir.display()))?;

    let name = sanitize_name(&crate::hls::output_name(&task.url));
    let dest = unique_path(&dir.join(name));
    task.dest = dest.display().to_string();

    let part = paths::parts_dir().join(format!("{}.mp4", part_key(&task.url)));
    fs::create_dir_all(paths::parts_dir()).ok();
    // 切片流和直链共用 `<key>.lock`：同一个地址只能有一个写者。
    let _part_lock = PartLock::acquire(&part, &task.url)?;
    cb(Event::Planned {
        total: 0,
        ranges: true,
        threads: 1,
    });

    let progress = |done: u64, total: u64| {
        cb(Event::ProgressSecs { done, total });
    };
    // 并行预取默认关闭：能回退的路径也要显式开启，不悄悄替换掉一条已经能用的路。
    // 实测（46MB/30 段/每连接 2MiB/s）：直连 12.04s，8 路并发 3.07s。
    let fast = std::env::var("SKYGET_HLS_FAST").map(|v| v == "1").unwrap_or(false);
    let mut handled = false;
    if fast {
        let jobs = if engine.threads == 0 { 8 } else { engine.threads };
        let work = part.with_extension("hlsdir");
        handled = crate::hls::fetch_parallel(
            &task.url, &part, &work, &engine.client, &engine.policy, cancel, &progress, jobs,
        )?;
    }
    if !handled {
        crate::hls::fetch(&task.url, &part, &engine.client, &engine.policy, cancel, &progress)?;
    }

    cb(Event::Verifying);
    let sha = sha256_file(&part)?;
    settle_trust(store, task, &sha, Some(&part))?;
    let size = fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    let cached = place(&part, &dest, &sha, engine.verify_cache)?;
    task.sha256 = sha;
    task.bytes_done = size;
    task.bytes_total = size;
    task.updated_at = now_ms();
    Ok(cached)
}

// ── BitTorrent ──────────────────────────────────────────────────────────

#[cfg(feature = "bt")]
fn run_torrent(
    engine: &Engine,
    task: &mut Task,
    cb: &(dyn Fn(Event) + Sync),
    live: &Arc<AtomicU64>,
    live_total: &Arc<AtomicU64>,
    links_live: &Arc<AtomicU32>,
    links_seen: &Arc<AtomicU32>,
    cancel: &Cancel,
) -> Result<bool> {
    cb(Event::Probing);
    crate::security::check_dest(&task.dest, &engine.policy)?;
    let work = paths::parts_dir().join(format!("bt-{}", part_key(&task.url)));
    // 种子工作区也有跨进程写者问题：两个进程往同一个目录里落 piece 会互相踩。
    let _part_lock = PartLock::acquire(&work, &task.url)?;
    // magnet 的头几秒还没有总大小：先按「下载中」显示，别让人以为卡住了
    cb(Event::Planned {
        total: 0,
        ranges: true,
        threads: 1,
    });
    cb(Event::Note(
        "种子在找 peers；magnet 要等拿到文件清单才显示总大小".into(),
    ));

    task.trust = "torrent".into();
    let outcome = crate::bt::download(
        &task.url,
        &work,
        cancel,
        &|done, total| {
            live.store(done, Ordering::Relaxed);
            if total > 0 {
                live_total.store(total, Ordering::Relaxed);
            }
            cb(Event::Progress { done, total });
        },
        &|name| cb(Event::Named(name.to_string())),
        &|live, seen| {
            links_live.store(live, Ordering::Relaxed);
            links_seen.store(seen, Ordering::Relaxed);
            cb(Event::Links { live, seen });
        },
    )?;

    cb(Event::Verifying);
    let dir = dest_dir(task);

    if outcome.single_file {
        let sha = sha256_file(&outcome.raw_path)?;
        if !task.expected_sha256.is_empty() && !sha.eq_ignore_ascii_case(&task.expected_sha256) {
            let _ = fs::remove_file(&outcome.raw_path);
            bail!(
                "校验不符：期望 {}…，实际 {}…。文件已丢弃。",
                short(&task.expected_sha256),
                short(&sha)
            );
        }
        let dest = unique_path(&dir.join(sanitize_name(&outcome.name)));
        let cached = place(&outcome.raw_path, &dest, &sha, engine.verify_cache)?;
        task.dest = dest.display().to_string();
        task.sha256 = sha;
        task.bytes_done = outcome.total;
        task.bytes_total = outcome.total;
        let _ = fs::remove_dir_all(&work);
        Ok(cached)
    } else {
        // 多文件种子：整体落成目录。目录没有单一 sha256，就不假装有。
        let dest = crate::bt::place_dir(&outcome.raw_path, &dir, &outcome.name)?;
        task.dest = dest.display().to_string();
        task.sha256.clear();
        task.bytes_done = outcome.total;
        task.bytes_total = outcome.total;
        let _ = fs::remove_dir_all(&work);
        cb(Event::Note(
            "多文件种子：落地为目录，目录没有单一 sha256（每个文件的校验由种子本身负责）".into(),
        ));
        Ok(false)
    }
}

// ── 分段下载 ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
struct Shard {
    start: u64,
    end: u64, // 半开区间 [start, end)
}

/// 段大小按文件大小定，不按线程数除。
///
/// 段越大请求越少，但中断一次就白丢一整段 —— 5MB 的段配上 1MB/s 的源，
/// 断一次等于白干 5 秒。小段 + 动态抢活的代价只是多几个请求，换来的是
/// 「中断最多丢 1MB」，这才是断点续传该有的手感。
/// 默认连接数按体积定：每 8 MiB 一条，最少 4，最多 16。
///
/// 为什么不是常数：小文件开一堆连接只会放大探测和握手的固定开销，大文件才是
/// 并行赚钱的地方。下限 4 是原来的默认量级（原来是固定的 5）—— 不因为文件小
/// 就退化到单连接；上限 16 是实测值，见 `bench/fanout.sh` 的两段对照：
/// 单连接受限时吞吐随连接近似线性，到 16 仍在涨；再往上对多数源只增加被限流
/// 的概率。
fn auto_threads(total: u64) -> usize {
    ((total / (8 << 20)) as usize).clamp(4, 16)
}

fn plan(total: u64, threads: usize) -> Vec<Shard> {
    let base = if total <= 100 << 20 {
        1 << 20
    } else if total <= 2 << 30 {
        4 << 20
    } else {
        16 << 20
    };
    // 文件小的时候别切得太碎；同时保证每个连接手上至少有活
    let target = (threads as u64).saturating_mul(4).max(1);
    let chunk = base.min((total / target).max(MIN_CHUNK)).clamp(MIN_CHUNK, MAX_CHUNK);
    let mut out = Vec::new();
    let mut s = 0u64;
    while s < total {
        let e = (s + chunk).min(total);
        out.push(Shard { start: s, end: e });
        s = e;
    }
    out
}

#[derive(Debug, Serialize, Deserialize)]
struct PartMeta {
    version: u32,
    url: String,
    total: u64,
    etag: String,
    done: Vec<bool>,
    /// 多源 + pin sha 时，位图按内容而不是 URL 归属；镜像换了也接着下。
    #[serde(default)]
    content_key: String,
}

impl PartMeta {
    fn bytes_done(&self, shards: &[Shard]) -> u64 {
        shards
            .iter()
            .zip(self.done.iter())
            .filter(|(_, d)| **d)
            .map(|(s, _)| s.end - s.start)
            .sum()
    }
}

/// 读已有段位图；url / 大小 / etag 任何一项对不上就重来（服务器上换了文件，续传就是错的）。
fn load_or_init(
    path: &Path,
    url: &str,
    total: u64,
    etag: &str,
    shards: &[Shard],
    content_key: &str,
) -> Result<PartMeta> {
    if let Ok(txt) = fs::read_to_string(path) {
        if let Ok(m) = serde_json::from_str::<PartMeta>(&txt) {
            // 多源任务按内容归位：镜像的 URL/ETag 本来就不一样，不能因此重置。
            let same_source = if content_key.is_empty() {
                m.url == url && m.etag == etag
            } else {
                m.content_key == content_key
            };
            if m.version == 1
                && same_source
                && m.total == total
                && m.done.len() == shards.len()
            {
                return Ok(m);
            }
        }
    }
    Ok(PartMeta {
        version: 1,
        url: url.to_string(),
        total,
        etag: etag.to_string(),
        done: vec![false; shards.len()],
        content_key: content_key.to_string(),
    })
}

fn persist(path: &Path, meta: &PartMeta) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec(meta)?).context("写段位图失败")?;
    fs::rename(&tmp, path).context("落段位图失败")?;
    Ok(())
}

fn mark_done(state: &Mutex<PartMeta>, path: &Path, idx: usize) {
    let mut m = state.lock().unwrap();
    m.done[idx] = true;
    if let Err(e) = persist(path, &m) {
        eprintln!("段位图落盘失败（不影响这次下载，但下次可能重下这段）：{e}");
    }
}

#[allow(clippy::too_many_arguments)]
fn segmented(
    engine: &Engine,
    task: &mut Task,
    probe: &Probe,
    total: u64,
    threads: usize,
    part: &Path,
    done: &Arc<AtomicU64>,
    cb: &(dyn Fn(Event) + Sync),
    cancel: &Cancel,
    auth: Option<&str>,
    sources: &[String],
) -> Result<bool> {
    let started = std::time::Instant::now();
    let shards = plan(total, threads);
    let meta_path = part.with_extension("json");
    let content_key = if task.sources.len() > 1
        && crate::security::is_sha256_hex(&task.expected_sha256)
    {
        task.expected_sha256.clone()
    } else {
        String::new()
    };
    let state = Mutex::new(load_or_init(
        &meta_path,
        &task.url,
        total,
        &probe.etag,
        &shards,
        &content_key,
    )?);
    // 一开工就先把位图写下去：即使一段都没下完就被杀，下次也知道这是哪个文件、多大、
    // 什么 etag —— 不用重新探测就能接着来。
    let mut prior = state.lock().unwrap().bytes_done(&shards);
    // 位图说「下过了」，盘上就必须真有一份这个大小的 .part。
    // 不然呢：上次下完把 .part 搬进内容缓存了，位图却还留着，下一次
    // 一段都不下，预分配出来的全零文件就被当成成品 —— 静默产出一个坏文件。
    if prior > 0 && fs::metadata(part).map(|m| m.len()).unwrap_or(0) != total {
        let mut m = state.lock().unwrap();
        m.done = vec![false; shards.len()];
        prior = 0;
    }
    persist(&meta_path, &state.lock().unwrap()).ok();
    done.store(prior, Ordering::Relaxed);
    task.bytes_done = prior;

    if prior > 0 {
        cb(Event::Note(format!(
            "接着上次下：已经有 {} / {}",
            human(prior),
            human(total)
        )));
    }
    cb(Event::Planned {
        total,
        ranges: true,
        threads,
    });
    cb(Event::Links {
        live: threads as u32,
        seen: 0,
    });

    // 预分配：磁盘不够的话在这里就报错，而不是下到 90% 才发现
    let f = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(part)
        .with_context(|| format!("建不了临时文件：{}", part.display()))?;
    f.set_len(total).with_context(|| {
        format!(
            "预分配 {} 失败：磁盘空间不够？（{}）",
            human(total),
            part.display()
        )
    })?;
    drop(f);

    let pending: VecDeque<usize> = (0..shards.len()).filter(|i| !state.lock().unwrap().done[*i]).collect();
    let queue = Mutex::new(pending);
    // 段号跟错误一起存：错误是各线程按**完成顺序**塞进来的，直接拼会出现「第 2、3、5、4、1 段」
    let errs: Mutex<Vec<(usize, String)>> = Mutex::new(Vec::new());
    let left = AtomicUsize::new(threads);
    let throttle = AtomicU32::new(0);

    // 连接数会自己往下退：源的总带宽一旦封顶，多开的连接只是排队等同一个上限。
    // 实测（全局限速 4MiB/s、64MiB）：j=8 比 j=2 慢 11%。所以「加不动就少开点」。
    let target = AtomicUsize::new(threads);
    let seats = AtomicUsize::new(0);
    // 分片发给谁：单源就是自己；多源时按**实测速度**挑，而不是轮流分。
    // 轮流分的毛病：一个慢但活着的镜像会分走一半分片，把整体拖到最慢那个的水平
    // （实测：4MiB/s + 256KiB/s 两源各一半，总耗时等于慢源独自下完）。
    let urls: Vec<String> = if sources.is_empty() {
        vec![task.url.clone()]
    } else {
        sources.to_vec()
    };
    // 每个源累计「拿到多少字节 / 花了多久」，比值就是实测速度。
    let src_bytes: Vec<AtomicU64> = (0..urls.len()).map(|_| AtomicU64::new(0)).collect();
    let src_nanos: Vec<AtomicU64> = (0..urls.len()).map(|_| AtomicU64::new(0)).collect();
    let src_fails: Vec<AtomicUsize> = (0..urls.len()).map(|_| AtomicUsize::new(0)).collect();

    std::thread::scope(|s| {
        // 进度采样：200ms 一次，不把回调打成洪水；worker 全退就收摊
        s.spawn(|| {
            let mut last = 0u64;
            let mut prev = 0u64;
            let mut no_gain = 0u8;
            let mut warmup = true;
            let mut probing = false;
            let mut locked = false;
            let mut settle = 0u8;
            let mut before = 0u64;
            // 退档是**默认关闭**的，因为它对两种源的效果相反：
            //   - 单连接被封顶（多数 CDN/网盘）：8 条稳稳跑满，退一条就真掉 12.5%
            //     → 实测 +5% 完成时间
            //   - 总带宽被封顶（限速网盘）：多开的连接只是排队 → 实测 -10%
            // 多数源属于前者，所以默认别退；已知源封总带宽时用 SKYGET_ADAPT=1 打开。
            let adaptive = std::env::var("SKYGET_ADAPT").map(|v| v == "1").unwrap_or(false);
            loop {
                if left.load(Ordering::Relaxed) == 0 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
                let d = done.load(Ordering::Relaxed);
                let delta = d.saturating_sub(last);
                if d != last {
                    last = d;
                    cb(Event::Progress { done: d, total });
                }
                if warmup {
                    // 第一个窗口是建连和 TCP 爬坡，拿它判断会误退
                    warmup = false;
                    prev = delta;
                    continue;
                }
                if delta > prev {
                    no_gain = 0;
                } else {
                    no_gain += 1;
                }

                // 退档要能反悔：两种饱和在「吞吐不涨」这一点上长得一模一样。
                // 单连接封顶时 8 条连接稳稳 16MB/s 也不涨，退一条就真掉 12.5%；
                // 总带宽封顶时退一条吞吐不变。所以退完必须量一下，掉了就加回来
                // 并停止尝试 —— 宁可少省一条连接，也不要为了省它丢带宽。
                if probing {
                    settle += 1;
                    if settle >= 2 {
                        let dropped = delta.saturating_mul(100) < before.saturating_mul(92);
                        if dropped {
                            let now = target.fetch_add(1, Ordering::Relaxed) + 1;
                            cb(Event::Links {
                                live: now as u32,
                                seen: 0,
                            });
                            locked = true;
                        }
                        probing = false;
                    }
                    prev = delta;
                    continue;
                }

                let idle = queue.lock().map(|q| q.is_empty()).unwrap_or(true);
                if adaptive && !locked && no_gain >= 3 && !idle && target.load(Ordering::Relaxed) > 2 {
                    let now = target.fetch_sub(1, Ordering::Relaxed) - 1;
                    no_gain = 0;
                    probing = true;
                    settle = 0;
                    before = prev.max(delta);
                    cb(Event::Links {
                        live: now as u32,
                        seen: 0,
                    });
                }
                prev = delta;
            }
        });

        for _ in 0..threads {
            s.spawn(|| {
                // 座位号在同一批线程里唯一：退档时按座位号决定谁先收工
                let seat = seats.fetch_add(1, Ordering::Relaxed);
                loop {
                    if cancel.cancelled() {
                        break;
                    }
                    // 退档时让位但不退出线程：万一是误判，加回来要能立刻回到原来的并发。
                    // （一开始写的是直接 break，结果「回滚」只是抬高了上限，
                    //  退掉的线程已经没了 —— 一次误判就把 12.5% 永久吃掉。）
                    if seat >= target.load(Ordering::Relaxed) {
                        if queue.lock().map(|q| q.is_empty()).unwrap_or(true) {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(50));
                        continue;
                    }
                    let idx = match queue.lock().unwrap().pop_front() {
                        Some(i) => i,
                        None => break,
                    };
                    // 选源：还没测过的先各打一片（探索），之后全给测出来最快的。
                    // 连续失败两次的源本轮不再用（限流/坏掉），单源时永远只有 0 号。
                    let mut landed = false;
                    let mut last_err = String::new();
                    let shard_len = shards[idx].end - shards[idx].start;
                    for _ in 0..urls.len() {
                        let mut pick = usize::MAX;
                        let mut best = 0f64;
                        for i in 0..urls.len() {
                            if src_fails[i].load(Ordering::Relaxed) >= 2 {
                                continue;
                            }
                            let ns = src_nanos[i].load(Ordering::Relaxed);
                            if ns == 0 {
                                // 还没样本：优先探索，否则慢源永远没机会证明自己
                                pick = i;
                                break;
                            }
                            let speed = src_bytes[i].load(Ordering::Relaxed) as f64 / ns as f64;
                            if speed > best {
                                best = speed;
                                pick = i;
                            }
                        }
                        if pick == usize::MAX {
                            break; // 全死了
                        }
                        let t0 = Instant::now();
                        let res = fetch_shard(
                            &engine.client,
                            &urls[pick],
                            shards[idx],
                            part,
                            done.as_ref(),
                            &throttle,
                            cancel,
                            auth,
                        );
                        match res {
                            Ok(()) => {
                                let dt = t0.elapsed().as_nanos() as u64;
                                src_bytes[pick].fetch_add(shard_len, Ordering::Relaxed);
                                src_nanos[pick].fetch_add(dt.max(1), Ordering::Relaxed);
                                landed = true;
                                break;
                            }
                            Err(e) => {
                                src_fails[pick].fetch_add(1, Ordering::Relaxed);
                                last_err = format!("{e}");
                            }
                        }
                    }
                    if landed {
                        mark_done(&state, &meta_path, idx);
                    } else {
                        errs.lock().unwrap().push((idx, last_err));
                    }
                }
                left.fetch_sub(1, Ordering::Relaxed);
            });
        }
    });

    // 主动停：剩下的段不算失败，位图已经按段记账了，下次接着来
    if cancel.cancelled() {
        bail!(Stopped);
    }

    let failed = errs.lock().unwrap();
    if !failed.is_empty() {
        // 所有段都倒在同一件事上（多半是这台机器连不上源）：说一句就够。
        // 二十行「请求失败：超时」读完还是不知道该怎么办，而且把真正的结论埋了。
        let all_msg = failed
            .iter()
            .map(|(_, e)| e.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        if classify_failure(&all_msg) == Failure::Network
            && failed
                .iter()
                .all(|(_, e)| classify_failure(e) == Failure::Network)
        {
            bail!(
                "连不上源：{} 次连接全失败了（{:.0} 秒）。\n\
                 已下的部分留着，重跑会接着下 —— 这是网络问题，不是源坏了。",
                failed.len(),
                started.elapsed().as_secs_f64()
            );
        }
        bail!("{}", segment_failure_message(&failed));
    }
    Ok(false)
}

/// 机器可读的失败结论。
///
/// `error` 是给人读的一句话；这个结构是给调用方分支用的。
/// agent 不该去解析「连不上源」这种中文句子 —— 那等于把产品文案当 API。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorInfo {
    pub code: &'static str,
    pub retryable: bool,
    pub retry_after_ms: u64,
    pub next_action: &'static str,
}

/// 从错误对象分类；能 downcast 的具体类型优先，字符串匹配只做兜底。
pub fn classify_error(e: &anyhow::Error) -> ErrorInfo {
    if e.downcast_ref::<crate::lock::PartBusy>().is_some() {
        return ErrorInfo {
            code: "part.busy",
            retryable: true,
            retry_after_ms: 5_000,
            next_action: "等另一个进程结束再试；它下的分片会被直接复用。",
        };
    }
    // 限流：服务器给了 Retry-After 就照它说的等 —— 这是"礼貌条款"里
    // 唯一需要读响应头的地方，也是不把自家 IP 撞进黑名单的关键。
    if let Some(t) = e.downcast_ref::<Throttled>() {
        return ErrorInfo {
            code: "http.throttled",
            retryable: true,
            retry_after_ms: if t.after_ms > 0 { t.after_ms } else { 5_000 },
            next_action: "按服务器给的 Retry-After 等；已下的分片会保留。",
        };
    }
    if let Some(d) = e.downcast_ref::<crate::security::Denied>() {
        let next = match d.code {
            "policy.egress_denied" => "这个地址指向内网/元数据，默认拒绝；确需访问请管理员放行。",
            "policy.metadata_denied" => {
                "云元数据地址默认拒绝（可能泄露凭据）；确需访问要显式设 SKYGET_ALLOW_METADATA=1。"
            }
            "policy.dest_denied" => "换到允许的落地目录，或让管理员扩大 SKYGET_DEST_ROOTS。",
            "policy.host_denied" => "换一个源，或让管理员把域名加进 SKYGET_ALLOWED_HOSTS。",
            "policy.credentials_in_url" => "用授权头/私有源配置，不要把账号密码写进 URL。",
            "policy.upload_source_denied" => {
                "上传默认关闭；让管理员设置 SKYGET_UPLOAD_ROOTS，并把源文件放进去。"
            }
            "policy.credential_denied" => {
                "凭证引用被拒绝；env 变量要在 SKYGET_UPLOAD_CREDENTIALS 白名单里，file 只能放 SKYGET_HOME/secrets/。"
            }
            "input.unsupported" => "换成 http/https 直链、种子或切片流地址。",
            _ => "这次请求被策略拒绝；看 why 字段，或找管理员放行。",
        };
        return ErrorInfo {
            code: d.code,
            retryable: false,
            retry_after_ms: 0,
            next_action: next,
        };
    }
    classify_message(&format!("{e:#}"))
}

/// 把失败分成人能转述、机器能分支的几类。
///
/// 顺序有讲究：先判「内容/源的问题」，再判 HTTP，最后才判网络。
/// 反过来会把「服务器回了 404」说成「网络不通」——重试一百次也一样。
pub fn classify_message(msg: &str) -> ErrorInfo {
    let m = msg.to_ascii_lowercase();
    let hit = |keys: &[&str]| keys.iter().any(|k| m.contains(k));

    if hit(&["另一个进程正在下", "partbusy"]) {
        return ErrorInfo { code: "part.busy", retryable: true, retry_after_ms: 5_000, next_action: "等另一个进程结束再试；它下的分片会被直接复用。" };
    }
    // 重定向策略把 Denied 转成字符串后，downcast 已经拿不到类型了；
    // 这里按措辞兜底，避免把「策略拒绝」误报成可无限重试的网络错误。
    if m.contains("策略拒绝") || m.contains("非公网地址") || m.contains("不在白名单") {
        // 元数据单独一类：它的 next_action 和「内网源」完全不同（要开的是另一个开关）。
        if m.contains("元数据") {
            return ErrorInfo {
                code: "policy.metadata_denied",
                retryable: false,
                retry_after_ms: 0,
                next_action: "云元数据地址默认拒绝（可能泄露凭据）；确需访问要显式设 SKYGET_ALLOW_METADATA=1。",
            };
        }
        let dest_denied = m.contains("目标路径")
            || m.contains("目标本身是符号链接")
            || m.contains("悬空符号链接");
        return if dest_denied {
            ErrorInfo { code: "policy.dest_denied", retryable: false, retry_after_ms: 0, next_action: "换到允许的落地目录，或让管理员扩大 SKYGET_DEST_ROOTS。" }
        } else {
            ErrorInfo { code: "policy.egress_denied", retryable: false, retry_after_ms: 0, next_action: "这个地址指向内网/元数据，默认拒绝；确需访问请管理员放行。" }
        };
    }
    if m.contains("校验不符") {
        return ErrorInfo { code: "verify.mismatch", retryable: false, retry_after_ms: 0, next_action: "文件已丢弃。核对 --sha256 是否来自可信来源。" };
    }
    if m.contains("这个地址的内容变了") {
        return ErrorInfo { code: "source.changed", retryable: false, retry_after_ms: 0, next_action: "确认是正常更新后，加 --accept-change 再试。" };
    }
    if m.contains("不是我要的那一段") || m.contains("谎报范围") {
        return ErrorInfo { code: "source.range_lied", retryable: false, retry_after_ms: 0, next_action: "这个源的分段支持是假的；用 --jobs 1 走单连接。" };
    }
    if hit(&["不接受分段", "不支持分段"]) {
        return ErrorInfo { code: "source.no_range", retryable: false, retry_after_ms: 0, next_action: "用 --jobs 1 再试。" };
    }
    if hit(&["429", "503", "限流", "太频繁"]) {
        return ErrorInfo { code: "http.throttled", retryable: true, retry_after_ms: 5_000, next_action: "等一会儿再试；已下的分片会保留。" };
    }
    if hit(&["401", "403", "需要登录", "授权", "防盗链"]) {
        return ErrorInfo { code: "http.auth", retryable: false, retry_after_ms: 0, next_action: "需要登录或授权；先在浏览器确认这个地址能直接下。" };
    }
    if hit(&["404", "410", "地址不存在", "链接过期"]) {
        return ErrorInfo { code: "http.not_found", retryable: false, retry_after_ms: 0, next_action: "地址过期或写错了；换一个源。" };
    }
    if hit(&["服务器出错", "服务器暂时不可用", "http 5"]) {
        return ErrorInfo { code: "http.server", retryable: true, retry_after_ms: 3_000, next_action: "服务器出错，不是你的问题；稍后重试。" };
    }
    if hit(&["超时", "timed out", "timeout"]) {
        return ErrorInfo { code: "network.timeout", retryable: true, retry_after_ms: 2_000, next_action: "网络超时；等一会儿重试，已下的分片会保留。" };
    }
    if hit(&["连接被重置", "broken pipe", "unexpected eof", "connection reset"]) {
        return ErrorInfo { code: "network.reset", retryable: true, retry_after_ms: 2_000, next_action: "连接被中途断开；重试会接着下。" };
    }
    if hit(&["连不上", "connection", "connect", "dns", "lookup address", "域名解析失败", "网络"]) {
        return ErrorInfo { code: "network.connect", retryable: true, retry_after_ms: 2_000, next_action: "网络不通；等一会儿重试。已下的分片会保留。" };
    }
    if hit(&["no space left", "磁盘空间"]) {
        return ErrorInfo { code: "disk.full", retryable: false, retry_after_ms: 0, next_action: "先腾出磁盘空间再重试。" };
    }
    if hit(&["permission denied", "权限不足", "拒绝访问", "eacces"]) {
        return ErrorInfo { code: "disk.permission", retryable: false, retry_after_ms: 0, next_action: "目标目录没有写权限；换一个目录或修权限。" };
    }
    if hit(&["只接 http/https", "thunder://", "ed2k://", "网盘", "不支持的地址"]) {
        return ErrorInfo { code: "input.unsupported", retryable: false, retry_after_ms: 0, next_action: "换成 http/https 直链、种子或切片流地址。" };
    }
    ErrorInfo { code: "internal", retryable: false, retry_after_ms: 0, next_action: "把这条错误和日志一起报给维护者。" }
}

/// 这次失败是「网不通」还是「源/内容有问题」。
///
/// 分这两类不是为了好看：网不通值得等一会儿自己重试，源拒绝分段、内容变了、
/// 校验不符这些重试一百次也一样 —— 硬重试只会让人以为程序卡住了。
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Failure {
    /// 连接都没建立 / 传输中途断了：等一会儿再试有意义
    Network,
    /// 服务器说「你打太快」（429 / 503）
    Throttle,
    /// 其他：源不支持分段、内容变了、校验不符……重试没用
    Other,
}

pub fn classify_failure(msg: &str) -> Failure {
    let info = classify_message(msg);
    if info.code == "http.throttled" {
        Failure::Throttle
    } else if info.code.starts_with("network.") {
        Failure::Network
    } else {
        Failure::Other
    }
}

/// 分段失败的说明文字：第一行说结论，之后**一段一行、按段号排序**。
///
/// 错误是各线程按完成顺序塞进 Vec 的，直接拼会出现「第 2、3、5、4、1 段」——
/// 界面要按段号念给用户听，顺序错了就是在制造噪音。
fn segment_failure_message(failed: &[(usize, String)]) -> String {
    let mut items: Vec<&(usize, String)> = failed.iter().collect();
    items.sort_by_key(|(i, _)| *i);
    let mut msg = format!("{} 段没下来（已下的保留，重跑会接着下）", items.len());
    for (i, e) in items {
        msg.push_str(&format!("\n第 {} 段：{e}", i + 1));
    }
    msg
}

// ── 按主机限流 ──────────────────────────────────────────────────────────

/// 同一个源同时开多少条连接 —— 整个进程共享。
///
/// 每个任务已经按体积自己决定连接数，但源看到的是**所有任务的总和**：
/// 四个 agent 各开 16 条，同一个 CDN 上就是 64 条，既会触发限流，也是把
/// 对方打挂最快的办法。上限按主机名算，默认 16（等于单任务上限）；
/// `SKYGET_MAX_PER_HOST=0` 关掉。
struct HostGate {
    limit: usize,
    active: Mutex<HashMap<String, usize>>,
    cv: Condvar,
}

struct HostPermit<'a> {
    gate: &'a HostGate,
    host: Option<String>,
}

impl HostGate {
    fn acquire(&self, host: &str) -> HostPermit<'_> {
        if self.limit == 0 {
            return HostPermit {
                gate: self,
                host: None,
            };
        }
        let mut m = self.active.lock().unwrap();
        while *m.get(host).unwrap_or(&0) >= self.limit {
            m = self.cv.wait(m).unwrap();
        }
        *m.entry(host.to_string()).or_insert(0) += 1;
        HostPermit {
            gate: self,
            host: Some(host.to_string()),
        }
    }
}

impl Drop for HostPermit<'_> {
    fn drop(&mut self) {
        let Some(host) = self.host.as_deref() else {
            return;
        };
        let mut m = self.gate.active.lock().unwrap();
        if let Some(n) = m.get_mut(host) {
            *n -= 1;
            if *n == 0 {
                m.remove(host);
            }
        }
        drop(m);
        // 唤醒一个等位子的线程；它自己会重新检查条件
        self.gate.cv.notify_one();
    }
}

fn host_gate() -> &'static HostGate {
    static GATE: OnceLock<HostGate> = OnceLock::new();
    GATE.get_or_init(|| {
        let limit = std::env::var("SKYGET_MAX_PER_HOST")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(16);
        HostGate {
            limit,
            active: Mutex::new(HashMap::new()),
            cv: Condvar::new(),
        }
    })
}

/// 请求期间占着这个源的一个位子；离开作用域自动还回去。
///
/// 地址解析不出来就不占位（交给后面的错误处理），不要为了限流制造新的失败。
fn host_permit(url: &str) -> Option<HostPermit<'static>> {
    let host = reqwest::Url::parse(url).ok()?.host_str()?.to_string();
    Some(host_gate().acquire(&host))
}

fn fetch_shard(
    client: &Client,
    url: &str,
    shard: Shard,
    part: &Path,
    done: &AtomicU64,
    throttle: &AtomicU32,
    cancel: &Cancel,
    auth: Option<&str>,
) -> Result<()> {
    let mut last_err = None;
    for attempt in 1..=MAX_TRIES {
        if cancel.cancelled() {
            return Err(anyhow::Error::new(Stopped));
        }
        // 节流阀：被 429 过就自己慢下来。五个连接一起硬撞，服务器只会把我们一起掐死。
        let level = throttle.load(Ordering::Relaxed).min(10);
        if level > 0 {
            std::thread::sleep(Duration::from_millis(
                200 * level as u64 + 120 * attempt as u64,
            ));
        }
        match shard_once(client, url, shard, part, done, cancel, auth) {
            Ok(()) => {
                // 通了就慢慢把阀门松回去
                let _ = throttle.try_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                    if v > 0 {
                        Some(v - 1)
                    } else {
                        None
                    }
                });
                return Ok(());
            }
            Err((e, written)) => {
                // 这次尝试的白下了：把计入的字节撤销，进度才不虚高。
                // 但「用户按了暂停」不算白下 —— 已经写进盘的就该算数，
                // 否则暂停瞬间进度会从 1.3MB 跌回 0（实测过，看着像全丢了）。
                if e.downcast_ref::<Stopped>().is_none() {
                    done.fetch_sub(written, Ordering::Relaxed);
                }
                let msg = format!("{e}");
                let server_wait = e
                    .downcast_ref::<Throttled>()
                    .map(|t| t.after_ms)
                    .filter(|ms| *ms > 0);
                if e.downcast_ref::<Throttled>().is_some() || msg.contains("429") || msg.contains("503")
                {
                    throttle.fetch_add(1, Ordering::Relaxed);
                }
                last_err = Some(e);
                if attempt < MAX_TRIES {
                    // 服务器说了等多久就等多久；没说才用我们的默认节奏。
                    let base = match server_wait {
                        Some(ms) => ms,
                        None if msg.contains("429") => 1500,
                        None => 300,
                    };
                    std::thread::sleep(Duration::from_millis(base * attempt as u64));
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("未知错误")))
}

fn shard_once(
    client: &Client,
    url: &str,
    shard: Shard,
    part: &Path,
    done: &AtomicU64,
    cancel: &Cancel,
    auth: Option<&str>,
) -> std::result::Result<(), (anyhow::Error, u64)> {
    let mut written = 0u64;
    // 占住这个源的一个连接位子，直到这一段的响应体读完为止
    let _permit = host_permit(url);
    let range = format!("bytes={}-{}", shard.start, shard.end - 1);
    let resp = match with_auth(client.get(url), auth).header(RANGE, &range).send() {
        Ok(r) => r,
        Err(e) => return Err((anyhow!("请求失败：{}", human_err(&e)), written)),
    };
    if resp.status().as_u16() != 206 {
        let code = resp.status().as_u16();
        // 429/503 是「你在打太快」，不是「我不支持分段」——别把这两种混成一句话。
        // 服务器给了 Retry-After 就按它说的等（礼貌条款）。
        if code == 429 || code == 503 {
            let t = Throttled {
                after_ms: retry_after_ms(resp.headers()),
                code,
            };
            return Err((anyhow::Error::new(t), written));
        }
        return Err((
            anyhow!("服务器不接受分段请求（HTTP {code}）"),
            written,
        ));
    }

    // 206 必须说清给的是哪一段，而且必须是我要的那一段。
    // 少了这一步，一个「声称支持 Range、其实永远从 0 开始发」的中间层
    // 会把数据写到错误的偏移上 —— 文件大小对、状态 206、我们报成功，内容是坏的。
    let cr = resp
        .headers()
        .get(CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let want = format!("bytes {}-{}", shard.start, shard.end - 1);
    if cr.is_empty() {
        return Err((
            anyhow!("服务器回了 206 但没给 Content-Range，无法确认收到的是哪一段"),
            written,
        ));
    }
    if !cr.starts_with(&want) {
        return Err((
            anyhow!(
                "服务器给的字节不是我要的那一段：请求 {want}，它说是 {cr}。这个源的分段支持是假的，重试没用 —— 用 --threads 1 走单连接。"
            ),
            written,
        ));
    }
    let f = OpenOptions::new()
        .write(true)
        .open(part)
        .map_err(|e| (anyhow!("打开临时文件失败：{e}"), written))?;
    let mut resp = resp;
    let mut buf = vec![0u8; BUF];
    let mut pos = shard.start;
    loop {
        // 每块数据之间看一眼：暂停最多等一个 BUF（64KB），不用把整段下完
        if cancel.cancelled() {
            return Err((anyhow::Error::new(Stopped), written));
        }
        let n = match resp.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => return Err((anyhow!("传输中断：{e}"), written)),
        };
        if pos + n as u64 > shard.end {
            return Err((anyhow!("服务器返回的数据超出请求范围"), written));
        }
        if let Err(e) = f.write_at(&buf[..n], pos) {
            return Err((anyhow!("写盘失败：{e}"), written));
        }
        pos += n as u64;
        written += n as u64;
        done.fetch_add(n as u64, Ordering::Relaxed);
    }
    // 数据先落盘，位图后记账：位图说完成了，盘上就必须真在
    let _ = f.sync_data();
    if pos != shard.end {
        return Err((
            anyhow!("这段没收全（{}/{} 字节）", pos - shard.start, shard.end - shard.start),
            written,
        ));
    }
    Ok(())
}

// ── 单连接（服务器不支持 Range，或文件太小） ──────────────────────────────

fn single(
    engine: &Engine,
    task: &mut Task,
    probe: &Probe,
    total: u64,
    part: &Path,
    done: &Arc<AtomicU64>,
    cb: &(dyn Fn(Event) + Sync),
    cancel: &Cancel,
    auth: Option<&str>,
) -> Result<bool> {
    cb(Event::Planned {
        total,
        ranges: probe.ranges,
        threads: 1,
    });

    // 支持 Range 的服务器上，单连接也能续：从已有长度接着要。
    // 但如果这里躺着分段下载留下的位图，那个 .part 是**预分配**的（长度等于总大小），
    // 拿它的长度当「已下载量」会把一个空壳当成完整文件 —— 直接从头来。
    let meta = part.with_extension("json");
    let from_segmented = meta.exists();
    let resume_from = if probe.ranges && !from_segmented {
        fs::metadata(part).map(|m| m.len()).unwrap_or(0)
    } else {
        0
    };
    if from_segmented {
        let _ = fs::remove_file(part);
    }
    if resume_from > 0 && total > 0 && resume_from >= total {
        done.store(resume_from, Ordering::Relaxed);
        task.bytes_done = resume_from;
        return Ok(false);
    }

    let mut req = with_auth(engine.client.get(&task.url), auth);
    if resume_from > 0 {
        req = req.header(RANGE, format!("bytes={resume_from}-"));
    }
    let resp = req.send().with_context(|| "请求失败".to_string())?;
    let status = resp.status().as_u16();
    if !resp.status().is_success() {
        bail!("{}", http_reason(status));
    }
    let mut file = if resume_from > 0 {
        OpenOptions::new().append(true).open(part)?
    } else {
        File::create(part)?
    };

    let mut resp = resp;
    let mut buf = vec![0u8; BUF];
    let mut got = resume_from;
    let mut last = Instant::now();
    loop {
        if cancel.cancelled() {
            bail!(Stopped);
        }
        let n = resp.read(&mut buf).with_context(|| "传输中断".to_string())?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).with_context(|| "写盘失败".to_string())?;
        got += n as u64;
        done.store(got, Ordering::Relaxed);
        if last.elapsed() >= Duration::from_millis(200) {
            last = Instant::now();
            cb(Event::Progress {
                done: got,
                total: total.max(got),
            });
        }
    }
    file.sync_data().ok();
    let size = fs::metadata(part).map(|m| m.len()).unwrap_or(got);
    if total > 0 && size != total {
        bail!("没收全：{}/{} 字节", size, total);
    }
    task.bytes_done = size;
    Ok(false)
}

// ── 探测 ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Probe {
    total: Option<u64>,
    ranges: bool,
    etag: String,
    filename: Option<String>,
    warning: Option<String>,
}

/// 服务器说「你打太快」，并给了建议等待时间（`Retry-After`）。
///
/// 单独成类型而不是拼进错误文本：分类（`retry_after_ms`）、节流阀、界面提示
/// 三处都要用同一个数字，从字符串里再抠一次迟早会抠错。
#[derive(Debug)]
pub struct Throttled {
    /// 服务器建议等多久（毫秒）。没给 `Retry-After` 时是 0，调用方用默认值。
    pub after_ms: u64,
    pub code: u16,
}

impl std::fmt::Display for Throttled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.after_ms > 0 {
            write!(
                f,
                "服务器限流（HTTP {}）：它让你等 {} 秒",
                self.code,
                self.after_ms.div_ceil(1000)
            )
        } else {
            write!(f, "服务器限流（HTTP {}）", self.code)
        }
    }
}
impl std::error::Error for Throttled {}

/// 读 `Retry-After`：只认「秒数」这种写法（HTTP-date 极少见，遇到就当没给）。
///
/// 上限夹到 5 分钟：一个源让你等一小时，正确做法是换源/失败，不是把任务挂死。
fn retry_after_ms(headers: &reqwest::header::HeaderMap) -> u64 {
    headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|secs| secs.saturating_mul(1000).min(300_000))
        .unwrap_or(0)
}

/// 探测：问一句「这是多大的文件、支不支持分段」。
///
/// 429 / 503 是「你打太快了」，不是「这个地址不行」——退一步再来，别把限流当成源坏了。
/// 三条自检里就有一条栽在这上面：服务器对第一句探测回 429，下载当场失败。
/// 给请求带上下载凭证；没有凭证就是原请求。
fn with_auth(
    rb: reqwest::blocking::RequestBuilder,
    auth: Option<&str>,
) -> reqwest::blocking::RequestBuilder {
    match auth {
        Some(token) => rb.bearer_auth(token),
        None => rb,
    }
}

fn probe(
    client: &Client,
    url: &str,
    auth: Option<&str>,
    cb: &(dyn Fn(Event) + Sync),
) -> Result<Probe> {
    let mut wait = Duration::from_millis(800);
    let mut last: Option<anyhow::Error> = None;
    for attempt in 1..=3 {
        match probe_once(client, url, auth, cb) {
            Ok(p) => return Ok(p),
            Err(e) => {
                let msg = format!("{e:#}");
                let throttled = msg.contains("429") || msg.contains("503");
                if !throttled || attempt == 3 {
                    return Err(e);
                }
                // 服务器给了 Retry-After 就用它的；没给才用指数的 0.8/1.6/3.2 秒。
                let ask = e.downcast_ref::<Throttled>().map(|t| t.after_ms).unwrap_or(0);
                let d = if ask > 0 {
                    Duration::from_millis(ask)
                } else {
                    wait
                };
                cb(Event::Note(format!(
                    "服务器说「太频繁」了，等 {:.1} 秒再问一次（第 {attempt} 次）",
                    d.as_secs_f32()
                )));
                std::thread::sleep(d);
                wait *= 2;
                last = Some(e);
            }
        }
    }
    Err(last.unwrap_or_else(|| anyhow!("探测失败")))
}

fn probe_once(
    client: &Client,
    url: &str,
    auth: Option<&str>,
    cb: &(dyn Fn(Event) + Sync),
) -> Result<Probe> {
    // 先 HEAD：省流量。信不信它，取决于它敢不敢说自己 Accept-Ranges: bytes
    let mut head = Probe {
        total: None,
        ranges: false,
        etag: String::new(),
        filename: None,
        warning: None,
    };
    if let Ok(r) = with_auth(client.head(url), auth).send() {
        if r.status().is_success() {
            head.total = header_u64(&r, CONTENT_LENGTH);
            head.ranges = r
                .headers()
                .get(ACCEPT_RANGES)
                .and_then(|v| v.to_str().ok())
                .map(|v| v.eq_ignore_ascii_case("bytes"))
                .unwrap_or(false);
            head.etag = r
                .headers()
                .get(ETAG)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            head.filename = filename_of(&r);
            if head.ranges && head.total.is_some() {
                return Ok(head);
            }
        } else if r.status().as_u16() >= 400 {
            // HEAD 被拒（不少站点如此），下面的 GET 探测会给准确答案
        }
    }

    // GET 探一个字节：206 说明能分段，200 说明只能整条下
    let r = with_auth(client.get(url), auth)
        .header(RANGE, "bytes=0-0")
        .send()
        .with_context(|| "连不上这个地址")?;
    let status = r.status().as_u16();
    if !r.status().is_success() {
        // 探测阶段的 429/503 也要带上服务器给的 Retry-After —— 第一句话就被限流
        // 是最常见的情况，退避节奏由它说了算。
        if status == 429 || status == 503 {
            return Err(anyhow::Error::new(Throttled {
                after_ms: retry_after_ms(r.headers()),
                code: status,
            }));
        }
        bail!("{}", http_reason(status));
    }
    let ctype = r
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let is_html = ctype.contains("text/html");

    let mut p = Probe {
        total: None,
        ranges: false,
        etag: head.etag,
        filename: head.filename.or_else(|| filename_of(&r)),
        warning: None,
    };
    if status == 206 {
        p.ranges = true;
        p.total = r
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit('/').next().map(|s| s.trim().to_string()))
            .and_then(|s| if s == "*" { None } else { s.parse::<u64>().ok() });
        if let Some(etag) = r.headers().get(ETAG).and_then(|v| v.to_str().ok()) {
            p.etag = etag.to_string();
        }
    } else {
        p.total = header_u64(&r, CONTENT_LENGTH).or(head.total);
        if is_html {
            p.warning = Some(
                "这个地址返回的是网页，不是文件。要的是直链（右键「复制链接地址」），不是页面。".into(),
            );
        }
    }
    if p.total == Some(0) {
        p.total = None;
    }
    cb(Event::Note(format!(
        "{} · {}",
        if p.ranges { "支持分段" } else { "不支持分段，单连接下" },
        p.total.map(human).unwrap_or_else(|| "大小未知".into())
    )));
    Ok(p)
}

// ── 落地：内容寻址 + 硬链接 ──────────────────────────────────────────────

/// 把下好的 `.part` 变成 `blobs/<xx>/<hash>`，再硬链到用户目录。
/// 返回 true 表示这份内容本机早就有了（没占第二份空间）。
/// 内容缓存里这个 sha 的那份文件，存在就返回路径。
/// 缓存捷径要落到哪儿：dest 是目录就用 URL 猜名字；dest 是个已有文件且内容不同，
/// 就只有「同一任务重跑」才允许覆盖，否则照样走 unique_path（不覆盖别人的文件）。
fn early_dest(task: &Task) -> PathBuf {
    let name = sanitize_name(&name_from_url(&task.url));
    let raw = task.dest.trim_end_matches('/');
    // 和 `dest_is_dir` 用同一套判据：尾斜杠**或**盘上已经是目录。
    // 以前缓存捷径只看尾斜杠，而正常路径看 is_dir —— 于是「dest 是个已存在目录、
    // 又没有尾斜杠」时，缓存命中的文件会落到目录本身（把目录变成文件）。
    let as_dir = raw.is_empty()
        || task.dest.ends_with('/')
        || Path::new(raw).is_dir();
    let dest = if as_dir {
        let dir = if raw.is_empty() {
            crate::settings::dest_dir()
        } else {
            PathBuf::from(raw)
        };
        dir.join(&name)
    } else {
        PathBuf::from(raw)
    };
    if task.rerun || fs::symlink_metadata(&dest).is_err() {
        return dest;
    }
    match blob_of(&task.expected_sha256) {
        // 盘上那个就是同一份内容：不用动
        Some(blob) if same_file(&blob, &dest) => dest,
        // 内容不同又是新任务：加 (1)，不覆盖
        _ => unique_path(&dest),
    }
}

/// 这一条该落到哪儿。
///
/// 默认走 `unique_path`（同名自动加 `(1)`，不覆盖别人的文件）。
/// 但「同一任务再来一次」（重试 / 继续）不该攒副本：那个文件本来就是它的，落回原处。
fn pick_dest(task: &Task, dir: &Path, name: &str) -> PathBuf {
    if task.rerun && !task.dest.is_empty() {
        let p = Path::new(&task.dest);
        if let Ok(m) = fs::symlink_metadata(p) {
            // 只复用普通文件；符号链接一律重新选名，绝不跟随。
            if m.is_file() && !m.file_type().is_symlink() {
                return p.to_path_buf();
            }
        }
    }
    unique_path(&dir.join(name))
}

fn blob_of(sha: &str) -> Option<PathBuf> {
    // 只接受 64 位十六进制。以前这里只判 len>=2 就拿去拼路径：
    // 传 `"/home/u/.ssh/id_ed25519"` 会被 `join` 当成绝对路径整体替换，
    // 传 `"中"` 会在 `&sha[..2]` 处直接 panic。缓存捷径比校验先跑，
    // 于是任意本地文件都能被“秒传”到 agent 指定的目录。
    if !crate::security::is_sha256_hex(sha) {
        return None;
    }
    let p = paths::blobs_dir().join(&sha[..2]).join(sha);
    // 只认普通文件：符号链接（哪怕指向真文件）不进缓存捷径。
    fs::symlink_metadata(&p)
        .map(|m| m.is_file())
        .unwrap_or(false)
        .then_some(p)
}

/// 缓存块内容是否真的等于它文件名里的 sha。
fn blob_matches(blob: &Path, sha: &str) -> bool {
    sha256_file(blob)
        .map(|h| h.eq_ignore_ascii_case(sha))
        .unwrap_or(false)
}

/// 两个路径是不是同一个文件（同设备、同 inode）。
///
/// 硬链接之后它们就是同一份数据；这一点决定了「能不能对它做 copy」。
/// 把「最近用过」记在 mtime 上（不清缓存块内容，只碰时间）。
fn touch(p: &Path) {
    if let Ok(f) = fs::OpenOptions::new().write(true).open(p) {
        let _ = f.set_times(fs::FileTimes::new().set_modified(std::time::SystemTime::now()));
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (fs::metadata(a), fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
        _ => false,
    }
}

/// 把缓存里的内容落到 dest。
///
/// **同一份内容就什么都不做**：落地文件本来就是缓存块的硬链接，对它 `fs::copy`
/// 会先截断目标、再读源 —— 源和目标同一个 inode，等于把文件清成 0 字节。
/// 实测踩过：一次「重试」把 102MB 的缓存块连同三个落地文件全清到 0。
fn land(blob: &Path, dest: &Path) -> Result<()> {
    // 走到这里就是「用了这份缓存」：把使用时间记下来，prune 才不会清掉常用块
    touch(blob);
    if same_file(blob, dest) {
        return Ok(());
    }
    // 缓存捷径会直接调 land（不经过 place），所以父目录必须在这里建。
    // 以前 e2e 都预建了目标目录，这个洞没露出来；目标目录不存在时
    // hard_link/copy 只会回一句 ENOENT。
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("建不了落地目录：{}", parent.display()))?;
    }
    // 最后一道防线：目标本身是符号链接就拒绝。硬链接失败后 fs::copy
    // 会跟随链接目标写 —— 悬空链接会因此在允许目录之外创建文件。
    if let Ok(m) = fs::symlink_metadata(dest) {
        if m.file_type().is_symlink() {
            bail!("目标是一个符号链接，拒绝跟随：{}", dest.display());
        }
    }
    // 硬链接：同一个 inode 两个名字，既省空间，删了缓存文件也还在
    if fs::hard_link(blob, dest).is_ok() {
        return Ok(());
    }
    fs::copy(blob, dest).with_context(|| format!("落地失败：{}", dest.display()))?;
    let (want, got) = (
        fs::metadata(blob).map(|m| m.len()).unwrap_or(0),
        fs::metadata(dest).map(|m| m.len()).unwrap_or(0),
    );
    if want != got {
        bail!(
            "落地后大小不对：{} 应 {} 字节，实际 {}",
            dest.display(),
            want,
            got
        );
    }
    Ok(())
}

/// 兼容旧名字（run_inner 里用的是 land_existing）
fn land_existing(blob: &Path, dest: &Path) -> Result<()> {
    land(blob, dest)
}

fn place(part: &Path, dest: &Path, sha: &str, verify_cache: bool) -> Result<bool> {
    let root = paths::blobs_dir();
    let blob = root.join(&sha[..2.min(sha.len())]).join(sha);
    fs::create_dir_all(blob.parent().unwrap()).ok();

    let existing = fs::symlink_metadata(&blob).ok();
    let is_file = existing.as_ref().map(|m| m.is_file()).unwrap_or(false);
    // 旧块只有在「是普通文件」且（不校验或真的等于 sha）时才可信。
    // 符号链接/被篡改的块必须由这次验证过的 part 替换，不能反过来用旧块落地。
    let trusted = is_file && (!verify_cache || blob_matches(&blob, sha));
    if trusted {
        // 已经有了：这份下载是重复的，删掉临时文件，直接链
        let _ = fs::remove_file(part);
        // 顺手把「最近用过」写在 mtime 上：prune 靠它判断该不该清。
        // 只靠 atime 不保险 —— 很多挂载是 relatime，读一次不一定更新。
        touch(&blob);
    } else {
        // 旧块不可信：先删掉（符号链接只删链接本身），再让验证过的 part 进来。
        if existing.is_some() {
            let _ = fs::remove_file(&blob);
        }
        // 同盘 rename 是瞬时的；跨盘才退化成拷贝
        if fs::rename(part, &blob).is_err() {
            fs::copy(part, &blob).context("存入内容缓存失败")?;
            let _ = fs::remove_file(part);
        }
    }

    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).ok();
    }
    land(&blob, dest)?;
    Ok(trusted)
}

pub(crate) fn sha256_file(p: &Path) -> Result<String> {
    let mut f = File::open(p).with_context(|| format!("打不开临时文件：{}", p.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(format!("{:x}", h.finalize()))
}

// ── 小工具 ──────────────────────────────────────────────────────────────

/// 分片文件的名字：同一个 URL 永远落到同一对 `.part` / `.json` 上。
/// 取消（不是暂停）时把半成品清干净：分片、位图、切片流的 mp4、种子工作区。
///
/// 暂停**不能**调它——暂停的意义就是留着这些，下次接着下。
pub fn discard_parts(url: &str) -> Result<()> {
    let key = part_key(url);
    let dir = paths::parts_dir();
    // 先确认没有别的进程正在写：取消一条任务，不该把另一个进程的分片删掉。
    // 锁文件和 `.part` / `.mp4` 共用同一个 `<key>.lock`。
    let _lock = PartLock::acquire(&dir.join(format!("{key}.part")), url)?;
    let _bt_lock = PartLock::acquire(&dir.join(format!("bt-{key}")), url)?;
    for name in [
        format!("{key}.part"),
        format!("{key}.json"),
        format!("{key}.mp4"),
    ] {
        match fs::remove_file(dir.join(&name)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("删不掉 {name}")),
        }
    }
    let bt = dir.join(format!("bt-{key}"));
    if bt.is_dir() {
        fs::remove_dir_all(&bt).with_context(|| format!("删不掉 {}", bt.display()))?;
    }
    Ok(())
}

pub fn part_key(url: &str) -> String {
    let mut h = Sha256::new();
    h.update(url.as_bytes());
    format!("{:x}", h.finalize())[..16].to_string()
}

/// 分片永远按**当前源 URL** 存，多源之间不共享 `.part`。
///
/// 曾经按 pin 的 sha 共享（`sha-<前16>`），想让镜像接着主源的分片下。
/// 但那等于信任“上一个源写下的字节”：分片只校验了长度/Content-Range，
/// 源可以在网络失败前写入内容错误的段，镜像补完缺口后整文件 sha 不符；
/// 重试又从坏源开始，任务永远收敛不了。没有逐段哈希就不能跨源复用。
/// 现在换源从零开始；同一个源自己的续传不受影响。
fn part_key_for(task: &Task) -> String {
    part_key(&task.url)
}

/// 这个任务可能落过盘的所有源 URL（主 url + sources，去重保序）。
fn task_source_urls(task: &Task) -> Vec<String> {
    let mut out = Vec::new();
    for s in std::iter::once(task.url.clone()).chain(task.sources.iter().cloned()) {
        let s = s.trim().to_string();
        if !s.is_empty() && !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

/// 清掉这个任务在所有源上的分片/位图。
///
/// 多源任务每个源各自一份 `.part`；只清当前源会留下别的源的残留，
/// 下一次重试又可能复用 —— 清理必须按 source 列表全走一遍。
pub fn discard_task_parts(task: &Task) -> Result<()> {
    let dir = paths::parts_dir();
    // 逐个源清：某个源的锁被在途请求占着（PartBusy）时跳过它，
    // 不能因为第一个源忙就放弃后面所有空闲源的清理。
    let mut first_err: Option<anyhow::Error> = None;
    for url in task_source_urls(task) {
        let key = part_key(&url);
        let lock = PartLock::acquire(&dir.join(format!("{key}.part")), &url);
        let bt_lock = PartLock::acquire(&dir.join(format!("bt-{key}")), &url);
        let (_lock, _bt_lock) = match (lock, bt_lock) {
            (Ok(a), Ok(b)) => (a, b),
            (a, b) => {
                if first_err.is_none() {
                    first_err = a.err().or_else(|| b.err());
                }
                continue;
            }
        };
        for name in [
            format!("{key}.part"),
            format!("{key}.json"),
            format!("{key}.mp4"),
        ] {
            match fs::remove_file(dir.join(&name)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    if first_err.is_none() {
                        first_err = Some(
                            anyhow::Error::new(e)
                                .context(format!("删不掉 {name}")),
                        );
                    }
                }
            }
        }
        let bt = dir.join(format!("bt-{key}"));
        if bt.is_dir() {
            if let Err(e) = fs::remove_dir_all(&bt) {
                if first_err.is_none() {
                    first_err = Some(
                        anyhow::Error::new(e)
                            .context(format!("删不掉 {}", bt.display())),
                    );
                }
            }
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

fn check_scheme(url: &str) -> Result<()> {
    let lower = url.trim().to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return Ok(());
    }
    let scheme = lower.split("://").next().unwrap_or("");
    bail!(
        "这个地址的类型是 {}，天网盒子只接 http/https 直链与种子（magnet、.torrent）。\
         要登录的网盘、thunder://、ed2k:// 这类我们不做——它们不是下载协议，是商业锁。",
        if scheme.is_empty() { "未知" } else { scheme }
    )
}

fn http_reason(code: u16) -> String {
    match code {
        401 | 403 => format!(
            "这个地址不对外直取（HTTP {code}）：需要登录、授权或防盗链。去浏览器里确认能不能直接下，别在这重试。"
        ),
        404 | 410 => format!("地址不存在（HTTP {code}）：链接过期或打错了。"),
        429 => format!("被限流了（HTTP {code}）：等一会儿再试。"),
        c if (500..600).contains(&c) => format!("服务器出错（HTTP {c}）：不是你的问题，稍后重试。"),
        c => format!("服务器拒绝了请求（HTTP {c}）。"),
    }
}

fn human_err(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "超时".into()
    } else if e.is_connect() {
        "连不上（DNS 或网络）".into()
    } else if e.is_decode() {
        "数据解不开".into()
    } else {
        e.to_string()
    }
}

fn header_u64(r: &reqwest::blocking::Response, h: reqwest::header::HeaderName) -> Option<u64> {
    r.headers()
        .get(h)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
}

fn filename_of(r: &reqwest::blocking::Response) -> Option<String> {
    let cd = r.headers().get(CONTENT_DISPOSITION)?.to_str().ok()?;
    for part in cd.split(';') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix("filename*=") {
            // RFC 5987: UTF-8''name
            if let Some(idx) = v.find("''") {
                return percent_decode(&v[idx + 2..]);
            }
        }
        if let Some(v) = part.strip_prefix("filename=") {
            let v = v.trim().trim_matches('"');
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn name_from_url(url: &str) -> String {
    let no_query = url.split(['?', '#']).next().unwrap_or(url);
    // 只看 path：域名不配当文件名（https://x.org/ 应该得到一个占位名）
    let after_scheme = no_query.split("://").nth(1).unwrap_or(no_query);
    let path = after_scheme.splitn(2, '/').nth(1).unwrap_or("");
    let last = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    let name = percent_decode(last).unwrap_or_else(|| last.to_string());
    if name.is_empty() {
        format!("download-{}", now_ms())
    } else {
        name
    }
}

/// 文件名落到本地目录前必须过一遍：种子和服务器都能给任意字符串，
/// `../` 会让人把文件写到别的地方去。
fn sanitize_name(name: &str) -> String {
    let base = name.rsplit('/').next().unwrap_or(name);
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.').trim();
    let s = if trimmed.is_empty() || trimmed == ".." {
        format!("download-{}", now_ms())
    } else {
        trimmed.to_string()
    };
    if s.len() > 180 {
        let ext = Path::new(&s).extension().map(|e| e.to_string_lossy().to_string());
        let stem: String = s.chars().take(160).collect();
        match ext {
            Some(e) => format!("{stem}.{e}"),
            None => stem,
        }
    } else {
        s
    }
}

fn unique_path(p: &Path) -> PathBuf {
    // 用 symlink_metadata 而不是 exists()：悬空符号链接 exists()=false，
    // 会被当成“没人占用”而选中，随后 hard_link 失败、fs::copy 顺着链接写到根外。
    if fs::symlink_metadata(p).is_err() {
        return p.to_path_buf();
    }
    // 后缀插在**第一个**扩展名之前：`a.tar.gz` → `a (1).tar.gz`。
    // 用 file_stem 会切出 `a.tar (1).gz`，看着像把文件改坏了。
    let name = p
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let (stem, ext) = match name.find('.') {
        Some(i) => (name[..i].to_string(), name[i..].to_string()),
        None => (name.clone(), String::new()),
    };
    let stem = if stem.is_empty() { name } else { stem };
    let parent = p.parent().unwrap_or(Path::new("."));
    for i in 1..10_000 {
        let cand = parent.join(format!("{stem} ({i}){ext}"));
        if fs::symlink_metadata(&cand).is_err() {
            return cand;
        }
    }
    p.to_path_buf()
}

/// 这个 dest 是不是明确指向一个目录。
///
/// 两种判据：盘上真的是目录，或者**写法上就以分隔符结尾**。
/// 后者是给 `-o /tmp/还没建的目录` 用的 —— 不然它会被当成文件名，
/// 文件就落成 `/tmp/还没建的目录`（没有扩展名），台账里的 path 也是错的。
/// 把任务的 dest 展开成绝对路径，同时保留「尾斜杠 = 目录」这个信号。
fn normalize_task_dest(dest: &str) -> String {
    let was_dir = dest.ends_with('/');
    let expanded = crate::settings::expand(dest);
    if was_dir && !expanded.is_empty() && !expanded.ends_with('/') {
        format!("{expanded}/")
    } else {
        expanded
    }
}

fn dest_is_dir(task: &Task) -> bool {
    task.dest.ends_with('/') || Path::new(&task.dest).is_dir()
}

fn dest_dir(task: &Task) -> PathBuf {
    let d = Path::new(&task.dest);
    if dest_is_dir(task) {
        return d.to_path_buf();
    }
    match d.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => crate::settings::dest_dir(),
    }
}

/// 一段字节的 sha256，给自检和测试算期望值用。
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

pub fn short(s: &str) -> String {
    s.chars().take(12).collect()
}

/// 播放时长：`12:34` / `1:02:03`。切片流的进度用它，不用字节。
pub fn clock(s: u64) -> String {
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{sec:02}")
    } else {
        format!("{m}:{sec:02}")
    }
}

/// 千进制，跟界面上其他地方一致（不玩 1024/1000 两套）。
pub fn human(n: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1000.0 && i < U.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 {
        format!("{}{}", n, U[0])
    } else {
        format!("{:.1}{}", v, U[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_header_becomes_our_retry_hint() {
        // 服务器说等 7 秒，我们就照它说的等，而不是固定 5 秒。
        let info = classify_error(&anyhow::Error::new(Throttled { after_ms: 7_000, code: 429 }));
        assert_eq!(info.code, "http.throttled");
        assert!(info.retryable);
        assert_eq!(info.retry_after_ms, 7_000);
        assert!(info.next_action.contains("Retry-After"), "{}", info.next_action);

        // 没给 Retry-After：退回默认值，而不是退化成 0（0 等于"不要重试"）
        let info = classify_error(&anyhow::Error::new(Throttled { after_ms: 0, code: 503 }));
        assert_eq!(info.retry_after_ms, 5_000);
    }

    #[test]
    fn retry_after_parses_seconds_and_clamps_absurd_values() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert("retry-after", "3".parse().unwrap());
        assert_eq!(retry_after_ms(&h), 3_000);

        // 让等一小时 = 挂死；夹到 5 分钟，超了就交给"换源/失败"
        h.insert("retry-after", "99999".parse().unwrap());
        assert_eq!(retry_after_ms(&h), 300_000);

        // HTTP-date 形式极少见：当成没给，不能让解析失败影响下载
        h.insert("retry-after", "Wed, 21 Oct 2026 07:28:00 GMT".parse().unwrap());
        assert_eq!(retry_after_ms(&h), 0);

        assert_eq!(retry_after_ms(&reqwest::header::HeaderMap::new()), 0);
    }

    #[test]
    fn a_single_mirror_still_counts_as_multi_source() {
        let mut t = Task::new("https://a.example/x", "/tmp/x/");
        t.sources = vec!["https://b.example/x".into()];
        assert!(has_mirrors(&t), "单个镜像不能被静默忽略");
        t.sources.clear();
        assert!(!has_mirrors(&t));
    }

    #[test]
    fn multi_source_parts_are_per_source_not_shared() {
        // 没有逐段哈希就不能跨源复用：A 源写下的字节可能内容错误，
        // B 源补完缺口后整文件 sha 永远不符。分片键必须跟着源 URL 走。
        let mut t = Task::new("https://a.example/x", "/tmp/x/");
        t.sources = vec!["https://a.example/x".into(), "https://b.example/x".into()];
        t.expected_sha256 = "a".repeat(64);
        t.url = "https://a.example/x".into();
        let ka = part_key_for(&t);
        t.url = "https://b.example/x".into();
        let kb = part_key_for(&t);
        assert_ne!(ka, kb, "不同源不能共用同一个 .part/位图");
        assert_eq!(task_source_urls(&t).len(), 2);
        assert!(task_source_urls(&t).contains(&"https://a.example/x".to_string()));
    }

    #[test]
    fn plan_covers_everything_exactly_once() {
        for total in [2u64 << 20, 10 << 20, 300 << 20] {
            for threads in [1usize, 5, 16] {
                let shards = plan(total, threads);
                assert_eq!(shards[0].start, 0);
                assert_eq!(shards.last().unwrap().end, total);
                let mut pos = 0;
                for s in &shards {
                    assert_eq!(s.start, pos);
                    assert!(s.end > s.start);
                    pos = s.end;
                }
                let sum: u64 = shards.iter().map(|s| s.end - s.start).sum();
                assert_eq!(sum, total);
            }
        }
    }

    #[test]
    fn task_dest_expansion_preserves_directory_semantics() {
        assert_eq!(normalize_task_dest("/tmp/out/"), "/tmp/out/");
        assert_eq!(normalize_task_dest("/tmp/out"), "/tmp/out");
        let home = crate::paths::home_dir();
        assert_eq!(
            normalize_task_dest("~/x/"),
            format!("{}/x/", home.display())
        );
        assert_eq!(normalize_task_dest("x"), format!("{}/x", home.display()));
    }

    #[test]
    fn blob_of_rejects_path_traversal_and_non_ascii() {
        // 这些以前会变成任意文件读取或字符边界 panic；必须在拼路径前拒绝。
        assert!(blob_of("/etc/passwd").is_none());
        assert!(blob_of("../x").is_none());
        assert!(blob_of("中").is_none());
        assert!(blob_of("ab").is_none());
    }

    #[test]
    fn sanitize_blocks_traversal() {
        assert_eq!(sanitize_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_name("a/b/c.txt"), "c.txt");
        // 空名回退到 download-<毫秒>：两次调用可能跨毫秒，不能拿两次结果比相等。
        let fallback = sanitize_name("..");
        assert!(fallback.starts_with("download-"), "{fallback}");
        assert!(!fallback.contains(".."));
        assert_eq!(sanitize_name("名字 正常.zip"), "名字 正常.zip");
    }

    #[test]
    fn name_from_url_handles_query_and_encoding() {
        assert_eq!(name_from_url("https://x.org/a/b.zip?token=1"), "b.zip");
        assert_eq!(name_from_url("https://x.org/%E4%B8%AD.zip"), "中.zip");
        // 空路径退回「download-<毫秒时间戳>」：两次调用可能落在不同毫秒上，
        // 拿它俩比相等是个会偶发失败的假断言（本机跑四遍里红过一遍）。
        assert!(name_from_url("https://x.org/").starts_with("download-"));
    }

    #[test]
    fn human_is_decimal() {
        assert_eq!(human(999), "999B");
        assert_eq!(human(1000), "1.0KB");
        assert_eq!(human(1_500_000), "1.5MB");
    }

    #[test]
    fn scheme_gate_refuses_locks() {
        assert!(check_scheme("https://a/b").is_ok());
        let e = check_scheme("thunder://QUFodHRw").unwrap_err().to_string();
        assert!(e.contains("thunder"), "{e}");
    }

    #[test]
    fn landing_on_its_own_hardlink_does_not_truncate_the_file() {
        let dir = std::env::temp_dir().join(format!("skyfetch-land-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let blob = dir.join("blob");
        fs::write(&blob, vec![7u8; 4096]).unwrap();
        let dest = dir.join("landed.bin");
        fs::hard_link(&blob, &dest).unwrap();
        assert!(same_file(&blob, &dest));

        // 这就是那个会清空文件的调用：源和目标同一个 inode
        land(&blob, &dest).unwrap();
        assert_eq!(fs::metadata(&dest).unwrap().len(), 4096, "不许被截成 0");
        assert_eq!(fs::metadata(&blob).unwrap().len(), 4096);

        // 内容不同的目标：正常覆盖过去
        let other = dir.join("other.bin");
        fs::write(&other, b"old").unwrap();
        land(&blob, &other).unwrap();
        assert_eq!(fs::metadata(&other).unwrap().len(), 4096);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn retry_lands_on_the_same_file_instead_of_making_copies() {
        let dir = std::env::temp_dir().join(format!("skyfetch-dest-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let existing = dir.join("llama.tar.gz");
        fs::write(&existing, b"x").unwrap();

        let mut t = Task::new(
            "https://example.org/llama.tar.gz",
            &dir.to_string_lossy().to_string(),
        );
        // 第一次：目录里已经有同名文件 → 不覆盖，加 (1)
        let first = pick_dest(&t, &dir, "llama.tar.gz");
        // 同名不覆盖：后缀插在最后一个点之前（llama (1).tar.gz），不是接在扩展名后面
        assert_eq!(first, dir.join("llama (1).tar.gz"), "新下载不许覆盖已有文件");

        // 重试：落回台账记着的那条路径，不攒副本
        t.rerun = true;
        t.dest = existing.display().to_string();
        assert_eq!(pick_dest(&t, &dir, "llama.tar.gz"), existing);

        // 重试但原文件已经不在了：还是走 unique_path（别把 dest 当成幽灵）
        t.dest = dir.join("gone.tar.gz").display().to_string();
        assert_eq!(
            pick_dest(&t, &dir, "llama.tar.gz"),
            dir.join("llama (1).tar.gz")
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn early_dest_treats_an_existing_directory_as_a_directory() {
        let dir = std::env::temp_dir().join(format!(
            "skyget-early-dest-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // 没有尾斜杠、但盘上确实是目录：缓存捷径必须落到 dir/文件名，
        // 而不是把目录本身当成文件路径。
        let t = Task::new("https://example.org/file.bin", &dir.display().to_string());
        assert_eq!(early_dest(&t), dir.join("file.bin"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failures_are_classified_for_retry() {
        use Failure::*;
        // 网不通：值得等一会儿再来
        assert_eq!(classify_failure("第 3 段：请求失败：超时"), Network);
        assert_eq!(classify_failure("error sending request: connection refused"), Network);
        assert_eq!(classify_failure("dns error: failed to lookup address"), Network);
        // 被限流：也是等一会儿再来
        assert_eq!(classify_failure("服务器限流（HTTP 429）"), Throttle);
        assert_eq!(classify_failure("HTTP 503"), Throttle);
        // 重试没用：源不支持分段、内容变了、校验不符
        assert_eq!(
            classify_failure("服务器不接受分段请求（HTTP 406）"),
            Other
        );
        assert_eq!(classify_failure("校验不符：期望 00…，实际 ab…"), Other);
        assert_eq!(classify_failure("这个地址的内容变了：上次…"), Other);
    }

    #[test]
    fn error_codes_are_machine_actionable() {
        // agent 要能直接分支：码 + 是否可重试 + 等多久 + 下一步
        let e = classify_message("校验不符：期望 00…，实际 ab…");
        assert_eq!(e.code, "verify.mismatch");
        assert!(!e.retryable);
        assert!(e.next_action.contains("sha256"));

        let e = classify_message("服务器限流（HTTP 429）");
        assert_eq!(e.code, "http.throttled");
        assert!(e.retryable);
        assert!(e.retry_after_ms >= 1_000);

        let e = classify_message("地址不存在（HTTP 404）：链接过期或打错了。");
        assert_eq!(e.code, "http.not_found");
        assert!(!e.retryable);

        let e = classify_message("服务器不接受分段请求（HTTP 406）");
        assert_eq!(e.code, "source.no_range");
        assert!(e.next_action.contains("--jobs 1"));

        let e = classify_message("另一个进程正在下同一个地址（pid 42）");
        assert_eq!(e.code, "part.busy");
        assert!(e.retryable);

        // 重定向里被策略拒绝：不能变成可无限重试的网络错误
        let e = classify_message("策略拒绝：地址 http://x 解析到非公网地址 127.0.0.1，已拒绝。");
        assert_eq!(e.code, "policy.egress_denied");
        assert!(!e.retryable);

        // 中文 DNS 失败要归到可重试的网络错误
        let e = classify_message("域名解析失败（example.invalid）：failed to lookup address");
        assert_eq!(e.code, "network.connect");
        assert!(e.retryable);
    }

    #[test]
    fn segment_failures_are_one_per_line_and_sorted() {
        // 故意打乱顺序：线程是按完成顺序 push 的
        let items = vec![
            (2usize, "超时".to_string()),
            (0usize, "超时".to_string()),
            (1usize, "请求失败：连接被重置".to_string()),
        ];
        let msg = segment_failure_message(&items);
        let lines: Vec<&str> = msg.lines().collect();
        assert_eq!(lines.len(), 4, "一行结论 + 三段各一行");
        assert_eq!(lines[0], "3 段没下来（已下的保留，重跑会接着下）");
        assert_eq!(lines[1], "第 1 段：超时");
        assert_eq!(lines[2], "第 2 段：请求失败：连接被重置");
        assert_eq!(lines[3], "第 3 段：超时");
        // 不许出现「第 3 段」接在「第 1 段」后面那种乱序
        let idx: Vec<usize> = lines[1..]
            .iter()
            .map(|l| {
                l.trim_start_matches("第 ")
                    .split(' ')
                    .next()
                    .unwrap()
                    .trim_end_matches('段')
                    .parse()
                    .unwrap()
            })
            .collect();
        assert_eq!(idx, vec![1, 2, 3]);
    }

    #[test]
    fn http_reasons_are_human() {
        assert!(http_reason(403).contains("需要登录"));
        assert!(http_reason(404).contains("不存在"));
        assert!(http_reason(503).contains("服务器出错"));
    }
}
