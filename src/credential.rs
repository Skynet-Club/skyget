//! 凭证引用：`env:NAME` / `file:PATH`；值只在用时读，不落台账。
//!
//! 两个限制都是防「借任务外传本机秘密」：
//! - env 只允许调用方指定的白名单变量名；
//! - file 只允许 `SKYGET_HOME/secrets/` 下的文件（canonicalize 后比较）。

use std::fs;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};

/// 解析 `credential_ref`。`allowlist_env` 是这个方向的白名单变量名：
/// 上传用 `SKYGET_UPLOAD_CREDENTIALS`，下载用 `SKYGET_DOWNLOAD_CREDENTIALS`。
pub fn resolve(reference: &str, allowlist_env: &str) -> Result<Option<String>> {
    let r = reference.trim();
    if r.is_empty() {
        return Ok(None);
    }
    if let Some(name) = r.strip_prefix("env:") {
        let name = name.trim();
        let allowed = std::env::var(allowlist_env)
            .ok()
            .map(|list| list.split(',').any(|x| x.trim() == name))
            .unwrap_or(false);
        if !allowed {
            return Err(crate::security::Denied {
                code: "policy.credential_denied",
                why: format!("环境变量 {name} 不在 {allowlist_env} 白名单里。"),
            }
            .into());
        }
        let v = std::env::var(name).map_err(|_| crate::security::Denied {
            code: "policy.credential_denied",
            why: format!("环境变量 {name} 没有设置。"),
        })?;
        return Ok(Some(v.trim().to_string()));
    }
    if let Some(path) = r.strip_prefix("file:") {
        let p = PathBuf::from(crate::settings::expand(path.trim()));
        let root = crate::paths::data_dir().join("secrets");
        let root = fs::canonicalize(&root).map_err(|_| crate::security::Denied {
            code: "policy.credential_denied",
            why: format!("凭证目录不存在：{}", root.display()),
        })?;
        let real = fs::canonicalize(&p).map_err(|e| crate::security::Denied {
            code: "policy.credential_denied",
            why: format!("读不到凭证文件 {}：{e}", p.display()),
        })?;
        if !real.starts_with(&root) {
            return Err(crate::security::Denied {
                code: "policy.credential_denied",
                why: format!(
                    "file: 凭证只允许放在 {} 下，收到 {}。",
                    root.display(),
                    real.display()
                ),
            }
            .into());
        }
        check_file_permissions(&real)?;
        let v = std::fs::read_to_string(&real)
            .with_context(|| format!("读不到凭证文件：{}", real.display()))?;
        return Ok(Some(v.trim().to_string()));
    }
    bail!("credential_ref 只支持 env:NAME / file:PATH，收到：{r}")
}

/// 凭证文件不能有组/其他用户的权限位：0644 的 token 对本机其他用户可见。
#[cfg(unix)]
fn check_file_permissions(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(path)?.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(crate::security::Denied {
            code: "policy.credential_denied",
            why: format!(
                "凭证文件权限过宽（{:o}）；请 chmod 600 {}",
                mode,
                path.display()
            ),
        }
        .into());
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_file_permissions(_path: &std::path::Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn file_credentials_must_not_be_group_or_world_readable() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let p = std::env::temp_dir().join(format!(
                "skyget-cred-mode-{}",
                std::process::id()
            ));
            std::fs::write(&p, b"secret").unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(super::check_file_permissions(&p).is_err(), "0644 必须拒绝");
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
            assert!(super::check_file_permissions(&p).is_ok(), "0600 应通过");
            let _ = std::fs::remove_file(p);
        }
    }
}
