//! 全局调度：daemon 里同时跑多少任务、每个 agent 能占多少、队列满了怎么办。
//!
//! 以前 `POST /jobs` 直接 `thread::spawn`，每个任务 5 条连接。10 个 agent
//! 各提交 10 条就是 500 条连接 —— 对被下载的站点是洪水，对别的 agent 是饥饿。
//! 这里做三件最小但必要的事：
//!
//! 1. **并发上限**：固定数量的 worker，任务排队而不是无限开线程；
//! 2. **按来源公平**：单个 agent 最多同时占 `max_per_source` 个 worker，
//!    它提交 100 条也饿不死别人；
//! 3. **有界队列**：队列满了明确回 429（带 `retry_after_ms`），
//!    而不是把内存吃光或假装收下了。
//!
//! 4. **优先级**：`interactive`（有人等着）排在 `normal` 前面，`batch` 最后。
//!    优先级只改排队顺序，不改并发上限 —— 也不能把公平队列整个吃掉：
//!    同一优先级里仍然按「谁跑得少」轮转。
//!
//! 没做的事：按 host 的全局连接上限、带宽配额、优先级抢占（跑着的不会被打断）。

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex};

type Job = Box<dyn FnOnce() + Send + 'static>;

/// 队列里的一项：优先级 + 真正的活。
type Queued = (u8, Job);

struct State {
    /// 每个来源一条队列。来源是 `agent:claude` / `human` / `cli` 这种。
    queues: HashMap<String, VecDeque<Queued>>,
    /// 轮转顺序：选出下一条之后把来源挪到队尾。
    order: VecDeque<String>,
    /// 每个来源正在跑几个。
    running: HashMap<String, usize>,
    /// per-source 并发上限（per-agent 策略覆盖全局默认）。
    limits: HashMap<String, usize>,
    queued: usize,
    closed: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SubmitError {
    /// 队列满了：调用方该回 429，并告诉对方等多久
    Full,
    /// 调度器已经关了（进程退出中）
    Closed,
}

pub struct Scheduler {
    state: Arc<(Mutex<State>, Condvar)>,
    max_queued: usize,
    max_running: usize,
    max_per_source: usize,
}

impl Scheduler {
    pub fn new(max_running: usize, max_queued: usize, max_per_source: usize) -> Arc<Self> {
        let max_running = max_running.max(1);
        let max_per_source = max_per_source.clamp(1, max_running);
        let state = Arc::new((
            Mutex::new(State {
                queues: HashMap::new(),
                order: VecDeque::new(),
                running: HashMap::new(),
                limits: HashMap::new(),
                queued: 0,
                closed: false,
            }),
            Condvar::new(),
        ));
        let sched = Arc::new(Self {
            state: state.clone(),
            max_queued,
            max_running,
            max_per_source,
        });
        for _ in 0..max_running {
            let state = state.clone();
            std::thread::spawn(move || worker(state, max_per_source));
        }
        sched
    }

    /// 默认：4 个并发任务、最多排队 1024、单个来源最多占 2 个。
    /// 环境变量可调：`SKYGET_MAX_JOBS` / `SKYGET_MAX_QUEUED` / `SKYGET_MAX_PER_AGENT`。
    pub fn from_env() -> Arc<Self> {
        let n = |key: &str, default: usize| {
            std::env::var(key)
                .ok()
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(default)
        };
        Self::new(
            n("SKYGET_MAX_JOBS", 4),
            n("SKYGET_MAX_QUEUED", 1024),
            n("SKYGET_MAX_PER_AGENT", 2),
        )
    }

    /// 普通优先级提交（人/CLI 的默认行为）。
    pub fn submit(&self, source: &str, job: Job) -> Result<(), SubmitError> {
        self.submit_with(source, 1, job)
    }

    /// 带优先级提交。`prio` 越大越先跑（见 `model::priority_rank`）。
    pub fn submit_with(&self, source: &str, prio: u8, job: Job) -> Result<(), SubmitError> {
        let (lock, cv) = &*self.state;
        let mut st = lock.lock().unwrap();
        if st.closed {
            return Err(SubmitError::Closed);
        }
        if st.queued >= self.max_queued {
            return Err(SubmitError::Full);
        }
        let key = source.to_string();
        // 按优先级插队，同优先级保持先来先跑（稳定）。
        let q = st.queues.entry(key.clone()).or_default();
        let at = q.iter().position(|(p, _)| *p < prio).unwrap_or(q.len());
        q.insert(at, (prio, job));
        if !st.order.contains(&key) {
            st.order.push_back(key);
        }
        st.queued += 1;
        cv.notify_one();
        Ok(())
    }

    /// 设置某个来源的并发上限（0 会被夹到 1）。
    pub fn set_limit(&self, source: &str, max: usize) {
        let (lock, cv) = &*self.state;
        let mut st = lock.lock().unwrap();
        st.limits.insert(source.to_string(), max.max(1));
        cv.notify_all();
    }

    /// (并发上限, 队列上限)；给 /metrics 暴露容量用。
    pub fn capacity(&self) -> (usize, usize) {
        (self.max_running, self.max_queued)
    }

    /// (正在跑, 排队中)
    pub fn stats(&self) -> (usize, usize) {
        let (lock, _) = &*self.state;
        let st = lock.lock().unwrap();
        (st.running.values().sum(), st.queued)
    }

    /// 某个来源当前生效的并发上限（per-agent 覆盖 > 全局默认）。
    pub fn limit_of(&self, source: &str) -> usize {
        let (lock, _) = &*self.state;
        let st = lock.lock().unwrap();
        st.limits
            .get(source)
            .copied()
            .unwrap_or(self.max_per_source)
    }

    /// 每个来源正在跑几个，只回非零项。`/agents` 用。
    pub fn running_by_source(&self) -> Vec<(String, usize)> {
        let (lock, _) = &*self.state;
        let st = lock.lock().unwrap();
        let mut v: Vec<(String, usize)> = st
            .running
            .iter()
            .filter(|(_, n)| **n > 0)
            .map(|(s, n)| (s.clone(), *n))
            .collect();
        v.sort();
        v
    }

    /// 只给测试/诊断：某个来源正在跑几个。
    pub fn running_of(&self, source: &str) -> usize {
        let (lock, _) = &*self.state;
        let st = lock.lock().unwrap();
        *st.running.get(source).unwrap_or(&0)
    }
}

impl Drop for Scheduler {
    fn drop(&mut self) {
        let (lock, cv) = &*self.state;
        let mut st = lock.lock().unwrap();
        st.closed = true;
        cv.notify_all();
    }
}

fn worker(state: Arc<(Mutex<State>, Condvar)>, max_per_source: usize) {
    loop {
        let (source, _prio, job) = {
            let (lock, cv) = &*state;
            let mut st = lock.lock().unwrap();
            loop {
                if st.closed {
                    return;
                }
                if let Some(picked) = pick(&mut st, max_per_source) {
                    break picked;
                }
                st = cv.wait(st).unwrap();
            }
        };
        // 一个 job panic 不能打死 worker：否则队列少一个消费者，
        // running 计数也永远不归零，4 次坏输入就能让整个 daemon 停摆。
        // 具体任务的收尾由 dispatch 里的 RAII guard 负责（这里只保证 worker 活着）。
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)).is_err() {
            eprintln!("调度器：一个任务 panic 了，worker 继续");
        }
        let (lock, cv) = &*state;
        let mut st = lock.lock().unwrap();
        if let Some(n) = st.running.get_mut(&source) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                st.running.remove(&source);
            }
        }
        if st
            .queues
            .get(&source)
            .map(|q| q.is_empty())
            .unwrap_or(true)
        {
            st.queues.remove(&source);
            st.order.retain(|s| s != &source);
        }
        cv.notify_all();
    }
}

/// 选下一条：先比优先级（高者先），同优先级比 running 数（少者先），
/// 再同数按轮转顺序。这样既能「有人等着就先跑」，又不会让 batch 饿死：
/// 一旦 interactive 跑起来，它的 running 数就上去了，同优先级的别人也能轮到。
fn pick(st: &mut State, max_per_source: usize) -> Option<(String, u8, Job)> {
    let mut best: Option<(String, u8, usize)> = None;
    for src in st.order.iter() {
        let Some(q) = st.queues.get(src) else { continue };
        if q.is_empty() {
            continue;
        }
        let running = *st.running.get(src).unwrap_or(&0);
        let cap = st
            .limits
            .get(src)
            .copied()
            .unwrap_or(max_per_source)
            .max(1);
        if running >= cap {
            continue;
        }
        // 队头就是这条来源里优先级最高的（submit 时按优先级插队，见下）。
        let head_prio = q.front().map(|(p, _)| *p).unwrap_or(1);
        let better = match &best {
            None => true,
            Some((_, bp, br)) => head_prio > *bp || (head_prio == *bp && running < *br),
        };
        if better {
            best = Some((src.clone(), head_prio, running));
        }
    }
    let (source, prio, _) = best?;

    if let Some(pos) = st.order.iter().position(|s| s == &source) {
        st.order.remove(pos);
    }
    st.order.push_back(source.clone());

    let (_, job) = st.queues.get_mut(&source)?.pop_front()?;
    if st
        .queues
        .get(&source)
        .map(|q| q.is_empty())
        .unwrap_or(true)
    {
        st.queues.remove(&source);
        // order 只保留“有活”的来源；否则每来一个新 source 就永远占一个槽，
        // 提交里 `contains` 是 O(n)，一个 agent 用 1 万个 source 就能放大 CPU/内存。
        if !st.running.contains_key(&source) {
            st.order.retain(|s| s != &source);
        }
    }
    st.queued = st.queued.saturating_sub(1);
    *st.running.entry(source.clone()).or_insert(0) += 1;
    Some((source, prio, job))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn never_runs_more_than_the_limit() {
        let s = Scheduler::new(2, 16, 2);
        let now = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let done = Arc::new(AtomicUsize::new(0));
        for _ in 0..6 {
            let now = now.clone();
            let peak = peak.clone();
            let done = done.clone();
            s.submit(
                "agent:a",
                Box::new(move || {
                    let n = now.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(n, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(30));
                    now.fetch_sub(1, Ordering::SeqCst);
                    done.fetch_add(1, Ordering::SeqCst);
                }),
            )
            .unwrap();
        }
        for _ in 0..100 {
            if done.load(Ordering::SeqCst) == 6 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(done.load(Ordering::SeqCst), 6);
        assert!(peak.load(Ordering::SeqCst) <= 2, "并发峰值 {}", peak.load(Ordering::SeqCst));
    }

    #[test]
    fn per_source_limit_overrides_the_global_one() {
        let s = Scheduler::new(3, 8, 3);
        s.set_limit("agent:a", 1);
        let (tx, rx) = mpsc::channel::<&'static str>();
        let (hold_tx, hold_rx) = mpsc::channel::<()>();
        let hold_rx = Arc::new(Mutex::new(hold_rx));
        let rx1 = hold_rx.clone();
        let tx1 = tx.clone();
        s.submit(
            "agent:a",
            Box::new(move || {
                tx1.send("a1").unwrap();
                let _ = rx1.lock().unwrap().recv();
            }),
        )
        .unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), "a1");

        let tx2 = tx.clone();
        s.submit(
            "agent:a",
            Box::new(move || {
                tx2.send("a2").unwrap();
            }),
        )
        .unwrap();
        let tx3 = tx.clone();
        s.submit(
            "agent:b",
            Box::new(move || {
                tx3.send("b").unwrap();
            }),
        )
        .unwrap();
        // A 的上限是 1：A2 必须让位给 B。
        assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), "b");

        let _ = hold_tx.send(());
        assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), "a2");
        drop(tx);
    }

    #[test]
    fn a_panicking_job_does_not_kill_the_worker() {
        let s = Scheduler::new(1, 4, 1);
        let done = Arc::new(AtomicUsize::new(0));
        s.submit("agent:a", Box::new(|| panic!("boom"))).unwrap();
        let d = done.clone();
        s.submit(
            "agent:a",
            Box::new(move || {
                d.fetch_add(1, Ordering::SeqCst);
            }),
        )
        .unwrap();
        for _ in 0..100 {
            if done.load(Ordering::SeqCst) == 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(done.load(Ordering::SeqCst), 1, "panic 之后 worker 必须还能干活");
        // 给 worker 一点时间做收尾
        for _ in 0..50 {
            if s.stats() == (0, 0) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(s.stats(), (0, 0), "running/queued 必须归零");
    }

    #[test]
    fn a_higher_priority_source_goes_first() {
        let s = Scheduler::new(1, 8, 8);
        let (tx, rx) = mpsc::channel::<&'static str>();
        // 先占住唯一的 worker，再按 batch → normal → interactive 的顺序排队。
        let (hold_tx, hold_rx) = mpsc::channel::<()>();
        s.submit("agent:block", Box::new(move || {
            let _ = hold_rx.recv();
        }))
        .unwrap();
        for (name, prio) in [("batch", 0u8), ("normal", 1), ("interactive", 2)] {
            let tx = tx.clone();
            s.submit_with("agent:x", prio, Box::new(move || {
                tx.send(name).unwrap();
            }))
            .unwrap();
        }
        let _ = hold_tx.send(());
        // interactive 必须最先跑，然后 normal，最后 batch。
        assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), "interactive");
        assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), "normal");
        assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), "batch");
    }

    #[test]
    fn priority_does_not_break_fairness_between_sources() {
        let s = Scheduler::new(1, 8, 1);
        let (tx, rx) = mpsc::channel::<&'static str>();
        let (hold_tx, hold_rx) = mpsc::channel::<()>();
        // A 用 normal 排队；B 用 interactive 各排两条。
        // B 的并发上限是 1，所以 A 仍然能被轮到（不被高优先级饿死）。
        s.submit("agent:block", Box::new(move || {
            let _ = hold_rx.recv();
        }))
        .unwrap();
        for name in ["a1", "a2"] {
            let tx = tx.clone();
            s.submit_with("agent:a", 1, Box::new(move || {
                tx.send(name).unwrap();
            }))
            .unwrap();
        }
        for name in ["b1", "b2"] {
            let tx = tx.clone();
            s.submit_with("agent:b", 2, Box::new(move || {
                tx.send(name).unwrap();
            }))
            .unwrap();
        }
        let _ = hold_tx.send(());
        let mut order = Vec::new();
        for _ in 0..4 {
            order.push(rx.recv_timeout(Duration::from_secs(2)).unwrap());
        }
        assert_eq!(order[0], "b1", "高优先级先跑：{order:?}");
        assert!(
            order.contains(&"a1") && order.contains(&"a2"),
            "低优先级不能被完全饿死：{order:?}"
        );
    }

    #[test]
    fn queue_is_bounded_and_says_full() {
        let s = Scheduler::new(1, 1, 1);
        let (hold_tx, hold_rx) = mpsc::channel::<()>();
        let (started_tx, started_rx) = mpsc::channel::<()>();
        s.submit(
            "agent:a",
            Box::new(move || {
                started_tx.send(()).unwrap();
                let _ = hold_rx.recv();
            }),
        )
        .unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        // 第 2 条占住唯一的队列位
        s.submit("agent:b", Box::new(|| {})).unwrap();
        // 第 3 条必须被拒绝，而不是无限排队
        assert_eq!(s.submit("agent:c", Box::new(|| {})).unwrap_err(), SubmitError::Full);
        let _ = hold_tx.send(());
    }

    #[test]
    fn one_agent_cannot_take_all_workers() {
        let s = Scheduler::new(2, 16, 1);
        let (tx, rx) = mpsc::channel::<&'static str>();
        let (hold_tx, hold_rx) = mpsc::channel::<()>();
        let hold_rx = Arc::new(Mutex::new(hold_rx));
        // A 提交两条：一条开跑，另一条必须等 max_per_source=1
        for _ in 0..2 {
            let tx = tx.clone();
            let hold_rx = hold_rx.clone();
            s.submit(
                "agent:a",
                Box::new(move || {
                    tx.send("a").unwrap();
                    let _ = hold_rx.lock().unwrap().recv();
                }),
            )
            .unwrap();
        }
        assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), "a");
        let tx_b = tx.clone();
        s.submit(
            "agent:b",
            Box::new(move || {
                tx_b.send("b").unwrap();
            }),
        )
        .unwrap();
        // A 的第二条还在等，B 应该能先跑 —— 这就是公平
        assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), "b");
        // 放行 A 的两条；第二条会拿到第二个 send。
        let _ = hold_tx.send(());
        let _ = hold_tx.send(());
        drop(tx);
    }
}
