//! 对照实验（不需要真鼠标，纯 egui 事件模拟）：
//! 标题栏的拖动区如果盖住了右边的窗口按钮，那个按钮还能不能收到点击？
//!
//!     cargo run --example drag_probe --features gui

use egui::{pos2, vec2, Event, Modifiers, PointerButton, RawInput, Rect, Sense};

/// 模拟「移过去 → 按下 → 松开」，返回按钮有没有被点到。
fn click_at_button(overlap: bool) -> bool {
    let ctx = egui::Context::default();
    let screen = Rect::from_min_size(pos2(0.0, 0.0), vec2(400.0, 34.0));
    let btn = Rect::from_min_size(pos2(400.0 - 34.0, 4.0), vec2(34.0, 26.0));
    let pos = btn.center();
    let mut clicked = false;

    let frames: Vec<Vec<Event>> = vec![
        vec![Event::PointerMoved(pos)],
        vec![
            Event::PointerMoved(pos),
            Event::PointerButton {
                pos,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
        ],
        vec![Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed: false,
            modifiers: Modifiers::NONE,
        }],
    ];

    for (i, evs) in frames.into_iter().enumerate() {
        let mut raw = RawInput {
            screen_rect: Some(screen),
            time: Some(i as f64 * 0.05),
            ..Default::default()
        };
        raw.events = evs;
        let out = ctx.run_ui(raw, |ui| {
            // 拖动区：旧写法盖住整条标题栏；新写法只铺到按钮左边
            let drag_rect = if overlap {
                screen
            } else {
                Rect::from_min_max(screen.min, pos2(btn.left() - 2.0, screen.max.y))
            };
            ui.interact(drag_rect, ui.id().with("drag"), Sense::click_and_drag());

            // 右端的关闭按钮
            let resp = ui.put(btn, egui::Button::new("X").frame(false));
            if resp.clicked() {
                clicked = true;
            }
        });
        out.drop_without_applying_deltas();
    }
    clicked
}

fn main() {
    let covered = click_at_button(true);
    let clear = click_at_button(false);
    println!("拖动区盖住按钮 → 按钮收到点击 = {covered}");
    println!("拖动区让开按钮 → 按钮收到点击 = {clear}");
    if !covered {
        println!("=> 证实：拖动区盖住按钮会把点击吃掉（这就是「X 点了没反应」）");
    }
    if clear {
        println!("=> 修复后按钮能收到点击");
    }
    std::process::exit(if clear { 0 } else { 1 });
}
