//! 用户设置：目前只有一件 —— 下载保存到哪。
//!
//! 为什么要单独一个文件，而不是塞进台账：台账是「发生过什么」的事后账本，
//! 设置是「以后怎么干」的意愿。两者的生命周期完全不同 —— 清台账不该把设置清掉，
//! 换设置也不该在台账里留一行「我改过设置」。
//!
//! 优先级（从高到低）：`SKYGET_DEST` 环境变量 > 设置文件 > `~/Downloads`。
//! 环境变量最高是为了让「临时换一下」不用动用户的长期选择。

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub struct Settings {
    /// 空 = 没设置过，用默认
    pub dest_dir: String,
}

fn file_in(dir: &Path) -> PathBuf {
    dir.join("settings.json")
}

pub fn load() -> Settings {
    load_in(&crate::paths::data_dir())
}

fn load_in(dir: &Path) -> Settings {
    let path = file_in(dir);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Settings::default();
    };
    // 文件坏了不该让下载器起不来：读不懂就当没设置过。
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(v) => Settings {
            dest_dir: v
                .get("dest_dir")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
        },
        Err(_) => Settings::default(),
    }
}

pub fn save(s: &Settings) -> Result<()> {
    save_in(&crate::paths::data_dir(), s)
}

fn save_in(dir: &Path, s: &Settings) -> Result<()> {
    std::fs::create_dir_all(dir).context("建数据目录失败")?;
    let text = serde_json::json!({ "dest_dir": s.dest_dir }).to_string();
    std::fs::write(file_in(dir), format!("{text}\n")).context("写设置失败")?;
    Ok(())
}

/// 现在到底下到哪：环境变量 > 设置文件 > 默认。
pub fn dest_dir() -> PathBuf {
    if let Some(v) = std::env::var_os("SKYGET_DEST") {
        let p = PathBuf::from(v);
        if !p.as_os_str().is_empty() {
            return p;
        }
    }
    let s = load();
    if !s.dest_dir.trim().is_empty() {
        return PathBuf::from(expand(&s.dest_dir));
    }
    crate::paths::default_dest_dir()
}

/// 把 `~/xxx` 展开成绝对路径；相对的也认（相对家目录），但不认空。
pub fn expand(p: &str) -> String {
    let t = p.trim();
    if t.is_empty() {
        return String::new();
    }
    if t == "~" {
        return crate::paths::home_dir().display().to_string();
    }
    if let Some(rest) = t.strip_prefix("~/") {
        return crate::paths::home_dir().join(rest).display().to_string();
    }
    if Path::new(t).is_absolute() {
        return t.trim_end_matches('/').to_string();
    }
    crate::paths::home_dir().join(t).display().to_string()
}

/// 记住一个新目录：先把它建出来（不存在就建），确认真的能写，再存设置。
///
/// 「能写」这一步不能省：路径看着存在、实际只读（比如挂载点、别人的目录），
/// 存进去之后每一个任务都会在落地那一步失败，而那时候的报错跟「目录选错了」隔着十万八千里。
pub fn set_dest(input: &str) -> Result<PathBuf> {
    let abs = expand(input);
    if abs.is_empty() {
        anyhow::bail!("目录不能为空");
    }
    let p = PathBuf::from(&abs);
    std::fs::create_dir_all(&p).with_context(|| format!("建不了这个目录：{abs}"))?;
    let meta = std::fs::metadata(&p).with_context(|| format!("看不到这个目录：{abs}"))?;
    if !meta.is_dir() {
        anyhow::bail!("这不是目录：{abs}");
    }
    // 真写一个临时文件再删掉 —— 只读目录在这一步暴露
    let probe = p.join(format!(".skyget-write-{}", std::process::id()));
    std::fs::write(&probe, b"1").with_context(|| format!("这个目录写不进去：{abs}"))?;
    let _ = std::fs::remove_file(&probe);

    let mut s = load();
    s.dest_dir = p.display().to_string();
    save(&s)?;
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个测试自己一个临时目录，**不动进程级环境变量**。
    /// 第一版在测试里 set_var("SKYGET_HOME") 了 —— 测试是并行跑的，
    /// 于是把别的测试（prune 那组）的数据根一起改了，出现莫名其妙的失败。
    /// 环境变量的验证放在 e2e 里做（那里本来就是一个进程一套 env）。
    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("skyset-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn expand_handles_tilde_and_relative() {
        let home = crate::paths::home_dir().display().to_string();
        assert_eq!(expand("~/下载"), format!("{home}/下载"));
        assert_eq!(expand("~"), home);
        assert_eq!(expand("/tmp/x/"), "/tmp/x");
        assert_eq!(expand("rel"), format!("{home}/rel"));
        assert_eq!(expand("   "), "");
    }

    #[test]
    fn settings_round_trip_and_broken_file_falls_back() {
        let dir = tmpdir("rt");
        let s = Settings {
            dest_dir: "/tmp/某某目录".into(),
        };
        save_in(&dir, &s).unwrap();
        assert_eq!(load_in(&dir).dest_dir, "/tmp/某某目录");

        // 坏掉的 json 不能把下载器弄死：当没设置过
        std::fs::write(file_in(&dir), "{ 这不是 json").unwrap();
        assert_eq!(load_in(&dir).dest_dir, "");

        // 没有文件也一样
        std::fs::remove_file(file_in(&dir)).unwrap();
        assert_eq!(load_in(&dir).dest_dir, "");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_empty_and_files() {
        assert!(set_dest("   ").is_err());
        let f = std::env::temp_dir().join(format!("skyset-file-{}", std::process::id()));
        std::fs::write(&f, b"x").unwrap();
        assert!(set_dest(&f.display().to_string()).is_err(), "指着文件应该说这不是目录");
        let _ = std::fs::remove_file(&f);
    }
}
