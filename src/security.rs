//! 出网与落地的边界。
//!
//! 数据面同时拥有网络和文件系统原语。人自己用时，"能在这台机器上跑程序"
//! 基本等于"本来就能干这些"；agent 来驱动时不是这样：一个被 prompt injection
//! 的 agent 可以让数据面去取云元数据（169.254.169.254）、内网服务，
//! 或者把文件写到 `~/.ssh/authorized_keys`。这些不是部署环境的问题，
//! 是产品必须在进程内拒绝的事 —— 数据面不能变成 agent 的 confused deputy。
//!
//! 默认值刻意分成两档：
//! - `Policy::default()` 放行私有地址：CLI / 单测 / 本地测试服务器还要用；
//! - `Policy::strict()` 只放行公网：daemon 这条多 agent 通道默认用它。
//!
//! 已知边界：DNS 解析在检查和连接之间仍有一次 TOCTOU 窗口（DNS rebinding）。
//! 彻底解决要自定义 resolver 把解析结果钉死；这一版先把"明显的内网/元数据"
//! 挡住，并在文档里写明残余风险，不假装已经解决。

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Result};
use url::Url;

/// 策略拒绝。单独成类型，上层能 downcast 成 `policy.*` 错误码，
/// 而不是让 agent 去猜一句中文的意思。
#[derive(Debug)]
pub struct Denied {
    pub code: &'static str,
    pub why: String,
}

impl std::fmt::Display for Denied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "策略拒绝：{}", self.why)
    }
}

impl std::error::Error for Denied {}

/// 一次下载的边界。
#[derive(Clone, Debug)]
pub struct Policy {
    /// 允许访问私有 / 回环 / 链路本地 / CGNAT 地址
    pub allow_private: bool,
    /// 允许访问云元数据地址（169.254.169.254 / 100.100.100.200 / fd00:ec2::254）。
    ///
    /// 单独一个开关，**不跟着 allow_private 走**：一个只想下内网镜像的部署，
    /// 不该顺带把「agent 能读云凭证」这件事也打开。默认关。
    pub allow_metadata: bool,
    /// 允许的 host；`None` = 不按 host 限制（仍受 allow_private 约束）。
    /// `Some([])` = 一个都不允许。
    pub allowed_hosts: Option<Vec<String>>,
    /// 允许落地的根目录；空 = 不限制（兼容旧行为）
    pub dest_roots: Vec<PathBuf>,
    /// 允许上传读取的本地根目录；空 = 禁止上传
    pub upload_roots: Vec<PathBuf>,
    /// 最多跟随几次重定向
    pub max_redirects: usize,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            // 人自己用：本地测试服务器、NAS、公司内网制品库都还要能用。
            allow_private: true,
            // 但元数据地址仍然默认拒绝：人自己下东西没有理由去读云凭证。
            allow_metadata: std::env::var("SKYGET_ALLOW_METADATA")
                .map(|v| v == "1")
                .unwrap_or(false),
            allowed_hosts: None,
            dest_roots: Vec::new(),
            upload_roots: Vec::new(),
            max_redirects: 5,
        }
    }
}

impl Policy {
    /// 给 agent 用的默认：只放行公网；落地根目录从 `SKYGET_DEST_ROOTS` 读
    /// （冒号分隔，例如 `~/Downloads:~/models:~/Applications`）。
    pub fn strict() -> Self {
        // 明确的逃生口：本地测试 daemon、公司内网镜像站需要私有地址时，
        // 由部署方显式设置，而不是让 agent 自己决定。
        let allow_metadata = std::env::var("SKYGET_ALLOW_METADATA")
            .map(|v| v == "1")
            .unwrap_or(false);
        let allow_private = std::env::var("SKYGET_ALLOW_PRIVATE")
            .map(|v| v == "1")
            .unwrap_or(false);
        Self {
            allow_private,
            allow_metadata,
            // daemon 也必须尊重白名单；以前 strict() 写死 None，
            // 部署方设了 SKYGET_ALLOWED_HOSTS 却完全不生效。
            allowed_hosts: hosts_from_env(),
            dest_roots: dest_roots_from_env(),
            upload_roots: upload_roots_from_env(),
            max_redirects: 5,
        }
    }

    /// CLI 的默认：环境变量 `SKYGET_EGRESS=strict` 或 `SKYGET_ALLOW_PRIVATE=0`
    /// 时收紧，否则保持"人自己用"的宽松档。
    pub fn from_env() -> Self {
        let strict = std::env::var("SKYGET_EGRESS")
            .map(|v| v.eq_ignore_ascii_case("strict"))
            .unwrap_or(false)
            || std::env::var("SKYGET_ALLOW_PRIVATE")
                .map(|v| v == "0")
                .unwrap_or(false);
        Self {
            allow_private: !strict,
            allow_metadata: std::env::var("SKYGET_ALLOW_METADATA")
                .map(|v| v == "1")
                .unwrap_or(false),
            allowed_hosts: hosts_from_env(),
            dest_roots: dest_roots_from_env(),
            upload_roots: upload_roots_from_env(),
            max_redirects: 5,
        }
    }
}

fn hosts_from_env() -> Option<Vec<String>> {
    let raw = std::env::var("SKYGET_ALLOWED_HOSTS").ok()?;
    Some(
        raw.split(',')
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect(),
    )
}

fn dest_roots_from_env() -> Vec<PathBuf> {
    roots_from_env("SKYGET_DEST_ROOTS")
}

/// 上传允许读取的本地根目录。默认空 = **禁止上传**。
///
/// 下载侧限制“写到哪”；上传侧必须限制“能读哪”，否则 agent 可以让 daemon
/// 把 `~/.ssh/id_rsa` 传到任意地址 —— 这是比 SSRF 更严重的一侧。
pub fn upload_roots_from_env() -> Vec<PathBuf> {
    roots_from_env("SKYGET_UPLOAD_ROOTS")
}

fn roots_from_env(var: &str) -> Vec<PathBuf> {
    match std::env::var(var) {
        Ok(raw) => raw
            .split(':')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| PathBuf::from(crate::settings::expand(s)))
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// sha256 的唯一合法形状：64 位十六进制。
///
/// 这不是“好看”：调用方给的 sha 会被拼进 `blobs/<前两位>/<sha>`。
/// 一个绝对路径或 `../` 会让 `Path::join` 指向任意文件；多字节字符还会让
/// `&sha[..2]` 在字符边界中间切片而 panic。所有入口都必须先过这一关。
/// 查询参数名里出现这些词，就把值当凭证处理。
///
/// 预签名 URL（S3 `X-Amz-Signature`、GCS `X-Goog-Signature`、各类 `token=`）
/// 本身就是读写凭证。台账里可以留着真实值给重试用，但**任何对外展示**
/// （API / CLI --json / 事件 / GUI）都不能原样带出去。
const SECRET_QUERY_KEYS: &[&str] = &[
    "signature", "sig", "token", "key", "secret", "password", "passwd",
    "credential", "authorization", "auth", "session", "expires", "x-amz-",
    "x-goog-", "access_key", "api_key",
];

fn key_is_secret(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    SECRET_QUERY_KEYS.iter().any(|needle| k.contains(needle))
}

/// 把 URL 里的签名/令牌类查询参数值替换成 `[redacted]`。
///
/// 合法 URL 走 `url::Url` 解析；解析失败时做一次**按 `&`/`;` 切段的兜底替换**。
/// 不能像以前那样直接原样返回：错误文本里的 URL 常常带尾部标点，解析失败时
/// 整个签名就会被原样写进共享事件流。
pub fn redact_url(raw: &str) -> String {
    if let Ok(mut u) = url::Url::parse(raw) {
        if !u.username().is_empty() {
            let _ = u.set_username("[redacted]");
        }
        if u.password().is_some() {
            let _ = u.set_password(Some("[redacted]"));
        }
        let pairs: Vec<(String, String)> = u
            .query_pairs()
            .map(|(k, v)| {
                if key_is_secret(&k) {
                    (k.into_owned(), "[redacted]".to_string())
                } else {
                    (k.into_owned(), v.into_owned())
                }
            })
            .collect();
        if !pairs.is_empty() {
            u.query_pairs_mut().clear().extend_pairs(pairs);
        }
        return u.to_string();
    }
    redact_url_loose(raw)
}

/// 解析失败时的兜底：只处理 `?query#fragment`，按 `&`/`;` 保留分隔符地替换
/// 敏感 key 的值。非法/畸形 URL 也不会把凭证原样带出去。
fn redact_url_loose(raw: &str) -> String {
    let (body, fragment) = match raw.split_once('#') {
        Some((b, f)) => (b, Some(f)),
        None => (raw, None),
    };
    let Some((head, query)) = body.split_once('?') else {
        return raw.to_string();
    };
    let mut out = String::with_capacity(raw.len());
    out.push_str(head);
    out.push('?');
    let mut start = 0;
    for (i, ch) in query.char_indices() {
        if ch == '&' || ch == ';' {
            out.push_str(&redact_query_segment(&query[start..i]));
            out.push(ch);
            start = i + ch.len_utf8();
        }
    }
    out.push_str(&redact_query_segment(&query[start..]));
    if let Some(f) = fragment {
        out.push('#');
        out.push_str(f);
    }
    out
}

fn redact_query_segment(seg: &str) -> String {
    match seg.split_once('=') {
        Some((k, _)) if key_is_secret(k) => format!("{k}=[redacted]"),
        _ => seg.to_string(),
    }
}

/// 从一段人话/错误文本里找出 http(s) URL 并脱敏。
///
/// 关键点：
/// - scheme 大小写不敏感（`HTTPS://…` 也要认）；
/// - URL 边界只按空白切，再把尾部标点（`),.;` 等）剥下来原样放回；
/// - 路径/查询里的逗号、分号是合法字符，不能像以前那样在 `,`/`;`/`)`
///   处直接截断 —— 截断后的剩余部分会被原样输出，正好漏掉后面的签名。
pub fn redact_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        // to_ascii_lowercase 只改 ASCII 且不改变字节长度，索引可直接复用。
        let lower = rest.to_ascii_lowercase();
        let Some(pos) = lower
            .find("http://")
            .into_iter()
            .chain(lower.find("https://"))
            .min()
        else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        // 空白是主边界；中文全角标点不可能出现在机器生成的预签名 URL 里，
        // 但会出现在“下载失败（…），请重试”这类人话里 —— 必须在这里切断，
        // 否则整段人话会被 URL parser 吞进 query value，一起被替换掉。
        let end = tail
            .find(|c: char| {
                c.is_whitespace()
                    || matches!(
                        c,
                        '）' | '，' | '。' | '；' | '！' | '？' | '（' | '“' | '”'
                    )
            })
            .unwrap_or(tail.len());
        let raw = &tail[..end];
        // 剥离尾部标点；它多半是句子的一部分，不是 URL。
        let mut cut = raw.len();
        let mut suffix = String::new();
        while cut > 0 {
            let ch = raw[..cut].chars().next_back().unwrap();
            if matches!(
                ch,
                ')' | ']' | '}' | '"' | '\'' | ',' | ';' | '.' | '）' | '，' | '。' | '；'
            ) {
                suffix.insert(0, ch);
                cut -= ch.len_utf8();
            } else {
                break;
            }
        }
        out.push_str(&redact_url(&raw[..cut]));
        out.push_str(&suffix);
        rest = &tail[end..];
        if rest.is_empty() {
            break;
        }
    }
    out
}

/// 事件 data 可能是任意 JSON：递归把字符串里的 URL 脱敏。
pub fn redact_json(v: &serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::String(s) => serde_json::Value::String(redact_text(s)),
        serde_json::Value::Array(a) => {
            serde_json::Value::Array(a.iter().map(redact_json).collect())
        }
        serde_json::Value::Object(o) => serde_json::Value::Object(
            o.iter()
                .map(|(k, v)| (k.clone(), redact_json(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

pub fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// 切片流能不能在“限制出网”的策略下跑。
///
/// HLS/DASH 交给 ffmpeg 取子清单和分片，daemon 拦不住它发出的子请求；只要
/// 策略本来在限制出网（严格模式或 host 白名单），默认拒绝，除非部署方显式
/// 用 `SKYGET_ALLOW_STREAM_EGRESS=1` 表示“播放列表可信、这个口子我认”。
pub fn stream_egress_allowed(policy: &Policy, opt_in: bool) -> bool {
    opt_in || (policy.allow_private && policy.allowed_hosts.is_none())
}

/// 检查一个 http(s) 地址能不能出网。
///
/// 返回解析后的 URL；host 会真的做一次 DNS 解析。调用方拿到结果后仍然用原地址
/// 发请求（reqwest 会再解析一次）—— 这里的检查是"挡明显的内网/元数据"，
/// 不是密码学意义上的 SSRF 证明。
pub fn check_url(raw: &str, policy: &Policy) -> Result<Url> {
    let url = Url::parse(raw).map_err(|e| {
        anyhow::anyhow!("地址格式不对：{e}")
    })?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(anyhow::Error::new(Denied {
            code: "input.unsupported",
            why: format!("只给 http/https 直链做边界检查，收到 {}", url.scheme()),
        }));
    }
    // URL 里带账号密码：既容易泄进日志，也不是下载该管的事。
    if !url.username().is_empty() || url.password().is_some() {
        return Err(anyhow::Error::new(Denied {
            code: "policy.credentials_in_url",
            why: "地址里带了账号密码；请用授权头或私有源配置，不要写进 URL。".into(),
        }));
    }
    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("地址里没有主机名：{raw}"))?;

    // IPv6 字面量在 host_str() 里是带方括号的（`[::1]`），白名单和日志里都不该带。
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if let Some(list) = &policy.allowed_hosts {
        if !host_allowed(bare, list) {
            return Err(anyhow::Error::new(Denied {
                code: "policy.host_denied",
                why: format!("主机 {host} 不在白名单里。"),
            }));
        }
    }

    let port = url.port_or_known_default().unwrap_or(443);
    // 字面量 IP 不会走 DNS；域名解析失败按网络错误报，不冒充策略拒绝。
    //
    // 以前这里一律 `(host, port).to_socket_addrs()`：IPv6 字面量的 host 带方括号
    // （`[::1]`），解析必然失败 —— 于是**任何 IPv6 地址都被当成「域名解析失败」拒掉**。
    let addrs: Vec<IpAddr> = match url.host() {
        Some(url::Host::Ipv4(ip)) => vec![IpAddr::V4(ip)],
        Some(url::Host::Ipv6(ip)) => vec![IpAddr::V6(ip)],
        Some(url::Host::Domain(d)) => match (d, port).to_socket_addrs() {
            Ok(it) => it.map(|s| s.ip()).collect(),
            Err(e) => bail!("域名解析失败（{d}）：{e}"),
        },
        None => bail!("地址里没有主机名：{raw}"),
    };
    if addrs.is_empty() {
        bail!("域名解析没有结果（{bare}）");
    }
    for ip in &addrs {
        // 元数据地址永远单独判定：`SKYGET_ALLOW_PRIVATE=1` 是为了公司内网镜像，
        // 不是为了把云凭证也一起放开。
        if !policy.allow_metadata && ip_is_metadata(*ip) {
            return Err(anyhow::Error::new(Denied {
                code: "policy.metadata_denied",
                why: format!(
                    "地址 {raw} 解析到云元数据地址 {ip}，已拒绝（要开得显式设 SKYGET_ALLOW_METADATA=1）。"
                ),
            }));
        }
        if !policy.allow_private && ip_is_private(*ip) {
            return Err(anyhow::Error::new(Denied {
                code: "policy.egress_denied",
                why: format!(
                    "地址 {raw} 解析到非公网地址 {ip}（私有/回环/链路本地/元数据），已拒绝。"
                ),
            }));
        }
    }
    Ok(url)
}

fn host_allowed(host: &str, list: &[String]) -> bool {
    let host = host.to_ascii_lowercase();
    list.iter().any(|pat| {
        if let Some(suffix) = pat.strip_prefix("*.") {
            host == suffix || host.ends_with(&format!(".{suffix}"))
        } else {
            host == *pat
        }
    })
}

/// 云元数据地址。和 `ip_is_private` 分开：那个管"内网能不能出"，
/// 这个管"能不能读云凭证" —— 两件事的风险差一个数量级。
pub fn ip_is_metadata(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            // AWS / GCP / Azure / 阿里云
            v4 == std::net::Ipv4Addr::new(169, 254, 169, 254)
                || v4 == std::net::Ipv4Addr::new(100, 100, 100, 200)
        }
        IpAddr::V6(v6) => {
            // AWS 的 IPv6 元数据端点 fd00:ec2::254（第二个 16 位组是 0x0ec2）
            v6.segments()[0] == 0xfd00
                && v6.segments()[1] == 0x0ec2
                && v6.segments()[2..7].iter().all(|g| *g == 0)
                && v6.segments()[7] == 0x0254
        }
    }
}

/// 这个 IP 是不是"不该让 agent 直接够到"的地址。
///
/// 除了标准私有段，还封了：
/// - 100.64.0.0/10（CGNAT；阿里云元数据 100.100.100.200 在这里）
/// - 169.254.0.0/16（链路本地；AWS/GCP 元数据 169.254.169.254 在这里）
/// - 0.0.0.0/8、240.0.0.0/4、组播、文档/基准测试段
/// - IPv6 的 ::1、fc00::/7、fe80::/10、ff00::/8，以及 IPv4-mapped 地址
pub fn ip_is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => ipv4_is_private(v4),
        IpAddr::V6(v6) => ipv6_is_private(v6),
    }
}

fn ipv4_is_private(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast()
        || o[0] == 0
        || o[0] >= 240
        // 100.64.0.0/10（CGNAT）
        || (o[0] == 100 && (64..=127).contains(&o[1]))
        // 198.18.0.0/15（benchmark）
        || (o[0] == 198 && (o[1] == 18 || o[1] == 19))
        // 192.0.0.0/24、192.0.2.0/24、198.51.100.0/24、203.0.113.0/24（特殊/文档）
        || (o[0] == 192 && o[1] == 0 && (o[2] == 0 || o[2] == 2))
        || (o[0] == 198 && o[1] == 51 && o[2] == 100)
        || (o[0] == 203 && o[1] == 0 && o[2] == 113)
}

fn ipv6_is_private(ip: Ipv6Addr) -> bool {
    if ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() {
        return true;
    }
    let s = ip.segments();
    // fc00::/7
    if (s[0] & 0xfe00) == 0xfc00 {
        return true;
    }
    // fe80::/10
    if (s[0] & 0xffc0) == 0xfe80 {
        return true;
    }
    // 2001:db8::/32（文档）
    if s[0] == 0x2001 && s[1] == 0x0db8 {
        return true;
    }
    // 64:ff9b::/96（NAT64 到内网/元数据）
    if s[0] == 0x0064 && s[1] == 0xff9b {
        return true;
    }
    // 2002::/16（6to4，可编码任意 IPv4）
    if s[0] == 0x2002 {
        return true;
    }
    // 2001::/32（Teredo）
    if s[0] == 0x2001 && s[1] == 0 {
        return true;
    }
    // fec0::/10（已废弃的 site-local）
    if (s[0] & 0xffc0) == 0xfec0 {
        return true;
    }
    // ::a.b.c.d（IPv4-compatible）：前 6 段全 0 且不是 :: / ::1
    if s[..6].iter().all(|x| *x == 0) && (s[6] != 0 || s[7] != 0) {
        return true;
    }
    // IPv4-mapped：::ffff:a.b.c.d 按 IPv4 再判一遍
    if let Some(v4) = ip.to_ipv4_mapped() {
        return ipv4_is_private(v4);
    }
    false
}

/// 检查落地路径在不在允许的根目录里。
///
/// 空 `task.dest` 时用默认落地目录。`dest_roots` 为空 = 不限制（兼容旧行为）。
/// 检查会先把路径里**已存在的最深祖先** canonicalize，再拼回不存在的尾部 ——
/// 这样 `~/Downloads/link-to-etc/passwd` 这种符号链接逃逸也能被发现。
pub fn check_dest(dest: &str, policy: &Policy) -> Result<PathBuf> {
    let raw = if dest.trim().is_empty() {
        crate::settings::dest_dir().display().to_string()
    } else {
        dest.to_string()
    };
    let path = PathBuf::from(crate::settings::expand(&raw));
    check_dest_path(&path, &policy.dest_roots)?;
    Ok(path)
}

fn check_dest_path(path: &Path, roots: &[PathBuf]) -> Result<()> {
    if roots.is_empty() {
        return Ok(());
    }
    // 悬空符号链接 exists()=false，会被 resolve_existing_ancestor 当成“不存在的尾巴”；
    // 落地时 fs::copy 却会跟随它写到根外。这里先把悬空链接和最终文件链接挡住。
    reject_dangling_symlinks(path)?;
    if let Ok(m) = std::fs::symlink_metadata(path) {
        if m.file_type().is_symlink() {
            return Err(anyhow::Error::new(Denied {
                code: "policy.dest_denied",
                why: format!("目标本身是符号链接：{}", path.display()),
            }));
        }
    }
    let resolved = resolve_existing_ancestor(path);
    let ok = roots.iter().any(|root| {
        let root = resolve_existing_ancestor(root);
        resolved.starts_with(&root)
    });
    if !ok {
        return Err(anyhow::Error::new(Denied {
            code: "policy.dest_denied",
            why: format!(
                "目标路径 {} 不在允许的目录里（{}）。",
                path.display(),
                roots
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(" · ")
            ),
        }));
    }
    Ok(())
}

/// 检查上传源文件：必须是允许根目录内的普通文件，不能是符号链接。
pub fn check_upload_source(path: &str, roots: &[PathBuf]) -> Result<PathBuf> {
    let p = PathBuf::from(crate::settings::expand(path));
    if roots.is_empty() {
        return Err(anyhow::Error::new(Denied {
            code: "policy.upload_source_denied",
            why: "上传默认关闭：没有配置 SKYGET_UPLOAD_ROOTS。".into(),
        }));
    }
    reject_dangling_symlinks(&p)?;
    let meta = std::fs::symlink_metadata(&p).map_err(|e| {
        anyhow::Error::new(Denied {
            code: "policy.upload_source_denied",
            why: format!("读不到源文件 {}：{e}", p.display()),
        })
    })?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Err(anyhow::Error::new(Denied {
            code: "policy.upload_source_denied",
            why: format!("源文件不是普通文件：{}", p.display()),
        }));
    }
    let resolved = resolve_existing_ancestor(&p);
    let ok = roots.iter().any(|root| {
        let root = resolve_existing_ancestor(root);
        resolved.starts_with(&root)
    });
    if !ok {
        return Err(anyhow::Error::new(Denied {
            code: "policy.upload_source_denied",
            why: format!(
                "源文件 {} 不在允许上传的目录里（{}）。",
                p.display(),
                roots
                    .iter()
                    .map(|x| x.display().to_string())
                    .collect::<Vec<_>>()
                    .join(" · ")
            ),
        }));
    }
    Ok(p)
}

/// 路径里任何一段是**悬空**符号链接都拒绝；指向存在的目标的链接交给
/// `resolve_existing_ancestor` 做 canonicalize 后再比较根目录。
fn reject_dangling_symlinks(path: &Path) -> Result<()> {
    let mut cur = PathBuf::new();
    for comp in path.components() {
        cur.push(comp.as_os_str());
        if let Ok(m) = std::fs::symlink_metadata(&cur) {
            if m.file_type().is_symlink() && std::fs::canonicalize(&cur).is_err() {
                return Err(anyhow::Error::new(Denied {
                    code: "policy.dest_denied",
                    why: format!("路径里有悬空符号链接：{}", cur.display()),
                }));
            }
        }
    }
    Ok(())
}

/// 把路径规整到"绝对、无 `.`/`..`"，再解析已存在的最深祖先（跟随符号链接）。
fn resolve_existing_ancestor(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        normalize_lexical(path)
    } else {
        normalize_lexical(&crate::paths::home_dir().join(path))
    };
    let mut existing = absolute.clone();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    while !existing.exists() {
        match existing.file_name() {
            Some(name) => tail.push(name.to_os_string()),
            None => break,
        }
        if !existing.pop() {
            break;
        }
    }
    let mut base = std::fs::canonicalize(&existing).unwrap_or(existing);
    for name in tail.into_iter().rev() {
        base.push(name);
    }
    base
}

fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                // 不越过根：/../etc 仍是 /etc，而不是逃到别处
                if !out.pop() {
                    out.push("/");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push("/");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn strict() -> Policy {
        Policy {
            allow_private: false,
            allow_metadata: false,
            allowed_hosts: None,
            dest_roots: Vec::new(),
            upload_roots: Vec::new(),
            max_redirects: 5,
        }
    }

    #[test]
    fn sha256_must_be_exactly_64_hex() {
        assert!(is_sha256_hex(&"ab".repeat(32)));
        assert!(!is_sha256_hex(""));
        assert!(!is_sha256_hex("abc"));
        assert!(!is_sha256_hex("../etc/passwd"));
        assert!(!is_sha256_hex(&"中".repeat(32)));
        assert!(!is_sha256_hex(&format!("/{}", "ab".repeat(31))));
    }

    #[test]
    fn stream_egress_is_fail_closed_under_restrictions() {
        let mut open = Policy::default();
        assert!(stream_egress_allowed(&open, false));
        assert!(stream_egress_allowed(&open, true));

        assert!(!stream_egress_allowed(&strict(), false));
        assert!(stream_egress_allowed(&strict(), true));

        open.allowed_hosts = Some(vec!["example.com".into()]);
        assert!(
            !stream_egress_allowed(&open, false),
            "配了 host 白名单也不能让 ffmpeg 绕过它"
        );
    }

    #[test]
    fn private_and_metadata_addresses_are_rejected() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.100.100.200",
            "0.0.0.0",
            "::1",
            "fc00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "64:ff9b::a9fe:a9fe",
            "2002:7f00:1::",
            "2001::1",
            "fec0::1",
            "::127.0.0.1",
        ] {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(ip_is_private(ip), "{ip} 应该被当成非公网");
        }
        for ip in ["1.1.1.1", "8.8.8.8", "2606:4700:4700::1111"] {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(!ip_is_private(ip), "{ip} 是公网，不该误杀");
        }
    }

    #[test]
    fn strict_policy_blocks_loopback_and_metadata_but_allows_public_ip() {
        let p = strict();
        let e = check_url("http://127.0.0.1:7788/x", &p).unwrap_err();
        assert_eq!(e.downcast_ref::<Denied>().unwrap().code, "policy.egress_denied");

        // 元数据单独归一类：它和「内网能不能出」不是一件事。
        let e = check_url("http://169.254.169.254/latest/meta-data/", &p).unwrap_err();
        assert_eq!(e.downcast_ref::<Denied>().unwrap().code, "policy.metadata_denied");

        // 字面量公网 IP 不查 DNS，测试不依赖外网。
        assert!(check_url("https://1.1.1.1/x", &p).is_ok());
    }

    #[test]
    fn ipv6_literals_are_parsed_not_mistaken_for_dns_failures() {
        let strict_p = strict();
        // ::1 → 私有，拒绝码要指到策略，不是「域名解析失败」
        let e = check_url("http://[::1]:7788/x", &strict_p).unwrap_err();
        assert_eq!(e.downcast_ref::<Denied>().unwrap().code, "policy.egress_denied");

        // 公网 IPv6 字面量：不查 DNS，直接放行（测试不依赖外网）
        assert!(check_url("https://[2606:4700:4700::1111]/x", &strict_p).is_ok());

        // AWS 的 IPv6 元数据端点
        let e = check_url("http://[fd00:ec2::254]/latest/meta-data/", &strict_p).unwrap_err();
        assert_eq!(e.downcast_ref::<Denied>().unwrap().code, "policy.metadata_denied");
    }

    #[test]
    fn allowing_private_does_not_also_open_cloud_metadata() {
        // 一个只想下内网镜像的部署，不该顺带把「agent 能读云凭证」打开。
        let p = Policy {
            allow_private: true,
            allow_metadata: false,
            ..Policy::default()
        };
        for url in [
            "http://169.254.169.254/latest/meta-data/",
            "http://100.100.100.200/latest/meta-data/",
            "http://[fd00:ec2::254]/latest/meta-data/",
        ] {
            let e = check_url(url, &p).unwrap_err();
            assert_eq!(
                e.downcast_ref::<Denied>().unwrap().code,
                "policy.metadata_denied",
                "{url} 应该被拦"
            );
        }
        // 内网镜像仍然放行（这是 allow_private 的正当用途）。
        assert!(check_url("http://10.0.0.5/x", &p).is_ok());
        // 显式打开才放行。
        let p2 = Policy {
            allow_metadata: true,
            ..p
        };
        assert!(check_url("http://169.254.169.254/x", &p2).is_ok());
    }

    #[test]
    fn permissive_policy_keeps_local_development_working() {
        let p = Policy::default();
        assert!(check_url("http://127.0.0.1:1234/x", &p).is_ok());
    }

    #[test]
    fn host_allowlist_matches_exact_and_wildcard() {
        let p = strict();
        let list = vec!["example.org".to_string(), "*.allowed.net".to_string()];
        assert!(host_allowed("example.org", &list));
        assert!(host_allowed("a.allowed.net", &list));
        assert!(host_allowed("allowed.net", &list));
        assert!(!host_allowed("evil.net", &list));
        assert!(!host_allowed("example.org.evil.net", &list));
        // host 规则本身用纯函数测，避免 DNS
        assert!(p.allowed_hosts.is_none());
    }

    #[test]
    fn credentials_in_url_are_rejected() {
        let p = Policy::default();
        let e = check_url("https://user:pass@1.1.1.1/x", &p).unwrap_err();
        assert_eq!(e.downcast_ref::<Denied>().unwrap().code, "policy.credentials_in_url");
    }

    #[test]
    fn upload_sources_are_confined_and_must_be_regular_files() {
        let base = std::env::temp_dir().join(format!("skyget-upload-{}", std::process::id()));
        let root = base.join("allowed");
        let outside = base.join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let ok = root.join("ok.bin");
        fs::write(&ok, b"hello").unwrap();
        let secret = outside.join("secret.bin");
        fs::write(&secret, b"secret").unwrap();
        let roots = vec![root.clone()];

        let e = check_upload_source(&ok.display().to_string(), &[]).unwrap_err();
        assert_eq!(
            e.downcast_ref::<Denied>().unwrap().code,
            "policy.upload_source_denied",
            "默认必须禁止上传"
        );
        check_upload_source(&ok.display().to_string(), &roots).unwrap();
        let e = check_upload_source(&secret.display().to_string(), &roots).unwrap_err();
        assert_eq!(e.downcast_ref::<Denied>().unwrap().code, "policy.upload_source_denied");
        #[cfg(unix)]
        {
            let link = root.join("link.bin");
            std::os::unix::fs::symlink(&secret, &link).unwrap();
            let e = check_upload_source(&link.display().to_string(), &roots).unwrap_err();
            assert_eq!(e.downcast_ref::<Denied>().unwrap().code, "policy.upload_source_denied");
        }
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn presigned_urls_are_redacted_but_plain_urls_are_left_alone() {
        let signed = "https://bucket.s3.amazonaws.com/m.bin?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=AKIA%2F20261030&X-Amz-Signature=deadbeef&X-Amz-Expires=900&part=1";
        let out = redact_url(signed);
        assert!(!out.contains("deadbeef"), "{out}");
        assert!(!out.contains("AKIA"), "{out}");
        assert!(out.to_ascii_lowercase().contains("redacted"), "{out}");
        // 非敏感参数保持可读
        assert!(out.contains("part=1"), "{out}");

        let plain = "https://example.org/a.bin?part=2&mirror=eu";
        assert_eq!(redact_url(plain), plain);

        // 错误文本里的 URL 也要脱敏，其它文字保留。
        let text = format!("下载失败：请求 {signed} 超时");
        let safe = redact_text(&text);
        assert!(safe.starts_with("下载失败：请求 https://"), "{safe}");
        assert!(!safe.contains("deadbeef"), "{safe}");
        assert!(safe.ends_with("超时"), "{safe}");

        // 事件是任意 JSON：字符串递归处理。
        let j = serde_json::json!({"error": text, "n": 1, "list": [signed]});
        let s = redact_json(&j).to_string();
        assert!(!s.contains("deadbeef"), "{s}");
        assert!(s.contains("\"n\":1"), "{s}");

        // 路径/查询里合法的逗号、分号不能导致截断后把签名漏出去。
        let comma = "https://bucket/path,movie.mkv?X-Amz-Signature=cafebabe&X-Amz-SignedHeaders=host;range";
        let prose = format!("下载失败（{comma}），请重试。");
        let safe = redact_text(&prose);
        assert!(!safe.contains("cafebabe"), "{safe}");
        assert!(!safe.contains("host;range"), "敏感值没替换：{safe}");
        assert!(safe.contains("path,movie.mkv"), "URL 非敏感部分不该丢：{safe}");
        assert!(safe.ends_with("），请重试。"), "尾部标点/人话被吃掉：{safe}");

        // scheme 大小写不敏感。
        let upper = "HTTPS://B.S3/X?X-Amz-Signature=upper-secret";
        let safe = redact_text(&format!("请求 {upper} 失败"));
        assert!(!safe.contains("upper-secret"), "{safe}");
        assert!(safe.contains("[redacted]") || safe.contains("%5Bredacted%5D"), "{safe}");

        // 畸形 URL（尾部多余标点）也必须走兜底，而不是原样返回。
        let broken = "https://x/y?token=abc;X-Amz-Signature=def).";
        let safe = redact_text(broken);
        assert!(!safe.contains("abc"), "{safe}");
        assert!(!safe.contains("def"), "{safe}");
        assert!(safe.to_ascii_lowercase().contains("redacted"), "{safe}");
    }

    #[test]
    fn dest_roots_confine_landing_and_block_dotdot_and_symlink_escape() {
        let base = std::env::temp_dir().join(format!("skyget-dest-{}", std::process::id()));
        let root = base.join("allowed");
        let outside = base.join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();

        let roots = vec![root.clone()];
        check_dest_path(&root.join("a/b.bin"), &roots).unwrap();
        let e = check_dest_path(&outside.join("x.bin"), &roots).unwrap_err();
        assert_eq!(e.downcast_ref::<Denied>().unwrap().code, "policy.dest_denied");
        let e = check_dest_path(&root.join("../outside/x.bin"), &roots).unwrap_err();
        assert_eq!(e.downcast_ref::<Denied>().unwrap().code, "policy.dest_denied");

        #[cfg(unix)]
        {
            let link = root.join("escape");
            std::os::unix::fs::symlink(&outside, &link).unwrap();
            let e = check_dest_path(&link.join("x.bin"), &roots).unwrap_err();
            assert_eq!(e.downcast_ref::<Denied>().unwrap().code, "policy.dest_denied");

            // 悬空符号链接：exists()=false，但 copy 会跟着它写。
            let dangling = root.join("dangling.bin");
            std::os::unix::fs::symlink(outside.join("not-there.bin"), &dangling).unwrap();
            let e = check_dest_path(&dangling, &roots).unwrap_err();
            assert_eq!(e.downcast_ref::<Denied>().unwrap().code, "policy.dest_denied");

            // 最终文件即使是有效链接也拒绝：绝不写穿链接目标。
            let target = root.join("inside.bin");
            fs::write(&target, b"x").unwrap();
            let valid = root.join("valid-link.bin");
            std::os::unix::fs::symlink(&target, &valid).unwrap();
            let e = check_dest_path(&valid, &roots).unwrap_err();
            assert_eq!(e.downcast_ref::<Denied>().unwrap().code, "policy.dest_denied");
        }

        let _ = fs::remove_dir_all(&base);
    }
}
