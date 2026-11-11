//! `skynet-download` · 天网盒子 GUI 的**旧名兼容入口**。
//!
//! 新名是 `skynet-dataplane`；两个二进制共用 `skyfetch::gui::run()`，
//! 同一个 app_id（保持 Dock/桌面文件归并）和同一份数据目录。

fn main() -> eframe::Result {
    skyfetch::gui::run()
}
