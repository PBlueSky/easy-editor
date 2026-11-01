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

fn main() -> eframe::Result<()> {
    let target = std::env::args().nth(1).map(std::path::PathBuf::from);

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1400.0, 880.0])
            .with_min_inner_size([900.0, 560.0])
            .with_title("EasyEditor"),
        ..Default::default()
    };

    eframe::run_native(
        "EasyEditor",
        options,
        Box::new(move |cc| Ok(Box::new(app::EasyEditorApp::new(cc, target)))),
    )
}
