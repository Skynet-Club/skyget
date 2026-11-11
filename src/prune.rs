//! 清缓存：只删「没人在用、而且很久没碰过」的东西。
//!
//! 这不是"清理工具"那种一键清空 —— 这个产品的卖点就是内容只存一份、下过的再来就是秒传。
//! 所以清扫的门槛必须写死在代码里，而不是交给用户勾选：
//!
//! 1. **还有落地文件链着它**（nlink > 1）→ 不删。删了不破坏用户的文件，但会丢掉秒传，
//!    下次同一个地址又要走一遍网络。
//! 2. **台账里有没跑完的任务**（排队 / 下载中 / 中断）→ 不删。那是活的。
//! 3. **比门槛年轻** → 不删。默认 30 天。
//!
//! 未完成的分片（`parts/`）另算：任务已经不在跑、而且分片 30 天没动，才清。

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};

use crate::model::Task;
use crate::paths;
use crate::store::Store;

/// 一块可以清掉的内容。
#[derive(Debug, Clone)]
pub struct Blob {
    pub path: PathBuf,
    pub sha: String,
    pub bytes: u64,
    pub modified: SystemTime,
}

/// 一个可以清掉的未完成分片（`.part` + `.json` 算一组）。
#[derive(Debug, Clone)]
pub struct Part {
    pub files: Vec<PathBuf>,
    pub bytes: u64,
}

/// 清扫计划：能删什么、为什么另有东西没删。
#[derive(Debug, Default)]
pub struct Plan {
    pub blobs: Vec<Blob>,
    pub parts: Vec<Part>,
    /// 当前缓存总量（含被 pin/链接/年龄保护、不会被清的那些）
    pub total_bytes: u64,
    /// 还有落地文件链着，跳过
    pub kept_linked: usize,
    /// 台账里有活任务，跳过
    pub kept_active: usize,
    /// 被 pin 钉住（agent 说「我在用」），跳过
    pub kept_pinned: usize,
    /// 太新，跳过
    pub kept_young: usize,
    /// 30 天没动但任务还在排队的，跳过的分片数
    pub parts_kept_active: usize,
}

impl Plan {
    pub fn blob_bytes(&self) -> u64 {
        self.blobs.iter().map(|b| b.bytes).sum()
    }

    pub fn part_bytes(&self) -> u64 {
        self.parts.iter().map(|p| p.bytes).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.blobs.is_empty() && self.parts.is_empty()
    }
}

/// `30d` / `12h` / `90m` / `45s` / `7`（纯数字按天）。
pub fn parse_age(s: &str) -> Result<Duration> {
    let t = s.trim();
    if t.is_empty() {
        bail!("时间写法看不懂：给 30d / 12h / 90m 这样的");
    }
    let (num, unit) = match t.char_indices().last() {
        Some((i, c)) if c.is_ascii_alphabetic() => (&t[..i], Some(c)),
        _ => (t, None),
    };
    let n: u64 = num
        .parse()
        .with_context(|| format!("时间写法看不懂：{t}（要 30d / 12h 这种）"))?;
    let secs = match unit.map(|c| c.to_ascii_lowercase()) {
        None | Some('d') => 86_400,
        Some('h') => 3_600,
        Some('m') => 60,
        Some('s') => 1,
        Some(other) => bail!("时间单位只认 d / h / m / s，不认 {other}"),
    };
    Ok(Duration::from_secs(n.saturating_mul(secs)))
}

/// 列出清扫计划（不动手）。
pub fn plan(store: &Store, older_than: Duration) -> Result<Plan> {
    let now = SystemTime::now();
    let mut out = Plan::default();

    // 台账里"活的"任务：它们的 sha 不许动
    let active: Vec<Task> = store.active()?;
    let active_shas: Vec<&str> = active
        .iter()
        .map(|t| t.sha256.as_str())
        .filter(|s| !s.is_empty())
        .collect();
    // agent 显式 pin 的内容：不管多老、有没有落地文件，都不清。
    let pinned = store.pinned_shas()?;

    let root = paths::blobs_dir();
    for dir in read_dir_sorted(&root)? {
        if !dir.is_dir() {
            continue;
        }
        for path in read_dir_sorted(&dir)? {
            let meta = match fs::metadata(&path) {
                Ok(m) => m,
                Err(_) => continue, // 刚被别人删了
            };
            if !meta.is_file() {
                continue;
            }
            let sha = path
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            out.total_bytes += meta.len();

            if meta.nlink() > 1 {
                out.kept_linked += 1;
                continue;
            }
            if active_shas.contains(&sha.as_str()) {
                out.kept_active += 1;
                continue;
            }
            if pinned.contains(&sha) {
                out.kept_pinned += 1;
                continue;
            }
            if !older_than_secs(&meta, now, older_than) {
                out.kept_young += 1;
                continue;
            }
            out.blobs.push(Blob {
                path,
                sha,
                bytes: meta.len(),
                modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            });
        }
    }

    // 分片：任务不在跑、而且文件 30 天没动，才清
    let active_ids: Vec<&str> = active.iter().map(|t| t.id.as_str()).collect();
    let parts_dir = paths::parts_dir();
    let mut seen: Vec<String> = Vec::new();
    for path in read_dir_sorted(&parts_dir)? {
        let meta = match fs::metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !meta.is_file() {
            continue;
        }
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let id = name.split('.').next().unwrap_or("").to_string();
        if seen.iter().any(|s| *s == id) {
            continue;
        }
        if active_ids.iter().any(|a| *a == id) {
            out.parts_kept_active += 1;
            seen.push(id);
            continue;
        }
        if !older_than_secs(&meta, now, older_than) {
            continue;
        }
        // 同一 id 的 .part 和 .json 一起清
        let group: Vec<PathBuf> = read_dir_sorted(&parts_dir)?
            .into_iter()
            .filter(|p| {
                p.file_name()
                    .map(|s| s.to_string_lossy().starts_with(&format!("{id}.")))
                    .unwrap_or(false)
            })
            .collect();
        let bytes = group
            .iter()
            .filter_map(|p| fs::metadata(p).ok().map(|m| m.len()))
            .sum();
        seen.push(id);
        out.parts.push(Part { files: group, bytes });
    }

    Ok(out)
}

/// 在原有年龄/保护门槛之上再按总量预算挑：从最老的开始删，直到
/// `total_bytes - 已选 <= max_bytes`。受保护/年轻的块不会被选。
pub fn cap_to(mut plan: Plan, max_bytes: u64) -> Plan {
    if plan.total_bytes <= max_bytes {
        plan.blobs.clear();
        return plan;
    }
    plan.blobs.sort_by_key(|b| b.modified);
    let mut total = plan.total_bytes;
    let mut selected = Vec::new();
    for b in plan.blobs.drain(..) {
        if total <= max_bytes {
            break;
        }
        total -= b.bytes;
        selected.push(b);
    }
    plan.blobs = selected;
    plan
}

/// 缓存现在占多少字节（真读盘，不查索引：缓存可能被外部改过）。
pub fn cache_bytes() -> u64 {
    let mut total = 0u64;
    let Ok(dirs) = read_dir_sorted(&paths::blobs_dir()) else {
        return 0;
    };
    for dir in dirs {
        if !dir.is_dir() {
            continue;
        }
        if let Ok(files) = read_dir_sorted(&dir) {
            for f in files {
                if let Ok(m) = fs::metadata(&f) {
                    if m.is_file() {
                        total += m.len();
                    }
                }
            }
        }
    }
    total
}

/// 一次水位线清扫的结果。
#[derive(Debug, Default, Clone, Copy)]
pub struct Evicted {
    pub blobs: usize,
    pub bytes: u64,
    /// 清完之后缓存占多少
    pub total_after: u64,
}

/// 缓存水位线：超过 `max_bytes` 就从最老的开始清到 `low_water` 以下。
///
/// 这是 daemon 的后台行为，和 `skyget prune` 共用同一套门槛（pin / 落地链接
/// / 活任务 / 最小年龄），只是没有人为它按一下回车。
///
/// 为什么要有最小年龄：一台机刚下完 20GB 模型，磁盘本来就紧，
/// 立刻把它当「没人要的旧块」清掉，等于把人刚下的东西删了 —— 那是破坏，不是清理。
/// 超过水位线、但所有块都比 `min_age` 年轻，就什么都不删（并如实返回 0）。
pub fn enforce_budget(
    store: &Store,
    max_bytes: u64,
    low_water: u64,
    min_age: Duration,
) -> Result<Evicted> {
    let plan = plan(store, min_age)?;
    let total = plan.total_bytes;
    if total <= max_bytes {
        return Ok(Evicted {
            blobs: 0,
            bytes: 0,
            total_after: total,
        });
    }
    let plan = cap_to(plan, low_water.min(max_bytes));
    let (blobs, bytes) = apply(&plan)?;
    Ok(Evicted {
        blobs,
        bytes,
        total_after: total.saturating_sub(bytes),
    })
}

/// 真的删。返回（删了几块/组，省了多少字节）。
pub fn apply(plan: &Plan) -> Result<(usize, u64)> {
    let mut n = 0;
    let mut bytes = 0;
    for b in &plan.blobs {
        match fs::remove_file(&b.path) {
            Ok(()) => {
                n += 1;
                bytes += b.bytes;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("删不掉 {}", b.path.display())),
        }
    }
    for p in &plan.parts {
        for f in &p.files {
            match fs::remove_file(f) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).with_context(|| format!("删不掉 {}", f.display())),
            }
        }
        n += 1;
        bytes += p.bytes;
    }
    Ok((n, bytes))
}

/// 这是不是「很久没碰过」。
///
/// 判据取 mtime 和 atime 里**较新**的那个：秒传命中只会读、不改内容，
/// 只看 mtime 的话，一个天天在用的缓存块和一个 31 天没人理的块看起来一样老，
/// 于是「常用的被清掉、不常用的留着」——正好反了。
/// （引擎在命中缓存时也会顺手 touch 一下 mtime，双保险：有的挂载选项不更新 atime。）
fn older_than_secs(meta: &fs::Metadata, now: SystemTime, older_than: Duration) -> bool {
    let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let accessed = meta.accessed().unwrap_or(SystemTime::UNIX_EPOCH);
    let touched = modified.max(accessed);
    match now.duration_since(touched) {
        Ok(age) => age >= older_than,
        Err(_) => false, // 未来时间（时钟被改过）：当它是新的，别删
    }
}

fn read_dir_sorted(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut v: Vec<PathBuf> = match fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok().map(|e| e.path())).collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("读不了 {}", dir.display())),
    };
    v.sort();
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{File, FileTimes};

    /// SKYGET_HOME 是进程级全局变量：这几个用例必须一个一个跑，否则会互相指到别的目录。
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn fake_home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("skyfetch-prune-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("blobs/ab")).unwrap();
        fs::create_dir_all(dir.join("parts")).unwrap();
        std::env::set_var("SKYGET_HOME", &dir);
        dir
    }

    /// 造一个内容块；`days_old` 天前改的
    fn blob(home: &Path, sha: &str, bytes: usize, days_old: u64) -> PathBuf {
        let p = home.join("blobs").join(&sha[..2]).join(sha);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, vec![7u8; bytes]).unwrap();
        if days_old > 0 {
            // 改和读都拨到过去：一个 40 天前下的、之后没人碰过的块
            let t = SystemTime::now() - Duration::from_secs(days_old * 86_400);
            let f = File::options().write(true).open(&p).unwrap();
            f.set_times(FileTimes::new().set_modified(t).set_accessed(t))
                .unwrap();
        }
        p
    }

    #[test]
    fn age_parsing() {
        assert_eq!(parse_age("30d").unwrap().as_secs(), 30 * 86_400);
        assert_eq!(parse_age("12h").unwrap().as_secs(), 12 * 3_600);
        assert_eq!(parse_age("90m").unwrap().as_secs(), 90 * 60);
        assert_eq!(parse_age("7").unwrap().as_secs(), 7 * 86_400); // 纯数字按天
        assert!(parse_age("30天").is_err());
        assert!(parse_age("").is_err());
        assert!(parse_age("abc").is_err());
    }

    #[test]
    fn old_unused_blob_is_a_candidate() {
        let _g = serial();
        let home = fake_home("old");
        let store = Store::open().unwrap();
        let p = blob(&home, &"a1".repeat(32), 100, 40);
        let plan = plan(&store, parse_age("30d").unwrap()).unwrap();
        assert_eq!(plan.blobs.len(), 1);
        assert_eq!(plan.blobs[0].path, p);
        assert_eq!(plan.blob_bytes(), 100);
    }

    #[test]
    fn recently_used_blob_is_kept_even_if_old() {
        let _g = serial();
        let home = fake_home("used");
        let store = Store::open().unwrap();
        let sha = "f6".repeat(32);
        let p = blob(&home, &sha, 64, 90); // 90 天没改过
        // 但昨天刚被用过（读一次 → atime 变新）
        let y = SystemTime::now() - Duration::from_secs(86_400);
        let f = File::options().write(true).open(&p).unwrap();
        f.set_times(FileTimes::new().set_accessed(y)).unwrap();
        drop(f);

        let plan = plan(&store, parse_age("30d").unwrap()).unwrap();
        assert!(
            plan.blobs.is_empty(),
            "90 天没改、但昨天用过 —— 这是常用块，不能清"
        );
    }

    #[test]
    fn young_blob_is_kept() {
        let _g = serial();
        let home = fake_home("young");
        let store = Store::open().unwrap();
        blob(&home, &"b2".repeat(32), 10, 0);
        let plan = plan(&store, parse_age("30d").unwrap()).unwrap();
        assert!(plan.blobs.is_empty());
        assert_eq!(plan.kept_young, 1);
    }

    #[test]
    fn max_bytes_budget_deletes_oldest_first() {
        let _g = serial();
        let home = fake_home("budget");
        let oldest = "aa".repeat(32);
        let middle = "bb".repeat(32);
        let newest = "cc".repeat(32);
        blob(&home, &oldest, 1000, 40);
        blob(&home, &middle, 1000, 30);
        blob(&home, &newest, 1000, 20);
        let store = Store::open().unwrap();
        let p = plan(&store, Duration::from_secs(10 * 86_400)).unwrap();
        assert_eq!(p.total_bytes, 3000);
        let p = cap_to(p, 2000);
        assert_eq!(p.blobs.len(), 1, "3000 → 2000 只该删一块：{p:?}");
        assert_eq!(p.blobs[0].sha, oldest);
        assert!(p.total_bytes - p.blob_bytes() <= 2000);
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn pinned_blob_is_kept_even_if_old_and_unlinked() {
        let _g = serial();
        let home = fake_home("pinned");
        let store = Store::open().unwrap();
        let sha = "d4".repeat(32);
        blob(&home, &sha, 10, 90);
        store.pin(&sha, "agent:claude", 0, "model in use").unwrap();
        let plan = plan(&store, parse_age("30d").unwrap()).unwrap();
        assert!(plan.blobs.is_empty(), "pin 住的内容不能被清");
        assert_eq!(plan.kept_pinned, 1);
    }

    #[test]
    fn blob_with_landing_file_is_kept() {
        let _g = serial();
        let home = fake_home("linked");
        let store = Store::open().unwrap();
        let p = blob(&home, &"c3".repeat(32), 10, 90);
        // 落地文件还在：硬链一份
        fs::hard_link(&p, home.join("landed.bin")).unwrap();
        let plan = plan(&store, parse_age("30d").unwrap()).unwrap();
        assert!(plan.blobs.is_empty(), "还有落地文件链着就不能删");
        assert_eq!(plan.kept_linked, 1);
    }

    #[test]
    fn apply_removes_only_candidates() {
        let _g = serial();
        let home = fake_home("apply");
        let store = Store::open().unwrap();
        let dead = blob(&home, &"d4".repeat(32), 33, 60);
        let alive = blob(&home, &"e5".repeat(32), 11, 0);
        let plan = plan(&store, parse_age("30d").unwrap()).unwrap();
        let (n, bytes) = apply(&plan).unwrap();
        assert_eq!((n, bytes), (1, 33));
        assert!(!dead.exists());
        assert!(alive.exists());
    }

    #[test]
    fn budget_evicts_oldest_down_to_low_water() {
        let _g = serial();
        let home = fake_home("gc");
        let store = Store::open().unwrap();
        let oldest = "11".repeat(32);
        let middle = "22".repeat(32);
        let newest = "33".repeat(32);
        blob(&home, &oldest, 1000, 40);
        blob(&home, &middle, 1000, 30);
        blob(&home, &newest, 1000, 20);

        // 上限 2500、低水位 1000：3000 → 要清到 1000 以下，删最老的两个。
        let ev = enforce_budget(
            &store,
            2500,
            1000,
            Duration::from_secs(10 * 86_400),
        )
        .unwrap();
        assert_eq!(ev.blobs, 2, "3000 → 1000 要删两块：{ev:?}");
        assert_eq!(ev.bytes, 2000);
        assert_eq!(ev.total_after, 1000);
        assert_eq!(cache_bytes(), 1000);
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn budget_does_nothing_below_the_watermark() {
        let _g = serial();
        let home = fake_home("gc-under");
        let store = Store::open().unwrap();
        blob(&home, &"44".repeat(32), 500, 90);
        let ev = enforce_budget(&store, 10_000, 9_000, Duration::from_secs(0)).unwrap();
        assert_eq!(ev.blobs, 0);
        assert_eq!(ev.total_after, 500);
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn budget_never_deletes_something_just_downloaded() {
        let _g = serial();
        let home = fake_home("gc-young");
        let store = Store::open().unwrap();
        blob(&home, &"55".repeat(32), 5000, 0); // 刚刚下的
        // 上限只有 1000，但唯一的块比最小年龄年轻：宁可超预算，也不删刚下完的。
        let ev = enforce_budget(&store, 1000, 900, Duration::from_secs(3600)).unwrap();
        assert_eq!(ev.blobs, 0, "刚下完的块不能被当成旧块清掉");
        assert_eq!(cache_bytes(), 5000);
        let _ = fs::remove_dir_all(home);
    }
}
