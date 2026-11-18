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

/// 终端窗口的标题前缀。
///
/// 那几扇是真正的操作系统窗口，egui 只负责往里画内容，标题栏照旧归 DWM。
/// 而 egui 又不把子视口的句柄交给应用，所以只能按这个前缀去本进程的顶层窗口里认人。
pub const CHILD_PREFIX: &str = "终端 · ";

/// 把主题同步给所有终端窗口。`skip` 是主窗口，它有自己的 [`apply`] 路径。
#[cfg(windows)]
pub fn apply_children(skip: Option<isize>, dark: bool) {
    let (caption, text) = palette(dark);
    win::paint_matching(
        std::process::id(),
        skip.unwrap_or(0),
        CHILD_PREFIX,
        dark,
        colorref(caption),
        colorref(text),
    );
}

#[cfg(not(windows))]
pub fn apply_children(_skip: Option<isize>, _dark: bool) {}

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

    #[link(name = "user32")]
    extern "system" {
        fn EnumWindows(callback: EnumProc, lparam: isize) -> i32;
        fn GetWindowThreadProcessId(hwnd: *mut c_void, pid: *mut u32) -> u32;
        fn GetWindowTextLengthW(hwnd: *mut c_void) -> i32;
        fn GetWindowTextW(hwnd: *mut c_void, buffer: *mut u16, max_chars: i32) -> i32;
        fn IsWindowVisible(hwnd: *mut c_void) -> i32;
    }

    type EnumProc = extern "system" fn(hwnd: *mut c_void, lparam: isize) -> i32;

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

    /// `EnumWindows` 的回调没有捕获，要带的东西全放这儿，靠 LPARAM 递进去。
    struct Sweep<'a> {
        pid: u32,
        skip: *mut c_void,
        prefix: &'a str,
        dark: bool,
        caption: u32,
        text: u32,
        painted: usize,
    }

    extern "system" fn on_window(hwnd: *mut c_void, lparam: isize) -> i32 {
        let sweep = unsafe { &mut *(lparam as *mut Sweep<'_>) };
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
        // 只认自己进程的窗口：别人的终端窗口不该被我们刷色。
        // 主窗口和输入法、剪贴板那些钩子窗口也一并挡掉——后者根本没有标题。
        let title = match title_of(hwnd) {
            Some(t) if pid == sweep.pid && hwnd != sweep.skip => t,
            _ => return 1,
        };
        if title.starts_with(sweep.prefix) {
            set_all(
                hwnd as isize,
                sweep.dark,
                sweep.caption,
                sweep.text,
            );
            sweep.painted += 1;
        }
        1
    }

    fn title_of(hwnd: *mut c_void) -> Option<String> {
        if unsafe { IsWindowVisible(hwnd) } == 0 {
            return None;
        }
        let len = unsafe { GetWindowTextLengthW(hwnd) };
        if len <= 0 {
            return None;
        }
        let mut buffer = vec![0u16; (len + 1) as usize];
        let written =
            unsafe { GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
        if written <= 0 {
            return None;
        }
        Some(String::from_utf16_lossy(&buffer[..written as usize]))
    }

    pub fn paint_matching(
        pid: u32,
        skip: isize,
        prefix: &str,
        dark: bool,
        caption: u32,
        text: u32,
    ) -> usize {
        let mut sweep = Sweep {
            pid,
            skip: skip as *mut c_void,
            prefix,
            dark,
            caption,
            text,
            painted: 0,
        };
        unsafe { EnumWindows(on_window, &mut sweep as *mut Sweep<'_> as isize) };
        sweep.painted
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
