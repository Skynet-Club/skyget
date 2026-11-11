//! Unix domain socket：同机身份，不用分发 secret。
//!
//! 守护进程原来只有 `127.0.0.1:7788` 这一条路，身份靠 `SKYGET_AGENTS` 发的
//! bearer token。在单机多 agent 的场景里这有两个毛病：
//!
//! 1. **token 要走环境变量** —— 每个容器、每个沙箱都得灌一份，灌漏了就退回“无鉴权”；
//! 2. **同 uid 的进程本来就分不开** —— 「谁在调我」只能靠对方自称。
//!
//! Unix socket 把身份交给内核：`SO_PEERCRED` 拿到的是**对端真实 uid/gid/pid**，
//! 没法伪造。于是：
//!
//! - 同 uid（daemon 自己的用户）→ 等价于主 token，能看全部（和以前一样）；
//! - 其它 uid 必须在 `SKYGET_UDS_AGENTS` 里登记 → 得到 `agent:<name>` 身份，
//!   配额/策略/任务隔离全部照旧生效；
//! - 没登记的 uid 直接 401，不需要 token 也不需要猜。
//!
//! socket 文件的权限交给部署方：daemon 建成 `0660`，组决定“谁能连”。
//!
//! ## 客户端
//!
//! `SKYGET_API=unix:/path/to/daemon.sock` 时，CLI/MCP 在本进程里起一个**回环转发**
//! （只用 `127.0.0.1`，随机端口 + 随机口令头），把请求原样转给 socket。
//! 这样做的原因很实在：内核里只有一份 HTTP 处理逻辑，客户端也只有一份（reqwest），
//! 不必为一个传输层再写一个 HTTP 解析器去伺候长轮询和流式响应。
//! 转发线程活在本进程里，所以 daemon 看到的对端 uid 仍然是**调用方自己的 uid**——
//! 身份没有被中转放大。别的本机进程要借用这个端口，得先猜中 128 位口令。

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// 对端身份。uid/gid 来自内核，pid 在 Linux 上由内核给（macOS 给不出来）。
#[derive(Debug, Clone, Copy)]
pub struct Peer {
    pub uid: u32,
    pub gid: u32,
    pub pid: i32,
}

/// 读一个已建立连接的 Unix socket 的对端身份。
///
/// `UnixStream::peer_cred()` 到 1.8x 还是 unstable（`peer_credentials_unix_socket`），
/// 所以这里直接调系统接口：
/// - Linux：`getsockopt(SOL_SOCKET, SO_PEERCRED)` 拿到 pid/uid/gid；
/// - macOS / BSD：`getpeereid()` 只有 uid/gid（没有对端 pid，pid 记 0）；
/// - 其它平台：拿不到 → `None`，调用方按「没有身份」处理（必须带 token）。
///
/// 拿不到就必须当不可信：把「不知道你是谁」当成可信，比没有这个功能更糟。
pub fn peer_of(stream: &std::os::unix::net::UnixStream) -> Option<Peer> {
    use std::os::fd::AsRawFd;
    imp(stream.as_raw_fd())
}

#[cfg(target_os = "linux")]
fn imp(fd: std::os::fd::RawFd) -> Option<Peer> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if rc != 0 || len as usize != std::mem::size_of::<libc::ucred>() {
        return None;
    }
    Some(Peer {
        uid: cred.uid,
        gid: cred.gid,
        pid: cred.pid,
    })
}

#[cfg(target_os = "macos")]
fn imp(fd: std::os::fd::RawFd) -> Option<Peer> {
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    if unsafe { libc::getpeereid(fd, &mut uid, &mut gid) } != 0 {
        return None;
    }
    Some(Peer { uid, gid, pid: 0 })
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn imp(_fd: std::os::fd::RawFd) -> Option<Peer> {
    // 其它 unix 没有稳定的对端身份接口：返回 None，调用方按「没有身份」处理
    // （也就是必须带 token）。宁可不给身份，也不能编一个出来。
    None
}

/// `SKYGET_UDS_AGENTS="1001=claude,1002=codex"`（uid 或 gid 都能当键）。
///
/// 只按 uid/gid 映射，不看 pid、不看进程名：pid 会被复用，进程名可以改。
pub fn agent_for(peer: Peer) -> Option<String> {
    agent_for_spec(peer, &std::env::var("SKYGET_UDS_AGENTS").ok()?)
}

/// 纯解析版：不读环境变量，测试可以并行跑。
fn agent_for_spec(peer: Peer, spec: &str) -> Option<String> {
    for item in spec.split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let Some((key, name)) = item.split_once('=') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let key = key
            .trim()
            .trim_start_matches("uid:")
            .trim_start_matches("gid:");
        if let Ok(n) = key.parse::<u32>() {
            if n == peer.uid || n == peer.gid {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// socket 放哪。
///
/// 默认 `$SKYGET_HOME/daemon.sock`。`SKYGET_API=unix:/path` 时用那个路径
/// （客户端和 daemon 必须指同一个）。
pub fn socket_path() -> PathBuf {
    if let Some(p) = std::env::var("SKYGET_API")
        .ok()
        .and_then(|v| v.strip_prefix("unix:").map(|s| s.to_string()))
        .filter(|s| !s.is_empty())
    {
        return PathBuf::from(p);
    }
    crate::paths::data_dir().join("daemon.sock")
}

/// socket 文件存在不等于服务可用：崩溃和 `kill -9` 都会留下一个连不上的路径。
pub fn socket_live() -> bool {
    std::os::unix::net::UnixStream::connect(socket_path()).is_ok()
}

/// 客户端：`SKYGET_API` 指向 unix socket 时，起一个进程内回环转发并返回 base URL。
///
/// 返回的就是个 `http://127.0.0.1:<port>`，现有客户端（reqwest）一行都不用改。
/// 不是 socket 就原样返回环境变量里的值。
pub fn base_url() -> String {
    let raw = std::env::var("SKYGET_API").unwrap_or_else(|_| "http://127.0.0.1:7788".to_string());
    let Some(path) = raw.strip_prefix("unix:").map(|s| s.to_string()) else {
        return raw;
    };
    static URL: OnceLock<String> = OnceLock::new();
    URL.get_or_init(|| {
        match start_relay(PathBuf::from(path)) {
            Ok(url) => url,
            Err(e) => {
                // 起不来就走一个不存在的端口：调用方会得到“连不上”，
                // 这比悄悄回落到 TCP（可能连到另一个 daemon）诚实。
                eprintln!("连不上 unix socket 转发：{e:#}");
                "http://127.0.0.1:1".to_string()
            }
        }
    })
    .clone()
}

fn start_relay(sock: PathBuf) -> std::io::Result<String> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    // 每进程随机口令：别的本机进程就算连上这个端口，也得先猜中它。
    let mut rnd = [0u8; 16];
    getrandom::getrandom(&mut rnd).map_err(|e| std::io::Error::other(e.to_string()))?;
    let token: String = rnd.iter().map(|b| format!("{b:02x}")).collect();
    let token_for_url = token.clone();

    std::thread::Builder::new()
        .name("uds-relay".into())
        .spawn(move || {
            for client in listener.incoming().flatten() {
                let sock = sock.clone();
                let token = token.clone();
                let _ = std::thread::Builder::new()
                    .stack_size(128 * 1024)
                    .spawn(move || relay_one(client, &sock, &token));
            }
        })?;
    // 口令放在路径前缀里（而不是自定义头）：所有调用方都只是 `format!("{base}{path}")`，
    // 于是 reqwest / doctor / statusline / MCP 一行都不用改。
    Ok(format!("http://127.0.0.1:{port}/{token_for_url}"))
}

/// 一条连接：先读请求头、核对路径前缀里的口令，再把两个方向原样对接。
///
/// 只解析**请求行**（第一行），剩下一个字节都不碰：长轮询、Range 响应、keep-alive
/// 全都自动成立，不需要在这里重新实现一遍 HTTP 的边界情况。
fn relay_one(mut client: std::net::TcpStream, sock: &Path, token: &str) {
    let _ = client.set_read_timeout(Some(std::time::Duration::from_secs(45)));
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    // 读到空行为止；最多 8KB，防止被无头请求撑爆内存。
    while head.len() < 8192 {
        match client.read(&mut byte) {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                head.push(byte[0]);
                if head.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
        }
    }
    let Some(first_end) = head.windows(2).position(|w| w == b"\r\n") else {
        return;
    };
    let line = String::from_utf8_lossy(&head[..first_end]).to_string();
    let mut parts = line.split_whitespace();
    let (Some(method), Some(target), Some(ver)) = (parts.next(), parts.next(), parts.next()) else {
        return;
    };
    let prefix = format!("/{token}");
    let Some(rest) = target.strip_prefix(&prefix) else {
        reject(&mut client, 403, "relay token mismatch", "auth.relay");
        return;
    };
    if !(rest.is_empty() || rest.starts_with('/') || rest.starts_with('?')) {
        reject(&mut client, 403, "relay token mismatch", "auth.relay");
        return;
    }
    let path = if rest.is_empty() {
        "/".to_string()
    } else if rest.starts_with('?') {
        format!("/{rest}")
    } else {
        rest.to_string()
    };
    let mut forwarded = format!("{method} {path} {ver}\r\n").into_bytes();
    forwarded.extend_from_slice(&head[first_end + 2..]);

    let Ok(mut upstream) = std::os::unix::net::UnixStream::connect(sock) else {
        reject(
            &mut client,
            502,
            "daemon socket unreachable",
            "daemon.unreachable",
        );
        return;
    };
    let _ = upstream.set_read_timeout(Some(std::time::Duration::from_secs(45)));
    if upstream.write_all(&forwarded).is_err() {
        return;
    }
    let mut up_read = match client.try_clone() {
        Ok(c) => c,
        Err(_) => return,
    };
    let mut up_write = match upstream.try_clone() {
        Ok(u) => u,
        Err(_) => return,
    };
    // 上行：客户端 → socket（线程）；下行：socket → 客户端（本线程）。
    let _ = std::thread::Builder::new()
        .stack_size(64 * 1024)
        .spawn(move || {
            let _ = std::io::copy(&mut up_read, &mut up_write);
        });
    let _ = std::io::copy(&mut upstream, &mut client);
}

fn reject(client: &mut std::net::TcpStream, code: u16, msg: &str, kind: &str) {
    let body = serde_json::json!({"error": msg, "code": kind}).to_string();
    let reason = if code == 403 {
        "Forbidden"
    } else {
        "Bad Gateway"
    };
    let _ = client.write_all(
        format!(
            "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .as_bytes(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(spec: &str) -> Option<String> {
        agent_for_spec(
            Peer {
                uid: 1001,
                gid: 1001,
                pid: 42,
            },
            spec,
        )
    }

    #[test]
    fn maps_uid_and_gid_but_ignores_junk() {
        assert_eq!(parse("1001=claude").as_deref(), Some("claude"));
        assert_eq!(parse("uid:1001=claude").as_deref(), Some("claude"));
        assert_eq!(parse("2000=other,1001=claude").as_deref(), Some("claude"));
        // 组 id 也算：容器共享一个组是常见做法。
        assert_eq!(parse("1001=claude").as_deref(), Some("claude"));
        assert_eq!(parse("2000=other").as_deref(), None);
        assert_eq!(parse("1001=").as_deref(), None);
        assert_eq!(parse("垃圾").as_deref(), None);
        assert_eq!(parse("").as_deref(), None);
    }

    #[test]
    fn peer_cred_works_on_a_real_socket_pair() {
        // 这是整条 UDS 身份链的地基：内核要真的把对端 uid 交给我们。
        // 拿不到就得返回 None（调用方按「没有身份」处理），绝不能编一个出来。
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        let peer = peer_of(&b).expect("socketpair 的 peer cred 应该拿得到");
        assert_eq!(
            peer.uid,
            unsafe { libc::getuid() },
            "uid 必须是真实的调用者"
        );
        assert_eq!(peer.gid, unsafe { libc::getgid() });
        // 写一句话，确认这对 socket 是真的连着（不是构造出来的假对象）。
        (&a).write_all(b"x").unwrap();
        let mut one = [0u8; 1];
        (&b).read_exact(&mut one).unwrap();
        assert_eq!(one[0], b'x');
    }

    #[test]
    fn a_missing_registry_means_no_identity() {
        std::env::remove_var("SKYGET_UDS_AGENTS");
        assert!(agent_for(Peer {
            uid: 1001,
            gid: 1001,
            pid: 1
        })
        .is_none());
    }
}
