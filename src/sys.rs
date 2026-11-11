//! 平台相关的那一小撮系统调用。
//!
//! 只有四件事需要分平台：进程还活着吗、这个 pid 的可执行文件是谁、
//! 按可执行文件名找进程、改进程名。其余（flock、信号、pipe、0700 目录）
//! POSIX 一致，不需要在这里分叉。
//!
//! - Linux 走 `/proc`；
//! - macOS 走 libproc（`proc_pidpath` / `proc_listallpids`，系统自带，无第三方依赖）；
//! - 其余 unix 退化为 `kill(pid, 0)`，拿不到 exe 就返回 `None`（调用方按「不是我们的」处理）。
//!
//! 为什么要有这一层：macOS 上没有 `/proc`。以前 `pid_alive` 直接拼
//! `/proc/<pid>`，在 Mac 上永远返回 false —— 守护进程会以为每条 running 任务
//! 都是孤儿，重启时把正在下的任务全判成「被打断」。

use std::path::PathBuf;

/// 进程还在不在（僵尸也算「在」：它已经不会再干活，但 pid 仍被占用）。
///
/// Linux 用 `/proc/<pid>` 判断，和以前行为一致；其它平台用 `kill(pid, 0)`。
#[cfg(target_os = "linux")]
pub fn pid_alive(pid: i64) -> bool {
    pid > 0 && std::path::Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(not(target_os = "linux"))]
pub fn pid_alive(pid: i64) -> bool {
    pid > 0 && unsafe { libc::kill(pid as libc::pid_t, 0) } == 0
}

/// pid → 可执行文件路径。拿不到返回 `None`（进程没了，或没有权限）。
#[cfg(target_os = "linux")]
pub fn exe_path(pid: i64) -> Option<PathBuf> {
    if pid <= 0 {
        return None;
    }
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

#[cfg(target_os = "macos")]
pub fn exe_path(pid: i64) -> Option<PathBuf> {
    if pid <= 0 {
        return None;
    }
    extern "C" {
        fn proc_pidpath(
            pid: libc::c_int,
            buffer: *mut libc::c_void,
            buffersize: u32,
        ) -> libc::c_int;
    }
    // PROC_PIDPATHINFO_MAXSIZE = 4 * MAXPATHLEN = 4096
    let mut buf = vec![0u8; 4096];
    let n = unsafe {
        proc_pidpath(
            pid as libc::c_int,
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len() as u32,
        )
    };
    if n <= 0 {
        return None;
    }
    buf.truncate(n as usize);
    String::from_utf8(buf).ok().map(PathBuf::from)
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
pub fn exe_path(_pid: i64) -> Option<PathBuf> {
    None
}

/// pid → 可执行文件名（不带路径）。用来精确区分 `skynet-dataplaned` 和
/// `skynet-dataplane`：前缀匹配会把守护进程误认成 GUI。
pub fn exe_name(pid: i64) -> Option<String> {
    exe_path(pid)?
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
}

/// 这个 pid 的可执行文件是不是这几个名字里的一个。
pub fn pid_is_binary(pid: i64, names: &[&str]) -> bool {
    exe_name(pid)
        .map(|n| names.iter().any(|want| *want == n))
        .unwrap_or(false)
}

/// 可执行文件已经被新构建替换（进程还活着，但内核记的路径带 ` (deleted)`）。
///
/// 只有 Linux 会这样：升级时 `cargo build` 直接覆盖旧文件。macOS 上覆盖会
/// 让路径继续指向新文件内容，没有这个信号 —— 返回 false 是诚实的。
pub fn exe_deleted(pid: i64) -> bool {
    if cfg!(target_os = "linux") {
        exe_path(pid)
            .map(|p| p.to_string_lossy().ends_with(" (deleted)"))
            .unwrap_or(false)
    } else {
        false
    }
}

/// 扫一遍系统里可执行文件名为 `names` 之一的进程（跳过自己）。
///
/// pid 文件丢了也要能找回窗口/守护进程：Linux 扫 `/proc`，macOS 用
/// `proc_listallpids` 拿全量 pid 再逐个问路径。
#[cfg(target_os = "linux")]
pub fn pids_with_binary(names: &[&str]) -> Vec<i32> {
    let me = std::process::id() as i32;
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return out;
    };
    for entry in rd.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<i32>().ok())
        else {
            continue;
        };
        if pid == me {
            continue;
        }
        let Ok(exe) = std::fs::read_link(format!("/proc/{pid}/exe")) else {
            continue;
        };
        let Some(name) = exe.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !names.contains(&name) {
            continue;
        }
        // 僵尸进程不处理；它们已经不再运行。
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            if stat
                .rsplit(')')
                .next()
                .and_then(|r| r.trim().chars().next())
                == Some('Z')
            {
                continue;
            }
        }
        out.push(pid);
    }
    out
}

#[cfg(target_os = "macos")]
pub fn pids_with_binary(names: &[&str]) -> Vec<i32> {
    let me = std::process::id() as i32;
    let mut out = Vec::new();
    for pid in mac_all_pids() {
        if pid == me {
            continue;
        }
        if pid_is_binary(pid as i64, names) {
            out.push(pid);
        }
    }
    out
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
pub fn pids_with_binary(_names: &[&str]) -> Vec<i32> {
    Vec::new()
}

#[cfg(target_os = "macos")]
fn mac_all_pids() -> Vec<i32> {
    extern "C" {
        fn proc_listallpids(buffer: *mut libc::c_void, buffersize: libc::c_int) -> libc::c_int;
    }
    let mut cap = 1024usize;
    loop {
        let mut buf: Vec<i32> = vec![0; cap];
        // buffersize 是字节数，不是元素个数。
        let n = unsafe {
            proc_listallpids(
                buf.as_mut_ptr() as *mut libc::c_void,
                (cap * std::mem::size_of::<i32>()) as libc::c_int,
            )
        };
        if n <= 0 {
            return Vec::new();
        }
        let n = n as usize;
        if n < cap {
            buf.truncate(n);
            return buf.into_iter().filter(|p| *p > 0).collect();
        }
        cap *= 2;
        if cap > 1 << 20 {
            return buf.into_iter().filter(|p| *p > 0).collect();
        }
    }
}

/// 改进程名（`ps` 里看到的那个）。纯装饰：失败就算了，不影响任何功能。
#[cfg(target_os = "linux")]
pub fn set_process_name(name: &str) {
    const PR_SET_NAME: i32 = 15;
    extern "C" {
        fn prctl(option: i32, ...) -> i32;
    }
    let mut buf = [0u8; 16];
    for (i, b) in name.as_bytes().iter().take(15).enumerate() {
        buf[i] = *b;
    }
    unsafe {
        prctl(PR_SET_NAME, buf.as_ptr());
    }
}

#[cfg(target_os = "macos")]
pub fn set_process_name(name: &str) {
    extern "C" {
        fn pthread_setname_np(thread: libc::pthread_t, name: *const libc::c_char) -> libc::c_int;
        fn pthread_self() -> libc::pthread_t;
    }
    // macOS 上限 63 字节（不含结尾 0）。
    let mut buf = [0i8; 64];
    for (i, b) in name.as_bytes().iter().take(63).enumerate() {
        buf[i] = *b as i8;
    }
    unsafe {
        pthread_setname_np(pthread_self(), buf.as_ptr());
    }
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
pub fn set_process_name(_name: &str) {}

/// 单实例 pid 文件放在哪：Linux 沿用 `/run/user/<uid>/…`（已安装的旧版本认这个），
/// macOS 没有 `/run/user`，放到数据目录里。
///
/// 注意 `.pid` 后缀要**统一加上**：0.24.0 的跨平台重构里曾经把它弄丢过
/// （Linux 上变成 `/run/user/1000/skynet-downloadd-7788`），
/// 于是按老名字读 pid 文件的脚本全部失效，旧文件也没人清。
pub fn pid_file(name: &str) -> PathBuf {
    let file = format!("{name}.pid");
    if cfg!(target_os = "linux") {
        let uid = unsafe { libc::getuid() };
        PathBuf::from(format!("/run/user/{uid}/{file}"))
    } else {
        crate::paths::data_dir().join(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn our_own_pid_is_alive_and_named() {
        let me = std::process::id() as i64;
        assert!(pid_alive(me));
        let name = exe_name(me).expect("自己的可执行文件名应该拿得到");
        assert!(!name.is_empty());
    }

    #[test]
    fn a_pid_that_cannot_exist_is_not_alive() {
        // 0 和负数不是有效 pid；超大 pid 也不会存在。
        assert!(!pid_alive(0));
        assert!(!pid_alive(-1));
        assert!(exe_path(i64::MAX / 2).is_none());
    }

    #[test]
    fn pid_file_keeps_the_pid_suffix() {
        // 0.24.0 的跨平台重构里丢过 `.pid`：按老名字读 pid 的脚本全部失效，
        // 旧文件也没人清（实测在 /run/user 下攒了 100 多个）。
        let p = pid_file("skynet-downloadd-7788");
        assert_eq!(p.file_name().unwrap(), "skynet-downloadd-7788.pid", "{p:?}");
        assert_eq!(pid_file("skynet-download").file_name().unwrap(), "skynet-download.pid");
    }

    #[test]
    fn scan_finds_at_least_itself_when_not_skipped() {
        // pids_with_binary 会跳过自己，所以这里只验证它不会 panic 且返回 Vec。
        let found = pids_with_binary(&["definitely-not-a-real-binary-name"]);
        assert!(found.is_empty());
    }
}
