//! 自带体检：一条命令把关键路径全跑一遍。
//!
//! 为什么要有它：e2e / hostile / crash 那几套脚本要么要外网、要么要一份素材目录，
//! 「这东西是对的」就不该依赖某个人的机器和记性。这里所有场景都由**进程内**的一个
//! 小 HTTP 服务提供（支持 Range、也能演假 Range / 429 / 慢速），所以断网也能跑。
//!
//! 判定只看行为：下载出来的字节对不对、校验不符有没有落地、暂停之后有没有重下、
//! 秒传有没有真的不发请求。不看日志措辞。

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;

use crate::{model::STATE_INTERRUPTED, paths, run_cancellable, Cancel, Engine, Event, Store, Task};

const SEG_SIZE: usize = 8 * 1024 * 1024; // 分段下载用：要够大才分得动
const SLOW_SIZE: usize = 4 * 1024 * 1024; // 暂停用：慢一点才来得及喊停

/// 确定性内容：同一份代码在任何机器上算出同一个 sha256。
fn payload(size: usize, seed: u8) -> Vec<u8> {
    (0..size)
        .map(|i| ((i as u64 * 31 + seed as u64 * 7) % 251) as u8)
        .collect()
}

/// 服务端要演哪种脾气。
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// 正常：认 Range，回 206
    Ok,
    /// 不认 Range：永远 200 给全长
    NoRange,
    /// 假 Range：回 206，但给的字节不是要的那一段
    Lie,
    /// 前几次 429，之后正常
    Flaky,
    /// 慢速：每 64KB 停一下，好让「暂停」来得及发生
    Slow,
}

struct Stat {
    requests: AtomicUsize,
    /// 服务端一共发出多少字节 —— 续传的判据不是「从哪开始」，而是「一共重发了多少」
    bytes_sent: AtomicU64,
    /// 第一条「从中间开始」的 Range 起点 —— 大于 0 就证明是接着下，不是从头
    resume_from: AtomicU64,
    threes: AtomicUsize,
}

struct Server {
    port: u16,
    stat: Arc<Stat>,
    stop: Arc<AtomicBool>,
}

impl Server {
    fn start(mode: Mode, size: usize, seed: u8) -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        let data = Arc::new(payload(size, seed));
        let stat = Arc::new(Stat {
            requests: AtomicUsize::new(0),
            bytes_sent: AtomicU64::new(0),
            resume_from: AtomicU64::new(0),
            threes: AtomicUsize::new(0),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let (st, sp, d) = (stat.clone(), stop.clone(), data.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if sp.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(s) = stream else { continue };
                let (st, d) = (st.clone(), d.clone());
                std::thread::spawn(move || {
                    let _ = serve(s, &d, &st, mode);
                });
            }
        });
        Ok(Self { port, stat, stop })
    }

    fn url(&self, name: &str) -> String {
        format!("http://127.0.0.1:{}/{}", self.port, name)
    }

    fn requests(&self) -> usize {
        self.stat.requests.load(Ordering::Relaxed)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // 叫醒 accept：连一下自己就退出来了
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

fn serve(mut s: TcpStream, data: &[u8], stat: &Stat, mode: Mode) -> Result<()> {
    s.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut r = BufReader::new(s.try_clone()?);
    let mut line = String::new();
    r.read_line(&mut line)?;
    let n = stat.requests.fetch_add(1, Ordering::Relaxed);
    let mut range: Option<(usize, usize)> = None;
    loop {
        let mut h = String::new();
        if r.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" || h == "\n" {
            break;
        }
        if let Some(v) = h.to_ascii_lowercase().strip_prefix("range:") {
            let v = v.trim().trim_start_matches("bytes=");
            let mut it = v.split('-');
            let a = it
                .next()
                .and_then(|x| x.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let b = it
                .next()
                .and_then(|x| x.trim().parse::<usize>().ok())
                .unwrap_or(data.len() - 1)
                .min(data.len() - 1);
            range = Some((a, b));
        }
    }

    if mode == Mode::Flaky && n < 3 {
        stat.threes.fetch_add(1, Ordering::Relaxed);
        s.write_all(b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")?;
        return Ok(());
    }

    // 真实引擎探测时会先要一小段
    let (status, body, cr) = match (mode, range) {
        (Mode::NoRange, _) | (Mode::Lie, None) | (Mode::Ok, None) | (Mode::Slow, None) | (Mode::Flaky, None) => {
            (200u16, data.to_vec(), String::new())
        }
        (Mode::Lie, Some((a, b))) => {
            // 假：说要 a..b，却从 0 开始给
            let _ = a;
            let end = b.min(data.len() - 1);
            (
                206,
                data[..=end].to_vec(),
                format!("bytes 0-{end}/{}", data.len()),
            )
        }
        (_, Some((a, b))) => {
            let end = b.min(data.len() - 1);
            if a > 0 && stat.resume_from.load(Ordering::Relaxed) == 0 {
                stat.resume_from.store(a as u64, Ordering::Relaxed);
            }
            (
                206,
                data[a..=end].to_vec(),
                format!("bytes {a}-{end}/{}", data.len()),
            )
        }
    };

    let head = if status == 206 {
        format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Type: application/octet-stream\r\nContent-Range: {cr}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
            body.len()
        )
    } else {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
            body.len()
        )
    };
    s.write_all(head.as_bytes())?;
    if mode == Mode::Slow {
        for chunk in body.chunks(64 * 1024) {
            s.write_all(chunk)?;
            s.flush().ok();
            std::thread::sleep(Duration::from_millis(20));
        }
    } else {
        s.write_all(&body)?;
    }
    s.flush().ok();
    stat.bytes_sent
        .fetch_add(body.len() as u64, Ordering::Relaxed);
    Ok(())
}

/// 一行结果。
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

/// 跑完全部体检。返回（是否全过，明细）。
pub fn run() -> Result<(bool, Vec<Check>)> {
    // 自己一个数据目录：绝不碰用户真正的台账和缓存
    let home = std::env::temp_dir().join(format!("skyget-selftest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home)?;
    std::env::set_var("SKYGET_HOME", &home);

    let store = Store::open()?;
    let mut checks: Vec<Check> = Vec::new();

    // ── 1. 分段下载 ──────────────────────────────────────────────────
    let srv = Server::start(Mode::Ok, SEG_SIZE, 1)?;
    let want = crate::engine::sha256_hex(&payload(SEG_SIZE, 1));
    let dir = home.join("out1");
    std::fs::create_dir_all(&dir)?;
    let mut t = Task::new(&srv.url("seg.bin"), &dir.display().to_string());
    t.expected_sha256 = want.clone();
    let out = run_cancellable(&store, &Engine::new(5), &mut t, &|_| {}, &Cancel::new());
    checks.push(Check {
        name: "分段下载 8.0MB".into(),
        ok: out.is_ok() && t.sha256 == want && srv.requests() >= 2,
        detail: format!("{} 次请求，sha {}", srv.requests(), if t.sha256 == want { "一致" } else { "不一致" }),
    });

    // ── 2. 秒传：同一个 sha 再来一次，一个请求都不该发 ────────────────
    let before = srv.requests();
    let mut t2 = Task::new(&srv.url("seg.bin"), &dir.display().to_string());
    t2.expected_sha256 = want.clone();
    let cached = Arc::new(AtomicBool::new(false));
    let c2 = cached.clone();
    let out2 = run_cancellable(
        &store,
        &Engine::new(5),
        &mut t2,
        &move |e: Event| {
            if let Event::Finished { cached } = e {
                c2.store(cached, Ordering::Relaxed);
            }
        },
        &Cancel::new(),
    );
    let after = srv.requests();
    // 秒传会顺手 touch 缓存块：prune 靠这个时间判断「最近用过」，
    // 不然天天在用的块和第 31 天没人理的块在它眼里一样老。
    let blob_mtime = paths::blobs_dir()
        .join(&want[..2])
        .join(&want)
        .metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map(|age| age.as_secs())
        .unwrap_or(u64::MAX);
    checks.push(Check {
        name: "秒传不重下且记下最近使用".into(),
        ok: out2.is_ok()
            && after == before
            && cached.load(Ordering::Relaxed)
            && blob_mtime < 60,
        detail: format!(
            "请求数 {} → {}，cached={}，缓存块 {} 秒前被碰过",
            before,
            after,
            cached.load(Ordering::Relaxed),
            blob_mtime
        ),
    });

    // ── 3. 校验不符：不许落地 ────────────────────────────────────────
    let dir3 = home.join("out3");
    std::fs::create_dir_all(&dir3)?;
    let mut t3 = Task::new(&srv.url("verify.bin"), &dir3.display().to_string());
    t3.expected_sha256 = "0".repeat(64);
    let err3 = run_cancellable(&store, &Engine::new(5), &mut t3, &|_| {}, &Cancel::new()).unwrap_err();
    let left = std::fs::read_dir(&dir3)?.count();
    checks.push(Check {
        name: "校验不符不落地".into(),
        ok: left == 0 && format!("{err3:#}").contains("文件已丢弃"),
        detail: format!("目标目录 {left} 个文件"),
    });

    // ── 4. 暂停：状态中断、分片留着；再跑接着下 ──────────────────────
    let slow = Server::start(Mode::Slow, SLOW_SIZE, 2)?;
    let dir4 = home.join("out4");
    std::fs::create_dir_all(&dir4)?;
    let mut t4 = Task::new(&slow.url("slow.bin"), &dir4.display().to_string());
    let cancel = Cancel::new();
    let done = Arc::new(AtomicU64::new(0));
    let (cn, dn) = (cancel.clone(), done.clone());
    let cb4 = move |e: Event| {
        if let Event::Progress { done: d, .. } = e {
            dn.store(d, Ordering::Relaxed);
            // 下到 1MB 就按暂停：模拟用户手快
            if d > 2_621_440 {
                cn.cancel();
            }
        }
    };
    let paused = run_cancellable(&store, &Engine::new(2), &mut t4, &cb4, &cancel);
    let paused_ok = paused.is_err()
        && t4.state == STATE_INTERRUPTED
        && done.load(Ordering::Relaxed) > 0
        && !std::fs::read_dir(paths::parts_dir())?.count().eq(&0);
    let mut t5 = Task::new(&slow.url("slow.bin"), &dir4.display().to_string());
    t5.rerun = true;
    t5.dest = t4.dest.clone();
    slow.stat.resume_from.store(0, Ordering::Relaxed);
    let sent_before = slow.stat.bytes_sent.load(Ordering::Relaxed);
    let resumed = run_cancellable(&store, &Engine::new(2), &mut t5, &|_| {}, &Cancel::new());
    let resent = slow
        .stat
        .bytes_sent
        .load(Ordering::Relaxed)
        .saturating_sub(sent_before);
    // 判据：第二趟只补了「没下到的那部分」。分片是按段位图跳过的，
    // 所以「从第几字节开始」不重要，重要的是没有把整个文件重发一遍。
    checks.push(Check {
        name: "暂停后接着下（不重下）".into(),
        ok: paused_ok && resumed.is_ok() && resent < (SLOW_SIZE as u64) * 7 / 10,
        detail: format!(
            "暂停在 {:.2}MB · 第二趟只补了 {:.2}MB（全量 {:.2}MB）· parts {} · {}",
            done.load(Ordering::Relaxed) as f64 / 1048576.0,
            resent as f64 / 1048576.0,
            SLOW_SIZE as f64 / 1048576.0,
            left_names(),
            match &paused {
                Ok(()) => "第一趟没停下来".to_string(),
                Err(e) => crate::engine::short(format!("{e:#}").lines().next().unwrap_or("")),
            }
        ),
    });

    // ── 4b. 暂停后：台账写的字节数必须是真的 ────────────────────────
    // 「中断」那行以前永远显示 0（已下 1.3MB 也一样），看起来像全丢了。
    let rec = store.get(&t4.id)?.unwrap_or_else(|| Task::new("x", "y"));
    let claimed = rec.bytes_done;
    let actual = done.load(Ordering::Relaxed);
    let close = claimed.abs_diff(actual) <= 128 * 1024; // 心跳与采样的时间差
    checks.push(Check {
        name: "暂停后台账字节数是真的".into(),
        ok: rec.state == STATE_INTERRUPTED && claimed > 0 && close,
        detail: format!(
            "台账 {} 字节 / 进度回调 {} 字节 / 状态 {}",
            claimed, actual, rec.state
        ),
    });

    // ── 5. 取消：分片清干净 ──────────────────────────────────────────
    let discarded = crate::engine::discard_parts(&slow.url("slow.bin")).is_ok();
    let key = crate::engine::part_key(&slow.url("slow.bin"));
    let left: Vec<String> = std::fs::read_dir(paths::parts_dir())?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with(&key))
        // `.lock` 是跨进程互斥用的，故意不删（unlink 和 flock 之间有竞态）；
        // 它只有几十字节，不是「临时分片没清干净」。
        .filter(|n| !n.ends_with(".lock"))
        .collect();
    checks.push(Check {
        name: "取消清掉临时分片".into(),
        ok: discarded && left.is_empty(),
        detail: if left.is_empty() {
            "分片与位图已清（锁文件保留）".to_string()
        } else {
            format!("还剩：{}", left.join("、"))
        },
    });

    // ── 6. 服务端骗人：假 Range 要拒绝，并给逃生口 ───────────────────
    let lie = Server::start(Mode::Lie, SEG_SIZE, 3)?;
    let dir6 = home.join("out6");
    std::fs::create_dir_all(&dir6)?;
    let mut t6 = Task::new(&lie.url("lie.bin"), &dir6.display().to_string());
    let err6 = run_cancellable(&store, &Engine::new(5), &mut t6, &|_| {}, &Cancel::new()).unwrap_err();
    let msg6 = format!("{err6:#}");
    checks.push(Check {
        name: "假 Range 会拒绝并给逃生口".into(),
        ok: msg6.contains("jobs 1") || msg6.contains("假"),
        detail: crate::engine::short(msg6.lines().next().unwrap_or("")),
    });

    // ── 7. 429 限流：退避后仍下完 ───────────────────────────────────
    let flaky = Server::start(Mode::Flaky, SEG_SIZE, 4)?;
    let dir7 = home.join("out7");
    std::fs::create_dir_all(&dir7)?;
    let mut t7 = Task::new(&flaky.url("flaky.bin"), &dir7.display().to_string());
    let out7 = run_cancellable(&store, &Engine::new(3), &mut t7, &|_| {}, &Cancel::new());
    let want7 = crate::engine::sha256_hex(&payload(SEG_SIZE, 4));
    checks.push(Check {
        name: "429 之后还能下完".into(),
        ok: out7.is_ok() && t7.sha256 == want7,
        detail: match &out7 {
            Ok(()) => format!("被拒 {} 次后成功，sha 一致", flaky.stat.threes.load(Ordering::Relaxed)),
            Err(e) => format!("失败：{}", crate::engine::short(&format!("{e:#}"))),
        },
    });

    // ── 8. 服务器不认分段：走单连接也能成 ───────────────────────────
    let flat = Server::start(Mode::NoRange, 512 * 1024, 5)?;
    let dir8 = home.join("out8");
    std::fs::create_dir_all(&dir8)?;
    let mut t8 = Task::new(&flat.url("flat.bin"), &dir8.display().to_string());
    let out8 = run_cancellable(&store, &Engine::new(5), &mut t8, &|_| {}, &Cancel::new());
    checks.push(Check {
        name: "不认分段的源也能下".into(),
        ok: out8.is_ok(),
        detail: format!("{} 次请求", flat.requests()),
    });

    // ── 9. 两个任务同时写台账（WAL 不是摆设）────────────────────────
    let a = Server::start(Mode::Ok, 512 * 1024, 6)?;
    let b = Server::start(Mode::Ok, 512 * 1024, 7)?;
    let (s1, s2) = (store_handle(&home)?, store_handle(&home)?);
    let (ua, ub) = (
        a.url("a.bin"),
        b.url("b.bin"),
    );
    let (d1, d2) = (home.join("out9a"), home.join("out9b"));
    std::fs::create_dir_all(&d1)?;
    std::fs::create_dir_all(&d2)?;
    let h1 = std::thread::spawn(move || {
        let mut t = Task::new(&ua, &d1.display().to_string());
        run_cancellable(&s1, &Engine::new(3), &mut t, &|_| {}, &Cancel::new()).is_ok()
    });
    let h2 = std::thread::spawn(move || {
        let mut t = Task::new(&ub, &d2.display().to_string());
        run_cancellable(&s2, &Engine::new(3), &mut t, &|_| {}, &Cancel::new()).is_ok()
    });
    let both = h1.join().unwrap_or(false) && h2.join().unwrap_or(false);
    checks.push(Check {
        name: "两个任务同时写台账".into(),
        ok: both,
        detail: format!("台账现在 {} 条", store.list(100)?.len()),
    });

    // ── 10. 缓存块被改过：verify --deep 要抓得住 ─────────────────────
    // 内容寻址的意义就是「文件名 = 内容」，改一个字节名字就不对了。
    // 这条检查的是**检查器本身**：自检说干净，就真的干净。
    let blob = paths::blobs_dir().join(&want[..2]).join(&want);
    let good = std::fs::read(&blob)?;
    let mut bad = good.clone();
    bad[1024] ^= 0xFF;
    std::fs::write(&blob, &bad)?;
    let rep = crate::verify::run(&store, true, false)?;
    let caught = !rep.clean();
    let kinds: Vec<&str> = rep.problems.iter().map(|p| p.kind.label()).collect();
    std::fs::write(&blob, &good)?; // 改回去，别影响别的检查
    checks.push(Check {
        name: "缓存块被改过能发现".into(),
        ok: caught,
        detail: if kinds.is_empty() {
            "改了一个字节还说干净 —— 检查器有问题".to_string()
        } else {
            let head: Vec<&str> = kinds.iter().take(2).copied().collect();
            format!("报出 {} 个问题（{}…）", kinds.len(), head.join("、"))
        },
    });

    // ── 11. stats 会把台账算成人能看懂的数 ─────────────────────────────
    let st = crate::stats::collect(&store)?;
    checks.push(Check {
        name: "stats 算得出命中率".into(),
        ok: st.tasks > 0 && st.cached_runs >= 1 && st.done_bytes > 0,
        detail: format!(
            "{} 条 · 下过 {} · 秒传 {} 次（占 {:.0}%）",
            st.tasks,
            crate::engine::human(st.done_bytes),
            st.cached_runs,
            st.saved_ratio() * 100.0
        ),
    });

    let all_ok = checks.iter().all(|c| c.ok);
    let _ = std::fs::remove_dir_all(&home);
    Ok((all_ok, checks))
}

/// 分片目录里现在有什么（自检的说明文字用）
fn left_names() -> String {
    match std::fs::read_dir(paths::parts_dir()) {
        Ok(rd) => {
            let v: Vec<String> = rd
                .filter_map(|e| e.ok())
                .map(|e| {
                    let n = e.file_name().to_string_lossy().to_string();
                    let sz = e.metadata().map(|m| m.len()).unwrap_or(0);
                    format!("{n}({sz})")
                })
                .collect();
            if v.is_empty() {
                "空".into()
            } else {
                v.join(" ")
            }
        }
        Err(_) => "读不了".into(),
    }
}

fn store_handle(_home: &std::path::Path) -> Result<Store> {
    Store::open()
}
