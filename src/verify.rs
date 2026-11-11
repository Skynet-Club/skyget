//! 自检：把「我认为一致」变成「随时可验证」。
//!
//! 这个产品唯一不可接受的失败是**静默产出坏文件**。所以这里检查的不是「能不能下」，
//! 而是六条不变式有没有被打破：
//!
//! 1. 台账说 done ⇒ 落地文件存在（不存在就是撒谎）
//! 2. 台账说 done ⇒ 文件大小 == 记录的字节数
//! 3. 台账说 done ⇒ 文件的 sha256 == 台账里的 sha256（`--deep` 才算，要读全文件）
//! 4. 内容缓存里的块 ⇒ 文件名就是它内容的 sha256（内容寻址自证；`--deep` 才算）
//! 5. 落地文件与缓存块是同一个 inode（说明没占两份空间）—— 不同只是浪费，不算错，但要报
//! 6. 分片位图说「这些段下完了」⇒ 对应 .part 文件真的在、且长度对
//!
//! 默认只做结构检查（快，几毫秒）；`--deep` 才重算 hash（慢，等于把文件全读一遍）。
//! `--repair` 只做一件事：位图说下完了但 .part 不成立时，废弃那张位图 ——
//! 别的绝不自动动数据。校验发现坏文件就报出来，删不删由人决定。

use std::collections::HashSet;
use std::path::Path;

use anyhow::Result;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::model::{Task, STATE_DONE};
use crate::paths;
use crate::store::Store;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Kind {
    /// 台账说完成，文件不在
    Missing,
    /// 大小与台账不符
    SizeMismatch,
    /// 内容 hash 与台账不符（只在 --deep）
    HashMismatch,
    /// 缓存块的文件名不是内容 hash（只在 --deep）
    BlobName,
    /// 位图说完成，但 .part 不成立
    BitmapStale,
    /// 落地文件与缓存不是同一份（不是错，是浪费）
    NotLinked,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Missing => "缺文件",
            Kind::SizeMismatch => "大小不符",
            Kind::HashMismatch => "内容不符",
            Kind::BlobName => "缓存块名不对",
            Kind::BitmapStale => "位图作废",
            Kind::NotLinked => "未共享存储",
        }
    }

    pub fn fatal(self) -> bool {
        matches!(
            self,
            Kind::Missing | Kind::SizeMismatch | Kind::HashMismatch | Kind::BlobName
        )
    }
}

#[derive(Debug, Serialize)]
pub struct Problem {
    pub kind: Kind,
    pub id: String,
    pub path: String,
    pub detail: String,
}

#[derive(Debug, Serialize, Default)]
pub struct Report {
    pub tasks: usize,
    pub blobs: usize,
    pub deep: bool,
    pub problems: Vec<Problem>,
    pub orphans: Vec<String>,
    pub repaired: usize,
    /// 每条完成记录的校验强度：pinned / indexed / tofu-* / torrent / none
    pub trust: std::collections::BTreeMap<String, usize>,
}

impl Report {
    /// 有没有「静默坏文件」级别的问题。
    pub fn clean(&self) -> bool {
        !self.problems.iter().any(|p| p.kind.fatal())
    }

    pub fn fatal_count(&self) -> usize {
        self.problems.iter().filter(|p| p.kind.fatal()).count()
    }
}

pub fn run(store: &Store, deep: bool, repair: bool) -> Result<Report> {
    let mut rep = Report {
        deep,
        ..Default::default()
    };

    let tasks: Vec<Task> = store.list(100_000)?;
    let done: Vec<&Task> = tasks
        .iter()
        .filter(|t| {
            t.state == STATE_DONE
                && !t.dest.is_empty()
                // 上传的 dest 是本地源文件，不是“落地产物”；源文件被清掉
                // 不代表台账坏了，不能把它当 Missing 报。
                && t.direction != crate::model::DIR_UPLOAD
        })
        .collect();
    rep.tasks = done.len();

    let mut referenced: HashSet<String> = HashSet::new();
    for t in &done {
        if !t.sha256.is_empty() {
            referenced.insert(t.sha256.clone());
        }
        let k = if t.trust.is_empty() {
            "none".to_string()
        } else {
            t.trust.clone()
        };
        *rep.trust.entry(k).or_insert(0) += 1;
        check_task(t, deep, &mut rep);
    }

    // 缓存块：文件名必须就是内容 hash
    for (dir, file, _size) in walk_blobs() {
        rep.blobs += 1;
        let name = file.to_string_lossy().to_string();
        let path = dir.join(&file);
        if name.len() != 64 || !name.chars().all(|c| c.is_ascii_hexdigit()) {
            rep.problems.push(Problem {
                kind: Kind::BlobName,
                id: String::new(),
                path: path.display().to_string(),
                detail: "缓存块的文件名应该是 64 位 sha256".into(),
            });
            continue;
        }
        if !referenced.contains(&name) {
            rep.orphans.push(path.display().to_string());
        }
        if deep {
            match sha256_file(&path) {
                Ok(h) if h == name => {}
                Ok(h) => rep.problems.push(Problem {
                    kind: Kind::HashMismatch,
                    id: String::new(),
                    path: path.display().to_string(),
                    detail: format!("缓存块内容 hash 是 {}…，与文件名不符", &h[..12]),
                }),
                Err(e) => rep.problems.push(Problem {
                    kind: Kind::HashMismatch,
                    id: String::new(),
                    path: path.display().to_string(),
                    detail: format!("读不了：{e}"),
                }),
            }
        }
    }

    check_bitmaps(repair, &mut rep);

    Ok(rep)
}

fn check_task(t: &Task, deep: bool, rep: &mut Report) {
    let p = Path::new(&t.dest);
    let meta = match std::fs::metadata(p) {
        Ok(m) => m,
        Err(_) => {
            rep.problems.push(Problem {
                kind: Kind::Missing,
                id: t.id.clone(),
                path: t.dest.clone(),
                detail: "台账说完成了，盘上没有这个文件".into(),
            });
            return;
        }
    };

    if t.bytes_total > 0 && meta.len() != t.bytes_total {
        rep.problems.push(Problem {
            kind: Kind::SizeMismatch,
            id: t.id.clone(),
            path: t.dest.clone(),
            detail: format!("盘上 {} 字节，台账记 {} 字节", meta.len(), t.bytes_total),
        });
    }

    if deep && !t.sha256.is_empty() {
        match sha256_file(p) {
            Ok(h) if h.eq_ignore_ascii_case(&t.sha256) => {}
            Ok(h) => rep.problems.push(Problem {
                kind: Kind::HashMismatch,
                id: t.id.clone(),
                path: t.dest.clone(),
                detail: format!("盘上 hash 是 {}…，台账记 {}…", &h[..12], short(&t.sha256)),
            }),
            Err(e) => rep.problems.push(Problem {
                kind: Kind::HashMismatch,
                id: t.id.clone(),
                path: t.dest.clone(),
                detail: format!("读不了：{e}"),
            }),
        }
    }

    // 与缓存是否同一份：不是错，但值得说
    if !t.sha256.is_empty() {
        let blob = blob_path(&t.sha256);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if let Ok(bm) = std::fs::metadata(&blob) {
                let same = bm.ino() == meta.ino() && bm.dev() == meta.dev();
                if !same {
                    rep.problems.push(Problem {
                        kind: Kind::NotLinked,
                        id: t.id.clone(),
                        path: t.dest.clone(),
                        detail: "落地文件与缓存不是同一份（占了双份空间）".into(),
                    });
                }
            }
        }
    }
}

/// 位图：说下完了，.part 就得真的对得上。
fn check_bitmaps(repair: bool, rep: &mut Report) {
    let Ok(rd) = std::fs::read_dir(paths::parts_dir()) else {
        return;
    };
    for e in rd.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let Ok(txt) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(doc) = serde_json::from_str::<serde_json::Value>(&txt) else {
            continue;
        };
        let total = doc.get("total").and_then(|v| v.as_u64()).unwrap_or(0);
        let done_all = doc
            .get("done")
            .and_then(|v| v.as_array())
            .map(|a| !a.is_empty() && a.iter().all(|x| x.as_bool().unwrap_or(false)))
            .unwrap_or(false);
        if !done_all || total == 0 {
            continue;
        }
        // 全部段完成却还留着位图：落地那一步没走完，或者这是上一轮的旧账
        let part = path.with_extension("part");
        let part_ok = std::fs::metadata(&part).map(|m| m.len() == total).unwrap_or(false);
        if repair {
            // 位图是派生物，删掉只会让下次从头校验一遍，不会丢数据
            let _ = std::fs::remove_file(&path);
            rep.repaired += 1;
        } else {
            rep.problems.push(Problem {
                kind: Kind::BitmapStale,
                id: String::new(),
                path: path.display().to_string(),
                detail: if part_ok {
                    "所有段都标完成，但临时文件还在（上一轮没落地）".into()
                } else {
                    "所有段都标完成，但临时文件不在或不完整".into()
                },
            });
        }
    }
}

fn walk_blobs() -> Vec<(std::path::PathBuf, std::ffi::OsString, u64)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(paths::blobs_dir()) else {
        return out;
    };
    for e in rd.flatten() {
        let Ok(ty) = e.file_type() else { continue };
        if !ty.is_dir() {
            continue;
        }
        if let Ok(inner) = std::fs::read_dir(e.path()) {
            for f in inner.flatten() {
                let size = f.metadata().map(|m| m.len()).unwrap_or(0);
                // 这里要的是「桶目录」，不是文件自己的路径 ——
                // 拿文件路径再 join 文件名，会变成 文件/文件名，报 "Not a directory"
                out.push((e.path(), f.file_name(), size));
            }
        }
    }
    out
}

fn blob_path(sha: &str) -> std::path::PathBuf {
    paths::blobs_dir().join(&sha[..2.min(sha.len())]).join(sha)
}

fn short(s: &str) -> String {
    s.chars().take(12).collect()
}

pub fn sha256_file(p: &Path) -> Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(p)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_of_known_bytes() {
        // "abc" 的 sha256
        let dir = std::env::temp_dir().join("skyverify-test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("abc");
        std::fs::write(&p, b"abc").unwrap();
        assert_eq!(
            sha256_file(&p).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn fatal_kinds_are_the_silent_bad_file_ones() {
        assert!(Kind::Missing.fatal());
        assert!(Kind::HashMismatch.fatal());
        assert!(!Kind::NotLinked.fatal());
        assert!(!Kind::BitmapStale.fatal());
    }
}
