//! 内容缓存导出/导入：给 CI 预热、离线搬运、跨机器拷贝用。
//!
//! 导出目录的形状是内容寻址的：`<dir>/<sha256>` + `manifest.json`。
//! 两边都重新算 hash：导出保证“拿出去的是对的”，导入保证“拿回来的没坏”。
//! 这不是 `cp -r blobs`：cp 不会告诉你块被改过，也不会拒绝坏块。

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub blobs: Vec<Entry>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Entry {
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Debug, Default, Serialize)]
pub struct Summary {
    pub blobs: usize,
    pub bytes: u64,
    pub skipped: usize,
}

/// 把 blobs 导出到 `dest`；`shas` 为空 = 全部。
pub fn export(blobs_dir: &Path, dest: &Path, shas: &[String]) -> Result<Summary> {
    fs::create_dir_all(dest).with_context(|| format!("建不了导出目录：{}", dest.display()))?;
    let paths = if shas.is_empty() {
        list_blobs(blobs_dir)?
    } else {
        let mut v = Vec::new();
        for s in shas {
            let s = s.trim().to_ascii_lowercase();
            if !crate::security::is_sha256_hex(&s) {
                bail!("sha256 必须是 64 位十六进制：{s}");
            }
            let p = blob_path(blobs_dir, &s);
            if !p.is_file() {
                bail!("缓存里没有这份内容：{s}");
            }
            v.push(p);
        }
        v
    };

    let mut manifest = Manifest {
        version: 1,
        blobs: Vec::new(),
    };
    let mut out = Summary::default();
    for p in paths {
        let name = p
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        if !crate::security::is_sha256_hex(&name) {
            eprintln!("跳过不是 sha256 命名的块：{}", p.display());
            continue;
        }
        let bytes = fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
        let got = sha256_file(&p)?;
        if got != name {
            bail!(
                "缓存块内容与文件名不符，拒绝导出：{}（实际 {}）",
                p.display(),
                got
            );
        }
        let target = dest.join(&name);
        let tmp = dest.join(format!("{name}.tmp"));
        // 目标已存在但不是普通文件（比如同名目录）：rename 会失败。
        // 以前这个 .ok() 被吞掉，export 仍然写 manifest，导出的块却不存在，
        // import 时才炸 —— 报错必须发生在做坏的那一步。
        if let Ok(meta) = fs::symlink_metadata(&target) {
            if !meta.file_type().is_file() {
                let _ = fs::remove_file(&tmp);
                bail!("导出目标已存在且不是普通文件：{}", target.display());
            }
        }
        fs::copy(&p, &tmp)
            .with_context(|| format!("导出失败：{}", p.display()))?;
        if let Err(e) = fs::rename(&tmp, &target) {
            let _ = fs::remove_file(&tmp);
            return Err(e).with_context(|| format!("导出落地失败：{}", target.display()));
        }
        manifest.blobs.push(Entry {
            sha256: name,
            bytes,
        });
        out.blobs += 1;
        out.bytes += bytes;
    }
    write_manifest(dest, &manifest)?;
    Ok(out)
}

/// 从 `src` 导入到 blobs；已有且内容正确的块跳过。
pub fn import(blobs_dir: &Path, src: &Path) -> Result<Summary> {
    let text = fs::read_to_string(src.join("manifest.json"))
        .with_context(|| format!("读不到 {}", src.join("manifest.json").display()))?;
    let manifest: Manifest =
        serde_json::from_str(&text).context("manifest.json 格式不对")?;
    if manifest.version != 1 {
        bail!("不认识的 manifest 版本：{}", manifest.version);
    }
    fs::create_dir_all(blobs_dir).context("建不了缓存目录")?;
    let mut out = Summary::default();
    for e in manifest.blobs {
        if !crate::security::is_sha256_hex(&e.sha256) {
            bail!("manifest 里有非法 sha256：{}", e.sha256);
        }
        let from = src.join(&e.sha256);
        let meta = fs::metadata(&from)
            .with_context(|| format!("导入文件不存在：{}", from.display()))?;
        if meta.len() != e.bytes {
            bail!(
                "导入文件大小不符：{} 期望 {}，实际 {}",
                from.display(),
                e.bytes,
                meta.len()
            );
        }
        let got = sha256_file(&from)?;
        if got != e.sha256 {
            bail!(
                "导入文件内容不符：{} 期望 {}，实际 {}",
                from.display(),
                e.sha256,
                got
            );
        }
        let target = blob_path(blobs_dir, &e.sha256);
        if target.is_file() && sha256_file(&target).map(|h| h == e.sha256).unwrap_or(false) {
            out.skipped += 1;
            continue;
        }
        let parent = target.parent().unwrap();
        fs::create_dir_all(parent)?;
        let tmp = target.with_extension("tmp");
        fs::copy(&from, &tmp)
            .with_context(|| format!("导入失败：{}", from.display()))?;
        fs::rename(&tmp, &target)
            .with_context(|| format!("导入落位失败：{}", target.display()))?;
        out.blobs += 1;
        out.bytes += e.bytes;
    }
    Ok(out)
}

fn list_blobs(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if !root.is_dir() {
        return Ok(out);
    }
    for bucket in fs::read_dir(root)? {
        let bucket = bucket?.path();
        if !bucket.is_dir() {
            continue;
        }
        for f in fs::read_dir(bucket)? {
            let p = f?.path();
            if p.is_file() {
                out.push(p);
            }
        }
    }
    out.sort();
    Ok(out)
}

fn blob_path(root: &Path, sha: &str) -> PathBuf {
    root.join(&sha[..2]).join(sha)
}

fn write_manifest(dest: &Path, m: &Manifest) -> Result<()> {
    let text = serde_json::to_string_pretty(m)?;
    let tmp = dest.join("manifest.json.tmp");
    fs::write(&tmp, text)?;
    fs::rename(&tmp, dest.join("manifest.json"))?;
    Ok(())
}

fn sha256_file(p: &Path) -> Result<String> {
    let mut f = fs::File::open(p).with_context(|| format!("打不开 {}", p.display()))?;
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

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "skyget-cas-{}-{tag}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn put_blob(root: &Path, data: &[u8]) -> String {
        let sha = {
            let mut h = Sha256::new();
            h.update(data);
            format!("{:x}", h.finalize())
        };
        let p = blob_path(root, &sha);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, data).unwrap();
        sha
    }

    #[test]
    fn export_then_import_round_trips_and_detects_tamper() {
        let base = temp("roundtrip");
        let blobs = base.join("blobs");
        let out = base.join("out");
        let sha = put_blob(&blobs, b"hello cas");
        let s = export(&blobs, &out, &[]).unwrap();
        assert_eq!(s.blobs, 1);
        assert!(out.join(&sha).is_file());

        // 清掉缓存，再导入
        fs::remove_dir_all(&blobs).unwrap();
        let s = import(&blobs, &out).unwrap();
        assert_eq!(s.blobs, 1);
        assert_eq!(sha256_file(&blob_path(&blobs, &sha)).unwrap(), sha);

        // 篡改导出文件：导入必须失败，不能把坏块放进缓存
        fs::write(out.join(&sha), b"tampered!").unwrap(); // 同长度，专门触发 hash 检查
        let e = import(&blobs, &out).unwrap_err();
        assert!(format!("{e:#}").contains("内容不符"), "{e:#}");
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn export_refuses_a_directory_squatting_on_the_blob_name() {
        let base = temp("squat");
        let blobs = base.join("blobs");
        let out = base.join("out");
        let sha = put_blob(&blobs, b"hello squat");
        // 同名目录：rename 会失败。以前 .ok() 吞掉错误，manifest 仍然登记这个块，
        // export 报成功、import 才发现块不存在。
        fs::create_dir_all(out.join(&sha)).unwrap();
        let e = export(&blobs, &out, &[]).unwrap_err();
        assert!(format!("{e:#}").contains("不是普通文件"), "{e:#}");
        assert!(
            !out.join("manifest.json").exists(),
            "失败的 export 不能写 manifest"
        );
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn export_rejects_a_blob_whose_name_does_not_match_content() {
        let base = temp("badblob");
        let blobs = base.join("blobs");
        let out = base.join("out");
        let sha = put_blob(&blobs, b"good");
        fs::write(blob_path(&blobs, &sha), b"bad").unwrap();
        assert!(export(&blobs, &out, &[]).is_err());
        let _ = fs::remove_dir_all(base);
    }
}
