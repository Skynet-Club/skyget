//! 落盘位置。全部可在 `SKYGET_HOME` 下一处搬走 —— 便携模式就是把这个变量指到 U 盘。

use std::path::PathBuf;

/// 数据根：任务台账、内容寻址缓存、未完成的分片。
pub fn data_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("SKYGET_HOME") {
        return PathBuf::from(home);
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".local/share"));
    base.join("skynet-download")
}

pub fn db_path() -> PathBuf {
    data_dir().join("tasks.db")
}

/// 内容寻址缓存：同一份内容全机只存一份（网盘的「秒传」和 HF 的 blob 都是这个）。
pub fn blobs_dir() -> PathBuf {
    data_dir().join("blobs")
}

/// 未完成的分片：`<id>.part` + `<id>.json`（段清单）。断了就从这里接着下。
pub fn parts_dir() -> PathBuf {
    data_dir().join("parts")
}

/// 默认落地目录：`~/Downloads`。没有就建一个 —— 下载器该有自己的落地目录，
/// 而不是把文件丢在家目录里（那看着像事故现场）。
pub fn default_dest_dir() -> PathBuf {
    let home = home_dir();
    let dl = home.join("Downloads");
    if dl.is_dir() {
        return dl;
    }
    if std::fs::create_dir_all(&dl).is_ok() {
        return dl;
    }
    home
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

pub fn ensure_dirs() -> std::io::Result<()> {
    // 新目录按 0700 建：台账里有预签名 URL，缓存/分片也可能是私有内容。
    // 已经存在的目录不强行 chmod（部署方可能是有意共享）；权限检查见
    // docs/security.md。
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        for d in [data_dir(), blobs_dir(), parts_dir(), data_dir().join("secrets")] {
            if !d.exists() {
                std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&d)?;
            }
        }
    }
    #[cfg(not(unix))]
    {
        for d in [data_dir(), blobs_dir(), parts_dir(), data_dir().join("secrets")] {
            std::fs::create_dir_all(&d)?;
        }
    }
    Ok(())
}
