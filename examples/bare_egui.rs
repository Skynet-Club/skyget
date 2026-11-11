//! 对照实验：极简绘制 + 我们的完整 viewport 配置。

struct Bare;

impl eframe::App for Bare {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui.label("对照实验");
    }
}

fn main() -> eframe::Result {
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")).unwrap();
    let mut opts = eframe::NativeOptions::default();
    opts.viewport = egui::ViewportBuilder::default()
        .with_inner_size([1180.0, 720.0])
        .with_min_inner_size([980.0, 520.0])
        .with_title("bare-egui")
        .with_decorations(false)
        .with_app_id("skynet-download")
        .with_icon(icon);
    eframe::run_native("bare-egui", opts, Box::new(|_cc| Ok(Box::new(Bare))))
}
