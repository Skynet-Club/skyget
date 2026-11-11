//! 天网盒子 · GUI 入口。下载/上传全在内核和 daemon 里，这里只管画和收事件。

use super::App;

pub fn run() -> eframe::Result {
    // 已经有实例在跑：叫醒它，然后自己安静退出。
    // 没有这条，窗口收进托盘后再点桌面图标会开第二个窗口。
    if !crate::tray::claim_single_instance() {
        return Ok(());
    }
    let start_hidden = std::env::args().skip(1).any(|a| a == "--hidden");

    // 图标在编译期嵌进来：装到哪儿都不会丢
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../../assets/icon.png"))
        .expect("assets/icon.png 解不开");
    // 验收旋钮：`SKYGET_GUI_SIZE=980x520` 指定初始窗口大小。
    // 这台机器的合成器吞掉 XTEST，没法用鼠标拖窗口 —— 窄布局只能这样看。
    let (w0, h0) = std::env::var("SKYGET_GUI_SIZE")
        .ok()
        .and_then(|v| {
            let (a, b) = v.split_once('x')?;
            Some((a.trim().parse::<f32>().ok()?, b.trim().parse::<f32>().ok()?))
        })
        .unwrap_or((1180.0, 720.0));
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([w0, h0])
            .with_min_inner_size([980.0, 520.0])
            .with_title("天网盒子 · Skynet-Club")
            // 自己画标题栏，才能跟天网哨兵、天网机库是同一张脸
            .with_decorations(false)
            // 稳定的 app_id 保留 `skynet-download`：Wayland/X11 靠它归并窗口，
            // 桌面文件和托盘图标也还认这个名字；改名不打断已安装的入口。
            .with_app_id("skynet-download")
            .with_icon(icon),
        ..Default::default()
    };
    let result = eframe::run_native(
        "天网盒子 · Skynet-Club",
        options,
        Box::new(move |cc| {
            let mut app = App::new(cc);
            let ctx = cc.egui_ctx.clone();
            // 托盘与唤醒都拿着 ctx：界面没有心跳，谁有变化谁叫一次重绘
            app.attach_tray(crate::tray::start(ctx.clone()));
            crate::tray::install_wake(ctx);
            app.set_start_hidden(start_hidden);
            Ok(Box::new(app))
        }),
    );
    crate::tray::release_single_instance();
    result
}
