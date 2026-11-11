//! 跨进程分片锁：同一个地址的 `.part` / 位图只能有一个写者。
//!
//! 为什么要有它：`part_key(url)`（`engine.rs`）是**地址指纹**，不是任务 id ——
//! 这是断点续传能跨任务命中的原因，也意味着 CLI、守护进程、两个 agent 同时下
//! 同一个地址时会打开同一份 `.part` 和同一张位图。0.16.2 的 `running` 表只挡住了
//! 单进程内的重复派发，挡不住跨进程。两个写者一起写，取消/完成/内容变化时还会
//! 互相删对方的临时文件 —— 这不是理论风险，是「所有 agent 共用一台机器」后的第一个坑。
//!
//! 这里用 `flock(LOCK_EX | LOCK_NB)`：拿不到就明确说「另一个进程正在下」，
//! 调用方可以把这条当成可重试的结论，而不是产出一个谁也说不清的文件。
//!
//! 锁文件不删：unlink 和 flock 之间有经典的「新建 inode」竞态 —— A 打开旧 inode
//! 还没锁，B 删掉再新建一个锁上，两个都以为自己拿到了锁。锁文件只有几十字节，
//! 留着；进程崩溃时内核会自动释放 flock，下一次 acquire 会覆盖里面的 pid。

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// 另一个进程正拿着同一份分片。
///
/// 单独做成一个类型，而不是一句字符串：上层要据此给出
/// `retryable: true` + `retry_after_ms`，并让 agent 知道「不是内容坏了」。
#[derive(Debug)]
pub struct PartBusy {
    pub url: String,
    pub holder: String,
}

impl std::fmt::Display for PartBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "另一个进程正在下同一个地址（{}）。已下的分片不会被重复写；等它结束再试，或直接查缓存。",
            if self.holder.is_empty() {
                "没留下 pid"
            } else {
                &self.holder
            }
        )
    }
}

impl std::error::Error for PartBusy {}

/// 拿住期间有效；Drop（或进程退出）即释放。
#[derive(Debug)]
pub struct PartLock {
    _file: File,
    path: PathBuf,
}

impl PartLock {
    /// 给 `part`（如 `parts/abcd.part`）加锁，锁文件是 `parts/abcd.lock`。
    ///
    /// 锁的是锁文件，不是 `.part` 本身：`.part` 完成时会被 rename 进缓存，
    /// 锁必须活过那次 rename；而且各分片线程会反复 open/close `.part`。
    pub fn acquire(part: &Path, url: &str) -> Result<Self> {
        let path = part.with_extension("lock");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("建不了锁文件目录：{}", parent.display()))?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("打不开分片锁：{}", path.display()))?;

        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            // SAFETY: fd 来自上面刚打开的 File，调用期间 File 一直活着。
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if rc != 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::WouldBlock {
                    let mut who = String::new();
                    let _ = file.read_to_string(&mut who);
                    return Err(anyhow::Error::new(PartBusy {
                        url: url.to_string(),
                        holder: holder_line(&who),
                    }));
                }
                return Err(err).with_context(|| format!("锁不住分片：{}", path.display()));
            }
        }

        // 拿到锁之后把持有者写进去，方便下一个进程在报错里说清是谁。
        let _ = file.set_len(0);
        let _ = file.seek(SeekFrom::Start(0));
        let _ = writeln!(file, "pid {} · {}", std::process::id(), url);
        let _ = file.flush();

        Ok(Self {
            _file: file,
            path,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// 锁文件里的第一行（可能为空：旧版本/刚 create 还没来得及写）。
fn holder_line(raw: &str) -> String {
    raw.lines().next().unwrap_or("").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_part(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "skyget-lock-{}-{tag}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("x.part")
    }

    #[test]
    fn second_acquire_fails_until_first_drops() {
        let part = tmp_part("one");
        let a = PartLock::acquire(&part, "https://example.org/x").unwrap();
        let err = PartLock::acquire(&part, "https://example.org/x").unwrap_err();
        let busy = err
            .downcast_ref::<PartBusy>()
            .expect("第二个写者必须得到 PartBusy，而不是一句模糊的错误");
        assert!(busy.holder.contains("pid"), "锁文件里要留下持有者：{}", busy.holder);

        drop(a);
        let c = PartLock::acquire(&part, "https://example.org/x")
            .expect("前一个写者退出后必须能重新拿到锁");
        drop(c);
        let _ = std::fs::remove_dir_all(part.parent().unwrap());
    }
}
