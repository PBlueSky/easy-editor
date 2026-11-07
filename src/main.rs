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
