//! 清单：一条命令把东西取下来、校验、放到该在的位置。
//!
//! 两种清单：
//! 1. **下载清单**（本文件定义的 JSON）：给 agent 用 —— 一次调用完成「下载 + 校验 +
//!    落位 + 权限 + 告诉你怎么调」。这是 npm/apt 不管的那部分：任意 URL、私有文件、
//!    模型权重、脚本产物。
//! 2. **天网机库目录**（只读）：agent 先发现「有什么」，再决定取哪个。
//!
//! 不做的事：不解析网页、不猜下载地址、不替包管理器装生态内的包。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::engine::{self, Engine};
use crate::model::{new_id, now_ms, Task};
use crate::paths;
use crate::store::Store;

// ── 下载清单 ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Manifest {
    pub name: String,
    #[serde(default)]
    pub version: String,
    pub files: Vec<FileSpec>,
    /// 装好之后该跑哪个命令（报给人/agent 的那一句）
    #[serde(default)]
    pub bin: String,
    #[serde(default)]
    pub usage: String,
    /// 可选：对「清单的规范化摘要」的 ed25519 签名。
    ///
    /// 有签名就必须验（没有可信公钥就报错，不静默跳过）——
    /// `sha256` 只证明「拿到的字节和某个 hash 一致」，证明不了「这个 hash 是发布方给的」。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<ManifestSignature>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ManifestSignature {
    /// 发布方自己起的名字，用来在多把公钥里挑对应的那一把
    #[serde(default)]
    pub key_id: String,
    /// ed25519 签名，128 位十六进制（64 字节）
    pub sig: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FileSpec {
    pub url: String,
    #[serde(default)]
    pub sha256: String,
    /// 落地路径，支持 `~`
    pub dest: String,
    /// 八进制权限，如 "755"
    #[serde(default)]
    pub mode: String,
}

#[derive(Debug, Serialize)]
pub struct Installed {
    pub name: String,
    pub version: String,
    pub bin: String,
    pub usage: String,
    pub files: Vec<InstalledFile>,
}

#[derive(Debug, Serialize)]
pub struct InstalledFile {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
    pub cached: bool,
}

/// `ensure` 的结论：每个文件到底是「这次装的」还是「本来就对」。
///
/// 这个区分是 `ensure` 存在的全部理由：agent 冷启动时想知道「机器现在是不是
/// 我要的样子」，而不是「又下了一遍」。已经正确的不联网、不写盘。
#[derive(Debug, Serialize)]
pub struct Ensured {
    pub name: String,
    pub version: String,
    pub bin: String,
    pub usage: String,
    /// 这次真的装了/换了的文件
    pub changed: Vec<EnsuredFile>,
    /// 本来就已经是对的（sha256 相符）
    pub unchanged: Vec<EnsuredFile>,
}

#[derive(Debug, Serialize)]
pub struct EnsuredFile {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
}

const MAX_MANIFEST: usize = 1 << 20;

/// 清单里的每个文件必须先过形状检查，且**必须**有 sha256：
/// 清单是安装器的信任根，没有 hash 的条目等于装什么全凭服务器。
fn validate_file(f: &FileSpec) -> Result<()> {
    if f.url.trim().is_empty() {
        bail!("清单文件缺少 url");
    }
    let sha = f.sha256.trim();
    if sha.is_empty() {
        bail!("清单文件必须带 sha256（{}）", f.url);
    }
    if !crate::security::is_sha256_hex(sha) {
        bail!("清单里的 sha256 不是 64 位十六进制（{}）", f.url);
    }
    if f.dest.trim().is_empty() {
        bail!("清单文件缺少 dest（{}）", f.url);
    }
    Ok(())
}

/// 清单的规范化形式（签名覆盖的就是它）。
///
/// 故意不含 JSON 的空白/键顺序：同样的内容必须得到同样的字节，
/// 否则「重新格式化一下就验不过」会变成日常事故。
/// 文件按 `(dest, url)` 排序 —— 清单里调换两行不该改变签名。
pub fn canonical(m: &Manifest) -> String {
    let mut out = String::new();
    out.push_str("skyget-manifest-v1\n");
    out.push_str(&format!("name:{}\n", m.name));
    out.push_str(&format!("version:{}\n", m.version));
    let mut files: Vec<&FileSpec> = m.files.iter().collect();
    files.sort_by(|a, b| (&a.dest, &a.url).cmp(&(&b.dest, &b.url)));
    for f in files {
        out.push_str(&format!(
            "file:{}\0{}\0{}\0{}\n",
            f.dest.trim(),
            f.sha256.trim().to_ascii_lowercase(),
            f.mode.trim(),
            f.url.trim()
        ));
    }
    out.push_str(&format!("bin:{}\n", m.bin));
    out.push_str(&format!("usage:{}\n", m.usage));
    out
}

/// 规范化摘要（十六进制）。签名工具签的就是这 32 字节。
pub fn digest_hex(m: &Manifest) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(canonical(m).as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// 从 `--key` / `SKYGET_TRUSTED_KEYS` 里的一项解析公钥。
///
/// 接受三种写法：`<64 位十六进制>`、`<key_id>=<64 位十六进制>`、`@/path/to/key`。
pub fn parse_key(spec: &str) -> Result<(String, Vec<u8>)> {
    let spec = spec.trim();
    let (id, raw) = if let Some(p) = spec.strip_prefix('@') {
        let text = std::fs::read_to_string(p)
            .with_context(|| format!("读不到公钥文件：{p}"))?;
        (String::new(), text)
    } else {
        match spec.split_once('=') {
            Some((id, rest)) => (id.trim().to_string(), rest.to_string()),
            None => (String::new(), spec.to_string()),
        }
    };
    let raw = raw.trim();
    if raw.len() != 64 {
        bail!("公钥要 64 位十六进制（ed25519 32 字节），拿到 {} 位", raw.len());
    }
    let bytes = decode_hex(raw).with_context(|| "公钥不是十六进制")?;
    if bytes.len() != 32 {
        bail!("公钥长度不对（{} 字节，要 32）", bytes.len());
    }
    Ok((id, bytes))
}

/// 验签。`keys` 是 `--key` 给的全部可信公钥（按 key_id 匹配，空 id 视作万能匹配）。
pub fn verify_signature(m: &Manifest, keys: &[(String, Vec<u8>)]) -> Result<()> {
    let Some(sig) = &m.signature else {
        bail!("清单没有签名（`signature` 字段为空）");
    };
    if keys.is_empty() {
        bail!(
            "清单带签名，但没有可信公钥：给 `--key <hex>` 或设 SKYGET_TRUSTED_KEYS"
        );
    }
    let sig_bytes = decode_hex(sig.sig.trim()).context("签名字段不是十六进制")?;
    if sig_bytes.len() != 64 {
        bail!("签名长度不对（{} 字节，要 64）", sig_bytes.len());
    }
    let msg = canonical(m);
    let candidates: Vec<&(String, Vec<u8>)> = keys
        .iter()
        .filter(|(id, _)| id.is_empty() || sig.key_id.is_empty() || *id == sig.key_id)
        .collect();
    if candidates.is_empty() {
        bail!(
            "签名用的 key_id「{}」不在可信公钥里（已知：{}）",
            sig.key_id,
            keys.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>().join(", ")
        );
    }
    for (_, key) in candidates {
        let pk = ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key);
        if pk.verify(msg.as_bytes(), &sig_bytes).is_ok() {
            return Ok(());
        }
    }
    bail!("清单签名校验失败：内容和签名对不上（可能是被改过，也可能拿错了公钥）")
}

/// 生成一把发布用密钥：返回（PKCS#8 DER 十六进制私钥，公钥十六进制）。
///
/// 私钥就是 ring 自己产出的 DER（不是自创格式）；公钥取 DER 末尾 32 字节 ——
/// ed25519 的 PKCS#8 v2 把公钥追加在 [1] BIT STRING 里，ring 生成的就是 v2。
pub fn generate_key() -> Result<(String, String)> {
    let rng = ring::rand::SystemRandom::new();
    let der = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng)
        .map_err(|_| anyhow::anyhow!("生成密钥失败（系统随机源不可用）"))?;
    let der = der.as_ref();
    if der.len() < 32 {
        bail!("生成的密钥格式意外（{} 字节）", der.len());
    }
    let hex = |b: &[u8]| -> String { b.iter().map(|x| format!("{x:02x}")).collect() };
    Ok((hex(der), hex(&der[der.len() - 32..])))
}

/// 给清单签名，返回签名的十六进制。
///
/// `private_der_hex` 是 `generate_key` 产出的 PKCS#8 DER。
pub fn sign_manifest(m: &Manifest, private_der_hex: &str, key_id: &str) -> Result<String> {
    let der = decode_hex(private_der_hex.trim()).context("私钥不是十六进制")?;
    let kp = ring::signature::Ed25519KeyPair::from_pkcs8(&der)
        .map_err(|_| anyhow::anyhow!("私钥格式不对（要 PKCS#8 DER 十六进制）"))?;
    let sig = kp.sign(canonical(m).as_bytes());
    let _ = key_id;
    Ok(sig.as_ref().iter().map(|b| format!("{b:02x}")).collect())
}

fn decode_hex(s: &str) -> Result<Vec<u8>> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) {
        bail!("十六进制长度必须是偶数");
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let b = s.as_bytes();
    for i in (0..b.len()).step_by(2) {
        let hi = (b[i] as char).to_digit(16).context("不是十六进制")?;
        let lo = (b[i + 1] as char).to_digit(16).context("不是十六进制")?;
        out.push((hi * 16 + lo) as u8);
    }
    Ok(out)
}

/// 读清单：本地文件或 http(s)。
pub fn load(engine: &Engine, src: &str) -> Result<Manifest> {
    let text = if src.starts_with("http://") || src.starts_with("https://") {
        // 清单本身也是出网：不能绕过 daemon 的私网/元数据边界。
        crate::security::check_url(src, &engine.policy)?;
        use std::io::Read;
        let resp = engine
            .client
            .get(src)
            .send()
            .with_context(|| format!("取不到清单：{src}"))?
            .error_for_status()
            .with_context(|| format!("清单地址返回错误：{src}"))?;
        // 边读边限流：不能先解压/读完一个 gzip bomb，再检查长度。
        let mut buf = Vec::new();
        resp.take(MAX_MANIFEST as u64 + 1)
            .read_to_end(&mut buf)
            .context("读清单失败")?;
        if buf.len() > MAX_MANIFEST {
            bail!("清单太大（上限 {MAX_MANIFEST} 字节）");
        }
        String::from_utf8(buf).context("清单不是 UTF-8 文本")?
    } else {
        let path = expand(src);
        let m = std::fs::metadata(&path).with_context(|| format!("读不到清单文件：{src}"))?;
        if !m.is_file() {
            bail!("清单必须是普通文件：{}", path.display());
        }
        if m.len() > MAX_MANIFEST as u64 {
            bail!("清单太大：{} 字节（上限 {MAX_MANIFEST}）", m.len());
        }
        std::fs::read_to_string(path).with_context(|| format!("读不到清单文件：{src}"))?
    };
    let mut m: Manifest = serde_json::from_str(&text).context("清单格式不对（要 JSON）")?;
    if m.files.is_empty() {
        bail!("清单里没有文件");
    }
    for f in &mut m.files {
        validate_file(f)?;
        // 缓存块路径和台账都用小写；清单里写大写或带空白时先归一，
        // 否则缓存捷径永远命不中，会把已在本机的文件重下一遍。
        f.sha256 = f.sha256.trim().to_ascii_lowercase();
    }
    Ok(m)
}

/// 按清单下载并落位。每个文件都进台账 —— 谁装的、装的什么版本、hash 是多少，查得到。
pub fn install(
    store: &Store,
    engine: &Engine,
    m: &Manifest,
    cb: &(dyn Fn(engine::Event) + Sync),
) -> Result<Installed> {
    let mut done = Vec::new();
    for (i, f) in m.files.iter().enumerate() {
        validate_file(f)?;
        let dest = expand(&f.dest);
        // 先查边界再建父目录：不能先替清单在根外把目录建出来。
        crate::security::check_dest(&dest.display().to_string(), &engine.policy)?;
        let parent = dest
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        std::fs::create_dir_all(&parent)
            .with_context(|| format!("建不了目标目录：{}", parent.display()))?;
        let name = dest
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| format!("{}#{}", m.name, i));

        let mut task = Task {
            id: new_id(&format!("{}#{}#{}", m.name, i, f.url)),
            direction: crate::model::DIR_DOWNLOAD.to_string(),
            protocol: String::new(),
            credential_ref: String::new(),
            sources: Vec::new(),
            url: f.url.clone(),
            dest: parent.join(&name).display().to_string(),
            state: crate::model::STATE_PENDING.to_string(),
            bytes_done: 0,
            bytes_total: 0,
            sha256: String::new(),
            error: String::new(),
            error_code: String::new(),
            retryable: false,
            retry_after_ms: 0,
            next_action: String::new(),
            source: "manifest".to_string(),
            // 直接构造 Manifest 调 install 的调用方也要归一，不能只靠 load。
            expected_sha256: f.sha256.trim().to_ascii_lowercase(),
            pid: 0,
            links_live: 0,
            links_seen: 0,
            trust: String::new(),
            priority: crate::model::PRIORITY_NORMAL.to_string(),
            accept_change: false,
            cached: false,
            rerun: false,
            created_at: now_ms(),
            updated_at: now_ms(),
        };

        engine::run(store, engine, &mut task, cb)
            .with_context(|| format!("取 {} 失败", f.url))?;

        if !f.mode.is_empty() {
            apply_mode(&task.dest, &f.mode)?;
        }
        done.push(InstalledFile {
            path: task.dest.clone(),
            sha256: task.sha256.clone(),
            bytes: task.bytes_total,
            cached: task.cached,
        });
    }

    Ok(Installed {
        name: m.name.clone(),
        version: m.version.clone(),
        bin: if m.bin.is_empty() {
            m.files
                .first()
                .map(|f| {
                    Path::new(&expand(&f.dest))
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default()
                })
                .unwrap_or_default()
        } else {
            m.bin.clone()
        },
        usage: m.usage.clone(),
        files: done,
    })
}

fn apply_mode(path: &str, mode: &str) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let m = u32::from_str_radix(mode.trim_start_matches("0o"), 8)
            .with_context(|| format!("权限写法不对：{mode}"))?
            & 0o777; // 不允许清单装 setuid/setgid/sticky
        let p = Path::new(path);
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(m))
            .with_context(|| format!("改不了权限：{path}"))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

/// 幂等对账：清单里已经正确的文件不联网、不写盘；不对的按 install 的方式取。
///
/// 边界（写死在代码里，不给开关）：
/// - **只增改，不删**。清单里没有的文件不碰 —— 这个工具不知道 `~/.local/bin`
///   里哪些东西是它装的，删错一次就是把别人的工具链拆了。
/// - 每一条都必须带 sha256（和 `install` 同一条规矩）：没有 hash 就没有
///   「已经是对的了」这个判断，`ensure` 会退化成每次重下。
/// - 权限只在文件真的变了的时候重设，避免每次都去动一个正在被执行的二进制。
pub fn ensure(
    store: &Store,
    engine: &Engine,
    m: &Manifest,
    cb: &(dyn Fn(engine::Event) + Sync),
) -> Result<Ensured> {
    let mut changed = Vec::new();
    let mut unchanged = Vec::new();
    for (i, f) in m.files.iter().enumerate() {
        validate_file(f)?;
        let want = f.sha256.trim().to_ascii_lowercase();
        let dest = expand(&f.dest);
        crate::security::check_dest(&dest.display().to_string(), &engine.policy)?;

        // 已经在缓存里、而且落地文件就是它：一个字节都不用碰。
        if let Ok(meta) = std::fs::metadata(&dest) {
            if meta.is_file() && crate::verify::sha256_file(&dest).ok().as_deref() == Some(want.as_str()) {
                unchanged.push(EnsuredFile {
                    path: dest.display().to_string(),
                    sha256: want,
                    bytes: meta.len(),
                });
                continue;
            }
        }

        let parent = dest
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        std::fs::create_dir_all(&parent)
            .with_context(|| format!("建不了目标目录：{}", parent.display()))?;
        let name = dest
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| format!("{}#{}", m.name, i));

        let mut task = Task {
            id: new_id(&format!("{}#{}#{}", m.name, i, f.url)),
            direction: crate::model::DIR_DOWNLOAD.to_string(),
            protocol: String::new(),
            credential_ref: String::new(),
            sources: Vec::new(),
            url: f.url.clone(),
            dest: parent.join(&name).display().to_string(),
            state: crate::model::STATE_PENDING.to_string(),
            bytes_done: 0,
            bytes_total: 0,
            sha256: String::new(),
            error: String::new(),
            error_code: String::new(),
            retryable: false,
            retry_after_ms: 0,
            next_action: String::new(),
            source: "manifest".to_string(),
            priority: crate::model::PRIORITY_NORMAL.to_string(),
            expected_sha256: want.clone(),
            pid: 0,
            links_live: 0,
            links_seen: 0,
            trust: String::new(),
            accept_change: false,
            cached: false,
            rerun: false,
            created_at: now_ms(),
            updated_at: now_ms(),
        };
        engine::run(store, engine, &mut task, cb)
            .with_context(|| format!("取 {} 失败", f.url))?;
        if !f.mode.is_empty() {
            apply_mode(&task.dest, &f.mode)?;
        }
        changed.push(EnsuredFile {
            path: task.dest.clone(),
            sha256: task.sha256.clone(),
            bytes: task.bytes_total,
        });
    }
    Ok(Ensured {
        name: m.name.clone(),
        version: m.version.clone(),
        bin: m.bin.clone(),
        usage: m.usage.clone(),
        changed,
        unchanged,
    })
}

pub fn expand(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        return paths::home_dir().join(rest);
    }
    if p == "~" {
        return paths::home_dir();
    }
    PathBuf::from(p)
}

// ── 天网机库目录（只读发现）────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct CatalogEntry {
    pub id: String,
    pub name: String,
    pub category: String,
    pub summary: String,
    pub repo: String,
    pub homepage: String,
    pub install: BTreeMap<String, String>,
}

fn catalog_path() -> PathBuf {
    std::env::var_os("SKYGET_CATALOG")
        .map(PathBuf::from)
        .unwrap_or_else(|| paths::home_dir().join("skynet-catalog/catalog.db"))
}

/// 机库里有哪些东西。查不到就明说「没装机库」，不要静默返回空。
pub fn catalog_list(query: Option<&str>, limit: usize) -> Result<Vec<CatalogEntry>> {
    let p = catalog_path();
    if !p.is_file() {
        bail!(
            "没找到天网机库目录（{}）。装一个，或用 SKYGET_CATALOG 指到 catalog.db。",
            p.display()
        );
    }
    let conn = rusqlite::Connection::open_with_flags(
        &p,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .with_context(|| format!("打不开机库目录：{}", p.display()))?;
    let mut stmt = conn.prepare("SELECT id, category, data FROM entries ORDER BY rank, id")?;
    let mut rows = stmt.query([])?;
    let q = query.unwrap_or("").to_lowercase();
    let mut out = Vec::new();
    while let Some(r) = rows.next()? {
        let id: String = r.get(0)?;
        let category: String = r.get(1)?;
        let data: String = r.get(2)?;
        let Ok(doc) = serde_json::from_str::<serde_json::Value>(&data) else {
            continue;
        };
        let name = doc.get("name").and_then(|v| v.as_str()).unwrap_or(&id).to_string();
        let summary = doc
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if !q.is_empty()
            && !(id.to_lowercase().contains(&q)
                || name.to_lowercase().contains(&q)
                || summary.to_lowercase().contains(&q))
        {
            continue;
        }
        let mut install = BTreeMap::new();
        if let Some(ins) = doc.get("install").and_then(|v| v.as_object()) {
            for k in ["method", "package", "url", "sha256", "size_bytes"] {
                if let Some(v) = ins.get(k) {
                    if !v.is_null() {
                        install.insert(k.to_string(), v.to_string().trim_matches('"').to_string());
                    }
                }
            }
        }
        out.push(CatalogEntry {
            id,
            name,
            category,
            summary,
            repo: doc
                .get("repo")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            homepage: doc
                .get("homepage")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            install,
        });
        if out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_skips_what_is_already_correct() {
        let dir = std::env::temp_dir().join(format!("skyget-ensure-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("tool.bin");
        std::fs::write(&file, b"already here").unwrap();
        let sha = crate::verify::sha256_file(&file).unwrap();

        let store = Store::open_at(&dir.join("tasks.db")).unwrap();
        let engine = Engine::new(1);
        let m = Manifest {
            name: "t".into(),
            version: "1".into(),
            files: vec![FileSpec {
                // 地址故意是打不通的：如果它真的去联网，这个用例会失败。
                url: "http://127.0.0.1:1/never".into(),
                sha256: sha.clone(),
                dest: file.display().to_string(),
                mode: String::new(),
            }],
            bin: String::new(),
            usage: String::new(),
            signature: None,
        };
        let out = ensure(&store, &engine, &m, &|_e| {}).unwrap();
        assert!(out.changed.is_empty(), "已经正确的文件不该重装：{out:?}");
        assert_eq!(out.unchanged.len(), 1);
        assert_eq!(out.unchanged[0].sha256, sha);
        assert_eq!(std::fs::read(&file).unwrap(), b"already here");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn ensure_does_not_delete_files_it_did_not_install() {
        // 只增改不删：清单里没有的文件必须原样留着。
        let dir = std::env::temp_dir().join(format!("skyget-ensure-keep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mine = dir.join("mine.bin");
        let theirs = dir.join("theirs.bin");
        std::fs::write(&mine, b"m").unwrap();
        std::fs::write(&theirs, b"t").unwrap();
        let sha = crate::verify::sha256_file(&mine).unwrap();
        let store = Store::open_at(&dir.join("tasks.db")).unwrap();
        let engine = Engine::new(1);
        let m = Manifest {
            name: "t".into(),
            version: "1".into(),
            files: vec![FileSpec {
                url: "http://127.0.0.1:1/never".into(),
                sha256: sha,
                dest: mine.display().to_string(),
                mode: String::new(),
            }],
            bin: String::new(),
            usage: String::new(),
            signature: None,
        };
        ensure(&store, &engine, &m, &|_e| {}).unwrap();
        assert!(theirs.exists(), "清单之外的文件不能被删");
        let _ = std::fs::remove_dir_all(dir);
    }

    fn test_manifest(sha: &str) -> Manifest {
        Manifest {
            name: "tool".into(),
            version: "1.0".into(),
            files: vec![
                FileSpec {
                    url: "https://example.org/a.bin".into(),
                    sha256: sha.into(),
                    dest: "~/bin/a".into(),
                    mode: "755".into(),
                },
                FileSpec {
                    url: "https://example.org/b.bin".into(),
                    sha256: "b".repeat(64),
                    dest: "~/bin/b".into(),
                    mode: String::new(),
                },
            ],
            bin: "~/bin/a".into(),
            usage: "a --help".into(),
            signature: None,
        }
    }

    fn sign(keypair: &ring::signature::Ed25519KeyPair, m: &mut Manifest) {
        let sig = keypair.sign(canonical(m).as_bytes());
        let hex: String = sig.as_ref().iter().map(|b| format!("{b:02x}")).collect();
        m.signature = Some(ManifestSignature {
            key_id: "release-1".into(),
            sig: hex,
        });
    }

    #[test]
    fn signature_round_trips_and_detects_tampering() {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let kp = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        // ring 的 Ed25519KeyPair 不公开私钥/公钥导出接口；公钥从 pkcs8 里按
        // 固定偏移取（ed25519 PKCS#8 的最后 32 字节就是公钥）。
        let pkcs8 = pkcs8.as_ref();
        let pub_bytes = pkcs8[pkcs8.len() - 32..].to_vec();
        let pub_hex: String = pub_bytes.iter().map(|b| format!("{b:02x}")).collect();
        let keys = vec![("release-1".to_string(), pub_bytes)];

        let mut m = test_manifest(&"a".repeat(64));
        sign(&kp, &mut m);
        verify_signature(&m, &keys).expect("正确签名必须通过");
        // 公钥写成十六进制字符串也要能解析
        let parsed = parse_key(&format!("release-1={pub_hex}")).unwrap();
        verify_signature(&m, &[parsed]).expect("hex 公钥也要能验");

        // 改一个 sha256：必须失败，而且失败原因要指得出「内容被改过」
        m.files[0].sha256 = "c".repeat(64);
        let err = verify_signature(&m, &keys).unwrap_err().to_string();
        assert!(err.contains("签名校验失败"), "{err}");

        // 签名对的但 key_id 不在可信集合里：拒绝
        let mut m2 = test_manifest(&"a".repeat(64));
        sign(&kp, &mut m2);
        let other = vec![("other".to_string(), vec![7u8; 32])];
        assert!(verify_signature(&m2, &other).is_err());
    }

    #[test]
    fn keygen_sign_verify_is_a_closed_loop() {
        let (priv_hex, pub_hex) = generate_key().unwrap();
        let mut m = test_manifest(&"a".repeat(64));
        let sig = sign_manifest(&m, &priv_hex, "release-1").unwrap();
        m.signature = Some(ManifestSignature {
            key_id: "release-1".into(),
            sig,
        });
        let keys = vec![parse_key(&pub_hex).unwrap()];
        verify_signature(&m, &keys).expect("自己签的必须自己验得过");
        // 换一把钥匙就验不过
        let (_, other_pub) = generate_key().unwrap();
        assert!(verify_signature(&m, &[parse_key(&other_pub).unwrap()]).is_err());
    }

    #[test]
    fn canonical_digest_ignores_file_order_and_whitespace() {
        let a = test_manifest(&"a".repeat(64));
        let mut b = a.clone();
        b.files.reverse();
        assert_eq!(digest_hex(&a), digest_hex(&b), "清单里调换两行不该改变摘要");
        // 但内容变了一定变
        let mut c = a.clone();
        c.files[0].sha256 = "d".repeat(64);
        assert_ne!(digest_hex(&a), digest_hex(&c));
    }

    #[test]
    fn bad_keys_are_rejected_with_a_useful_message() {
        assert!(parse_key("abc").is_err());
        assert!(parse_key(&"z".repeat(64)).is_err(), "非十六进制要拒绝");
        assert!(parse_key(&"ab".repeat(16)).is_err(), "16 字节不是 ed25519 公钥");
        assert!(parse_key(&"ab".repeat(32)).is_ok());
    }

    #[test]
    fn tilde_expands() {
        let home = paths::home_dir();
        assert_eq!(expand("~/.local/bin/x"), home.join(".local/bin/x"));
        assert_eq!(expand("/abs/x"), PathBuf::from("/abs/x"));
    }

    #[test]
    fn manifest_requires_files() {
        let bad: Result<Manifest> = serde_json::from_str::<Manifest>("{\"name\":\"x\",\"files\":[]}")
            .map_err(Into::into);
        assert!(bad.is_ok());
        assert!(bad.unwrap().files.is_empty());
    }

    #[test]
    fn manifest_files_require_sha_and_dest() {
        let good = FileSpec {
            url: "https://x/a".into(),
            sha256: "a".repeat(64),
            dest: "/tmp/x".into(),
            mode: "755".into(),
        };
        assert!(validate_file(&good).is_ok());

        let mut f = good.clone();
        f.sha256.clear();
        assert!(validate_file(&f).is_err(), "没有 sha 的清单文件必须拒绝");

        let mut f = good.clone();
        f.sha256 = "ab".into();
        assert!(validate_file(&f).is_err(), "短 sha 必须拒绝");

        let mut f = good;
        f.dest.clear();
        assert!(validate_file(&f).is_err(), "没有 dest 必须拒绝");
    }

    #[test]
    fn load_rejects_non_regular_manifest() {
        let engine = Engine::new(1);
        let err = load(&engine, "/dev/null").unwrap_err().to_string();
        assert!(err.contains("普通文件"), "实际：{err}");
    }

    #[test]
    fn manifest_parses_full() {
        let m: Manifest = serde_json::from_str(
            r#"{"name":"opencode","version":"1.0","bin":"opencode",
                "files":[{"url":"https://x/a","sha256":"ab","dest":"~/.local/bin/opencode","mode":"755"}]}"#,
        )
        .unwrap();
        assert_eq!(m.files.len(), 1);
        assert_eq!(m.files[0].mode, "755");
    }

    #[test]
    fn load_normalizes_manifest_sha() {
        let dir = std::env::temp_dir().join(format!("skyget-manifest-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("manifest.json");
        let upper = "A".repeat(64);
        std::fs::write(
            &path,
            format!(
                r#"{{"name":"t","files":[{{"url":"https://x/a","sha256":"  {upper}  ","dest":"/tmp/a"}}]}}"#
            ),
        )
        .unwrap();
        let m = load(&Engine::new(1), path.to_str().unwrap()).unwrap();
        assert_eq!(m.files[0].sha256, "a".repeat(64));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
