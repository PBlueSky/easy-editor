//! 文件夹树：walkdir 读取目录，懒加载 + 展开状态。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use eframe::egui::{self, Align2, Color32, FontId, Rect, RichText, Sense, Ui, Vec2};
use walkdir::WalkDir;

use crate::format;

/// 遍历搜索时跳过的重型 / 版本控制目录。
const SKIP_DIRS: [&str; 9] = [
    ".git",
    "target",
    "node_modules",
    "dist",
    "build",
    ".idea",
    ".vscode",
    "__pycache__",
    ".venv",
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EntryKind {
    Dir,
    /// 可执行文件（.exe/.bat/.cmd/.ps1… 或带执行位的文件）
    Exec,
    File,
}

#[derive(Clone)]
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    pub kind: EntryKind,
}

/// 类似 shell 里 `ls --color` 的配色：目录蓝、可执行绿、普通文件用正文色。
fn kind_color(kind: EntryKind, dark: bool) -> Color32 {
    match (kind, dark) {
        (EntryKind::Dir, false) => Color32::from_rgb(11, 98, 196),
        (EntryKind::Dir, true) => Color32::from_rgb(108, 182, 255),
        (EntryKind::Exec, false) => Color32::from_rgb(17, 122, 61),
        (EntryKind::Exec, true) => Color32::from_rgb(126, 231, 135),
        (EntryKind::File, false) => Color32::from_rgb(60, 66, 74),
        (EntryKind::File, true) => Color32::from_rgb(178, 186, 196),
    }
}

/// 判断是否为可执行文件。Windows 按扩展名，类 Unix 再看执行位。
fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            if meta.permissions().mode() & 0o111 != 0 {
                return true;
            }
        }
    }
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "exe" | "bat" | "cmd" | "com" | "ps1" | "msi" | "scr" | "vbs" | "sh"
    )
}

/// 文件树操作结果。
pub enum TreeAction {
    Open(PathBuf),
    Created(PathBuf),
    /// 右键「格式化」：JSON / XML 文件原地格式化
    Format(PathBuf),
}

pub struct FileTree {
    pub root: PathBuf,
    children: HashMap<PathBuf, Vec<Entry>>,
    pub expanded: HashSet<PathBuf>,
    pub selected: Option<PathBuf>,
    /// (父目录, 是否为目录)
    new_entry: Option<(PathBuf, bool)>,
    /// 新建对话框里正在输入的名字
    pub new_name: String,
    /// 文件搜索框：按名称 / 相对路径片段过滤整个工作区
    pub filter: String,
    /// 上次搜索结果，`(查询串, 命中)`；查询没变就不重新遍历目录
    filter_cache: Option<(String, Vec<Entry>)>,
    pub show_hidden: bool,
    pub error: Option<String>,
}

impl FileTree {
    pub fn new(root: PathBuf) -> Self {
        let mut tree = Self {
            root: root.clone(),
            children: HashMap::new(),
            expanded: HashSet::new(),
            selected: None,
            new_entry: None,
            new_name: String::new(),
            filter: String::new(),
            filter_cache: None,
            show_hidden: false,
            error: None,
        };
        tree.expanded.insert(root);
        tree
    }

    pub fn refresh(&mut self) {
        self.children.clear();
        self.filter_cache = None;
    }

    fn read(&self, dir: &Path) -> Vec<Entry> {
        let mut items: Vec<Entry> = WalkDir::new(dir)
            .min_depth(1)
            .max_depth(1)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_dir() || e.file_type().is_file())
            .filter(|e| {
                if self.show_hidden {
                    return true;
                }
                // 默认隐藏 .git / .workbuddy-ai 这类点开头的条目
                !e.file_name().to_string_lossy().starts_with('.')
            })
            .map(|e| {
                let is_dir = e.file_type().is_dir();
                let kind = if is_dir {
                    EntryKind::Dir
                } else if is_executable(e.path()) {
                    EntryKind::Exec
                } else {
                    EntryKind::File
                };
                Entry {
                    name: e.file_name().to_string_lossy().to_string(),
                    path: e.path().to_path_buf(),
                    is_dir,
                    kind,
                }
            })
            .collect();
        items.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        items
    }

    fn children_of(&mut self, dir: &Path) -> Vec<Entry> {
        if let Some(items) = self.children.get(dir) {
            return items.clone();
        }
        let items = self.read(dir);
        self.children.insert(dir.to_path_buf(), items.clone());
        items
    }

    /// 请求新建文件/文件夹（供工具栏与快捷键调用）。
    pub fn request_new(&mut self, is_dir: bool) {
        let parent = self
            .selected
            .clone()
            .filter(|p| p.is_dir())
            .or_else(|| {
                self.selected
                    .clone()
                    .and_then(|p| p.parent().map(|x| x.to_path_buf()))
            })
            .unwrap_or_else(|| self.root.clone());
        self.new_entry = Some((parent, is_dir));
        self.new_name.clear();
    }

    /// 返回需要打开或新建完成后的文件。
    pub fn ui(&mut self, ui: &mut Ui, dark: bool) -> Option<TreeAction> {
        let mut action = None;

        ui.horizontal(|ui| {
            ui.add_space(2.0);
            let name = self
                .root
                .file_name()
                .map(|s| s.to_string_lossy().to_uppercase())
                .unwrap_or_else(|| "工作区".into());
            ui.label(RichText::new(name).strong().size(12.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add(egui::Button::new(RichText::new("刷新").size(11.5)).frame(false))
                    .on_hover_text("刷新目录")
                    .clicked()
                {
                    self.refresh();
                }
                if ui
                    .add(egui::Button::new(RichText::new("新文件夹").size(11.5)).frame(false))
                    .on_hover_text("新建文件夹 Ctrl+Shift+N")
                    .clicked()
                {
                    self.request_new(true);
                }
                if ui
                    .add(egui::Button::new(RichText::new("新文件").size(11.5)).frame(false))
                    .on_hover_text("新建文件 Ctrl+N")
                    .clicked()
                {
                    self.request_new(false);
                }
            });
        });

        // 根目录也支持右键新建，工作区为空时也能操作
        let (root_rect, root_resp) = ui.allocate_exact_size(
            Vec2::new(ui.available_width(), 2.0),
            Sense::click(),
        );
        let _ = root_rect;
        root_resp.context_menu(|ui| {
            if ui.button("在根目录新建文件").clicked() {
                self.new_entry = Some((self.root.clone(), false));
                self.new_name.clear();
                ui.close();
            }
            if ui.button("在根目录新建文件夹").clicked() {
                self.new_entry = Some((self.root.clone(), true));
                self.new_name.clear();
                ui.close();
            }
            ui.separator();
            let mut hidden = self.show_hidden;
            if ui.checkbox(&mut hidden, "显示隐藏文件").changed() {
                self.show_hidden = hidden;
                self.refresh();
            }
            if ui.button("刷新").clicked() {
                self.refresh();
                ui.close();
            }
        });

        ui.add_space(2.0);

        // 文件搜索：输入片段后在整个工作区里按名称 / 相对路径过滤
        ui.horizontal(|ui| {
            ui.add_space(2.0);
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text("搜索文件（名称或路径片段）")
                    .desired_width((ui.available_width() - 4.0).max(60.0)),
            );
        });

        egui::ScrollArea::both()
            .id_salt("file_tree_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if self.filter.trim().is_empty() {
                    let root = self.root.clone();
                    self.render_dir(ui, &root, 0, dark, &mut action);
                } else {
                    self.render_search_results(ui, dark, &mut action);
                }
            });

        if let Some((parent, is_dir)) = self.new_entry.clone() {
            self.show_new_dialog(ui.ctx(), parent, is_dir, &mut action);
        }

        action
    }

    /// 扫描整个工作区，返回名称或相对路径包含筛选词的条目。
    /// 结果缓存到查询串变化为止，打字时不会每帧重新遍历目录。
    fn matching_entries(&mut self) -> Vec<Entry> {
        let query = self.filter.trim().to_lowercase();
        if query.is_empty() {
            return Vec::new();
        }
        if let Some((cached, hits)) = &self.filter_cache {
            if *cached == query {
                return hits.clone();
            }
        }
        let show_hidden = self.show_hidden;
        let mut hits: Vec<Entry> = Vec::new();
        let walker = WalkDir::new(&self.root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                if e.depth() == 0 {
                    return true;
                }
                let name = e.file_name().to_string_lossy();
                if !show_hidden && name.starts_with('.') {
                    return false;
                }
                !(e.file_type().is_dir() && SKIP_DIRS.contains(&name.as_ref()))
            });
        for e in walker.filter_map(|e| e.ok()) {
            if e.depth() == 0 || !(e.file_type().is_dir() || e.file_type().is_file()) {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_string();
            let rel = relative(&self.root, e.path());
            if !name.to_lowercase().contains(&query) && !rel.to_lowercase().contains(&query) {
                continue;
            }
            let is_dir = e.file_type().is_dir();
            let kind = if is_dir {
                EntryKind::Dir
            } else if is_executable(e.path()) {
                EntryKind::Exec
            } else {
                EntryKind::File
            };
            hits.push(Entry {
                name,
                path: e.path().to_path_buf(),
                is_dir,
                kind,
            });
            if hits.len() >= 500 {
                break;
            }
        }
        hits.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.path.cmp(&b.path))
        });
        self.filter_cache = Some((query, hits.clone()));
        hits
    }

    fn render_search_results(
        &mut self,
        ui: &mut Ui,
        dark: bool,
        out: &mut Option<TreeAction>,
    ) {
        let hits = self.matching_entries();
        if hits.is_empty() {
            ui.add_space(8.0);
            ui.label(RichText::new("没有匹配的文件").weak().size(12.0));
            return;
        }
        let font = FontId::proportional(12.5);
        let sub_font = FontId::proportional(11.0);
        for entry in hits {
            let row_h = 20.0;
            let width = ui.available_width().max(120.0);
            let (rect, response) =
                ui.allocate_exact_size(Vec2::new(width, row_h), Sense::click());

            if self.selected.as_deref() == Some(entry.path.as_path()) {
                ui.painter()
                    .rect_filled(rect, 3.0, ui.visuals().selection.bg_fill.gamma_multiply(0.35));
            } else if response.hovered() {
                ui.painter()
                    .rect_filled(rect, 3.0, ui.visuals().widgets.hovered.weak_bg_fill);
            }

            let color = kind_color(entry.kind, dark);
            let galley =
                ui.fonts_mut(|f| f.layout_no_wrap(entry.name.clone(), font.clone(), color));
            let name_width = galley.size().x;
            let painter = ui.painter();
            painter.galley(
                egui::pos2(rect.left() + 6.0, rect.center().y - galley.size().y * 0.5),
                galley,
                color,
            );

            let rel = relative(&self.root, &entry.path);
            let dir = rel.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
            if !dir.is_empty() {
                painter.text(
                    egui::pos2(rect.left() + 6.0 + name_width + 10.0, rect.center().y),
                    Align2::LEFT_CENTER,
                    dir,
                    sub_font.clone(),
                    color.gamma_multiply(0.6),
                );
            }

            if response.clicked() {
                self.selected = Some(entry.path.clone());
                if entry.is_dir {
                    // 点目录就展开到它在树里的位置，顺手退出搜索
                    self.expand_to(&entry.path);
                    self.filter.clear();
                    self.filter_cache = None;
                } else {
                    *out = Some(TreeAction::Open(entry.path.clone()));
                }
            }
        }
    }

    /// 展开从根目录到 `path` 的所有父级，供搜索结果跳回树视图。
    fn expand_to(&mut self, path: &Path) {
        let mut cur = path.parent();
        while let Some(p) = cur {
            if !p.starts_with(&self.root) {
                break;
            }
            self.expanded.insert(p.to_path_buf());
            if p == self.root {
                break;
            }
            cur = p.parent();
        }
    }

    fn render_dir(
        &mut self,
        ui: &mut Ui,
        dir: &Path,
        depth: usize,
        dark: bool,
        out: &mut Option<TreeAction>,
    ) {
        let items = self.children_of(dir);
        for entry in items {
            let indent = 6.0 + depth as f32 * 13.0;
            let row_h = 20.0;
            let width = ui.available_width().max(120.0);
            let (rect, response) =
                ui.allocate_exact_size(Vec2::new(width, row_h), Sense::click());

            let hovered = response.hovered();
            let selected = self.selected.as_deref() == Some(entry.path.as_path());

            if selected {
                ui.painter()
                    .rect_filled(rect, 3.0, ui.visuals().selection.bg_fill.gamma_multiply(0.35));
            } else if hovered {
                ui.painter()
                    .rect_filled(rect, 3.0, ui.visuals().widgets.hovered.weak_bg_fill);
            }

            let text_color = kind_color(entry.kind, dark);

            let expanded = self.expanded.contains(&entry.path);
            let arrow = if entry.is_dir {
                if expanded {
                    "▾"
                } else {
                    "▸"
                }
            } else {
                " "
            };

            let painter = ui.painter();
            painter.text(
                egui::pos2(rect.left() + indent, rect.center().y),
                Align2::LEFT_CENTER,
                arrow,
                FontId::proportional(10.0),
                text_color.gamma_multiply(0.8),
            );
            painter.text(
                egui::pos2(rect.left() + indent + 13.0, rect.center().y),
                Align2::LEFT_CENTER,
                &entry.name,
                FontId::proportional(12.5),
                text_color,
            );

            if response.clicked() {
                self.selected = Some(entry.path.clone());
                if entry.is_dir {
                    if expanded {
                        self.expanded.remove(&entry.path);
                    } else {
                        self.expanded.insert(entry.path.clone());
                    }
                } else {
                    *out = Some(TreeAction::Open(entry.path.clone()));
                }
            }

            response.context_menu(|ui| {
                let target_dir = if entry.is_dir {
                    entry.path.clone()
                } else {
                    entry.path.parent().map(|p| p.to_path_buf()).unwrap_or_default()
                };
                if ui.button("新建文件").clicked() {
                    self.new_entry = Some((target_dir.clone(), false));
                    self.new_name.clear();
                    ui.close();
                }
                if ui.button("新建文件夹").clicked() {
                    self.new_entry = Some((target_dir.clone(), true));
                    self.new_name.clear();
                    ui.close();
                }
                ui.separator();
                if !entry.is_dir && format::supports(&entry.path) && ui.button("格式化").clicked() {
                    *out = Some(TreeAction::Format(entry.path.clone()));
                    ui.close();
                }
                if !entry.is_dir && ui.button("重命名").clicked() {
                    self.selected = Some(entry.path.clone());
                    ui.close();
                }
                if ui.button("在资源管理器中显示").clicked() {
                    reveal(&entry.path);
                    ui.close();
                }
                if ui.button("在此处打开终端").clicked() {
                    reveal(&target_dir);
                    ui.close();
                }
            });

            if entry.is_dir && self.expanded.contains(&entry.path) {
                self.render_dir(ui, &entry.path, depth + 1, dark, out);
            }
        }
    }

    fn show_new_dialog(
        &mut self,
        ctx: &egui::Context,
        parent: PathBuf,
        is_dir: bool,
        out: &mut Option<TreeAction>,
    ) {
        let mut close = false;
        let mut create = false;
        egui::Window::new(if is_dir { "新建文件夹" } else { "新建文件" })
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label(format!("位置：{}", parent.to_string_lossy()));
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.new_name)
                        .hint_text(if is_dir { "文件夹名称" } else { "文件名，如 main.rs" })
                        .desired_width(300.0),
                );
                resp.request_focus();
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    close = true;
                }
                if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    create = true;
                }
                ui.horizontal(|ui| {
                    if ui.button("创建").clicked() {
                        create = true;
                    }
                    if ui.button("取消").clicked() {
                        close = true;
                    }
                });
            });

        if create && !self.new_name.trim().is_empty() {
            let path = parent.join(self.new_name.trim());
            let result = if is_dir {
                std::fs::create_dir_all(&path)
            } else {
                if let Some(p) = path.parent() {
                    let _ = std::fs::create_dir_all(p);
                }
                if path.exists() {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        "同名文件已存在",
                    ))
                } else {
                    std::fs::write(&path, "")
                }
            };
            match result {
                Ok(_) => {
                    self.refresh();
                    self.expanded.insert(parent.clone());
                    self.selected = Some(path.clone());
                    if !is_dir {
                        *out = Some(TreeAction::Created(path));
                    }
                    self.error = None;
                }
                Err(e) => self.error = Some(format!("创建失败：{e}")),
            }
            close = true;
        }

        if close {
            self.new_entry = None;
            self.new_name.clear();
        }
    }
}

fn reveal(path: &Path) {
    #[cfg(target_os = "windows")]
    {
        let arg = if path.is_dir() {
            path.to_path_buf()
        } else {
            path.parent().map(|p| p.to_path_buf()).unwrap_or_default()
        };
        let _ = std::process::Command::new("explorer").arg(arg).spawn();
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = path;
    }
}

/// 收集工作区所有文件（用于快速打开），跳过重型目录。
pub fn collect_files(root: &Path, limit: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let walker = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            if e.file_type().is_dir() && e.depth() > 0 {
                let name = e.file_name().to_string_lossy();
                !SKIP_DIRS.contains(&name.as_ref())
            } else {
                true
            }
        });
    for entry in walker.filter_map(|e| e.ok()) {
        if entry.file_type().is_file() {
            out.push(entry.path().to_path_buf());
            if out.len() >= limit {
                break;
            }
        }
    }
    out
}

pub fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| path.to_string_lossy().to_string())
}

#[allow(dead_code)]
pub fn row_rect(ui: &Ui) -> Rect {
    Rect::from_min_size(ui.cursor().min, Vec2::new(ui.available_width(), 20.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Vec2 = Vec2::new(420.0, 700.0);

    /// 走 egui 正常的输入管线跑一帧：事件必须是 RawInput 里的，
    /// 因为指针状态在 begin_pass 就算好了，事后往 events 里塞是没用的。
    fn pass(ctx: &egui::Context, tree: &mut FileTree, events: Vec<egui::Event>) -> egui::FullOutput {
        let raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, SCREEN)),
            events,
            ..Default::default()
        };
        let mut out = ctx.run_pass(raw, |ctx| {
            ctx.root_ui(|ui| {
                egui::Panel::left("side").show(ui, |ui| {
                    let _ = tree.ui(ui, false);
                });
            });
        });
        // 测试里没有真正的渲染器，字体图集的增量没人消费，
        // epaint 会在 TexturesDelta 被 drop 时断言，这里手动清掉。
        out.textures_delta.clear();
        out
    }

    fn pointer(pos: egui::Pos2, button: egui::PointerButton, pressed: bool) -> Vec<egui::Event> {
        vec![
            egui::Event::PointerMoved(pos),
            egui::Event::PointerButton {
                pos,
                button,
                pressed,
                modifiers: egui::Modifiers::NONE,
            },
        ]
    }

    /// 在渲染结果里找一段文字的位置，用来算「该点哪里」。
    fn find_text(out: &egui::FullOutput, needle: &str) -> Option<Rect> {
        fn walk(shape: &egui::Shape, needle: &str) -> Option<Rect> {
            match shape {
                egui::Shape::Text(t) => {
                    (t.galley.job.text == needle).then(|| t.galley.rect.translate(t.pos.to_vec2()))
                }
                egui::Shape::Vec(shapes) => shapes.iter().find_map(|s| walk(s, needle)),
                _ => None,
            }
        }
        out.shapes.iter().find_map(|c| walk(&c.shape, needle))
    }

    /// 菜单弹出的帧数不固定，多跑几帧等到它出现。
    fn wait_for_text(
        ctx: &egui::Context,
        tree: &mut FileTree,
        needle: &str,
    ) -> Option<Rect> {
        for _ in 0..4 {
            let out = pass(ctx, tree, vec![]);
            if let Some(rect) = find_text(&out, needle) {
                return Some(rect);
            }
        }
        None
    }

    #[test]
    fn right_click_new_file_dialog_accepts_typing() {
        let dir = std::env::temp_dir().join("easy-editor-tree-menu-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "a\n").unwrap();

        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(1.0);
        let mut tree = FileTree::new(dir.clone());

        let out = pass(&ctx, &mut tree, vec![]);
        let row = find_text(&out, "a.txt")
            .expect("文件树里没画出 a.txt")
            .center();

        pass(&ctx, &mut tree, pointer(row, egui::PointerButton::Secondary, true));
        pass(&ctx, &mut tree, pointer(row, egui::PointerButton::Secondary, false));
        let item = wait_for_text(&ctx, &mut tree, "新建文件")
            .expect("右键没有弹出「新建文件」菜单")
            .center();

        pass(&ctx, &mut tree, pointer(item, egui::PointerButton::Primary, true));
        pass(&ctx, &mut tree, pointer(item, egui::PointerButton::Primary, false));
        // Window 的第一帧只量尺寸不画内容，再跑一帧才能看到它
        let out = pass(&ctx, &mut tree, vec![]);
        assert!(tree.new_entry.is_some(), "点了菜单项却没记下新建目标");
        assert!(
            find_text(&out, "文件名，如 main.rs").is_some(),
            "新建对话框没画出来"
        );

        pass(
            &ctx,
            &mut tree,
            vec![egui::Event::Text("probe.txt".into())],
        );
        assert_eq!(
            tree.new_name, "probe.txt",
            "对话框里的输入框没收到键盘文字"
        );

        pass(
            &ctx,
            &mut tree,
            vec![egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        assert!(dir.join("probe.txt").is_file(), "回车没能把文件建出来");
    }

    /// 文件搜索按名称和相对路径片段匹配，并且不会漏掉子目录里的文件。
    #[test]
    fn filter_matches_names_and_relative_paths() {
        let dir = std::env::temp_dir().join("easy-editor-tree-filter-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join("src/lib.rs"), "pub fn x() {}\n").unwrap();
        std::fs::write(dir.join("notes.txt"), "hello\n").unwrap();

        let mut tree = FileTree::new(dir.clone());

        tree.filter = "main".into();
        let hits = tree.matching_entries();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "main.rs");

        // 路径片段同样能搜到子目录里的文件
        tree.filter = "src/lib".into();
        let hits = tree.matching_entries();
        assert!(hits.iter().any(|e| e.path.ends_with("src/lib.rs")));

        // 大小写不敏感
        tree.filter = "NOTES".into();
        let hits = tree.matching_entries();
        assert!(hits.iter().any(|e| e.name == "notes.txt"));

        tree.filter = "不存在的文件".into();
        assert!(tree.matching_entries().is_empty());
    }
}
