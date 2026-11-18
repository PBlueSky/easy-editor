//! 系统标题栏（图标 + 标题 + 最小化/最大化/关闭那一行）的配色。
//!
//! 这一行由 Windows 的 DWM 画，不归 egui 管，它默认只看**系统**深浅色设置，
//! 不看应用自己的主题。系统深色 + 应用浅色时，DWM 给出浅底却按深色调配按钮字形，
//! 白字压在白底上，最小化/最大化按钮就「看不见」了；应用切到深色，标题栏又还是白的。
//! 所以主题一变，就得把 DWM 的属性同步过去。

use egui::Color32;

/// 标题栏底色 / 标题文字色。底色直接取面板底色，
/// 这样标题栏和顶栏连成一片，看不出接缝。
pub fn palette(dark: bool) -> (Color32, Color32) {
    let text = if dark {
        Color32::from_rgb(205, 210, 216)
    } else {
        Color32::from_rgb(32, 34, 38)
    };
    (crate::theme::panel_fill(dark), text)
}

/// DWM 收的是 COLORREF（`0x00BBGGRR`），字节序和 egui 的 RGB 相反。
fn colorref(c: Color32) -> u32 {
    (u32::from(c.b()) << 16) | (u32::from(c.g()) << 8) | u32::from(c.r())
}

/// 把标题栏刷成当前主题。`hwnd` 为 `None`（无头测试、非 Windows）时什么都不做。
#[cfg(windows)]
pub fn apply(hwnd: Option<isize>, dark: bool) {
    let Some(hwnd) = hwnd else { return };
    let (caption, text) = palette(dark);
    win::set_all(hwnd, dark, colorref(caption), colorref(text));
}

#[cfg(not(windows))]
pub fn apply(_hwnd: Option<isize>, _dark: bool) {}

#[cfg(windows)]
mod win {
    use std::ffi::c_void;

    #[link(name = "dwmapi")]
    extern "system" {
        fn DwmSetWindowAttribute(
            hwnd: *mut c_void,
            attr: u32,
            value: *const c_void,
            size: u32,
        ) -> i32;
    }

    /// 深色标题栏开关。Windows 10 1903/1909 用的是 19，2004 起改成 20，两个都试一次。
    const USE_IMMERSIVE_DARK_MODE: u32 = 20;
    const USE_IMMERSIVE_DARK_MODE_LEGACY: u32 = 19;
    /// 下面两个要 Windows 11 22000+，老系统返回失败直接忽略。
    const CAPTION_COLOR: u32 = 35;
    const TEXT_COLOR: u32 = 36;

    fn set(hwnd: *mut c_void, attr: u32, value: i32) -> bool {
        unsafe {
            DwmSetWindowAttribute(
                hwnd,
                attr,
                &value as *const i32 as *const c_void,
                std::mem::size_of::<i32>() as u32,
            ) == 0
        }
    }

    pub fn set_all(hwnd: isize, dark: bool, caption: u32, text: u32) {
        let hwnd = hwnd as *mut c_void;
        let on = i32::from(dark);
        if !set(hwnd, USE_IMMERSIVE_DARK_MODE, on) {
            let _ = set(hwnd, USE_IMMERSIVE_DARK_MODE_LEGACY, on);
        }
        let _ = set(hwnd, CAPTION_COLOR, caption as i32);
        let _ = set(hwnd, TEXT_COLOR, text as i32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colorref_swaps_to_blue_green_red() {
        assert_eq!(colorref(Color32::from_rgb(0x12, 0x34, 0x56)), 0x0056_3412);
        assert_eq!(colorref(Color32::BLACK), 0);
    }

    /// 标题栏底色必须和面板底色一致，否则顶栏和标题栏之间会多出一条色差带。
    #[test]
    fn caption_matches_the_panel_fill() {
        let ctx = egui::Context::default();
        crate::theme::apply_style(&ctx, true, crate::theme::UI_FONT_DEFAULT);
        assert_eq!(palette(true).0, ctx.global_style().visuals.panel_fill);
        crate::theme::apply_style(&ctx, false, crate::theme::UI_FONT_DEFAULT);
        assert_eq!(palette(false).0, ctx.global_style().visuals.panel_fill);
    }
}
