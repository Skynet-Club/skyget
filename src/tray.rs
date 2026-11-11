//! 托盘图标（StatusNotifierItem）。
//!
//! 跟天网哨兵同一套做法：走 D-Bus 的 SNI 协议，由桌面自己的托盘宿主
//! （gnome-shell + ubuntu-appindicators 扩展）去画，所以不引 GTK、不碰 eframe 的事件循环。
//!
//! 图标只给**主题名**，不给位图：桌面会按自己的尺寸从 hicolor 里挑，
//! 我们用 scripts/install-desktop.sh 已经把 16→512 八个尺寸装进去了。
//!
//! 关窗口 ≠ 退出：有托盘时点关闭只是把窗口收起来，下载线程照跑。
//! 没有托盘宿主时仍然「关窗口即退出」—— 否则用户关了窗口就再也找不到入口。

use std::io::{Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
#[cfg(target_os = "linux")]
use std::sync::mpsc::{channel, Sender};
use std::sync::Mutex;

#[cfg(target_os = "linux")]
use ksni::menu::{MenuItem, StandardItem};

/// 单实例锁文件。锁由 flock 持有到进程退出；pid 文件内容只用于“叫醒旧窗口”。
static INSTANCE_FILE: Mutex<Option<std::fs::File>> = Mutex::new(None);

/// 托盘菜单能触发的事
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayCmd {
    /// 把窗口叫回来
    Show,
    /// 真退出
    Quit,
}

#[cfg(target_os = "linux")]
struct Tray {
    tx: Sender<TrayCmd>,
    /// 叫醒界面用。窗口收在托盘里时 eframe 不出帧，
    /// 所以点菜单这一下必须自己 request_repaint —— 否则命令永远没人处理。
    ctx: egui::Context,
    /// 这个桌面能不能由程序把窗口叫回来。
    /// Wayland 下 winit 的 `set_visible(false)` / `set_minimized(false)` / `focus_window()`
    /// 全是空实现，所以「显示窗口」在那里是死键 —— 干脆不画出来，别再造一个「点了没反应」。
    can_restore: bool,
}

#[cfg(target_os = "linux")]
impl Tray {
    fn fire(&self, cmd: TrayCmd) {
        let _ = self.tx.send(cmd);
        self.ctx.request_repaint();
        if cmd == TrayCmd::Quit {
            // 不能等 GUI 下一帧再处理 Quit：窗口最小化/收进托盘后（尤其 Wayland）
            // eframe 不出帧，request_repaint 也可能叫不醒它，ViewportCommand::Close
            // 就永远不会被处理 —— 表现就是「点了退出，托盘和 Dock 还在」。
            //
            // 也不能在这个 D-Bus 回调里立刻 exit：菜单宿主会收不到方法回复
            // （桌面上表现为短暂转圈/“应用无响应”）。先让回调返回，100ms 后
            // 由独立线程结束进程 —— 不依赖任何 GUI 帧。
            std::thread::spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(100));
                quit_now();
            });
        }
    }
}

/// 立即退出 GUI 进程；先释放单实例 pid，避免下次启动误以为还有窗口在跑。
/// 不依赖 egui 下一帧 —— 隐藏窗口没有下一帧。
pub fn quit_now() -> ! {
    log("托盘/快捷键：退出 GUI");
    release_single_instance();
    std::process::exit(0);
}

/// 托盘线程没有 egui 日志通道；直接写 gui.log，和界面按钮日志同文件。
fn log(msg: &str) {
    use std::io::Write;
    let dir = crate::paths::data_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("gui.log"))
    {
        let _ = writeln!(f, "{msg}");
    }
}

#[cfg(target_os = "linux")]
impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "skynet-download".into()
    }

    fn title(&self) -> String {
        "天网盒子".into()
    }

    fn icon_name(&self) -> String {
        // 指向 ~/.local/share/icons/hicolor/*/apps/skynet-download.png
        "skynet-download".into()
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        // 左键点图标 = 叫回窗口。Wayland 下叫不回来（winit 的 focus/visible 都是空实现），
        // 那就别发一条注定没结果的命令。
        if self.can_restore {
            self.fire(TrayCmd::Show);
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut items: Vec<MenuItem<Self>> = Vec::new();
        if self.can_restore {
            items.push(
                StandardItem {
                    label: "显示窗口".into(),
                    activate: Box::new(|t: &mut Self| t.fire(TrayCmd::Show)),
                    ..Default::default()
                }
                .into(),
            );
        } else {
            items.push(
                StandardItem {
                    label: "窗口在 dash 里，点图标找回".into(),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            );
        }
        items.push(MenuItem::Separator);
        items.push(
            StandardItem {
                label: "退出".into(),
                activate: Box::new(|t: &mut Self| t.fire(TrayCmd::Quit)),
                ..Default::default()
            }
            .into(),
        );
        items
    }
}

/// 启动托盘，返回接收命令的通道。
/// 桌面没有 SNI 宿主时返回 None —— 那就老老实实关窗口即退出。
///
/// 注意：启动失败**不是终局**。GNOME 的 appindicator 扩展会掉线、也会被重新启用，
/// 调用方应当隔一会儿再试一次，别一次失败就永久放弃。
#[cfg(target_os = "linux")]
pub fn start(ctx: egui::Context) -> Option<Receiver<TrayCmd>> {
    // Wayland 下窗口既藏不起来也叫不回来（winit 那三个方法都是空实现），
    // 菜单里就不放「显示窗口」这一项。
    let can_restore = std::env::var_os("WAYLAND_DISPLAY").is_none();
    let (tx, rx) = channel();
    match ksni::blocking::TrayMethods::spawn(Tray {
        tx,
        ctx,
        can_restore,
    }) {
        Ok(_handle) => {
            // handle 必须活到进程结束：drop 掉它自己就从托盘消失了。
            // 这里存进静态变量，顺便给“托盘还在不在”留个把手。
            if let Ok(mut slot) = HANDLE.lock() {
                *slot = Some(_handle);
            }
            Some(rx)
        }
        Err(e) => {
            eprintln!("托盘起不来（{e}）");
            None
        }
    }
}

#[cfg(target_os = "linux")]
static HANDLE: std::sync::Mutex<Option<ksni::blocking::Handle<Tray>>> =
    std::sync::Mutex::new(None);

/// 把托盘项撤下来。重连之前必须先撤旧的那个 ——
/// 否则 Watcher 回来时旧服务会重新注册，顶栏上就会出现两个一模一样的图标。
#[cfg(target_os = "linux")]
pub fn shutdown() {
    if let Ok(mut slot) = HANDLE.lock() {
        *slot = None; // drop handle = 从托盘移除
    }
}

/// 托盘项此刻还在不在座。用来判断「窗口收起来了，但托盘图标其实没了」——
/// 那是死角：用户既看不到窗口，也点不到图标。
///
/// 判据是去问 StatusNotifierWatcher：注册名单里还有没有我们。
#[cfg(target_os = "linux")]
pub fn is_registered() -> bool {
    let Ok(bus) = zbus::blocking::Connection::session() else {
        return false;
    };
    let Ok(reply) = bus.call_method(
        Some("org.kde.StatusNotifierWatcher"),
        "/StatusNotifierWatcher",
        Some("org.freedesktop.DBus.Properties"),
        "Get",
        &("org.kde.StatusNotifierWatcher", "RegisteredStatusNotifierItems"),
    ) else {
        return false;
    };
    let Ok((value,)): Result<(zbus::zvariant::OwnedValue,), _> = reply.body().deserialize() else {
        return false;
    };
    let Ok(list) = Vec::<String>::try_from(value) else {
        return false;
    };
    let mine = format!("-{}-", std::process::id());
    list.iter().any(|s| s.contains(&mine))
}

/// 非 Linux 没有 SNI/D-Bus 托盘（macOS 要用 NSStatusItem，那是另一套移植）。
/// 这里保留同一份 API，让 GUI 编译和单实例逻辑不用分叉：托盘为空，
/// 「关窗口即退出」这个诚实的退路由 GUI 自己处理。
#[cfg(not(target_os = "linux"))]
pub fn start(_ctx: egui::Context) -> Option<Receiver<TrayCmd>> {
    None
}

#[cfg(not(target_os = "linux"))]
pub fn shutdown() {}

#[cfg(not(target_os = "linux"))]
pub fn is_registered() -> bool {
    false
}

static WAKE: AtomicBool = AtomicBool::new(false);
/// 自管道的写端。信号处理器里能做的只有写一个字节（async-signal-safe）。
static WAKE_FD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

extern "C" fn on_sigusr1(_sig: libc::c_int) {
    WAKE.store(true, Ordering::SeqCst);
    let fd = WAKE_FD.load(Ordering::SeqCst);
    if fd >= 0 {
        let byte = 1u8;
        unsafe {
            libc::write(fd, &byte as *const u8 as *const libc::c_void, 1);
        }
    }
}

/// 装唤醒：第二个实例启动时发的 SIGUSR1 → 叫醒界面。
///
/// 走自管道（self-pipe）：信号处理器里只写一个字节，一个读线程阻塞在那里等着，
/// 醒来才 request_repaint。这样「再点一次桌面图标」不会带来任何常驻重绘 ——
/// 以前是靠界面每 400ms 自己醒来查标志位，那层轮询就是多余的。
pub fn install_wake(ctx: egui::Context) {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        eprintln!("装唤醒失败：建不了自管道（第二个实例将无法叫回窗口）");
        return;
    }
    WAKE_FD.store(fds[1], Ordering::SeqCst);
    unsafe {
        libc::signal(libc::SIGUSR1, on_sigusr1 as *const () as libc::sighandler_t);
    }
    let read_fd = fds[0];
    std::thread::spawn(move || {
        let mut buf = [0u8; 1];
        loop {
            let n = unsafe { libc::read(read_fd, buf.as_mut_ptr() as *mut libc::c_void, 1) };
            if n <= 0 {
                return;
            }
            ctx.request_repaint();
        }
    });
}

/// 这一帧有没有收到唤醒信号（第二个实例启动时发的）
pub fn take_wake() -> bool {
    WAKE.swap(false, Ordering::SeqCst)
}

fn pid_path() -> String {
    crate::sys::pid_file("skynet-download").display().to_string()
}

/// 单实例：已经有别的实例在跑就唤醒它并返回 false。
///
/// 这条对「关窗口收进托盘」是必需的：否则用户再点一次桌面图标会开第二个窗口，
/// 而不是把收起来的那个叫回来。
/// 这个 pid 真的是「天网盒子」吗（防止 pid 被重用时的误伤）。
/// 新旧 GUI 二进制名都认，升级过程中不会各开一个窗口。
fn pid_is_ours(pid: i32) -> bool {
    // 用可执行文件名做**精确**匹配：前缀匹配会让守护进程 `skynet-dataplaned`
    // 被误认成 GUI `skynet-dataplane`。新旧 GUI 名字都认。
    crate::sys::pid_is_binary(pid as i64, &["skynet-dataplane", "skynet-download"])
}

fn pid_alive(pid: i32) -> bool {
    crate::sys::pid_alive(pid as i64)
}

fn read_pid(path: &str) -> Option<i32> {
    std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<i32>()
        .ok()
}

/// 可执行文件已经被新构建替换（进程还活着，但 `/proc/<pid>/exe` 显示 deleted）。
/// 这种情况下不叫醒旧窗口，直接结束它，让新版本接手。
fn exe_deleted(pid: i32) -> bool {
    crate::sys::exe_deleted(pid as i64)
}

fn signal_wake(pid: i32) {
    unsafe {
        libc::kill(pid, libc::SIGUSR1);
    }
}

fn terminate_stale(pid: i32) {
    log(&format!("单实例：结束旧 GUI pid {pid}（二进制已替换）"));
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
    for _ in 0..20 {
        if !pid_alive(pid) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
}

/// pid 文件丢了也不能开第二个窗口：扫一遍系统找同名的 GUI。
/// 守护进程名是 `skynet-dataplaned`/`skynet-downloadd`，精确名不会误伤。
fn find_other_guis() -> Vec<i32> {
    crate::sys::pids_with_binary(&["skynet-dataplane", "skynet-download"])
}

pub fn claim_single_instance() -> bool {
    let path = pid_path();
    let me = std::process::id() as i32;

    if let Some(dir) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&path)
    {
        Ok(f) => f,
        Err(e) => {
            log(&format!("单实例：打不开 {}：{e}", path));
            return false;
        }
    };

    // 1. 先抢 flock。旧版 GUI 没有锁，会在这里输给新进程；
    //    真正在跑的新版 GUI 持有锁，抢不到。
    let mut locked = false;
    for _ in 0..3 {
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            locked = true;
            break;
        }
        // 锁被别人持有：正常唤醒；如果它跑的是已被替换的旧二进制，结束它再抢。
        // 没有 pid 时只处理“已删除的旧二进制”，绝不向刚启动、还没装 SIGUSR1
        // 处理器的并发实例发信号（那会把它直接杀掉）。
        match read_pid(&path) {
            Some(pid) if pid != me && pid_alive(pid) && pid_is_ours(pid) => {
                if exe_deleted(pid) {
                    terminate_stale(pid);
                    continue;
                }
                signal_wake(pid);
                return false;
            }
            _ => {
                let deleted: Vec<i32> = find_other_guis()
                    .into_iter()
                    .filter(|p| *p != me && exe_deleted(*p))
                    .collect();
                if deleted.is_empty() {
                    return false;
                }
                for pid in deleted {
                    terminate_stale(pid);
                }
                continue;
            }
        }
    }
    if !locked {
        return false;
    }

    // 2. 锁已拿到：此时还活着的其它 GUI 都是没锁的旧版/并发启动者。
    //    结束它们，保证只剩这一份窗口和托盘。
    for pid in find_other_guis() {
        if pid != me {
            terminate_stale(pid);
        }
    }

    let _ = file.set_len(0);
    let _ = file.seek(SeekFrom::Start(0));
    let _ = write!(file, "{me}");
    let _ = file.flush();
    // 文件句柄必须活到进程结束：drop 掉 flock 就释放。
    if let Ok(mut slot) = INSTANCE_FILE.lock() {
        *slot = Some(file);
    }
    true
}

/// 退出前释放单实例锁。先删 pid 文件，再 drop 锁；新实例会创建新 inode。
pub fn release_single_instance() {
    let file = INSTANCE_FILE.lock().ok().and_then(|mut g| g.take());
    if file.is_some() {
        let _ = std::fs::remove_file(pid_path());
        drop(file);
        return;
    }
    // 兼容路径：没有锁句柄时只删写着当前 pid 的文件。
    let path = pid_path();
    let me = std::process::id().to_string();
    if std::fs::read_to_string(&path)
        .map(|s| s.trim() == me)
        .unwrap_or(false)
    {
        let _ = std::fs::remove_file(path);
    }
}
