//! `skynet-dataplane` · 天网盒子 GUI。
//!
//! 窗口只是守护进程的前端：关掉窗口，下载/上传继续。
//! 旧名 `skynet-download` 继续可用，两个入口行为完全一致。

fn main() -> eframe::Result {
    skyfetch::gui::run()
}
