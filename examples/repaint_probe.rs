//! 诊断用：谁在不停地要求重绘 + 一个候选修法的对照实验。
//!
//! 环境变量 `DEDUPE_IME=1` 时，每帧在画之前把「与上一条完全相同的 IME 预编辑事件」丢掉。
//! 平台背景（egui-winit 0.36 自己的文档里就记着这一条）：
//!   Debian/GNOME/Wayland + fcitx5：`Preedit("", None)` 会一直重复发，直到输入框失焦。
//!   而 egui 的 `wants_repaint_after()` 把「有事件」当成「立刻重绘」，
//!   egui-winit 又在「有事件」时重设 IME 光标位置 —— 于是形成 150~200fps 的空转循环。
//!
//!     cargo run --example repaint_probe --features full
//!     DEDUPE_IME=1 cargo run --example repaint_probe --features full

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

struct Probe {
    inner: skyfetch::gui::App,
    tally: std::collections::BTreeMap<String, u64>,
    frames: u64,
    dedupe: bool,
    last_preedit: Option<String>,
    dropped: u64,
    ime_total: u64,
}

impl eframe::App for Probe {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.frames += 1;
        for c in ui.ctx().repaint_causes() {
            *self.tally.entry(format!("{c}")).or_insert(0) += 1;
        }

        if self.dedupe {
            let mut last = self.last_preedit.clone();
            let mut dropped = 0u64;
            ui.ctx().input_mut(|i| {
                i.events.retain(|e| match e {
                    egui::Event::Ime(egui::ImeEvent::Preedit { text, .. }) => {
                        if last.as_deref() == Some(text.as_str()) {
                            dropped += 1;
                            false
                        } else {
                            last = Some(text.clone());
                            true
                        }
                    }
                    _ => true,
                });
            });
            self.last_preedit = last;
            self.dropped += dropped;
        }

        if self.frames % 40 == 0 {
            let total: u64 = self.tally.values().sum();
            let ime_per_frame = ui.ctx().input(|i| {
                i.raw
                    .events
                    .iter()
                    .filter(|e| matches!(e, egui::Event::Ime(_)))
                    .count()
            });
            self.ime_total += ime_per_frame as u64;
            let first = ui.ctx().input(|i| i.raw.events.first().map(|e| format!("{e:?}")).unwrap_or_default());
            eprintln!(
                "[帧] 第 {} 帧；本帧事件 {} 个；丢掉重复 IME {} 个；IME 事件累计 {}",
                self.frames, ime_per_frame, self.dropped, self.ime_total
            );
            eprintln!("      首个事件：{first}");
            let mut v: Vec<_> = self.tally.iter().collect();
            v.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
            for (k, n) in v.iter().take(3) {
                let short: String = k
                    .chars()
                    .rev()
                    .take(46)
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect();
                eprintln!("      {n:>6}  {short}");
            }
            eprintln!("      （累计重绘来源 {total} 次）");
        }
        eframe::App::ui(&mut self.inner, ui, frame);
    }
}

fn main() -> eframe::Result {
    let dedupe = std::env::var("DEDUPE_IME").is_ok_and(|v| v == "1");
    eprintln!("[模式] DEDUPE_IME={dedupe}");

    let count = Arc::new(AtomicU64::new(0));
    let count2 = count.clone();
    let t0 = Instant::now();
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(2));
        eprintln!(
            "[统计] 累计 request_repaint {} 次（{:.1}/s）",
            count2.load(Ordering::Relaxed),
            count2.load(Ordering::Relaxed) as f64 / t0.elapsed().as_secs_f64()
        );
    });

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 720.0])
            .with_app_id("skynet-download-probe"),
        ..Default::default()
    };
    eframe::run_native(
        "repaint probe",
        options,
        Box::new(move |cc| {
            let n = count.clone();
            let n0 = Arc::new(AtomicU64::new(0));
            let n0b = n0.clone();
            cc.egui_ctx.set_request_repaint_callback(move |info| {
                n.fetch_add(1, Ordering::Relaxed);
                if info.delay.is_zero() {
                    let c = n0b.fetch_add(1, Ordering::Relaxed);
                    if c < 12 {
                        eprintln!(
                            "=== 立即重绘 #{} pass={}\n{}",
                            c,
                            info.current_cumulative_pass_nr,
                            std::backtrace::Backtrace::force_capture()
                        );
                    }
                }
            });
            let mut app = skyfetch::gui::App::new(cc);
            let ctx = cc.egui_ctx.clone();
            if std::env::var("PROBE_TRAY").is_ok_and(|v| v == "1") {
                app.attach_tray(skyfetch::tray::start(ctx.clone()));
                skyfetch::tray::install_wake(ctx);
            }
            Ok(Box::new(Probe {
                inner: app,
                tally: Default::default(),
                frames: 0,
                dedupe,
                last_preedit: None,
                dropped: 0,
                ime_total: 0,
            }))
        }),
    )
}
