//! 上传：把本地文件送到远端，和下载共用台账、事件、调度、策略。
//!
//! 这一版支持两种协议：
//! - `put`：一次 HTTP PUT；不可续传，适合小文件/内部接口。
//! - `tus` 1.0：创建/HEAD/PATCH，断点续传；适合大文件。
//!
//! 三个安全前提（见 ADR 0003）：
//! 1. 源文件必须在 `SKYGET_UPLOAD_ROOTS` 内，默认**禁止上传**；
//! 2. 目标地址过与下载相同的出网策略（私网/元数据默认拒绝）；
//! 3. 凭证只从 `credential_ref`（env:/file:）取，不落台账。

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};

use crate::engine::{Cancel, Engine, Event, Stopped};
use crate::model::{now_ms, Task, STATE_DONE, STATE_FAILED, STATE_INTERRUPTED, STATE_RUNNING};
use crate::store::Store;

/// 跑一个上传任务。成功返回 Ok，失败返回 Err（task 已写好终态）。
pub fn run(
    store: &Store,
    engine: &Engine,
    task: &mut Task,
    cb: &(dyn Fn(Event) + Sync),
    cancel: &Cancel,
) -> Result<()> {
    task.state = STATE_RUNNING.to_string();
    task.pid = std::process::id() as i64;
    task.updated_at = now_ms();
    // 行由 API 在派发前写入。这里只能用 save_if_exists：如果提交后立刻被取消/删除，
    // 全量 upsert 会把删掉的行重新插回来（幽灵任务），随后终态又写进这条幽灵。
    if !store.save_if_exists(task).unwrap_or(false) {
        return Err(anyhow::Error::new(Stopped));
    }

    let done = Arc::new(AtomicU64::new(0));
    let result = run_transfer(store, engine, task, cb, cancel, &done);

    match result {
        Ok(()) => {
            task.state = STATE_DONE.to_string();
            task.bytes_done = done.load(Ordering::Relaxed).max(task.bytes_done);
            task.error.clear();
            task.error_code.clear();
            task.retryable = false;
            task.retry_after_ms = 0;
            task.next_action.clear();
            task.updated_at = now_ms();
            if !store.save_if_exists(task).unwrap_or(false) {
                return Err(anyhow!("任务记录已被删除（可能在另一处取消）"));
            }
            cb(Event::Finished { cached: false });
            Ok(())
        }
        Err(e) if e.downcast_ref::<Stopped>().is_some() => {
            task.state = STATE_INTERRUPTED.to_string();
            task.bytes_done = done.load(Ordering::Relaxed).max(task.bytes_done);
            task.error = "已暂停，继续上传不会从头来".to_string();
            task.error_code = "job.paused".to_string();
            task.retryable = true;
            task.retry_after_ms = 0;
            task.next_action = "继续即可；tus 会从远端 offset 接着传。".to_string();
            task.updated_at = now_ms();
            let _ = store.save_if_exists(task);
            Err(e)
        }
        Err(e) => {
            let msg = format!("{e:#}");
            task.state = STATE_FAILED.to_string();
            task.bytes_done = done.load(Ordering::Relaxed).max(task.bytes_done);
            task.error = msg.clone();
            let info = crate::engine::classify_error(&e);
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

/// 预检 + 传输。错误由 `run` 统一写终态；这里只负责把字节送出去。
fn run_transfer(
    store: &Store,
    engine: &Engine,
    task: &mut Task,
    cb: &(dyn Fn(Event) + Sync),
    cancel: &Cancel,
    done: &Arc<AtomicU64>,
) -> Result<()> {
    let source =
        crate::security::check_upload_source(&task.dest, &engine.policy.upload_roots)?;
    task.dest = source.display().to_string();
    let meta = fs::symlink_metadata(&source)
        .with_context(|| format!("读不到源文件：{}", source.display()))?;
    let size = meta.len();
    // 下载和上传共用同一个日额度；超了就别再花时间算 hash/传字节。
    crate::quota::check(store, &task.source, size)?;

    cb(Event::Verifying);
    // 大文件哈希可能跑几十秒；必须能取消，否则 cancel 等 5 秒删行后，
    // 哈希结束时的写库会把任务复活。
    let sha = sha256_cancellable(&source, cancel)?;
    if !task.expected_sha256.is_empty() && !sha.eq_ignore_ascii_case(&task.expected_sha256) {
        bail!(
            "校验不符：期望 {}，实际 {}。文件没有上传。",
            short(&task.expected_sha256),
            short(&sha)
        );
    }
    task.sha256 = sha;
    task.trust = if task.expected_sha256.is_empty() {
        "local".to_string()
    } else {
        "pinned".to_string()
    };
    task.bytes_total = size;
    task.bytes_done = 0;
    task.updated_at = now_ms();
    if !store.save_if_exists(task).unwrap_or(false) {
        return Err(anyhow::Error::new(Stopped));
    }

    cb(Event::Planned {
        total: size,
        ranges: false,
        threads: 1,
    });

    // 目标地址同样过出网策略；上传不是绕过 SSRF 边界的后门。
    crate::security::check_url(&task.url, &engine.policy)?;
    let credential = credential(&task.credential_ref)?;
    if credential.is_some() && engine.policy.allowed_hosts.is_none() {
        bail!(
            "使用上传凭证前必须配置 SKYGET_ALLOWED_HOSTS（或 per-agent allowed_hosts），\
             否则 token 可能被发到任意公网主机。"
        );
    }

    let finished = Arc::new(AtomicBool::new(false));
    std::thread::scope(|s| {
        let hb = {
            let done = done.clone();
            let finished = finished.clone();
            let id = task.id.clone();
            s.spawn(move || {
                let mut last_done = 0u64;
                let mut last_save = Instant::now() - Duration::from_secs(2);
                while !finished.load(Ordering::Relaxed) {
                    let n = done.load(Ordering::Relaxed);
                    if n != last_done {
                        cb(Event::Progress { done: n, total: size });
                        last_done = n;
                    }
                    if last_save.elapsed() >= Duration::from_millis(900) {
                        let _ = store.save_progress(&id, n, size, 0, 0);
                        last_save = Instant::now();
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
            })
        };
        let r = match task.protocol.as_str() {
            "put" | "" => put(&source, size, &task.url, credential.as_deref(), done, cancel),
            "tus" => tus(
                task,
                &source,
                size,
                credential.as_deref(),
                done,
                cancel,
                &engine.policy,
            ),
            other => Err(anyhow!("不支持的上传协议：{other}")),
        };
        finished.store(true, Ordering::Relaxed);
        let _ = hb.join();
        r
    })
}

fn short(s: &str) -> String {
    s.chars().take(12).collect()
}

/// 分块算 sha256，每块看一次取消信号。上传的源文件可能几十 GB，
/// 不可取消的哈希会让「取消」在台账上成功、后台却继续跑很久。
fn sha256_cancellable(path: &Path, cancel: &Cancel) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut f = File::open(path)
        .with_context(|| format!("打不开源文件：{}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        if cancel.cancelled() {
            return Err(anyhow::Error::new(Stopped));
        }
        let n = f.read(&mut buf).context("读源文件失败")?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(format!("{:x}", h.finalize()))
}

/// 上传凭证只从引用读；解析和边界检查在 `credential::resolve`。
fn credential(reference: &str) -> Result<Option<String>> {
    crate::credential::resolve(reference, "SKYGET_UPLOAD_CREDENTIALS")
}

fn upload_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        // 大文件上传可能几小时；不能用下载客户端的 90 秒总超时。
        .timeout(Duration::from_secs(6 * 3600))
        // 上传不做重定向跟随：3xx 一律当失败，避免把文件体重放到别处。
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("构造上传客户端失败")
}

/// 单次上传请求的上限：按“最低可接受速度”算，而不是整条任务 6 小时。
/// 服务器接了连接却不读 body 时，阻塞式 reqwest 没法被 cancel 打断；
/// 至少让它在有限时间内失败，别把全局 worker 占到任务超时。
fn transfer_timeout(bytes: u64) -> Duration {
    // 32 KiB/s 下限；最短 2 分钟，最长 6 小时。
    let secs = bytes / (32 * 1024);
    Duration::from_secs(secs.clamp(120, 6 * 3600))
}

fn with_auth(
    rb: reqwest::blocking::RequestBuilder,
    credential: Option<&str>,
) -> reqwest::blocking::RequestBuilder {
    match credential {
        Some(t) if !t.is_empty() => rb.bearer_auth(t),
        _ => rb,
    }
}

/// 读取时计数并检查取消。reqwest 的 blocking Body 需要一个 'static reader，
/// 所以这里只带计数字和取消信号；cb 由外层心跳线程按 200ms 读计数。
struct CountReader {
    file: File,
    done: Arc<AtomicU64>,
    cancel: Cancel,
}

impl Read for CountReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.cancel.cancelled() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "upload cancelled",
            ));
        }
        let n = self.file.read(buf)?;
        self.done.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

fn put(
    source: &Path,
    size: u64,
    target: &str,
    credential: Option<&str>,
    done: &Arc<AtomicU64>,
    cancel: &Cancel,
) -> Result<()> {
    let file = File::open(source)
        .with_context(|| format!("打不开源文件：{}", source.display()))?;
    let reader = CountReader {
        file,
        done: done.clone(),
        cancel: cancel.clone(),
    };
    let body = reqwest::blocking::Body::sized(reader, size);
    let client = upload_client()?;
    let rb = client
        .put(target)
        .timeout(transfer_timeout(size))
        .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
        .header(reqwest::header::CONTENT_LENGTH, size.to_string())
        .body(body);
    let resp = with_auth(rb, credential).send().map_err(|e| {
        if cancel.cancelled() {
            anyhow::Error::new(Stopped)
        } else {
            anyhow!("上传请求失败：{e}")
        }
    })?;
    let status = resp.status();
    let text = resp.text().unwrap_or_default();
    if !status.is_success() {
        bail!("上传目标返回 HTTP {}：{}", status.as_u16(), text);
    }
    done.store(size, Ordering::Relaxed);
    Ok(())
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct TusState {
    url: String,
    location: String,
    offset: u64,
}

fn tus_state_path(task_id: &str) -> PathBuf {
    crate::paths::parts_dir().join(format!("tus-{task_id}.json"))
}

/// 取消/删除上传任务时清掉 tus sidecar（里面可能有 Location 凭证）。
pub fn discard_state(task_id: &str) {
    let _ = fs::remove_file(tus_state_path(task_id));
}

fn load_tus(path: &Path, url: &str) -> Option<TusState> {
    let text = fs::read_to_string(path).ok()?;
    let st: TusState = serde_json::from_str(&text).ok()?;
    (st.url == url).then_some(st)
}

fn save_tus(path: &Path, st: &TusState) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ok();
    }
    let mut f = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .with_context(|| format!("写不了 tus 状态：{}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = f.set_permissions(fs::Permissions::from_mode(0o600));
    }
    f.write_all(serde_json::to_string(st)?.as_bytes())?;
    f.sync_data().ok();
    Ok(())
}

/// 远端上传对象已经不存在（404/410）。调用方会清 sidecar 并重建一次。
#[derive(Debug)]
struct TusGone;

impl std::fmt::Display for TusGone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("远端 tus 上传已不存在（404/410）")
    }
}
impl std::error::Error for TusGone {}

/// Location 必须是合法、可出网、且与上传地址**同源**的 URL。
///
/// tus 服务器完全可以把 Location 指向任意地址。不校验的话，一个白名单内的
/// 上传目标就能把 daemon 变成 SSRF 跳板：后续 HEAD/PATCH 会带着 Bearer 凭证
/// 和文件内容发到 `169.254.169.254` 或攻击者域名。同源限制足够覆盖常规 tus
/// （相对路径或同一主机的绝对 URL）；跨域上传端目前不支持。
fn validate_location(
    policy: &crate::security::Policy,
    base_url: &str,
    location: &str,
) -> Result<()> {
    // 先查同源，再做出网检查：不同源根本不该为它解析 DNS，
    // 也保证跨域 Location 永远是明确的策略拒绝，而不是随机 DNS 错误。
    let base = url::Url::parse(base_url).context("上传地址不是合法 URL")?;
    let loc = url::Url::parse(location)
        .with_context(|| format!("Tus Location 不是合法 URL：{location}"))?;
    let same_origin = base.scheme() == loc.scheme()
        && base.host_str() == loc.host_str()
        && base.port_or_known_default() == loc.port_or_known_default();
    if !same_origin {
        return Err(crate::security::Denied {
            code: "policy.egress_denied",
            why: format!(
                "Tus Location 与上传地址不同源，拒绝把文件/凭证发过去：{location}"
            ),
        }
        .into());
    }
    crate::security::check_url(location, policy)?;
    Ok(())
}

/// tus 入口：远端对象丢失时重建**一次**。
///
/// 服务器重启、上传过期、临时文件被 GC 后，sidecar 里旧 Location 会永远 404。
/// 以前 HEAD 失败被 `unwrap_or(offset)` 吞掉，PATCH 再 404，任务永久卡死。
fn tus(
    task: &Task,
    source: &Path,
    size: u64,
    credential: Option<&str>,
    done: &Arc<AtomicU64>,
    cancel: &Cancel,
    policy: &crate::security::Policy,
) -> Result<()> {
    let state_path = tus_state_path(&task.id);
    match tus_once(task, source, size, credential, done, cancel, policy, &state_path) {
        Err(e) if e.downcast_ref::<TusGone>().is_some() => {
            let _ = fs::remove_file(&state_path);
            match tus_once(task, source, size, credential, done, cancel, policy, &state_path) {
                Err(e2) if e2.downcast_ref::<TusGone>().is_some() => {
                    bail!("远端 tus 上传已失效（404/410），重建后仍然不存在；检查目标存储")
                }
                other => other,
            }
        }
        other => other,
    }
}

#[allow(clippy::too_many_arguments)]
fn tus_once(
    task: &Task,
    source: &Path,
    size: u64,
    credential: Option<&str>,
    done: &Arc<AtomicU64>,
    cancel: &Cancel,
    policy: &crate::security::Policy,
    state_path: &Path,
) -> Result<()> {
    if cancel.cancelled() {
        return Err(anyhow::Error::new(Stopped));
    }
    let client = upload_client()?;
    // 旧 sidecar 的 Location 也要重新过策略/同源校验：它可能来自旧版本或本机被改过。
    let (location, mut offset) = match load_tus(state_path, &task.url) {
        Some(st) => {
            validate_location(policy, &task.url, &st.location)?;
            // 以远端 HEAD 的 offset 为准：进程被杀后本地状态可能落后。
            match tus_head(&client, &st.location, credential)? {
                Head::Offset(off) => (st.location, off),
                Head::Gone => return Err(anyhow::Error::new(TusGone)),
            }
        }
        None => {
            // 创建上传；Location 可能是相对路径。
            let rb = client
                .post(&task.url)
                .timeout(Duration::from_secs(120))
                .header("Tus-Resumable", "1.0.0")
                .header("Upload-Length", size.to_string());
            let resp = with_auth(rb, credential)
                .send()
                .map_err(|e| anyhow!("Tus 创建失败：{e}"))?;
            let status = resp.status();
            if !status.is_success() {
                let text = resp.text().unwrap_or_default();
                bail!("Tus 创建失败：HTTP {} {}", status.as_u16(), text);
            }
            let loc = resp
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| anyhow!("Tus 服务器没给 Location 头"))?
                .to_string();
            let base = url::Url::parse(&task.url).context("上传地址不是合法 URL")?;
            let location = base
                .join(&loc)
                .with_context(|| format!("Tus Location 解析失败：{loc}"))?
                .to_string();
            validate_location(policy, &task.url, &location)?;
            let st = TusState {
                url: task.url.clone(),
                location: location.clone(),
                offset: 0,
            };
            // 取消路径可能已经删了 sidecar；创建请求返回后先看取消，
            // 否则这里会把 sidecar 又写回来（API 的 discard 早就跑完了）。
            if cancel.cancelled() {
                return Err(anyhow::Error::new(Stopped));
            }
            save_tus(state_path, &st)?;
            match tus_head(&client, &location, credential)? {
                Head::Offset(off) => (location, off),
                Head::Gone => return Err(anyhow::Error::new(TusGone)),
            }
        }
    };

    if offset > size {
        bail!("远端 offset {offset} 大于文件大小 {size}");
    }
    done.store(offset, Ordering::Relaxed);

    let mut file = File::open(source)
        .with_context(|| format!("打不开源文件：{}", source.display()))?;
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; 8 << 20];
    while offset < size {
        if cancel.cancelled() {
            return Err(anyhow::Error::new(Stopped));
        }
        let want = ((size - offset) as usize).min(buf.len());
        let n = file
            .read(&mut buf[..want])
            .context("读源文件中途失败")?;
        if n == 0 {
            bail!("源文件提前结束（远端 offset {offset} / 总大小 {size}）");
        }
        let chunk = buf[..n].to_vec();
        let rb = client
            .patch(&location)
            .timeout(transfer_timeout(n as u64))
            .header("Tus-Resumable", "1.0.0")
            .header("Upload-Offset", offset.to_string())
            .header("Content-Type", "application/offset+octet-stream")
            .header("Content-Length", n.to_string())
            .body(chunk);
        let resp = with_auth(rb, credential)
            .send()
            .map_err(|e| {
                if cancel.cancelled() {
                    anyhow::Error::new(Stopped)
                } else {
                    anyhow!("Tus PATCH 失败：{e}")
                }
            })?;
        let status = resp.status();
        if matches!(status.as_u16(), 404 | 410) {
            return Err(anyhow::Error::new(TusGone));
        }
        if status.as_u16() != 204 {
            let text = resp.text().unwrap_or_default();
            bail!("Tus PATCH 返回 HTTP {}：{}", status.as_u16(), text);
        }
        let new_offset = resp
            .headers()
            .get("upload-offset")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(offset + n as u64);
        if new_offset != offset + n as u64 {
            bail!(
                "Tus 服务器偏移不对：发了 {} 字节，它说 offset={new_offset}",
                n
            );
        }
        offset = new_offset;
        done.store(offset, Ordering::Relaxed);
        // 取消后别再写 sidecar：取消路径刚清掉它，这里再写会把 sidecar 复活。
        if cancel.cancelled() {
            return Err(anyhow::Error::new(Stopped));
        }
        save_tus(
            state_path,
            &TusState {
                url: task.url.clone(),
                location: location.clone(),
                offset,
            },
        )?;
    }
    // 完成后再 HEAD 一次，确认远端真的收满了。
    match tus_head(&client, &location, credential)? {
        Head::Offset(off) if off == size => {}
        Head::Offset(off) => bail!("Tus 收尾校验失败：远端 offset 是 {off}，不是 {size}"),
        Head::Gone => return Err(anyhow::Error::new(TusGone)),
    }
    let _ = fs::remove_file(state_path);
    Ok(())
}

enum Head {
    Offset(u64),
    Gone,
}

fn tus_head(
    client: &reqwest::blocking::Client,
    location: &str,
    credential: Option<&str>,
) -> Result<Head> {
    let rb = client
        .head(location)
        .timeout(Duration::from_secs(60))
        .header("Tus-Resumable", "1.0.0");
    let resp = with_auth(rb, credential)
        .send()
        .map_err(|e| anyhow!("Tus HEAD 失败：{e}"))?;
    let status = resp.status();
    if matches!(status.as_u16(), 404 | 410) {
        return Ok(Head::Gone);
    }
    if !status.is_success() {
        let text = resp.text().unwrap_or_default();
        bail!("Tus HEAD 返回 HTTP {}：{}", status.as_u16(), text);
    }
    let off = resp
        .headers()
        .get("upload-offset")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .ok_or_else(|| anyhow!("Tus HEAD 没有合法的 Upload-Offset 头"))?;
    Ok(Head::Offset(off))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tus_location_must_be_same_origin_and_pass_egress_policy() {
        let mut policy = crate::security::Policy::default();
        policy.allow_private = true;
        let base = "http://127.0.0.1:9000/files";
        assert!(validate_location(&policy, base, "http://127.0.0.1:9000/files/abc").is_ok());

        // 换主机/换端口都要拒绝：Location 是服务器给的，不能当跳板。
        let e = validate_location(&policy, base, "http://127.0.0.1:9001/files/abc").unwrap_err();
        let d = e.downcast_ref::<crate::security::Denied>().unwrap();
        assert_eq!(d.code, "policy.egress_denied");
        let e = validate_location(&policy, base, "http://evil.example/upload").unwrap_err();
        assert_eq!(
            e.downcast_ref::<crate::security::Denied>().unwrap().code,
            "policy.egress_denied"
        );

        // 同源但私网/回环：仍然过不了禁止私网的出网策略。
        let strict = crate::security::Policy {
            allow_private: false,
            ..crate::security::Policy::default()
        };
        let e = validate_location(&strict, base, "http://127.0.0.1:9000/files/abc").unwrap_err();
        assert!(
            e.downcast_ref::<crate::security::Denied>().is_some(),
            "{e:#}"
        );
    }

    #[test]
    fn credentials_are_not_an_arbitrary_secret_reader() {
        // 默认白名单为空：任何 env: 引用都拒绝
        std::env::remove_var("SKYGET_UPLOAD_CREDENTIALS");
        let e = credential("env:AWS_SECRET_ACCESS_KEY").unwrap_err();
        assert_eq!(
            e.downcast_ref::<crate::security::Denied>().unwrap().code,
            "policy.credential_denied"
        );
        // 白名单内的变量可以读
        std::env::set_var("SKYGET_UPLOAD_CREDENTIALS", "SKYGET_TEST_UPLOAD_TOKEN");
        std::env::set_var("SKYGET_TEST_UPLOAD_TOKEN", "abc");
        assert_eq!(
            credential("env:SKYGET_TEST_UPLOAD_TOKEN")
                .unwrap()
                .as_deref(),
            Some("abc")
        );
        std::env::remove_var("SKYGET_TEST_UPLOAD_TOKEN");
        std::env::remove_var("SKYGET_UPLOAD_CREDENTIALS");

        // file: 不能指向 secrets 目录之外
        assert!(credential("file:/etc/passwd").is_err());
        assert!(credential("file:/home/skynet/.ssh/id_ed25519").is_err());
    }
}
