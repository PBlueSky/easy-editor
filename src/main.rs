#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod config;
mod editor;
mod filetree;
mod find;
mod format;
mod journal;
mod keymap;
mod nav;
mod pane;
mod search;
mod syntax;
mod terminal;
mod theme;
mod titlebar;

fn main() -> eframe::Result<()> {
    let target = std::env::args().nth(1).map(std::path::PathBuf::from);

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1400.0, 880.0])
            .with_min_inner_size([900.0, 560.0])
            .with_title("EasyEditor")
            .with_icon(window_icon()),
        ..Default::default()
    };

    eframe::run_native(
        "EasyEditor",
        options,
        Box::new(move |cc| Ok(Box::new(app::EasyEditorApp::new(cc, target)))),
    )
}

/// 窗口标题栏和任务栏用的图标。
///
/// exe 文件在资源管理器里的图标是另一回事，那份由 build.rs 把 assets/easy-editor.ico
/// 编进 PE 资源；运行时这份得自己给。源图和重新生成流程见 tools/render-icon.ps1。
fn window_icon() -> eframe::egui::IconData {
    let png = include_bytes!("../assets/easy-editor-256.png");
    eframe::icon_data::from_png_bytes(png)
        .expect("assets/easy-editor-256.png 不是能解码的 PNG，重新跑 tools/render-icon.ps1")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 出过一档「图标只剩一半」：无头 Chrome 在 `--window-size=256` 下把整张画往右推、
    /// 右边裁掉，标题栏和任务栏拿到的就是那张半截图。现在 256 由 tools/make-ico.mjs
    /// 从 1024 母图缩小，这里兜底盯住构图：四边留边要相等且不贴到画布边缘。
    #[test]
    fn window_icon_is_centered_and_not_clipped() {
        let icon = window_icon();
        let size = icon.width as usize;
        assert_eq!((size, icon.height as usize), (256, 256));

        let (mut min_x, mut min_y) = (size, size);
        let (mut max_x, mut max_y) = (0usize, 0usize);
        for y in 0..size {
            for x in 0..size {
                if icon.rgba[(y * size + x) * 4 + 3] > 8 {
                    min_x = min_x.min(x);
                    max_x = max_x.max(x);
                    min_y = min_y.min(y);
                    max_y = max_y.max(y);
                }
            }
        }

        // SVG 的徽章在 1024 格上留 32 的边，等比到这张图就是 size / 32。
        let want = size / 32;
        let sides = [
            ("左", min_x),
            ("上", min_y),
            ("右", size - 1 - max_x),
            ("下", size - 1 - max_y),
        ];
        for (side, got) in sides {
            assert!(
                got.abs_diff(want) <= 2,
                "图标{side}边留了 {got}px，应为 {want}±2（构图整体偏移或被裁切，重跑 tools/render-icon.ps1）；\
                 不透明区 x={min_x}..{max_x} y={min_y}..{max_y}"
            );
        }
    }
}
