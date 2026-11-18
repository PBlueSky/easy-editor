//! 文件夹树：walkdir 读取目录，懒加载 + 展开状态。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use eframe::egui::{self, Align2, Color32, FontId, Rect, RichText, Sense, Ui, Vec2};
use walkdir::WalkDir;

use crate::format;
use crate::ime;

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
/// 树里一行的字号和行高。
///
/// 行高由字号量出来，不是写死的 20：界面字号调到 16 时，20 那一格会把字挤成半高。
/// 多给的 5 点是行内上下留白，让选中底色比字稍高一点。
fn row_metrics(ui: &mut Ui) -> (FontId, f32) {
    let font = FontId::proportional(crate::theme::md(ui));
    let row_h = ui.fonts_mut(|f| f.row_height(&font)) + 5.0;
    (font, row_h)
}

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
#[derive(Debug)]
pub enum TreeAction {
    Open(PathBuf),
    Created(PathBuf),
    /// 右键「格式化」：JSON / XML 文件原地格式化
    Format(PathBuf),
    /// 右键「重命名」改名成功：旧路径 → 新路径，已经打开的标签要跟着换路径
    Renamed(PathBuf, PathBuf),
    /// 右键「在此处打开终端」：在这个目录里起一块新终端
    OpenTerminal(PathBuf),
    /// 右键「新建 SQL 查询」：文件由 app 层直接建好，这里只报目录
    NewQuery(PathBuf),
}

/// 校验改名输入：去掉首尾空白，空串 / `.` / `..` / 带路径分隔符或 Windows 非法字符的都算不合法。
fn sanitized_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        return None;
    }
    if trimmed
        .chars()
        .any(|c| matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
    {
        return None;
    }
    Some(trimmed.to_string())
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
    /// 正在就地改名的文件（改名输入框画在它那一行上）
    renaming: Option<PathBuf>,
    /// 改名输入框里的新名字
    rename_buf: String,
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
            renaming: None,
            rename_buf: String::new(),
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
        self.cancel_rename();
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
        // 同时只开一个输入框，免得两个都盯着键盘焦点
        self.cancel_rename();
    }

    /// 请求就地改名：输入框先填上现在的名字。
    pub fn start_rename(&mut self, path: &Path) {
        self.rename_buf = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        self.renaming = Some(path.to_path_buf());
        self.selected = Some(path.to_path_buf());
        self.error = None;
    }

    pub fn cancel_rename(&mut self) {
        self.renaming = None;
        self.rename_buf.clear();
    }

    /// 落地改名：磁盘 `rename` 成功后把「旧路径 → 新路径」交给上层换标签页。
    fn commit_rename(&mut self, out: &mut Option<TreeAction>) {
        let Some(old) = self.renaming.take() else {
            return;
        };
        let raw = std::mem::take(&mut self.rename_buf);
        let Some(name) = sanitized_name(&raw) else {
            self.error = Some(format!("非法的文件名：「{}」", raw.trim()));
            return;
        };
        let Some(parent) = old.parent() else {
            return;
        };
        let new = parent.join(&name);
        if new == old {
            return;
        }
        if new.exists() {
            self.error = Some(format!("「{name}」已经存在"));
            return;
        }
        match std::fs::rename(&old, &new) {
            Ok(()) => {
                self.refresh();
                self.selected = Some(new.clone());
                self.error = None;
                *out = Some(TreeAction::Renamed(old, new));
            }
            Err(e) => self.error = Some(format!("重命名失败：{e}")),
        }
    }

    /// 返回需要打开或新建完成后的文件。
    pub fn ui(&mut self, ui: &mut Ui, dark: bool) -> Option<TreeAction> {
        let mut action = None;

        // 一行两头：工作区名钉在最左，按钮从最右边往左排。
        // 名字不能跟按钮排在同一条横向布局里——那一坨按钮多宽要等排完才知道，
        // 名字一长就压到按钮上面；所以先排按钮，再把剩下的那块矩形交给名字。
        // 名字用 max_rect 单独摆：`Ui::put` 那套是居中+两端对齐的，
        // 只有显式的 left_to_right 子 ui 才真的把它按在最左边。
        // 外面这层 horizontal 不能省：它把行高定成一行，
        // 单用 with_layout 会让这一行吃掉面板剩下的全部高度、还被垂直居中到中间。
        let row = ui.available_rect_before_wrap();
        ui.horizontal(|ui| {
            let buttons = ui.scope_builder(
                egui::UiBuilder::new().layout(egui::Layout::right_to_left(egui::Align::Center)),
                |ui| {
                    ui.add_space(4.0);
                    if ui
                        .add(egui::Button::new(RichText::new("刷新").small()).frame(false))
                        .on_hover_text("刷新目录")
                        .clicked()
                    {
                        self.refresh();
                    }
                    if ui
                        .add(egui::Button::new(RichText::new("新文件夹").small()).frame(false))
                        .on_hover_text("新建文件夹 Ctrl+Shift+N")
                        .clicked()
                    {
                        self.request_new(true);
                    }
                    if ui
                        .add(egui::Button::new(RichText::new("新文件").small()).frame(false))
                        .on_hover_text("新建文件 Ctrl+N")
                        .clicked()
                    {
                        self.request_new(false);
                    }
                    ui.min_rect()
                },
            );
            // 名字剩下多少用多少，窄到没位置就干脆不画，别和按钮叠在一起。
            // 矩形的高度必须自己钉死在这一行上：`available_rect_before_wrap` 给的是
            // 面板剩下的整块，直接拿它的下沿会把名字撑成一层楼高，
            // 后面的搜索框和文件行全被顶到面板外面（egui 按 used_rect 记账）。
            let right = buttons.inner.left() - 6.0;
            let row_h = ui.spacing().interact_size.y.min(24.0);
            if right > row.left() + 24.0 {
                let rect = egui::Rect::from_min_size(
                    row.left_top(),
                    egui::vec2(right - row.left(), row_h),
                );
                ui.scope_builder(
                    egui::UiBuilder::new()
                        .max_rect(rect)
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                    |ui| {
                        let name = self
                            .root
                            .file_name()
                            .map(|s| s.to_string_lossy().to_uppercase())
                            .unwrap_or_else(|| "工作区".into());
                        let full = self.root.display().to_string();
                        ui.add(
                            egui::Label::new(
                                RichText::new(name).strong().size(crate::theme::md(ui)),
                            )
                                .truncate()
                                .selectable(false),
                        )
                        .on_hover_text(full);
                    },
                );
            }
        });

        // 根目录也支持右键新建，工作区为空时也能操作
        let (root_rect, root_resp) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 2.0), Sense::click());
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

    fn render_search_results(&mut self, ui: &mut Ui, dark: bool, out: &mut Option<TreeAction>) {
        let hits = self.matching_entries();
        if hits.is_empty() {
            ui.add_space(8.0);
            ui.label(RichText::new("没有匹配的文件").weak().size(crate::theme::md(ui)));
            return;
        }
        let (font, row_h) = row_metrics(ui);
        let sub_font = FontId::proportional(crate::theme::sm(ui));
        for entry in hits {
            let width = ui.available_width().max(120.0);
            let (rect, response) = ui.allocate_exact_size(Vec2::new(width, row_h), Sense::click());
            let response = response.on_hover_cursor(crate::theme::HAND);

            if self.selected.as_deref() == Some(entry.path.as_path()) {
                ui.painter().rect_filled(
                    rect,
                    3.0,
                    ui.visuals().selection.bg_fill.gamma_multiply(0.35),
                );
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
            let (name_font, row_h) = row_metrics(ui);
            let width = ui.available_width().max(120.0);
            let (rect, response) = ui.allocate_exact_size(Vec2::new(width, row_h), Sense::click());
            let response = response.on_hover_cursor(crate::theme::HAND);

            let hovered = response.hovered();
            let selected = self.selected.as_deref() == Some(entry.path.as_path());

            if selected {
                ui.painter().rect_filled(
                    rect,
                    3.0,
                    ui.visuals().selection.bg_fill.gamma_multiply(0.35),
                );
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

            // clone 一份：改名分支里还要 `ui.put`，不能一直借着 ui.painter()
            let painter = ui.painter().clone();
            painter.text(
                egui::pos2(rect.left() + indent, rect.center().y),
                Align2::LEFT_CENTER,
                arrow,
                FontId::proportional(crate::theme::xs(ui)),
                text_color.gamma_multiply(0.8),
            );

            let renaming = self.renaming.as_deref() == Some(entry.path.as_path());
            if renaming {
                // 改名输入框盖在名字原来所在的位置上
                let edit_rect = Rect::from_min_max(
                    egui::pos2(rect.left() + indent + 13.0, rect.top()),
                    rect.max - Vec2::new(2.0, 0.0),
                );
                let edit = egui::TextEdit::singleline(&mut self.rename_buf)
                    .font(name_font.clone())
                    .desired_width(edit_rect.width());
                let resp = ui.put(edit_rect, edit);
                // 每帧无条件 request_focus 会把输入法组字打断（见 src/ime.rs）
                ime::focus_input_box(&resp);
                if !ime::composing(ui) {
                    let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let esc = ui.input(|i| i.key_pressed(egui::Key::Escape));
                    if esc {
                        self.cancel_rename();
                    } else if enter || resp.lost_focus() {
                        // 单行输入框收到回车自己就交还焦点，两条路都得接住
                        self.commit_rename(out);
                    }
                }
            } else {
                painter.text(
                    egui::pos2(rect.left() + indent + 13.0, rect.center().y),
                    Align2::LEFT_CENTER,
                    &entry.name,
                    name_font.clone(),
                    text_color,
                );
            }

            if response.clicked() && !renaming {
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
                    entry
                        .path
                        .parent()
                        .map(|p| p.to_path_buf())
                        .unwrap_or_default()
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
                if ui
                    .button("新建 SQL 查询")
                    .on_hover_text("时间戳命名的 .sql 文件，首行带好连接的 SQL 块，建完直接打开")
                    .clicked()
                {
                    *out = Some(TreeAction::NewQuery(target_dir.clone()));
                    ui.close();
                }
                ui.separator();
                if !entry.is_dir && format::supports(&entry.path) && ui.button("格式化").clicked()
                {
                    *out = Some(TreeAction::Format(entry.path.clone()));
                    ui.close();
                }
                if !entry.is_dir && ui.button("重命名").clicked() {
                    self.start_rename(&entry.path);
                    ui.close();
                }
                if ui.button("在资源管理器中显示").clicked() {
                    reveal(&entry.path);
                    ui.close();
                }
                if ui.button("在此处打开终端").clicked() {
                    *out = Some(TreeAction::OpenTerminal(target_dir.clone()));
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
        egui::Window::new(if is_dir {
            "新建文件夹"
        } else {
            "新建文件"
        })
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
        .show(ctx, |ui| {
            ui.label(format!("位置：{}", parent.to_string_lossy()));
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.new_name)
                    .hint_text(if is_dir {
                        "文件夹名称"
                    } else {
                        "文件名，如 main.rs"
                    })
                    .desired_width(300.0),
            );
            // 每帧无条件 request_focus 会把输入法组字打断，中文填不进来（见 src/ime.rs）
            ime::focus_input_box(&resp);
            // 组字期间的回车/Esc 是输入法在用（确认候选词 / 撤销组字），别当成建文件
            if !ime::composing(ui) {
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    close = true;
                }
                if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    create = true;
                }
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

pub(crate) fn reveal(path: &Path) {
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
    /// 返回值第二项是这一帧树交上来的操作（打开 / 改名 / 开终端……）。
    fn pass(
        ctx: &egui::Context,
        tree: &mut FileTree,
        events: Vec<egui::Event>,
    ) -> (egui::FullOutput, Option<TreeAction>) {
        let raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, SCREEN)),
            events,
            ..Default::default()
        };
        let mut action = None;
        let mut out = ctx.run_pass(raw, |ctx| {
            ctx.root_ui(|ui| {
                egui::Panel::left("side").show(ui, |ui| {
                    action = tree.ui(ui, false);
                });
            });
        });
        // 测试里没有真正的渲染器，字体图集的增量没人消费，
        // epaint 会在 TexturesDelta 被 drop 时断言，这里手动清掉。
        out.textures_delta.clear();
        (out, action)
    }

    /// 只关心画面、不关心操作结果的帧。
    fn frame(
        ctx: &egui::Context,
        tree: &mut FileTree,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        pass(ctx, tree, events).0
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
    fn wait_for_text(ctx: &egui::Context, tree: &mut FileTree, needle: &str) -> Option<Rect> {
        for _ in 0..4 {
            let out = frame(ctx, tree, vec![]);
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

        let out = frame(&ctx, &mut tree, vec![]);
        let row = find_text(&out, "a.txt")
            .expect("文件树里没画出 a.txt")
            .center();

        pass(
            &ctx,
            &mut tree,
            pointer(row, egui::PointerButton::Secondary, true),
        );
        pass(
            &ctx,
            &mut tree,
            pointer(row, egui::PointerButton::Secondary, false),
        );
        let item = wait_for_text(&ctx, &mut tree, "新建文件")
            .expect("右键没有弹出「新建文件」菜单")
            .center();

        pass(
            &ctx,
            &mut tree,
            pointer(item, egui::PointerButton::Primary, true),
        );
        pass(
            &ctx,
            &mut tree,
            pointer(item, egui::PointerButton::Primary, false),
        );
        // Window 的第一帧只量尺寸不画内容，再跑一帧才能看到它
        let out = frame(&ctx, &mut tree, vec![]);
        assert!(tree.new_entry.is_some(), "点了菜单项却没记下新建目标");
        assert!(
            find_text(&out, "文件名，如 main.rs").is_some(),
            "新建对话框没画出来"
        );

        frame(&ctx, &mut tree, vec![egui::Event::Text("probe.txt".into())]);
        assert_eq!(tree.new_name, "probe.txt", "对话框里的输入框没收到键盘文字");

        frame(
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

    /// 右键「重命名」→ 就地改名 → 回车：磁盘文件真的换了名字。
    #[test]
    fn right_click_rename_replaces_the_file_on_disk() {
        let dir = std::env::temp_dir().join("easy-editor-tree-rename-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let old = dir.join("a.txt");
        std::fs::write(&old, "hello\n").unwrap();

        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(1.0);
        let mut tree = FileTree::new(dir.clone());

        let out = frame(&ctx, &mut tree, vec![]);
        let row = find_text(&out, "a.txt")
            .expect("文件树里没画出 a.txt")
            .center();

        frame(
            &ctx,
            &mut tree,
            pointer(row, egui::PointerButton::Secondary, true),
        );
        frame(
            &ctx,
            &mut tree,
            pointer(row, egui::PointerButton::Secondary, false),
        );
        let item = wait_for_text(&ctx, &mut tree, "重命名")
            .expect("右键没有弹出「重命名」菜单")
            .center();
        frame(
            &ctx,
            &mut tree,
            pointer(item, egui::PointerButton::Primary, true),
        );
        frame(
            &ctx,
            &mut tree,
            pointer(item, egui::PointerButton::Primary, false),
        );

        // 改名框出现，并预填当前名字
        let out = frame(&ctx, &mut tree, vec![]);
        assert_eq!(tree.renaming.as_deref(), Some(old.as_path()));
        assert_eq!(tree.rename_buf, "a.txt");
        assert!(
            find_text(&out, "a.txt").is_some(),
            "改名输入框没画出当前文件名"
        );

        // 模拟全选后重新敲一个名字
        tree.rename_buf.clear();
        frame(&ctx, &mut tree, vec![egui::Event::Text("b.txt".into())]);
        assert_eq!(tree.rename_buf, "b.txt", "改名框没收到键盘文字");

        let (_, action) = pass(
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
        let new = dir.join("b.txt");
        match action {
            Some(TreeAction::Renamed(from, to)) => {
                assert_eq!(from, old);
                assert_eq!(to, new);
            }
            other => panic!("回车改名没交出 Renamed 动作：{other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(&new).unwrap(),
            "hello\n",
            "内容换了文件"
        );
        assert!(!old.exists(), "旧名字还留在磁盘上");
        assert_eq!(tree.renaming, None, "改名框该关掉");
    }

    /// Esc 取消、非法名字不落盘：两者都不该动磁盘。
    #[test]
    fn rename_rejects_junk_names_and_cancels() {
        let dir = std::env::temp_dir().join("easy-editor-tree-rename-bad-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let old = dir.join("a.txt");
        std::fs::write(&old, "a\n").unwrap();
        std::fs::write(dir.join("taken.txt"), "t\n").unwrap();

        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(1.0);
        let mut tree = FileTree::new(dir.clone());

        // 带路径分隔符：不合法
        tree.start_rename(&old);
        tree.rename_buf = "..\\evil.txt".into();
        let mut action = None;
        tree.commit_rename(&mut action);
        assert!(action.is_none());
        assert!(tree.error.is_some(), "非法名字要给出提示");
        assert!(old.exists());

        // 和已有文件重名：不改
        tree.start_rename(&old);
        tree.rename_buf = "taken.txt".into();
        tree.commit_rename(&mut action);
        assert!(action.is_none());
        assert!(dir.join("taken.txt").is_file());
        assert!(old.exists(), "撞名时把原文件弄丢了");

        // 名字没变：静默关掉，不产生动作
        tree.start_rename(&old);
        tree.rename_buf = "a.txt".into();
        tree.commit_rename(&mut action);
        assert!(action.is_none());
        assert!(tree.error.is_none());
        assert_eq!(tree.renaming, None);

        // Esc 取消
        let out = frame(&ctx, &mut tree, vec![]);
        let row = find_text(&out, "a.txt").expect("没画出 a.txt").center();
        frame(
            &ctx,
            &mut tree,
            pointer(row, egui::PointerButton::Secondary, true),
        );
        frame(
            &ctx,
            &mut tree,
            pointer(row, egui::PointerButton::Secondary, false),
        );
        let item = wait_for_text(&ctx, &mut tree, "重命名")
            .expect("右键没有弹出「重命名」菜单")
            .center();
        frame(
            &ctx,
            &mut tree,
            pointer(item, egui::PointerButton::Primary, true),
        );
        frame(
            &ctx,
            &mut tree,
            pointer(item, egui::PointerButton::Primary, false),
        );
        frame(&ctx, &mut tree, vec![]);
        assert_eq!(tree.renaming.as_deref(), Some(old.as_path()));
        frame(
            &ctx,
            &mut tree,
            vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        assert_eq!(tree.renaming, None, "Esc 没能取消改名");
        assert!(old.exists());
    }

    /// 右键「在此处打开终端」交出目录，而不是去开资源管理器。
    #[test]
    fn open_terminal_menu_emits_the_directory() {
        let dir = std::env::temp_dir().join("easy-editor-tree-term-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/a.txt"), "a\n").unwrap();

        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(1.0);
        let mut tree = FileTree::new(dir.clone());
        tree.expanded.insert(dir.join("sub"));

        let out = frame(&ctx, &mut tree, vec![]);
        let row = find_text(&out, "a.txt")
            .expect("文件树里没画出 sub/a.txt")
            .center();
        frame(
            &ctx,
            &mut tree,
            pointer(row, egui::PointerButton::Secondary, true),
        );
        frame(
            &ctx,
            &mut tree,
            pointer(row, egui::PointerButton::Secondary, false),
        );
        let item = wait_for_text(&ctx, &mut tree, "在此处打开终端")
            .expect("右键没有弹出「在此处打开终端」菜单")
            .center();
        frame(
            &ctx,
            &mut tree,
            pointer(item, egui::PointerButton::Primary, true),
        );
        let (_, action) = pass(
            &ctx,
            &mut tree,
            pointer(item, egui::PointerButton::Primary, false),
        );
        match action {
            Some(TreeAction::OpenTerminal(at)) => assert_eq!(at, dir.join("sub")),
            other => panic!("点「在此处打开终端」没交出目录：{other:?}"),
        }
    }

    /// 改名框是 `Ui::put` 塞进行矩形里的，它会把父布局的光标往前推。
    /// 推多了整棵树会往下跳，这里量一下下面那几行有没有挪。
    #[test]
    fn rename_box_does_not_push_the_rows_below() {
        let dir = std::env::temp_dir().join("easy-editor-tree-rename-shift-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["a.txt", "m.txt", "z.txt"] {
            std::fs::write(dir.join(name), "x\n").unwrap();
        }

        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(1.0);
        let mut tree = FileTree::new(dir.clone());

        let out = frame(&ctx, &mut tree, vec![]);
        let before = find_text(&out, "z.txt")
            .expect("没画出 z.txt")
            .min
            .to_vec2();

        tree.start_rename(&dir.join("a.txt"));
        let out = frame(&ctx, &mut tree, vec![]);
        let after = find_text(&out, "z.txt")
            .expect("改名时 z.txt 不见了")
            .min
            .to_vec2();

        assert!(
            (after - before).length() < 2.0,
            "改名框把下面的行推了 {before:?} → {after:?}"
        );
    }

    #[test]
    fn sanitized_name_rejects_path_tricks() {
        assert_eq!(sanitized_name("  a.txt  ").as_deref(), Some("a.txt"));
        assert_eq!(sanitized_name(""), None);
        assert_eq!(sanitized_name("   "), None);
        assert_eq!(sanitized_name("."), None);
        assert_eq!(sanitized_name(".."), None);
        assert_eq!(sanitized_name("a/b"), None);
        assert_eq!(sanitized_name("a\\b"), None);
        assert_eq!(sanitized_name("C:evil"), None);
        assert_eq!(sanitized_name("a*b?c"), None);
        assert_eq!(sanitized_name("报告.md").as_deref(), Some("报告.md"));
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
