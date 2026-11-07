//! 应用主体：布局、快捷键、文件打开 / 保存、全局搜索、查找替换、日志块、终端。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use eframe::egui::{self, Color32, Key, Modifiers, RichText, Ui};
use egui_extras::{Column, TableBuilder};

use crate::config::{self, ConfigStore};
use crate::editor::{self, OpenFile, PendingEdit};
use crate::filetree::{self, FileTree, TreeAction};
use crate::find::FindState;
use crate::format;
use crate::journal;
use crate::keymap::{self, Keymap};
use crate::nav::{CaretHistory, NavPoint};
use crate::pane::{self, Axis, Dir, PaneId, PaneTree, TreeLayout};
use crate::search::SearchState;
use crate::syntax::Highlighter;
use crate::terminal::{TerminalPanel, TerminalSettings};
use crate::theme;

#[derive(Debug, PartialEq, Clone, Copy)]
enum SideTab {
    Files,
    Search,
    Journal,
}

impl SideTab {
    fn key(self) -> &'static str {
        match self {
            Self::Files => "files",
            Self::Search => "search",
            Self::Journal => "journal",
        }
    }

    fn from_key(key: &str) -> Self {
        match key {
            "search" => Self::Search,
            "journal" => Self::Journal,
            _ => Self::Files,
        }
    }
}

/// 中央区域的一块面板：要么在看文件，要么在跑 shell。
///
/// 编辑器面板存的是 `tabs` 的下标而不是路径：同一个文件允许同时出现在两块面板里
/// （对比、边看边写），用下标才表达得出「两块面板共用一份 buffer」。
#[derive(Clone, Copy, Debug)]
enum PaneKind {
    Editor(Option<usize>),
    Terminal(u64),
}

impl PaneKind {
    fn tab(self) -> Option<usize> {
        match self {
            Self::Editor(idx) => idx,
            Self::Terminal(_) => None,
        }
    }

    fn is_editor(self) -> bool {
        matches!(self, Self::Editor(_))
    }

    fn terminal_id(self) -> Option<u64> {
        match self {
            Self::Terminal(id) => Some(id),
            Self::Editor(_) => None,
        }
    }

    /// 标签关掉之后下标可能越界，退回空白面板而不是 panic。
    fn sanitize(self, tabs_len: usize) -> Self {
        match self {
            Self::Editor(Some(i)) if i >= tabs_len => Self::Editor(None),
            other => other,
        }
    }
}

struct QuickOpen {
    query: String,
    matches: Vec<PathBuf>,
    sel: usize,
}

pub struct EasyEditorApp {
    root: Option<PathBuf>,
    tree: Option<FileTree>,
    index: Vec<PathBuf>,
    tabs: Vec<OpenFile>,
    active: Option<usize>,
    side: SideTab,
    show_side: bool,
    terminal: TerminalPanel,
    search: SearchState,
    find: FindState,
    hl: Highlighter,
    font_size: f32,
    dark: bool,
    /// 编辑器自动换行：开着长行折进面板，关着长行横向滚出去
    word_wrap: bool,
    /// 顶栏内容上一帧的高度（取整）。面板高度是照上一帧量的，换行后旧高度会把第二排裁掉
    toolbar_h: i32,
    status: String,
    quick_open: Option<QuickOpen>,
    pending_close: Option<usize>,
    show_help: bool,
    want_open_folder: bool,
    focus_search: bool,
    frame_count: u32,
    /// 新日志块使用的标题；留空则自动编号
    journal_title: String,
    /// 用户配置（字号 / 主题 / 上次工作目录 / 快捷键），带防抖落盘
    config: ConfigStore,
    /// 光标位置历史（Ctrl+Alt+← / →）
    nav: CaretHistory,
    /// 可配置快捷键
    keymap: Keymap,
    /// 正在录制新快捷键的动作 id
    recording: Option<String>,
    /// 「终端设置」窗口是否打开
    show_terminal_settings: bool,
    /// 「终端设置」窗口这次出现时要不要摆回屏幕正中
    center_terminal_settings: bool,
    /// 中央区域的面板树（tmux 式：一块块面板递归切分，编辑器与终端共用一棵树）
    panes: PaneTree<PaneKind>,
    /// 上一帧算出的面板矩形，方位键找邻居时要用
    pane_layout: TreeLayout,
    /// 最后一次聚焦过的编辑面板。焦点切到终端时，文本类操作还是落在这块上
    last_editor_pane: Option<PaneId>,
    /// 调试脚本的计时起点
    boot: std::time::Instant,
    /// 调试脚本走到第几步
    verify_stage: usize,
}

impl EasyEditorApp {
    pub fn new(cc: &eframe::CreationContext<'_>, target: Option<PathBuf>) -> Self {
        Self::bootstrap(&cc.egui_ctx, target)
    }

    /// 不碰 `eframe` 上下文的构造入口：测试里没有真实的窗口和 GL 上下文，
    /// 但照样要把配置、字体、面板树这套起手式跑一遍。
    fn bootstrap(ctx: &egui::Context, target: Option<PathBuf>) -> Self {
        let config = ConfigStore::load();
        let font_size = config.data.font_size;
        let dark = config.data.dark;
        let show_side = config.data.show_side;
        let word_wrap = config.data.word_wrap;
        let side = SideTab::from_key(&config.data.side_tab);

        theme::install_fonts(ctx);
        theme::apply_style(ctx, dark);

        // egui 自带一套「像浏览器那样缩放整个界面」的快捷键（Ctrl+= / Ctrl+- / Ctrl+0），
        // 它改的是 zoom_factor，菜单、侧栏、编辑器会一起变大。本应用的字号快捷键和它撞车，
        // 撞车后放大走整窗缩放、缩小走字号，只放不缩，所以整窗缩放关掉，字号缩放自己管。
        ctx.options_mut(|o| o.zoom_with_keyboard = false);

        let keymap = Keymap::from_config(&config.data.keymap);

        let mut app = Self {
            root: None,
            tree: None,
            index: Vec::new(),
            tabs: Vec::new(),
            active: None,
            side,
            show_side,
            terminal: TerminalPanel::new(
                std::env::current_dir().unwrap_or_default(),
                TerminalSettings::from_config(&config.data.terminal_programs, &config.data.terminal_args),
            ),
            search: SearchState::default(),
            find: FindState::default(),
            hl: Highlighter::new(dark),
            font_size,
            dark,
            word_wrap,
            toolbar_h: 0,
            status: "Ctrl+O 打开文件夹 · Ctrl+P 快速打开 · Ctrl+` 终端".into(),
            quick_open: None,
            pending_close: None,
            show_help: false,
            want_open_folder: false,
            focus_search: false,
            frame_count: 0,
            journal_title: String::new(),
            config,
            nav: CaretHistory::new(),
            keymap,
            recording: None,
            show_terminal_settings: false,
            center_terminal_settings: false,
            panes: PaneTree::new(),
            pane_layout: TreeLayout::default(),
            last_editor_pane: None,
            boot: std::time::Instant::now(),
            verify_stage: 0,
        };
        // 中央区域始终至少有一块面板；空工作区时它显示欢迎页
        let root = app.panes.set_root(PaneKind::Editor(None));
        app.last_editor_pane = Some(root);
        // 配置里指定的终端偏好：本机没有这个种类就保持自动选择
        app.terminal.apply_pref(&app.config.data.terminal_shell);

        // 命令行参数优先
        if let Some(path) = target {
            let path = if path.is_absolute() {
                path
            } else {
                std::env::current_dir().unwrap_or_default().join(path)
            };
            if path.is_dir() {
                app.open_workspace(path);
            } else if path.is_file() {
                let parent = path.parent().map(|p| p.to_path_buf());
                if let Some(parent) = parent {
                    app.open_workspace(parent);
                }
                app.open_file(path, None);
            }
            app.apply_verify_flags();
            return app;
        }

        // 没有参数时：当前目录像个工程就用它，否则恢复上次打开的工作目录
        let cwd = std::env::current_dir().ok();
        let cwd_is_project = cwd
            .as_ref()
            .is_some_and(|c| c.join("Cargo.toml").exists() || c.join(".git").exists());
        if cwd_is_project {
            if let Some(cwd) = cwd {
                app.open_workspace(cwd);
            }
        } else if let Some(last) = app.config.data.last_workspace.clone() {
            let path = PathBuf::from(&last);
            if path.is_dir() {
                app.open_workspace(path);
                app.status = format!("已恢复上次的工作目录 {}", last);
            } else {
                // 目录被删了或移走了，清掉记录免得下次还提示
                app.config.data.last_workspace = None;
                app.config.touch();
            }
        }
        app.apply_verify_flags();
        app
    }

    /// 每帧末尾把 UI 状态同步回配置，有变化才标记，攒够时间自动落盘。
    fn sync_config(&mut self) {
        let mut changed = false;
        if self.config.data.font_size != self.font_size {
            self.config.data.font_size = self.font_size;
            changed = true;
        }
        if self.config.data.dark != self.dark {
            self.config.data.dark = self.dark;
            changed = true;
        }
        if self.config.data.show_side != self.show_side {
            self.config.data.show_side = self.show_side;
            changed = true;
        }
        if self.config.data.word_wrap != self.word_wrap {
            self.config.data.word_wrap = self.word_wrap;
            changed = true;
        }
        let tab = self.side.key();
        if self.config.data.side_tab != tab {
            self.config.data.side_tab = tab.to_string();
            changed = true;
        }
        if let Some(tree) = &self.tree {
            if self.config.data.show_hidden != tree.show_hidden {
                self.config.data.show_hidden = tree.show_hidden;
                changed = true;
            }
        }
        // 只把跟默认值不一样的绑定写进配置，配置文件才不会一堆噪音。
        // 空串要保留——它表示用户主动解绑了这个动作。
        let mut km = self.keymap.to_config();
        km.retain(|id, binding| binding != keymap::default_binding(id));
        if self.config.data.keymap != km {
            self.config.data.keymap = km;
            changed = true;
        }
        // 终端：默认种类 + 路径/参数覆盖。覆盖项先去掉空串再比，配置文件才不会长毛。
        let pref = self.terminal.shell.id().to_string();
        if self.config.data.terminal_shell != pref {
            self.config.data.terminal_shell = pref;
            changed = true;
        }
        let (programs, args) = self.terminal.settings.to_config();
        if self.config.data.terminal_programs != programs {
            self.config.data.terminal_programs = programs;
            changed = true;
        }
        if self.config.data.terminal_args != args {
            self.config.data.terminal_args = args;
            changed = true;
        }
        if changed {
            self.config.touch();
        }
        self.config.maybe_save();
    }

    /// 调试用：EASY_EDITOR_VERIFY 让启动后自动展开某个面板，便于截图核对。
    /// 必须在文件/工作区打开之后调用，否则会被 open_file 里的状态重置覆盖。
    fn apply_verify_flags(&mut self) {
        match crate::config::env_var("VERIFY").as_deref() {
            Some("terminal") => {
                self.new_terminal_pane();
                // 自检：让终端自动获得焦点，截图里能看到光标即说明焦点机制生效
                if let Some(id) = self.first_terminal_id() {
                    self.terminal.request_focus(id);
                }
            }
            Some("find") => {
                self.find.open_find(false);
                self.find.query = "一段".into();
                self.find.stale = true;
            }
            Some("replace") => {
                self.find.open_find(true);
                self.find.query = "一段".into();
                self.find.replace = "内容".into();
                self.find.stale = true;
            }
            Some("journal") => {
                self.side = SideTab::Journal;
                self.show_side = true;
            }
            Some("search") => {
                self.side = SideTab::Search;
                self.show_side = true;
                self.search.query = "fn ".into();
                if let Some(root) = self.root.clone() {
                    self.search.start(&root);
                }
            }
            Some("newfile") => {}
            _ => {}
        }
    }

    // ---------- 工作区 / 文件 ----------

    fn open_workspace(&mut self, path: PathBuf) {
        self.root = Some(path.clone());
        let mut tree = FileTree::new(path.clone());
        tree.show_hidden = self.config.data.show_hidden;
        self.tree = Some(tree);
        self.index = filetree::collect_files(&path, 50_000);
        self.terminal.set_cwd(path.clone());
        self.tabs.clear();
        self.active = None;
        // 标签表清空了，面板里存的下标全部作废；终端面板原样保留
        self.panes
            .foreach_payload_mut(|_, kind| {
                if let PaneKind::Editor(slot) = kind {
                    *slot = None;
                }
            });
        self.close_find_bar();
        // 记住工作目录，下次启动自动恢复
        let text = path.to_string_lossy().to_string();
        if self.config.data.last_workspace.as_deref() != Some(text.as_str()) {
            self.config.data.last_workspace = Some(text);
            self.config.touch();
        }
        self.status = format!("已打开 {}（{} 个文件）", path.display(), self.index.len());
    }

    fn open_folder_dialog(&mut self) {
        let mut dialog = rfd::FileDialog::new().set_title("打开文件夹");
        if let Some(root) = &self.root {
            dialog = dialog.set_directory(root);
        }
        if let Some(dir) = dialog.pick_folder() {
            self.open_workspace(dir);
        }
    }

    fn open_file(&mut self, path: PathBuf, line: Option<usize>) {
        // 文件已在某个标签中打开？把对应面板切到前台
        if let Some(idx) = self.tabs.iter().position(|t| t.path == path) {
            self.activate_tab(idx);
            if let Some(line) = line {
                let byte = line_start_offset(&self.tabs[idx].text, line);
                self.tabs[idx].pending_jump = Some(byte);
            }
            return;
        }

        if self.tabs.len() >= editor::MAX_TABS {
            self.status = format!("最多同时打开 {} 个文件，请先关闭一些标签", editor::MAX_TABS);
            return;
        }

        let meta = std::fs::metadata(&path).ok();
        if let Some(meta) = &meta {
            if meta.len() as usize > editor::MAX_FILE_BYTES {
                self.status = format!(
                    "{} 有 {}，超过 {} 上限，未打开",
                    path.file_name().unwrap_or_default().to_string_lossy(),
                    editor::human_size(meta.len() as usize),
                    editor::human_size(editor::MAX_FILE_BYTES)
                );
                return;
            }
        }

        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                self.status = format!("打开失败：{e}");
                return;
            }
        };
        if bytes.iter().take(4096).any(|b| *b == 0) {
            self.status = format!("{} 看起来是二进制文件，已跳过", path.display());
            return;
        }
        let text = String::from_utf8_lossy(&bytes).to_string();
        let mut file = OpenFile::new(path.clone(), text);
        file.mtime = meta.and_then(|m| m.modified().ok());
        if let Some(line) = line {
            file.pending_jump = Some(line_start_offset(&file.text, line));
        }
        self.status = if file.large {
            format!(
                "已打开 {}（{}，只读预览）",
                path.display(),
                editor::human_size(file.bytes_len)
            )
        } else {
            format!("已打开 {}", path.display())
        };
        self.tabs.push(file);
        let new_idx = self.tabs.len() - 1;
        self.show_in_focused_pane(new_idx);
        // 查找栏不关，但匹配要按新文件重算（旧偏移属于上一个文件）
        self.revalidate_find();
    }

    fn save_active(&mut self) {
        let Some(idx) = self.focused_tab() else { return };
        let Some(file) = self.tabs.get_mut(idx) else { return };
        if file.large {
            self.status = "大文件为只读预览，未保存".into();
            return;
        }
        match std::fs::write(&file.path, file.text.as_bytes()) {
            Ok(_) => {
                file.mark_saved();
                file.mtime = std::fs::metadata(&file.path)
                    .ok()
                    .and_then(|m| m.modified().ok());
                self.status = format!("已保存 {}", file.path.display());
            }
            Err(e) => self.status = format!("保存失败：{e}"),
        }
    }

    fn save_all(&mut self) {
        let mut saved = 0;
        let mut error = None;
        for file in &mut self.tabs {
            if !file.dirty || file.large {
                continue;
            }
            match std::fs::write(&file.path, file.text.as_bytes()) {
                Ok(_) => {
                    file.mark_saved();
                    file.mtime = std::fs::metadata(&file.path)
                        .ok()
                        .and_then(|m| m.modified().ok());
                    saved += 1;
                }
                Err(e) => error = Some(format!("{} 保存失败：{e}", file.path.display())),
            }
        }
        self.status = match (saved, error) {
            (_, Some(e)) => e,
            (0, None) => "没有需要保存的文件".into(),
            (n, None) => format!("已保存 {n} 个文件"),
        };
    }

    fn close_tab(&mut self, idx: usize) {
        if idx >= self.tabs.len() {
            return;
        }
        if self.tabs[idx].dirty {
            self.pending_close = Some(idx);
            return;
        }
        self.force_close_tab(idx);
    }

    fn force_close_tab(&mut self, idx: usize) {
        if idx >= self.tabs.len() {
            return;
        }
        self.tabs.remove(idx);
        // 标签表短了一格，每块面板存的下标都要跟着挪，否则会指向隔壁的文件
        let remaining = self.tabs.len();
        let was_focused = self.focused_tab() == Some(idx);
        self.panes
            .foreach_payload_mut(|_, kind| {
                if let PaneKind::Editor(slot) = kind {
                    *slot = remap_tab(*slot, idx, remaining);
                }
            });
        if was_focused && remaining > 0 {
            // 焦点面板空了的话接着显示隔壁的标签，和大多数编辑器一致
            let next = idx.min(remaining - 1);
            if let Some(p) = self.editor_pane_for_write() {
                if let Some(PaneKind::Editor(slot)) = self.panes.payload_of_mut(p) {
                    if slot.is_none() {
                        *slot = Some(next);
                    }
                }
            }
        }
        self.sync_active();
        // 关掉一个标签后当前文件可能变了，匹配要重算
        self.revalidate_find();
    }

    // ---------- 日志块 ----------

    fn insert_journal_block(&mut self) {
        let title_base = self.journal_title.trim().to_string();
        let Some(idx) = self.focused_tab() else {
            self.status = "日志块写在文本文件里，请先打开一个文件".into();
            return;
        };
        let file = &mut self.tabs[idx];
        if file.large {
            self.status = "大文件为只读预览，无法插入日志块".into();
            return;
        }
        let blocks = journal::scan(&file.text);
        let n = blocks.len() + 1;
        let title = if title_base.is_empty() {
            format!("工作日志{n}")
        } else {
            title_base
        };
        let (new_text, caret) = journal::insert_block(&file.text, file.caret_byte, &title, n);
        let total = new_text.len();
        file.pending_edit = Some(PendingEdit {
            range: Some((0, total.min(file.text.len()))),
            insert: new_text,
            caret: Some(caret),
        });
        self.status = format!("已插入「{title}」，直接输入内容即可");
    }

    fn journal_goto(&mut self, forward: bool) {
        let Some(idx) = self.focused_tab() else { return };
        let file = &mut self.tabs[idx];
        let blocks = journal::scan(&file.text);
        if blocks.is_empty() {
            self.status = "当前文件没有日志块，Ctrl+J 插入一个".into();
            return;
        }
        let line = file.line.saturating_sub(1);
        let target = if forward {
            journal::next_block(&blocks, line)
        } else {
            journal::prev_block(&blocks, line)
        };
        match target {
            Some(b) => {
                file.pending_jump = Some(b.head_start);
                self.status = journal::describe(&blocks, b.line);
            }
            None => {
                self.status = if forward {
                    "已经是最后一个日志块"
                } else {
                    "已经是第一个日志块"
                }
                .into();
            }
        }
    }

    fn journal_toggle(&mut self) {
        let Some(idx) = self.focused_tab() else { return };
        let file = &mut self.tabs[idx];
        let blocks = journal::scan(&file.text);
        let line = file.line.saturating_sub(1);
        let Some(block) = journal::block_at(&blocks, line) else {
            self.status = "光标不在日志块内".into();
            return;
        };
        let (a, b, head) = journal::toggle_edit(block);
        let caret = a + head.len();
        let next = if block.is_done() {
            journal::STATUS_TODO
        } else {
            journal::STATUS_DONE
        };
        file.pending_edit = Some(PendingEdit {
            range: Some((a, b)),
            insert: head,
            caret: Some(caret),
        });
        self.status = format!("标记为 [{next}]");
    }

    fn journal_jump_to(&mut self, byte: usize) {
        if let Some(idx) = self.active {
            self.tabs[idx].pending_jump = Some(byte);
        }
    }

    // ---------- 查找替换 ----------

    fn sync_find_to_file(&mut self) {
        let Some(idx) = self.focused_tab() else { return };
        let matches = Arc::new(self.find.matches.clone());
        let current = if self.find.matches.is_empty() {
            None
        } else {
            Some(self.find.current)
        };
        let file = &mut self.tabs[idx];
        file.matches = matches;
        file.current_match = current;
    }

    fn find_recompute(&mut self) {
        let Some(idx) = self.focused_tab() else { return };
        self.find.recompute(&self.tabs[idx].text);
        self.sync_find_to_file();
        if let Some((a, _)) = self.find.current_range() {
            self.tabs[idx].pending_jump = Some(a);
            self.tabs[idx].jump_without_focus = true;
        }
    }

    fn find_step(&mut self, forward: bool) {
        if self.find.matches.is_empty() {
            return;
        }
        if forward {
            self.find.next_match();
        } else {
            self.find.prev_match();
        }
        self.sync_find_to_file();
        if let Some(idx) = self.focused_tab() {
            if let Some((a, _)) = self.find.current_range() {
                self.tabs[idx].pending_jump = Some(a);
                self.tabs[idx].jump_without_focus = true;
            }
        }
    }

    fn find_replace_current(&mut self) {
        let Some(idx) = self.focused_tab() else { return };
        if self.tabs[idx].large {
            self.status = "大文件为只读预览，无法替换".into();
            return;
        }
        let mut text = std::mem::take(&mut self.tabs[idx].text);
        let caret = self.find.replace_current(&mut text);
        self.tabs[idx].text = text;
        self.tabs[idx].reindex();
        self.tabs[idx].refresh_dirty();
        if let Some(c) = caret {
            self.tabs[idx].pending_jump = Some(c);
            self.tabs[idx].jump_without_focus = true;
        }
        self.sync_find_to_file();
        self.status = format!("已替换，剩余 {} 处", self.find.count());
    }

    fn find_replace_all(&mut self) {
        let Some(idx) = self.focused_tab() else { return };
        if self.tabs[idx].large {
            self.status = "大文件为只读预览，无法替换".into();
            return;
        }
        let mut text = std::mem::take(&mut self.tabs[idx].text);
        let n = self.find.replace_all(&mut text);
        self.tabs[idx].text = text;
        self.tabs[idx].reindex();
        self.tabs[idx].refresh_dirty();
        self.sync_find_to_file();
        self.status = format!("已替换 {n} 处");
    }

    // ---------- 快捷键 ----------

    /// 全部走 `keymap`，用户改过的绑定立刻生效。
    ///
    /// 同一个键上挂了多个动作（`Ctrl+W` / `Ctrl+Shift+W`），归属由 keymap 按修饰键
    /// 挑最吻合的那个，所以这里的书写顺序不影响结果。
    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        // 录制新快捷键时把所有快捷键都屏蔽掉，否则按键会顺手触发一堆动作
        if self.recording.is_some() {
            return;
        }

        if self.keymap.consume(ctx, "open_folder") {
            self.want_open_folder = true;
        }
        if self.keymap.consume(ctx, "save_all") {
            self.save_all();
        }
        if self.keymap.consume(ctx, "save") {
            self.save_active();
        }
        if self.keymap.consume(ctx, "quick_open") {
            self.open_quick_open();
        }
        if self.keymap.consume(ctx, "global_search") {
            self.side = SideTab::Search;
            self.show_side = true;
            self.focus_search = true;
        }
        if self.keymap.consume(ctx, "find") {
            self.prefill_find_query();
            self.find.open_find(false);
            self.find_recompute();
        }
        if self.keymap.consume(ctx, "replace") {
            self.prefill_find_query();
            self.find.open_find(true);
            self.find_recompute();
        }
        if self.keymap.consume(ctx, "journal_insert") {
            self.insert_journal_block();
        }
        if self.keymap.consume(ctx, "journal_next") {
            self.journal_goto(true);
        }
        if self.keymap.consume(ctx, "journal_prev") {
            self.journal_goto(false);
        }
        if self.keymap.consume(ctx, "journal_toggle") {
            self.journal_toggle();
        }
        if self.keymap.consume(ctx, "nav_back") {
            self.nav_goto(false);
        }
        if self.keymap.consume(ctx, "nav_forward") {
            self.nav_goto(true);
        }
        if self.keymap.consume(ctx, "new_folder") {
            if let Some(tree) = &mut self.tree {
                tree.request_new(true);
            }
        }
        if self.keymap.consume(ctx, "new_file") {
            if let Some(tree) = &mut self.tree {
                tree.request_new(false);
            }
        }
        if self.keymap.consume(ctx, "side_toggle") {
            self.show_side = !self.show_side;
        }
        if self.keymap.consume(ctx, "wrap_toggle") {
            self.toggle_wrap();
        }
        if self.keymap.consume(ctx, "terminal_new") {
            self.new_terminal_pane();
        }
        if self.keymap.consume(ctx, "terminal_toggle") {
            self.focus_or_new_terminal();
        }
        if self.keymap.consume(ctx, "tab_close") {
            self.close_focused();
        }
        if self.keymap.consume(ctx, "font_inc") {
            self.set_font_size(self.font_size + 1.0, ctx);
        }
        if self.keymap.consume(ctx, "font_dec") {
            self.set_font_size(self.font_size - 1.0, ctx);
        }
        if self.keymap.consume(ctx, "help") {
            self.show_help = !self.show_help;
        }
        if self.keymap.consume(ctx, "screenshot") {
            self.take_screenshot(ctx);
        }
        if self.keymap.consume(ctx, "split_col") {
            self.split_focused(Axis::Horizontal);
        }
        if self.keymap.consume(ctx, "split_row") {
            self.split_focused(Axis::Vertical);
        }
        if self.keymap.consume(ctx, "split_auto") {
            self.split_auto();
        }
        if self.keymap.consume(ctx, "pane_close") {
            if let Some(id) = self.panes.focus() {
                self.close_pane(id);
            }
        }
        if self.keymap.consume(ctx, "pane_next") {
            self.panes.focus_cycle(1);
            self.after_focus_change();
        }
        if self.keymap.consume(ctx, "pane_prev") {
            self.panes.focus_cycle(-1);
            self.after_focus_change();
        }
        for (action, dir) in [
            ("focus_pane_left", Dir::Left),
            ("focus_pane_right", Dir::Right),
            ("focus_pane_up", Dir::Up),
            ("focus_pane_down", Dir::Down),
        ] {
            if self.keymap.consume(ctx, action) {
                self.panes.focus_dir(&self.pane_layout, dir);
                self.after_focus_change();
            }
        }
        if self.keymap.consume(ctx, "format") {
            self.format_active();
        }

        // Esc 关查找栏：这个不绑定，属于「按了就管用」的习惯键。
        // 快速打开窗口自己也要用 Esc，所以它在前面时让路。
        if self.find.open
            && self.quick_open.is_none()
            && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape))
        {
            self.close_find_bar();
        }
    }

    // ---------- 面板树 ----------

    /// 焦点所在编辑器面板的标签；焦点在终端时退回最后一次聚焦的编辑面板，
    /// 这样 Ctrl+S / Ctrl+F 在终端里按下去也不会变成空操作。
    fn focused_tab(&self) -> Option<usize> {
        let focused = self.panes.focus().and_then(|p| self.panes.payload_of(p).copied());
        if focused.map(PaneKind::is_editor) == Some(true) {
            return focused.and_then(PaneKind::tab);
        }
        let last = self
            .last_editor_pane
            .filter(|p| self.panes.contains(*p))?;
        self.panes.payload_of(last).copied().and_then(PaneKind::tab)
    }

    /// 本次要写入标签的面板：焦点在编辑器上就是它，否则用最后一次聚焦的编辑面板。
    fn editor_pane_for_write(&self) -> Option<PaneId> {
        let focused = self.panes.focus()?;
        if self.panes.payload_of(focused).copied().is_some_and(|k| k.is_editor()) {
            return Some(focused);
        }
        let last = self.last_editor_pane.filter(|p| self.panes.contains(*p))?;
        self.panes
            .payload_of(last)
            .copied()
            .filter(|k| k.is_editor())
            .map(|_| last)
    }

    /// 把标签交给「当前该显示它的地方」：焦点是编辑器面板就换掉它的内容，
    /// 焦点是终端则另开一块编辑面板——绝不覆盖正在跑的 shell。
    fn show_in_focused_pane(&mut self, idx: usize) {
        let write = self.editor_pane_for_write();
        match write {
            Some(p) => {
                if let Some(PaneKind::Editor(slot)) = self.panes.payload_of_mut(p) {
                    *slot = Some(idx);
                }
                self.panes.set_focus(p);
            }
            None => {
                self.split_with(Axis::Horizontal, PaneKind::Editor(Some(idx)));
            }
        }
        self.after_focus_change();
    }

    /// 设置当前焦点面板的激活标签。
    fn set_focused_tab(&mut self, idx: Option<usize>) {
        let Some(p) = self.editor_pane_for_write() else { return };
        if let Some(PaneKind::Editor(slot)) = self.panes.payload_of_mut(p) {
            *slot = idx;
        }
        self.after_focus_change();
    }

    /// 把 `self.active` 这个镜像刷成焦点面板的真实内容。
    ///
    /// `active` 被状态栏、侧栏、工具栏等三十来处读，留着它比逐个改调用点稳。
    fn sync_active(&mut self) {
        self.active = self.focused_tab();
    }

    /// 焦点变了之后要做的事：记住最后一个编辑面板、把键盘焦点交给终端、刷新镜像。
    fn after_focus_change(&mut self) {
        if let Some(id) = self.panes.focus() {
            let kind = self.panes.payload_of(id).copied();
            if kind.is_some_and(|k| k.is_editor()) {
                self.last_editor_pane = Some(id);
            }
            if let Some(t) = kind.and_then(PaneKind::terminal_id) {
                self.terminal.request_focus(t);
            }
        }
        self.sync_active();
    }

    /// 在 `target` 旁边切出一块新面板。
    fn split_with(&mut self, axis: Axis, payload: PaneKind) -> Option<PaneId> {
        let target = self.panes.focus()?;
        let id = self.panes.split(target, axis, payload)?;
        if let Some(t) = payload.terminal_id() {
            self.terminal.request_focus(t);
        }
        self.status = if payload.terminal_id().is_some() {
            "已新建终端面板".into()
        } else {
            "已拆分面板".into()
        };
        self.after_focus_change();
        Some(id)
    }

    /// 沿某个方向再切一刀。编辑器面板复制当前文件（对比 / 边看边写），
    /// 终端面板起一个新 shell。
    fn split_focused(&mut self, axis: Axis) {
        let Some(target) = self.panes.focus() else { return };
        let payload = match self.panes.payload_of(target).copied() {
            Some(PaneKind::Editor(idx)) => PaneKind::Editor(idx),
            Some(PaneKind::Terminal(_)) => match self.terminal.create() {
                Ok(id) => PaneKind::Terminal(id),
                Err(e) => {
                    self.status = e;
                    return;
                }
            },
            None => return,
        };
        if self.split_with(axis, payload).is_none() && payload.terminal_id().is_some() {
            // split 失败（面板已经不在树里）时别留下一个没人看的 shell
            if let Some(t) = payload.terminal_id() {
                self.terminal.remove(t);
            }
        }
    }

    /// 自动挑方向再切一刀，和 Windows Terminal 的 `Alt+Shift+D` 一个思路：
    /// 当前面板宽就左右拆、高就上下拆，拆完两块都更接近方形。
    fn split_auto(&mut self) {
        let axis = self
            .panes
            .focus()
            .and_then(|id| self.pane_layout.pane_rect(id))
            .map(|r| {
                if r.width() >= r.height() {
                    Axis::Horizontal
                } else {
                    Axis::Vertical
                }
            })
            .unwrap_or(Axis::Horizontal);
        self.split_focused(axis);
    }

    /// 新建一块终端面板，切在焦点面板右侧。
    ///
    /// 之前固定切在下方，用户没有任何办法把终端放到右边；改成左右并排后，
    /// 编辑器在左、终端在右，正好是多数人习惯的布局。
    fn new_terminal_pane(&mut self) {
        let id = match self.terminal.create() {
            Ok(id) => id,
            Err(e) => {
                self.status = e;
                return;
            }
        };
        if self
            .split_with(Axis::Horizontal, PaneKind::Terminal(id))
            .is_none()
        {
            self.terminal.remove(id);
            self.status = "没有可拆分的面板".into();
        }
    }

    /// 把一块面板移到工作区最靠某个方向的边缘（右键面板标题）。
    ///
    /// 先把它从树里摘下来（父 Split 自动收起），再把负载切到边缘面板的对应一侧。
    /// `split` 总是把新面板排在后面，要放到左侧/上方时再交换一次负载。
    fn move_pane_to(&mut self, id: PaneId, dir: Dir) {
        if self.panes.count() < 2 || !self.panes.contains(id) {
            return;
        }
        let Some(payload) = self.panes.payload_of(id).copied() else {
            return;
        };
        let edge_key = |rect: egui::Rect| match dir {
            Dir::Right => rect.max.x,
            Dir::Left => -rect.min.x,
            Dir::Up => -rect.min.y,
            Dir::Down => rect.max.y,
        };
        let mut best: Option<(PaneId, f32)> = None;
        for p in &self.pane_layout.panes {
            if p.id == id {
                continue;
            }
            let key = edge_key(p.rect);
            if best.is_none_or(|(_, k)| key > k) {
                best = Some((p.id, key));
            }
        }
        let Some((target, _)) = best else { return };

        if self.panes.remove(id).is_none() {
            return;
        }
        let axis = match dir {
            Dir::Left | Dir::Right => Axis::Horizontal,
            Dir::Up | Dir::Down => Axis::Vertical,
        };
        let Some(new_id) = self.panes.split(target, axis, payload) else {
            return;
        };
        if matches!(dir, Dir::Left | Dir::Up) {
            // 拆出来的面板排在右侧/下方，交换负载就能让它落到左侧/上方
            if let (Some(a), Some(b)) = (
                self.panes.payload_of(target).copied(),
                self.panes.payload_of(new_id).copied(),
            ) {
                if let Some(slot) = self.panes.payload_of_mut(target) {
                    *slot = b;
                }
                if let Some(slot) = self.panes.payload_of_mut(new_id) {
                    *slot = a;
                }
            }
        }
        self.panes.set_focus(new_id);
        if let Some(t) = self.panes.payload_of(new_id).copied().and_then(PaneKind::terminal_id) {
            self.terminal.request_focus(t);
        }
        self.status = "已移动面板".into();
        self.after_focus_change();
    }

    /// 已经在某块终端面板上就不动，否则聚焦第一块 / 新建一块。
    fn focus_or_new_terminal(&mut self) {
        let already_terminal = self
            .panes
            .focus()
            .and_then(|id| self.panes.payload_of(id).copied())
            .is_some_and(|k| k.terminal_id().is_some());
        if already_terminal {
            return;
        }
        for id in self.panes.pane_ids() {
            if let Some(t) = self.panes.payload_of(id).copied().and_then(PaneKind::terminal_id) {
                if self.panes.set_focus(id) {
                    self.terminal.request_focus(t);
                    self.after_focus_change();
                }
                return;
            }
        }
        self.new_terminal_pane();
    }

    /// 第一块终端面板的 id，自检脚本往里敲命令用。
    fn first_terminal_id(&self) -> Option<u64> {
        self.panes
            .pane_ids()
            .into_iter()
            .find_map(|id| self.panes.payload_of(id).copied().and_then(PaneKind::terminal_id))
    }

    /// 关掉一块面板。只剩一块时不销毁，退回空白编辑面板。
    fn close_pane(&mut self, id: PaneId) {
        let Some(kind) = self.panes.payload_of(id).copied() else {
            return;
        };
        if self.panes.count() <= 1 {
            if let Some(t) = kind.terminal_id() {
                self.terminal.remove(t);
            }
            if let Some(slot) = self.panes.payload_of_mut(id) {
                // 最后一块面板不销毁，退回空白编辑面板；
                // 留着 Terminal(已关掉的 id) 会让这块面板变成死格子
                *slot = PaneKind::Editor(None);
            }
            self.after_focus_change();
            self.status = "已清空面板".into();
            return;
        }
        self.panes.remove(id);
        if let Some(t) = kind.terminal_id() {
            self.terminal.remove(t);
        }
        self.after_focus_change();
        self.status = "已关闭面板".into();
    }

    /// 关闭焦点面板：终端关面板，编辑器关标签。
    fn close_focused(&mut self) {
        let Some(id) = self.panes.focus() else { return };
        match self.panes.payload_of(id).copied() {
            Some(PaneKind::Terminal(_)) => self.close_pane(id),
            Some(PaneKind::Editor(idx)) => {
                if let Some(idx) = idx {
                    self.close_tab(idx);
                }
            }
            None => {}
        }
    }

    // ---------- 格式化 ----------

    /// 格式化焦点面板里的整个文件。
    ///
    /// 只改 buffer 并标脏，不写盘、不开新面板——要不要落盘由用户按 Ctrl+S 决定。
    fn format_active(&mut self) {
        let Some(idx) = self.focused_tab() else {
            self.status = "请先打开一个文件".into();
            return;
        };
        let file = &mut self.tabs[idx];
        if file.large {
            self.status = "大文件为只读预览，无法格式化".into();
            return;
        }
        match format::auto_format(&file.text, Some(file.path.as_path())) {
            Ok(formatted) => {
                if formatted == file.text {
                    self.status = "已经是格式化后的样子".into();
                    return;
                }
                let caret = file.caret_byte.min(formatted.len());
                file.text = formatted;
                file.reindex();
                file.refresh_dirty();
                file.caret_byte = caret;
                self.status = "已格式化，Ctrl+S 保存".into();
                self.revalidate_find();
            }
            Err(e) => self.status = format!("格式化失败：{e}"),
        }
    }

    /// 文件树右键的格式化：已经打开的文件改 buffer（等 Ctrl+S），没打开的原地写盘。
    fn format_file(&mut self, path: PathBuf) {
        if let Some(idx) = self.tabs.iter().position(|t| t.path == path) {
            self.activate_tab(idx);
            self.format_active();
            return;
        }
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string());
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                self.status = format!("读取 {name} 失败：{e}");
                return;
            }
        };
        match format::auto_format(&text, Some(path.as_path())) {
            Ok(formatted) if formatted == text => {
                self.status = format!("{name} 已经是格式化后的样子");
            }
            Ok(formatted) => match std::fs::write(&path, formatted.as_bytes()) {
                Ok(_) => self.status = format!("已格式化并保存 {name}"),
                Err(e) => self.status = format!("写入 {name} 失败：{e}"),
            },
            Err(e) => self.status = format!("格式化失败：{e}"),
        }
    }

    /// `Ctrl+F` 时把选中的文字带进查找框。
    /// 只带单行、长度合理的片段——多行选区通常是整段代码，塞进查找框没意义。
    fn prefill_find_query(&mut self) {
        let Some(idx) = self.focused_tab() else { return };
        let Some(file) = self.tabs.get(idx) else { return };
        let sel = file.selection.trim();
        if sel.is_empty() || sel.contains('\n') || sel.chars().count() > 120 {
            return;
        }
        let sel = sel.to_string();
        if self.find.query != sel {
            self.find.query = sel;
        }
    }

    fn close_find_bar(&mut self) {
        self.find.close();
        self.clear_all_matches();
    }

    /// 清掉所有标签上的匹配高亮。
    ///
    /// 之前只清了当前标签，导致「查找后关闭，再切回另一个标签」时
    /// 会看到没有查找栏却还亮着的孤儿高亮。
    fn clear_all_matches(&mut self) {
        for tab in &mut self.tabs {
            if !tab.matches.is_empty() || tab.current_match.is_some() {
                tab.matches = Arc::new(Vec::new());
                tab.current_match = None;
            }
        }
    }

    /// 换了文件之后匹配偏移全部作废，让查找栏下一帧重算。
    fn revalidate_find(&mut self) {
        if self.find.open {
            self.find.stale = true;
        }
    }

    // ---------- 光标位置历史 ----------

    /// 每帧把当前光标喂给历史；停留够久才真正记一个点。
    fn poll_caret(&mut self, ctx: &egui::Context) {
        let Some(idx) = self.active else { return };
        let Some(file) = self.tabs.get(idx) else { return };
        // 大文件是只读虚拟渲染，没有可编辑光标，不参与历史
        if file.large {
            return;
        }
        let point = NavPoint {
            path: file.path.clone(),
            byte: file.caret_byte,
            line: file.line,
        };
        self.nav.poll(point);
        if self.nav.waiting() {
            // 用户停手后还得再跑一帧才能把位置记进历史
            ctx.request_repaint_after(crate::nav::DWELL + std::time::Duration::from_millis(40));
        }
    }

    fn nav_goto(&mut self, forward: bool) {
        let target = if forward {
            self.nav.forward()
        } else {
            self.nav.back()
        };
        let Some(t) = target else {
            self.status = if forward {
                "已经是最后一个位置了".into()
            } else {
                "没有更早的位置了".into()
            };
            return;
        };

        let name = t
            .path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| t.path.to_string_lossy().to_string());

        if !self.tabs.iter().any(|f| f.path == t.path) {
            self.open_file(t.path.clone(), None);
        }
        let Some(idx) = self.tabs.iter().position(|f| f.path == t.path) else {
            self.status = format!("{} 已无法打开", name);
            return;
        };
        self.set_focused_tab(Some(idx));
        let len = self.tabs[idx].text.len();
        self.tabs[idx].pending_jump = Some(t.byte.min(len));
        self.tabs[idx].jump_without_focus = false;
        self.status = format!("{name} · 行 {}", t.line);
    }

    /// 调试脚本：`EASY_EDITOR_VERIFY=nav|enter` 用真实时间驱动，验证完打印结果。
    /// 用绝对时间而不是帧数，因为不同机器帧率差得远。
    fn verify_script(&mut self, ctx: &egui::Context) {
        let Some(mode) = crate::config::env_var("VERIFY") else {
            return;
        };
        // 窗口初始化可能要一两秒，第一帧来晚的话所有步骤会挤在同一帧里跑完，
        // 中间那些「下一帧才生效」的状态就来不及更新。这里重新对一次表。
        if self.verify_stage == 0 && self.boot.elapsed().as_millis() > 400 {
            self.boot = std::time::Instant::now();
            ctx.request_repaint();
            return;
        }
        let ms = self.boot.elapsed().as_millis() as u64;
        ctx.request_repaint();

        match mode.as_str() {
            "nav" => {
                const STAGES: [u64; 6] = [300, 1000, 1700, 2400, 2700, 2900];
                if self.verify_stage < STAGES.len() && ms >= STAGES[self.verify_stage] {
                    match self.verify_stage {
                        // 依次跳到三处，每处之间隔 700ms，远超 DWELL
                        0 | 1 | 2 => {
                            let line = [40usize, 120, 200][self.verify_stage];
                            if let Some(idx) = self.active {
                                let byte = line_start_offset(&self.tabs[idx].text, line);
                                self.tabs[idx].pending_jump = Some(byte);
                                self.tabs[idx].jump_without_focus = true;
                            }
                        }
                        3 => self.nav_goto(false),
                        4 => {
                            let line = self.active.map(|i| self.tabs[i].line).unwrap_or(0);
                            println!(
                                "[verify] nav 共 {} 个位置，可后退 {} 可前进 {}，后退后落在第 {} 行",
                                self.nav.len(),
                                self.nav.back_count(),
                                self.nav.forward_count(),
                                line
                            );
                        }
                        _ => self.take_screenshot(ctx),
                    }
                    self.verify_stage += 1;
                }
            }
            "enter" => {
                const STAGES: [u64; 5] = [300, 700, 1100, 1500, 1700];
                if self.verify_stage < STAGES.len() && ms >= STAGES[self.verify_stage] {
                    match self.verify_stage {
                        0 => {
                            self.find.open_find(false);
                            self.find.query = "fn ".into();
                            self.find_recompute();
                        }
                        // 往输入队列里塞一个真实的 Enter 事件，走的是和键盘一样的路径
                        1 | 2 => {
                            ctx.input_mut(|i| {
                                i.events.push(egui::Event::Key {
                                    key: Key::Enter,
                                    physical_key: None,
                                    pressed: true,
                                    repeat: false,
                                    modifiers: Modifiers::NONE,
                                })
                            });
                        }
                        3 => {
                            println!(
                                "[verify] enter 当前匹配 {}/{}（状态栏 {}）",
                                self.find.current + 1,
                                self.find.count(),
                                self.find.status_text()
                            );
                        }
                        _ => self.take_screenshot(ctx),
                    }
                    self.verify_stage += 1;
                }
            }
            "keys" => {
                const STAGES: [u64; 5] = [300, 700, 1100, 1400, 1700];
                if self.verify_stage < STAGES.len() && ms >= STAGES[self.verify_stage] {
                    match self.verify_stage {
                        0 => self.show_help = true,
                        // 进入录制状态，同时塞一个 Ctrl+F 事件进去
                        1 => {
                            self.recording = Some("nav_back".into());
                            ctx.input_mut(|i| {
                                i.events.push(egui::Event::Key {
                                    key: Key::F,
                                    physical_key: None,
                                    pressed: true,
                                    repeat: false,
                                    modifiers: Modifiers::CTRL,
                                })
                            });
                        }
                        2 => {
                            println!(
                                "[verify] keys 录制结果 nav_back={} 冲突={:?}",
                                self.keymap.get("nav_back"),
                                self.keymap.conflict_of("nav_back")
                            );
                        }
                        3 => self.take_screenshot(ctx),
                        _ => self.keymap.reset_all(),
                    }
                    self.verify_stage += 1;
                }
            }
            "findflow" => {
                const STAGES: [u64; 6] = [300, 900, 1200, 1800, 2100, 2400];
                if self.verify_stage < STAGES.len() && ms >= STAGES[self.verify_stage] {
                    match self.verify_stage {
                        0 => {
                            self.find.open_find(false);
                            self.find.query = "fn ".into();
                            self.find_recompute();
                            println!("[verify] findflow A 命中 {}", self.find.count());
                        }
                        1 => {
                            let other = self
                                .active
                                .and_then(|i| self.tabs.get(i))
                                .and_then(|f| f.path.parent().map(|p| p.join("b.rs")));
                            if let Some(other) = other {
                                self.open_file(other, None);
                            }
                        }
                        2 => {
                            println!(
                                "[verify] findflow 切到 B 后 open={} 命中={}（A 是 5，B 应为 4）",
                                self.find.open,
                                self.find.count()
                            );
                        }
                        3 => {
                            if let Some(idx) = self.active {
                                self.tabs[idx].selection = "beta_7".into();
                            }
                            self.prefill_find_query();
                            self.find.open_find(false);
                            self.find_recompute();
                            println!(
                                "[verify] findflow 选中带入 query={:?} 命中={}",
                                self.find.query,
                                self.find.count()
                            );
                        }
                        4 => {
                            self.close_find_bar();
                            let left = self
                                .tabs
                                .iter()
                                .filter(|t| !t.matches.is_empty() || t.current_match.is_some())
                                .count();
                            println!(
                                "[verify] findflow 关闭查找后仍有高亮的标签数={left}（应为 0）"
                            );
                        }
                        _ => self.take_screenshot(ctx),
                    }
                    self.verify_stage += 1;
                }
            }
            "split" => {
                const STAGES: [u64; 11] =
                    [400, 1200, 2000, 2800, 3400, 3900, 4500, 5100, 5800, 6400, 7000];
                if self.verify_stage < STAGES.len() && ms >= STAGES[self.verify_stage] {
                    match self.verify_stage {
                        0 => {
                            // 打开第一个文件
                            let p = std::path::PathBuf::from(
                                "C:/Users/blues/AppData/Local/Temp/ee-split/data.json",
                            );
                            self.open_file(p, None);
                        }
                        1 => {
                            // 左右切一刀，两块面板看同一个文件
                            self.split_focused(Axis::Horizontal);
                            println!(
                                "[verify] split 拆分后面板数={} 焦点={:?}",
                                self.panes.count(),
                                self.panes.focus()
                            );
                        }
                        2 => {
                            // 在新面板里打开第二个文件
                            let p = std::path::PathBuf::from(
                                "C:/Users/blues/AppData/Local/Temp/ee-split/data.xml",
                            );
                            self.open_file(p, None);
                            let shown: Vec<Option<usize>> = self
                                .panes
                                .pane_ids()
                                .into_iter()
                                .filter_map(|id| self.panes.payload_of(id).copied())
                                .map(|k| k.tab())
                                .collect();
                            println!("[verify] split 每块面板的标签={shown:?} active={:?}", self.active);
                        }
                        3 => {
                            // 逐块面板格式化
                            let ids = self.panes.pane_ids();
                            for p in ids {
                                if self.panes.payload_of(p).copied().is_some_and(|k| k.is_editor()) {
                                    self.panes.set_focus(p);
                                    self.format_active();
                                }
                            }
                            println!("[verify] split 格式化完成 status={}", self.status);
                        }
                        4 => {
                            // 起两块终端面板，验证 tmux 式终端
                            self.new_terminal_pane();
                            self.new_terminal_pane();
                            println!(
                                "[verify] split 终端面板数={} 终端数={}",
                                self.panes.count(),
                                self.terminal.len()
                            );
                        }
                        5 => {
                            // 往第一块终端敲一条命令，验证 PTY 写入链路
                            match self.first_terminal_id() {
                                Some(id) => {
                                    self.terminal.send_text(id, "echo EASY_EDITOR_PTY_OK\r");
                                    println!("[verify] split 已向终端面板 {id:?} 写入命令");
                                }
                                None => println!("[verify] split 没有终端面板，跳过 PTY 检查"),
                            }
                        }
                        // 下面三步走真实事件队列：按键由 handle_shortcuts 在下一帧消费，
                        // 所以每一步之间要留一帧以上的间隔
                        6 => {
                            ctx.input_mut(|i| {
                                i.events.push(egui::Event::Key {
                                    key: Key::D,
                                    physical_key: None,
                                    pressed: true,
                                    repeat: false,
                                    modifiers: Modifiers::ALT | Modifiers::SHIFT,
                                })
                            });
                        }
                        7 => println!(
                            "[verify] split Alt+Shift+D 后面板数={}（终端面板 2 宽 2 高 → 应左右拆，变 5）",
                            self.panes.count()
                        ),
                        // Alt+Left 落在左边的兄弟终端上，所以「焦点标签=None」是对的，
                        // 要看的是焦点面板的左边缘坐标有没有变小
                        8 => {
                            println!(
                                "[verify] split Alt+Left 前 焦点={}",
                                pane_desc(&self.panes, &self.pane_layout)
                            );
                            ctx.input_mut(|i| {
                                i.events.push(egui::Event::Key {
                                    key: Key::ArrowLeft,
                                    physical_key: None,
                                    pressed: true,
                                    repeat: false,
                                    modifiers: Modifiers::ALT,
                                })
                            });
                        }
                        9 => println!(
                            "[verify] split Alt+Left 后 焦点={}",
                            pane_desc(&self.panes, &self.pane_layout)
                        ),
                        _ => self.take_screenshot(ctx),
                    }
                    self.verify_stage += 1;
                }
            }
            "word" => {
                const STAGES: [u64; 5] = [300, 900, 1400, 2200, 2800];
                if self.verify_stage < STAGES.len() && ms >= STAGES[self.verify_stage] {
                    match self.verify_stage {
                        0 => {
                            // id 既独立出现 3 次，也藏在 width 里两次
                            let dir = std::path::PathBuf::from(
                                "C:/Users/blues/AppData/Local/Temp/ee-word",
                            );
                            let _ = std::fs::create_dir_all(&dir);
                            let p = dir.join("sample.rs");
                            let _ = std::fs::write(
                                &p,
                                "fn calc(id: u32) -> u32 {\n    let width = 3;\n    let area = id * width;\n    id + area\n}\n",
                            );
                            self.open_file(p, None);
                        }
                        1 => {
                            // 选中第 8..10 字节的独立 id → 期望整词匹配 3 处
                            if let Some(idx) = self.focused_tab() {
                                self.tabs[idx].pending_select = Some((8, 10));
                            }
                        }
                        2 => {
                            if let Some(idx) = self.focused_tab() {
                                println!(
                                    "[verify] word 整词选中 id → 高亮 {} 处（期望 3，width 里的不算）",
                                    self.tabs[idx].occurrences.len()
                                );
                            }
                            // 截图停在整词这一步，半截选中的样子肉眼也能推出来
                            self.take_screenshot(ctx);
                        }
                        3 => {
                            // 选中 width 里那半截 id（第 35..37 字节）→ 退化成子串匹配，
                            // 全文 5 处：三个独立 id + 两个 width 里的 id
                            if let Some(idx) = self.focused_tab() {
                                self.tabs[idx].pending_select = Some((35, 37));
                            }
                        }
                        4 => {
                            if let Some(idx) = self.focused_tab() {
                                println!(
                                    "[verify] word 半截选中 → 高亮 {} 处（期望 5）",
                                    self.tabs[idx].occurrences.len()
                                );
                            }
                        }
                        _ => {}
                    }
                    self.verify_stage += 1;
                }
            }
            // 新建对话框能不能吃键盘：先让对话框弹出，再塞一条真实 Text 事件进去
            "newfile" => {
                const STAGES: [u64; 5] = [300, 900, 1500, 2100, 2600];
                if self.verify_stage < STAGES.len() && ms >= STAGES[self.verify_stage] {
                    match self.verify_stage {
                        0 => {
                            let dir = std::path::PathBuf::from(
                                "C:/Users/blues/AppData/Local/Temp/ee-new",
                            );
                            let _ = std::fs::create_dir_all(&dir);
                            let _ = std::fs::write(dir.join("a.txt"), "a\n");
                            self.open_workspace(dir);
                        }
                        1 => {
                            if let Some(tree) = &mut self.tree {
                                tree.request_new(false);
                            }
                        }
                        2 => ctx.input_mut(|i| {
                            i.events.push(egui::Event::Text("probe.txt".into()));
                        }),
                        3 => println!(
                            "[verify] newfile 输入后文件名={:?} 焦点={:?}",
                            self.tree.as_ref().map(|t| t.new_name.clone()),
                            ctx.memory(|m| m.focused().map(|id| id.value()))
                        ),
                        4 => self.take_screenshot(ctx),
                        _ => {}
                    }
                    self.verify_stage += 1;
                }
            }
            _ => {}
        }
    }

    fn take_screenshot(&mut self, ctx: &egui::Context) {
        let dir = config::app_data_dir();
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("screenshot.bmp");
        let target = path.clone();
        ctx.request_screenshot(move |image| {
            let _ = save_bmp(&image, &target);
        });
        self.status = format!("已保存截图 {}", path.display());
    }

    fn set_font_size(&mut self, size: f32, ctx: &egui::Context) {
        self.font_size = size.clamp(10.0, 28.0);
        let _ = ctx;
    }

    /// 切换自动换行。长行要么折进面板，要么横向滚出去，两种都只能看见一种。
    fn toggle_wrap(&mut self) {
        self.word_wrap = !self.word_wrap;
        self.status = if self.word_wrap {
            "已开启自动换行"
        } else {
            "已关闭自动换行，长行可以横向滚动"
        }
        .to_string();
    }

    fn open_quick_open(&mut self) {
        let mut matches = self.index.clone();
        matches.truncate(200);
        self.quick_open = Some(QuickOpen {
            query: String::new(),
            matches,
            sel: 0,
        });
    }

    // ---------- 面板 ----------

    fn toolbar(&mut self, ui: &mut Ui) {
        // 提示文字跟着用户配置走，避免改了快捷键后按钮还在说老组合。
        // 先快照一份，闭包里就不用再借 self 了。
        let keys = self.keymap.to_config();
        let hint = |action: &str| keys.get(action).cloned().unwrap_or_default();

        // 顶栏用可换行的横排：窗口拖窄时按钮排到第二排去，而不是被挤成一团。
        let top = ui.min_rect().bottom();
        ui.horizontal_wrapped(|ui| {
            ui.add_space(2.0);
            let h_open = hint("open_folder");
            if ui
                .button(RichText::new("打开文件夹").size(12.5))
                .on_hover_text(h_open)
                .clicked()
            {
                self.want_open_folder = true;
            }
            let h_save = hint("save");
            if ui
                .add_enabled(
                    self.active.is_some(),
                    egui::Button::new(RichText::new("保存").size(12.5)),
                )
                .on_hover_text(h_save)
                .clicked()
            {
                self.save_active();
            }

            ui.separator();

            // 光标位置前后跳转
            let h_back = format!("上一个光标位置  {}", hint("nav_back"));
            let h_fwd = format!("下一个光标位置  {}", hint("nav_forward"));
            if ui
                .add_enabled(
                    self.nav.can_back(),
                    egui::Button::new(RichText::new("←").size(13.0)),
                )
                .on_hover_text(h_back)
                .clicked()
            {
                self.nav_goto(false);
            }
            if ui
                .add_enabled(
                    self.nav.can_forward(),
                    egui::Button::new(RichText::new("→").size(13.0)),
                )
                .on_hover_text(h_fwd)
                .clicked()
            {
                self.nav_goto(true);
            }

            ui.separator();

            let h_new_file = hint("new_file");
            if ui
                .add_enabled(
                    self.tree.is_some(),
                    egui::Button::new(RichText::new("新文件").size(12.5)),
                )
                .on_hover_text(h_new_file)
                .clicked()
            {
                if let Some(tree) = &mut self.tree {
                    tree.request_new(false);
                }
            }
            let h_new_folder = hint("new_folder");
            if ui
                .add_enabled(
                    self.tree.is_some(),
                    egui::Button::new(RichText::new("新文件夹").size(12.5)),
                )
                .on_hover_text(h_new_folder)
                .clicked()
            {
                if let Some(tree) = &mut self.tree {
                    tree.request_new(true);
                }
            }

            ui.separator();

            let h_side = hint("side_toggle");
            let mut side = self.show_side;
            if ui
                .selectable_label(side, RichText::new("侧栏").size(12.5))
                .on_hover_text(h_side)
                .clicked()
            {
                side = !side;
            }
            self.show_side = side;

            let h_wrap = format!("长行折进面板 / 横向滚动  {}", hint("wrap_toggle"));
            if ui
                .selectable_label(self.word_wrap, RichText::new("折行").size(12.5))
                .on_hover_text(h_wrap)
                .clicked()
            {
                self.toggle_wrap();
            }

            let h_term = format!("聚焦终端面板，没有就开一块  {}", hint("terminal_toggle"));
            if ui
                .button(RichText::new("终端").size(12.5))
                .on_hover_text(h_term)
                .clicked()
            {
                self.focus_or_new_terminal();
            }
            let h_tnew = hint("terminal_new");
            if ui
                .button(RichText::new("+终端").size(12.5))
                .on_hover_text(h_tnew)
                .clicked()
            {
                self.new_terminal_pane();
            }

            let h_col = format!("向右拆分当前面板  {}", hint("split_col"));
            if ui
                .button(RichText::new("‖ 拆分").size(12.5))
                .on_hover_text(h_col)
                .clicked()
            {
                self.split_focused(Axis::Horizontal);
            }
            let h_row = format!("向下拆分当前面板  {}", hint("split_row"));
            if ui
                .button(RichText::new("═ 拆分").size(12.5))
                .on_hover_text(h_row)
                .clicked()
            {
                self.split_focused(Axis::Vertical);
            }

            ui.separator();

            let h_quick = hint("quick_open");
            if ui
                .button(RichText::new("快速打开").size(12.5))
                .on_hover_text(h_quick)
                .clicked()
            {
                self.open_quick_open();
            }
            let h_search = hint("global_search");
            if ui
                .button(RichText::new("全局搜索").size(12.5))
                .on_hover_text(h_search)
                .clicked()
            {
                self.side = SideTab::Search;
                self.show_side = true;
                self.focus_search = true;
            }
            let h_find = hint("find");
            if ui
                .add_enabled(
                    self.active.is_some(),
                    egui::Button::new(RichText::new("查找").size(12.5)),
                )
                .on_hover_text(h_find)
                .clicked()
            {
                self.find.open_find(false);
                self.find_recompute();
            }
            let h_journal = format!("{} 在光标处插入日志块", hint("journal_insert"));
            if ui
                .add_enabled(
                    self.active.is_some(),
                    egui::Button::new(RichText::new("日志块").size(12.5)),
                )
                .on_hover_text(h_journal)
                .clicked()
            {
                self.insert_journal_block();
            }

            // 右组是从右往左排的，这一排剩下的地方不够就会一路压过左组的字，
            // 所以先换一排再排它。注意要量「这一排还剩多少」：可换行布局下
            // available_width() 报的是整排的宽度，永远碰不到这个阈值。
            // 430 = 右组固定那部分（实测约 350）再留一点看工作区名的地方。
            if ui.available_rect_before_wrap().width() < 430.0 {
                ui.end_row();
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let h_help = format!("快捷键设置  {}", hint("help"));
                if ui
                    .button(RichText::new("?").size(12.5))
                    .on_hover_text(h_help)
                    .clicked()
                {
                    self.show_help = !self.show_help;
                }
                if ui
                    .button(RichText::new(if self.dark { "浅色" } else { "深色" }).size(12.5))
                    .on_hover_text("切换主题")
                    .clicked()
                {
                    self.dark = !self.dark;
                    theme::apply_style(ui.ctx(), self.dark);
                    self.hl.set_theme(if self.dark {
                        crate::syntax::DARK_THEME
                    } else {
                        crate::syntax::LIGHT_THEME
                    });
                }
                // 新建终端用哪个 shell。原来在终端面板的页签条上，页签条没了就挪到这儿
                egui::ComboBox::from_id_salt("shell_picker")
                    .selected_text(RichText::new(format!("shell: {}", self.terminal.shell.label())).size(11.5))
                    .width(130.0)
                    .show_ui(ui, |ui| {
                        for kind in self.terminal.available.clone() {
                            if ui
                                .selectable_label(self.terminal.shell == kind, kind.label())
                                .clicked()
                            {
                                self.terminal.shell = kind;
                            }
                        }
                    });
                if ui
                    .small_button("⚙")
                    .on_hover_text("配置终端的程序路径与启动参数")
                    .clicked()
                {
                    self.show_terminal_settings = !self.show_terminal_settings;
                    // 只在重新打开的那一刻要求居中，之后窗口随用户拖到哪算哪
                    self.center_terminal_settings = self.show_terminal_settings;
                }
                let h_dec = hint("font_dec");
                if ui
                    .small_button("-")
                    .on_hover_text(h_dec)
                    .clicked()
                {
                    self.font_size = (self.font_size - 1.0).max(10.0);
                }
                ui.label(RichText::new(format!("{:.0}", self.font_size)).size(11.5).weak());
                let h_inc = hint("font_inc");
                if ui
                    .small_button("+")
                    .on_hover_text(h_inc)
                    .clicked()
                {
                    self.font_size = (self.font_size + 1.0).min(28.0);
                }

                if let Some(root) = &self.root {
                    let parent = root.parent().unwrap_or(root.as_path());
                    // 工作区名可能很长，顶栏不是看全名的地方：排不下就截断，完整路径挂悬浮。
                    ui.add(
                        egui::Label::new(
                            RichText::new(filetree::relative(parent, root))
                                .size(11.5)
                                .weak(),
                        )
                        .truncate(),
                    )
                    .on_hover_text(root.display().to_string());
                }
            });
        });

        // 顶栏换行后要比上一帧更高，而面板高度是照上一帧量的——旧高度的裁剪区会把
        // 第二排切掉。宽度是外部事件，不会再触发一帧，所以这里主动补要一帧。
        let h = (ui.min_rect().bottom() - top).round() as i32;
        if h != self.toolbar_h {
            self.toolbar_h = h;
            ui.ctx().request_repaint();
        }
    }

    fn side_panel(&mut self, ui: &mut Ui) {
        if !self.show_side {
            return;
        }
        let mut tree_action = None;
        let mut journal_jump = None;

        egui::Panel::left("side")
            .resizable(true)
            .default_size(300.0)
            // 上限给足：全局搜索结果里的长路径要靠拉宽侧栏看全
            .size_range(180.0..=900.0)
            .show(ui, |ui| {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    for (tab, label) in [
                        (SideTab::Files, "文件"),
                        (SideTab::Search, "搜索"),
                        (SideTab::Journal, "日志"),
                    ] {
                        if ui
                            .selectable_label(self.side == tab, RichText::new(label).size(12.5))
                            .clicked()
                        {
                            self.side = tab;
                        }
                    }
                    if self.side == SideTab::Journal {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let count = self
                                .active
                                .and_then(|i| self.tabs.get(i))
                                .map(|f| journal::scan(&f.text).len())
                                .unwrap_or(0);
                            ui.label(RichText::new(format!("{count}")).size(11.0).weak());
                        });
                    }
                });
                ui.separator();

                match self.side {
                    SideTab::Files => {
                        if let Some(tree) = &mut self.tree {
                            tree_action = tree.ui(ui, self.dark);
                            if let Some(err) = &tree.error {
                                ui.colored_label(Color32::from_rgb(200, 90, 90), err);
                            }
                        } else {
                            ui.add_space(20.0);
                            ui.vertical_centered(|ui| {
                                ui.label(RichText::new("尚未打开文件夹").weak());
                                if ui.button("打开文件夹").clicked() {
                                    self.want_open_folder = true;
                                }
                            });
                        }
                    }
                    SideTab::Search => {
                        self.search_panel(ui);
                    }
                    SideTab::Journal => {
                        journal_jump = self.journal_panel(ui);
                    }
                }
            });

        match tree_action {
            Some(TreeAction::Open(path)) => self.open_file(path, None),
            Some(TreeAction::Format(path)) => self.format_file(path),
            Some(TreeAction::Created(path)) => {
                if let Some(root) = &self.root {
                    self.index = filetree::collect_files(root, 50_000);
                }
                self.open_file(path, None);
            }
            None => {}
        }
        if let Some(byte) = journal_jump {
            self.journal_jump_to(byte);
        }
    }

    /// 开始全局搜索；没打开文件夹时给出提示，而不是静默无反应。
    fn search_now(&mut self) {
        match self.root.clone() {
            Some(root) => self.search.start(&root),
            None => self.search.info = "先打开文件夹再搜索".into(),
        }
    }

    fn search_panel(&mut self, ui: &mut Ui) {        ui.add_space(2.0);
        let resp = ui.add(
            egui::TextEdit::singleline(&mut self.search.query)
                .hint_text("搜索内容，回车开始")
                .desired_width(f32::INFINITY),
        );
        if self.focus_search {
            resp.request_focus();
            self.focus_search = false;
        }
        let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
        if enter {
            self.search_now();
        }

        ui.horizontal(|ui| {
            if ui.button(RichText::new("搜索").size(12.0)).clicked() {
                self.search_now();
            }
            ui.checkbox(&mut self.search.case_sensitive, "Aa");
            ui.checkbox(&mut self.search.regex, ".*");
            if !self.search.info.is_empty() {
                ui.label(RichText::new(&self.search.info).size(11.0).weak());
            }
        });
        ui.separator();

        if self.search.hits.is_empty() {
            ui.add_space(10.0);
            ui.vertical_centered(|ui| {
                if self.search.busy {
                    ui.spinner();
                    ui.label(RichText::new("搜索中…").weak().size(12.0));
                } else {
                    ui.label(RichText::new("输入关键字后回车").weak().size(12.0));
                    ui.label(
                        RichText::new(format!("搜索引擎：{}", self.search.engine))
                            .weak()
                            .size(11.0),
                    );
                }
            });
            return;
        }

        let root = self.root.clone();
        let hits = self.search.hits.clone();
        let mut clicked: Option<usize> = None;

        TableBuilder::new(ui)
            .id_salt("search_results")
            .striped(true)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            // 交互同时挂在整格和格子里的 Label 上。格子是 egui_extras 在内容之后注册的，
            // 但 Label 铺满格宽后会在命中测试里赢过格子，于是格子只在上下边缘那点区域
            // 收得到 hover；正文的浮窗、手形光标、点击都得靠 Label 自己来。
            // 默认 `Sense::hover()` 只能悬停不能点，所以两处都要显式要 click。
            .sense(egui::Sense::click())
            // 位置列可以拖右边线加宽（宽度记在表格状态里），内容列吃掉剩下的空间。
            // 这里刻意不用 `Column::auto()`：表格每帧都要跑一遍测量 pass，auto 列会被
            // 按内容重新算宽，刚拖出来的宽度下一帧就被打回原形。
            .column(
                Column::initial(140.0)
                    .at_least(60.0)
                    .resizable(true)
                    .clip(true),
            )
            .column(Column::remainder().clip(true))
            .header(18.0, |mut header| {
                let (_, resp) = header.col(|ui| {
                    let lr = ui.add(
                        egui::Label::new(RichText::new("位置").size(11.0).strong())
                            .sense(egui::Sense::hover()),
                    );
                    lr.on_hover_text("拖动表头分隔线可以加宽这一列");
                });
                resp.on_hover_text("拖动表头分隔线可以加宽这一列");
                header.col(|ui| {
                    ui.label(RichText::new("内容").size(11.0).strong());
                });
            })
            .body(|body| {
                body.rows(18.0, hits.len(), |mut row| {
                    let index = row.index();
                    let hit = &hits[index];
                    let tip = hit.path.to_string_lossy().to_string();
                    let (_, resp) = row.col(|ui| {
                        let name = root
                            .as_ref()
                            .map(|r| filetree::relative(r, &hit.path))
                            .unwrap_or_else(|| hit.path.to_string_lossy().to_string());
                        // truncate 只要那个省略号；全文交给浮窗
                        let lr = ui.add(
                            egui::Label::new(
                                RichText::new(format!("{}:{}", name, hit.line))
                                    .size(11.0)
                                    .monospace(),
                            )
                            .truncate()
                            .sense(egui::Sense::click()),
                        );
                        let lr = lr
                            .on_hover_text(tip.clone())
                            .on_hover_cursor(egui::CursorIcon::PointingHand);
                        if lr.clicked() {
                            clicked = Some(index);
                        }
                    });
                    // 格子的 hover 覆盖 Label 之外的边缘，指针也一起变手形
                    let resp = resp
                        .on_hover_text(tip)
                        .on_hover_cursor(egui::CursorIcon::PointingHand);
                    if resp.clicked() {
                        clicked = Some(index);
                    }
                    let tip = format!("第 {} 行：{}", hit.line, hit.preview);
                    let (_, resp) = row.col(|ui| {
                        let lr = ui.add(
                            egui::Label::new(
                                RichText::new(hit.preview.clone()).size(11.0).monospace(),
                            )
                            .truncate()
                            .sense(egui::Sense::click()),
                        );
                        let lr = lr
                            .on_hover_text(tip.clone())
                            .on_hover_cursor(egui::CursorIcon::PointingHand);
                        if lr.clicked() {
                            clicked = Some(index);
                        }
                    });
                    let resp = resp
                        .on_hover_text(tip)
                        .on_hover_cursor(egui::CursorIcon::PointingHand);
                    if resp.clicked() {
                        clicked = Some(index);
                    }
                });
            });

        if let Some(index) = clicked {
            if let Some(hit) = hits.get(index) {
                let path = hit.path.clone();
                let line = hit.line;
                self.open_file(path, Some(line));
            }
        }
    }

    /// 侧栏「日志」页签：列出当前文件里的所有日志块。
    fn journal_panel(&mut self, ui: &mut Ui) -> Option<usize> {
        let Some(idx) = self.active else {
            ui.add_space(12.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("先打开一个文本文件").weak().size(12.0));
                ui.label(
                    RichText::new("日志块是写在文件里的一种格式，\n不是独立数据")
                        .weak()
                        .size(11.0),
                );
            });
            return None;
        };

        let blocks = journal::scan(&self.tabs[idx].text);
        let current_line = self.tabs[idx].line.saturating_sub(1);

        ui.horizontal(|ui| {
            if ui
                .button(RichText::new("插入块").size(11.5))
                .on_hover_text("Ctrl+J")
                .clicked()
            {
                self.insert_journal_block();
            }
            if ui
                .button(RichText::new("上一个").size(11.5))
                .on_hover_text("Ctrl+Alt+↑")
                .clicked()
            {
                self.journal_goto(false);
            }
            if ui
                .button(RichText::new("下一个").size(11.5))
                .on_hover_text("Ctrl+Alt+↓")
                .clicked()
            {
                self.journal_goto(true);
            }
            if ui
                .button(RichText::new("标记完成").size(11.5))
                .on_hover_text("Ctrl+Alt+D")
                .clicked()
            {
                self.journal_toggle();
            }
        });
        ui.add_space(2.0);

        if blocks.is_empty() {
            ui.label(RichText::new("这个文件里还没有日志块").weak().size(11.5));
            return None;
        }

        let mut jump = None;
        egui::ScrollArea::vertical()
            .id_salt("journal_list")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for block in &blocks {
                    let active = block.line == current_line;
                    let mark = if block.is_done() { "[已完成]" } else { "[待处理]" };
                    let label = format!("{} {}", mark, block.title);
                    let resp = ui.add(
                        egui::Label::new(
                            RichText::new(label)
                                .size(12.0)
                                .color(if block.is_done() {
                                    Color32::from_rgb(120, 150, 120)
                                } else {
                                    ui.visuals().text_color()
                                }),
                        )
                        .sense(egui::Sense::click())
                        .truncate(),
                    );
                    if active {
                        ui.painter().rect_filled(
                            resp.rect.expand2(egui::vec2(2.0, 1.0)),
                            3.0,
                            ui.visuals().selection.bg_fill.gamma_multiply(0.28),
                        );
                    }
                    if resp.clicked() {
                        jump = Some(block.head_start);
                    }
                    resp.on_hover_text(format!("{} {} {}", block.date, block.time, block.status));
                }
            });
        jump
    }

    /// 底部查找替换条。
    fn find_bar(&mut self, ui: &mut Ui) {
        if !self.find.open {
            return;
        }
        let mut request_close = false;
        let mut do_find = false;
        let mut do_replace = false;
        let mut do_replace_all = false;
        let mut step: Option<bool> = None;
        let mut query_changed = false;
        // Some(true) -> 回到查找框，Some(false) -> 去替换框
        let mut move_focus: Option<bool> = None;

        let query_id = egui::Id::unique("find_query_input");
        let replace_id = egui::Id::unique("find_replace_input");

        egui::Panel::bottom("find_bar")
            .frame(
                egui::Frame::new()
                    .fill(ui.ctx().global_style().visuals.faint_bg_color)
                    .inner_margin(egui::Margin::symmetric(8, 5)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("查找").size(11.5).weak());

                    // 关键点：按键必须在 TextEdit 之前摘走。
                    // 单行 TextEdit 收到 Enter 会主动 `surrender_focus`，等 add 返回后再判断
                    // `has_focus()` 永远是 false —— 之前 Enter 不生效就是这个原因。
                    let focused = ui.memory(|m| m.has_focus(query_id));
                    let mut enter = false;
                    let mut shift_enter = false;
                    let mut escape = false;
                    let mut tab = false;
                    if focused {
                        enter = ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter));
                        shift_enter = ui.input_mut(|i| i.consume_key(Modifiers::SHIFT, Key::Enter));
                        escape = ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape));
                        // 替换行没展开时不抢 Tab，留给 egui 自己的焦点切换
                        if self.find.show_replace {
                            tab = ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Tab));
                        }
                    }

                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.find.query)
                            .id(query_id)
                            .hint_text("查找内容，Enter 下一个，Shift+Enter 上一个")
                            .desired_width(240.0),
                    );
                    if self.find.focus_query {
                        resp.request_focus();
                        self.find.focus_query = false;
                    }
                    if resp.changed() {
                        query_changed = true;
                    }
                    if enter {
                        step = Some(true);
                    }
                    if shift_enter {
                        step = Some(false);
                    }
                    if escape {
                        request_close = true;
                    }
                    if tab && self.find.show_replace {
                        move_focus = Some(false);
                    }

                    ui.label(
                        RichText::new(self.find.status_text())
                            .size(11.5)
                            .weak()
                            .monospace(),
                    );

                    if ui
                        .small_button("↑")
                        .on_hover_text("上一个 Shift+Enter")
                        .clicked()
                    {
                        step = Some(false);
                    }
                    if ui
                        .small_button("↓")
                        .on_hover_text("下一个 Enter")
                        .clicked()
                    {
                        step = Some(true);
                    }
                    if ui.checkbox(&mut self.find.case_sensitive, "Aa").changed() {
                        query_changed = true;
                    }
                    if ui.checkbox(&mut self.find.regex, ".*").changed() {
                        query_changed = true;
                    }
                    if ui
                        .selectable_label(self.find.show_replace, RichText::new("替换").size(11.5))
                        .on_hover_text("Tab 在查找框和替换框之间切换")
                        .clicked()
                    {
                        self.find.show_replace = !self.find.show_replace;
                    }
                    if ui.small_button("×").on_hover_text("关闭 Esc").clicked() {
                        request_close = true;
                    }
                });

                if self.find.show_replace {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("替换").size(11.5).weak());

                        let focused = ui.memory(|m| m.has_focus(replace_id));
                        let mut enter = false;
                        let mut ctrl_enter = false;
                        let mut shift_enter = false;
                        let mut tab = false;
                        if focused {
                            ctrl_enter =
                                ui.input_mut(|i| i.consume_key(Modifiers::CTRL, Key::Enter));
                            enter = ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter));
                            shift_enter =
                                ui.input_mut(|i| i.consume_key(Modifiers::SHIFT, Key::Enter));
                            tab = ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Tab));
                        }

                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut self.find.replace)
                                .id(replace_id)
                                .hint_text("替换为（正则可用 $1），Enter 替换当前")
                                .desired_width(240.0),
                        );
                        if self.find.focus_replace {
                            resp.request_focus();
                            self.find.focus_replace = false;
                        }
                        if enter {
                            do_replace = true;
                        }
                        if ctrl_enter {
                            do_replace_all = true;
                        }
                        if shift_enter {
                            step = Some(false);
                        }
                        if tab {
                            move_focus = Some(true);
                        }

                        if ui
                            .button(RichText::new("替换").size(11.5))
                            .on_hover_text("Enter")
                            .clicked()
                        {
                            do_replace = true;
                        }
                        if ui
                            .button(RichText::new("全部替换").size(11.5))
                            .on_hover_text("Ctrl+Enter")
                            .clicked()
                        {
                            do_replace_all = true;
                        }
                        if self.find.regex {
                            ui.label(
                                RichText::new("正则模式，替换串支持 $1 $2")
                                    .size(10.5)
                                    .weak(),
                            );
                        }
                    });
                }
            });

        if query_changed {
            do_find = true;
        }
        if do_find {
            self.find_recompute();
        }
        if let Some(forward) = step {
            self.find_step(forward);
        }
        if do_replace {
            self.find_replace_current();
        }
        if do_replace_all {
            self.find_replace_all();
        }
        match move_focus {
            Some(true) => self.find.focus_query = true,
            Some(false) => self.find.focus_replace = true,
            None => {}
        }
        if request_close {
            self.close_find_bar();
        }
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        egui::Panel::bottom("status_bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                if let Some(idx) = self.active {
                    if let Some(file) = self.tabs.get(idx) {
                        let dirty = if file.dirty { " ●" } else { "" };
                        ui.label(
                            RichText::new(format!("{}{}", file.path.display(), dirty))
                                .size(11.0)
                                .weak(),
                        );
                        ui.separator();
                        if file.large {
                            ui.label(
                                RichText::new(format!(
                                    "只读预览 · {} 行 · {}",
                                    file.line_count(),
                                    editor::human_size(file.bytes_len)
                                ))
                                .size(11.0)
                                .color(Color32::from_rgb(190, 140, 60)),
                            );
                        } else {
                            ui.label(RichText::new(editor::cursor_text(file)).size(11.0));
                            ui.separator();
                            let lang = self.hl.language_name(Some(&file.path), &file.text);
                            ui.label(RichText::new(lang).size(11.0).weak());
                        }
                    }
                    // 光标历史：能后退几步 / 能前进几步
                    if self.nav.len() > 1 {
                        ui.separator();
                        ui.label(
                            RichText::new(format!(
                                "历史 ←{} →{}",
                                self.nav.back_count(),
                                self.nav.forward_count()
                            ))
                            .size(11.0)
                            .weak(),
                        )
                        .on_hover_text(format!(
                            "光标停留后自动记录，共 {} 个位置\n{} 后退 / {} 前进",
                            self.nav.len(),
                            self.keymap.get("nav_back"),
                            self.keymap.get("nav_forward")
                        ));
                    }
                } else {
                    ui.label(RichText::new("未打开文件").size(11.0).weak());
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(&self.status).size(11.0).weak());
                    if let Some(idx) = self.active {
                        if let Some(file) = self.tabs.get(idx) {
                            let n = journal::scan(&file.text).len();
                            if n > 0 {
                                ui.separator();
                                ui.label(
                                    RichText::new(format!("{n} 个日志块"))
                                        .size(11.0)
                                        .weak(),
                                );
                            }
                        }
                    }
                });
            });
        });
    }

    fn central(&mut self, ui: &mut Ui) {
        // 空状态提示也读真实快捷键，用户改过之后这里不会说老组合
        let hints = {
            let k = |a: &str| self.keymap.get(a).to_string();
            format!(
                "{} 打开文件夹   {} 快速打开   {} 全局搜索\n\
                 {} 查找   {} 替换   {} 日志块   {} / {} 块间跳转\n\
                 {} 新建文件   {} 终端   {} 新建终端\n\
                 {} 自动拆分   {} / {} 手动拆分   {} 关闭面板   {} 下一个面板   {} 快捷键",
                k("open_folder"),
                k("quick_open"),
                k("global_search"),
                k("find"),
                k("replace"),
                k("journal_insert"),
                k("journal_prev"),
                k("journal_next"),
                k("new_file"),
                k("terminal_toggle"),
                k("terminal_new"),
                k("split_auto"),
                k("split_col"),
                k("split_row"),
                k("pane_close"),
                k("pane_next"),
                k("help"),
            )
        };

        egui::CentralPanel::default().show(ui, |ui| {
            self.tab_bar(ui);
            ui.separator();
            let area = ui.available_rect_before_wrap();

            // 点在哪块面板上就聚焦它：按几何命中判断，容器自己的 response 抢不过里面的 TextEdit
            if let Some(pos) = pane_click(ui.ctx(), area) {
                if let Some(id) = pane_at(&self.pane_layout, pos) {
                    if self.panes.focus() != Some(id) {
                        self.panes.set_focus(id);
                        self.after_focus_change();
                    }
                }
            }

            let layout = self.panes.layout(area, pane::DIVIDER);
            self.draw_panes(ui, &layout, &hints);
            self.pane_layout = layout;
        });
    }

    /// 画面板树：每块面板一个用 `max_rect` 定位的子 `Ui`，最后再叠分隔线。
    ///
    /// 子 `Ui` 一律走 `scope_builder`，这样 egui 会正常收尾（记录 min_rect、推进光标）；
    /// 直接 `Ui::new` 造出来的面板没有父级关联，hover 和键盘焦点都会出问题。
    fn draw_panes(&mut self, ui: &mut Ui, layout: &TreeLayout, hints: &str) {
        let focus = self.panes.focus();
        let font_size = self.font_size;
        let dark = self.dark;
        let wrap = self.word_wrap;
        let keys = self.keymap.to_config();
        let hint = |action: &str| keys.get(action).cloned().unwrap_or_default();

        // 先把每块面板的位置和内容快照下来：下面闭包里要改 self.tabs / self.terminal，
        // 不能同时再借 self.panes。越界的下标顺手当作空白面板处理。
        let panes: Vec<(PaneId, egui::Rect, PaneKind, bool)> = layout
            .panes
            .iter()
            .filter_map(|p| {
                let kind = self.panes.payload_of(p.id).copied()?;
                Some((p.id, p.rect, kind.sanitize(self.tabs.len()), Some(p.id) == focus))
            })
            .collect();

        let mut changed = false;
        let mut close_id: Option<PaneId> = None;
        let mut split_at: Option<(PaneId, Axis)> = None;
        let mut move_to: Option<(PaneId, Dir)> = None;

        for (id, rect, kind, focused) in panes {
            let stroke = if focused {
                ui.style().visuals.selection.stroke
            } else {
                egui::Stroke::NONE
            };
            let pane_id = ui.make_persistent_id(("pane", id.0));
            let builder = egui::UiBuilder::new()
                .scope_id(pane_id)
                .max_rect(rect)
                .layout(egui::Layout::top_down(egui::Align::LEFT));
            ui.scope_builder(builder, |ui| {
                // 子 Ui 的 clip 是从父级继承下来的，只有根 Ui 才会把 clip 设成自己的
                // max_rect。不夹一下，正文里不换行的长行、终端网格这些横向溢出的东西
                // 就照着整个中央面板去画，左右两块面板的文字直接叠在一起。
                ui.shrink_clip_rect(rect);
                // 描边自己画（见循环末尾）：Frame 是照着 content 的 min_rect 上色的，
                // 空白面板内容只有一行标题，焦点框就会缩成一条，撑不满这一格。
                egui::Frame::new()
                    .inner_margin(egui::Margin::same(2))
                    .show(ui, |ui| {
                        let title = match kind {
                            PaneKind::Editor(Some(idx)) => {
                                let file = &self.tabs[idx];
                                let mut t = file.title();
                                if file.dirty {
                                    t.push_str(" ●");
                                }
                                if file.large {
                                    t.push_str(" [大]");
                                }
                                t
                            }
                            PaneKind::Editor(None) => "空白面板".to_string(),
                            PaneKind::Terminal(t) => {
                                let name = self
                                    .terminal
                                    .pane_title(t)
                                    .unwrap_or_else(|| "终端已退出".to_string());
                                format!("⌨ {name}")
                            }
                        };
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::Label::new(RichText::new(title).size(11.5).weak())
                                    .sense(egui::Sense::click()),
                            )
                            .on_hover_text("右键：移动面板")
                            .context_menu(|ui| {
                                // 终端只能开在下方时用户没法把它挪到右边，
                                // 这里给每块面板一个「移到边缘」的入口。
                                ui.label(RichText::new("移动面板").size(11.0).weak());
                                for (label, dir) in [
                                    ("移到最右侧", Dir::Right),
                                    ("移到最左侧", Dir::Left),
                                    ("移到最上方", Dir::Up),
                                    ("移到最下方", Dir::Down),
                                ] {
                                    if ui.button(label).clicked() {
                                        move_to = Some((id, dir));
                                        ui.close();
                                    }
                                }
                            });
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui
                                    .small_button(RichText::new("×").size(11.0))
                                    .on_hover_text(format!("关闭面板 {}", hint("pane_close")))
                                    .clicked()
                                {
                                    close_id = Some(id);
                                }
                                if ui
                                    .small_button(RichText::new("上下").size(10.5))
                                    .on_hover_text(format!("向下拆分 {}", hint("split_row")))
                                    .clicked()
                                {
                                    split_at = Some((id, Axis::Vertical));
                                }
                                if ui
                                    .small_button(RichText::new("左右").size(10.5))
                                    .on_hover_text(format!("向右拆分 {}", hint("split_col")))
                                    .clicked()
                                {
                                    split_at = Some((id, Axis::Horizontal));
                                }
                            });
                        });
                        ui.separator();

                        match kind {
                            PaneKind::Editor(Some(idx)) => {
                                let out = editor::show(
                                    ui,
                                    &mut self.tabs[idx],
                                    &mut self.hl,
                                    font_size,
                                    dark,
                                    pane_id,
                                    wrap,
                                );
                                changed |= out.changed;
                            }
                            PaneKind::Editor(None) => {
                                let full = self.tabs.is_empty() && layout.panes.len() == 1;
                                if pane_welcome(ui, full, hints, &hint) {
                                    self.want_open_folder = true;
                                }
                            }
                            PaneKind::Terminal(t) => {
                                self.terminal.ui_leaf(ui, t, font_size * 0.95, dark);
                            }
                        }
                    });
            });
            if stroke.width > 0.0 {
                ui.painter()
                    .rect_stroke(rect, 0.0, stroke, egui::StrokeKind::Inside);
            }
        }

        for d in &layout.dividers {
            let did = ui.make_persistent_id(("divider", d.id.0));
            let resp = ui.interact(d.rect, did, egui::Sense::drag());
            if resp.dragged() {
                if let Some(pos) = ui.ctx().input(|i| i.pointer.interact_pos()) {
                    let coord = match d.axis {
                        Axis::Horizontal => pos.x,
                        Axis::Vertical => pos.y,
                    };
                    self.panes.drag_divider(d.id, d, coord);
                }
            }
            let hot = resp.hovered() || resp.dragged();
            let fill = if hot {
                ui.visuals().selection.bg_fill.gamma_multiply(0.6)
            } else {
                ui.visuals().faint_bg_color
            };
            ui.painter().rect_filled(d.rect, 0.0, fill);
            if hot {
                ui.ctx().set_cursor_icon(match d.axis {
                    Axis::Horizontal => egui::CursorIcon::ResizeHorizontal,
                    Axis::Vertical => egui::CursorIcon::ResizeVertical,
                });
            }
        }

        if let Some((id, axis)) = split_at {
            self.panes.set_focus(id);
            self.split_focused(axis);
        }
        if let Some((id, dir)) = move_to {
            self.move_pane_to(id, dir);
        }
        if let Some(id) = close_id {
            self.close_pane(id);
        }
        if changed && self.find.open {
            self.find.stale = true;
        }
    }

    /// 顶部标签栏。标签是全局的：一块面板只是「正在显示某个标签」，
    /// 关掉面板不会关掉标签，同一个标签也可以同时显示在两块面板里。
    fn tab_bar(&mut self, ui: &mut Ui) {
        let mut activate = None;
        let mut close = None;
        let shown = self.focused_tab();

        egui::ScrollArea::horizontal()
            .id_salt("tab_bar")
            .max_height(28.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (i, tab) in self.tabs.iter().enumerate() {
                        let mut title = tab.title();
                        if tab.dirty {
                            title.push_str(" ●");
                        }
                        if tab.large {
                            title.push_str(" [大]");
                        }
                        // 有几个面板正在显示这个文件
                        let count = self.pane_count_showing(i);
                        if count > 1 {
                            title.push_str(&format!(" ×{count}"));
                        }

                        if ui
                            .selectable_label(Some(i) == shown, RichText::new(title).size(12.5))
                            .on_hover_text(format!(
                                "{}\n{} 关闭标签 · {} 关闭面板",
                                tab.path.display(),
                                self.keymap.get("tab_close"),
                                self.keymap.get("pane_close"),
                            ))
                            .clicked()
                        {
                            activate = Some(i);
                        }
                        if ui
                            .add(egui::Button::new(RichText::new("×").size(11.0)).frame(false))
                            .on_hover_text("关闭标签")
                            .clicked()
                        {
                            close = Some(i);
                        }
                        ui.add_space(4.0);
                    }
                });
            });

        if let Some(i) = activate {
            self.activate_tab(i);
        }
        if let Some(i) = close {
            self.close_tab(i);
        }
    }

    /// 有多少块面板正在显示这个标签——分屏对比时标签上要能看出来。
    fn pane_count_showing(&self, idx: usize) -> usize {
        self.panes
            .pane_ids()
            .into_iter()
            .filter(|p| self.panes.payload_of(*p).copied().and_then(PaneKind::tab) == Some(idx))
            .count()
    }

    /// 点击标签：已经显示在某块面板里就把焦点切过去，否则送到该显示它的地方。
    fn activate_tab(&mut self, idx: usize) {
        let hit = self
            .panes
            .pane_ids()
            .into_iter()
            .find(|p| self.panes.payload_of(*p).copied().and_then(PaneKind::tab) == Some(idx));
        match hit {
            Some(p) => {
                self.panes.set_focus(p);
                self.after_focus_change();
            }
            None => self.show_in_focused_pane(idx),
        }
        self.revalidate_find();
    }

    // ---------- 弹窗 ----------

    fn quick_open_window(&mut self, ctx: &egui::Context) {
        let Some(state) = &mut self.quick_open else { return };
        let mut close = false;
        let mut choose: Option<PathBuf> = None;
        let mut query_changed = false;

        egui::Window::new("快速打开")
            .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 90.0))
            .resizable(false)
            .collapsible(false)
            .title_bar(false)
            .default_width(620.0)
            .show(ctx, |ui| {
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut state.query)
                        .hint_text("输入文件名，回车打开")
                        .desired_width(f32::INFINITY),
                );
                resp.request_focus();
                if resp.changed() {
                    query_changed = true;
                }
                if ui.input(|i| i.key_pressed(Key::Escape)) {
                    close = true;
                }
                if ui.input(|i| i.key_pressed(Key::ArrowDown)) {
                    state.sel = (state.sel + 1).min(state.matches.len().saturating_sub(1));
                }
                if ui.input(|i| i.key_pressed(Key::ArrowUp)) {
                    state.sel = state.sel.saturating_sub(1);
                }
                let enter = ui.input(|i| i.key_pressed(Key::Enter));
                ui.separator();

                egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                    for (i, path) in state.matches.iter().take(30).enumerate() {
                        let name = path
                            .file_name()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_default();
                        let rel = self
                            .root
                            .as_ref()
                            .map(|r| filetree::relative(r, path))
                            .unwrap_or_else(|| path.to_string_lossy().to_string());
                        let selected = i == state.sel;
                        let text = format!("{name}    {rel}");
                        if ui
                            .selectable_label(selected, RichText::new(text).size(12.0))
                            .clicked()
                        {
                            choose = Some(path.clone());
                        }
                    }
                });

                if enter {
                    if let Some(path) = state.matches.get(state.sel) {
                        choose = Some(path.clone());
                    }
                }
            });

        if query_changed {
            let query = state.query.clone();
            let mut matches: Vec<(i32, PathBuf)> = self
                .index
                .iter()
                .filter_map(|p| {
                    let rel = self
                        .root
                        .as_ref()
                        .map(|r| filetree::relative(r, p))
                        .unwrap_or_else(|| p.to_string_lossy().to_string());
                    fuzzy_score(&rel, &query).map(|score| (score, p.clone()))
                })
                .collect();
            matches.sort_by(|a, b| b.0.cmp(&a.0));
            state.matches = matches.into_iter().map(|(_, p)| p).collect();
            state.sel = 0;
        }

        if let Some(path) = choose {
            self.open_file(path, None);
            close = true;
        }
        if close {
            self.quick_open = None;
        }
    }

    fn close_confirm(&mut self, ctx: &egui::Context) {
        let Some(idx) = self.pending_close else { return };
        if idx >= self.tabs.len() {
            self.pending_close = None;
            return;
        }
        let name = self.tabs[idx].title();
        let mut action: Option<u8> = None;

        egui::Window::new("未保存的修改")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.label(format!("{name} 有未保存的修改，要保存吗？"));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("保存并关闭").clicked() {
                        action = Some(0);
                    }
                    if ui.button("直接关闭").clicked() {
                        action = Some(1);
                    }
                    if ui.button("取消").clicked() {
                        action = Some(2);
                    }
                });
            });

        match action {
            Some(0) => {
                if let Some(file) = self.tabs.get_mut(idx) {
                    let _ = std::fs::write(&file.path, file.text.as_bytes());
                    file.mark_saved();
                }
                self.force_close_tab(idx);
                self.pending_close = None;
            }
            Some(1) => {
                self.force_close_tab(idx);
                self.pending_close = None;
            }
            Some(_) => self.pending_close = None,
            None => {}
        }
    }

    /// 「终端设置」：给当前选中的 shell 指定可执行文件路径和启动参数，改动直接进 config.json。
    ///
    /// 装在非标准目录的 Git Bash（`which` 和内置候选都摸不到）靠这个才起得来。
    fn terminal_settings_window(&mut self, ctx: &egui::Context) {
        if !self.show_terminal_settings {
            return;
        }
        let mut open = true;
        let mut done = false;
        let win_id = egui::Id::unique("terminal_settings");
        let mut window = egui::Window::new("终端设置")
            .id(win_id)
            .open(&mut open)
            .collapsible(false)
            // 窗口跟着内容收放：尺寸写死就会在按钮底下空出一大片
            .auto_sized();
        // anchor 每帧都生效，一直挂着就拖不动，所以只在刚打开那几帧居中。
        // auto_sized 第一帧还在量尺寸（尺寸是无穷大），照那个尺寸算居中会算飞到屏幕外，
        // 因此等 egui 记下真实矩形后再摆正。
        if self.center_terminal_settings && ctx.memory(|m| m.area_rect(win_id).is_some()) {
            window = window.anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO);
            self.center_terminal_settings = false;
        }
        window.show(ctx, |ui| {
            // auto_sized 按最宽的子控件撑窗口，路径字符串一塞就撑到屏幕外，所以宽度锁死
            ui.set_max_width(420.0);
            ui.set_min_width(420.0);

            let kind = self.terminal.shell;
            let id = kind.id().to_string();
            // 先取本地副本：闭包里既要读又要写 self.terminal.settings，直接借会打架
            let mut program = self
                .terminal
                .settings
                .programs
                .get(&id)
                .cloned()
                .unwrap_or_default();
            let mut args = self
                .terminal
                .settings
                .args
                .get(&id)
                .cloned()
                .unwrap_or_default();

            ui.label(
                RichText::new(format!("{}（{}）", kind.label(), kind.id()))
                    .size(12.5)
                    .strong(),
            );
            ui.separator();

            ui.label(RichText::new("程序路径（留空 = 自动探测）").size(11.5).weak());
            let p_resp = ui.add(
                egui::TextEdit::singleline(&mut program)
                    .hint_text(r"C:\Program Files\Git\bin\bash.exe")
                    .desired_width(f32::INFINITY),
            );
            let found = match kind.detected() {
                Some(path) => format!("自动探测到：{}", path.display()),
                None => "本机没探测到，需要手工指定路径".to_string(),
            };
            muted_line(ui, found);
            if !program.trim().is_empty() && !Path::new(program.trim()).is_file() {
                ui.colored_label(ui.visuals().error_fg_color, "这个路径现在还不存在");
            }

            ui.add_space(6.0);
            ui.label(RichText::new("启动参数（留空 = 内置参数）").size(11.5).weak());
            let a_resp = ui.add(
                egui::TextEdit::singleline(&mut args)
                    .hint_text("例如：-NoProfile -NoLogo")
                    .desired_width(f32::INFINITY),
            );
            muted_line(ui, "按空格拆分，带空格的参数用双引号包住。");
            muted_line(ui, format!("改动即时生效，并写进 {}", config::Config::path().display()));

            ui.add_space(6.0);
            let mut cleared = false;
            ui.horizontal(|ui| {
                if ui.button("清除覆盖").clicked() {
                    program.clear();
                    args.clear();
                    cleared = true;
                }
                // 主操作贴到右缘，别和「清除覆盖」挤在一坨
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("完成").clicked() {
                        done = true;
                    }
                });
            });

            if p_resp.changed() || a_resp.changed() || cleared {
                self.terminal
                    .settings
                    .programs
                    .insert(id.clone(), program.trim().to_string());
                self.terminal.settings.args.insert(id, args.trim().to_string());
                // 路径可能让某个种类从「没有」变成「可用」，可用列表要跟着重算
                self.terminal.refresh();
            }
        });
        // open 由标题栏的 ✕ 写，done 由「完成」按钮写，两个都得认
        self.show_terminal_settings = open && !done;
    }

    fn help_window(&mut self, ctx: &egui::Context) {
        if !self.show_help {
            return;
        }

        // 录制中：把这一帧的按键抓下来当新绑定
        if let Some(action) = self.recording.clone() {
            if let Some(combo) = keymap::capture(ctx) {
                match combo.as_str() {
                    "Escape" => self.recording = None,
                    // Backspace / Delete 当作「解绑」
                    "Backspace" | "Delete" => {
                        self.keymap.set(&action, "");
                        self.recording = None;
                    }
                    _ => {
                        self.keymap.set(&action, &combo);
                        self.recording = None;
                    }
                }
            } else {
                // 等待按键时也要刷新，否则「按下按键…」这个提示不会动
                ctx.request_repaint();
            }
        }

        let mut open = self.show_help;
        let warn = Color32::from_rgb(200, 62, 62);
        egui::Window::new("快捷键设置")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(470.0)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui.button(RichText::new("全部恢复默认").size(12.0)).clicked() {
                        self.keymap.reset_all();
                        self.recording = None;
                    }
                    ui.label(
                        RichText::new("点组合键即可重新录制 · Esc 取消 · Backspace 解绑")
                            .size(10.5)
                            .weak(),
                    );
                });
                ui.separator();

                egui::ScrollArea::vertical()
                    .max_height(430.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let mut group = "";
                        for def in keymap::ACTIONS {
                            if def.group != group {
                                if !group.is_empty() {
                                    ui.add_space(8.0);
                                }
                                ui.label(
                                    RichText::new(def.group).size(11.0).strong().weak(),
                                );
                                group = def.group;
                            }

                            ui.horizontal(|ui| {
                                ui.add_sized(
                                    [132.0, 20.0],
                                    egui::Label::new(RichText::new(def.label).size(12.0)),
                                );

                                let recording = self.recording.as_deref() == Some(def.id);
                                let binding = self.keymap.get(def.id).to_string();
                                let text = if recording {
                                    "按下按键…".to_string()
                                } else if binding.is_empty() {
                                    "（未绑定）".to_string()
                                } else {
                                    binding.clone()
                                };
                                let conflict = self.keymap.conflict_of(def.id);
                                let mut rich = RichText::new(text).size(12.0).monospace();
                                if conflict.is_some() {
                                    rich = rich.color(warn);
                                }
                                if ui
                                    .add_sized([148.0, 20.0], egui::Button::new(rich))
                                    .on_hover_text("点击后按下新的组合键")
                                    .clicked()
                                {
                                    self.recording = if recording {
                                        None
                                    } else {
                                        Some(def.id.to_string())
                                    };
                                }

                                if let Some(other) = conflict {
                                    ui.label(
                                        RichText::new(format!("与「{other}」冲突"))
                                            .size(10.5)
                                            .color(warn),
                                    );
                                }

                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if !self.keymap.is_default(def.id)
                                            && ui
                                                .small_button(RichText::new("重置").size(11.0))
                                                .clicked()
                                        {
                                            self.keymap.reset(def.id);
                                        }
                                    },
                                );
                            });
                        }
                    });

                ui.separator();
                ui.label(
                    RichText::new(
                        "查找栏里：Enter 下一个 / Shift+Enter 上一个 / Tab 在查找与替换框间切换 / Esc 关闭。\n\
                         替换框里：Enter 替换当前 / Ctrl+Enter 全部替换。\n\
                         鼠标选中一个词，全文相同的词会一并标出（淡蓝底）；双击可直接取整个词。\n\
                         日志块格式：51 个 = 开头，接标题、日期时间、[状态]，下一行起写正文。",
                    )
                    .size(10.5)
                    .weak(),
                );
            });
        self.show_help = open;
    }
}

impl EasyEditorApp {
    /// 一帧的全部 UI。单独拆出来是因为 `eframe::Frame` 在测试里造不出来，
    /// 拆开后无窗口测试也能驱动一遍真实布局。
    fn draw(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        self.frame_count += 1;

        self.search.poll();
        // 后台面板的输出也要每帧读进屏幕模型，否则 PTY 的 channel 会一直堆
        self.terminal.pump();
        if self.find.open && self.find.stale {
            self.find_recompute();
        }
        // 脚本要往事件队列里塞按键，而 egui 每帧开头会重建 events，
        // 上一帧塞的到下一帧就没了——所以脚本必须排在快捷键分发之前
        self.verify_script(&ctx);
        self.handle_shortcuts(&ctx);

        // 调试用：EASY_EDITOR_VERIFY=insert 时自动插入一个日志块
        if self.frame_count == 20
            && crate::config::env_var("VERIFY").as_deref() == Some("insert")
        {
            self.insert_journal_block();
        }
        // 调试用：EASY_EDITOR_VERIFY=replace 时自动执行全部替换
        if self.frame_count == 30
            && crate::config::env_var("VERIFY").as_deref() == Some("replace")
        {
            self.find_replace_all();
        }

        // 调试用：EASY_EDITOR_SHOT=1 启动后自动截图
        if crate::config::env_flag("SHOT") {
            if self.frame_count == 150 {
                self.take_screenshot(&ctx);
            } else if self.frame_count < 150 {
                ctx.request_repaint();
            }
        }

        if self.want_open_folder {
            self.want_open_folder = false;
            self.open_folder_dialog();
        }

        egui::Panel::top("toolbar")
            .frame(
                egui::Frame::new()
                    .fill(ctx.global_style().visuals.panel_fill)
                    .inner_margin(egui::Margin::symmetric(8, 5)),
            )
            .show(ui, |ui| self.toolbar(ui));

        self.status_bar(ui);
        self.find_bar(ui);
        self.side_panel(ui);
        self.central(ui);
        // 面板树是焦点的事实来源，画完（可能点过、拆过、关过）再刷一次镜像
        self.sync_active();

        self.quick_open_window(&ctx);
        self.close_confirm(&ctx);
        self.help_window(&ctx);
        self.terminal_settings_window(&ctx);

        // 编辑器在 central() 里才会把最新光标写回 OpenFile，所以放在最后收
        self.poll_caret(&ctx);

        if self.search.busy {
            ctx.request_repaint_after(std::time::Duration::from_millis(120));
        }
        // 有终端活着就要持续拉输出；大约 30fps，够跟上手速也不至于空转
        if !self.terminal.is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_millis(33));
        }

        self.sync_config();
    }
}

impl eframe::App for EasyEditorApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.draw(ui);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // 防抖可能还没来得及落盘，退出时补一次
        self.config.save_now();
    }
}

/// 窄面板里的一行说明：超宽就截断，全文交给悬浮提示。
///
/// 浮窗宽度是固定的，而 `C:\Users\<很长>\AppData\Roaming\...` 这种串没有可折行的空格，
/// 直接 label 会横着捅穿窗口边框，盖住底下的内容。
fn muted_line(ui: &mut Ui, text: impl Into<String>) {
    let text = text.into();
    let resp = ui.add(
        egui::Label::new(RichText::new(&text).size(11.0).weak())
            .truncate()
            .sense(egui::Sense::hover()),
    );
    resp.on_hover_text(text);
}

/// 把 `tabs` 里的下标从旧长度重映射到新长度。
///
/// 分屏之后每块面板都存着自己的下标，`Vec::remove` 会让后面全体前移，
/// 忘了挪就会让某块面板莫名其妙地跳到隔壁的文件上。
/// 恰好指向被关标签的那块面板退回空白，由调用方决定要不要顶上隔壁标签。
fn remap_tab(slot: Option<usize>, removed: usize, remaining: usize) -> Option<usize> {
    let a = slot?;
    if remaining == 0 {
        return None;
    }
    if a == removed {
        Some(a.min(remaining - 1))
    } else if a > removed {
        Some(a - 1)
    } else {
        Some(a)
    }
}

/// 这一帧在面板区域里点了一下吗？返回点击位置。
///
/// 只能按几何判断：egui 把容器自身的 sense 注册在它包含的控件**之下**，
/// 所以点在 TextEdit 上的点击永远不会冒泡到面板背景，用父 response 做点击聚焦会失灵。
///
/// 纯几何又会误伤悬浮层：盖在面板上的 `Window`（新建文件、终端设置）和右键菜单，落点
/// 同样"在面板矩形里"。一旦认账就会 `set_focus` → `after_focus_change` → 终端面板
/// `request_focus`，而这一步发生在左侧栏的对话框之后，键盘焦点被终端抢走，
/// 表现为"新建文件时打不了字"。所以先问 egui 这一点命中了哪个可交互 Area。
fn pane_click(ctx: &egui::Context, area: egui::Rect) -> Option<egui::Pos2> {
    let pos = ctx.input(|i| {
        if !i.pointer.primary_clicked() {
            return None;
        }
        i.pointer.interact_pos().filter(|p| area.contains(*p))
    })?;
    // layer_id_at 要读 memory，不能嵌在 ctx.input 的读锁里。
    // 面板本身就在 Background 层，只有 Middle/Foreground 才是真正压在上面的窗口和菜单。
    if ctx
        .layer_id_at(pos)
        .is_some_and(|layer| layer.order >= egui::Order::Middle)
    {
        return None;
    }
    Some(pos)
}

/// 命中测试：这个点落在哪块面板上。
fn pane_at(layout: &TreeLayout, pos: egui::Pos2) -> Option<PaneId> {
    layout
        .panes
        .iter()
        .find(|p| p.rect.contains(pos))
        .map(|p| p.id)
}

/// 调试脚本用：把当前焦点面板描述成一行字，方便核对方向聚焦有没有跳对。
fn pane_desc(panes: &PaneTree<PaneKind>, layout: &TreeLayout) -> String {
    let Some(id) = panes.focus() else {
        return "无焦点".into();
    };
    let kind = match panes.payload_of(id).copied() {
        Some(PaneKind::Editor(idx)) => format!("编辑 标签={idx:?}"),
        Some(PaneKind::Terminal(t)) => format!("终端 id={t}"),
        None => "空".into(),
    };
    let x = layout.pane_rect(id).map_or(-1.0, |r| r.min.x) as i32;
    format!("PaneId({}) {kind} 左边缘x={x}", id.0)
}

/// 空白面板的内容。`full` 是「整屏就这一块面板且一个文件都没开」，那种情况才显示欢迎页。
/// 返回是否请求打开文件夹。
fn pane_welcome(ui: &mut Ui, full: bool, hints: &str, hint: &dyn Fn(&str) -> String) -> bool {
    if !full {
        ui.add_space(16.0);
        ui.vertical_centered(|ui| {
            ui.label(
                RichText::new(format!(
                    "这块面板还没有文件 · {} 打开，点标签也会送到这里",
                    hint("quick_open")
                ))
                .size(12.0)
                .weak(),
            );
        });
        return false;
    }
    let mut want_open = false;
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() * 0.22);
        ui.label(RichText::new("EasyEditor").size(30.0).strong());
        ui.label(
            RichText::new("小而好用的编辑器 · Rust + egui")
                .size(13.0)
                .weak(),
        );
        ui.add_space(14.0);
        if ui.button("打开文件夹").clicked() {
            want_open = true;
        }
        ui.add_space(18.0);
        ui.label(RichText::new(hints).size(11.5).weak());
    });
    want_open
}

/// 把 egui 的截图写成 24 位 BMP（避免引入图片编码依赖）
fn save_bmp(image: &egui::ColorImage, path: &Path) -> std::io::Result<()> {
    let (w, h) = (image.width(), image.height());
    let row_bytes = w * 3;
    let padding = (4 - (row_bytes % 4)) % 4;
    let data_size = (row_bytes + padding) * h;
    let file_size = 54 + data_size;

    let mut out = Vec::with_capacity(file_size);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(file_size as u32).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(h as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&(data_size as u32).to_le_bytes());
    out.extend_from_slice(&2835u32.to_le_bytes());
    out.extend_from_slice(&2835u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());

    for y in (0..h).rev() {
        for x in 0..w {
            let c = image.pixels[y * w + x];
            out.push(c.b());
            out.push(c.g());
            out.push(c.r());
        }
        for _ in 0..padding {
            out.push(0);
        }
    }

    std::fs::write(path, out)
}

fn line_start_offset(text: &str, line: usize) -> usize {
    if line <= 1 {
        return 0;
    }
    let mut current = 1usize;
    for (idx, ch) in text.char_indices() {
        if ch == '\n' {
            current += 1;
            if current == line {
                return idx + 1;
            }
        }
    }
    0
}

/// 子序列模糊匹配，返回分值（越大越靠前）
fn fuzzy_score(haystack: &str, needle: &str) -> Option<i32> {
    if needle.is_empty() {
        return Some(0);
    }
    let hay: Vec<char> = haystack.to_lowercase().chars().collect();
    let nee: Vec<char> = needle.to_lowercase().chars().collect();
    let mut score = 0i32;
    let mut hi = 0usize;
    let mut last_match: Option<usize> = None;

    for &c in &nee {
        let mut found = None;
        while hi < hay.len() {
            if hay[hi] == c {
                found = Some(hi);
                break;
            }
            hi += 1;
        }
        let pos = found?;
        score += 10;
        if let Some(last) = last_match {
            if pos == last + 1 {
                score += 8;
            }
        }
        if pos == 0 || hay.get(pos - 1) == Some(&'/') || hay.get(pos - 1) == Some(&'\\') {
            score += 12;
        }
        last_match = Some(pos);
        hi = pos + 1;
    }
    score -= hay.len().min(60) as i32 / 4;
    Some(score)
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Pos2, Rect, Vec2};

    const SCREEN: Vec2 = Vec2::new(1400.0, 880.0);

    /// 走 egui 正常的输入管线跑一帧。指针状态在 `begin_pass` 里就算好了，
    /// 事后往 `input_mut` 的 events 里塞点击是没用的，必须从这里进。
    fn frame(
        app: &mut EasyEditorApp,
        ctx: &egui::Context,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        frame_at(app, ctx, SCREEN, events)
    }

    /// 按一下键，再多跑两帧。`zoom_factor` 是下一趟 pass 才生效的，当场量看不出来。
    fn press_key(app: &mut EasyEditorApp, ctx: &egui::Context, key: Key, mods: Modifiers) {
        frame(app, ctx, vec![key_press(key, mods)]);
        frame(app, ctx, vec![]);
        frame(app, ctx, vec![]);
    }

    /// 同上，但自己指定屏幕尺寸——窗口挤到多窄就得量多窄。
    fn frame_at(
        app: &mut EasyEditorApp,
        ctx: &egui::Context,
        size: Vec2,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        let raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)),
            events,
            ..Default::default()
        };
        let mut out = ctx.run_pass(raw, |ctx| {
            ctx.root_ui(|ui| app.draw(ui));
        });
        // 没有真的渲染器消费字体图集的增量，epaint 会在 drop 时断言
        out.textures_delta.clear();
        out
    }

    fn pointer(pos: Pos2, button: egui::PointerButton, pressed: bool) -> Vec<egui::Event> {
        vec![
            egui::Event::PointerMoved(pos),
            egui::Event::PointerButton {
                pos,
                button,
                pressed,
                modifiers: Modifiers::NONE,
            },
        ]
    }

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

    fn wait_for_text(
        app: &mut EasyEditorApp,
        ctx: &egui::Context,
        needle: &str,
    ) -> Option<Rect> {
        for _ in 0..4 {
            let out = frame(app, ctx, vec![]);
            if let Some(rect) = find_text(&out, needle) {
                return Some(rect);
            }
        }
        None
    }

    /// 在只放了 `a.txt` 的临时目录里起一个应用，停在侧栏「文件」页签上。
    ///
    /// 页签和侧栏可见性都得显式设：`bootstrap` 读的是真实用户的 config.json，
    /// 里面存着上一次手工操作留下的页签，测试不能跟着它漂。
    fn app_in_folder(dir_name: &str, content: &str) -> (egui::Context, EasyEditorApp, PathBuf) {
        let dir = std::env::temp_dir().join(dir_name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), content).unwrap();

        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(1.0);
        let mut app = EasyEditorApp::bootstrap(&ctx, Some(dir.clone()));
        app.side = SideTab::Files;
        app.show_side = true;
        (ctx, app, dir)
    }

    /// 右键文件树 → 点「新建文件」→ 打字 → 回车建文件。
    ///
    /// 只单测 FileTree 复现不了这条链路：对话框是浮在中央面板上的，
    /// 点菜单那一下还会顺带走过面板聚焦，两边凑在一起才看得出焦点最终落在谁身上。
    #[test]
    fn right_click_new_file_takes_typing_in_full_app() {
        let (ctx, mut app, dir) = app_in_folder("easy-editor-app-menu-test", "a\n");

        let out = frame(&mut app, &ctx, vec![]);
        let row = find_text(&out, "a.txt")
            .expect("文件树里没画出 a.txt")
            .center();

        frame(&mut app, &ctx, pointer(row, egui::PointerButton::Secondary, true));
        frame(
            &mut app,
            &ctx,
            pointer(row, egui::PointerButton::Secondary, false),
        );
        let item = wait_for_text(&mut app, &ctx, "新建文件")
            .expect("右键没有弹出「新建文件」菜单")
            .center();

        frame(&mut app, &ctx, pointer(item, egui::PointerButton::Primary, true));
        frame(
            &mut app,
            &ctx,
            pointer(item, egui::PointerButton::Primary, false),
        );
        let out = frame(&mut app, &ctx, vec![]);
        assert!(
            find_text(&out, "文件名，如 main.rs").is_some(),
            "新建对话框没画出来"
        );

        frame(
            &mut app,
            &ctx,
            vec![egui::Event::Text("probe.txt".into())],
        );
        assert_eq!(
            app.tree.as_ref().unwrap().new_name,
            "probe.txt",
            "对话框里的输入框没收到键盘文字，焦点不在它身上"
        );

        frame(
            &mut app,
            &ctx,
            vec![egui::Event::Key {
                key: Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            }],
        );
        assert!(dir.join("probe.txt").is_file(), "回车没能把文件建出来");
    }

    /// 右键某一行 → 点菜单里的「新建文件」，把对话框打开。返回输入框位置。
    fn open_new_dialog(app: &mut EasyEditorApp, ctx: &egui::Context, row: Pos2) -> Pos2 {
        frame(app, ctx, pointer(row, egui::PointerButton::Secondary, true));
        frame(
            app,
            ctx,
            pointer(row, egui::PointerButton::Secondary, false),
        );
        let item = wait_for_text(app, ctx, "新建文件")
            .expect("右键没有弹出「新建文件」菜单")
            .center();
        frame(app, ctx, pointer(item, egui::PointerButton::Primary, true));
        frame(
            app,
            ctx,
            pointer(item, egui::PointerButton::Primary, false),
        );
        // Window 首帧只量尺寸不画内容，再跑一帧才算真的开好了
        let out = frame(app, ctx, vec![]);
        let hint = find_text(&out, "文件名，如 main.rs");
        assert!(hint.is_some(), "新建对话框没画出来");
        hint.unwrap().center()
    }

    /// 一帧里同时给一个 Key 和一个 Text，和 winit 对可打印键的真实行为一致。
    fn typed(ch: char) -> Vec<egui::Event> {
        let key = match ch {
            'a' => Key::A,
            'b' => Key::B,
            'c' => Key::C,
            'x' => Key::X,
            'y' => Key::Y,
            'z' => Key::Z,
            _ => unreachable!(),
        };
        vec![
            egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            },
            egui::Event::Text(ch.to_string()),
        ]
    }

    /// 正常打字：逐键、每键一帧。
    #[test]
    fn new_file_dialog_takes_keypresses() {
        let (ctx, mut app, _dir) = app_in_folder("easy-editor-app-key-test", "a\n");

        let out = frame(&mut app, &ctx, vec![]);
        let row = find_text(&out, "a.txt")
            .expect("文件树里没画出 a.txt")
            .center();
        open_new_dialog(&mut app, &ctx, row);

        for ch in ['a', 'b', 'c'] {
            frame(&mut app, &ctx, typed(ch));
        }
        assert_eq!(
            app.tree.as_ref().unwrap().new_name,
            "abc",
            "逐个按键打不进新建对话框"
        );
    }

    /// 先点编辑器（键盘焦点落在编辑器上），再走右键新建。
    #[test]
    fn new_file_dialog_takes_keypresses_after_editor_click() {
        let (ctx, mut app, dir) = app_in_folder("easy-editor-app-key-edit-test", "hello\n");
        app.open_file(dir.join("a.txt"), None);

        let out = frame(&mut app, &ctx, vec![]);
        let row = find_text(&out, "a.txt")
            .expect("文件树里没画出 a.txt")
            .center();

        // 在编辑区第一行文字上点一下，把键盘焦点交给编辑器。
        // 必须点在真有文字的地方：TextEdit 只按行数占高，短文件的编辑区下方是空白，
        // 点在空白处连编辑器都命中不了。
        let spot = find_text(&out, "hello\n")
            .or_else(|| find_text(&out, "hello"))
            .expect("编辑器里没画出 hello")
            .center();
        frame(&mut app, &ctx, pointer(spot, egui::PointerButton::Primary, true));
        frame(
            &mut app,
            &ctx,
            pointer(spot, egui::PointerButton::Primary, false),
        );
        for ch in ['x', 'y', 'z'] {
            frame(&mut app, &ctx, typed(ch));
        }
        assert!(
            app.tabs[0].text.contains("xyz"),
            "前置条件不成立：焦点没落到编辑器上"
        );

        open_new_dialog(&mut app, &ctx, row);
        for ch in ['a', 'b', 'c'] {
            frame(&mut app, &ctx, typed(ch));
        }
        assert_eq!(
            app.tree.as_ref().unwrap().new_name,
            "abc",
            "编辑器抢过焦点后，新建对话框就打不进字了"
        );
    }

    /// 悬浮窗盖在面板上时，点它不能算点面板。
    ///
    /// 这是「新建文件打不了字」的根因：认账之后 `after_focus_change` 会给终端面板
    /// `request_focus`，而它发生在左侧栏对话框之后，键盘焦点就被终端抢走。
    /// 但也不能一律不算——点在面板空白处仍要能切换聚焦面板。
    #[test]
    fn pane_click_skips_points_covered_by_floating_windows() {
        let (ctx, mut app, dir) = app_in_folder("easy-editor-app-pane-click-test", "hello\n");
        app.open_file(dir.join("a.txt"), None);

        let out = frame(&mut app, &ctx, vec![]);
        let row = find_text(&out, "a.txt")
            .expect("文件树里没画出 a.txt")
            .center();
        let pane = app.pane_layout.panes[0].rect;
        let dialog = open_new_dialog(&mut app, &ctx, row);
        assert!(
            pane.contains(dialog),
            "前置条件不成立：对话框没落在面板矩形里（{dialog:?} vs {pane:?}）"
        );

        let click = |app: &mut EasyEditorApp, pos: Pos2| {
            frame(app, &ctx, pointer(pos, egui::PointerButton::Primary, true));
            frame(app, &ctx, pointer(pos, egui::PointerButton::Primary, false));
            pane_click(&ctx, pane)
        };

        // 面板左上角那块空白：没有悬浮窗压着，照常算面板点击
        let empty = pane.min + Vec2::new(40.0, 20.0);
        assert_eq!(
            click(&mut app, empty),
            Some(empty),
            "点在面板空白处没算成面板点击，聚焦面板的鼠标切换会失灵"
        );
        assert_eq!(
            click(&mut app, dialog),
            None,
            "点在悬浮对话框上被当成了点面板，焦点会从输入框抢走"
        );
    }

    fn texts(out: &egui::FullOutput) -> Vec<String> {
        fn walk(shape: &egui::Shape, v: &mut Vec<String>) {
            match shape {
                egui::Shape::Text(t) => v.push(t.galley.job.text.clone()),
                egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| walk(s, v)),
                _ => {}
            }
        }
        let mut v = Vec::new();
        out.shapes.iter().for_each(|c| walk(&c.shape, &mut v));
        v
    }

    /// 跑一帧，同时推进 `RawInput.time`。
    /// 悬浮提示的延时是按 time 算的，time 不动就永远等不到。
    fn frame_time(
        app: &mut EasyEditorApp,
        ctx: &egui::Context,
        time: f64,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        let raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, SCREEN)),
            time: Some(time),
            events,
            ..Default::default()
        };
        let mut out = ctx.run_pass(raw, |ctx| {
            ctx.root_ui(|ui| app.draw(ui));
        });
        out.textures_delta.clear();
        out
    }

    /// 造一个「搜索只命中一条、路径长到必须截断」的应用，停在结果表格上。
    fn app_with_long_hit(dir_name: &str) -> (egui::Context, EasyEditorApp, PathBuf) {
        let dir = std::env::temp_dir().join(dir_name);
        let _ = std::fs::remove_dir_all(&dir);
        let nested = dir
            .join("a_very_long_directory_name_for_testing_truncation")
            .join("b_another_nested_level");
        std::fs::create_dir_all(&nested).unwrap();
        let file = nested.join("c_long_named_file.txt");
        std::fs::write(&file, "needle line\n").unwrap();

        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(1.0);
        let mut app = EasyEditorApp::bootstrap(&ctx, Some(dir.clone()));
        app.side = SideTab::Search;
        app.show_side = true;
        app.search.hits = vec![crate::search::SearchHit {
            path: file.clone(),
            line: 1,
            col: 1,
            preview: "needle line".into(),
        }];
        (ctx, app, file)
    }

    fn text_rect_where(out: &egui::FullOutput, pred: impl Fn(&str) -> bool) -> Option<Rect> {
        fn walk(shape: &egui::Shape, pred: &impl Fn(&str) -> bool) -> Option<Rect> {
            match shape {
                egui::Shape::Text(t) => (pred)(&t.galley.job.text)
                    .then(|| t.galley.rect.translate(t.pos.to_vec2())),
                egui::Shape::Vec(shapes) => shapes.iter().find_map(|s| walk(s, pred)),
                _ => None,
            }
        }
        out.shapes.iter().find_map(|c| walk(&c.shape, &pred))
    }

    /// 结果行里那条路径的矩形。用 starts_with 排除浮窗：浮窗显示的是绝对路径。
    fn hit_row(out: &egui::FullOutput) -> Rect {
        text_rect_where(out, |t| t.starts_with("a_very_long")).expect("搜索结果行没画出来")
    }

    /// 结果行右边最近的一条竖线，就是拖列宽用的分隔线。
    fn divider_after(out: &egui::FullOutput, row: Rect) -> f32 {
        fn walk(shape: &egui::Shape, hits: &mut Vec<(f32, f32, f32)>) {
            match shape {
                egui::Shape::LineSegment { points, .. } => {
                    if (points[0].x - points[1].x).abs() < 0.5 {
                        hits.push((points[0].x, points[0].y, points[1].y));
                    }
                }
                egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| walk(s, hits)),
                _ => {}
            }
        }
        let mut lines = Vec::new();
        for c in &out.shapes {
            walk(&c.shape, &mut lines);
        }
        let y = row.center().y;
        lines
            .iter()
            .filter(|(x, _, _)| *x > row.right())
            .filter(|(_, y0, y1)| y0.min(*y1) <= y && y <= y0.max(*y1))
            .map(|(x, _, _)| *x)
            .min_by(f32::total_cmp)
            .unwrap_or_else(|| panic!("结果行右侧 {y} 处找不到列分隔线，竖线={lines:?}"))
    }

    /// 悬浮结果行要浮出完整路径，点一下要能打开对应文件。
    #[test]
    fn search_result_row_hovers_full_path_and_opens_file() {
        let (ctx, mut app, file) = app_with_long_hit("easy-editor-app-search-hover-test");
        let out = frame(&mut app, &ctx, vec![]);
        let before = texts(&out);
        let row = hit_row(&out);

        // 指针到位后不再动，tooltip_delay 才走得完
        let mut out =
            frame_time(&mut app, &ctx, 1.0, vec![egui::Event::PointerMoved(row.center())]);
        for i in 1..8 {
            out = frame_time(&mut app, &ctx, 1.0 + i as f64 * 0.3, vec![]);
        }
        let after = texts(&out);
        let full = file.to_string_lossy().to_string();
        assert!(
            after.iter().any(|t| t == &full),
            "悬浮在结果行上没有浮出完整路径，新增文字 {:?}",
            after
                .iter()
                .filter(|t| !before.contains(t))
                .collect::<Vec<_>>()
        );
        // 手形光标截图里看不出来，只能问 egui 这一帧想设哪个光标
        assert_eq!(
            out.platform_output.cursor_icon,
            egui::CursorIcon::PointingHand,
            "悬浮在结果行上时光标没变成手形"
        );

        let center = row.center();
        frame(&mut app, &ctx, pointer(center, egui::PointerButton::Primary, true));
        frame(
            &mut app,
            &ctx,
            pointer(center, egui::PointerButton::Primary, false),
        );
        assert_eq!(
            app.active.map(|i| app.tabs[i].path.clone()),
            Some(file),
            "点结果行没能打开对应文件"
        );
    }

    /// 拖位置列的分隔线加宽之后，下一帧不能被表格自己的测量 pass 打回原宽。
    ///
    /// `Column::auto()` 会踩这个坑：ScrollArea 每帧先跑一遍 sizing pass，
    /// auto 列在 sizing pass 里无条件按内容重算宽度，把手拖出来的宽度只活一帧。
    #[test]
    fn search_result_column_keeps_the_dragged_width() {
        let (ctx, mut app, _file) = app_with_long_hit("easy-editor-app-search-resize-test");
        let out = frame(&mut app, &ctx, vec![]);
        let row = hit_row(&out);
        let x = divider_after(&out, row);
        let narrow = row.width();
        let y = row.center().y;

        let start = egui::pos2(x, y);
        frame(&mut app, &ctx, vec![egui::Event::PointerMoved(start)]);
        frame(&mut app, &ctx, pointer(start, egui::PointerButton::Primary, true));
        for i in 1..=6 {
            let p = egui::pos2(x + i as f32 * 12.0, y);
            frame(&mut app, &ctx, vec![egui::Event::PointerMoved(p)]);
        }
        frame(
            &mut app,
            &ctx,
            pointer(egui::pos2(x + 72.0, y), egui::PointerButton::Primary, false),
        );

        for i in 0..3 {
            let out = frame(&mut app, &ctx, vec![]);
            let wide = hit_row(&out).width();
            assert!(
                wide > narrow + 20.0,
                "拖宽位置列后第 {i} 帧宽度被打回 {wide}（拖之前 {narrow}）"
            );
        }
    }

    /// 插入日志块后把光标放进标题里打字，不能崩（用户反馈的崩溃路径）。
    #[test]
    fn editing_journal_title_after_insert_does_not_panic() {
        let (ctx, mut app, dir) = app_in_folder("easy-editor-app-journal-title-test", "hello\n");
        app.open_file(dir.join("a.txt"), None);
        for _ in 0..3 {
            frame(&mut app, &ctx, vec![]);
        }
        app.insert_journal_block();
        frame(&mut app, &ctx, vec![]);
        let text = app.tabs[0].text.clone();
        assert!(text.contains(crate::journal::RULE));
        let title_at = text.find("工作日志1").expect("标题不在文本里");
        // 光标落在标题中间（标题是多字节中文，这里专门验字符边界）
        app.tabs[0].pending_jump = Some(title_at + 3);
        frame(&mut app, &ctx, vec![]);
        for ch in ["标", "题", "x"] {
            frame(&mut app, &ctx, vec![egui::Event::Text(ch.into())]);
        }
        assert!(app.tabs[0].text.contains("标题"));
        // 选中标题剩下的部分再覆盖输入
        let text = app.tabs[0].text.clone();
        let a = text.find("日志1").expect("标题不在文本里");
        let b = a + "日志1".len();
        app.tabs[0].pending_select = Some((a, b));
        frame(&mut app, &ctx, vec![]);
        frame(&mut app, &ctx, vec![egui::Event::Text("新标题".into())]);
        assert!(app.tabs[0].text.contains("新标题"));
        // 侧栏日志页签也过一遍
        app.side = SideTab::Journal;
        app.show_side = true;
        for _ in 0..3 {
            frame(&mut app, &ctx, vec![]);
        }
        app.journal_goto(true);
        app.journal_toggle();
        frame(&mut app, &ctx, vec![]);
        assert!(app.tabs[0].text.contains(crate::journal::RULE));
    }

    fn key_press(key: Key, modifiers: Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    /// Ctrl+Shift+F 走完整链路：开侧栏 → 输入关键字 → 回车 → 后台搜索回填结果。
    #[test]
    fn global_search_shortcut_finds_workspace_matches() {
        let (ctx, mut app, _dir) =
            app_in_folder("easy-editor-app-search-flow", "hello world\nbye\n");
        frame(&mut app, &ctx, vec![]);
        frame(
            &mut app,
            &ctx,
            vec![key_press(Key::F, Modifiers::CTRL | Modifiers::SHIFT)],
        );
        assert_eq!(app.side, SideTab::Search, "快捷键没有切到搜索页签");
        assert!(app.show_side, "快捷键没有打开侧栏");

        frame(&mut app, &ctx, vec![egui::Event::Text("hello".into())]);
        assert_eq!(app.search.query, "hello", "搜索框没收到键盘输入");
        frame(&mut app, &ctx, vec![key_press(Key::Enter, Modifiers::NONE)]);

        for _ in 0..200 {
            frame(&mut app, &ctx, vec![]);
            if !app.search.busy && !app.search.hits.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            app.search.hits.iter().any(|h| h.preview.contains("hello")),
            "全局搜索没有结果：info={:?}",
            app.search.info
        );
    }

    /// 「移到最右侧」：原先上下堆叠的面板要变成左右并排，并且贴到工作区右缘。
    #[test]
    fn moving_pane_to_right_splits_side_by_side() {
        let (ctx, mut app, _dir) =
            app_in_folder("easy-editor-app-move-pane-test", "hello\n");
        let root = app.panes.focus().unwrap();
        app.panes.split(root, Axis::Vertical, PaneKind::Editor(None));
        frame(&mut app, &ctx, vec![]);
        let bottom = app.panes.focus().unwrap();
        let before = app.pane_layout.pane_rect(bottom).unwrap();

        app.move_pane_to(bottom, Dir::Right);
        frame(&mut app, &ctx, vec![]);

        let moved_id = app.panes.focus().unwrap();
        let moved = app.pane_layout.pane_rect(moved_id).unwrap();
        let other = app
            .pane_layout
            .panes
            .iter()
            .find(|p| p.id != moved_id)
            .unwrap()
            .rect;
        assert!(
            moved.min.x >= other.max.x,
            "没有变成左右并排：moved={moved:?} other={other:?}"
        );
        assert!(moved.min.x > before.min.x, "没有向右移动：before={before:?}");
    }

    /// 「移到最下方」：左右并排的面板要变成上下堆叠，并且贴到工作区下缘。
    #[test]
    fn moving_pane_to_bottom_stacks_below() {
        let (ctx, mut app, _dir) =
            app_in_folder("easy-editor-app-move-pane-down", "hello\n");
        let left = app.panes.focus().unwrap();
        app.panes.split(left, Axis::Horizontal, PaneKind::Editor(None));
        frame(&mut app, &ctx, vec![]);
        let before = app.pane_layout.pane_rect(left).unwrap();

        app.move_pane_to(left, Dir::Down);
        frame(&mut app, &ctx, vec![]);

        let moved_id = app.panes.focus().unwrap();
        let moved = app.pane_layout.pane_rect(moved_id).unwrap();
        let other = app
            .pane_layout
            .panes
            .iter()
            .find(|p| p.id != moved_id)
            .unwrap()
            .rect;
        assert!(
            moved.min.y >= other.max.y,
            "没有变成上下堆叠：moved={moved:?} other={other:?}"
        );
        assert!(moved.min.y > before.min.y, "没有向下移动：before={before:?}");
        assert_eq!(app.status, "已移动面板");
    }

    /// 「终端设置」浮窗：内容都装得下窗口，而且标题条拖着能动。
    ///
    /// 原来窗口尺寸写死 340×420：条目一多就从下边缘捅出去，按钮画到窗口外面的面板上；
    /// 再加上 anchor 每帧生效，拖到哪都会被弹回屏幕正中。
    #[test]
    fn terminal_settings_window_fits_content_and_moves() {
        let (ctx, mut app, _dir) = app_in_folder("easy-editor-app-settings-window", "a\n");
        app.show_terminal_settings = true;
        app.center_terminal_settings = true;
        // 头几帧只在量尺寸、摆位置，尺寸落定后 egui 才让窗口参与交互
        for _ in 0..5 {
            frame(&mut app, &ctx, vec![]);
        }
        let out = frame(&mut app, &ctx, vec![]);

        let win = egui::Id::unique("terminal_settings");
        let window = ctx
            .memory(|m| m.area_rect(win))
            .expect("没有记下「终端设置」窗口的矩形");
        assert!(
            (window.center() - Pos2::new(SCREEN.x / 2.0, SCREEN.y / 2.0)).length() < 8.0,
            "打开时没摆到屏幕正中：{window:?}"
        );
        let check = |starts: &str| {
            let needle = starts.to_string();
            let r = text_rect_where(&out, |t| t.starts_with(needle.as_str()))
                .unwrap_or_else(|| panic!("窗口里没画出「{starts}」"));
            assert!(
                window.expand2(Vec2::splat(1.0)).contains(r.min)
                    && window.expand2(Vec2::splat(1.0)).contains(r.max),
                "「{starts}」画在窗口外面：文字 {r:?}，窗口 {window:?}"
            );
        };
        for line in [
            "程序路径",
            "自动探测到",
            "启动参数",
            "按空格拆分",
            "改动即时生效",
        ] {
            check(line);
        }

        // 按钮那一行：两个按钮各占一块，谁也不许压在别的文字上
        let clear = find_text(&out, "清除覆盖").expect("没画出「清除覆盖」");
        let done = find_text(&out, "完成").expect("没画出「完成」");
        assert!(
            clear.max.x < done.min.x,
            "两个按钮叠在一起：{clear:?} {done:?}"
        );
        assert!(
            window.expand2(Vec2::splat(1.0)).contains(done.max),
            "「完成」按钮跑到窗口外了：{done:?}，窗口 {window:?}"
        );
        // 按钮是最后一行，底下不许再留一大片空白（用户看到的「按钮压在内容上面」就是顺序 + 空场）
        let footer = text_rect_where(&out, |t| t.starts_with("改动即时生效"))
            .expect("没画出「改动即时生效…」这行");
        assert!(
            footer.max.y <= clear.min.y + 1.0,
            "按钮排到了说明文字上面：按钮 {clear:?}，说明 {footer:?}"
        );
        assert!(
            window.max.y - done.max.y < 24.0,
            "窗口底下空了一大截：按钮行 {done:?}，窗口 {window:?}"
        );

        let title = find_text(&out, "终端设置").expect("窗口标题没画出来");
        let grab = title.center();
        let before = window.min;
        let drag = Vec2::new(120.0, 60.0);
        frame(&mut app, &ctx, pointer(grab, egui::PointerButton::Primary, true));
        for i in 1..=6 {
            let p = grab + drag * (i as f32 / 6.0);
            frame(&mut app, &ctx, vec![egui::Event::PointerMoved(p)]);
        }
        frame(
            &mut app,
            &ctx,
            pointer(grab + drag, egui::PointerButton::Primary, false),
        );
        for _ in 0..3 {
            frame(&mut app, &ctx, vec![]);
        }
        let after = ctx
            .memory(|m| m.area_rect(win))
            .expect("拖完就没有窗口矩形了")
            .min;
        assert!(
            (after - before - drag).length() < 2.0,
            "拖标题条没能挪动窗口：从 {before:?} 拖 {drag:?}，结果在 {after:?}"
        );
    }

    /// 左右两块面板并排时，各自的内容不许画到隔壁那一格去。
    ///
    /// 子 `Ui` 的 clip 是从父级继承的，不会跟着 `max_rect` 收窄，所以正文里不换行的
    /// 长行会照着整个中央面板去画，两块面板的文字叠在一起看不清。
    #[test]
    fn pane_content_is_clipped_to_its_own_rect() {
        /// 返回 (正文, 这一笔实际被夹在哪个矩形里)。galley 自己的矩形可以超出面板
        /// ——那只是排版宽度——真正决定屏幕上有没有画出去的是 `ClippedShape.clip_rect`。
        fn painted(out: &egui::FullOutput, ch: char) -> Vec<(String, Rect)> {
            let mut hits = Vec::new();
            fn walk(shape: &egui::Shape, ch: char, hits: &mut Vec<String>) {
                match shape {
                    egui::Shape::Text(t) => {
                        let s = t.galley.job.text.clone();
                        if s.len() > 40 && s.chars().all(|c| c == ch || c == '\n') {
                            hits.push(s);
                        }
                    }
                    egui::Shape::Vec(shapes) => {
                        for s in shapes {
                            walk(s, ch, hits);
                        }
                    }
                    _ => {}
                }
            }
            for c in &out.shapes {
                let mut inner = Vec::new();
                walk(&c.shape, ch, &mut inner);
                hits.extend(inner.into_iter().map(|s| (s, c.clip_rect)));
            }
            hits
        }

        let (ctx, mut app, dir) = app_in_folder("easy-editor-app-clip", "x\n");
        std::fs::write(dir.join("a_left.txt"), format!("{}\n", "L".repeat(300))).unwrap();
        std::fs::write(dir.join("b_right.txt"), format!("{}\n", "R".repeat(300))).unwrap();
        app.open_file(dir.join("a_left.txt"), None);
        app.open_file(dir.join("b_right.txt"), None);
        let left = app.panes.focus().unwrap();
        let right = app
            .panes
            .split(left, Axis::Horizontal, PaneKind::Editor(Some(0)))
            .unwrap();
        // 左边看左文件、右边看右文件，两边的正文才好分辨
        *app.panes.payload_of_mut(left).unwrap() = PaneKind::Editor(Some(0));
        *app.panes.payload_of_mut(right).unwrap() = PaneKind::Editor(Some(1));

        let mut t = 1.0;
        let mut out = frame_time(&mut app, &ctx, t, vec![]);
        t += 0.1;
        let lrect = app.pane_layout.pane_rect(left).unwrap();
        let rrect = app.pane_layout.pane_rect(right).unwrap();
        assert!(lrect.max.x < rrect.min.x, "两块面板没有左右并排");

        // 指针在两块面板之间来回，悬浮、光标闪烁这些额外绘制也要一起认
        for pos in [lrect.center(), rrect.center(), lrect.center()] {
            out = frame_time(&mut app, &ctx, t, vec![egui::Event::PointerMoved(pos)]);
            t += 0.1;
            for _ in 0..4 {
                out = frame_time(&mut app, &ctx, t, vec![]);
                t += 0.1;
            }
        }

        let left_text = painted(&out, 'L');
        let right_text = painted(&out, 'R');
        assert!(
            !left_text.is_empty() && !right_text.is_empty(),
            "两边正文没同时画出来：L={} R={}",
            left_text.len(),
            right_text.len()
        );
        for (s, clip) in left_text {
            assert!(
                clip.max.x <= lrect.max.x + 1.0,
                "左面板的正文往右画出了自己的格子：{:?} 夹在 {:.0}..{:.0}，格子到 {:.0} 为止",
                &s[..12],
                clip.min.x,
                clip.max.x,
                lrect.max.x
            );
        }
        for (s, clip) in right_text {
            assert!(
                clip.min.x >= rrect.min.x - 1.0,
                "右面板的正文往左画出了自己的格子：{:?} 夹在 {:.0}..{:.0}，格子从 {:.0} 开始",
                &s[..12],
                clip.min.x,
                clip.max.x,
                rrect.min.x
            );
        }
    }

    /// 面板夹了 clip 之后，右键标题的「移动面板」菜单还得照样浮出来、照样点得动。
    ///
    /// 菜单是独立图层的 Area，本就不该跟着面板的裁剪走；这条是给上面那个修复兜底的。
    #[test]
    fn pane_context_menu_escapes_the_pane_clip() {
        let (ctx, mut app, _dir) = app_in_folder("easy-editor-app-pane-menu", "a\n");
        let left = app.panes.focus().unwrap();
        app.panes
            .split(left, Axis::Horizontal, PaneKind::Editor(None));
        frame(&mut app, &ctx, vec![]);
        let lrect = app.pane_layout.pane_rect(left).unwrap();
        let grab = Pos2::new(lrect.min.x + 20.0, lrect.min.y + 10.0);

        frame(&mut app, &ctx, pointer(grab, egui::PointerButton::Secondary, true));
        frame(
            &mut app,
            &ctx,
            pointer(grab, egui::PointerButton::Secondary, false),
        );
        let item = wait_for_text(&mut app, &ctx, "移到最下方")
            .expect("右键标题没有弹出「移动面板」菜单")
            .center();

        frame(&mut app, &ctx, pointer(item, egui::PointerButton::Primary, true));
        frame(
            &mut app,
            &ctx,
            pointer(item, egui::PointerButton::Primary, false),
        );
        frame(&mut app, &ctx, vec![]);
        assert_eq!(app.status, "已移动面板", "点「移到最下方」没有生效");
    }

    /// 收集画面：`(正文, 矩形)`，只挑单层嵌套的图形。
    fn all_texts(out: &egui::FullOutput) -> Vec<(String, Rect)> {
        fn walk(shape: &egui::Shape, v: &mut Vec<(String, Rect)>) {
            match shape {
                egui::Shape::Text(t) => {
                    v.push((t.galley.job.text.clone(), t.galley.rect.translate(t.pos.to_vec2())))
                }
                egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| walk(s, v)),
                _ => {}
            }
        }
        let mut v = Vec::new();
        out.shapes.iter().for_each(|c| walk(&c.shape, &mut v));
        v
    }

    /// 行号栏里的数字，按 y 从上到下。取面板左边那一条竖带，正文不在这儿。
    fn gutter(out: &egui::FullOutput, pane: Rect) -> Vec<(String, Rect)> {
        let mut nums: Vec<(String, Rect)> = all_texts(out)
            .into_iter()
            .filter(|(s, r)| {
                !s.is_empty()
                    && s.chars().all(|c| c.is_ascii_digit())
                    && r.min.x >= pane.min.x
                    && r.max.x <= pane.min.x + 60.0
            })
            .collect();
        nums.sort_by(|a, b| a.1.top().total_cmp(&b.1.top()));
        nums
    }

    /// 那份正文（整份文件一块 galley）的矩形。
    fn body(out: &egui::FullOutput, ch: char) -> Option<Rect> {
        all_texts(out)
            .into_iter()
            .filter(|(s, _)| s.matches(ch).count() > 40)
            .map(|(_, r)| r)
            .max_by(|a, b| a.width().total_cmp(&b.width()))
    }

    /// 折行开关：关着时长行照原样排出面板（好挂横向滚动条），
    /// 开着时同一个长行要折进面板里，而且行号得跟着折出来的视觉行走，
    /// 不能还按「一行一个视觉行」往下排。
    #[test]
    fn word_wrap_folds_long_lines_and_moves_line_numbers() {
        let long = "A".repeat(400);
        let (ctx, mut app, dir) = app_in_folder("easy-editor-app-wrap", "seed\n");
        // 第 1 行超长，第 3 行是空行：空行也要占一个视觉行，不然行号整列会漂
        std::fs::write(dir.join("w.txt"), format!("{long}\nb\n\nc\n")).unwrap();
        app.open_file(dir.join("w.txt"), None);
        app.word_wrap = false;

        let out = frame(&mut app, &ctx, vec![]);
        let pane = app
            .pane_layout
            .pane_rect(app.panes.focus().unwrap())
            .unwrap();
        let flat = body(&out, 'A').expect("不折行时长行没画出来");
        assert!(
            flat.width() > pane.width() * 2.0,
            "不折行的长行本该排出面板，却只有 {:.0} 宽（面板 {:.0}）",
            flat.width(),
            pane.width()
        );
        let nums = gutter(&out, pane);
        assert_eq!(
            nums.len(),
            5,
            "行号不齐：{:?}",
            nums.iter().map(|n| &n.0).collect::<Vec<_>>()
        );
        let row = nums[1].1.top() - nums[0].1.top();
        assert!(row > 1.0, "一个视觉行高只有 {row:.1}？");

        app.word_wrap = true;
        let out = frame(&mut app, &ctx, vec![]);
        let folded = body(&out, 'A').expect("折行后长行没画出来");
        assert!(
            folded.width() <= pane.width(),
            "折行后正文还是比面板宽：{:.0} vs {:.0}",
            folded.width(),
            pane.width()
        );
        let nums = gutter(&out, pane);
        assert_eq!(
            nums.len(),
            5,
            "折行后行号不齐：{:?}",
            nums.iter().map(|n| &n.0).collect::<Vec<_>>()
        );
        // 第 1 行至少折成了两个视觉行，所以第 2 行的行号要比原来低一整行以上
        let gap = nums[1].1.top() - nums[0].1.top();
        assert!(
            gap > row * 1.5,
            "折行后第 2 行的行号没跟着往下挪：差 {gap:.0}，一个行高 {row:.0}"
        );
        // 空行和短行还是一个视觉行，间距要回到行高
        for i in 2..4 {
            let d = nums[i + 1].1.top() - nums[i].1.top();
            assert!(
                (d - row).abs() < 1.0,
                "第 {} 行到第 {} 行的间距是 {d:.1}，行高是 {row:.1}",
                i + 1,
                i + 2
            );
        }
    }

    /// 不折行时长行要真的能横向滚出来，而且行号栏不能跟着滚走。
    #[test]
    fn long_line_scrolls_horizontally_when_wrap_is_off() {
        let long = "A".repeat(400);
        let (ctx, mut app, dir) = app_in_folder("easy-editor-app-hscroll", "seed\n");
        std::fs::write(dir.join("h.txt"), format!("{long}\nb\n")).unwrap();
        app.open_file(dir.join("h.txt"), None);
        app.word_wrap = false;

        let pane = {
            frame(&mut app, &ctx, vec![]);
            app.pane_layout
                .pane_rect(app.panes.focus().unwrap())
                .unwrap()
        };
        // 指针停在正文上，横向滚动条才吃得到滚轮
        let over = pane.center();
        let mut t = 1.0;
        let out = frame_time(&mut app, &ctx, t, vec![egui::Event::PointerMoved(over)]);
        t += 0.1;
        let before = body(&out, 'A').expect("正文没画出来");
        let nums_before = gutter(&out, pane);

        // `delta` 说的是「内容往哪儿挪」，所以要往右滚得给负的 x
        let wheel = egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: Vec2::new(-600.0, 0.0),
            phase: egui::TouchPhase::Move,
            source: egui::MouseWheelSource::Wheel,
            modifiers: Modifiers::NONE,
        };
        // 滚轮有惯性，要跑几帧才停得下来，时间也得跟着推进去
        let mut out = frame_time(&mut app, &ctx, t, vec![wheel]);
        t += 0.1;
        for _ in 0..6 {
            out = frame_time(&mut app, &ctx, t, vec![]);
            t += 0.1;
        }

        let after = body(&out, 'A').expect("滚动后正文没画出来");
        assert!(
            after.left() < before.left() - 20.0,
            "横向滚轮没把正文推走：{:.0} -> {:.0}",
            before.left(),
            after.left()
        );
        let nums_after = gutter(&out, pane);
        assert_eq!(nums_before.len(), nums_after.len());
        for (i, (num, before_r)) in nums_before.iter().enumerate() {
            let after_r = &nums_after[i].1;
            assert!(
                (before_r.left() - after_r.left()).abs() < 1.0,
                "行号 {num} 跟着横向滚走了：{:.0} -> {:.0}",
                before_r.left(),
                after_r.left()
            );
        }
    }

    /// 顶栏那些按钮的字。窄窗口下挤不下时宁可让它们换行，也不能叠着画。
    const TOOLBAR_LABELS: &[&str] = &[
        "打开文件夹",
        "保存",
        "←",
        "→",
        "新文件",
        "新文件夹",
        "侧栏",
        "折行",
        "终端",
        "+终端",
        "‖ 拆分",
        "═ 拆分",
        "快速打开",
        "全局搜索",
        "查找",
        "日志块",
        "深色",
        "?",
    ];

    /// 顶栏里的每一行文字（按钮、字号、shell 下拉、工作区名都算），
    /// 按 (排, 列) 从上到下、从左到右。
    ///
    /// 用「形状属于哪块裁剪区」认人，不用 y 坐标带：顶栏换行后会变高，
    /// 而侧栏表头、欢迎页上也有「新文件」「打开文件夹」这些同名按钮。
    /// 顶栏那条裁剪区贴在窗口顶上，别的都从它下面开始。
    fn toolbar_texts(out: &egui::FullOutput) -> Vec<(String, Rect)> {
        fn walk(shape: &egui::Shape, v: &mut Vec<(String, Rect)>) {
            match shape {
                egui::Shape::Text(t) => {
                    let text = t.galley.job.text.as_str();
                    if !text.trim().is_empty() {
                        v.push((text.to_owned(), t.galley.rect.translate(t.pos.to_vec2())));
                    }
                }
                egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| walk(s, v)),
                _ => {}
            }
        }
        let mut v: Vec<(String, Rect)> = Vec::new();
        for clipped in &out.shapes {
            if clipped.clip_rect.min.y <= 12.0 && clipped.clip_rect.max.y <= 200.0 {
                walk(&clipped.shape, &mut v);
            }
        }
        v.sort_by(|a, b| {
            (a.1.top().total_cmp(&b.1.top())).then_with(|| a.1.left().total_cmp(&b.1.left()))
        });
        v
    }

    /// 两两比对，返回叠在一起的名字对。
    fn overlaps(v: &[(String, Rect)]) -> Vec<(String, String)> {
        let mut bad = Vec::new();
        for (i, (a, ra)) in v.iter().enumerate() {
            for (b, rb) in &v[i + 1..] {
                if ra.intersects(*rb) {
                    bad.push((a.clone(), b.clone()));
                }
            }
        }
        bad
    }

    /// 顶栏文字按 (排, 列) 排好，并跑到高度稳定为止。
    ///
    /// 面板高度是照上一帧的内容量的，顶栏换行后得再画一帧，第二排才不会被旧高度的
    /// 裁剪区切掉——`toolbar` 自己会补要这一帧，测试这边同样多跑几帧模拟重绘。
    fn settled_toolbar(
        app: &mut EasyEditorApp,
        ctx: &egui::Context,
        size: Vec2,
    ) -> Vec<(String, Rect)> {
        let mut v = Vec::new();
        for _ in 0..3 {
            v = toolbar_texts(&frame_at(app, ctx, size, vec![]));
        }
        v
    }

    fn names(v: &[(String, Rect)]) -> Vec<&str> {
        let mut n: Vec<&str> = v.iter().map(|(s, _)| s.as_str()).collect();
        n.sort_unstable();
        n
    }

    /// 拖窄那一帧顶栏还是旧高度，第二排会被裁剪区切掉，所以换行得主动补要重绘帧；
    /// 但补个一两帧量稳就该停，一直要会变成空转。
    #[test]
    fn toolbar_repaint_settles_when_it_stacks() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let (ctx, mut app, _dir) = app_in_folder("easy-editor-app-toolbar-repaint", "a\n");
        let hits = std::sync::Arc::new(AtomicUsize::new(0));
        {
            let hits = hits.clone();
            ctx.set_request_repaint_callback(move |_| {
                hits.fetch_add(1, Ordering::Relaxed);
            });
        }
        let count = || hits.load(Ordering::Relaxed);

        // 头一帧还在量顶栏高度，那一次补要不作数
        frame(&mut app, &ctx, vec![]);
        frame(&mut app, &ctx, vec![]);
        let base = count();
        frame(&mut app, &ctx, vec![]);
        assert_eq!(count(), base, "宽窗口顶栏稳定后还在反复要重绘：{base}");

        frame_at(&mut app, &ctx, Vec2::new(900.0, 880.0), vec![]);
        let jumped = count();
        assert!(
            jumped > base,
            "顶栏换行没补要重绘帧：第二排会被旧高度的裁剪区切掉"
        );
        frame_at(&mut app, &ctx, Vec2::new(900.0, 880.0), vec![]);
        assert!(
            count() <= base + 2,
            "一次换行最多补两帧就该停：base={base}，一帧后={jumped}"
        );
        for _ in 0..3 {
            frame_at(&mut app, &ctx, Vec2::new(900.0, 880.0), vec![]);
        }
        let stacked = count();
        for _ in 0..3 {
            frame_at(&mut app, &ctx, Vec2::new(900.0, 880.0), vec![]);
        }
        assert_eq!(count(), stacked, "顶栏高度量稳后还在反复要重绘：会空转");

        // 拖回去也一样：换回一排同样要补帧，补完就停
        frame(&mut app, &ctx, vec![]);
        frame(&mut app, &ctx, vec![]);
        assert!(count() > stacked, "顶栏收回一排同样要补帧");
        for _ in 0..3 {
            frame(&mut app, &ctx, vec![]);
        }
        let back = count();
        for _ in 0..3 {
            frame(&mut app, &ctx, vec![]);
        }
        assert_eq!(count(), back, "回到宽窗口后还在反复要重绘：会空转");
    }

    /// 刚打开、没动过的文件不该挂未保存标记（标签、面板标题、状态栏都不该出现 ●）。
    #[test]
    fn freshly_opened_file_shows_no_unsaved_marker() {
        let (ctx, mut app, dir) = app_in_folder("easy-editor-app-clean-marker", "hello\nworld\n");
        app.open_file(dir.join("a.txt"), None);
        let mut out = frame_time(&mut app, &ctx, 0.0, vec![]);
        for i in 1..6 {
            out = frame_time(&mut app, &ctx, i as f64, vec![]);
        }
        assert!(!app.tabs[0].dirty, "刚打开的文件被标成了已修改");
        assert!(
            !texts(&out).iter().any(|t| t.contains('●')),
            "没改过的文件上方出现了修改标记：{:?}",
            texts(&out)
        );
    }

    /// 输入一个字再撤销回原样，内容又和磁盘一致了，标记要跟着消失。
    #[test]
    fn undo_back_to_saved_text_clears_unsaved_marker() {
        let (ctx, mut app, dir) = app_in_folder("easy-editor-app-undo-marker", "hello\nworld\n");
        app.open_file(dir.join("a.txt"), None);
        let mut t = 0.0;
        let out = frame_time(&mut app, &ctx, t, vec![]);
        let p = text_rect_where(&out, |s| s.contains("hello"))
            .map(|r| r.center())
            .expect("没找到正文");
        for pressed in [true, false] {
            t += 0.1;
            frame_time(
                &mut app,
                &ctx,
                t,
                pointer(p, egui::PointerButton::Primary, pressed),
            );
        }
        t += 0.1;
        frame_time(&mut app, &ctx, t, vec![egui::Event::Text("x".into())]);
        assert!(app.tabs[0].dirty, "输入内容后没有标记为已修改");

        // egui 的撤销点要等输入稳定 1 秒才落，先空跑几帧
        for _ in 0..4 {
            t += 0.4;
            frame_time(&mut app, &ctx, t, vec![]);
        }

        // 真实键盘事件里 Ctrl 同时带 ctrl/command 两个位，只写 CTRL 匹配不上 COMMAND
        let cmd = Modifiers {
            ctrl: true,
            command: true,
            ..Default::default()
        };
        t += 0.1;
        frame_time(
            &mut app,
            &ctx,
            t,
            vec![
                egui::Event::ModifiersChanged(cmd),
                key_press(Key::Z, cmd),
            ],
        );
        assert_eq!(
            app.tabs[0].text, "hello\nworld\n",
            "撤销没有恢复原内容：{:?}",
            app.tabs[0].text
        );
        assert!(!app.tabs[0].dirty, "撤销回原样后仍挂着未保存标记");
    }

    /// 窗口拖窄时顶栏按钮换排，不能挤成一团把字叠着画；够宽时仍然是一排。
    #[test]
    fn toolbar_stacks_instead_of_overlapping_when_narrow() {
        let (ctx, mut app, _dir) = app_in_folder("easy-editor-app-toolbar", "a\n");
        // 那个按钮写「深色」还是「浅色」跟着 self.dark 走，钉死免得跟着用户配置漂
        app.dark = false;
        let mut want = TOOLBAR_LABELS.to_vec();
        want.sort_unstable();

        // 从窗口最小宽度往上一档一档量：哪一档都不许叠字、不许挤掉按钮、不许画出右边缘
        for w in [800.0, 900.0, 1000.0, 1100.0, 1200.0, 1300.0, 1400.0] {
            let all = settled_toolbar(&mut app, &ctx, Vec2::new(w, 880.0));
            let buttons: Vec<(String, Rect)> = all
                .iter()
                .filter(|(s, _)| TOOLBAR_LABELS.contains(&s.as_str()))
                .cloned()
                .collect();
            assert_eq!(
                names(&buttons),
                want,
                "{w} 宽顶栏按钮没排齐：{:?}",
                names(&buttons)
            );
            assert!(
                overlaps(&all).is_empty(),
                "{w} 宽顶栏叠字：{:?}",
                overlaps(&all)
            );
            assert!(
                all.iter().all(|(_, r)| r.right() <= w - 8.0),
                "{w} 宽顶栏画到窗口外面了：{all:?}"
            );

            // 1400 放得下一整排，就别白占高度；900 放不下，必须换排
            if w >= 1400.0 {
                assert!(
                    all.iter().all(|(_, r)| r.top() < 24.0),
                    "1400 宽顶栏占了不止一排：{all:?}"
                );
            }
            if w <= 900.0 {
                assert!(
                    all.iter().any(|(_, r)| r.top() >= 24.0),
                    "{w} 宽顶栏还在硬挤一排：{all:?}"
                );
            }
        }
    }

    /// 字号快捷键只动编辑器和终端的字号，不能把整个界面的缩放带跑偏。
    ///
    /// 曾经 `Ctrl+=` 漏给了 egui 内置的整窗缩放（菜单、侧栏跟着变大），而 `Ctrl+-`
    /// 被本应用消费掉（只缩字号）——只放不缩，界面卡在放大状态回不去。
    #[test]
    fn font_shortcuts_scale_only_the_editor_font() {
        let (ctx, mut app, _dir) = app_in_folder("easy-editor-app-font-keys", "a\n");
        app.font_size = 14.0;
        // 真实键盘事件里 Ctrl 同时带 ctrl/command 两个位，只写 CTRL 匹配不上 COMMAND
        let ctrl = Modifiers {
            ctrl: true,
            command: true,
            ..Default::default()
        };

        // 不带 Shift 的 `Ctrl+=` 才是日常按「+」的方式
        press_key(&mut app, &ctx, Key::Equals, ctrl);
        assert_eq!(app.font_size, 15.0, "Ctrl+= 没放大编辑器字号");
        assert_eq!(ctx.zoom_factor(), 1.0, "Ctrl+= 把整个界面缩放了");

        // 带 Shift 的那一下是 Key::Plus，egui 的 ZOOM_IN 认的就是它
        press_key(
            &mut app,
            &ctx,
            Key::Plus,
            Modifiers { shift: true, ..ctrl },
        );
        assert_eq!(app.font_size, 16.0, "Ctrl+Shift+= 没放大编辑器字号");
        assert_eq!(ctx.zoom_factor(), 1.0, "Ctrl+Shift+= 把整个界面缩放了");

        for _ in 0..2 {
            press_key(&mut app, &ctx, Key::Minus, ctrl);
        }
        assert_eq!(app.font_size, 14.0, "Ctrl+- 没缩回编辑器字号");
        assert_eq!(ctx.zoom_factor(), 1.0, "Ctrl+- 把整个界面缩放了");

        // egui 的缩放复位键（本应用没绑定）也该被关掉，别留下半放大的界面
        press_key(&mut app, &ctx, Key::Num0, ctrl);
        assert_eq!(ctx.zoom_factor(), 1.0, "Ctrl+0 动了整窗缩放");
    }
}
