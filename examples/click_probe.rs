//! 诊断用：这台机器上，鼠标点击能不能打进 winit/egui。
//!
//!     cargo build --example click_probe --features gui
//!     然后 xdotool 点一下，看 stderr 有没有 CLICKED。

use std::time::Instant;

struct Probe {
    clicks: u32,
    t0: Instant,
}

impl eframe::App for Probe {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui.vertical_centered(|ui| {
            ui.add_space(40.0);
            ui.label(format!("点击计数 {}", self.clicks));
            if ui
                .allocate_rect(ui.available_rect_before_wrap(), egui::Sense::click())
                .clicked()
            {
                self.clicks += 1;
                eprintln!("CLICKED {} at {:.2}s", self.clicks, self.t0.elapsed().as_secs_f32());
            }
        });
        // 把所有原始事件打出来：能分清「事件没进来」和「进来了但没判定成点击」
        let evs: Vec<String> = ui
            .ctx()
            .input(|i| i.raw.events.iter().map(|e| format!("{e:?}")).collect());
        for e in evs {
            eprintln!("EVENT {e}");
        }
    }
}

fn main() -> eframe::Result {
    let t0 = Instant::now();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([400.0, 300.0])
            .with_position([200.0, 200.0])
            .with_decorations(false),
        ..Default::default()
    };
    eframe::run_native(
        "click probe",
        options,
        Box::new(move |_cc| Ok(Box::new(Probe { clicks: 0, t0 }))),
    )
}
