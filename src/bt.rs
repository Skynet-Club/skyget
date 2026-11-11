//! BitTorrent：magnet 与 .torrent。
//!
//! 用 librqbit（纯 Rust，跟 HTTP 引擎共用同一个落地层）。
//! 为什么不用 aria2 / qbittorrent-nox：那要带外部二进制，还得跟它的 RPC 对齐状态；
//! 这里要的是「同一个任务台账里的另一种下载方式」，不是再养一个守护进程。
//!
//! 这一版只做下载：v1 / hybrid 种子、magnet（DHT + tracker）、种子内全部文件。
//! v2-only 种子（BEP-52）librqbit 还不支持 —— 报错要说清楚是哪一种不支持，别让人干等。

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use librqbit::{AddTorrent, AddTorrentOptions, AddTorrentResponse, Session};

pub struct Outcome {
    /// 落地前的原始位置（单文件就是文件，多文件就是目录）
    pub raw_path: PathBuf,
    pub name: String,
    pub bytes: u64,
    /// 种子里所有文件的总大小（来自 stats，比目录 metadata 可靠）
    pub total: u64,
    pub single_file: bool,
}

/// 取一个种子的内容到 `work_dir`（下载过程中的临时落点）。
/// `cb(已下字节, 总字节)`：总字节在拿到 metadata 前是 0。
pub fn download(
    src: &str,
    work_dir: &Path,
    cancel: &crate::engine::Cancel,
    cb: &(dyn Fn(u64, u64) + Sync),
    on_name: &(dyn Fn(&str) + Sync),
    on_links: &(dyn Fn(u32, u32) + Sync),
) -> Result<Outcome> {
    std::fs::create_dir_all(work_dir)
        .with_context(|| format!("建不了 BT 临时目录：{}", work_dir.display()))?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("建 tokio 运行时失败")?;

    rt.block_on(async {
        let session = Session::new(work_dir.to_path_buf())
            .await
            .context("启动 BT 会话失败")?;

        // 三种来源都认：magnet、http(s) 上的 .torrent、本地 .torrent 文件
        let add = if Path::new(src).is_file() {
            AddTorrent::from_local_filename(src).context("读不了这个 .torrent 文件")?
        } else {
            AddTorrent::from_url(src)
        };
        let handle = match session
            .add_torrent(
                add,
                Some(AddTorrentOptions {
                    overwrite: true,
                    ..Default::default()
                }),
            )
            .await
        {
            Ok(AddTorrentResponse::Added(_, h)) => h,
            Ok(AddTorrentResponse::AlreadyManaged(_, h)) => h,
            Ok(_) => bail!("这个种子加不进会话（可能格式不被支持）"),
            Err(e) => {
                let msg = format!("{e:#}");
                if msg.to_ascii_lowercase().contains("v2") {
                    bail!("这是 BitTorrent v2 种子，当前引擎还不支持 v2-only（v1 与 hybrid 可以）。");
                }
                return Err(anyhow!("加种子失败：{msg}"));
            }
        };

        // 等 metadata：magnet 得先从 DHT/tracker 找到 peers 才拿得到文件清单
        let mut waited = 0u64;
        loop {
            if cancel.cancelled() {
                anyhow::bail!(crate::engine::Stopped);
            }
            let s = handle.stats();
            cb(s.progress_bytes, s.total_bytes);
            if s.total_bytes > 0 {
                break;
            }
            if waited >= 120_000 {
                bail!("两分钟还没拿到种子信息（可能没有做种者在线）；地址可能已经没人保种了。");
            }
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            waited += 400;
        }

        // with_metadata 会把闭包结果包成 Option（有些种子还没标准化完）
        // 闭包里就得转成 String：Cow 借的是 metadata 的生命周期，拿不出来
        let name = match handle.with_metadata(|m| m.info.name().map(|c| c.to_string())) {
            Ok(Some(s)) => s,
            _ => "torrent".to_string(),
        };
        // .torrent 的名字是种子文件名；真正的内容名要等 metadata
        on_name(&name);

        // 轮询进度直到下完。不用 wait_until_completed：这条循环顺便把进度报上去，
        // 界面才有波形可看。
        let mut last_total;
        loop {
            // 暂停：退出这条循环，会话随作用域销毁；已有分块留在工作区，重新加种子会接着下
            if cancel.cancelled() {
                anyhow::bail!(crate::engine::Stopped);
            }
            let s = handle.stats();
            cb(s.progress_bytes, s.total_bytes);
            // 线路：live = 已连上的对等端，seen = 发现到的总数（种子里这些都可能是线路）
            if let Some(live) = s.live.as_ref() {
                let ps = &live.snapshot.peer_stats;
                // 只算真正连上的：queued 上千个是 DHT 里的候选，报出来只会让人以为在下 965 路
                on_links(ps.live, ps.seen);
            }
            last_total = s.total_bytes;
            if s.finished {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        }

        let raw = work_dir.join(&name);
        let single = raw.is_file();
        let bytes = std::fs::metadata(&raw).map(|m| m.len()).unwrap_or(0);
        Ok(Outcome {
            raw_path: raw,
            name,
            bytes,
            total: last_total,
            single_file: single,
        })
    })
}

/// 多文件种子：整个目录搬过去。种子里的目录名已经过 librqbit 的清洗，
/// 这里再挡一次 `..`，因为落地路径最终由我们拼。
pub fn place_dir(raw: &Path, dest_dir: &Path, name: &str) -> Result<PathBuf> {
    let safe = sanitize_dir(name);
    let dest = unique_dir(dest_dir.join(&safe));
    std::fs::create_dir_all(&dest)?;
    copy_dir(raw, &dest)?;
    Ok(dest)
}

fn sanitize_dir(name: &str) -> String {
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
    let t = cleaned.trim().trim_matches('.').trim();
    if t.is_empty() || t == ".." {
        "torrent".to_string()
    } else {
        t.to_string()
    }
}

fn unique_dir(p: PathBuf) -> PathBuf {
    if !p.exists() {
        return p;
    }
    let parent = p.parent().map(|x| x.to_path_buf()).unwrap_or_default();
    let stem = p
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "torrent".into());
    for i in 1..1000 {
        let c = parent.join(format!("{stem} ({i})"));
        if !c.exists() {
            return c;
        }
    }
    p
}

fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    for entry in std::fs::read_dir(src).with_context(|| format!("读不了 {}", src.display()))? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            std::fs::create_dir_all(&to)?;
            copy_dir(&entry.path(), &to)?;
        } else if ty.is_file() {
            // 硬链接优先（同盘瞬时、不占第二份），跨设备再退化成拷贝
            if std::fs::hard_link(entry.path(), &to).is_err() {
                std::fs::copy(entry.path(), &to)?;
            }
        }
    }
    Ok(())
}

/// 这个地址是不是 BT。
pub fn is_bt(url: &str) -> bool {
    let l = url.to_ascii_lowercase();
    l.starts_with("magnet:") || l.ends_with(".torrent")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_bt_sources() {
        assert!(is_bt("magnet:?xt=urn:btih:abc"));
        assert!(is_bt("https://x.org/a/b.torrent"));
        assert!(!is_bt("https://x.org/a/b.zip"));
    }

    #[test]
    fn dir_names_are_sanitized() {
        assert_eq!(sanitize_dir("../../etc"), "etc");
        assert_eq!(sanitize_dir(".."), "torrent");
        assert_eq!(sanitize_dir("正常名字"), "正常名字");
    }
}
