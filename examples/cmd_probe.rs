//! viewport 命令到底生不生效（不需要鼠标）：
//!
//!     cargo run --example cmd_probe --features gui -- hide
//!
//! 时间线：每 200ms 醒一次；t=2s 发 `Visible(false)`；t=6s 再发一次 `Visible(true)`。
//! 打印每条路径被调用的次数，用来判断 eframe 到底有没有停止跑 ui()。

use std::time::Instant;

struct Probe {
    t0: Instant,
    ui_calls: u64,
    logic_calls: u64,
    hidden_sent: bool,
    shown_back: bool,
}

impl Probe {
    fn tick(&mut self, ctx: &egui::Context) {
        let el = self.t0.elapsed().as_secs_f32();
        if !self.hidden_sent && el > 2.0 {
            self.hidden_sent = true;
            eprintln!("[probe] t={el:.2}s 发 Visible(false)");
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
        if self.hidden_sent && !self.shown_back && el > 6.0 {
            self.shown_back = true;
            eprintln!("[probe] t={el:.2}s 发 Visible(true)");
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        }
        if el > 10.0 {
            eprintln!(
                "[probe] 10 秒小结：ui() 调用 {} 次，logic() 调用 {} 次",
                self.ui_calls, self.logic_calls
            );
            std::process::exit(0);
        }
    }
}

impl eframe::App for Probe {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.logic_calls += 1;
        if self.logic_calls % 10 == 0 {
            eprintln!(
                "[probe] logic() #{} t={:.2}s",
                self.logic_calls,
                self.t0.elapsed().as_secs_f32()
            );
        }
        self.tick(ctx);
        ctx.request_repaint_after(std::time::Duration::from_millis(200));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.ui_calls += 1;
        if self.ui_calls % 10 == 0 {
            eprintln!(
                "[probe] ui() #{} t={:.2}s",
                self.ui_calls,
                self.t0.elapsed().as_secs_f32()
            );
        }
        let ctx = ui.ctx().clone();
        self.tick(&ctx);
        ui.label(format!("ui #{}", self.ui_calls));
        ctx.request_repaint_after(std::time::Duration::from_millis(200));
    }
}

fn main() -> eframe::Result {
    eprintln!("[probe] 开始");
    let r = eframe::run_native(
        "cmd probe",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([300.0, 200.0])
                .with_decorations(false),
            ..Default::default()
        },
        Box::new(|_cc| {
            Ok(Box::new(Probe {
                t0: Instant::now(),
                ui_calls: 0,
                logic_calls: 0,
                hidden_sent: false,
                shown_back: false,
            }))
        }),
    );
    eprintln!("[probe] run_native 返回：{r:?}");
    r
}
