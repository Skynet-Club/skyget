//! 视觉层：照搬 PhotoCraft 的语义 token 命名，只保留一个暗色主题
//! （Pro Medium Gray：#535353 面板 / #282828 画布）。
//!
//! 抄的是它的「命名与尺寸」，不是它那 14 套主题——编辑器才需要换肤。

use egui::{
    Color32, Context, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Stroke,
    TextStyle, Visuals,
};

pub const BG: Color32 = Color32::from_rgb(0x28, 0x28, 0x28);
pub const CHROME: Color32 = Color32::from_rgb(0x3a, 0x3a, 0x3a);
pub const DOCK: Color32 = Color32::from_rgb(0x42, 0x42, 0x42);
pub const ROW: Color32 = Color32::from_rgb(0x31, 0x31, 0x31);
pub const ROW_ALT: Color32 = Color32::from_rgb(0x36, 0x36, 0x36);
pub const ROW_HOVER: Color32 = Color32::from_rgb(0x3f, 0x3f, 0x3f);
pub const ROW_SEL: Color32 = Color32::from_rgb(0x4d, 0x4d, 0x4d);
pub const FIELD: Color32 = Color32::from_rgb(0x45, 0x45, 0x45);
pub const FIELD_BORDER: Color32 = Color32::from_rgb(0x68, 0x68, 0x68);
pub const SEP: Color32 = Color32::from_rgb(0x3e, 0x3e, 0x3e);
pub const TEXT: Color32 = Color32::from_rgb(0xee, 0xee, 0xee);
pub const DIM: Color32 = Color32::from_rgb(0xd4, 0xd4, 0xd4);
/// 弱文本。**三个角色都要在「最亮的那个面」上过 AA 4.5:1**：
/// 这个界面不是纯暗底 —— 顶栏 CHROME(#3a3a3a)、右栏 DOCK(#424242)、
/// 隔行 ROW_ALT(#363636) 都比 BG 亮，弱色在它们上面最先掉到 4.5 以下。
/// 原来 #a0a0a0 在 DOCK 上只有 3.84:1（底栏筛选、顶栏「机群」都在那上面）。
/// 现值：BG 6.65 / ROW_ALT 5.45 / CHROME 5.13 / DOCK 4.53。
pub const FAINT: Color32 = Color32::from_rgb(0xae, 0xae, 0xae);
/// 强调色。同样是按 DOCK 上的文本算的（4.07 → 4.67）：
/// 它既是选中态的填充，也是「机群」这种文字的颜色。
pub const ACCENT: Color32 = Color32::from_rgb(0x8d, 0xb4, 0xe2);
pub const OK: Color32 = Color32::from_rgb(0x9e, 0xc9, 0xa7);
/// 警告：4.47 → 4.74（DOCK 上）。
pub const WARN: Color32 = Color32::from_rgb(0xdf, 0xa9, 0x4a);
/// 危险：失败行的状态字画在 ROW/ROW_ALT 上、失败原因画在 DOCK 上，
/// 原值在这两处只有 4.41 / 3.41。现值：ROW 6.17 / DOCK 4.77。
pub const DANGER: Color32 = Color32::from_rgb(0xe8, 0xa0, 0xa0);

// ── 字号：只有 5 档比例 + 1 个等宽 ────────────────────────────────
//
// 之前是 9 档（10 / 10.5 / 11 / 11.5 / 12 / 12.5 / 13 / 13.5 / 14 / 17），
// 差 0.5px 的两档肉眼分不出，却让「这行该多小」每天都得重新决定。
// 现在：微（10.5）小（11.5）正文（12）标题（13.5）大标题（17），等宽统一 11.5。
pub const T_MICRO: f32 = 10.5;
pub const T_SMALL: f32 = 11.5;
pub const T_BODY: f32 = 12.0;
pub const T_TITLE: f32 = 13.5;
pub const T_HEAD: f32 = 17.0;
pub const T_MONO: f32 = 11.5;

/// 系统里可能存在的 CJK 字体，按优先级。自带字体文件是后面的事（跨平台打包时再内嵌）。
const CJK_CANDIDATES: &[&str] = &[
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/opentype/noto/NotoSansCJKsc-Regular.otf",
    "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
    "/System/Library/Fonts/PingFang.ttc",
    "C:\\Windows\\Fonts\\msyh.ttc",
    "C:\\Windows\\Fonts\\msyh.ttf",
];

pub fn install(ctx: &Context) {
    install_fonts(ctx);

    ctx.all_styles_mut(|style| {
    let mut v = Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = DOCK;
    v.extreme_bg_color = FIELD;
    v.faint_bg_color = ROW_ALT;
    v.override_text_color = Some(TEXT);
    v.selection.bg_fill = ACCENT.gamma_multiply(0.30);
    v.selection.stroke = Stroke::new(1.0, ACCENT);

    v.widgets.noninteractive.bg_fill = DOCK;
    v.widgets.noninteractive.weak_bg_fill = DOCK;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, SEP);
    v.widgets.noninteractive.corner_radius = CornerRadius::same(2);

    v.widgets.inactive.bg_fill = FIELD;
    v.widgets.inactive.weak_bg_fill = FIELD;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, FIELD_BORDER);
    v.widgets.inactive.corner_radius = CornerRadius::same(3);

    v.widgets.hovered.bg_fill = ROW_HOVER;
    v.widgets.hovered.weak_bg_fill = ROW_HOVER;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, ACCENT);
    v.widgets.hovered.corner_radius = CornerRadius::same(3);

    v.widgets.active.bg_fill = ROW_SEL;
    v.widgets.active.weak_bg_fill = ROW_SEL;
    v.widgets.active.bg_stroke = Stroke::new(1.0, ACCENT);
    v.widgets.active.corner_radius = CornerRadius::same(3);

    v.text_cursor.blink = false;
    style.visuals = v;
    style.text_styles = [
        (TextStyle::Small, FontId::proportional(T_MICRO)),
        (TextStyle::Body, FontId::proportional(T_BODY)),
        (TextStyle::Button, FontId::proportional(T_BODY)),
        (TextStyle::Heading, FontId::proportional(T_TITLE)),
        (TextStyle::Monospace, FontId::monospace(T_MONO)),
    ]
    .into();
    style.spacing.item_spacing = egui::vec2(8.0, 5.0);
    style.spacing.button_padding = egui::vec2(10.0, 4.0);
    });
}

fn install_fonts(ctx: &Context) {
    let mut fonts = FontDefinitions::default();
    for path in CJK_CANDIDATES {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let name = "cjk".to_owned();
        fonts
            .font_data
            .insert(name.clone(), std::sync::Arc::new(FontData::from_owned(bytes)));
        fonts
            .families
            .entry(FontFamily::Proportional)
            .or_default()
            .insert(0, name.clone());
        // 等宽也要能显示中文，否则版本号旁边的说明会变成方块。
        fonts
            .families
            .entry(FontFamily::Monospace)
            .or_default()
            .push(name);
        break;
    }
    // 再补一张含下标/数学符号的拉丁字体垫底：Noto Sans CJK 里没有 U+209A(ₚ)
    // 这类字符，不垫就会渲染成空方块（界面上看着像坏了）。
    const LATIN_FALLBACK: &str = "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf";
    if let Ok(bytes) = std::fs::read(LATIN_FALLBACK) {
        let name = "latin_fallback".to_owned();
        fonts
            .font_data
            .insert(name.clone(), std::sync::Arc::new(FontData::from_owned(bytes)));
        // push 到末尾 = 最后才回退到它，不会抢中文的字形。
        fonts
            .families
            .entry(FontFamily::Proportional)
            .or_default()
            .push(name.clone());
        fonts
            .families
            .entry(FontFamily::Monospace)
            .or_default()
            .push(name);
    }
    ctx.set_fonts(fonts);
}
