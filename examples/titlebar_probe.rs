//! 复刻标题栏的真实结构，用模拟事件点那个 ×，看它到底收不收得到点击。
//!
//!     cargo run --example titlebar_probe --features gui

use egui::{
    pos2, vec2, Align, Event, Layout, Margin, Modifiers, PointerButton, RawInput, Rect, Sense, Ui,
};

fn win_button(ui: &mut Ui) -> (bool, Rect) {
    let (rect, resp) = ui.allocate_exact_size(vec2(34.0, 26.0), Sense::click());
    ui.painter()
        .rect_stroke(rect, 0.0, egui::Stroke::new(1.0, egui::Color32::RED), egui::StrokeKind::Inside);
    (resp.clicked(), rect)
}

fn main() {
    let ctx = egui::Context::default();
    let screen = Rect::from_min_size(pos2(0.0, 0.0), vec2(600.0, 300.0));
    let mut btn_rect = Rect::NOTHING;
    let mut clicked = false;

    let mk = |events: Vec<Event>, t: f64| {
        let mut r = RawInput {
            screen_rect: Some(screen),
            time: Some(t),
            ..Default::default()
        };
        r.events = events;
        r
    };

    // 第 1 帧：把控件摆出来，拿到按钮矩形
    let out = ctx.run_ui(mk(vec![], 0.0), |ui| {
        egui::Panel::top("titlebar")
            .exact_size(34.0)
            .frame(
                egui::Frame::NONE
                    .fill(egui::Color32::from_rgb(58, 58, 58))
                    .inner_margin(Margin::symmetric(10, 4)),
            )
            .show(ui, |ui| {
                let full = ui.available_rect_before_wrap();
                let drag = Rect::from_min_max(full.min, pos2(full.max.x - 102.0, full.max.y));
                ui.interact(drag, ui.id().with("drag"), Sense::click_and_drag());
                ui.horizontal_centered(|ui| {
                    ui.label("天网盒子");
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let (_, r) = win_button(ui);
                        btn_rect = r;
                    });
                });
            });
    });
    out.drop_without_applying_deltas();
    println!("标题栏内容区高度：{:.1}", 34.0 - 8.0);
    println!("× 按钮的矩形：{btn_rect:?}");

    let pos = btn_rect.center();
    // 第 2 帧：移过去 + 按下；第 3 帧：松开
    for (t, evs) in [
        (
            0.05,
            vec![
                Event::PointerMoved(pos),
                Event::PointerButton {
                    pos,
                    button: PointerButton::Primary,
                    pressed: true,
                    modifiers: Modifiers::NONE,
                },
            ],
        ),
        (
            0.10,
            vec![Event::PointerButton {
                pos,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            }],
        ),
    ] {
        let out = ctx.run_ui(mk(evs, t), |ui| {
            egui::Panel::top("titlebar")
                .exact_size(34.0)
                .frame(
                    egui::Frame::NONE
                        .fill(egui::Color32::from_rgb(58, 58, 58))
                        .inner_margin(Margin::symmetric(10, 4)),
                )
                .show(ui, |ui| {
                    let full = ui.available_rect_before_wrap();
                    let drag = Rect::from_min_max(full.min, pos2(full.max.x - 102.0, full.max.y));
                    ui.interact(drag, ui.id().with("drag"), Sense::click_and_drag());
                    ui.horizontal_centered(|ui| {
                        ui.label("天网盒子");
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let (c, _) = win_button(ui);
                            if c {
                                clicked = true;
                            }
                        });
                    });
                });
        });
        out.drop_without_applying_deltas();
    }

    println!("点到 × 了吗：{clicked}");
    std::process::exit(if clicked { 0 } else { 1 });
}
