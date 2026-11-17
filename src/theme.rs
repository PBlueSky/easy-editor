//! 字体与配色：加载等宽字体 / 中文字体，并统一样式。
//!
//! ## 中英文基线对齐
//!
//! egui 排版时每个字形的位置是
//! `font_face_ascent + valign*(行高 - 该段行高) + 0.5*(该段行高 - 字体高度)`，
//! 也就是说基线取决于**字形所属字体自己的 ascent**。
//!
//! 实测各字体 ascent（相对 em）：
//!
//! | 字体 | ascent | 行高 |
//! |---|---|---|
//! | Consolas | 0.7427 | 1.1709 |
//! | 微软雅黑 | 1.0581 | 1.3198 |
//! | Segoe UI Symbol | 1.0791 | 1.3301 |
//!
//! 微软雅黑比 Consolas 高 0.315em，14px 下中文会下沉约 4.4px，看起来就是
//! 「英文贴顶、中文掉底」。而且只要某行含中文，该行行高会被撑到 1.32em，
//! 又会让同一行里的英文再往下挪，导致纯英文行和混排行的基线还不一致。
//!
//! 因此这里做两件事：
//! 1. 所有代码区文本统一 `line_height = MONO_LINE_HEIGHT * 字号`，让行高恒定；
//! 2. 给中文/符号字体加 `y_offset_factor`，把基线差值精确抵消。
//!
//! 统一行高后，英文基线与中文基线之差为
//! `(1.0581 + (L-1.3198)/2) - (0.7427 + (L-1.1709)/2) = 0.2410`，
//! 所以中文上移 0.2410em 即对齐；符号字体同理为 0.2568em。

use std::sync::Arc;

use eframe::egui::{
    self, Color32, FontData, FontDefinitions, FontFamily, FontId, FontTweak, TextStyle,
};

/// 代码区统一行高（字号倍数）。取中文行高，保证字形不被裁切。
pub const MONO_LINE_HEIGHT: f32 = 1.32;

/// 微软雅黑相对 Consolas 的基线偏移校正（em）。
const CJK_Y_OFFSET: f32 = -0.2410;
/// Segoe UI Symbol 相对 Consolas 的基线偏移校正（em）。
const SYMBOL_Y_OFFSET: f32 = -0.2568;

fn read_first(paths: &[&str]) -> Option<Vec<u8>> {
    for p in paths {
        if let Ok(bytes) = std::fs::read(p) {
            if !bytes.is_empty() {
                return Some(bytes);
            }
        }
    }
    None
}

fn baseline_tweak(y_offset_factor: f32) -> FontTweak {
    FontTweak {
        y_offset_factor,
        ..Default::default()
    }
}

/// 安装字体：等宽字体优先 Consolas，中文回退到微软雅黑。
///
/// 同一个字体文件会被注册两次：一份给等宽家族（带基线校正），
/// 一份给比例家族（不带校正），字节通过 `Arc` 共享，不额外占内存。
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();

    if let Some(bytes) = read_first(&[
        "C:/Windows/Fonts/consola.ttf",
        "C:/Windows/Fonts/Consola.ttf",
        "C:/Windows/Fonts/CascadiaMono.ttf",
        "C:/Windows/Fonts/cascadiamono.ttf",
    ]) {
        fonts
            .font_data
            .insert("consolas".to_owned(), Arc::new(FontData::from_owned(bytes)));
        fonts
            .families
            .entry(FontFamily::Monospace)
            .or_default()
            .insert(0, "consolas".to_owned());
    }

    // 符号字体：补上 ▸ ▾ × 这类字形（egui 0.37 不再内置 emoji 字体）
    if let Some(bytes) = read_first(&[
        "C:/Windows/Fonts/seguisym.ttf",
        "C:/Windows/Fonts/SEGUISYM.TTF",
    ]) {
        let blob = Arc::new(bytes);
        let plain = FontData::from_blob(blob.clone(), 0);
        let mut mono = FontData::from_blob(blob, 0);
        mono.tweak = baseline_tweak(SYMBOL_Y_OFFSET);

        fonts
            .font_data
            .insert("symbols".to_owned(), Arc::new(plain));
        fonts
            .font_data
            .insert("symbols_mono".to_owned(), Arc::new(mono));

        fonts
            .families
            .entry(FontFamily::Proportional)
            .or_default()
            .push("symbols".to_owned());
        fonts
            .families
            .entry(FontFamily::Monospace)
            .or_default()
            .push("symbols_mono".to_owned());
    }

    // 中文字体：等宽家族用带基线校正的副本
    let cjk = [
        "C:/Windows/Fonts/msyh.ttc",
        "C:/Windows/Fonts/msyh.ttf",
        "C:/Windows/Fonts/simhei.ttf",
        "C:/Windows/Fonts/simsun.ttc",
        "C:/Windows/Fonts/Deng.ttf",
    ];
    if let Some(bytes) = read_first(&cjk) {
        let blob = Arc::new(bytes);
        let plain = FontData::from_blob(blob.clone(), 0);
        let mut mono = FontData::from_blob(blob, 0);
        mono.tweak = baseline_tweak(CJK_Y_OFFSET);

        fonts.font_data.insert("cjk".to_owned(), Arc::new(plain));
        fonts
            .font_data
            .insert("cjk_mono".to_owned(), Arc::new(mono));

        fonts
            .families
            .entry(FontFamily::Proportional)
            .or_default()
            .push("cjk".to_owned());
        fonts
            .families
            .entry(FontFamily::Monospace)
            .or_default()
            .push("cjk_mono".to_owned());
    }

    ctx.set_fonts(fonts);
}

/// 代码区行高（点）。编辑器与行号栏必须用同一个值，否则会错行。
///
/// epaint 会把每行高度按物理像素取整，125% / 150% 缩放下行高不是整数点。
/// 这里提前做同样的取整，行号、当前行底色和光标才能和文字落在同一条线上。
pub fn mono_row_height(font_size: f32, pixels_per_point: f32) -> f32 {
    let raw = font_size * MONO_LINE_HEIGHT;
    (raw * pixels_per_point).round() / pixels_per_point
}

/// 面板底色。系统标题栏要和它取同一个值，顶栏才连成一片（见 `titlebar::palette`）。
pub fn panel_fill(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(30, 32, 36)
    } else {
        Color32::from_rgb(249, 250, 251)
    }
}

/// 能点的地方一律换成手型光标。
pub const HAND: egui::CursorIcon = egui::CursorIcon::PointingHand;

/// 行内那种「看着像文字、其实能按」的小按钮。
///
/// `frame(false)` 的按钮悬浮时一笔不画，用户分不清那是一句说明还是一个能按的东西；
/// 这里改用 egui 自己的「没激活就藏起边框」——底色只在悬浮和按下时出现，
/// 边距却一直留着，所以点下去不会把整行推歪。字号和边距在这一处统一，
/// 免得各面板各写各的，一排小按钮高矮胖瘦不齐。
pub fn chip(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        ui.spacing_mut().button_padding = egui::vec2(4.0, 2.0);
        ui.add(
            egui::Button::new(egui::RichText::new(text).size(11.0))
                .frame(true)
                .frame_when_inactive(false),
        )
    })
    .inner
    .on_hover_cursor(HAND)
}

/// 应用整体视觉风格。
pub fn apply_style(ctx: &egui::Context, dark: bool) {
    ctx.set_theme(if dark {
        egui::ThemePreference::Dark
    } else {
        egui::ThemePreference::Light
    });

    let mut visuals = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };

    if dark {
        visuals.panel_fill = panel_fill(true);
        visuals.window_fill = Color32::from_rgb(34, 37, 42);
        visuals.faint_bg_color = Color32::from_rgb(38, 41, 46);
        visuals.extreme_bg_color = Color32::from_rgb(24, 26, 30);
        visuals.widgets.noninteractive.bg_stroke =
            egui::Stroke::new(1.0, Color32::from_rgb(48, 52, 58));
    } else {
        visuals.panel_fill = panel_fill(false);
        visuals.window_fill = Color32::from_rgb(255, 255, 255);
        visuals.faint_bg_color = Color32::from_rgb(243, 244, 246);
        visuals.extreme_bg_color = Color32::from_rgb(255, 255, 255);
        visuals.widgets.noninteractive.bg_stroke =
            egui::Stroke::new(1.0, Color32::from_rgb(224, 226, 230));
    }

    let mut style = (*ctx.global_style()).clone();
    style.visuals = visuals;
    // 按钮一悬停就换手型光标，跟浏览器一致；自绘的行和页签不吃这条，
    // 得各自 .on_hover_cursor(HAND)
    style.visuals.interact_cursor = Some(egui::CursorIcon::PointingHand);
    style.spacing.item_spacing = egui::vec2(6.0, 5.0);
    style.spacing.button_padding = egui::vec2(8.0, 3.0);
    style.spacing.menu_margin = egui::Margin::same(6);
    style
        .text_styles
        .insert(TextStyle::Body, FontId::proportional(13.5));
    style
        .text_styles
        .insert(TextStyle::Button, FontId::proportional(13.5));
    style
        .text_styles
        .insert(TextStyle::Small, FontId::proportional(11.5));
    style
        .text_styles
        .insert(TextStyle::Heading, FontId::proportional(18.0));
    style.text_styles.insert(
        TextStyle::Monospace,
        FontId::new(13.0, FontFamily::Monospace),
    );
    ctx.set_global_style(style);
}
