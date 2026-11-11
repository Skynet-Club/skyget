//! HLS（.m3u8）与 DASH（.mpd）。
//!
//! 切片流没有「一个文件」可下，硬拼是自己给自己找 bug：变体播放列表、加密、
//! fMP4 初始化段、时间戳偏移 —— ffmpeg 全都处理好了。这里只负责三件事：
//! 估算总时长（好看进度）、把 ffmpeg 的进度翻成事件、收尾交给统一的落地层。
//!
//! 进度按**播放时长**报，不按字节 —— 切片流的字节数在最后一刻才知道，
//! 按字节画进度条就是在骗人。

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;

use anyhow::{bail, Context, Result};

/// 这个地址是不是切片流。
pub fn is_stream(url: &str) -> bool {
    let no_q = url.split(['?', '#']).next().unwrap_or(url).to_ascii_lowercase();
    no_q.ends_with(".m3u8") || no_q.ends_with(".mpd")
}

/// 落地文件名：`a/b/index.m3u8` → `index.mp4`。
pub fn output_name(url: &str) -> String {
    let no_q = url.split(['?', '#']).next().unwrap_or(url);
    let after = no_q.split("://").nth(1).unwrap_or(no_q);
    let last = after
        .splitn(2, '/')
        .nth(1)
        .unwrap_or("")
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("stream");
    let stem = last
        .rsplit_once('.')
        .map(|(s, _)| s)
        .filter(|s| !s.is_empty())
        .unwrap_or("stream");
    format!("{stem}.mp4")
}

fn ffmpeg_bin() -> String {
    std::env::var("SKYGET_FFMPEG").unwrap_or_else(|_| "ffmpeg".to_string())
}

/// 取流并合并到 `out`。`cb(已处理秒, 总秒)`：总秒为 0 表示估不出来。
pub fn fetch(
    url: &str,
    out: &Path,
    client: &reqwest::blocking::Client,
    policy: &crate::security::Policy,
    cancel: &crate::engine::Cancel,
    cb: &(dyn Fn(u64, u64) + Sync),
) -> Result<()> {
    fetch_with(url, out, client, policy, cancel, cb, "http,https,tls,tcp")
}

/// 同 `fetch`，但能指定 ffmpeg 输入协议白名单（本地清单要 `file`）。
fn fetch_with(
    url: &str,
    out: &Path,
    client: &reqwest::blocking::Client,
    policy: &crate::security::Policy,
    cancel: &crate::engine::Cancel,
    cb: &(dyn Fn(u64, u64) + Sync),
    whitelist: &str,
) -> Result<()> {
    let total = probe_secs(client, url, policy).unwrap_or(0);

    let mut cmd = Command::new(ffmpeg_bin());
    cmd.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-y",
        // 播放列表里的分段/子清单只允许走网络协议：恶意 m3u8 不能塞 file://
        // 把本机文件读进产物。这个选项只作用于输入，不影响写 mp4 的 file 输出。
        "-protocol_whitelist",
        whitelist,
        "-i",
        url,
        // 切片流原样拷贝：转码一次就是几十分钟 CPU，也没必要
        "-c",
        "copy",
        "-f",
        "mp4",
        out.to_str().context("输出路径不是合法 UTF-8")?,
        "-progress",
        "pipe:1",
    ])
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());

    let mut child = cmd.spawn().with_context(|| {
        format!(
            "调不起 ffmpeg（{}）。装一个，或用 SKYGET_FFMPEG 指到可执行文件。",
            ffmpeg_bin()
        )
    })?;

    // stderr 收着，失败时才有话可说；同时读走，避免管道写满把 ffmpeg 卡死
    let stderr = child.stderr.take().unwrap();
    let (etx, erx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut tail: Vec<String> = Vec::new();
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if !line.trim().is_empty() {
                tail.push(line);
                if tail.len() > 20 {
                    tail.remove(0);
                }
            }
        }
        let _ = etx.send(tail.join("\n"));
    });

    let stdout = child.stdout.take().unwrap();
    // out_time 是**输出流的时间戳**，不是「已经下了多久」：有些流的 PTS 起点
    // 就是十几小时（bipbop 这种测试流就这样），直接用会把进度画成 13:51:19。
    // 记下第一个非零值当零点，之后一律减掉它。
    let mut base: Option<u64> = None;
    let mut done_secs = 0u64;
    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        // 暂停：把 ffmpeg 收掉。切片流没有「续传」的概念，半截的 mp4 下次重下。
        if cancel.cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!(crate::engine::Stopped);
        }
        // 只认 out_time=HH:MM:SS.ffffff：`out_time_ms` 在 ffmpeg 里名不副实
        // （实际是微秒，历史 bug），照它换算会放大一千倍。
        let raw = line
            .trim()
            .strip_prefix("out_time=")
            .and_then(|v| parse_hms(v.trim()));
        if let Some(s) = raw {
            let b = *base.get_or_insert(s);
            let now = s.saturating_sub(b);
            if now > done_secs {
                done_secs = now;
                cb(done_secs, total);
            }
        }
    }

    let status = child.wait().context("等 ffmpeg 结束失败")?;
    if !status.success() {
        let detail = erx.recv_timeout(std::time::Duration::from_secs(2)).unwrap_or_default();
        if detail.to_ascii_lowercase().contains("encryption") {
            bail!("这个流是加密的（DRM），拿不到可播放的内容。");
        }
        bail!(
            "ffmpeg 收尾失败（退出码 {:?}）：{}",
            status.code(),
            tail_of(&detail)
        );
    }
    if !out.is_file() || std::fs::metadata(out).map(|m| m.len()).unwrap_or(0) == 0 {
        bail!("ffmpeg 说成功了，但没有产出可用的文件——这个流可能已经下线。");
    }
    // 收尾再报一次满进度，界面才知道结束了
    cb(total.max(done_secs), total);
    Ok(())
}

/// `01:23:45.678` → 5025 秒。
fn parse_hms(v: &str) -> Option<u64> {
    let mut secs = 0u64;
    for part in v.split(':') {
        let n: f64 = part.parse().ok()?;
        secs = secs * 60 + n as u64;
    }
    Some(secs)
}

fn tail_of(s: &str) -> String {
    let lines: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
    lines
        .iter()
        .rev()
        .take(3)
        .rev()
        .cloned()
        .collect::<Vec<_>>()
        .join("；")
}

/// 估算总时长：m3u8 把 #EXTINF 加起来；master 播放列表往里钻一层；mpd 读 ISO8601 时长。
fn probe_secs(
    client: &reqwest::blocking::Client,
    url: &str,
    policy: &crate::security::Policy,
) -> Option<u64> {
    let body = client.get(url).send().ok()?.text().ok()?;
    if body.len() > 4 << 20 {
        return None;
    }
    secs_from(url, &body, 0, client, policy)
}

fn secs_from(
    url: &str,
    body: &str,
    depth: usize,
    client: &reqwest::blocking::Client,
    policy: &crate::security::Policy,
) -> Option<u64> {
    if body.contains("<MPD") {
        return mpd_secs(body);
    }
    // master 播放列表：只往下钻一层，取第一个变体就够估个大概
    if body.contains("#EXT-X-STREAM-INF") {
        if depth >= 1 {
            return None;
        }
        let mut lines = body.lines();
        while let Some(l) = lines.next() {
            if l.starts_with("#EXT-X-STREAM-INF") {
                let child = lines
                    .find(|x| !x.trim_start().starts_with('#') && !x.trim().is_empty())?;
                let child_url = resolve(url, child.trim());
                // 子播放列表也必须过同一套出网策略：顶层 m3u8 是公网，
                // 不代表它指向的变体可以是 169.254.169.254 / 127.0.0.1。
                crate::security::check_url(&child_url, policy).ok()?;
                // 用同一个 client：超时和逐跳重定向检查都继承下来。
                let text = client.get(&child_url).send().ok()?.text().ok()?;
                return secs_from(&child_url, &text, depth + 1, client, policy);
            }
        }
        return None;
    }
    let mut total = 0f64;
    for l in body.lines() {
        if let Some(v) = l.trim().strip_prefix("#EXTINF:") {
            let n = v.split(',').next()?.trim();
            if let Ok(sec) = n.parse::<f64>() {
                total += sec;
            }
        }
    }
    if total > 0.0 {
        Some(total as u64)
    } else {
        None
    }
}

fn mpd_secs(body: &str) -> Option<u64> {
    let i = body.find("mediaPresentationDuration=\"")?;
    let rest = &body[i + "mediaPresentationDuration=\"".len()..];
    let end = rest.find('"')?;
    let dur = &rest[..end];
    let mut total = 0f64;
    let mut num = String::new();
    for c in dur.chars() {
        match c {
            'P' => {}
            'T' => {}
            'H' | 'M' | 'S' => {
                let v: f64 = num.parse().ok()?;
                total += match c {
                    'H' => v * 3600.0,
                    'M' => v * 60.0,
                    _ => v,
                };
                num.clear();
            }
            d if d.is_ascii_digit() || d == '.' => num.push(d),
            _ => {}
        }
    }
    if total > 0.0 {
        Some(total as u64)
    } else {
        None
    }
}

fn resolve(base: &str, rel: &str) -> String {
    if rel.starts_with("http://") || rel.starts_with("https://") {
        return rel.to_string();
    }
    let after = base.split("://").nth(1).unwrap_or(base);
    let host_end = after.find('/').map(|i| i + base.len() - after.len()).unwrap_or(base.len());
    let root = &base[..host_end];
    if rel.starts_with('/') {
        format!("{root}{rel}")
    } else {
        let dir = match base.rfind('/') {
            Some(i) if i > base.len() - after.len() => &base[..=i],
            _ => &format!("{root}/")[..],
        };
        format!("{dir}{rel}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_streams() {
        assert!(is_stream("https://a/b/index.m3u8"));
        assert!(is_stream("https://a/b/manifest.mpd?token=1"));
        assert!(!is_stream("https://a/b/video.mp4"));
    }

    #[test]
    fn output_names() {
        assert_eq!(output_name("https://a/b/index.m3u8"), "index.mp4");
        assert_eq!(output_name("https://a/x.m3u8?t=2"), "x.mp4");
        assert_eq!(output_name("https://a/"), "stream.mp4");
    }

    #[test]
    fn extinf_sums() {
        let p = "#EXTM3U\n#EXTINF:4.5,\nb1.ts\n#EXTINF:5.5,\nb2.ts\n";
        let client = reqwest::blocking::Client::new();
        let policy = crate::security::Policy::default();
        assert_eq!(
            secs_from("https://a/i.m3u8", p, 0, &client, &policy),
            Some(10)
        );
    }

    #[test]
    fn mpd_duration() {
        let d = "<MPD mediaPresentationDuration=\"PT1H2M3.5S\"><Period/></MPD>";
        assert_eq!(mpd_secs(d), Some(3723));
    }

    #[test]
    fn hms_parsing() {
        assert_eq!(parse_hms("00:00:01.500000"), Some(1));
        assert_eq!(parse_hms("01:23:45.000000"), Some(5025));
        assert_eq!(parse_hms("12:00.0"), Some(720));
    }

    #[test]
    fn relative_urls_resolve() {
        assert_eq!(
            resolve("https://a/b/index.m3u8", "v1/prog.m3u8"),
            "https://a/b/v1/prog.m3u8"
        );
        assert_eq!(
            resolve("https://a/b/index.m3u8", "/v1/prog.m3u8"),
            "https://a/v1/prog.m3u8"
        );
    }
}

// ── 并行预取 ────────────────────────────────────────────────────────────
//
// 实测（46MB / 30 段 / 每连接限 2MiB/s）：ffmpeg 直连 12.04s，8 路并发取同样
// 分片 3.07s —— 3.91 倍。ffmpeg 自己会预取（约 2 条连接），所以就别重写 HLS
// 解析了：EXT-X-MAP、密钥、时间戳仍交给它，我们只负责把字节搬快。
//
// 只处理最普通的点播明文流；任何不认识的形态都返回 `Ok(false)` 让调用方回退。

struct FastPlan {
    segments: Vec<String>,
    map: Option<String>,
}

fn plan_fast(base: &str, text: &str) -> Option<FastPlan> {
    if text.contains("#EXT-X-STREAM-INF")
        || text.contains("#EXT-X-BYTERANGE")
        || !text.contains("#EXT-X-ENDLIST")
    {
        return None;
    }
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix("#EXT-X-KEY:") {
            if !rest.to_ascii_uppercase().contains("METHOD=NONE") {
                return None;
            }
        }
    }
    let base_url = url::Url::parse(base).ok()?;
    let mut segments = Vec::new();
    let mut map = None;
    for line in text.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("#EXT-X-MAP:") {
            let u = attr_value(rest, "URI")?;
            map = Some(base_url.join(&u).ok()?.to_string());
            continue;
        }
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        segments.push(base_url.join(t).ok()?.to_string());
    }
    if segments.is_empty() {
        return None;
    }
    Some(FastPlan { segments, map })
}

/// 从 `KEY="value",KEY2="value2"` 里取一个属性。
fn attr_value(attrs: &str, key: &str) -> Option<String> {
    for part in attrs.split(',') {
        if let Some((k, v)) = part.split_once('=') {
            if k.trim().eq_ignore_ascii_case(key) {
                return Some(v.trim().trim_matches('"').to_string());
            }
        }
    }
    None
}

/// 临时目录，离开作用域就删（成功失败都删）。
struct DirGuard(std::path::PathBuf);
impl Drop for DirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 并行取分片 → 写成本地清单 → 让 ffmpeg 在本地清单上 remux。
///
/// `Ok(false)` 表示这条流不适用（调用方必须回退到直连），不是失败。
#[allow(clippy::too_many_arguments)]
pub fn fetch_parallel(
    url: &str,
    out: &Path,
    work: &Path,
    client: &reqwest::blocking::Client,
    policy: &crate::security::Policy,
    cancel: &crate::engine::Cancel,
    cb: &(dyn Fn(u64, u64) + Sync),
    jobs: usize,
) -> Result<bool> {
    let resp = client.get(url).send().context("取播放列表失败")?;
    if !resp.status().is_success() {
        return Ok(false);
    }
    let text = resp.text().context("读播放列表失败")?;
    if text.len() > 4 << 20 {
        return Ok(false);
    }
    let Some(plan) = plan_fast(url, &text) else {
        return Ok(false);
    };
    // 分片地址是从清单里拼出来的，逐个过一遍策略（清单本身前面已经查过）
    for s in plan.segments.iter().chain(plan.map.iter()) {
        if crate::security::check_url(s, policy).is_err() {
            return Ok(false);
        }
    }

    let total_secs = secs_from(url, &text, 0, client, policy);
    let _ = std::fs::remove_dir_all(work);
    std::fs::create_dir_all(work).context("建不了临时目录")?;
    let _guard = DirGuard(work.to_path_buf());

    let n = plan.segments.len();
    let dest_for = |i: usize| work.join(format!("seg-{i:05}.ts"));
    let queue = std::sync::Mutex::new((0..n).collect::<std::collections::VecDeque<_>>());
    let done_n = std::sync::atomic::AtomicUsize::new(0);
    let errors: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

    std::thread::scope(|sc| {
        for _ in 0..jobs.max(1) {
            sc.spawn(|| loop {
                if cancel.cancelled() {
                    return;
                }
                let idx = match queue.lock().unwrap().pop_front() {
                    Some(i) => i,
                    None => return,
                };
                let mut last = String::new();
                let mut ok = false;
                for _ in 0..3 {
                    match client.get(&plan.segments[idx]).send() {
                        Ok(r) if r.status().is_success() => match r.bytes() {
                            Ok(b) if !b.is_empty() => {
                                if std::fs::write(dest_for(idx), &b).is_ok() {
                                    ok = true;
                                    break;
                                }
                                last = "写临时文件失败".to_string();
                            }
                            _ => last = "分片是空的".to_string(),
                        },
                        Ok(r) => last = format!("HTTP {}", r.status()),
                        Err(e) => last = format!("{e}"),
                    }
                }
                if ok {
                    let d = done_n.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    if let Some(total) = total_secs {
                        cb(total * d as u64 / n as u64, total);
                    }
                } else {
                    errors.lock().unwrap().push(format!("第 {} 段：{last}", idx + 1));
                }
            });
        }
    });

    if cancel.cancelled() {
        anyhow::bail!(crate::engine::Stopped);
    }
    let failed = errors.lock().unwrap().len();
    if failed > 0 {
        bail!("分片没取全（{failed} / {n} 段失败）");
    }

    // fMP4 的初始化段也要取，否则本地清单解不开
    if let Some(map_url) = plan.map.as_deref() {
        let body = client
            .get(map_url)
            .send()
            .and_then(|r| r.error_for_status())
            .and_then(|r| r.bytes())
            .context("取初始化段失败")?;
        std::fs::write(work.join("init.mp4"), &body).context("写初始化段失败")?;
    }

    // 重写清单：分片换成本地名，其余标签原样保留（EXT-X-MAP / 时长 / ENDLIST）
    let mut local = String::with_capacity(text.len() + 64);
    let mut i = 0usize;
    for line in text.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("#EXT-X-MAP:") {
            let orig = attr_value(rest, "URI").unwrap_or_default();
            local.push_str(&format!("#EXT-X-MAP:{}\n", rest.replace(&orig, "init.mp4")));
            continue;
        }
        if !t.is_empty() && !t.starts_with('#') {
            local.push_str(&format!("seg-{i:05}.ts\n"));
            i += 1;
            continue;
        }
        local.push_str(line);
        local.push('\n');
    }
    let local_playlist = work.join("local.m3u8");
    std::fs::write(&local_playlist, &local).context("写本地清单失败")?;

    // 本地清单里的分片都是文件，白名单只要 file
    fetch_with(
        local_playlist.to_str().context("临时路径不是合法 UTF-8")?,
        out,
        client,
        policy,
        cancel,
        cb,
        "file",
    )?;
    Ok(true)
}
