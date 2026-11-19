//! 应用主体：布局、快捷键、文件打开 / 保存、全局搜索、查找替换、日志块、终端。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use eframe::egui::{self, Color32, Key, Modifiers, RichText, Ui};
use egui_extras::{Column, TableBuilder};

use crate::config::{self, ConfigStore};
use crate::database::DatabasePanel;
use crate::editor::{self, OpenFile, PendingEdit};
use crate::filetree::{self, FileTree, TreeAction};
use crate::find::FindState;
use crate::format;
use crate::ime;
use crate::journal;
use crate::keymap::{self, Keymap};
use crate::nav::{CaretHistory, NavPoint};
use crate::pane::{self, Axis, Dir, PaneId, PaneTree, TreeLayout};
use crate::results::{self, ResultsDock, Settled};
use crate::search::SearchState;
use crate::sql;
use crate::syntax::Highlighter;
use crate::terminal::{CustomShell, Shell, ShellKind, TerminalPanel, TerminalSettings};
use crate::theme;
use crate::titlebar;

#[derive(Debug, PartialEq, Clone, Copy)]
enum SideTab {
    Files,
    Search,
    Journal,
    Database,
}

impl SideTab {
    fn key(self) -> &'static str {
        match self {
            Self::Files => "files",
            Self::Search => "search",
            Self::Journal => "journal",
            Self::Database => "database",
        }
    }

    fn from_key(key: &str) -> Self {
        match key {
            "search" => Self::Search,
            "journal" => Self::Journal,
            "database" => Self::Database,
            _ => Self::Files,
        }
    }
}

/// 设置窗口里的分区。左侧导航照 `ALL` 的顺序排。
///
/// 快捷键和终端设置原来是两个各开各的窗口，用户得先知道「哪个设置藏在哪个窗口里」。
/// 收进这一个窗口，入口只剩一个，`?` 和 `⚙` 只是各自跳到对应分区。
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum SettingsSection {
    Appearance,
    Keymap,
    Terminal,
    System,
    Database,
}

impl SettingsSection {
    const ALL: [Self; 5] = [
        Self::Appearance,
        Self::Keymap,
        Self::Terminal,
        Self::System,
        Self::Database,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Appearance => "外观",
            Self::Keymap => "快捷键",
            Self::Terminal => "终端",
            Self::System => "系统",
            Self::Database => "数据库",
        }
    }
}

/// 中央区域的一块面板：要么在看文件，要么在跑 shell。
///
/// 编辑器面板存的是 `tabs` 的下标而不是路径：同一个文件允许同时出现在两块面板里
/// （对比、边看边写），用下标才表达得出「两块面板共用一份 buffer」。
///
/// `Terminal` 只出现在终端页签的树里：编辑那棵树从头到尾只有 `Editor`，
/// 两边共用这一个类型，面板命令（拆分 / 移动 / 关闭 / 聚焦）才能用同一套代码
/// 按当前作用域作用在对应的树上，不必为两种载荷各写一遍。
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

/// 面板命令的作用范围：编辑那棵树，还是某个终端页签的 shell 树。
///
/// 两棵树装的是同一个 [`PaneKind`]，所以结构性动作（拆分 / 移动 / 关闭 / 切焦点）
/// 用同一套命令、同一套网格代码，靠这个作用域决定打到哪棵树上。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum PaneScope {
    Main,
    Term(u64),
}

/// 一组终端：页签条上占一格，里面是一棵可以切分的 shell 面板树。
///
/// 终端页签和文件页签排在同一条上（浏览器式）：点「+终端」新开一格，一格里面还能
/// 再上下左右切分，每格一条独立 PTY。格子里的 × 只结束那一条 shell，页签上的 ×
/// 把这一组全收掉；组里最后一格被关掉，这个页签跟着消失。
struct TermTab {
    /// 稳定序号：页签会从中间被关掉，命令按它找页签，不按下标
    id: u64,
    tree: PaneTree<PaneKind>,
    /// 这一帧算出的面板矩形：方位跳转、移动面板定落点、拖分隔条都要用它
    layout: TreeLayout,
}

impl TermTab {
    /// 新开一组终端：里面只有 `term_id` 这一条 shell。
    fn new(id: u64, term_id: u64) -> Self {
        let mut tree = PaneTree::new();
        tree.set_root(PaneKind::Terminal(term_id));
        Self {
            id,
            tree,
            layout: TreeLayout::default(),
        }
    }

    /// 这一组里的 shell 有哪些（顺序按面板树）。
    fn term_ids(&self) -> Vec<u64> {
        self.tree
            .pane_ids()
            .into_iter()
            .filter_map(|p| self.tree.payload_of(p).copied().and_then(PaneKind::terminal_id))
            .collect()
    }

    /// 页签上的名字：跟着第一条还活着的 shell 走，一排页签里看不出哪个是哪个就不好了。
    fn title(&self, panel: &TerminalPanel) -> String {
        self.term_ids()
            .into_iter()
            .find_map(|t| panel.pane_title(t))
            .unwrap_or_else(|| "已退出".to_string())
    }

    /// 面板级命令（拆分 / 关闭 / 移动 / 聚焦）作用在 `self.tree` 上。
    /// 返回要说给用户听的那句（起不了 shell、关掉了一格），没什么要说就 `None`。
    fn command(&mut self, cmd: PaneCmd, panel: &mut TerminalPanel) -> Option<String> {
        match cmd {
            PaneCmd::Split(id, axis) => {
                self.tree.set_focus(id);
                let target = self.tree.focus()?;
                let t = self
                    .tree
                    .payload_of(target)
                    .copied()
                    .and_then(PaneKind::terminal_id)?;
                // 同组再切一格就是再起一条 shell，跟着那格的工作目录走
                let cwd = panel
                    .get(t)
                    .map(|term| term.cwd.clone())
                    .unwrap_or_else(|| panel.cwd.clone());
                let new_id = match panel.create_at(&cwd) {
                    Ok(id) => id,
                    Err(e) => return Some(e),
                };
                if self
                    .tree
                    .split(target, axis, PaneKind::Terminal(new_id))
                    .is_none()
                {
                    // 树里没这块面板了（同帧内被关掉的快捷键会撞见），别留下没人看的 shell
                    panel.remove(new_id);
                    return Some("没有可拆分的面板".into());
                }
                Some("已在本组新建一格终端".into())
            }
            PaneCmd::Move(id, dir) => {
                let moved = self.tree.move_to_edge(&self.layout, id, dir);
                self.request_focused_term(panel);
                moved.map(|_| "已移动面板".into())
            }
            PaneCmd::Close(id) => {
                let Some(PaneKind::Terminal(t)) = self.tree.payload_of(id).copied() else {
                    return None;
                };
                panel.remove(t);
                self.tree.remove(id);
                if self.tree.count() == 0 {
                    // 最后一格关掉 = 这一组没用了，页签跟着收掉（调用方按 `tree.count()`
                    // 把它摘掉）。留一个没 shell 的空页签不如关掉。
                    return Some("已关闭终端页签".into());
                }
                self.request_focused_term(panel);
                Some("已关闭面板".into())
            }
            PaneCmd::Focus(id) => {
                self.tree.set_focus(id);
                self.request_focused_term(panel);
                None
            }
            PaneCmd::Cycle(delta) => {
                self.tree.focus_cycle(delta);
                self.request_focused_term(panel);
                None
            }
            PaneCmd::DirFocus(dir) => {
                self.tree.focus_dir(&self.layout, dir);
                self.request_focused_term(panel);
                None
            }
        }
    }

    /// 键盘焦点跟着面板走：焦点格里的 shell 拿到输入，别的 shell 不会串台。
    fn request_focused_term(&self, panel: &mut TerminalPanel) {
        if let Some(t) = self
            .tree
            .focus()
            .and_then(|id| self.tree.payload_of(id).copied())
            .and_then(PaneKind::terminal_id)
        {
            panel.request_focus(t);
        }
    }
}

/// 面板级命令。两棵树装的是同一个 `PaneKind`，所以同一份命令既能打在编辑树上，
/// 也能打在终端页签的 shell 树上——`Split` / `Move` 这些结构性动作由 `PaneTree`
/// 自己做，载荷怎么来的由各自的实现管。
enum PaneCmd {
    /// 在 `id` 这格旁边再切一格（终端页签里 = 再起一条同目录的 shell）
    Split(PaneId, Axis),
    Move(PaneId, Dir),
    Close(PaneId),
    /// 点哪格就聚焦哪格（按几何命中，见 [`TreeLayout::pane_at`]）
    Focus(PaneId),
    /// 顺序切焦点（Ctrl+Alt+← / →）
    Cycle(isize),
    /// 按方位切焦点
    DirFocus(Dir),
}

/// 一格面板在画之前定下来的样子。网格只吃这份快照，命令再按 [`PaneScope`]
/// 打回对应的树，这样画面板的闭包里可以自由改 `self` 的其它字段（标签、终端、状态）。
struct PaneLeaf {
    id: PaneId,
    rect: egui::Rect,
    kind: PaneKind,
    focused: bool,
}

/// 把一棵树在这一帧的矩形读成快照。标签下标越界的（刚关掉标签）顺手退回空白面板。
fn pane_leaves(
    tree: &PaneTree<PaneKind>,
    layout: &TreeLayout,
    tabs_len: usize,
) -> Vec<PaneLeaf> {
    let focus = tree.focus();
    layout
        .panes
        .iter()
        .filter_map(|p| {
            let kind = tree.payload_of(p.id).copied()?;
            Some(PaneLeaf {
                id: p.id,
                rect: p.rect,
                kind: kind.sanitize(tabs_len),
                focused: Some(p.id) == focus,
            })
        })
        .collect()
}

/// 「自动拆分」往哪个方向切：这一格宽就左右拆、高就上下拆，拆完两块都更接近方形；
/// 量不到尺寸（刚启动、还没排版）就按左右拆。
fn auto_axis(rect: Option<egui::Rect>) -> Axis {
    match rect {
        Some(r) if r.height() > r.width() => Axis::Vertical,
        _ => Axis::Horizontal,
    }
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
    /// 界面字号：非编辑区所有文字跟着它走（见 `theme` 模块头）
    ui_font: f32,
    /// 「外观」分区里滑块正在调的两个字号。拖动只改这里，点「应用」才写回上面那两个。
    /// 即时生效的话，每来一次拖动事件就要重铺字体图集、整页重排——连滑块自己都会被挪走，
    /// 界面就在指针底下闪。设置窗口关掉时这份暂存会被丢掉。
    pending_font: f32,
    pending_ui_font: f32,
    dark: bool,
    /// 主窗口的原生句柄，用来把系统标题栏刷成当前主题；无头测试里是 `None`
    hwnd: Option<isize>,
    /// 编辑器自动换行：开着长行折进面板，关着长行横向滚出去
    word_wrap: bool,
    /// 顶栏内容上一帧的高度（取整）。面板高度是照上一帧量的，换行后旧高度会把第二排裁掉
    toolbar_h: i32,
    status: String,
    /// 「配置文件读不出来」那个弹窗被用户点掉过没有。没点掉就一直摆在正中。
    config_trouble_seen: bool,
    quick_open: Option<QuickOpen>,
    pending_close: Option<usize>,
    /// 设置窗口是否打开
    show_settings: bool,
    /// 设置窗口当前显示哪个分区
    settings_section: SettingsSection,
    /// 设置窗口这次出现时要不要摆回屏幕正中
    center_settings: bool,
    want_open_folder: bool,
    /// 「打开文件…」的延迟标记。原生对话框会阻塞当前线程，不能在布局跑到一半时弹，
    /// 所以点按钮和按快捷键都只置位，等这一帧画完再开。
    want_open_files: bool,
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
    /// 中央区域的面板树（tmux 式：一块块面板递归切分）。只装编辑面板——
    /// shell 全在终端页签的树里，见 `term_tabs`
    panes: PaneTree<PaneKind>,
    /// 终端页签：一格一组 shell（组内还能再切分），和文件页签排在同一条页签栏上
    term_tabs: Vec<TermTab>,
    /// 已经用过的终端页签序号。页签会从中间被关掉，所以序号只增不复用
    term_seq: u64,
    /// 中央区当前显示哪一组终端；`None` = 显示编辑区。存的是页签序号而不是下标：
    /// 关掉一个页签不会让选中状态挪到隔壁那组去
    term_active: Option<u64>,
    /// 数据库连接：侧栏「数据库」页签 + 连接管理窗口，SQL 块也从这里取连接
    database: DatabasePanel,
    /// 底部结果区：一条语句一个面板，同时展示这次执行拿到的所有结果
    results: ResultsDock,
    /// 标了 `auto` 的只读块：块 id → 已经自动跑过的那份正文。正文没改就不重复敲门。
    auto_ran: std::collections::HashMap<String, String>,
    /// 自动执行的到点时间。用户还在改就一帧帧往后推，手停下来才真跑。
    auto_due: Option<std::time::Instant>,
    /// 上一帧看到的那批「待自动执行」的指纹，变了说明内容还在动
    auto_sig: String,
    /// 上一帧算出的主窗口面板矩形，方位键找邻居、移动面板定落点都要用
    pane_layout: TreeLayout,
    /// 调试脚本的计时起点
    boot: std::time::Instant,
    /// 调试脚本走到第几步
    verify_stage: usize,
}

/// 主窗口的原生句柄（`HWND`）。非 Windows 和 eframe 拿不到句柄时返回 `None`。
#[cfg(windows)]
fn native_hwnd(cc: &eframe::CreationContext<'_>) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
    match cc.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    }
}

#[cfg(not(windows))]
fn native_hwnd(_cc: &eframe::CreationContext<'_>) -> Option<isize> {
    None
}

/// 手停下来多久才允许自动执行。太短了边打字边打库，太长了像在等人。
const AUTO_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(700);

/// 一段 SQL 交给哪条连接执行、结果面板挂回哪儿。
///
/// 人手 Ctrl+Enter 和自动执行都拼这一个走同一个出口：两条路各写一遍提交，
/// 自动那条迟早会少做一步收尾（比如忘了记块头，跑完状态就不回写了）。
struct Submission {
    path: PathBuf,
    conn: String,
    title: String,
    sql: String,
    /// `sql` 在文件里从哪个字节起跳，报错的语句要标回原文
    base: usize,
    /// 结果面板「回块」跳到哪个字节
    jump: usize,
    /// 整块执行才记块头，跑完把头部回写成 `[已执行]` / `[失败]`
    block_head: Option<usize>,
    /// 状态栏后缀，说清楚这几条语句出自哪一块
    where_: String,
}

/// 区间开头那段空白有多长。交出去的是 trim 过的正文，下标得对齐回文件里。
fn lead(text: &str, a: usize, b: usize) -> usize {
    let raw = &text[a..b];
    a + (raw.len() - raw.trim_start().len())
}

impl EasyEditorApp {
    pub fn new(cc: &eframe::CreationContext<'_>, target: Option<PathBuf>) -> Self {
        let mut app = Self::bootstrap(&cc.egui_ctx, target);
        app.hwnd = native_hwnd(cc);
        titlebar::apply(app.hwnd, app.dark);
        app.register_txt_association();
        app
    }

    /// 把自己登记进 `.txt` 的「打开方式」。放在 `new` 而不是 `bootstrap`：
    /// 后者每个无头测试都会走一遍，那会让 `cargo test` 顺手把测试二进制
    /// 注册成这台机器的文本编辑器。
    fn register_txt_association(&mut self) {
        if !self.config.data.txt_association {
            return;
        }
        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(why) => {
                self.status = format!("没取到自身路径，跳过文件关联：{why}");
                return;
            }
        };
        match crate::assoc::register(&exe) {
            // 只在真建出新键的那一次报一句：这一下每回启动都会跑，从第二次起注册表里
            // 那几个值原样躺着，天天在状态栏喊一遍就成噪音了。状态栏还空着才报，
            // 别把更要紧的提示盖掉。
            Ok(written) => {
                if written.fresh > 0 && self.status.is_empty() {
                    self.status =
                        "已登记为 .txt 的可选程序：右键「打开方式」，或在系统「默认应用」里选 EasyEditor"
                            .to_string();
                }
            }
            Err(why) => self.status = format!(".txt 关联登记失败：{why}"),
        }
    }

    /// 不碰 `eframe` 上下文的构造入口：测试里没有真实的窗口和 GL 上下文，
    /// 但照样要把配置、字体、面板树这套起手式跑一遍。
    fn bootstrap(ctx: &egui::Context, target: Option<PathBuf>) -> Self {
        let config = ConfigStore::load();
        let font_size = config.data.font_size;
        let ui_font = config.data.ui_font_size;
        let dark = config.data.dark;
        let show_side = config.data.show_side;
        let word_wrap = config.data.word_wrap;
        let side = SideTab::from_key(&config.data.side_tab);
        // 后面 `config` 整个挪进 Self，所以要读的字段先在这儿取出来
        let db_connections = config.data.db_connections.clone();

        theme::install_fonts(ctx);
        theme::apply_style(ctx, dark, ui_font);

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
                TerminalSettings::from_config(
                    &config.data.terminal_programs,
                    &config.data.terminal_args,
                    &config.data.terminal_custom_shells,
                ),
            ),
            search: SearchState::default(),
            find: FindState::default(),
            hl: Highlighter::new(dark),
            font_size,
            ui_font,
            pending_font: font_size,
            pending_ui_font: ui_font,
            dark,
            hwnd: None,
            word_wrap,
            toolbar_h: 0,
            status: "Ctrl+O 打开文件夹 · Ctrl+P 快速打开 · Ctrl+` 终端".into(),
            config_trouble_seen: false,
            quick_open: None,
            pending_close: None,
            show_settings: false,
            settings_section: SettingsSection::Appearance,
            center_settings: false,
            want_open_folder: false,
            want_open_files: false,
            focus_search: false,
            frame_count: 0,
            journal_title: String::new(),
            config,
            nav: CaretHistory::new(),
            keymap,
            recording: None,
            panes: PaneTree::new(),
            term_tabs: Vec::new(),
            term_seq: 0,
            term_active: None,
            database: DatabasePanel::new(db_connections),
            results: ResultsDock::new(),
            auto_ran: std::collections::HashMap::new(),
            auto_due: None,
            auto_sig: String::new(),
            pane_layout: TreeLayout::default(),
            boot: std::time::Instant::now(),
            verify_stage: 0,
        };
        // 中央区域始终至少有一块面板；空工作区时它显示欢迎页
        app.panes.set_root(PaneKind::Editor(None));
        // 配置里指定的终端偏好：本机没有这个种类就保持自动选择
        app.terminal.apply_pref(&app.config.data.terminal_shell);
        // 配置文件读坏了要立刻留个话：这一份启动是按默认值跑的，
        // 而且原文没保住之前落盘是停着的，不说明白用户只会以为「配置又自己丢了」
        if let Some(trouble) = &app.config.trouble {
            app.status = if trouble.blocks_save() {
                "配置文件读不出来，也没敢往回写 —— 看弹窗说明".to_string()
            } else {
                "配置文件读不出来，已按默认值启动，原文留档了 —— 看弹窗说明".to_string()
            };
        }

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
                // `.txt` 双击带进来的就是这种。只把这一个文件开在标签里，不拿它的父目录
                // 当工作区：那等于每双击一个 Downloads 里的笔记就把整个 Downloads 建成索引，
                // 还会顺手挡掉用户原本想恢复的工作目录。想看这一片就在侧栏点「打开文件夹」。
                app.open_file(path.clone(), None);
            } else {
                app.status = format!("{} 不存在，什么都没打开", path.display());
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
        if self.config.data.ui_font_size != self.ui_font {
            self.config.data.ui_font_size = self.ui_font;
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
        // 终端：默认种类 + 路径/参数覆盖 + 自己加的 shell。覆盖项先去掉空串再比，配置文件才不会长毛。
        let pref = self.terminal.shell.id();
        if self.config.data.terminal_shell != pref {
            self.config.data.terminal_shell = pref;
            changed = true;
        }
        let (programs, args, customs) = self.terminal.settings.to_config();
        if self.config.data.terminal_programs != programs {
            self.config.data.terminal_programs = programs;
            changed = true;
        }
        if self.config.data.terminal_args != args {
            self.config.data.terminal_args = args;
            changed = true;
        }
        if self.config.data.terminal_custom_shells != customs {
            self.config.data.terminal_custom_shells = customs;
            changed = true;
        }
        // 连接列表整份比：条目不多，而「少存了一条连接」比多写几字节贵得多
        if self.config.data.db_connections != self.database.connections() {
            self.config.data.db_connections = self.database.connections().to_vec();
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
                self.new_terminal();
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
            Some("database") => {
                self.side = SideTab::Database;
                self.show_side = true;
                self.database.open_manager();
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
        self.panes.foreach_payload_mut(|_, kind| {
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

    /// 选一个或几个文件打开，位置不限。侧栏那个文件夹只是一个入口，不是边界：
    /// 这里不动工作区（树、索引、终端 cwd 全保持原样），也不清已有标签，
    /// 选中的文件只进标签条。
    fn open_files_dialog(&mut self) {
        let mut dialog = rfd::FileDialog::new().set_title("打开文件");
        // 起点用当前工作区；没有工作区时不指定，让系统回落到它记得的位置。
        if let Some(root) = &self.root {
            dialog = dialog.set_directory(root);
        }
        let Some(paths) = dialog.pick_files() else {
            return;
        };
        let before = self.tabs.len();
        for path in paths {
            self.open_file(path, None);
        }
        // 单个文件留用 open_file 自己那句「已打开 …」；一批里 open_file 会逐个覆写状态，
        // 收尾给个总数，免得只剩最后那一条路径。
        if self.tabs.len() - before > 1 {
            self.status = format!("已打开 {} 个文件", self.tabs.len() - before);
        }
    }

    /// 从资源管理器拖进来的东西：文件照旧只进标签、不动工作区，文件夹才换工作区。
    /// 两类一起拖时先认文件夹再开文件——`open_workspace` 会清标签，反过来排会把
    /// 同一批里拖进来的文件清掉。多个文件夹只认第一个，一次拖进来本来就是同一件事。
    fn open_dropped(&mut self, paths: Vec<PathBuf>) {
        let dirs: Vec<PathBuf> = paths.iter().filter(|p| p.is_dir()).cloned().collect();
        let files: Vec<PathBuf> = paths.into_iter().filter(|p| !p.is_dir()).collect();
        if let Some(dir) = dirs.first() {
            self.open_workspace(dir.clone());
        }
        let before = self.tabs.len();
        for file in &files {
            self.open_file(file.clone(), None);
        }
        let opened = self.tabs.len() - before;
        // 只拖文件、或只拖文件夹时 open_file / open_workspace 已经各自写过状态；
        // 混着拖就得收尾说一句，不然屏幕上只剩其中一半的结果。
        if !dirs.is_empty() && opened > 0 {
            self.status = format!("已打开 {}，另开 {opened} 个文件", dirs[0].display());
        } else if opened > 1 {
            self.status = format!("已打开 {opened} 个文件");
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

    /// 文件树里改了名：已打开的标签跟着换路径，未保存的改动仍会落到新名字上。
    fn file_renamed(&mut self, old: &Path, new: &Path) {
        for tab in &mut self.tabs {
            if tab.path == old {
                tab.path = new.to_path_buf();
            }
        }
        if let Some(root) = &self.root {
            self.index = filetree::collect_files(root, 50_000);
        }
        self.status = format!(
            "已重命名 {} → {}",
            old.file_name().unwrap_or_default().to_string_lossy(),
            new.file_name().unwrap_or_default().to_string_lossy()
        );
    }

    fn save_active(&mut self) {
        let Some(idx) = self.focused_tab() else {
            return;
        };
        let Some(file) = self.tabs.get_mut(idx) else {
            return;
        };
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
        self.panes.foreach_payload_mut(|_, kind| {
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
        let Some(idx) = self.focused_tab() else {
            return;
        };
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
        let Some(idx) = self.focused_tab() else {
            return;
        };
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

    // ---------- SQL 块与结果 ----------

    fn insert_sql_block(&mut self) {
        let Some(idx) = self.focused_tab() else {
            self.status = "SQL 块写在文本文件里，请先打开一个文件".into();
            return;
        };
        let file = &mut self.tabs[idx];
        if file.large {
            self.status = "大文件为只读预览，无法插入 SQL 块".into();
            return;
        }
        // 头部写死当前选中的连接：块放在文件里发给别人时，收件人看得出这段 SQL 跑在哪
        let conn = self.database.active_conn().unwrap_or_default().to_string();
        let n = sql::scan(&file.text).len() + 1;
        let title = format!("查询{n}");
        let (new_text, caret) = sql::insert_block(&file.text, file.caret_byte, &title, &conn);
        let total = new_text.len();
        file.pending_edit = Some(PendingEdit {
            range: Some((0, total.min(file.text.len()))),
            insert: new_text,
            caret: Some(caret),
        });
        self.status = if conn.is_empty() {
            format!("已插入「{title}」，写好 SQL 就能执行（还没配连接）")
        } else {
            format!("已插入「{title}」，连到「{conn}」，写好 SQL 就能执行")
        };
    }

    /// 新建一个查询文件：`query-年月日-时分秒.sql`，首行是绑好当前连接的 SQL 块。
    ///
    /// 时间戳命名 + 撞名加序号，所以连点两次得到两个文件，谁也不覆盖谁。
    /// 文件一落盘就按 `.sql` 打开，`sql_mode` 自动为真，Ctrl+Enter 直接跑整份。
    fn new_query(&mut self, dir: Option<PathBuf>) {
        let Some(dir) = self.query_dir(dir) else {
            self.status = "没地方放这个查询文件：先打开一个文件夹".into();
            return;
        };
        let conn = self.database.active_conn().unwrap_or_default().to_string();
        let head = sql::make_head("查询", &conn, sql::STATUS_PENDING);
        let text = format!("{head}\n\n");
        let mut created: Option<PathBuf> = None;
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
        for n in 1..1000u32 {
            let name = if n == 1 {
                format!("query-{stamp}.sql")
            } else {
                format!("query-{stamp}-{n}.sql")
            };
            let path = dir.join(name);
            // create_new 而不是 exists 判断：两步之间别人也可能建出同名文件
            if std::fs::File::create_new(&path).is_ok() {
                if let Err(e) = std::fs::write(&path, text.as_bytes()) {
                    self.status = format!("写入 {} 失败：{e}", path.display());
                    return;
                }
                created = Some(path);
                break;
            }
        }
        let Some(path) = created else {
            self.status = format!("「{}」里同一秒新建的查询文件超过 999 个", dir.display());
            return;
        };
        if Some(&dir) == self.root.as_ref() {
            self.index = filetree::collect_files(&dir, 50_000);
        }
        self.open_file(path.clone(), None);
        if let Some(idx) = self.tabs.iter().position(|t| t.path == path) {
            // 光标落在块头下面那一行，进来就能写 SQL
            self.tabs[idx].pending_jump = Some((head.len() + 1).min(text.len()));
        }
        self.status = if conn.is_empty() {
            format!(
                "已新建 {}（还没配连接）",
                path.file_name().unwrap_or_default().to_string_lossy()
            )
        } else {
            format!(
                "已新建 {}，连到「{conn}」",
                path.file_name().unwrap_or_default().to_string_lossy()
            )
        };
    }

    /// 查询文件的落点：文件树右键给的目录 > 工作区根 > 当前文件所在目录 > 启动目录。
    fn query_dir(&self, explicit: Option<PathBuf>) -> Option<PathBuf> {
        explicit
            .or_else(|| self.root.clone())
            .or_else(|| {
                self.active
                    .and_then(|i| self.tabs.get(i))
                    .and_then(|f| f.path.parent().map(Path::to_path_buf))
            })
            .or_else(|| std::env::current_dir().ok())
    }

    /// 「数据库」下拉顶上那一行：默认连接是谁、连不连得上。
    ///
    /// 这块信息以前只存在于侧栏某一行的底色里，而块头没写 `conn=` 时
    /// SQL 恰好就发到这里 —— 最该看见的时候最看不见。
    fn connection_row(&self, ui: &mut Ui) {
        let Some(name) = self.database.active_conn().map(|s| s.to_string()) else {
            let warn = ui.visuals().warn_fg_color;
            ui.label(RichText::new("还没有数据库连接").size(crate::theme::sm(ui)).color(warn));
            return;
        };
        // 说「生效的那份」配置：树上换过当前库，这里就得跟着换，不然它指旧库。
        let eff = self.database.effective_spec(&name);
        let target = eff.as_ref().map(|s| s.target()).unwrap_or_default();
        let accent = ui.visuals().hyperlink_color;
        ui.horizontal(|ui| {
            ui.label(RichText::new("默认连接").size(crate::theme::sm(ui)).weak());
            ui.label(RichText::new(&name).size(crate::theme::sm(ui)).strong().color(accent));
            let (mark, state, _) = self.database.link_mark(ui, &name);
            ui.label(RichText::new(mark).size(crate::theme::sm(ui)).strong().color(accent))
                .on_hover_text(state);
            ui.label(RichText::new(target).size(crate::theme::xs(ui)).weak());
        });
    }

    /// 执行 SQL：`Ctrl+Enter`——选中的那段优先，其次只跑光标所在的那一句。
    fn sql_run(&mut self) {
        self.run_sql(false);
    }

    /// 执行 SQL：`Ctrl+Shift+Enter`——整块交出去（SQL 模式下没有块就是整份文件）。
    ///
    /// 分两条键是有原因的：块状态的 `[待执行]` → `[已执行]` 回写说的得是整块，
    /// 光标那一句跑完就宣布整块执行完，比不回写更坏。
    fn sql_run_block(&mut self) {
        self.run_sql(true);
    }

    fn run_sql(&mut self, whole: bool) {
        let Some(idx) = self.focused_tab() else {
            self.status = "先打开一个文件，里面写个 SQL 块".into();
            return;
        };
        // 整份文本拷出来：下面要拿 self.database 发消息，不能同时借着 tabs
        let text = self.tabs[idx].text.clone();
        let path = self.tabs[idx].path.clone();
        let line = self.tabs[idx].line.saturating_sub(1);
        let caret = self.tabs[idx].caret_byte;
        let blocks = sql::scan(&text);
        let in_block = sql::block_at(&blocks, line);
        let selection = self.tabs[idx].selection_range;
        // 查询文件本来就是一整篇 SQL：没有选区也没有块时，范围就是整份文件
        let whole_file = selection.is_none() && in_block.is_none() && self.tabs[idx].sql_mode;
        let body = match (selection, in_block) {
            (Some(range), block) => (
                range,
                "选中内容".to_string(),
                block.map(|b| b.head_start).unwrap_or(range.0),
            ),
            (None, Some(block)) => {
                let mut title = block.title.clone();
                if title.is_empty() {
                    title = "SQL".into();
                }
                ((block.body_start, block.body_end), title, block.head_start)
            }
            (None, None) if whole_file => (
                (0, text.len()),
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("查询")
                    .to_string(),
                0,
            ),
            (None, None) => {
                self.status =
                    "光标这一句没有 SQL，也没有选区。Ctrl+Shift+J 插一个块，或开本文件 SQL 模式。"
                        .into();
                return;
            }
        };
        // 只有整块交出去才回写状态；跑光标那一句不代表这块跑完
        let mut block_head = match (selection, in_block) {
            (None, Some(b)) if whole => Some(b.head_start),
            _ => None,
        };
        let mut where_ = if whole_file {
            "（整个文件）".to_string()
        } else if in_block.is_some() {
            format!("（{}）", sql::describe(&blocks, line))
        } else {
            String::new()
        };
        // 范围和这份文本快照是同一帧取的，理论上不会越界；真越界也只夹不炸
        let (a, b) = body.0;
        let mut from = a.min(text.len());
        let mut to = b.min(text.len()).max(from);
        // 没选区又不是显式整块执行：按分号切，只留光标那一行关联的那一句
        if !whole && selection.is_none() {
            let rel = caret.saturating_sub(from).min(to - from);
            let ranges = sql::split_ranges(&text[from..to]);
            match sql::statement_index_at(&text[from..to], &ranges, rel) {
                Some(index) => {
                    let (x, y) = ranges[index];
                    from += x;
                    to = from + (y - x);
                    block_head = None;
                    where_ = format!("（第 {}/{} 句）", index + 1, ranges.len());
                }
                // 这一句都挑不出来，整块更不该偷偷跑掉
                None => {
                    self.status = "这一行没有可执行的 SQL 语句。".into();
                    return;
                }
            }
        }
        let conn = match in_block.map(|b| b.conn.clone()).filter(|c| !c.is_empty()) {
            Some(conn) => conn,
            None => self.database.active_conn().unwrap_or_default().to_string(),
        };
        let sub = Submission {
            path,
            conn,
            title: body.1,
            base: lead(&text, from, to),
            sql: text[from..to].trim().to_string(),
            jump: body.2,
            block_head,
            where_,
        };
        self.status = match self.submit(&sub) {
            Ok(n) => format!("已提交 {n} 条语句到「{}」{}", sub.conn, sub.where_),
            Err(why) => why,
        };
    }

    /// 把一段 SQL 交出去执行。Ok 给的是交出去的语句条数，Err 直接拿去当状态栏。
    fn submit(&mut self, sub: &Submission) -> Result<usize, String> {
        if sub.sql.is_empty() {
            return Err("这里没有可执行的 SQL".into());
        }
        if sub.conn.is_empty() {
            return Err("还没有数据库连接，先在顶栏「数据库 → 连接管理」里加一个".into());
        }
        match self.database.run(&sub.conn, &sub.sql) {
            Ok(parts) if parts.is_empty() => Err("只有注释，没有语句可跑".into()),
            Ok(parts) => {
                // 拆句和后台用的是同一把尺子，第 i 个区间必然对得上第 i 条语句
                let mut spans = sql::split_ranges(&sub.sql).into_iter();
                let owned: Vec<results::Part> = parts
                    .into_iter()
                    .map(|(task, stmt)| results::Part {
                        task,
                        stmt,
                        range: spans.next().map(|(a, b)| (sub.base + a, sub.base + b)),
                    })
                    .collect();
                let n = owned.len();
                // 这一段重新跑了，旧的红色先抹掉：跑对了它不该还留在屏上
                if let Some(idx) = self.tabs.iter().position(|t| t.path == sub.path) {
                    let file = &mut self.tabs[idx];
                    let (a, b) = (sub.base, sub.base + sub.sql.len());
                    file.sql_errors = Arc::new(
                        file.sql_errors
                            .iter()
                            .filter(|&&(x, y)| y <= a || x >= b)
                            .copied()
                            .collect(),
                    );
                }
                self.results.start_run(
                    &sub.conn,
                    &sub.title,
                    &owned,
                    Some(results::Origin {
                        path: sub.path.clone(),
                        jump: sub.jump,
                        block_head: sub.block_head,
                    }),
                );
                Ok(n)
            }
            Err(err) => Err(err),
        }
    }

    /// 手停下来一会儿，把标了 `auto` 的只读块自己跑掉——jupyter 那种「改完就有结果」。
    ///
    /// 三道闸：`sql::auto_runnable`（写了 auto 且条条只读）、同一份正文只跑一次、
    /// 结果区还在等回执就不开新的一批。写操作永远只能人手按。
    ///
    /// 那道闸按连接看，不看全局：一条连接只有一个工作线程，往排队的那条再塞是白等；
    /// 另一条连接明明空着，凭什么跟着一起停。
    ///
    /// `now` 由调用方给，测试才能不用真睡 700 毫秒。
    fn auto_tick(&mut self, now: std::time::Instant) {
        let Some(idx) = self.focused_tab() else {
            return;
        };
        let text = self.tabs[idx].text.clone();
        let path = self.tabs[idx].path.clone();
        let default_conn = self.database.active_conn().unwrap_or_default().to_string();
        let pending: Vec<sql::Block> = sql::scan(&text)
            .into_iter()
            .filter(|b| sql::auto_runnable(b, &text))
            .filter(|b| self.auto_ran.get(&b.id()).map(String::as_str) != Some(b.sql(&text)))
            .filter(|b| {
                let conn = if b.conn.is_empty() {
                    default_conn.as_str()
                } else {
                    b.conn.as_str()
                };
                !self.results.busy_on(conn)
            })
            .collect();
        if pending.is_empty() {
            return;
        }
        // 每改一个字这批的指纹就变，到点时间跟着往后推。手没停就一直在推，停下才跑得掉。
        let sig = pending
            .iter()
            .map(|b| format!("{}|{}", b.id(), b.sql(&text)))
            .collect::<Vec<_>>()
            .join("\u{1}");
        if sig != self.auto_sig {
            self.auto_sig = sig;
            self.auto_due = Some(now + AUTO_DEBOUNCE);
            return;
        }
        let Some(due) = self.auto_due else {
            return;
        };
        if now < due {
            return;
        }
        self.auto_due = None;
        self.auto_sig.clear();
        for block in pending {
            let body = block.sql(&text).to_string();
            // 先记账再提交：连不上库的时候不该每帧重敲一次门，改了正文才会再试
            self.auto_ran.insert(block.id(), body.clone());
            let conn = if block.conn.is_empty() {
                self.database.active_conn().unwrap_or_default().to_string()
            } else {
                block.conn.clone()
            };
            let mut title = block.title.clone();
            if title.is_empty() {
                title = "SQL".into();
            }
            let sub = Submission {
                jump: block.head_start,
                block_head: Some(block.head_start),
                where_: String::new(),
                base: lead(&text, block.body_start, block.body_end),
                path: path.clone(),
                conn,
                title,
                sql: body,
            };
            self.status = match self.submit(&sub) {
                Ok(n) => format!("自动执行 {n} 条语句 →「{}」", sub.conn),
                Err(why) => why,
            };
        }
    }

    /// 跑失败的语句在原文里铺一层红底——改 SQL 的人第一眼要看到的是哪一句出的问题。
    ///
    /// 区间是提交那一帧量下的，中间文本改过就对不上号。所以只认原文正好等于这条
    /// 语句的那一处，对不上宁可丢掉：少标一片红没人受害，标错地方会误导人改别句。
    fn paint_failures(&mut self) {
        for (path, a, b, stmt) in self.results.take_failures() {
            let Some(idx) = self.tabs.iter().position(|t| t.path == path) else {
                continue;
            };
            let file = &mut self.tabs[idx];
            if file.text.get(a..b) != Some(stmt.as_str()) {
                continue;
            }
            let dup = file.sql_errors.iter().any(|&(x, y)| (x, y) == (a, b));
            if !dup {
                Arc::make_mut(&mut file.sql_errors).push((a, b));
            }
        }
    }

    /// 跑完的一批把块头部从 `[待执行]` 改成 `[已执行]` / `[失败]`。
    ///
    /// 只认提交那一帧记下的字节位置：文件在这期间被改过就放弃回写，宁可头部不动，
    /// 不能把状态塞进别的行里。
    fn mark_block(&mut self, done: &Settled) {
        let Some(idx) = self.tabs.iter().position(|t| t.path == done.path) else {
            return;
        };
        let found = sql::scan(&self.tabs[idx].text)
            .into_iter()
            .find(|b| b.head_start == done.head);
        let Some(block) = found else {
            self.status = "文件改过了，SQL 块状态没回写".into();
            return;
        };
        let next = if done.ok {
            sql::STATUS_DONE
        } else {
            sql::STATUS_FAILED
        };
        if block.status == next {
            return;
        }
        let (a, b, head) = sql::status_edit(&block, next);
        self.tabs[idx].pending_edit = Some(PendingEdit {
            range: Some((a, b)),
            insert: head,
            caret: None,
        });
    }

    /// 结果面板的「回块」：把文件摊到前台，再让光标跳到块头。
    fn jump_to_byte(&mut self, path: &Path, byte: usize) {
        self.open_file(path.to_path_buf(), None);
        let Some(idx) = self.tabs.iter().position(|t| t.path == path) else {
            self.status = "那个文件已经不在标签里了".into();
            return;
        };
        self.tabs[idx].pending_jump = Some(byte);
    }

    /// 结果区「导出」按下来的那份 CSV，走系统存盘框落盘。
    ///
    /// 文件开头补一个 UTF-8 BOM：Windows 上的 Excel 只认 BOM 才按 UTF-8 读，
    /// 少了它中文列直接变成一堆乱码，导出的意义就没了。
    fn save_csv(&mut self, export: results::Export) {
        let Some(path) = rfd::FileDialog::new()
            .set_title("导出结果")
            .set_file_name(&export.name)
            .add_filter("CSV", &["csv"])
            .save_file()
        else {
            self.status = "导出取消".into();
            return;
        };
        let mut bytes = vec![0xef, 0xbb, 0xbf];
        bytes.extend_from_slice(export.csv.as_bytes());
        self.status = match std::fs::write(&path, bytes) {
            Ok(()) => format!("已导出「{}」到 {}", export.name, path.display()),
            Err(err) => format!("导出失败：{err}"),
        };
    }

    fn results_panel(&mut self, ui: &mut Ui) {
        let keys = self.keymap.to_config();
        let hint = keys
            .get("sql_run")
            .cloned()
            .unwrap_or_else(|| "Ctrl+Enter".into());
        egui::Panel::bottom("db_results")
            // 高度得由面板自己定死一个起点，不能靠内容量：内容超过面板预留的那格会被
            // 裁掉，egui 连 shape 都不留。给足两三个面板的空间，多余的走滚动，
            // 上边缘照常可以拖着调。
            .resizable(true)
            .default_size(320.0)
            .size_range(120.0..=600.0)
            .frame(
                egui::Frame::new()
                    .fill(ui.ctx().global_style().visuals.faint_bg_color)
                    .inner_margin(egui::Margin::symmetric(8, 5)),
            )
            .show(ui, |ui| self.results.show(ui, &hint));
    }

    // ---------- 查找替换 ----------

    fn sync_find_to_file(&mut self) {
        let Some(idx) = self.focused_tab() else {
            return;
        };
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
        let Some(idx) = self.focused_tab() else {
            return;
        };
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
        let Some(idx) = self.focused_tab() else {
            return;
        };
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
        let Some(idx) = self.focused_tab() else {
            return;
        };
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
        if self.keymap.consume(ctx, "open_file") {
            self.want_open_files = true;
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
        if self.keymap.consume(ctx, "sql_new_query") {
            self.new_query(None);
        }
        if self.keymap.consume(ctx, "sql_insert") {
            self.insert_sql_block();
        }
        if self.keymap.consume(ctx, "sql_run") {
            self.sql_run();
        }
        if self.keymap.consume(ctx, "sql_run_block") {
            self.sql_run_block();
        }
        if self.keymap.consume(ctx, "results_toggle") {
            self.results.open = !self.results.open;
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
            self.new_terminal();
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
            self.toggle_settings(SettingsSection::Keymap);
        }
        if self.keymap.consume(ctx, "screenshot") {
            self.take_screenshot(ctx);
        }
        // 面板级快捷键统一走 pane_shortcuts，打在「当前在前面的那棵树」上：
        // 编辑区在前就是编辑树，终端页签在前就是那一组的 shell 树
        let scope = self.front_scope();
        self.pane_shortcuts(ctx, scope);
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

    /// 焦点所在编辑器面板的标签。shell 都在终端页签的树里，编辑树的焦点永远是编辑格。
    fn focused_tab(&self) -> Option<usize> {
        self.panes
            .focus()
            .and_then(|p| self.panes.payload_of(p).copied())
            .and_then(PaneKind::tab)
    }

    /// 标签和面板标题上那份名字。同名文件会带上能区分开的父目录段，见 [`filetree::unique_titles`]。
    fn tab_title(&self, idx: usize) -> String {
        let paths: Vec<PathBuf> = self.tabs.iter().map(|t| t.path.clone()).collect();
        filetree::unique_titles(&paths)
            .into_iter()
            .nth(idx)
            .unwrap_or_default()
    }

    /// 本次要写入标签的面板：主窗口那棵树的焦点面板。
    fn editor_pane_for_write(&self) -> Option<PaneId> {
        let focused = self.panes.focus()?;
        self.panes
            .payload_of(focused)
            .copied()
            .filter(|k| k.is_editor())
            .map(|_| focused)
    }

    /// 把标签交给「当前该显示它的地方」：焦点面板换成这个标签，并且翻回编辑区——
    /// 从文件树里点开一个文件，不该还对着终端那一格。
    fn show_in_focused_pane(&mut self, idx: usize) {
        self.term_active = None;
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
        self.sync_active();
    }

    /// 设置当前焦点面板的激活标签。
    fn set_focused_tab(&mut self, idx: Option<usize>) {
        let Some(p) = self.editor_pane_for_write() else {
            return;
        };
        if let Some(PaneKind::Editor(slot)) = self.panes.payload_of_mut(p) {
            *slot = idx;
        }
        self.sync_active();
    }

    /// 把 `self.active` 这个镜像刷成焦点面板的真实内容。
    ///
    /// `active` 被状态栏、侧栏、工具栏等三十来处读，留着它比逐个改调用点稳。
    fn sync_active(&mut self) {
        self.active = self.focused_tab();
    }

    /// 在焦点面板旁边切出一块新面板（主窗口那棵树）。
    fn split_with(&mut self, axis: Axis, payload: PaneKind) -> Option<PaneId> {
        let target = self.panes.focus()?;
        let id = self.panes.split(target, axis, payload)?;
        self.status = "已拆分面板".into();
        self.sync_active();
        Some(id)
    }

    /// 面板命令落到作用域对应的那棵树上：编辑树，或某个终端页签的 shell 树。
    ///
    /// 返回值里带一句要报给用户的话（起不了 shell、关掉了一格），没有就不改状态栏。
    fn pane_cmd(&mut self, scope: PaneScope, cmd: PaneCmd) {
        let status = match scope {
            PaneScope::Main => self.main_pane_cmd(cmd),
            PaneScope::Term(tab) => match self.term_tabs.iter_mut().find(|t| t.id == tab) {
                Some(t) => t.command(cmd, &mut self.terminal),
                None => None,
            },
        };
        if matches!(scope, PaneScope::Term(_)) {
            // 格子里的 × 关到最后一格，这个页签就没用了：摘掉它，中央区该显示什么
            // 也由这里定（还留着别的页签就翻到最后那个，一组都没有就回编辑区）
            self.prune_term_tabs();
        }
        if let Some(text) = status {
            self.status = text;
        }
    }

    /// 主窗口那棵树的命令。这一棵树只有编辑面板，所以格子里永远是「某个标签」
    /// 或「空白」，不会出现起进程这种事。
    fn main_pane_cmd(&mut self, cmd: PaneCmd) -> Option<String> {
        match cmd {
            // 编辑面板再切一格 = 同一份文件再看一处（对比 / 边看边写）
            PaneCmd::Split(id, axis) => {
                let payload = self.panes.payload_of(id).copied()?;
                self.panes.set_focus(id);
                self.split_with(axis, payload);
                None
            }
            PaneCmd::Move(id, dir) => {
                self.move_pane_to(id, dir);
                None
            }
            PaneCmd::Close(id) => {
                self.close_pane(id);
                None
            }
            PaneCmd::Focus(id) => {
                if self.panes.set_focus(id) {
                    self.sync_active();
                }
                None
            }
            PaneCmd::Cycle(delta) => {
                self.panes.focus_cycle(delta);
                self.sync_active();
                None
            }
            PaneCmd::DirFocus(dir) => {
                self.panes.focus_dir(&self.pane_layout, dir);
                self.sync_active();
                None
            }
        }
    }

    /// 面板级快捷键（拆分 / 关闭 / 切焦点）。一次只打在一棵树上：谁在中央区前面
    /// 就归谁（见 [`EasyEditorApp::front_scope`]），所以编辑区和终端页签不会互相
    /// 抢同一串按键。
    fn pane_shortcuts(&mut self, ctx: &egui::Context, scope: PaneScope) {
        let focused = match scope {
            PaneScope::Main => self.panes.focus(),
            PaneScope::Term(id) => self
                .term_tabs
                .iter()
                .find(|t| t.id == id)
                .and_then(|t| t.tree.focus()),
        };
        if self.keymap.consume(ctx, "split_col") {
            if let Some(id) = focused {
                self.pane_cmd(scope, PaneCmd::Split(id, Axis::Horizontal));
            }
        }
        if self.keymap.consume(ctx, "split_row") {
            if let Some(id) = focused {
                self.pane_cmd(scope, PaneCmd::Split(id, Axis::Vertical));
            }
        }
        if self.keymap.consume(ctx, "split_auto") {
            if let Some(id) = focused {
                let rect = match scope {
                    PaneScope::Main => self.pane_layout.pane_rect(id),
                    PaneScope::Term(tab) => self
                        .term_tabs
                        .iter()
                        .find(|t| t.id == tab)
                        .and_then(|t| t.layout.pane_rect(id)),
                };
                self.pane_cmd(scope, PaneCmd::Split(id, auto_axis(rect)));
            }
        }
        if self.keymap.consume(ctx, "pane_close") {
            if let Some(id) = focused {
                self.pane_cmd(scope, PaneCmd::Close(id));
            }
        }
        if self.keymap.consume(ctx, "pane_next") {
            self.pane_cmd(scope, PaneCmd::Cycle(1));
        }
        if self.keymap.consume(ctx, "pane_prev") {
            self.pane_cmd(scope, PaneCmd::Cycle(-1));
        }
        for action in ["focus_pane_left", "focus_pane_right", "focus_pane_up", "focus_pane_down"] {
            if self.keymap.consume(ctx, action) {
                let dir = match action {
                    "focus_pane_left" => Dir::Left,
                    "focus_pane_right" => Dir::Right,
                    "focus_pane_up" => Dir::Up,
                    _ => Dir::Down,
                };
                self.pane_cmd(scope, PaneCmd::DirFocus(dir));
            }
        }
    }

    /// 沿某个方向再切一刀：把当前焦点标签复制一份到新的一格。
    fn split_focused(&mut self, axis: Axis) {
        if let Some(id) = self.panes.focus() {
            self.pane_cmd(PaneScope::Main, PaneCmd::Split(id, axis));
        }
    }

    /// 新建一个终端页签：起一条 shell，配一棵只装它的面板树，并翻到它前面。
    ///
    /// 终端页签和文件页签排在同一条页签栏上，中央区一次只显示一格——这就是「终端
    /// 不跟文件编辑挤在一起」的落点：编辑那棵树从头到尾只有编辑面板，shell 全在
    /// 页签自己的树里。
    fn new_terminal_tab(&mut self, cwd: &Path) {
        let term_id = match self.terminal.create_at(cwd) {
            Ok(id) => id,
            Err(e) => {
                self.status = e;
                return;
            }
        };
        let id = self.term_seq;
        self.term_seq += 1;
        self.term_tabs.push(TermTab::new(id, term_id));
        self.term_active = Some(id);
        // 新页签里的 shell 直接接手键盘，点完「+终端」就能开始打字
        self.terminal.request_focus(term_id);
        self.status = "已新建终端页签".into();
    }

    /// 在当前工作目录下新建一个终端页签（顶栏「+终端」、`Ctrl+⇧\``、自检脚本）。
    fn new_terminal(&mut self) {
        let cwd = self.terminal.cwd.clone();
        self.new_terminal_tab(&cwd);
    }

    /// 中央区这一帧该把面板命令打在哪棵树上：终端页签在前就归它，否则归编辑树。
    fn front_scope(&self) -> PaneScope {
        self.term_active
            .map(PaneScope::Term)
            .unwrap_or(PaneScope::Main)
    }

    /// 翻到某个终端页签前面，并把键盘交给它焦点格里的 shell。
    fn show_term_tab(&mut self, id: u64) {
        let Some(at) = self.term_tabs.iter().position(|t| t.id == id) else {
            return;
        };
        self.term_tabs[at].request_focused_term(&mut self.terminal);
        self.term_active = Some(id);
    }

    /// 格子全被关掉的页签摘掉。眼前那组被摘掉就翻到最后剩下的那组，一组都不剩
    /// 才回编辑区——页签栏空了还杵在终端视图上会剩一块死白。
    fn prune_term_tabs(&mut self) {
        self.term_tabs.retain(|t| t.tree.count() > 0);
        if let Some(id) = self.term_active {
            if !self.term_tabs.iter().any(|t| t.id == id) {
                self.term_active = self.term_tabs.last().map(|t| t.id);
            }
        }
    }

    /// 页签栏上那把 ×：这一组的 shell 全收掉，页签跟着关掉。
    fn close_term_tab(&mut self, id: u64) {
        let Some(at) = self.term_tabs.iter().position(|t| t.id == id) else {
            return;
        };
        for t in self.term_tabs[at].term_ids() {
            self.terminal.remove(t);
        }
        self.term_tabs.remove(at);
        self.prune_term_tabs();
        self.status = "已关闭终端页签".into();
    }

    /// 把一块面板移到最靠某个方向的边缘（右键面板标题）。
    fn move_pane_to(&mut self, id: PaneId, dir: Dir) {
        if self.panes.move_to_edge(&self.pane_layout, id, dir).is_some() {
            self.status = "已移动面板".into();
        }
        self.sync_active();
    }

    /// `Ctrl+`` / 顶栏「终端」：在编辑区和终端之间来回切。终端页签正显示着就收回
    /// 编辑区；否则翻到最后那组接着用（滚动区、还在跑的命令都原样留着）；一组都
    /// 没有才新建。
    fn focus_or_new_terminal(&mut self) {
        if self.term_active.is_some() {
            self.term_active = None;
            self.status = "已切回编辑区 · Ctrl+` 唤回终端".into();
            return;
        }
        if let Some(id) = self.term_tabs.last().map(|t| t.id) {
            self.show_term_tab(id);
            self.status = "已唤回终端页签".into();
            return;
        }
        self.new_terminal();
    }

    /// 第一条还活着的 shell，自检脚本往里敲命令用。没在页签上显示的组也算：
    /// PTY 一直活着，脚本要的就是这条链路能写进去。
    fn first_terminal_id(&self) -> Option<u64> {
        self.term_tabs
            .iter()
            .find_map(|t| t.term_ids().first().copied())
    }

    /// 关掉一块面板。只剩一块时不销毁，退回空白编辑面板。
    fn close_pane(&mut self, id: PaneId) {
        if self.panes.count() <= 1 {
            if let Some(slot) = self.panes.payload_of_mut(id) {
                // 最后一块面板不销毁，退回空白编辑面板，不然主窗口剩一格死的
                *slot = PaneKind::Editor(None);
            }
            self.sync_active();
            self.status = "已清空面板".into();
            return;
        }
        self.panes.remove(id);
        self.sync_active();
        self.status = "已关闭面板".into();
    }

    /// 关闭焦点面板里的标签。编辑树的格子全是编辑格，所以这里不会碰到 shell；
    /// 终端页签上的 × 走 [`EasyEditorApp::close_term_tab`]，格子里的 × 走
    /// [`PaneCmd::Close`]（真杀进程），都不占这个键。
    fn close_focused(&mut self) {
        if let Some(idx) = self.focused_tab() {
            self.close_tab(idx);
        }
    }

    // ---------- 终端页签 ----------

    /// 终端页签在前时，中央区画的是这一组的网格：和编辑区走同一套
    /// [`EasyEditorApp::draw_grid`]，只是命令打到这个页签自己的树上。
    fn term_grid(&mut self, ui: &mut Ui, tab_id: u64) {
        let Some(at) = self.term_tabs.iter().position(|t| t.id == tab_id) else {
            // 页签刚被关掉：回编辑区，别让这一帧画空
            self.term_active = None;
            return;
        };
        let scope = PaneScope::Term(tab_id);
        let area = ui.available_rect_before_wrap();
        let layout = self.term_tabs[at].tree.layout(area, pane::DIVIDER);
        self.term_tabs[at].layout = layout.clone();
        // 点在哪格就聚焦哪格：快捷键能循环切，但鼠标指哪打哪才是大家习惯的
        if let Some(pos) = pane_click(ui.ctx(), area) {
            if let Some(id) = layout.pane_at(pos) {
                self.pane_cmd(scope, PaneCmd::Focus(id));
            }
        }
        let leaves = pane_leaves(&self.term_tabs[at].tree, &layout, self.tabs.len());
        self.draw_grid(ui, &layout, &leaves, scope, "");
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
        let Some(idx) = self.focused_tab() else {
            return;
        };
        let Some(file) = self.tabs.get(idx) else {
            return;
        };
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
        let Some(file) = self.tabs.get(idx) else {
            return;
        };
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
                        0 => self.open_settings(SettingsSection::Keymap),
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
                const STAGES: [u64; 13] = [
                    400, 1200, 2000, 2800, 3400, 3900, 4500, 5100, 5800, 6400, 7000, 7600, 8200,
                ];
                if self.verify_stage < STAGES.len() && ms >= STAGES[self.verify_stage] {
                    match self.verify_stage {
                        0 => {
                            // 打开第一个文件
                            let p = std::env::temp_dir().join("ee-split").join("data.json");
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
                            let p = std::env::temp_dir().join("ee-split").join("data.xml");
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
                            // 每新建一个终端就多一个页签：编辑区的格子数一个都不会变
                            self.new_terminal();
                            self.new_terminal();
                            println!(
                                "[verify] split 终端页签数={} shell数={} 编辑面板数={}",
                                self.term_tabs.len(),
                                self.terminal.len(),
                                self.panes.count()
                            );
                        }
                        5 => {
                            // 往第一个页签的 shell 敲一条命令，验证 PTY 写入链路
                            match self.first_terminal_id() {
                                Some(id) => {
                                    self.terminal.send_text(id, "echo EASY_EDITOR_PTY_OK\r");
                                    println!("[verify] split 已向终端 {id:?} 写入命令");
                                }
                                None => println!("[verify] split 没有终端，跳过 PTY 检查"),
                            }
                        }
                        // 下面三步走真实事件队列：按键由 handle_shortcuts 在下一帧消费，
                        // 所以每一步之间要留一帧以上的间隔
                        6 => {
                            // 新建终端时页签翻到了前面，这里先让回编辑区再试自动拆分
                            self.term_active = None;
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
                            "[verify] split Alt+Shift+D 后编辑面板数={}（两块编辑格 → 应左右拆，变 3）",
                            self.panes.count()
                        ),
                        // 页签内再切一格走的是同一套面板命令，和编辑区的拆分互不相干
                        8 => {
                            let target = self
                                .term_tabs
                                .first()
                                .and_then(|t| t.tree.focus().map(|id| (t.id, id)));
                            if let Some((tab, id)) = target {
                                self.pane_cmd(PaneScope::Term(tab), PaneCmd::Split(id, Axis::Horizontal));
                            }
                            println!(
                                "[verify] split 页签内拆分后 第一组格子数={} shell数={} status={}",
                                self.term_tabs.first().map_or(0, |t| t.tree.count()),
                                self.terminal.len(),
                                self.status
                            );
                        }
                        9 => println!(
                            "[verify] split 终端页签总览={}",
                            self.term_tabs
                                .iter()
                                .map(|t| format!(
                                    "#{} {}格 {}在前 「{}」",
                                    t.id,
                                    t.tree.count(),
                                    if Some(t.id) == self.term_active { "翻" } else { "没" },
                                    t.title(&self.terminal)
                                ))
                                .collect::<Vec<_>>()
                                .join(" | ")
                        ),
                        10 => self.take_screenshot_as(ctx, "shot-editor.bmp"),
                        // 截图只拍到「编辑在前」那一面；这里再翻到终端页签，
                        // 下一拍才是终端网格铺满中央区的那一面
                        11 => {
                            if let Some(id) = self.term_tabs.first().map(|t| t.id) {
                                self.show_term_tab(id);
                            }
                        }
                        _ => {
                            let cells = self
                                .term_active
                                .and_then(|id| self.term_tabs.iter().find(|t| t.id == id))
                                .map_or(0, |t| t.tree.count());
                            println!(
                                "[verify] split 停在终端页签 term_active={:?} 该组格子数={} 编辑面板数={}",
                                self.term_active, cells, self.panes.count()
                            );
                            self.take_screenshot_as(ctx, "shot-term.bmp");
                        }
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
                            let dir = std::env::temp_dir().join("ee-word");
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
                            let dir = std::env::temp_dir().join("ee-new");
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
        self.take_screenshot_as(ctx, "screenshot.bmp");
    }

    /// 截图存成指定文件名：一个脚本要拍好几面时（编辑在前 / 终端在前），
    /// 都往 screenshot.bmp 写的话后一张会把前一张盖掉。
    fn take_screenshot_as(&mut self, ctx: &egui::Context, name: &str) {
        let dir = config::app_data_dir();
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(name);
        let target = path.clone();
        ctx.request_screenshot(move |image| {
            let _ = save_bmp(&image, &target);
        });
        self.status = format!("已保存截图 {}", path.display());
    }

    fn set_font_size(&mut self, size: f32, ctx: &egui::Context) {
        self.font_size = size.clamp(theme::EDITOR_FONT_MIN, theme::EDITOR_FONT_MAX);
        self.pending_font = self.font_size;
        let _ = ctx;
    }

    /// 界面字号。样式表是挂在 `Context` 上的全局状态，所以改完必须立刻重铺，
    /// 否则要等到下次切主题才看得出动静。
    fn set_ui_font_size(&mut self, size: f32, ctx: &egui::Context) {
        self.ui_font = size.clamp(theme::UI_FONT_MIN, theme::UI_FONT_MAX);
        self.pending_ui_font = self.ui_font;
        theme::apply_style(ctx, self.dark, self.ui_font);
    }

    /// 深浅色。样式表、系统标题栏、语法高亮三处都要跟着换，漏一处就有一半界面是旧色。
    fn set_theme(&mut self, dark: bool, ctx: &egui::Context) {
        if self.dark == dark {
            return;
        }
        self.dark = dark;
        theme::apply_style(ctx, self.dark, self.ui_font);
        titlebar::apply(self.hwnd, self.dark);
        self.hl.set_theme(if dark {
            crate::syntax::DARK_THEME
        } else {
            crate::syntax::LIGHT_THEME
        });
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
                .button(RichText::new("打开文件夹").size(crate::theme::md(ui)))
                .on_hover_text(h_open)
                .clicked()
            {
                self.want_open_folder = true;
            }
            let h_open_file = hint("open_file");
            if ui
                .button(RichText::new("打开文件").size(crate::theme::md(ui)))
                .on_hover_text(format!("{h_open_file}  任意位置的文件，工作区不变"))
                .clicked()
            {
                self.want_open_files = true;
            }
            let h_save = hint("save");
            if ui
                .add_enabled(
                    self.active.is_some(),
                    egui::Button::new(RichText::new("保存").size(crate::theme::md(ui))),
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
                    egui::Button::new("←"),
                )
                .on_hover_text(h_back)
                .clicked()
            {
                self.nav_goto(false);
            }
            if ui
                .add_enabled(
                    self.nav.can_forward(),
                    egui::Button::new("→"),
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
                    egui::Button::new(RichText::new("新文件").size(crate::theme::md(ui))),
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
                    egui::Button::new(RichText::new("新文件夹").size(crate::theme::md(ui))),
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
                .selectable_label(side, RichText::new("侧栏").size(crate::theme::md(ui)))
                .on_hover_text(h_side)
                .clicked()
            {
                side = !side;
            }
            self.show_side = side;

            let h_wrap = format!("长行折进面板 / 横向滚动  {}", hint("wrap_toggle"));
            if ui
                .selectable_label(self.word_wrap, RichText::new("折行").size(crate::theme::md(ui)))
                .on_hover_text(h_wrap)
                .clicked()
            {
                self.toggle_wrap();
            }

            let h_term = format!(
                "在编辑区和终端之间来回切，一组终端都没有就新开  {}",
                hint("terminal_toggle")
            );
            if ui
                .selectable_label(
                    self.term_active.is_some(),
                    RichText::new("终端").size(crate::theme::md(ui)),
                )
                .on_hover_text(h_term)
                .clicked()
            {
                self.focus_or_new_terminal();
            }
            let h_tnew = format!("新建一个终端页签  {}", hint("terminal_new"));
            if ui
                .button(RichText::new("+终端").size(crate::theme::md(ui)))
                .on_hover_text(h_tnew)
                .clicked()
            {
                self.new_terminal();
            }

            let h_col = format!("向右拆分当前面板  {}", hint("split_col"));
            if ui
                .button(RichText::new("‖ 拆分").size(crate::theme::md(ui)))
                .on_hover_text(h_col)
                .clicked()
            {
                self.split_focused(Axis::Horizontal);
            }
            let h_row = format!("向下拆分当前面板  {}", hint("split_row"));
            if ui
                .button(RichText::new("═ 拆分").size(crate::theme::md(ui)))
                .on_hover_text(h_row)
                .clicked()
            {
                self.split_focused(Axis::Vertical);
            }

            ui.separator();

            let h_quick = hint("quick_open");
            if ui
                .button(RichText::new("快速打开").size(crate::theme::md(ui)))
                .on_hover_text(h_quick)
                .clicked()
            {
                self.open_quick_open();
            }
            let h_search = hint("global_search");
            if ui
                .button(RichText::new("全局搜索").size(crate::theme::md(ui)))
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
                    egui::Button::new(RichText::new("查找").size(crate::theme::md(ui))),
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
                    egui::Button::new(RichText::new("日志块").size(crate::theme::md(ui))),
                )
                .on_hover_text(h_journal)
                .clicked()
            {
                self.insert_journal_block();
            }

            // 顶栏只多这一个入口：连接相关的操作都在这个下拉里，不再摊开一排按钮
            let menu = ui.menu_button(RichText::new("数据库").size(crate::theme::md(ui)), |ui| {
                // 先把「这段 SQL 发去哪儿」说清楚，再给动作
                self.connection_row(ui);
                ui.separator();
                let h_new_query = format!(
                    "新建一个 .sql 查询文件，首行带好连接  {}",
                    hint("sql_new_query")
                );
                if ui.button("新建查询").on_hover_text(h_new_query).clicked() {
                    self.new_query(None);
                    ui.close();
                }
                let h_sql_insert = format!("在光标处插入 SQL 块  {}", hint("sql_insert"));
                if ui
                    .add_enabled(self.active.is_some(), egui::Button::new("插入 SQL 块"))
                    .on_hover_text(h_sql_insert)
                    .clicked()
                {
                    self.insert_sql_block();
                    ui.close();
                }
                let h_sql_run = format!(
                    "只跑选中的那段；没有选区就跑光标所在的这一句（按分号认）  {}",
                    hint("sql_run")
                );
                if ui
                    .add_enabled(self.active.is_some(), egui::Button::new("执行 SQL"))
                    .on_hover_text(h_sql_run)
                    .clicked()
                {
                    self.sql_run();
                    ui.close();
                }
                let h_sql_block = format!(
                    "整个块的语句一起交出去，跑完回写块头的状态；SQL 模式的查询文件跑整份  {}",
                    hint("sql_run_block")
                );
                if ui
                    .add_enabled(self.active.is_some(), egui::Button::new("执行整个块"))
                    .on_hover_text(h_sql_block)
                    .clicked()
                {
                    self.sql_run_block();
                    ui.close();
                }
                let n = self.results.count();
                let label = if n == 0 {
                    "结果区".to_string()
                } else {
                    format!("结果区（{n}）")
                };
                if ui
                    .selectable_label(self.results.open, label)
                    .on_hover_text("底部结果区，一条语句一个面板")
                    .clicked()
                {
                    self.results.open = !self.results.open;
                    ui.close();
                }
                // SQL 模式：.sql 文件天然开着，其他文件在这儿手动开
                let idx = self.focused_tab();
                let file_name = idx
                    .and_then(|i| self.tabs.get(i))
                    .map(|f| {
                        f.path
                            .file_stem()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned()
                    })
                    .unwrap_or_default();
                let mut on = idx.is_some_and(|i| self.tabs[i].sql_mode);
                let check = ui
                    .add_enabled(
                        idx.is_some(),
                        egui::Checkbox::new(&mut on, "本文件按 SQL 处理"),
                    )
                    .on_hover_text(
                        "按 SQL 上色，且 Ctrl+Enter 在没有选区也没有块时跑整份文件。\n\
                         打开 .sql 文件时自动开着，这里关掉的只影响当前标签。",
                    );
                if check.changed() {
                    if let Some(i) = idx {
                        self.tabs[i].sql_mode = on;
                        self.status = if on {
                            format!("「{file_name}」按 SQL 处理，Ctrl+Enter 可直接跑整份文件")
                        } else {
                            format!("「{file_name}」不再按 SQL 处理，回到只认 SQL 块")
                        };
                    }
                }
                ui.separator();
                if ui
                    .button("连接管理…")
                    .on_hover_text("增删改数据库连接，配置写在 config.json")
                    .clicked()
                {
                    self.database.open_manager();
                    ui.close();
                }
                if ui
                    .button("侧栏数据库树")
                    .on_hover_text("在侧栏里按 连接 → 库 → 表 浏览")
                    .clicked()
                {
                    self.side = SideTab::Database;
                    self.show_side = true;
                    ui.close();
                }
            });
            let _ = menu
                .response
                .on_hover_text("SQL 块的插入与执行，以及连接管理和库表树");

            // 右组是从右往左排的，这一排剩下的地方不够就会一路压过左组的字，
            // 所以先换一排再排它。注意要量「这一排还剩多少」：可换行布局下
            // available_width() 报的是整排的宽度，永远碰不到这个阈值。
            // 380 = 右组固定那部分（默认界面字号下实测约 350）再留一点看工作区名的地方。
            // 加了「数据库」下拉后左组变宽，这个数再往下压就只剩个位数给工作区名了。
            // 字号调大时右组本身也变宽，所以阈值跟着档位换算，不能停在 380。
            if ui.available_rect_before_wrap().width() < theme::scale(ui.ctx(), 380.0) {
                ui.end_row();
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let h_help = format!("快捷键设置  {}", hint("help"));
                let h_settings = "设置：外观、快捷键、终端、系统、数据库";
                if ui
                    .button(RichText::new("?").size(crate::theme::md(ui)))
                    .on_hover_text(h_help)
                    .clicked()
                {
                    self.toggle_settings(SettingsSection::Keymap);
                }
                if ui
                    .button(RichText::new(if self.dark { "浅色" } else { "深色" }).size(crate::theme::md(ui)))
                    .on_hover_text("切换主题")
                    .clicked()
                {
                    let dark = !self.dark;
                    self.set_theme(dark, ui.ctx());
                }
                // 新建终端用哪个 shell。原来在终端面板的页签条上，页签条没了就挪到这儿
                egui::ComboBox::from_id_salt("shell_picker")
                    .selected_text(
                        RichText::new(format!("shell: {}", self.terminal.shell.label())).size(crate::theme::sm(ui)),
                    )
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
                    .on_hover_text(h_settings)
                    .clicked()
                {
                    self.toggle_settings(SettingsSection::Appearance);
                }
                let h_dec = hint("font_dec");
                if ui.small_button("-").on_hover_text(h_dec).clicked() {
                    self.set_font_size(self.font_size - 1.0, ui.ctx());
                }
                ui.label(
                    RichText::new(format!("{:.0}", self.font_size))
                        .size(crate::theme::sm(ui))
                        .weak(),
                );
                let h_inc = hint("font_inc");
                if ui.small_button("+").on_hover_text(h_inc).clicked() {
                    self.set_font_size(self.font_size + 1.0, ui.ctx());
                }

                if let Some(root) = &self.root {
                    let parent = root.parent().unwrap_or(root.as_path());
                    // 工作区名可能很长，顶栏不是看全名的地方：排不下就截断，完整路径挂悬浮。
                    ui.add(
                        egui::Label::new(
                            RichText::new(filetree::relative(parent, root))
                                .size(crate::theme::sm(ui))
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
                        (SideTab::Database, "数据库"),
                    ] {
                        if ui
                            .selectable_label(self.side == tab, RichText::new(label).size(crate::theme::md(ui)))
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
                            ui.label(RichText::new(format!("{count}")).size(crate::theme::sm(ui)).weak());
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
                                // 只说清楚去哪儿开：顶栏那两颗按钮已经是唯一的入口了，
                                // 侧栏再来一遍会在左边多出一排重复的。
                                ui.label(
                                    RichText::new("尚未打开文件夹 · 顶栏有「打开文件夹」和「打开文件」")
                                        .weak(),
                                );
                            });
                        }
                    }
                    SideTab::Search => {
                        self.search_panel(ui);
                    }
                    SideTab::Journal => {
                        journal_jump = self.journal_panel(ui);
                    }
                    SideTab::Database => {
                        // 每条连接排了几条语句先抄出来：侧栏要拿它给行尾摆「⏳2」
                        let busy = self.results.busy_by_conn();
                        self.database.sidebar(ui, &busy);
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
            Some(TreeAction::Renamed(old, new)) => self.file_renamed(&old, &new),
            Some(TreeAction::OpenTerminal(dir)) => self.new_terminal_tab(&dir),
            Some(TreeAction::NewQuery(dir)) => self.new_query(Some(dir)),
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

    fn search_panel(&mut self, ui: &mut Ui) {
        ui.add_space(2.0);
        // 回车归这个框，只在这一帧焦点真在框上。以前判的是 `resp.lost_focus()`：
        // 单行框收到 Enter 会当场交还焦点，那个条件就把「焦点这一帧刚被别人拿走」
        // 也算了进来——用户在正文里敲的换行于是又发起一次搜索。跟查找栏一个写法：
        // 按键在 `add` 之前摘，摘完这一帧的 Enter 就不会再往下走。
        let query_id = egui::Id::unique("side_search_query");
        let focused = ui.memory(|m| m.has_focus(query_id));
        // 组字那一帧的回车是输入法在确认候选词，不是「开始搜索」
        let composing = ime::composing(ui);
        let enter =
            focused && !composing && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter));

        let resp = ui.add(
            egui::TextEdit::singleline(&mut self.search.query)
                .id(query_id)
                .hint_text("搜索内容，回车开始")
                .desired_width(f32::INFINITY),
        );
        if self.focus_search {
            resp.request_focus();
            self.focus_search = false;
        }
        if enter {
            self.search_now();
        }

        ui.horizontal(|ui| {
            if ui.button(RichText::new("搜索").size(crate::theme::md(ui))).clicked() {
                self.search_now();
            }
            ui.checkbox(&mut self.search.case_sensitive, "Aa");
            ui.checkbox(&mut self.search.regex, ".*");
            if !self.search.info.is_empty() {
                ui.label(RichText::new(&self.search.info).size(crate::theme::sm(ui)).weak());
            }
        });
        ui.separator();

        if self.search.hits.is_empty() {
            ui.add_space(10.0);
            ui.vertical_centered(|ui| {
                if self.search.busy {
                    ui.spinner();
                    ui.label(RichText::new("搜索中…").weak().size(crate::theme::md(ui)));
                } else {
                    ui.label(RichText::new("输入关键字后回车").weak().size(crate::theme::md(ui)));
                    ui.label(
                        RichText::new(format!("搜索引擎：{}", self.search.engine))
                            .weak()
                            .size(crate::theme::sm(ui)),
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
                        egui::Label::new(RichText::new("位置").size(crate::theme::sm(ui)).strong())
                            .sense(egui::Sense::hover()),
                    );
                    lr.on_hover_text("拖动表头分隔线可以加宽这一列");
                });
                resp.on_hover_text("拖动表头分隔线可以加宽这一列");
                header.col(|ui| {
                    ui.label(RichText::new("内容").size(crate::theme::sm(ui)).strong());
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
                                    .size(crate::theme::sm(ui))
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
                                RichText::new(hit.preview.clone()).size(crate::theme::sm(ui)).monospace(),
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
                ui.label(RichText::new("先打开一个文本文件").weak().size(crate::theme::md(ui)));
                ui.label(
                    RichText::new("日志块是写在文件里的一种格式，\n不是独立数据")
                        .weak()
                        .size(crate::theme::sm(ui)),
                );
            });
            return None;
        };

        let blocks = journal::scan(&self.tabs[idx].text);
        let current_line = self.tabs[idx].line.saturating_sub(1);

        ui.horizontal(|ui| {
            if ui
                .button(RichText::new("插入块").size(crate::theme::sm(ui)))
                .on_hover_text("Ctrl+J")
                .clicked()
            {
                self.insert_journal_block();
            }
            if ui
                .button(RichText::new("上一个").size(crate::theme::sm(ui)))
                .on_hover_text("Ctrl+Alt+↑")
                .clicked()
            {
                self.journal_goto(false);
            }
            if ui
                .button(RichText::new("下一个").size(crate::theme::sm(ui)))
                .on_hover_text("Ctrl+Alt+↓")
                .clicked()
            {
                self.journal_goto(true);
            }
            if ui
                .button(RichText::new("标记完成").size(crate::theme::sm(ui)))
                .on_hover_text("Ctrl+Alt+D")
                .clicked()
            {
                self.journal_toggle();
            }
        });
        ui.add_space(2.0);

        if blocks.is_empty() {
            ui.label(RichText::new("这个文件里还没有日志块").weak().size(crate::theme::sm(ui)));
            return None;
        }

        let mut jump = None;
        egui::ScrollArea::vertical()
            .id_salt("journal_list")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for block in &blocks {
                    let active = block.line == current_line;
                    let mark = if block.is_done() {
                        "[已完成]"
                    } else {
                        "[待处理]"
                    };
                    let label = format!("{} {}", mark, block.title);
                    let resp = ui.add(
                        egui::Label::new(RichText::new(label).size(crate::theme::md(ui)).color(
                            if block.is_done() {
                                Color32::from_rgb(120, 150, 120)
                            } else {
                                ui.visuals().text_color()
                            },
                        ))
                        .sense(egui::Sense::click())
                        .truncate(),
                    );
                    let resp = resp.on_hover_cursor(crate::theme::HAND);
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
                    ui.label(RichText::new("查找").size(crate::theme::sm(ui)).weak());

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
                            .size(crate::theme::sm(ui))
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
                    if ui.small_button("↓").on_hover_text("下一个 Enter").clicked() {
                        step = Some(true);
                    }
                    if ui.checkbox(&mut self.find.case_sensitive, "Aa").changed() {
                        query_changed = true;
                    }
                    if ui.checkbox(&mut self.find.regex, ".*").changed() {
                        query_changed = true;
                    }
                    if ui
                        .selectable_label(self.find.show_replace, RichText::new("替换").size(crate::theme::sm(ui)))
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
                        ui.label(RichText::new("替换").size(crate::theme::sm(ui)).weak());

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
                            .button(RichText::new("替换").size(crate::theme::sm(ui)))
                            .on_hover_text("Enter")
                            .clicked()
                        {
                            do_replace = true;
                        }
                        if ui
                            .button(RichText::new("全部替换").size(crate::theme::sm(ui)))
                            .on_hover_text("Ctrl+Enter")
                            .clicked()
                        {
                            do_replace_all = true;
                        }
                        if self.find.regex {
                            ui.label(
                                RichText::new("正则模式，替换串支持 $1 $2")
                                    .size(crate::theme::xs(ui))
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
                                .size(crate::theme::sm(ui))
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
                                .size(crate::theme::sm(ui))
                                .color(Color32::from_rgb(190, 140, 60)),
                            );
                        } else {
                            ui.label(RichText::new(editor::cursor_text(file)).size(crate::theme::sm(ui)));
                            ui.separator();
                            let lang = self.hl.language_name(Some(&file.path), &file.text);
                            ui.label(RichText::new(lang).size(crate::theme::sm(ui)).weak());
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
                            .size(crate::theme::sm(ui))
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
                    ui.label(RichText::new("未打开文件").size(crate::theme::sm(ui)).weak());
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(&self.status).size(crate::theme::sm(ui)).weak());
                    if let Some(idx) = self.active {
                        if let Some(file) = self.tabs.get(idx) {
                            let n = journal::scan(&file.text).len();
                            if n > 0 {
                                ui.separator();
                                ui.label(RichText::new(format!("{n} 个日志块")).size(crate::theme::sm(ui)).weak());
                            }
                        }
                    }
                    self.db_chip(ui);
                });
            });
        });
    }

    /// 状态栏最右边那一小块：这个文件的 SQL 会发到哪条连接，点一下跳侧栏。
    ///
    /// 只在跟 SQL 有关的文件上出现（开了 SQL 模式或有块），其余时候不占地方。
    fn db_chip(&mut self, ui: &mut Ui) {
        let Some(idx) = self.active else { return };
        let Some(file) = self.tabs.get(idx) else {
            return;
        };
        if !file.sql_mode && sql::scan(&file.text).is_empty() {
            return;
        }
        ui.separator();
        let name = self.database.active_conn().map(|s| s.to_string());
        let accent = ui.visuals().hyperlink_color;
        let (mark, state) = match &name {
            // 「○ 未连接」也照主色写字：那颗 ○ 已经说了状态，字再灰一层就整块像坏了
            Some(n) => {
                let (mark, state, _) = self.database.link_mark(ui, n);
                (mark.to_string(), state)
            }
            None => (String::new(), "还没有数据库连接".to_string()),
        };
        let label = match &name {
            // 库名一起写：同一条连接上换库是常事，只报连接等于报了一半
            Some(name) => {
                let db = self.database.current_db(name);
                if db.is_empty() {
                    format!("SQL → {name} {mark}")
                } else {
                    format!("SQL → {name}/{db} {mark}")
                }
            }
            None => "SQL → 未配置连接".to_string(),
        };
        let resp = ui
            .add(
                egui::Button::new(RichText::new(label).size(crate::theme::sm(ui)).color(accent))
                    .frame(true)
                    .frame_when_inactive(false),
            )
            .on_hover_cursor(crate::theme::HAND)
            .on_hover_text(format!(
                "{state}\n点一下打开侧栏数据库树：那里能连接 / 断开、换库、看每条连接在跑几条"
            ));
        if resp.clicked() {
            self.side = SideTab::Database;
            self.show_side = true;
        }
    }

    fn central(&mut self, ui: &mut Ui) {
        // 空状态提示也读真实快捷键，用户改过之后这里不会说老组合
        let hints = {
            let k = |a: &str| self.keymap.get(a).to_string();
            format!(
                "{} 打开文件夹   {} 打开文件   {} 快速打开   {} 全局搜索\n\
                 {} 查找   {} 替换   {} 日志块   {} / {} 块间跳转\n\
                 {} 新建文件   {} 终端   {} 新建终端\n\
                 {} 新建查询   {} 插入 SQL 块   {} 执行 SQL   {} 结果区\n\
                 {} 自动拆分   {} / {} 手动拆分   {} 关闭面板   {} 下一个面板   {} 快捷键",
                k("open_folder"),
                k("open_file"),
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
                k("sql_new_query"),
                k("sql_insert"),
                k("sql_run"),
                k("results_toggle"),
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
            // 终端页签翻在前面：中央区整块换成那一组的网格，编辑树原样退到后面，
            // 切回文件页签时还是原来那几格、原来那个焦点。
            if let Some(tab) = self.term_active {
                self.term_grid(ui, tab);
                return;
            }
            let area = ui.available_rect_before_wrap();
            let layout = self.panes.layout(area, pane::DIVIDER);
            // 先存下这一帧的矩形：网格里点的「移动面板 / 拆分」按钮在同一帧末尾执行，
            // 要用的是当前几何，不是上一帧的
            self.pane_layout = layout.clone();

            // 点在哪块面板上就聚焦它：按几何命中判断，容器自己的 response 抢不过里面的 TextEdit
            if let Some(pos) = pane_click(ui.ctx(), area) {
                if let Some(id) = layout.pane_at(pos) {
                    self.pane_cmd(PaneScope::Main, PaneCmd::Focus(id));
                }
            }

            let leaves = pane_leaves(&self.panes, &layout, self.tabs.len());
            self.draw_grid(ui, &layout, &leaves, PaneScope::Main, &hints);
        });
    }

    /// 画一格面板的网格：每块面板一个用 `max_rect` 定位的子 `Ui`，最后再叠分隔线。
    ///
    /// 编辑树和终端页签的 shell 树共用这一套——格子的标题、拆分 / 移动 / 关闭三个
    /// 按钮、焦点描边、分隔条拖拽全都一样，不一样的是格子里放什么（编辑器还是
    /// shell）以及命令打到哪棵树（`scope`）。
    ///
    /// 两棵树的 `PaneId` / `SplitId` 各数各的，所以 id 一律按作用域加盐：不然切到
    /// 另一个终端页签会捡到上一个页签的面板状态（焦点、滚动位置串台）。
    ///
    /// 子 `Ui` 一律走 `scope_builder`，这样 egui 会正常收尾（记录 min_rect、推进光标）；
    /// 直接 `Ui::new` 造出来的面板没有父级关联，hover 和键盘焦点都会出问题。
    fn draw_grid(
        &mut self,
        ui: &mut Ui,
        layout: &TreeLayout,
        leaves: &[PaneLeaf],
        scope: PaneScope,
        hints: &str,
    ) {
        let font_size = self.font_size;
        let dark = self.dark;
        let wrap = self.word_wrap;
        let keys = self.keymap.to_config();
        let hint = |action: &str| keys.get(action).cloned().unwrap_or_default();

        let mut changed = false;
        let mut close_id: Option<PaneId> = None;
        let mut split_at: Option<(PaneId, Axis)> = None;
        let mut move_to: Option<(PaneId, Dir)> = None;

        for leaf in leaves {
            let (id, rect, kind, focused) = (leaf.id, leaf.rect, leaf.kind, leaf.focused);
            let stroke = if focused {
                ui.style().visuals.selection.stroke
            } else {
                egui::Stroke::NONE
            };
            let pane_id = ui.make_persistent_id(("pane", scope, id.0));
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
                                let (dirty, large) = (self.tabs[idx].dirty, self.tabs[idx].large);
                                let mut t = self.tab_title(idx);
                                if dirty {
                                    t.push_str(" ●");
                                }
                                if large {
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
                                format!(">_ {name}")
                            }
                        };
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::Label::new(RichText::new(title).size(crate::theme::sm(ui)).weak())
                                    .sense(egui::Sense::click()),
                            )
                            .on_hover_text("右键：移动面板")
                            .on_hover_cursor(crate::theme::HAND)
                            .context_menu(|ui| {
                                // 终端只能开在下方时用户没法把它挪到右边，
                                // 这里给每块面板一个「移到边缘」的入口。
                                ui.label(RichText::new("移动面板").size(crate::theme::sm(ui)).weak());
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
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .small_button(RichText::new("×").size(crate::theme::sm(ui)))
                                        .on_hover_text(format!("关闭面板 {}", hint("pane_close")))
                                        .clicked()
                                    {
                                        close_id = Some(id);
                                    }
                                    if ui
                                        .small_button(RichText::new("上下").size(crate::theme::xs(ui)))
                                        .on_hover_text(format!("向下拆分 {}", hint("split_row")))
                                        .clicked()
                                    {
                                        split_at = Some((id, Axis::Vertical));
                                    }
                                    if ui
                                        .small_button(RichText::new("左右").size(crate::theme::xs(ui)))
                                        .on_hover_text(format!("向右拆分 {}", hint("split_col")))
                                        .clicked()
                                    {
                                        split_at = Some((id, Axis::Horizontal));
                                    }
                                },
                            );
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
                                pane_welcome(ui, full, hints, &hint);
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
            let did = ui.make_persistent_id(("divider", scope, d.id.0));
            let resp = ui.interact(d.rect, did, egui::Sense::drag());
            if resp.dragged() {
                if let Some(pos) = ui.ctx().input(|i| i.pointer.interact_pos()) {
                    let coord = match d.axis {
                        Axis::Horizontal => pos.x,
                        Axis::Vertical => pos.y,
                    };
                    self.drag_divider(scope, d.id, *d, coord);
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
            self.pane_cmd(scope, PaneCmd::Split(id, axis));
        }
        if let Some((id, dir)) = move_to {
            self.pane_cmd(scope, PaneCmd::Move(id, dir));
        }
        if let Some(id) = close_id {
            self.pane_cmd(scope, PaneCmd::Close(id));
        }
        if changed && self.find.open {
            self.find.stale = true;
        }
    }

    /// 拖动分隔条。两棵树的 `SplitId` 各数各的（编辑树从 1 起，每个终端页签也从 1 起），
    /// 所以必须按作用域打回自己那棵树，否则会把编辑区的比例改到终端的面板上。
    fn drag_divider(
        &mut self,
        scope: PaneScope,
        id: pane::SplitId,
        divider: pane::DividerRect,
        pointer: f32,
    ) {
        match scope {
            PaneScope::Main => {
                self.panes.drag_divider(id, &divider, pointer);
            }
            PaneScope::Term(tab) => {
                if let Some(t) = self.term_tabs.iter_mut().find(|t| t.id == tab) {
                    t.tree.drag_divider(id, &divider, pointer);
                }
            }
        }
    }

    /// 顶部标签栏。文件标签是全局的：一块面板只是「正在显示某个标签」，
    /// 关掉面板不会关掉标签，同一个标签也可以同时显示在两块面板里。
    ///
    /// 终端页签排在同一排的后面（浏览器式）：一次只显示一格，选中谁中央区就画谁。
    /// 页签上的 × 收掉这一组的全部 shell；格子里的 × 关掉最后一条时页签自己消失。
    fn tab_bar(&mut self, ui: &mut Ui) {
        let mut activate = None;
        let mut close = None;
        let mut term_activate = None;
        let mut term_close = None;
        // 终端页签翻在前面时，文件页签一个都不该亮着——中央区画的不是它们
        let shown = if self.term_active.is_some() {
            None
        } else {
            self.focused_tab()
        };
        // 一次算完整排：每个标签单独算会变成 O(n²) 的字符串拼活
        let paths: Vec<PathBuf> = self.tabs.iter().map(|t| t.path.clone()).collect();
        let titles = filetree::unique_titles(&paths);

        egui::ScrollArea::horizontal()
            .id_salt("tab_bar")
            .max_height(theme::scale(ui.ctx(), 28.0))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (i, tab) in self.tabs.iter().enumerate() {
                        let mut title = titles[i].clone();
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
                            .selectable_label(Some(i) == shown, RichText::new(title).size(crate::theme::md(ui)))
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
                            .add(egui::Button::new(RichText::new("×").size(crate::theme::sm(ui))).frame(false))
                            .on_hover_text("关闭标签")
                            .clicked()
                        {
                            close = Some(i);
                        }
                        ui.add_space(4.0);
                    }
                    if !self.tabs.is_empty() && !self.term_tabs.is_empty() {
                        // 一条竖线隔开：后面这几格是终端，不是文件
                        ui.separator();
                    }
                    for tab in self.term_tabs.iter() {
                        let name = tab.title(&self.terminal);
                        let cells = tab.tree.count();
                        let mut label = format!(">_ {name}");
                        if cells > 1 {
                            label.push_str(&format!(" ×{cells}"));
                        }
                        if ui
                            .selectable_label(
                                Some(tab.id) == self.term_active,
                                RichText::new(label).size(crate::theme::md(ui)),
                            )
                            .on_hover_text(format!(
                                "{name}：这一组有 {cells} 格终端，格子里还能再左右 / 上下拆分\n\
                                 点 × 关闭这一组的全部 shell"
                            ))
                            .clicked()
                        {
                            term_activate = Some(tab.id);
                        }
                        if ui
                            .add(
                                egui::Button::new(RichText::new("×").size(crate::theme::sm(ui)))
                                    .frame(false),
                            )
                            .on_hover_text("关闭这一组终端")
                            .clicked()
                        {
                            term_close = Some(tab.id);
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
        if let Some(id) = term_activate {
            self.show_term_tab(id);
        }
        if let Some(id) = term_close {
            self.close_term_tab(id);
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
    /// 选中文件页签也顺带把中央区从终端让回来——一次只显示一格。
    fn activate_tab(&mut self, idx: usize) {
        self.term_active = None;
        let hit = self
            .panes
            .pane_ids()
            .into_iter()
            .find(|p| self.panes.payload_of(*p).copied().and_then(PaneKind::tab) == Some(idx));
        match hit {
            Some(p) => {
                self.panes.set_focus(p);
                self.sync_active();
            }
            None => self.show_in_focused_pane(idx),
        }
        self.revalidate_find();
    }

    // ---------- 弹窗 ----------

    fn quick_open_window(&mut self, ctx: &egui::Context) {
        let Some(state) = &mut self.quick_open else {
            return;
        };
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
                // 每帧无条件 request_focus 会把输入法组字打断，中文填不进来（见 src/ime.rs）
                ime::focus_input_box(&resp);
                if resp.changed() {
                    query_changed = true;
                }
                // 组字期间方向键在选候选词、回车在确认候选，都归输入法，别当成界面操作
                let mut enter = false;
                if !ime::composing(ui) {
                    if ui.input(|i| i.key_pressed(Key::Escape)) {
                        close = true;
                    }
                    if ui.input(|i| i.key_pressed(Key::ArrowDown)) {
                        state.sel = (state.sel + 1).min(state.matches.len().saturating_sub(1));
                    }
                    if ui.input(|i| i.key_pressed(Key::ArrowUp)) {
                        state.sel = state.sel.saturating_sub(1);
                    }
                    enter = ui.input(|i| i.key_pressed(Key::Enter));
                }
                ui.separator();

                egui::ScrollArea::vertical()
                    .max_height(320.0)
                    .show(ui, |ui| {
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
                                .selectable_label(selected, RichText::new(text).size(crate::theme::md(ui)))
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

    /// 配置文件没读出来时的一次性说明。
    ///
    /// 这个弹窗不点掉不收：以前遇到解析失败是悄悄按默认值启动、然后照常落盘，
    /// 用户看到的后果就是「连接列表和自定义快捷键自己没了」。现在既不静默，
    /// 也不在原文没保住的情况下写回，这两件事都得让用户先看见。
    fn config_trouble_window(&mut self, ctx: &egui::Context) {
        if self.config_trouble_seen {
            return;
        }
        let Some(trouble) = &self.config.trouble else {
            return;
        };
        let file = trouble.file().to_path_buf();
        let lines = trouble.lines();
        let mut dismiss = false;
        egui::Window::new("配置文件没读出来")
            .collapsible(false)
            .resizable(false)
            .default_width(430.0)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                for line in &lines {
                    ui.label(line.as_str());
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    // 长路径没有可折行的空格，label 会横着捅穿边框，所以截断 + 悬浮看全 + 可复制
                    muted_line(ui, file.display().to_string());
                    if theme::chip(ui, "复制路径")
                        .on_hover_text("整条路径进剪贴板，粘到资源管理器地址栏就能打开")
                        .clicked()
                    {
                        ui.ctx().copy_text(file.display().to_string());
                    }
                });
                if trouble.blocks_save() {
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new("在它被改对之前，这次的设置都不会写回这个文件。")
                            .size(crate::theme::sm(ui))
                            .weak(),
                    );
                }
                ui.add_space(8.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("知道了").clicked() {
                        dismiss = true;
                    }
                });
            });

        if dismiss {
            self.config_trouble_seen = true;
        }
    }

    fn close_confirm(&mut self, ctx: &egui::Context) {
        let Some(idx) = self.pending_close else {
            return;
        };
        if idx >= self.tabs.len() {
            self.pending_close = None;
            return;
        }
        let name = self.tab_title(idx);
        // 这是要落盘还是要丢掉的决定，光给个文件名不够——同名文件在两个目录里时，
        // 「保存并关闭」写错地方比不保存麻烦得多。
        let where_ = self.tabs[idx].path.display().to_string();
        let mut action: Option<u8> = None;

        egui::Window::new("未保存的修改")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.label(format!("{name} 有未保存的修改，要保存吗？"));
                muted_line(ui, where_);
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

    /// 打开设置窗口，停在指定分区。已经开着就只换分区。
    fn open_settings(&mut self, section: SettingsSection) {
        if !self.show_settings {
            // 只在重新打开的那一刻要求居中，之后窗口拖到哪算哪
            self.center_settings = true;
        }
        self.settings_section = section;
        self.show_settings = true;
    }

    /// 快捷键那种开开合合：同一个动作再按一次就关掉。
    fn toggle_settings(&mut self, section: SettingsSection) {
        if self.show_settings && self.settings_section == section {
            self.recording = None;
            self.show_settings = false;
        } else {
            self.open_settings(section);
        }
    }

    /// 设置窗口：左边选分区，右边是当前分区的内容。
    ///
    /// 「快捷键设置」「终端设置」原来各是一个窗口，用户得先记住哪件事藏在哪个窗口里。
    /// 合成一个窗口后入口只剩这一个，顶栏的 `?` 与 `⚙` 只是各自跳到对应分区。
    fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.show_settings {
            // 没开着就没有「还没应用」这回事：把暂存对齐回实际值，下次打开照实显示
            self.pending_font = self.font_size;
            self.pending_ui_font = self.ui_font;
            return;
        }
        // 录制中：把这一帧的按键抓下来当新绑定。抓在窗口壳里而不放在分区里，
        // 是因为窗口被关掉时同样要把挂着的录制收掉。
        if self.settings_section == SettingsSection::Keymap {
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
        }

        let win_id = egui::Id::unique("ee_settings");
        let mut open = self.show_settings;
        let mut window = egui::Window::new("设置")
            .id(win_id)
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .min_width(theme::scale(ctx, 460.0))
            .default_size(egui::vec2(
                theme::scale(ctx, 700.0),
                theme::scale(ctx, 440.0),
            ));
        // anchor 每帧都生效，一直挂着就拖不动，所以只在刚打开那几帧居中。
        // 头一帧窗口还在量尺寸（尺寸是无穷大），照那个尺寸算居中会算飞到屏幕外，
        // 因此等 egui 记下真实矩形后再摆正。
        if self.center_settings && ctx.memory(|m| m.area_rect(win_id).is_some()) {
            window = window.anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO);
            self.center_settings = false;
        }
        window.show(ctx, |ui| {
            egui::Panel::left("settings_nav")
                .resizable(false)
                .exact_size(theme::scale(ui.ctx(), 112.0))
                .frame(egui::Frame::side_top_panel(ui.style()))
                .show(ui, |ui| {
                    for section in SettingsSection::ALL {
                        if ui
                            .selectable_value(&mut self.settings_section, section, section.label())
                            .on_hover_cursor(theme::HAND)
                            .clicked()
                        {
                            // 录制条只属于快捷键分区，切走就收掉，不然按键会被下一分区吞
                            self.recording = None;
                        }
                    }
                });
            egui::CentralPanel::default()
                .frame(
                    egui::Frame::central_panel(ui.style())
                        .inner_margin(theme::scale(ui.ctx(), 12.0)),
                )
                .show(ui, |ui| {
                    // 先取副本：下面每个分支都要以 &mut self 调进去
                    let section = self.settings_section;
                    match section {
                        SettingsSection::Appearance => self.settings_appearance(ui),
                        SettingsSection::Keymap => self.settings_keymap(ui),
                        SettingsSection::Terminal => self.settings_terminal(ui),
                        SettingsSection::System => self.settings_system(ui),
                        SettingsSection::Database => self.settings_database(ui),
                    }
                });
        });
        // 窗口是被标题栏的 ✕ 关的：录制还挂着的话，下一次按键会改掉最后一个动作
        if !open {
            self.recording = None;
        }
        self.show_settings = open;
    }

    /// 「外观」分区：界面字号、编辑器字号、主题、自动换行。
    ///
    /// 两个字号各管一摊：界面字号管编辑区以外的所有文字（档位怎么由它铺开见 `theme`
    /// 模块头「字号只有一处来源」），编辑器字号只管编辑器和终端正文。
    /// 以前只有后面那一个旋钮，所以「字号调了，只有编辑器动，别处纹丝不动」。
    ///
    /// 字号拖动只写 `pending_*`，点「应用」才提交：改界面字号要重铺字体图集并让整页重排，
    /// 拖动过程中每一帧都排一次，滑块自己也会被挪走，看起来就是整片在闪。
    /// 主题和自动换行不动字形度量，仍然即时生效。
    fn settings_appearance(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        setting_head(ui, "界面字号");
        muted_line(ui, "顶栏、侧栏、页签、结果区和各个窗口里的字一起跟着这一档走。");
        ui.horizontal(|ui| {
            let slider = egui::Slider::new(
                &mut self.pending_ui_font,
                theme::UI_FONT_MIN..=theme::UI_FONT_MAX,
            )
            .step_by(0.5)
            .text("点");
            ui.add(slider);
            if ui.button("恢复默认").clicked() {
                self.pending_ui_font = theme::UI_FONT_DEFAULT;
            }
        });

        ui.add_space(12.0);
        setting_head(ui, "编辑器字号");
        muted_line(ui, "只管编辑器正文和终端正文；Ctrl+= 与 Ctrl+- 调的是这一档。");
        ui.horizontal(|ui| {
            let slider = egui::Slider::new(
                &mut self.pending_font,
                theme::EDITOR_FONT_MIN..=theme::EDITOR_FONT_MAX,
            )
            .step_by(1.0)
            .text("点");
            ui.add(slider);
            if ui.button("恢复默认").clicked() {
                self.pending_font = theme::EDITOR_FONT_DEFAULT;
            }
        });

        ui.add_space(8.0);
        let dirty = self.pending_ui_font != self.ui_font || self.pending_font != self.font_size;
        ui.horizontal(|ui| {
            let apply = ui
                .add_enabled(
                    dirty,
                    egui::Button::new(RichText::new("应用").size(theme::md(ui))),
                )
                .on_hover_cursor(theme::HAND);
            if apply.clicked() {
                let (next_ui, next_editor) = (self.pending_ui_font, self.pending_font);
                self.set_ui_font_size(next_ui, &ctx);
                self.set_font_size(next_editor, &ctx);
                self.status = format!(
                    "字号已应用：界面 {next_ui:.1} 点，编辑器 {next_editor:.1} 点"
                );
            }
            if dirty
                && ui
                    .button("放弃改动")
                    .on_hover_cursor(theme::HAND)
                    .clicked()
            {
                self.pending_ui_font = self.ui_font;
                self.pending_font = self.font_size;
            }
        });
        muted_line(
            ui,
            if dirty {
                format!(
                    "有未应用的改动：界面 {} → {}，编辑器 {} → {}。",
                    self.ui_font, self.pending_ui_font, self.font_size, self.pending_font
                )
            } else {
                "拖动滑块界面不会跟着动，点「应用」才换字号。".to_string()
            },
        );

        ui.add_space(12.0);
        setting_head(ui, "主题");
        ui.horizontal(|ui| {
            for (dark, label) in [(false, "浅色"), (true, "深色")] {
                if ui
                    .selectable_label(self.dark == dark, label)
                    .on_hover_cursor(theme::HAND)
                    .clicked()
                {
                    self.set_theme(dark, &ctx);
                }
            }
        });

        ui.add_space(12.0);
        setting_head(ui, "长行");
        let _ = ui.checkbox(&mut self.word_wrap, "自动换行");
        muted_line(ui, "开着时长行折进面板，关掉时长行横向滚出去。只改显示，不动文件内容。");
    }

    /// 「系统」分区：与 Windows 的集成，外加配置文件本身在哪儿。
    fn settings_system(&mut self, ui: &mut Ui) {
        setting_head(ui, ".txt 文件关联");
        let mut on = self.config.data.txt_association;
        if ui
            .checkbox(&mut on, "启动时把自己列进 .txt 的「打开方式」")
            .on_hover_cursor(theme::HAND)
            .changed()
        {
            self.config.data.txt_association = on;
            self.config.touch();
            self.status = if on {
                "下次启动会重新登记进「打开方式」"
            } else {
                "下次启动不再登记；已经设好的默认应用不会被改回去"
            }
            .to_string();
        }
        muted_line(
            ui,
            "登记只写当前用户的注册表（HKCU\\Software\\Classes），不动任何文件类型的默认处理器。",
        );
        muted_line(
            ui,
            "这个开关只管往后还登记不登记；要撤掉已经登记的条目，得跑 assets\\build\\unregister-txt.ps1。",
        );

        ui.add_space(12.0);
        setting_head(ui, "配置文件");
        muted_line(ui, config::Config::path().display().to_string());
        if ui
            .button("打开所在文件夹")
            .on_hover_cursor(theme::HAND)
            .clicked()
        {
            crate::filetree::reveal(&config::Config::path());
        }
        muted_line(ui, "界面上的改动即时生效并写回这个文件；手工编辑过的内容要下次启动才读进来。");
    }

    /// 「数据库」分区：连接管理的入口，外加「哪条语句发到哪条连接」的交代。
    ///
    /// 连接本身的新建、编辑、删除还在那个窗口里做——那里的表单比这一栏宽敞。
    fn settings_database(&mut self, ui: &mut Ui) {
        setting_head(ui, "连接管理");
        let n = self.database.connections().len();
        let active = self
            .database
            .active_conn()
            .unwrap_or("（还没有连接）")
            .to_string();
        muted_line(ui, format!("已保存 {n} 条连接，默认走「{active}」。"));
        if ui
            .button("打开连接管理")
            .on_hover_cursor(theme::HAND)
            .clicked()
        {
            self.database.open_manager();
        }

        ui.add_space(12.0);
        setting_head(ui, "查询送到哪条连接");
        muted_line(ui, "SQL 块头部写 conn=连接名 就送那条连接；没写就走默认连接。");
        muted_line(ui, "不同连接各跑各的：自动执行闸按连接分开，一条在等回包不挡另一条。");
    }

    /// 「终端」分区：选 shell、改它的路径和启动参数，或者自己加一条，改动直接进 config.json。
    ///
    /// 装在非标准目录的 Git Bash（`which` 和内置候选都摸不到）靠路径覆盖才起得来；
    /// 内置那五种之外的 shell（装在别处的 MSYS2、busybox……）靠底下的「新增自定义」。
    fn settings_terminal(&mut self, ui: &mut Ui) {
        ui.label(
            RichText::new("新建终端用哪种 shell")
                .size(crate::theme::sm(ui))
                .weak(),
        );
        // 顶栏那个下拉是同一个值，两处点的都是「下次新建终端用谁」，不会打架
        egui::ComboBox::from_id_salt("shell_picker_settings")
            .selected_text(self.terminal.shell.label())
            .show_ui(ui, |ui| {
                for shell in self.terminal.available.clone() {
                    if ui
                        .selectable_label(self.terminal.shell == shell, shell.label())
                        .clicked()
                    {
                        self.terminal.shell = shell;
                    }
                }
            });
        ui.separator();

        // 内置的和自定义的填法不一样：前者填的是「覆盖」，后者填的就是那一条本身
        let shell = self.terminal.shell.clone();
        match shell {
            Shell::Builtin(kind) => self.shell_fields_builtin(ui, kind),
            Shell::Custom(c) => self.shell_fields_custom(ui, c),
        }

        ui.add_space(10.0);
        ui.separator();
        if ui
            .button("+ 新增自定义 shell")
            .on_hover_cursor(theme::HAND)
            .on_hover_text("给装在别处的 shell 加一条，加完就在上面的下拉里，填好路径即可新建终端")
            .clicked()
        {
            self.terminal.add_custom();
            self.status = "新加了一条自定义 shell，填上程序路径就能用".to_string();
        }
        muted_line(
            ui,
            format!("改动即时生效，并写进 {}", config::Config::path().display()),
        );
    }

    /// 内置 shell：程序路径与启动参数的**覆盖项**，留空回到自动探测 / 内置参数。
    fn shell_fields_builtin(&mut self, ui: &mut Ui, kind: ShellKind) {
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
                .size(crate::theme::md(ui))
                .strong(),
        );
        ui.separator();

        field_label(ui, "程序路径（留空 = 自动探测）");
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
        field_label(ui, "启动参数（留空 = 内置参数）");
        let a_resp = ui.add(
            egui::TextEdit::singleline(&mut args)
                .hint_text("例如：-NoProfile -NoLogo")
                .desired_width(f32::INFINITY),
        );
        muted_line(ui, "按空格拆分，带空格的参数用双引号包住。");

        ui.add_space(6.0);
        let mut cleared = false;
        if ui.button("清除覆盖").clicked() {
            program.clear();
            args.clear();
            cleared = true;
        }

        if p_resp.changed() || a_resp.changed() || cleared {
            self.terminal
                .settings
                .programs
                .insert(id.clone(), program.trim().to_string());
            self.terminal
                .settings
                .args
                .insert(id, args.trim().to_string());
            // 路径可能让某个种类从「没有」变成「可用」，可用列表要跟着重算
            self.terminal.refresh();
        }
    }

    /// 自定义 shell：名字、路径、参数都是**这一条自己的**内容，没有「回到默认」可言。
    fn shell_fields_custom(&mut self, ui: &mut Ui, shell: CustomShell) {
        let id = shell.id.clone();
        let mut name = shell.name;
        let mut program = shell.program;
        let mut args = shell.args;

        ui.label(
            RichText::new(format!("自定义 shell（{id}）"))
                .size(crate::theme::md(ui))
                .strong(),
        );
        ui.separator();

        field_label(ui, "显示名称");
        let n_resp = ui.add(
            egui::TextEdit::singleline(&mut name)
                .hint_text("例如：MSYS2 Bash")
                .desired_width(f32::INFINITY),
        );

        ui.add_space(6.0);
        field_label(ui, "程序路径");
        let p_resp = ui.add(
            egui::TextEdit::singleline(&mut program)
                .hint_text(r"D:\Softwares\msys64\usr\bin\bash.exe")
                .desired_width(f32::INFINITY),
        );
        let path = program.trim().to_string();
        if path.is_empty() {
            muted_line(ui, "路径还空着，这时候新建终端选了它会起不来。");
        } else if !Path::new(&path).is_file() {
            ui.colored_label(ui.visuals().error_fg_color, "这个路径现在还不存在");
        } else {
            muted_line(ui, format!("启动的就是这个：{path}"));
        }

        ui.add_space(6.0);
        field_label(ui, "启动参数（留空 = 不带参数）");
        let a_resp = ui.add(
            egui::TextEdit::singleline(&mut args)
                .hint_text("例如：-i -l")
                .desired_width(f32::INFINITY),
        );
        muted_line(ui, "按空格拆分，带空格的参数用双引号包住。");

        ui.add_space(6.0);
        if n_resp.changed() || p_resp.changed() || a_resp.changed() {
            self.terminal
                .update_custom(&id, &name, &program, &args);
        }
        if ui
            .button("删除这条")
            .on_hover_cursor(theme::HAND)
            .on_hover_text("只删这条自定义记录，那个程序本身不动")
            .clicked()
        {
            self.terminal.remove_custom(&id);
            self.status = "已删除这条自定义 shell".to_string();
        }
    }

    /// 「快捷键」分区：整表都能重录。
    ///
    /// 录制中那一帧的按键不在这里抓——关掉窗口时也要把录制收掉，
    /// 所以抓取放在窗口壳里，这里只画和响应点击。
    fn settings_keymap(&mut self, ui: &mut Ui) {
        let warn = Color32::from_rgb(200, 62, 62);
        ui.horizontal(|ui| {
            if ui.button(RichText::new("全部恢复默认").size(crate::theme::md(ui))).clicked() {
                self.keymap.reset_all();
                self.recording = None;
            }
            ui.label(
                RichText::new("点组合键即可重新录制 · Esc 取消 · Backspace 解绑")
                    .size(crate::theme::xs(ui))
                    .weak(),
            );
        });
        ui.separator();

        // 列表铺满分区给的高度：分区在窗口正中，窗口能拖大，这里就不该再钉一个 430
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .id_salt("keymap_rows")
            .show(ui, |ui| {
                // 两列宽和行高照着字号量。写死 132／148×20 是界面字号 13.5
                // 那一档量出来的，字号调到 20 就会把长标签和长组合键切掉。
                let md = crate::theme::md(ui);
                let name_w = widest_label(
                    ui,
                    keymap::ACTIONS.iter().map(|d| d.label),
                    egui::FontId::proportional(md),
                ) + 12.0;
                let key_w = widest_label(
                    ui,
                    keymap::ACTIONS.iter().map(|d| d.default),
                    egui::FontId::monospace(md),
                ) + 28.0;
                let row_h = ui.text_style_height(&egui::TextStyle::Body) + 3.0;
                let mut group = "";
                for def in keymap::ACTIONS {
                    if def.group != group {
                        if !group.is_empty() {
                            ui.add_space(8.0);
                        }
                        ui.label(
                            RichText::new(def.group).size(crate::theme::sm(ui)).strong().weak(),
                        );
                        group = def.group;
                    }

                    ui.horizontal(|ui| {
                        ui.add_sized(
                            [name_w, row_h],
                            egui::Label::new(RichText::new(def.label).size(crate::theme::md(ui))),
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
                        let mut rich = RichText::new(text).size(crate::theme::md(ui)).monospace();
                        if conflict.is_some() {
                            rich = rich.color(warn);
                        }
                        if ui
                            .add_sized([key_w, row_h], egui::Button::new(rich))
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
                                    .size(crate::theme::xs(ui))
                                    .color(warn),
                            );
                        }

                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                if !self.keymap.is_default(def.id)
                                    && ui
                                        .small_button(RichText::new("重置").size(crate::theme::sm(ui)))
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
            .size(crate::theme::xs(ui))
            .weak(),
        );
    }
}

impl EasyEditorApp {
    /// 一帧的全部 UI。单独拆出来是因为 `eframe::Frame` 在测试里造不出来，
    /// 拆开后无窗口测试也能驱动一遍真实布局。
    fn draw(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        self.frame_count += 1;

        self.search.poll();
        self.database.poll();
        // 闲置够久的连接在这里放掉，卡住的「正在连接」也在这里翻成「连不上」
        self.database.tick(std::time::Instant::now());
        // SQL 块的回执不归侧栏管，转手给底部结果区
        for event in self.database.take_stray() {
            self.results.on_event(&event);
        }
        self.paint_failures();
        // 一整批都到家了才回写块状态：半途写会把还在跑的块说成已经跑完
        for done in self.results.settled() {
            self.mark_block(&done);
        }
        if self.database.busy() || self.results.running() {
            // 库表列表和测试结果都在后台线程，不催一帧就永远停在「读取中…」
            ctx.request_repaint_after(std::time::Duration::from_millis(60));
        } else if self.database.has_link() {
            // 挂着会话时按半分钟醒一次：闲置回收晚一圈没有代价，每帧重画有
            ctx.request_repaint_after(std::time::Duration::from_secs(30));
        }
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
        if self.frame_count == 20 && crate::config::env_var("VERIFY").as_deref() == Some("insert") {
            self.insert_journal_block();
        }
        // 调试用：EASY_EDITOR_VERIFY=replace 时自动执行全部替换
        if self.frame_count == 30 && crate::config::env_var("VERIFY").as_deref() == Some("replace")
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
        if self.want_open_files {
            self.want_open_files = false;
            self.open_files_dialog();
        }

        // 拖拽：`dropped_files` 每帧从 winit 那边一次性交过来，这一帧不处理就没了；
        // `hovered_files` 反过来会一直留着，只用来在松手前给一句「放开就打开」。
        let (hovered, dropped) = ctx.input(|i| {
            let hovered = i
                .raw
                .hovered_files
                .iter()
                .filter_map(|f| f.path.as_ref())
                .map(|p| {
                    p.file_name()
                        .unwrap_or(p.as_os_str())
                        .to_string_lossy()
                        .into_owned()
                })
                .collect::<Vec<_>>();
            let dropped = i
                .raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect::<Vec<_>>();
            (hovered, dropped)
        });
        if !dropped.is_empty() {
            self.open_dropped(dropped);
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
        if self.results.open {
            self.results_panel(ui);
        }
        if let Some((path, byte)) = self.results.pending_jump.take() {
            self.jump_to_byte(&path, byte);
        }
        if let Some(export) = self.results.take_export() {
            self.save_csv(export);
        }
        self.side_panel(ui);
        self.central(ui);
        // 面板树是焦点的事实来源，画完（可能点过、拆过、关过）再刷一次镜像
        self.sync_active();

        self.quick_open_window(&ctx);
        self.close_confirm(&ctx);
        self.settings_window(&ctx);
        self.database.manager_window(&ctx);
        self.results.show_popup(&ctx);
        // 排最后：这是开局的告警，盖在其他浮窗上面才不会被当成普通窗口埋掉
        self.config_trouble_window(&ctx);

        // 编辑器在 central() 里才会把最新光标写回 OpenFile，所以放在最后收
        self.poll_caret(&ctx);
        // 自动执行要看的是这一帧最新的文本，判完才知道要不要接着排闹钟
        self.auto_tick(std::time::Instant::now());
        if self.auto_due.is_some() {
            ctx.request_repaint_after(AUTO_DEBOUNCE);
        }

        if self.search.busy {
            ctx.request_repaint_after(std::time::Duration::from_millis(120));
        }
        // 有终端活着就要持续拉输出；大约 30fps，够跟上手速也不至于空转
        if !self.terminal.is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_millis(33));
        }

        // 悬停提示压在最上面：拖到编辑器或侧栏上时也得看得见，不然用户不知道松手有没有用。
        if !hovered.is_empty() {
            let head = if hovered.len() == 1 {
                format!("松开打开 {}", hovered[0])
            } else {
                format!("松开打开 {} 项", hovered.len())
            };
            egui::Area::new(egui::Id::unique("drop-hint"))
                .order(egui::Order::Foreground)
                .movable(false)
                .interactable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, -60.0])
                .show(&ctx, |ui| {
                    egui::Frame::new()
                        .fill(ui.visuals().window_fill())
                        .shadow(ui.visuals().window_shadow)
                        .corner_radius(8.0)
                        .inner_margin(egui::Margin::symmetric(14, 10))
                        .show(ui, |ui| {
                            ui.vertical_centered(|ui| {
                                ui.label(RichText::new(head).size(crate::theme::md(ui)).strong());
                                ui.label(
                                    RichText::new("文件夹换到工作区，文件进标签条")
                                        .size(crate::theme::sm(ui))
                                        .weak(),
                                );
                            });
                        });
                });
        }

        // 输入法锚点要最后处理：前面画完才知道这一帧谁拿到了锚点
        ime::hide_ime_anchor_while_pointer_held(&ctx);

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
        // 后台 shell 不会因为窗口关了自己退出，得挨个收掉（含它们起的子进程）
        self.terminal.kill_all();
    }
}

/// 设置页每条的小标题。
fn setting_head(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).size(crate::theme::md(ui)).strong());
}

/// 输入框上面那一行的字段名。
fn field_label(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).size(crate::theme::sm(ui)).weak());
}

/// 一行次要说明：字号走小字档，太长就截断，完整内容挂悬浮。
fn muted_line(ui: &mut Ui, text: impl Into<String>) {
    let text = text.into();
    let resp = ui.add(
        egui::Label::new(RichText::new(&text).size(crate::theme::sm(ui)).weak())
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

/// 空白面板的内容。`full` 是「整屏就这一块面板且一个文件都没开」，那种情况才显示欢迎页。
///
/// 欢迎页只说「这是什么、怎么开始」，不摆按钮：`打开文件夹` / `打开文件` 顶栏已经有了，
/// 这里再排一颗等于把同一件事画两遍。
fn pane_welcome(ui: &mut Ui, full: bool, hints: &str, hint: &dyn Fn(&str) -> String) {
    if !full {
        ui.add_space(16.0);
        ui.vertical_centered(|ui| {
            ui.label(
                RichText::new(format!(
                    "这块面板还没有文件 · {} 打开，点标签也会送到这里",
                    hint("quick_open")
                ))
                .size(crate::theme::md(ui))
                .weak(),
            );
        });
        return;
    }
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() * 0.22);
        ui.label(RichText::new("EasyEditor").size(crate::theme::display(ui)).strong());
        ui.label(
            RichText::new("小而好用的编辑器 · Rust + egui")
                .size(crate::theme::md(ui))
                .weak(),
        );
        ui.add_space(18.0);
        ui.label(RichText::new(hints).size(crate::theme::sm(ui)).weak());
    });
}

/// 一批文案里最宽的那条有多长，照给定字号实量。
///
/// 快捷键表的两列用它定宽，列宽才是跟着界面字号长的；写死数字的话，
/// 字号一调大，长标签和长组合键就会被切掉。
fn widest_label(
    ui: &egui::Ui,
    texts: impl Iterator<Item = &'static str>,
    font: egui::FontId,
) -> f32 {
    let painter = ui.painter();
    texts
        .map(|t| {
            painter
                .layout_no_wrap(t.to_owned(), font.clone(), egui::Color32::PLACEHOLDER)
                .size()
                .x
        })
        .fold(0.0f32, f32::max)
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
        frame_raw(
            app,
            ctx,
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)),
                events,
                ..Default::default()
            },
        )
    }

    /// 直接喂一份 `RawInput` 跑一帧。拖放这类不走 `events` 的输入只能从这里进。
    fn frame_raw(
        app: &mut EasyEditorApp,
        ctx: &egui::Context,
        raw: egui::RawInput,
    ) -> egui::FullOutput {
        let mut out = ctx.run_pass(raw, |ctx| {
            ctx.root_ui(|ui| app.draw(ui));
        });
        // 没有真的渲染器消费字体图集的增量，epaint 会在 drop 时断言
        out.textures_delta.clear();
        out
    }

    /// `RawInput::dropped_files` 里那只句柄：winit 给的是真实文件，测试里只要路径。
    #[derive(Debug)]
    struct Dropped(PathBuf);

    impl egui::DroppedFile for Dropped {
        fn path(&self) -> &std::path::Path {
            &self.0
        }

        fn bytes(&self) -> Result<Vec<u8>, String> {
            std::fs::read(&self.0).map_err(|e| e.to_string())
        }
    }

    /// 拼一帧「用户把这几样拖到窗口上并松了手」。
    fn drop_raw(paths: &[PathBuf]) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, SCREEN)),
            dropped_files: paths
                .iter()
                .map(|p| std::sync::Arc::new(Dropped(p.clone())) as egui::DroppedFileHandle)
                .collect(),
            ..Default::default()
        }
    }

    /// 拼一帧「拖进来的东西还悬在窗口上方，没松手」。
    fn hover_raw(paths: &[PathBuf]) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, SCREEN)),
            hovered_files: paths
                .iter()
                .map(|p| egui::HoveredFile {
                    path: Some(p.clone()),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
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

    fn wait_for_text(app: &mut EasyEditorApp, ctx: &egui::Context, needle: &str) -> Option<Rect> {
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

        frame(
            &mut app,
            &ctx,
            pointer(row, egui::PointerButton::Secondary, true),
        );
        frame(
            &mut app,
            &ctx,
            pointer(row, egui::PointerButton::Secondary, false),
        );
        let item = wait_for_text(&mut app, &ctx, "新建文件")
            .expect("右键没有弹出「新建文件」菜单")
            .center();

        frame(
            &mut app,
            &ctx,
            pointer(item, egui::PointerButton::Primary, true),
        );
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

        frame(&mut app, &ctx, vec![egui::Event::Text("probe.txt".into())]);
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

    /// 右键文件树某个条目 → 等菜单出现 → 点它。
    fn click_tree_menu(app: &mut EasyEditorApp, ctx: &egui::Context, at: Pos2, label: &str) {
        frame(app, ctx, pointer(at, egui::PointerButton::Secondary, true));
        frame(app, ctx, pointer(at, egui::PointerButton::Secondary, false));
        let item = wait_for_text(app, ctx, label)
            .unwrap_or_else(|| panic!("右键没有弹出「{label}」菜单"))
            .center();
        frame(app, ctx, pointer(item, egui::PointerButton::Primary, true));
        frame(app, ctx, pointer(item, egui::PointerButton::Primary, false));
    }

    /// 右键「重命名」：改名后已打开的标签跟着换路径，保存落到新文件。
    ///
    /// 只单测 FileTree 复现不了标签页这一半：磁盘换名和 `tabs` 里的路径是两处状态，
    /// 漏掉任何一处都会表现为「存盘又生出一个旧名字的副本」。
    #[test]
    fn renaming_in_the_tree_follows_the_open_tab() {
        let (ctx, mut app, dir) = app_in_folder("easy-editor-app-rename-test", "hello\n");
        let old = dir.join("a.txt");
        app.open_file(old.clone(), None);
        assert_eq!(app.tabs.len(), 1);

        let out = frame(&mut app, &ctx, vec![]);
        let row = find_text(&out, "a.txt")
            .expect("文件树里没画出 a.txt")
            .center();
        click_tree_menu(&mut app, &ctx, row, "重命名");
        // 改名框要到下一帧才画出来并拿到焦点，这一帧敲键会丢
        let out = frame(&mut app, &ctx, vec![]);
        assert!(find_text(&out, "a.txt").is_some(), "改名框没在原位画出来");

        // 改名框预填了当前名字：Ctrl+A 全选，再敲新名字整段替换，最后回车落地
        frame(&mut app, &ctx, vec![key_press(Key::A, Modifiers::COMMAND)]);
        frame(&mut app, &ctx, vec![egui::Event::Text("b.txt".into())]);
        frame(&mut app, &ctx, vec![key_press(Key::Enter, Modifiers::NONE)]);

        let new = dir.join("b.txt");
        assert!(new.is_file(), "改名没落到磁盘上");
        assert!(!old.exists(), "旧名字还留在磁盘上");
        assert_eq!(app.tabs[0].path, new, "标签页还指着旧路径");
        assert!(
            app.index.contains(&new) && !app.index.contains(&old),
            "快速打开索引没跟着改名"
        );

        // 未保存的改动要写进新名字的文件里
        app.tabs[0].text = "changed\n".into();
        app.tabs[0].refresh_dirty();
        app.save_active();
        assert_eq!(
            std::fs::read_to_string(&new).unwrap(),
            "changed\n",
            "保存写到了别的地方"
        );
        assert!(!old.exists(), "保存又生成了旧名字的副本");
    }

    /// 把 shell 钉死成 cmd.exe：`bootstrap` 读的是真实用户的 config.json，
    /// 那里面手填的 WSL / Git Bash 路径换台机器就不存在，会让终端测试假红。
    #[cfg(windows)]
    fn force_cmd_shell(app: &mut EasyEditorApp) {
        app.terminal.settings = TerminalSettings::default();
        app.terminal.shell = Shell::Builtin(crate::terminal::ShellKind::Cmd);
    }

    /// 编辑树里现在还有没有终端格子。
    fn has_terminal_pane(app: &EasyEditorApp) -> bool {
        app.panes
            .pane_ids()
            .into_iter()
            .filter_map(|id| app.panes.payload_of(id).copied())
            .any(|kind| kind.terminal_id().is_some())
    }

    /// 页签条上那一排「>_ ××」画了几处：每格页签一处，加上正在显示的那组里每格面板一处。
    fn painted_term_labels(out: &egui::FullOutput, label: &str) -> usize {
        all_texts(out)
            .into_iter()
            .filter(|(t, _)| t == label)
            .count()
    }

    /// 右键「在此处打开终端」：在这个目录里新开一个终端页签。
    ///
    /// 编辑那棵树一个格子都不许多——用户嫌的就是终端和文件挤在同一个大窗口里，
    /// 现在终端走页签，中央区一次只画翻在前面那一组。
    #[cfg(windows)]
    #[test]
    fn open_terminal_menu_opens_a_terminal_tab() {
        let (ctx, mut app, dir) = app_in_folder("easy-editor-app-open-term-test", "a\n");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        app.tree.as_mut().unwrap().expanded.insert(dir.join("sub"));
        force_cmd_shell(&mut app);

        let shells = app.terminal.len();
        let tabs = app.term_tabs.len();
        let out = frame(&mut app, &ctx, vec![]);
        let row = find_text(&out, "sub").expect("文件树里没画出 sub").center();
        click_tree_menu(&mut app, &ctx, row, "在此处打开终端");
        // 页签和终端计数都要走完一帧才看得清
        frame(&mut app, &ctx, vec![]);

        assert_eq!(app.term_tabs.len(), tabs + 1, "没有多开一个终端页签");
        assert_eq!(app.terminal.len(), shells + 1, "没有多起一条 shell");
        assert!(!has_terminal_pane(&app), "终端又混回编辑树里了");
        let tab = app.term_tabs.last().unwrap();
        assert_eq!(app.term_active, Some(tab.id), "新页签没翻到前面");
        assert_eq!(tab.tree.count(), 1, "新起的一个页签里不止一格");
        let spawned = tab.term_ids();
        assert_eq!(spawned.len(), 1);
        assert_eq!(
            app.terminal.get(spawned[0]).map(|t| t.cwd.as_path()),
            Some(dir.join("sub").as_path()),
            "终端没开在右键的那个目录里"
        );

        // 收尾：杀掉这条测试起的 shell，别把进程留在机器上
        for t in spawned {
            app.terminal.remove(t);
        }
    }

    /// 页签上的名字跟着这组第一条还活着的 shell 走；shell 没了要说清楚，别留个空名字。
    #[test]
    fn terminal_tab_title_follows_its_first_shell() {
        let (_ctx, app, _dir) = app_in_folder("easy-editor-app-term-title-test", "a\n");
        let tab = TermTab::new(1, 7);
        assert_eq!(
            tab.title(&app.terminal),
            "已退出",
            "没起进程的终端页签该说「已退出」"
        );
    }

    /// 点几次「新建终端」就有几个页签，每个装一条 shell，编辑区不受影响；
    /// 页签条上两格名字都画得出来，终端正文只画翻在前面那一组。
    #[cfg(windows)]
    #[test]
    fn each_new_terminal_gets_its_own_tab() {
        let (ctx, mut app, _dir) = app_in_folder("easy-editor-app-term-tab-test", "a\n");
        force_cmd_shell(&mut app);
        let main_panes = app.panes.count();

        app.new_terminal();
        frame(&mut app, &ctx, vec![]);
        app.new_terminal();
        let out = frame(&mut app, &ctx, vec![]);

        assert_eq!(app.term_tabs.len(), 2, "两条 shell 该有两个页签");
        assert_eq!(app.terminal.len(), 2, "两条 shell 没各自起进程");
        assert!(
            app.term_tabs.iter().all(|t| t.tree.count() == 1),
            "新建终端不该顺手把已有页签拆开"
        );
        let ids: Vec<u64> = app.term_tabs.iter().map(|t| t.id).collect();
        assert_ne!(ids[0], ids[1], "两个页签共用了序号");
        assert_eq!(app.term_active, Some(ids[1]), "最后新建的页签没翻到前面");
        let terms: Vec<u64> = app.term_tabs.iter().flat_map(|t| t.term_ids()).collect();
        assert_eq!(terms.len(), 2);
        assert_ne!(terms[0], terms[1], "两个页签里挂的是同一条 shell");
        assert_eq!(app.panes.count(), main_panes, "编辑树跟着长了格子");

        // 「>_ ××」出现三处 = 两个页签 + 在前那组的一格面板。第四处就说明没在前的
        // 那组也被画进了中央区
        let label = format!(">_ {}", app.term_tabs[1].title(&app.terminal));
        assert_eq!(
            painted_term_labels(&out, &label),
            3,
            "页签条或终端网格画多了"
        );

        for t in terms {
            app.terminal.remove(t);
        }
    }

    /// 页签里再切一格：格子 +1、shell +1、页签数不变（tmux 那套分屏），
    /// 页签上还要标出这组有几格。
    #[cfg(windows)]
    #[test]
    fn splitting_a_terminal_tab_adds_a_cell_not_a_tab() {
        let (ctx, mut app, _dir) = app_in_folder("easy-editor-app-term-split-test", "a\n");
        force_cmd_shell(&mut app);
        app.new_terminal();
        frame(&mut app, &ctx, vec![]);

        let tab_id = app.term_tabs[0].id;
        let cell = app.term_tabs[0].tree.focus().expect("页签里没有可拆的格子");
        let first = app.term_tabs[0].term_ids()[0];
        let first_cwd = app.terminal.get(first).map(|t| t.cwd.clone());
        let shells = app.terminal.len();

        // 拆分按钮和快捷键最后都打到这里，直接喂命令，省得等坐标
        app.pane_cmd(PaneScope::Term(tab_id), PaneCmd::Split(cell, Axis::Vertical));
        let out = frame(&mut app, &ctx, vec![]);

        assert_eq!(app.term_tabs.len(), 1, "拆分不该另开一个页签");
        assert_eq!(app.term_tabs[0].tree.count(), 2, "页签里没多出一格");
        assert_eq!(app.terminal.len(), shells + 1, "新的一格没起自己的 shell");
        assert_eq!(app.status, "已在本组新建一格终端");
        let ids = app.term_tabs[0].term_ids();
        assert_eq!(ids.len(), 2);
        let new = *ids.iter().find(|t| **t != first).expect("树里找不到新建的那条 shell");
        assert_eq!(
            app.terminal.get(new).map(|t| t.cwd.clone()),
            first_cwd,
            "新的一格没有跟着上一格的目录走"
        );
        let label = format!(">_ {} ×2", app.term_tabs[0].title(&app.terminal));
        assert!(
            find_text(&out, &label).is_some(),
            "页签上没标出这组有两格"
        );

        for t in ids {
            app.terminal.remove(t);
        }
    }

    /// `Ctrl+``（顶栏「终端」同一条路）在编辑区和终端页签之间来回切：切走时页签还
    /// 排在页签条上、shell 照跑、正文不再画；切回来还是同一条 shell。
    #[cfg(windows)]
    #[test]
    fn terminal_toggle_switches_between_editor_and_tab() {
        let (ctx, mut app, _dir) = app_in_folder("easy-editor-app-term-toggle-test", "a\n");
        force_cmd_shell(&mut app);
        app.new_terminal();
        frame(&mut app, &ctx, vec![]);

        let tab_id = app.term_tabs[0].id;
        let term_id = app.term_tabs[0].term_ids()[0];
        let label = format!(">_ {}", app.terminal.pane_title(term_id).unwrap_or_default());

        // 在前：页签一处 + 格子的标题条一处
        let out = frame(&mut app, &ctx, vec![]);
        assert_eq!(painted_term_labels(&out, &label), 2, "终端页签的格子没画出来");

        app.focus_or_new_terminal();
        let out = frame(&mut app, &ctx, vec![]);
        assert_eq!(app.term_active, None, "终端还占着中央区");
        assert_eq!(app.term_tabs.len(), 1, "切走时把页签删了");
        assert_eq!(app.term_tabs[0].term_ids(), vec![term_id]);
        assert!(app.terminal.get(term_id).is_some(), "切回编辑区把 PTY 杀了");
        assert_eq!(
            painted_term_labels(&out, &label),
            1,
            "只剩页签那一处才对，中央区还在画终端正文"
        );

        app.focus_or_new_terminal();
        frame(&mut app, &ctx, vec![]);
        assert_eq!(app.term_active, Some(tab_id), "没有翻回终端页签");
        assert_eq!(app.term_tabs.len(), 1, "翻回来时又新建了一个");
        assert_eq!(app.status, "已唤回终端页签");

        app.terminal.remove(term_id);
    }

    /// 格子里的 × 是真杀进程；关掉最后一格，这个页签跟着关掉，中央区回编辑区。
    #[cfg(windows)]
    #[test]
    fn closing_the_last_cell_closes_the_tab() {
        let (ctx, mut app, _dir) = app_in_folder("easy-editor-app-term-close-test", "a\n");
        force_cmd_shell(&mut app);
        app.new_terminal();
        frame(&mut app, &ctx, vec![]);

        let tab_id = app.term_tabs[0].id;
        let cell = app.term_tabs[0].tree.focus().unwrap();
        app.pane_cmd(PaneScope::Term(tab_id), PaneCmd::Split(cell, Axis::Horizontal));
        frame(&mut app, &ctx, vec![]);
        let ids = app.term_tabs[0].term_ids();
        assert_eq!(ids.len(), 2);

        // 先关一格：还剩一格的页签留在原地，被关掉那条 shell 立刻没命
        app.pane_cmd(
            PaneScope::Term(tab_id),
            PaneCmd::Close(app.term_tabs[0].tree.focus().unwrap()),
        );
        frame(&mut app, &ctx, vec![]);
        assert_eq!(app.term_tabs.len(), 1, "还剩一格就把页签摘了");
        assert_eq!(app.term_tabs[0].tree.count(), 1, "格子没少");
        assert_eq!(app.terminal.len(), 1, "关掉的那格 shell 还活着");
        assert_eq!(app.term_active, Some(tab_id), "在前那组被改动了却丢了选中");

        // 再关最后一格：页签从排里消失，中央区该回编辑区
        let last = app.term_tabs[0].tree.focus().unwrap();
        app.pane_cmd(PaneScope::Term(tab_id), PaneCmd::Close(last));
        frame(&mut app, &ctx, vec![]);
        assert!(app.term_tabs.is_empty(), "最后一格关掉了页签还挂着");
        assert_eq!(app.terminal.len(), 0, "会话没收干净");
        assert_eq!(app.term_active, None, "页签全关了还没回编辑区");
        assert!(!has_terminal_pane(&app), "收掉的终端又冒回编辑树");
    }

    /// 页签上的 × 一次收掉这一组的全部 shell；在前那组被收掉就翻到剩下的那组，
    /// 全收完了才回编辑区。
    #[cfg(windows)]
    #[test]
    fn closing_a_terminal_tab_kills_its_whole_group() {
        let (ctx, mut app, _dir) = app_in_folder("easy-editor-app-term-kill-test", "a\n");
        force_cmd_shell(&mut app);
        app.new_terminal();
        frame(&mut app, &ctx, vec![]);
        app.new_terminal();
        frame(&mut app, &ctx, vec![]);

        let first = app.term_tabs[0].id;
        let second = app.term_tabs[1].id;
        // 给第一组再来一格，这样收掉一个页签要杀两条 shell
        let cell = app.term_tabs[0].tree.focus().unwrap();
        app.pane_cmd(PaneScope::Term(first), PaneCmd::Split(cell, Axis::Horizontal));
        frame(&mut app, &ctx, vec![]);
        assert_eq!(app.terminal.len(), 3);

        // 收掉正翻在前的那组
        app.close_term_tab(second);
        frame(&mut app, &ctx, vec![]);
        assert_eq!(app.term_tabs.len(), 1, "页签没从排里摘掉");
        assert_eq!(app.term_active, Some(first), "没翻到剩下的那组");
        assert_eq!(app.terminal.len(), 2, "收掉的页签没把 shell 杀干净");

        app.close_term_tab(first);
        frame(&mut app, &ctx, vec![]);
        assert!(app.term_tabs.is_empty(), "最后一个页签没关掉");
        assert_eq!(app.terminal.len(), 0, "整组 shell 没杀干净");
        assert_eq!(app.term_active, None, "页签全关了还没回编辑区");
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
        frame(app, ctx, pointer(item, egui::PointerButton::Primary, false));
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
        frame(
            &mut app,
            &ctx,
            pointer(spot, egui::PointerButton::Primary, true),
        );
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
                egui::Shape::Text(t) => {
                    (pred)(&t.galley.job.text).then(|| t.galley.rect.translate(t.pos.to_vec2()))
                }
                egui::Shape::Vec(shapes) => shapes.iter().find_map(|s| walk(s, pred)),
                _ => None,
            }
        }
        out.shapes.iter().find_map(|c| walk(&c.shape, &pred))
    }

    /// 这一笔绘制是不是从 `area` 那一格里开始画的。
    ///
    /// 光比文字矩形不够：窗口浮在欢迎页上时，欢迎页那几行大字也落在窗口那块矩形里，
    /// 「窗口里的文字不许互相压着」就全是假阳性。裁剪框能分开两者，但不能反着比大小——
    /// egui 的裁剪框只往右下开到屏幕边缘（窗口内容那一格是 `窗口左上角 → 屏幕右下角`），
    /// 所以 `contains_rect` 永远不成立。能信的是左上角：谁的内容就谁的框，
    /// 起点不会跑到自己外面去。
    fn clipped_in(c: &egui::epaint::ClippedShape, area: Rect) -> bool {
        area.contains(c.clip_rect.min)
    }

    /// 落在 `area` 里的、文字正好等于 `needle` 的那些矩形。
    ///
    /// 「终端」「数据库」这类导航项的名字在顶栏和侧栏也各有一份，光按文字找会先捡到
    /// 界面顶上那一处，所以按矩形框筛。
    fn text_rects_in(out: &egui::FullOutput, area: Rect, needle: &str) -> Vec<Rect> {
        fn walk(shape: &egui::Shape, needle: &str, area: Rect, hits: &mut Vec<Rect>) {
            match shape {
                egui::Shape::Text(t) => {
                    if t.galley.job.text == needle {
                        let r = t.galley.rect.translate(t.pos.to_vec2());
                        if area.contains(r.min) && area.contains(r.max) {
                            hits.push(r);
                        }
                    }
                }
                egui::Shape::Vec(shapes) => {
                    for s in shapes {
                        walk(s, needle, area, hits);
                    }
                }
                _ => {}
            }
        }
        let mut hits = Vec::new();
        for c in &out.shapes {
            if !clipped_in(c, area) {
                continue;
            }
            walk(&c.shape, needle, area, &mut hits);
        }
        hits
    }

    /// 落在 `area` 里的所有文字，连它们各自的矩形。
    fn painted_texts(out: &egui::FullOutput, area: Rect) -> Vec<(String, Rect)> {
        fn walk(shape: &egui::Shape, area: Rect, hits: &mut Vec<(String, Rect)>) {
            match shape {
                egui::Shape::Text(t) => {
                    let text = t.galley.job.text.as_str();
                    if !text.trim().is_empty() {
                        let r = t.galley.rect.translate(t.pos.to_vec2());
                        if area.contains(r.min) && area.contains(r.max) {
                            hits.push((text.to_owned(), r));
                        }
                    }
                }
                egui::Shape::Vec(shapes) => {
                    for s in shapes {
                        walk(s, area, hits);
                    }
                }
                _ => {}
            }
        }
        let mut hits = Vec::new();
        for c in &out.shapes {
            if !clipped_in(c, area) {
                continue;
            }
            walk(&c.shape, area, &mut hits);
        }
        hits
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
        let mut out = frame_time(
            &mut app,
            &ctx,
            1.0,
            vec![egui::Event::PointerMoved(row.center())],
        );
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
        frame(
            &mut app,
            &ctx,
            pointer(center, egui::PointerButton::Primary, true),
        );
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
        frame(
            &mut app,
            &ctx,
            pointer(start, egui::PointerButton::Primary, true),
        );
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

    /// Alt 类组合键：egui-winit 除了按键还附带一个该键打出的字符（按住 Ctrl 才不附）。
    /// 快捷键层以前只摘按键，剩下的 z 顺着焦点就打进正在输入的框——
    /// 按 Alt+Z 切自动换行，搜索框里同时多出一个 z。
    #[test]
    fn shortcut_chord_does_not_type_its_own_character() {
        let (ctx, mut app, _dir) = app_in_folder("easy-editor-app-chord-leak", "hello\n");
        frame(
            &mut app,
            &ctx,
            vec![key_press(Key::F, Modifiers::CTRL | Modifiers::SHIFT)],
        );
        // `focus_search` 要下一帧才落到搜索框上，多跑两帧确保焦点真的到位
        frame(&mut app, &ctx, vec![]);
        frame(&mut app, &ctx, vec![]);
        // 先证明这个框此刻确实吃键盘，否则下面的断言是空的
        frame(&mut app, &ctx, vec![egui::Event::Text("h".into())]);
        assert_eq!(app.search.query, "h", "搜索框没拿到焦点，测不出漏字符");
        app.search.query.clear();

        let wrap = app.word_wrap;
        frame(
            &mut app,
            &ctx,
            vec![
                key_press(Key::Z, Modifiers::ALT),
                egui::Event::Text("z".into()),
            ],
        );
        assert_ne!(app.word_wrap, wrap, "Alt+Z 没切自动换行");
        assert_eq!(app.search.query, "", "Alt+Z 顺带的字符漏进了输入框");
    }

    /// 搜索框里按过回车以后，焦点回到正文：正文里再按回车只能换行，不该把全局搜索
    /// 又跑一遍。以前判的是 `resp.lost_focus()`，正文那一下换行正好落进这个条件里。
    #[test]
    fn enter_in_the_editor_is_a_newline_not_another_search() {
        let (ctx, mut app, dir) =
            app_in_folder("easy-editor-app-search-enter", "hello world\nbye\n");
        app.open_file(dir.join("a.txt"), None);
        frame(&mut app, &ctx, vec![]);

        // 侧栏搜索：填关键字 → 回车开跑 → 等结果回填
        app.side = SideTab::Search;
        app.show_side = true;
        app.focus_search = true;
        frame(&mut app, &ctx, vec![]);
        app.search.query = "hello".into();
        frame(&mut app, &ctx, vec![key_press(Key::Enter, Modifiers::NONE)]);
        for _ in 0..200 {
            frame(&mut app, &ctx, vec![]);
            if !app.search.busy {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let started = app.search.generation;
        assert!(started > 0, "搜索框里那下回车没把搜索跑起来");

        // 回到正文：点编辑器、敲回车。点的是第一行那一带——正文只有两行时
        // TextEdit 就只排两行高，面板正中在它下面，点了等于没点。
        let pane = app
            .pane_layout
            .pane_rect(app.panes.focus().unwrap())
            .unwrap();
        let over = egui::pos2(pane.left() + 80.0, pane.top() + 40.0);
        frame(
            &mut app,
            &ctx,
            pointer(over, egui::PointerButton::Primary, true),
        );
        frame(
            &mut app,
            &ctx,
            pointer(over, egui::PointerButton::Primary, false),
        );
        let before = app.tabs[app.active.unwrap()].text.clone();

        frame(&mut app, &ctx, vec![key_press(Key::Enter, Modifiers::NONE)]);
        frame(&mut app, &ctx, vec![]);

        assert_eq!(app.search.generation, started, "正文里回车把搜索又跑了一遍");
        assert!(
            app.tabs[app.active.unwrap()].text.len() > before.len(),
            "正文里那一下回车没换成行"
        );
    }

    /// 「移到最右侧」：原先上下堆叠的面板要变成左右并排，并且贴到工作区右缘。
    #[test]
    fn moving_pane_to_right_splits_side_by_side() {
        let (ctx, mut app, _dir) = app_in_folder("easy-editor-app-move-pane-test", "hello\n");
        let root = app.panes.focus().unwrap();
        app.panes
            .split(root, Axis::Vertical, PaneKind::Editor(None));
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
        assert!(
            moved.min.x > before.min.x,
            "没有向右移动：before={before:?}"
        );
    }

    /// 「移到最下方」：左右并排的面板要变成上下堆叠，并且贴到工作区下缘。
    #[test]
    fn moving_pane_to_bottom_stacks_below() {
        let (ctx, mut app, _dir) = app_in_folder("easy-editor-app-move-pane-down", "hello\n");
        let left = app.panes.focus().unwrap();
        app.panes
            .split(left, Axis::Horizontal, PaneKind::Editor(None));
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
        assert!(
            moved.min.y > before.min.y,
            "没有向下移动：before={before:?}"
        );
        assert_eq!(app.status, "已移动面板");
    }

    /// 设置窗口：导航和内容各占一栏、内容装得下窗口，而且标题条拖着能动。
    ///
    /// 「终端设置」原来尺寸写死 340×420：条目一多就从下边缘捅出去，按钮画到窗口外面；
    /// 再加上 anchor 每帧生效，拖到哪都会被弹回屏幕正中。合成设置窗口后这两条都要照验。
    #[test]
    fn settings_window_fits_content_and_moves() {
        let (ctx, mut app, _dir) = app_in_folder("easy-editor-app-settings-window", "a\n");
        app.open_settings(SettingsSection::Terminal);
        // 头几帧只在量尺寸、摆位置，尺寸落定后 egui 才让窗口参与交互
        for _ in 0..5 {
            frame(&mut app, &ctx, vec![]);
        }
        let out = frame(&mut app, &ctx, vec![]);

        let win = egui::Id::unique("ee_settings");
        let window = ctx
            .memory(|m| m.area_rect(win))
            .expect("没有记下「设置」窗口的矩形");
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
            "新建终端用哪种 shell",
            "程序路径",
            "自动探测到",
            "启动参数",
            "按空格拆分",
            "改动即时生效",
        ] {
            check(line);
        }

        // 左侧导航五项齐全，内容画在导航右边——分区是左右两栏，不是上下堆
        let content = find_text(&out, "新建终端用哪种 shell").expect("终端分区没画出 shell 选择");
        // 同一个词在顶栏、侧栏也有一份，只认窗口里最靠左的那一处：导航就是那一栏
        let nav = |name: &'static str| {
            text_rects_in(&out, window, name)
                .into_iter()
                .min_by(|a, b| a.min.x.partial_cmp(&b.min.x).unwrap())
                .unwrap_or_else(|| panic!("导航里没有「{name}」"))
        };
        let nav_first = nav("外观");
        for name in ["快捷键", "终端", "系统", "数据库"] {
            let r = nav(name);
            assert!(
                (r.min.x - nav_first.min.x).abs() < 2.0,
                "导航项「{name}」没和「外观」对齐成一栏：{r:?} {nav_first:?}"
            );
            assert!(
                r.min.y > nav_first.max.y,
                "导航项「{name}」没排在「外观」下面：{r:?} {nav_first:?}"
            );
            assert!(
                r.max.x < content.min.x,
                "导航项「{name}」压到了内容那一栏：{r:?}，内容 {content:?}"
            );
        }
        assert!(
            content.min.x > nav_first.max.x,
            "内容压到了导航那一栏：内容 {content:?}，导航 {nav_first:?}"
        );
        let clear = find_text(&out, "清除覆盖").expect("没画出「清除覆盖」");
        assert!(
            window.expand2(Vec2::splat(1.0)).contains(clear.min)
                && window.expand2(Vec2::splat(1.0)).contains(clear.max),
            "「清除覆盖」跑到窗口外了：{clear:?}，窗口 {window:?}"
        );
        // 「新增自定义 shell」排在整张表单下面：它是给列表添新条目的，不属于当前这条
        let add = find_text(&out, "+ 新增自定义 shell").expect("没画出「+ 新增自定义 shell」");
        assert!(
            window.expand2(Vec2::splat(1.0)).contains(add.min)
                && window.expand2(Vec2::splat(1.0)).contains(add.max),
            "「新增自定义 shell」跑到窗口外了：{add:?}，窗口 {window:?}"
        );
        assert!(
            add.min.y > clear.min.y,
            "新增按钮排到了表单字段中间：新增 {add:?}，清除覆盖 {clear:?}"
        );
        let footer = text_rect_where(&out, |t| t.starts_with("改动即时生效"))
            .expect("没画出「改动即时生效…」这行");
        assert!(
            window.expand2(Vec2::splat(1.0)).contains(footer.min)
                && window.expand2(Vec2::splat(1.0)).contains(footer.max),
            "「改动即时生效…」这行跑到窗口外了：{footer:?}，窗口 {window:?}"
        );

        let title = text_rects_in(&out, window, "设置")
            .into_iter()
            .min_by(|a, b| a.min.y.partial_cmp(&b.min.y).unwrap())
            .expect("窗口标题没画出来");
        let grab = title.center();
        let before = window.min;
        let drag = Vec2::new(120.0, 60.0);
        frame(
            &mut app,
            &ctx,
            pointer(grab, egui::PointerButton::Primary, true),
        );
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

    /// 连接管理窗口：列表和表单各成一档，标签压在输入框上面。
    ///
    /// 用户说这两块「看着很难区分」：以前一个窗口里摊着同色的两堆控件，中间只有一条
    /// 分隔线，标签还和输入框横着排——一整片一样轻的灰字，分不清哪里能填。现在列表在
    /// 上、表单在下，字段一栏一个标签，成对的字段左右各占一栏。
    #[test]
    fn manager_window_separates_list_from_form() {
        let (ctx, mut app, _dir) = app_in_folder("easy-editor-app-db-manager", "a\n");
        app.database = mock_database();
        app.database.open_manager();
        // 头几帧只在量尺寸，尺寸落定后窗口才参与交互
        for _ in 0..4 {
            frame(&mut app, &ctx, vec![]);
        }
        let win = egui::Id::unique("db_manager");
        let early = ctx
            .memory(|m| m.area_rect(win))
            .expect("没有记下「数据库连接」窗口的矩形");

        // 点列表里那条连接，表单这才有东西可填
        let out = frame(&mut app, &ctx, vec![]);
        let row = text_rects_in(&out, early, "本机")
            .into_iter()
            .next()
            .expect("列表里没画出连接名「本机」");
        frame(
            &mut app,
            &ctx,
            pointer(row.center(), egui::PointerButton::Primary, true),
        );
        frame(
            &mut app,
            &ctx,
            pointer(row.center(), egui::PointerButton::Primary, false),
        );
        let out = frame(&mut app, &ctx, vec![]);
        // 窗口是 auto_sized 的，表单展开后长高了一截——矩形要按点完之后的那一帧量
        let window = ctx.memory(|m| m.area_rect(win)).expect("点完没了窗口矩形");

        // 「本机」在两处都有：列表那一行和表单的名称框。取靠上的那个才是列表
        let label = |name: &str| {
            text_rects_in(&out, window, name)
                .into_iter()
                .min_by(|a, b| a.min.y.partial_cmp(&b.min.y).unwrap())
                .unwrap_or_else(|| panic!("连接管理窗口里没画出「{name}」"))
        };

        let head = label("已保存的连接（1）");
        let name = label("名称（SQL 块里 conn= 用的就是这个名字）");
        assert!(
            name.min.y > head.max.y,
            "表单排到了列表上面：列表标题 {head:?}，名称标签 {name:?}"
        );

        // 成对的字段左右各一栏，同一行；标签在这一栏的上面
        for (left, right) in [("引擎", "端口"), ("主机", "默认库"), ("用户", "密码")] {
            let a = label(left);
            let b = label(right);
            assert!(
                a.min.y < b.max.y && b.min.y < a.max.y,
                "「{left}」「{right}」没排在同一行：{a:?} {b:?}"
            );
            assert!(
                a.max.x < b.min.x,
                "「{left}」压到了「{right}」那一栏：{a:?} {b:?}"
            );
        }
        // 从上往下：引擎/端口 → 主机/默认库 → 用户/密码 → 环境变量勾选
        let lefts = [label("引擎"), label("主机"), label("用户")];
        let env = label("密码从环境变量读");
        for pair in lefts.windows(2) {
            assert!(
                pair[0].max.y <= pair[1].min.y + 1.0,
                "字段行叠在一起了：{:?} 画在 {:?} 上面还不让位",
                pair[0],
                pair[1]
            );
            assert!(
                (pair[0].min.x - pair[1].min.x).abs() < 1.0,
                "每一行的左栏没对齐成一列：{:?} {:?}",
                pair[0],
                pair[1]
            );
        }
        assert!(
            lefts[2].max.y <= env.min.y + 1.0,
            "「密码从环境变量读」和上面的字段挤在一起：{:?} {:?}",
            lefts[2],
            env
        );


        // 满窗口不许有两块文字互相压着——卡片、标签、输入框都算在内
        let bad = overlaps(&painted_texts(&out, window));
        assert!(bad.is_empty(), "连接管理窗口里有文字叠在一起：{bad:?}");
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

        frame(
            &mut app,
            &ctx,
            pointer(grab, egui::PointerButton::Secondary, true),
        );
        frame(
            &mut app,
            &ctx,
            pointer(grab, egui::PointerButton::Secondary, false),
        );
        let item = wait_for_text(&mut app, &ctx, "移到最下方")
            .expect("右键标题没有弹出「移动面板」菜单")
            .center();

        frame(
            &mut app,
            &ctx,
            pointer(item, egui::PointerButton::Primary, true),
        );
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
                egui::Shape::Text(t) => v.push((
                    t.galley.job.text.clone(),
                    t.galley.rect.translate(t.pos.to_vec2()),
                )),
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
        "打开文件",
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
            vec![egui::Event::ModifiersChanged(cmd), key_press(Key::Z, cmd)],
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
        // 「1500 宽必须排成一排」同样是字号的函数：不钉死，用户把界面字号调大一点就判失败
        app.set_ui_font_size(theme::UI_FONT_DEFAULT, &ctx);
        let mut want = TOOLBAR_LABELS.to_vec();
        want.sort_unstable();

        // 从窗口最小宽度往上一档一档量：哪一档都不许叠字、不许挤掉按钮、不许画出右边缘
        // 顶栏左端多了「打开文件」，右边那组（字号 / shell / 主题 / 帮助）跟着被挤到第二排，
        // 量下来一排排得下的界限从 1400 抬到了 1500。
        for w in [800.0, 900.0, 1000.0, 1100.0, 1200.0, 1300.0, 1400.0, 1500.0, 1600.0] {
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

            // 1500 放得下一整排，就别白占高度；900 放不下，必须换排
            if w >= 1500.0 {
                assert!(
                    all.iter().all(|(_, r)| r.top() < 24.0),
                    "1500 宽顶栏占了不止一排：{all:?}"
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
            Modifiers {
                shift: true,
                ..ctrl
            },
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

    /// 「界面字号」这一档真的推着界面走：顶栏的字跟着变大，行高也跟着长，不压字。
    ///
    /// 从前只有编辑器字号能调，界面其余各处全写死在代码里——字号调大，顶栏、侧栏、结果区
    /// 一动不动。这里量的是顶栏「保存」那颗按钮：它的高度是照字号量的，字号动了它必须动。
    /// 只放大字号而不跟着放高，字就会叠在相邻的字上，所以顺手查一遍有没有重叠。
    #[test]
    fn ui_font_size_scales_the_chrome() {
        let (ctx, mut app, _dir) = app_in_folder("easy-editor-app-ui-font-scale", "a\n");
        let label = |v: &[(String, Rect)], name: &str| -> Rect {
            v.iter()
                .find(|(s, _)| s == name)
                .map(|(_, r)| *r)
                .unwrap_or_else(|| panic!("顶栏里没画出「{name}」"))
        };
        let before = settled_toolbar(&mut app, &ctx, SCREEN);
        let small = label(&before, "保存");
        let editor_before = app.font_size;

        app.set_ui_font_size(theme::UI_FONT_MAX, &ctx);
        let after = settled_toolbar(&mut app, &ctx, SCREEN);
        let large = label(&after, "保存");
        assert!(
            large.height() > small.height() * 1.15 && large.width() > small.width() * 1.15,
            "界面字号拉到顶（{}）后顶栏「保存」没跟着变大：{small:?} → {large:?}",
            theme::UI_FONT_MAX
        );
        assert!(
            large.bottom() <= SCREEN.y,
            "字号调大后顶栏「保存」被挤出屏幕：{large:?}"
        );
        let bad = overlaps(&after);
        assert!(bad.is_empty(), "字号调大后顶栏的字叠在一起：{bad:?}");
        // 两档字号各管各的：调界面不该顺手改掉编辑器的
        assert_eq!(
            app.font_size, editor_before,
            "界面字号这一档改掉了编辑器字号"
        );
    }

    /// 拖动字号滑块时界面不能跟着动，点「应用」才换字号。
    ///
    /// 即时生效那一版被反馈「页面快速闪烁」：改界面字号要重铺字体图集并让整页重排，
    /// 拖动过程中每一帧排一次，连滑块自己都在指针底下挪，看起来就是整片在抖。
    /// 现在滑块只写 `pending_*`，提交走「应用」这一颗。
    #[test]
    fn font_slider_waits_for_the_apply_button() {
        let (ctx, mut app, _dir) = app_in_folder("easy-editor-app-font-apply", "a\n");
        let chip = |app: &mut EasyEditorApp| -> Rect {
            let v = settled_toolbar(app, &ctx, SCREEN);
            v.iter()
                .find(|(s, _)| s == "保存")
                .map(|(_, r)| *r)
                .unwrap_or_else(|| panic!("顶栏里没画出「保存」"))
        };
        app.open_settings(SettingsSection::Appearance);
        for _ in 0..5 {
            frame(&mut app, &ctx, vec![]);
        }
        let live_before = app.ui_font;
        let chip_before = chip(&mut app);
        let out = frame(&mut app, &ctx, vec![]);
        assert_eq!(
            app.pending_ui_font, live_before,
            "刚打开就该和实际字号一致"
        );

        // 「拖」滑块：从滑块右端往右挪一段。落点用旁边的「恢复默认」当尺子——
        // 滑块横向铺在这一排里，往左退一档一定还在滑块上（`find_text` 给的第一个
        // 「恢复默认」就是界面字号那一排的）。
        let reset = find_text(&out, "恢复默认").expect("外观分区没画出「恢复默认」");
        let y = reset.center().y;
        let from = Pos2::new(reset.min.x - 40.0, y);
        let to = Pos2::new(reset.min.x + 50.0, y);
        frame(&mut app, &ctx, pointer(from, egui::PointerButton::Primary, true));
        frame(&mut app, &ctx, vec![egui::Event::PointerMoved(to)]);
        frame(&mut app, &ctx, pointer(to, egui::PointerButton::Primary, false));
        let out = frame(&mut app, &ctx, vec![]);

        assert_ne!(
            app.pending_ui_font, live_before,
            "这一下拖动没改到暂存值，滑块没接到 pending 上"
        );
        assert_eq!(
            app.ui_font, live_before,
            "拖动滑块就改了实际字号，界面会在指针底下重排"
        );
        assert_eq!(
            chip(&mut app),
            chip_before,
            "还没点应用，顶栏就已经跟着重排了"
        );
        // 攒着改动时必须看得见「应用」这颗，否则用户不知道下一步点哪
        let apply = find_text(&out, "应用").expect("有未应用改动时没画出「应用」");

        tap(&mut app, &ctx, apply.center());
        for _ in 0..3 {
            frame(&mut app, &ctx, vec![]);
        }
        let wanted = app.pending_ui_font;
        assert_eq!(app.ui_font, wanted, "点了应用还没换字号");
        assert_ne!(chip(&mut app), chip_before, "换了字号顶栏却没动");

        // 关掉再打开，暂存要回到实际值：不该有「看不见的改动」攒在那儿
        app.show_settings = false;
        frame(&mut app, &ctx, vec![]);
        app.open_settings(SettingsSection::Appearance);
        for _ in 0..3 {
            frame(&mut app, &ctx, vec![]);
        }
        assert_eq!(app.pending_ui_font, app.ui_font);
    }

    /// 设置「终端」里点「新增自定义 shell」：立刻选上、进下拉、落到配置里。
    ///
    /// 内置那五种只能改路径，加不出第六种；装在别处的 MSYS2、busybox 之类的
    /// 得能自己登记一条，登记完和内置的平起平坐。
    #[test]
    fn terminal_section_adds_a_custom_shell() {
        let (ctx, mut app, _dir) = app_in_folder("easy-editor-app-custom-shell", "a\n");
        app.open_settings(SettingsSection::Terminal);
        for _ in 0..5 {
            frame(&mut app, &ctx, vec![]);
        }
        let out = frame(&mut app, &ctx, vec![]);
        let add = find_text(&out, "+ 新增自定义 shell").expect("没画出「+ 新增自定义 shell」");
        tap(&mut app, &ctx, add.center());
        for _ in 0..3 {
            frame(&mut app, &ctx, vec![]);
        }

        let picked = app.terminal.shell.id();
        assert!(
            picked.starts_with("custom:"),
            "点完新增没选中新那条：{picked}"
        );
        assert!(
            app.terminal.available.iter().any(|s| s.id() == picked),
            "新加的没进下拉列表"
        );
        // 顶栏那个下拉是同一个值，不用打开设置也看得见加了什么
        let bar = toolbar_texts(&frame(&mut app, &ctx, vec![]));
        assert!(
            bar.iter()
                .any(|(t, _)| t == "shell: 未命名 shell"),
            "顶栏没显示新加的自定义 shell：{:?}",
            names(&bar)
        );
        // 表单换成了自定义那三项，其中没有「清除覆盖」——它没有默认值可回
        let out = frame(&mut app, &ctx, vec![]);
        assert!(find_text(&out, "显示名称").is_some(), "没切到自定义 shell 的表单");
        assert!(find_text(&out, "删除这条").is_some(), "自定义 shell 没给删除");
        // 配置里落下这一条，`terminal_shell` 也指向它
        app.sync_config();
        assert_eq!(app.config.data.terminal_custom_shells.len(), 1);
        assert_eq!(app.config.data.terminal_shell, picked);
    }

    /// 输入法组字能不能活过对话框刷新的每一帧。
    ///
    /// 之前输入框每帧无条件 `request_focus()`，egui 把
    /// `should_interrupt_composition` 置真发给后端，winit 当场取消合成，
    /// 拼音还没挑完候选词就被清空，中文永远填不进「新建文件/文件夹」。
    #[test]
    fn new_file_dialog_survives_ime_composition() {
        let (ctx, mut app, dir) = app_in_folder("easy-editor-app-ime-test", "a\n");
        let out = frame(&mut app, &ctx, vec![]);
        let row = find_text(&out, "a.txt")
            .expect("文件树里没画出 a.txt")
            .center();
        open_new_dialog(&mut app, &ctx, row);

        // 拿到焦点的那一帧 egui 还没有光标 range，输入法要到下一帧才申请
        let out = frame(&mut app, &ctx, vec![]);
        let ime = out
            .platform_output
            .ime
            .expect("输入框拿到焦点后没向系统申请输入法");
        assert!(
            !ime.should_interrupt_composition,
            "对话框还在每帧打断输入法组字"
        );

        // 打「ceshi」出候选串，再空跑几帧：真人打输入法的速度比一帧慢得多，
        // 组字期间应用照样会刷好几帧，每一帧都不能打断
        frame(
            &mut app,
            &ctx,
            vec![egui::Event::Ime(egui::ImeEvent::Preedit {
                text: "测试".into(),
                active_range_chars: Some(0..2),
            })],
        );
        assert_eq!(
            app.tree.as_ref().unwrap().new_name,
            "测试",
            "输入法候选串没进新建对话框"
        );
        for _ in 0..3 {
            let out = frame(&mut app, &ctx, vec![]);
            let ime = out.platform_output.ime.expect("组字途中输入法申请被撤掉了");
            assert!(
                !ime.should_interrupt_composition,
                "组字途中被打断，候选串会被清空"
            );
        }

        // 挑定候选词：提交后回车才该建文件
        frame(
            &mut app,
            &ctx,
            vec![egui::Event::Ime(egui::ImeEvent::Commit("测试".into()))],
        );
        assert_eq!(app.tree.as_ref().unwrap().new_name, "测试");
        frame(&mut app, &ctx, vec![key_press(Key::Enter, Modifiers::NONE)]);
        assert!(dir.join("测试").is_file(), "输入法提交的中文名没能建出文件");
    }

    /// 候选串还挂在输入框里时，同一帧进来的回车是给输入法的，不是「创建」。
    #[test]
    fn enter_during_composition_does_not_create_file() {
        let (ctx, mut app, dir) = app_in_folder("easy-editor-app-ime-enter-test", "a\n");
        let out = frame(&mut app, &ctx, vec![]);
        let row = find_text(&out, "a.txt")
            .expect("文件树里没画出 a.txt")
            .center();
        open_new_dialog(&mut app, &ctx, row);
        frame(&mut app, &ctx, vec![]);

        let out = frame(
            &mut app,
            &ctx,
            vec![
                egui::Event::Ime(egui::ImeEvent::Preedit {
                    text: "测试".into(),
                    active_range_chars: Some(0..2),
                }),
                key_press(Key::Enter, Modifiers::NONE),
            ],
        );
        assert_eq!(
            app.tree.as_ref().unwrap().new_name,
            "测试",
            "前置条件不成立：候选串没进框"
        );
        assert!(
            find_text(&out, "新建文件").is_some(),
            "组字里的回车把对话框当「创建」关掉了"
        );
        assert!(!dir.join("测试").exists(), "组字中的回车提前建了文件");

        // 提交候选词之后的回车才真的建文件
        frame(
            &mut app,
            &ctx,
            vec![egui::Event::Ime(egui::ImeEvent::Commit("测试".into()))],
        );
        frame(&mut app, &ctx, vec![key_press(Key::Enter, Modifiers::NONE)]);
        assert!(dir.join("测试").is_file(), "提交之后的回车没能建文件");
    }

    /// Ctrl+J 插日志块时，长文档不能因为排版表对不上被甩回文件顶部。
    ///
    /// 折行的文件里一个逻辑行占好几视觉行，跳转只能查刚排好的那张表；
    /// 表还没重建就去量行数，就只能按「行号 × 行高」粗算，能差出上百行。
    #[test]
    fn journal_insert_keeps_the_view_on_wrapped_lines() {
        let long = "一二三四五六七八九十".repeat(12);
        let body: String = (1..=300).map(|i| format!("L{i} {long}\n")).collect();
        let (ctx, mut app, dir) = app_in_folder("easy-editor-app-journal-wrap", "seed\n");
        std::fs::write(dir.join("j.txt"), &body).unwrap();
        app.open_file(dir.join("j.txt"), None);
        app.word_wrap = true;

        let mut t = 1.0;
        let mut next = |app: &mut EasyEditorApp, ctx: &egui::Context, n: usize| {
            let mut out = None;
            for _ in 0..n {
                out = Some(frame_time(app, ctx, t, vec![]));
                t += 0.1;
            }
            out.unwrap()
        };

        // 先把光标送到第 200 行并滚过去
        let byte = line_start_offset(&app.tabs[0].text, 200);
        app.tabs[0].caret_byte = byte;
        app.tabs[0].pending_jump = Some(byte);
        let pane = {
            let _ = next(&mut app, &ctx, 2);
            app.pane_layout
                .pane_rect(app.panes.focus().unwrap())
                .unwrap()
        };
        let from = {
            let out = next(&mut app, &ctx, 20);
            let first = gutter(&out, pane)
                .first()
                .expect("跳转后一行号都没画出来")
                .0
                .clone();
            first.parse::<usize>().expect("行号不是数字")
        };
        assert!(from > 150, "跳到第 200 行没滚到位：从第 {from} 行开始");

        app.insert_journal_block();
        let out = next(&mut app, &ctx, 20);
        let to = gutter(&out, pane)
            .first()
            .expect("插入后一行号都没画出来")
            .0
            .parse::<usize>()
            .expect("行号不是数字");
        assert!(
            to.abs_diff(from) <= 20,
            "插完日志块视图被甩走了：第 {from} 行 -> 第 {to} 行"
        );
        assert!(
            app.tabs[0].line > 190,
            "光标跟着视图一起跑了：第 {} 行",
            app.tabs[0].line
        );
    }

    /// 不折行的长行：按住左键拖到视口右边缘，正文要自己往左走，选区跟着往右长。
    ///
    /// egui 的选区只跟着指针走，指针又出不了视口，所以一超出屏幕就圈不到外面的字。
    #[test]
    fn dragging_to_the_viewport_edge_auto_scrolls_horizontally() {
        let long = "abcdefghij".repeat(60);
        let (ctx, mut app, dir) = app_in_folder("easy-editor-app-edgedrag", "seed\n");
        std::fs::write(dir.join("e.txt"), format!("{long}\nlast\n")).unwrap();
        app.open_file(dir.join("e.txt"), None);
        app.word_wrap = false;

        let pane = {
            frame(&mut app, &ctx, vec![]);
            app.pane_layout
                .pane_rect(app.panes.focus().unwrap())
                .unwrap()
        };
        // 正文只有两行高，指针得落在字上才按得到 TextEdit
        let before = {
            let out = frame(&mut app, &ctx, vec![]);
            body(&out, 'a').expect("正文没画出来")
        };
        let start = egui::pos2(before.left() + 120.0, before.top() + 10.0);
        frame(
            &mut app,
            &ctx,
            pointer(start, egui::PointerButton::Primary, true),
        );
        frame(&mut app, &ctx, vec![]);
        let col0 = app.tabs[0].col;

        // 停在右边缘上不动：指针不发事件也得接着滚，所以每帧补一个 move
        let edge = egui::pos2(pane.right() - 3.0, start.y);
        let mut out = frame(&mut app, &ctx, vec![egui::Event::PointerMoved(edge)]);
        for _ in 0..10 {
            out = frame(&mut app, &ctx, vec![egui::Event::PointerMoved(edge)]);
        }

        let after = body(&out, 'a').expect("滚动后正文没画出来");
        assert!(
            after.left() < before.left() - 40.0,
            "拖到右边缘没自动横向滚动：{:.0} -> {:.0}",
            before.left(),
            after.left()
        );
        assert!(
            app.tabs[0].col > col0 + 5,
            "滚过去了但选区没跟着长：第 {col0} 列 -> 第 {} 列",
            app.tabs[0].col
        );
    }

    #[test]
    fn toolbar_journal_button_keeps_caret_and_focus() {
        // 点顶栏「日志块」的那一帧：有鼠标点击、鼠标又不在正文上。
        // egui 会对「点了别处」的已聚焦控件 surrender_focus，
        // 早于 TextEdit::interact 要来的焦点会被当场抹掉，表现为插完块光标没了、字也打不了。
        let (ctx, mut app, dir) = app_in_folder("easy-editor-app-journal-click", "seed\n");
        std::fs::write(dir.join("j.txt"), "alpha\nbeta\ngamma\n").unwrap();
        app.open_file(dir.join("j.txt"), None);
        let _ = frame(&mut app, &ctx, vec![]);
        let out = frame(&mut app, &ctx, vec![]);
        let pane = app
            .pane_layout
            .pane_rect(app.panes.focus().unwrap())
            .unwrap();
        let row2 = gutter(&out, pane)
            .into_iter()
            .find(|(s, _)| s == "2")
            .map(|(_, r)| r.center())
            .expect("行号 2 没找到");
        let spot = egui::pos2(pane.min.x + 150.0, row2.y);
        let _ = frame(
            &mut app,
            &ctx,
            pointer(spot, egui::PointerButton::Primary, true),
        );
        let _ = frame(
            &mut app,
            &ctx,
            pointer(spot, egui::PointerButton::Primary, false),
        );
        let _ = frame(&mut app, &ctx, vec![]);
        let focus_before = ctx.memory(|m| m.focused());
        assert!(focus_before.is_some(), "点正文后编辑器没拿到焦点");

        let out = frame(&mut app, &ctx, vec![]);
        let btn = find_text(&out, "日志块")
            .expect("顶栏「日志块」按钮没画出来")
            .center();
        let _ = frame(
            &mut app,
            &ctx,
            pointer(btn, egui::PointerButton::Primary, true),
        );
        let _ = frame(
            &mut app,
            &ctx,
            pointer(btn, egui::PointerButton::Primary, false),
        );
        for _ in 0..4 {
            let _ = frame(&mut app, &ctx, vec![]);
        }

        let text = app.tabs[0].text.clone();
        let new_row = text
            .lines()
            .position(|l| l.trim() == "1、")
            .expect("没有新建出「1、」任务行");
        assert_eq!(app.tabs[0].line, new_row + 1, "光标没停在新任务行上");
        assert_eq!(
            app.tabs[0].caret_byte,
            line_start_offset(&text, new_row + 1) + "1、".len(),
            "光标没停在新任务行末尾：{text:?}"
        );
        assert_eq!(
            ctx.memory(|m| m.focused()),
            focus_before,
            "点顶栏按钮后编辑器焦点被抢走了，中文输入和打字都会失效"
        );
    }

    /// 用鼠标拖选正文时，输入法图标不能一帧一闪。
    ///
    /// egui 每帧都会对拖选中的 TextEdit 调 `request_focus`，那会顺带置位
    /// 「打断组字」；后端收到就 `set_ime_allowed(false)`+`(true)`，把输入法
    /// 上下文一帧一次卸下再挂回，屏幕上就是输入法图标在闪。
    #[test]
    fn dragging_a_selection_does_not_thrash_the_ime() {
        let long = "abcdefghij".repeat(60);
        let (ctx, mut app, dir) = app_in_folder("easy-editor-app-ime-drag", "seed\n");
        std::fs::write(dir.join("d.txt"), format!("{long}\nlast\n")).unwrap();
        app.open_file(dir.join("d.txt"), None);
        app.word_wrap = false;
        frame(&mut app, &ctx, vec![]);
        let out = frame(&mut app, &ctx, vec![]);
        let before = body(&out, 'a').expect("正文没画出来");
        let start = egui::pos2(before.left() + 40.0, before.top() + 10.0);
        let _ = frame(
            &mut app,
            &ctx,
            pointer(start, egui::PointerButton::Primary, true),
        );

        let mut col_last = app.tabs[0].col;
        for i in 0..6 {
            let pos = egui::pos2(start.x + 20.0 * (i as f32 + 1.0), start.y);
            let out = frame(&mut app, &ctx, vec![egui::Event::PointerMoved(pos)]);
            if let Some(ime) = &out.platform_output.ime {
                assert!(
                    !ime.should_interrupt_composition,
                    "拖选第 {i} 帧还在打断输入法组字，图标会跟着闪"
                );
            }
            let col = app.tabs[0].col;
            assert!(
                col > col_last,
                "撤掉锚点把拖选也弄坏了：第 {col_last} 列 -> 第 {col} 列"
            );
            col_last = col;
        }

        // 松手后锚点必须自己回来，否则中文输入法就再也挂不上这个窗口了
        let end = egui::pos2(start.x + 160.0, start.y);
        let _ = frame(
            &mut app,
            &ctx,
            pointer(end, egui::PointerButton::Primary, false),
        );
        let out = frame(&mut app, &ctx, vec![]);
        let ime = out.platform_output.ime.expect("松手后没把输入法锚点还回来");
        assert!(
            !ime.should_interrupt_composition,
            "拖选结束那一帧还在打断输入法"
        );
    }

    /// 结果区用的假连接：不碰 MySQL，两条 SELECT 各回一份网格。
    fn mock_database() -> DatabasePanel {
        DatabasePanel::with_hub(
            crate::db::DbHub::with_driver(|_| Ok(Box::new(crate::db::MockDriver::new()))),
            vec![crate::db::ConnectionSpec {
                name: "本机".into(),
                database: "test".into(),
                ..Default::default()
            }],
        )
    }

    /// 一执行就报错的假连接，用来试语句失败之后编辑器和结果区各干了什么。
    fn failing_database() -> DatabasePanel {
        DatabasePanel::with_hub(
            crate::db::DbHub::with_driver(|_| {
                Ok(Box::new(crate::db::MockDriver::failing("1064 语法错了")))
            }),
            vec![crate::db::ConnectionSpec {
                name: "本机".into(),
                database: "test".into(),
                ..Default::default()
            }],
        )
    }

    const SQL_BLOCK: &str = "-- sql 查用户 2026-01-01 10:00 [待执行] conn=本机\n\
                              SELECT id FROM a;\n\
                              SELECT name FROM b;\n";

    /// 把 SQL 块喂给应用，停在正文第二行（也就是光标在块里）。
    fn app_on_sql_block(dir_name: &str) -> (egui::Context, EasyEditorApp, PathBuf) {
        let (ctx, mut app, dir) = app_in_folder(dir_name, SQL_BLOCK);
        app.database = mock_database();
        app.open_file(dir.join("a.txt"), None);
        let idx = app.active.unwrap();
        app.tabs[idx].line = 2;
        (ctx, app, dir)
    }

    /// 等后台线程把这一批语句的回执都送回来。
    fn settle_results(app: &mut EasyEditorApp, ctx: &egui::Context) {
        for _ in 0..60 {
            frame(app, ctx, vec![]);
            if !app.results.running() {
                return;
            }
        }
        panic!("结果区一直停在「执行中」，后台回执没接到");
    }

    /// 看得见的所有文字：文本 + 它在屏幕上的矩形。
    ///
    /// `texts()` 只看 shape 存不存在——egui 对完全落在裁剪区外的文字照样留一条
    /// shape，于是「面板挤出结果区所以看不见」这种 bug 它查不出来，只能连 clip 一起量。
    fn seen(out: &egui::FullOutput) -> Vec<(String, Rect)> {
        fn walk(shape: &egui::Shape, clip: Rect, v: &mut Vec<(String, Rect)>) {
            match shape {
                egui::Shape::Text(t) => {
                    let rect = t.galley.rect.translate(t.pos.to_vec2());
                    if clip.intersects(rect) {
                        v.push((t.galley.job.text.clone(), rect));
                    }
                }
                egui::Shape::Vec(shapes) => {
                    for s in shapes {
                        walk(s, clip, v);
                    }
                }
                _ => {}
            }
        }
        let mut v = Vec::new();
        for cs in &out.shapes {
            walk(&cs.shape, cs.clip_rect, &mut v);
        }
        v
    }

    /// 一次交两条语句 = 页签栏两格，屏上只摊开一格。
    ///
    /// 从前是两个面板竖着叠：看着公平，实际结果区就那么高，第三个开始全在裁剪区外，
    /// 用户得先拖高面板再滚——十条的时候等于只有两条结果。
    #[test]
    fn sql_block_gets_one_result_tab_per_statement() {
        let (ctx, mut app, _dir) = app_on_sql_block("ee_sql_two_panels");
        app.sql_run_block();
        assert_eq!(app.results.count(), 2, "块里两条语句就该两个页签");
        assert!(app.results.open, "提交了结果区就该亮出来");

        settle_results(&mut app, &ctx);
        let shown = texts(&frame(&mut app, &ctx, vec![]));
        assert!(shown.contains(&"结果 · 2 个".to_string()), "{shown:?}");
        for label in ["1·SELECT id FROM a", "2·SELECT name FROM b"] {
            assert!(
                shown.iter().any(|t| t == label),
                "页签栏少了「{label}」那一格：{shown:?}"
            );
        }
        // 当前那一页摊在屏上：抬头 + 一份网格；另一条只占一个页签
        assert_eq!(on_screen(&shown, "SELECT id FROM a"), 1, "{shown:?}");
        assert_eq!(
            on_screen(&shown, "SELECT name FROM b"),
            0,
            "第二页不该同时铺开"
        );
        assert_eq!(
            shown.iter().filter(|t| t.as_str() == "id").count(),
            1,
            "屏上只该有一份网格：{shown:?}"
        );

        let shown = tab_to(&mut app, &ctx, "2·SELECT name FROM b");
        assert_eq!(app.results.active_task(), Some(2), "点页签没换过去");
        assert_eq!(on_screen(&shown, "SELECT name FROM b"), 1, "{shown:?}");
        assert_eq!(on_screen(&shown, "SELECT id FROM a"), 0, "上一份该收走");
    }

    /// 页签那一格右边的 ✕：和标签同一行、排在标签右边。
    /// 面板抬头也有一个 ✕，但它在分隔线下面那一行，靠 y 就分得开。
    fn tab_cross(out: &egui::FullOutput, label: &str) -> egui::Pos2 {
        let marks = seen(out);
        let tab = marks
            .iter()
            .find(|(t, _)| t == label)
            .map(|(_, r)| *r)
            .unwrap_or_else(|| panic!("页签「{label}」没画出来"));
        marks
            .iter()
            .find(|(t, r)| {
                t == "✕" && (r.center().y - tab.center().y).abs() < 6.0 && r.center().x > tab.min.x
            })
            .map(|(_, r)| r.center())
            .unwrap_or_else(|| panic!("页签「{label}」上没有关闭那一下"))
    }

    #[test]
    fn closing_one_tab_leaves_its_neighbour() {
        let (ctx, mut app, _dir) = app_on_sql_block("ee_sql_close_one");
        app.sql_run_block();
        settle_results(&mut app, &ctx);
        let out = frame(&mut app, &ctx, vec![]);
        let cross = tab_cross(&out, "2·SELECT name FROM b");
        tap(&mut app, &ctx, cross);
        assert_eq!(app.results.count(), 1, "只该关掉点中的那个页签");
        assert_eq!(app.results.panels()[0].task, 1, "留下的是第一条");
        assert_eq!(
            app.results.active_task(),
            Some(1),
            "当前那页没被关，不该跟着跳"
        );

        // 再关掉当前这一页：屏上不该留一个「谁都没指着」的空结果区
        let out = frame(&mut app, &ctx, vec![]);
        let cross = tab_cross(&out, "1·SELECT id FROM a");
        tap(&mut app, &ctx, cross);
        assert_eq!(app.results.count(), 0);
        assert_eq!(app.results.active_task(), None, "全关完了就别还指着一条");
        let shown = texts(&frame(&mut app, &ctx, vec![]));
        assert_eq!(on_screen(&shown, "SELECT id FROM a"), 0);
    }

    #[test]
    fn selection_beats_the_block_around_it() {
        let (ctx, mut app, _dir) = app_on_sql_block("ee_sql_selection");
        let idx = app.active.unwrap();
        // 只选第二条语句那几个字
        let text = app.tabs[idx].text.clone();
        let a = text.find("SELECT name").unwrap();
        app.tabs[idx].selection_range = Some((a, a + "SELECT name".len()));
        app.sql_run();
        assert_eq!(app.results.count(), 1, "选了半截就只跑那段");
        settle_results(&mut app, &ctx);
        assert!(
            app.results.count() == 1 && !app.results.running(),
            "选中的这条跑完就该安静下来"
        );
    }

    #[test]
    fn a_failing_run_leaves_nothing_spinning() {
        let (ctx, mut app, _dir) = app_on_sql_block("ee_sql_failing");
        app.database = failing_database();
        app.sql_run_block();
        settle_results(&mut app, &ctx);
        let shown = texts(&frame(&mut app, &ctx, vec![]));
        // 砸的那条正是当前那页，报错原文当场就在屏上
        assert!(
            shown.iter().any(|t| t.contains("语法错了")),
            "报错原文要贴在面板里：{shown:?}"
        );
        assert!(
            app.results.panels().iter().all(|p| !p.slot.is_running()),
            "回执都到家了不许还转着圈"
        );
        // 被连累没执行的那条在另一个页签上，点过去得能看到
        let shown = tab_to(&mut app, &ctx, "2·SELECT name FROM b");
        assert!(
            shown.iter().any(|t| t.contains("未执行")),
            "第二条要说清它没跑过：{shown:?}"
        );
    }

    #[test]
    fn a_finished_block_gets_its_status_written_back() {
        let (ctx, mut app, _dir) = app_on_sql_block("ee_sql_mark_done");
        app.sql_run_block();
        settle_results(&mut app, &ctx);
        frame(&mut app, &ctx, vec![]);
        let idx = app.active.unwrap();
        assert!(
            app.tabs[idx].text.contains("[已执行]"),
            "跑完了头部还挂着待执行：{}",
            app.tabs[idx].text
        );
    }

    #[test]
    fn a_failed_block_is_marked_as_failed() {
        let (ctx, mut app, _dir) = app_on_sql_block("ee_sql_mark_failed");
        app.database = failing_database();
        app.sql_run_block();
        settle_results(&mut app, &ctx);
        frame(&mut app, &ctx, vec![]);
        let idx = app.active.unwrap();
        assert!(
            app.tabs[idx].text.contains("[失败]"),
            "有一条报错就该标成失败：{}",
            app.tabs[idx].text
        );
    }

    #[test]
    fn a_failing_statement_goes_red_in_the_block() {
        let (ctx, mut app, _dir) = app_on_sql_block("ee_sql_red");
        app.database = failing_database();
        app.sql_run_block();
        settle_results(&mut app, &ctx);
        // 头部回写 [失败] 让正文整体前移了 3 个字节，红底得跟着挪
        frame(&mut app, &ctx, vec![]);
        let idx = app.active.unwrap();
        let file = &app.tabs[idx];
        let marks = file.sql_errors.iter().copied().collect::<Vec<_>>();
        assert_eq!(marks.len(), 1, "第二条是被连累跳过的，不该跟着红");
        assert_eq!(
            &file.text[marks[0].0..marks[0].1],
            "SELECT id FROM a",
            "红底没盖在报错的那句话上：{:?}",
            file.text
        );

        // 改对了再跑一遍，旧的红色该跟着这次提交一起抹掉
        app.database = mock_database();
        app.tabs[idx].line = 2;
        app.sql_run_block();
        assert!(
            app.tabs[idx].sql_errors.is_empty(),
            "重跑先清掉这一段旧的红"
        );
        settle_results(&mut app, &ctx);
        frame(&mut app, &ctx, vec![]);
        let idx = app.active.unwrap();
        assert!(
            app.tabs[idx].sql_errors.is_empty(),
            "这回两条都过了：{:?}",
            app.tabs[idx].sql_errors
        );
    }

    #[test]
    fn a_selection_run_leaves_the_block_status_alone() {
        let (ctx, mut app, _dir) = app_on_sql_block("ee_sql_mark_selection");
        let idx = app.active.unwrap();
        let text = app.tabs[idx].text.clone();
        let a = text.find("SELECT id").unwrap();
        app.tabs[idx].selection_range = Some((a, a + "SELECT id FROM a".len()));
        app.sql_run();
        settle_results(&mut app, &ctx);
        frame(&mut app, &ctx, vec![]);
        let idx = app.active.unwrap();
        assert!(
            app.tabs[idx].text.contains("[待执行]"),
            "只跑了块里挑出来的一段，整块还不算执行完：{}",
            app.tabs[idx].text
        );
    }

    #[test]
    fn zooming_a_result_opens_its_own_window() {
        let (ctx, mut app, _dir) = app_on_sql_block("ee_sql_zoom");
        app.sql_run_block();
        settle_results(&mut app, &ctx);
        let out = frame(&mut app, &ctx, vec![]);
        let buttons = seen(&out)
            .into_iter()
            .filter(|(t, _)| t == "放大")
            .map(|(_, r)| r.center())
            .collect::<Vec<_>>();
        assert_eq!(buttons.len(), 1, "屏上只摊开一页，放大的入口也就一个");

        frame(
            &mut app,
            &ctx,
            pointer(buttons[0], egui::PointerButton::Primary, true),
        );
        frame(
            &mut app,
            &ctx,
            pointer(buttons[0], egui::PointerButton::Primary, false),
        );
        // 新窗口头几帧只在量尺寸、摆位置，尺寸落定才轮到画内容
        frame(&mut app, &ctx, vec![]);
        let out = frame(&mut app, &ctx, vec![]);
        let shown = texts(&out);
        assert!(
            shown.contains(&"结果 · 查用户".to_string()),
            "放大窗口没开：{shown:?}"
        );
        // 窗口和底部面板看的是同一份数据：列头两份（底部一份 + 窗口一份）
        assert_eq!(
            shown.iter().filter(|t| t.as_str() == "id").count(),
            2,
            "窗口里该另起一份网格"
        );
        assert!(
            shown.iter().any(|t| t == "收起"),
            "开着的那块按钮要改口，不然看着像没反应"
        );

        // 一次点击只记一帧的账，窗口得在后面的帧里一直留着
        let out = frame(&mut app, &ctx, vec![]);
        assert!(
            texts(&out).contains(&"结果 · 查用户".to_string()),
            "放大窗口开完下一帧就自己关了"
        );

        // 放大的是「这一页」：换页签时窗口跟着换，不会留着上一份的结果
        let shown = tab_to(&mut app, &ctx, "2·SELECT name FROM b");
        assert_eq!(app.results.active_task(), Some(2));
        assert!(
            shown.iter().any(|t| t == "放大"),
            "换过来那一页也要能单独放大：{shown:?}"
        );
    }

    #[test]
    fn a_query_result_offers_a_csv_export_button() {
        let (ctx, mut app, _dir) = app_on_sql_block("ee_sql_export");
        app.sql_run_block();
        settle_results(&mut app, &ctx);
        let shown = texts(&frame(&mut app, &ctx, vec![]));
        assert_eq!(
            shown.iter().filter(|t| t.as_str() == "导出").count(),
            1,
            "当前那页给一个导出按钮，存盘框归系统弹，这里只测按钮在不在：{shown:?}"
        );
        // 换一页签，导出跟着换——两条查询结果都得能单独存
        let shown = tab_to(&mut app, &ctx, "2·SELECT name FROM b");
        assert_eq!(
            shown.iter().filter(|t| t.as_str() == "导出").count(),
            1,
            "第二条结果的导出按钮没跟上：{shown:?}"
        );
    }

    /// 把当前文件换成一个带 `auto` 标记的块，正文就是传进来的那句 SQL。
    fn auto_text(app: &mut EasyEditorApp, sql: &str) {
        let idx = app.active.unwrap();
        app.tabs[idx].text =
            format!("-- sql 探测 2026-01-01 10:00 [待执行] conn=本机 auto\n{sql}\n");
    }

    /// 把当前文件的正文换成一个 `auto` 块，发到指定那条连接。
    fn auto_conn(app: &mut EasyEditorApp, conn: &str) {
        let idx = app.active.unwrap();
        app.tabs[idx].text =
            format!("-- sql 探测 2026-01-01 10:00 [待执行] conn={conn} auto\nSELECT id FROM a\n");
    }

    #[test]
    fn an_auto_block_runs_itself_once_you_stop_typing() {
        let (ctx, mut app, _dir) = app_on_sql_block("ee_sql_auto");
        auto_text(&mut app, "SELECT id FROM a");
        let t0 = std::time::Instant::now();
        app.auto_tick(t0);
        assert_eq!(app.results.count(), 0, "刚发现就跑等于没防抖，字还没打完");
        app.auto_tick(t0);
        assert_eq!(app.results.count(), 0, "到点时间还没到，不该跑");

        app.auto_tick(t0 + AUTO_DEBOUNCE + std::time::Duration::from_millis(1));
        assert_eq!(app.results.count(), 1, "手停下来了，只读块该自己跑掉");
        settle_results(&mut app, &ctx);

        // 正文一个字没改，就不该再敲第二次门
        app.auto_tick(t0 + AUTO_DEBOUNCE * 4);
        assert_eq!(app.results.count(), 1, "同一份正文只自动跑一次");

        // 改了正文才值得再算一遍
        auto_text(&mut app, "SELECT id2 FROM a");
        app.auto_tick(t0 + AUTO_DEBOUNCE * 5);
        app.auto_tick(t0 + AUTO_DEBOUNCE * 8);
        assert_eq!(app.results.count(), 2, "改了 SQL 就该重算");
    }

    #[test]
    fn auto_never_runs_a_write_statement() {
        let (_ctx, mut app, _dir) = app_on_sql_block("ee_sql_auto_write");
        auto_text(&mut app, "DELETE FROM t");
        let t0 = std::time::Instant::now();
        app.auto_tick(t0);
        app.auto_tick(t0 + AUTO_DEBOUNCE * 4);
        assert_eq!(app.results.count(), 0, "标了 auto 的写操作也只能人手按");
        assert!(
            app.auto_ran.is_empty(),
            "没跑就不该记账，改了正文还得再看一眼"
        );
    }

    #[test]
    fn results_dock_stays_out_of_the_way_when_closed() {
        let (ctx, mut app, _dir) = app_on_sql_block("ee_sql_closed");
        let out = frame(&mut app, &ctx, vec![]);
        assert!(!texts(&out).iter().any(|t| t.starts_with("结果 · ")));
    }

    #[test]
    fn jump_back_from_a_result_brings_its_file_forward() {
        let (_ctx, mut app, dir) = app_in_folder("ee_sql_jump", "one\ntwo\nthree\n");
        let first = dir.join("a.txt");
        app.open_file(first.clone(), None);
        std::fs::write(dir.join("b.txt"), "b\n").unwrap();
        app.open_file(dir.join("b.txt"), None);

        app.jump_to_byte(&first, 4);
        assert_eq!(app.tabs[app.active.unwrap()].path, first, "文件没摊到前台");
        let idx = app.tabs.iter().position(|t| t.path == first).unwrap();
        assert_eq!(
            app.tabs[idx].pending_jump,
            Some(4),
            "字节位置要原样交给编辑器"
        );
    }

    #[test]
    fn insert_sql_block_binds_the_active_connection() {
        let (_ctx, mut app, _dir) = app_in_folder("ee_sql_insert", "hello\n");
        app.database = mock_database();
        app.open_file(_dir.join("a.txt"), None);
        app.insert_sql_block();
        let idx = app.active.unwrap();
        // 插入只是排了个待办，真正落地在下一帧的编辑器里
        assert!(app.tabs[idx].pending_edit.is_some());
        let edit = app.tabs[idx].pending_edit.take().unwrap();
        assert!(
            edit.insert.contains("conn=本机"),
            "头部要写死当前连接：{}",
            edit.insert
        );
    }

    /// 这一帧让 egui 往系统剪贴板写了什么（可能多条，按顺序）。
    fn copied(out: &egui::FullOutput) -> Vec<String> {
        out.platform_output
            .commands
            .iter()
            .filter_map(|cmd| match cmd {
                egui::OutputCommand::CopyText(text) => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    /// 点中屏上最靠上的那段这种文字。返回松手那一帧——
    /// 剪贴板这类「一次性请求」只出现在处理点击的那一帧里，往后就没了。
    fn click_first(
        app: &mut EasyEditorApp,
        ctx: &egui::Context,
        out: &egui::FullOutput,
        needle: &str,
    ) -> egui::FullOutput {
        let mut hits = seen(out)
            .into_iter()
            .filter(|(t, _)| t == needle)
            .map(|(_, r)| r.center())
            .collect::<Vec<_>>();
        assert!(!hits.is_empty(), "屏上找不到「{needle}」");
        hits.sort_by(|a, b| a.y.total_cmp(&b.y));
        frame(
            app,
            ctx,
            pointer(hits[0], egui::PointerButton::Primary, true),
        );
        frame(
            app,
            ctx,
            pointer(hits[0], egui::PointerButton::Primary, false),
        )
    }

    /// 点结果区的一个页签，换到那份结果，返回换完之后那一帧屏上的字。
    ///
    /// 换页签那一帧画的还是旧的那一份（`active` 是在内容画完之后才记账的），
    /// 所以要多空跑一帧，测试才是用户实际看到的样子。
    fn tab_to(app: &mut EasyEditorApp, ctx: &egui::Context, label: &str) -> Vec<String> {
        let out = frame(app, ctx, vec![]);
        click_first(app, ctx, &out, label);
        texts(&frame(app, ctx, vec![]))
    }

    /// 屏上「这一份结果是第几条」那个抬头里的语句预览出现了几次。
    ///
    /// 页签上的字带序号前缀（`1·SELECT …`），和抬头的纯语句是两个不同的字串，
    /// 用它就能分清「列在页签栏里」和「正摊在屏上」。
    fn on_screen(shown: &[String], stmt: &str) -> usize {
        shown.iter().filter(|t| t.as_str() == stmt).count()
    }

    /// 在屏上某一点按下一又松开（按下、松开各占一帧，egui 才认这是一次点击）。
    fn tap(app: &mut EasyEditorApp, ctx: &egui::Context, at: egui::Pos2) {
        frame(app, ctx, pointer(at, egui::PointerButton::Primary, true));
        frame(app, ctx, pointer(at, egui::PointerButton::Primary, false));
    }

    /// 自动执行的闸要按连接分，不能全局一刀切。
    ///
    /// 两条连接各有自己的工作线程，甲堵着三条不影响乙一秒出结果；
    /// 从前那句「有结果在跑就先不跑」把两条连接排成了独木桥。
    #[test]
    fn auto_blocks_are_gated_per_connection() {
        let (_ctx, mut app, _dir) = app_on_sql_block("ee_sql_auto_gate");
        app.database = DatabasePanel::with_hub(
            crate::db::DbHub::with_driver(|_| Ok(Box::new(crate::db::MockDriver::new()))),
            ["甲", "乙"]
                .iter()
                .map(|name| crate::db::ConnectionSpec {
                    name: (*name).into(),
                    database: "test".into(),
                    ..Default::default()
                })
                .collect(),
        );
        // 甲 那边压一条永远没回执的在跑
        let stuck = [results::Part {
            task: 900,
            stmt: "SELECT 堵着的".into(),
            range: None,
        }];
        app.results.start_run("甲", "手工", &stuck, None);
        assert!(app.results.busy_on("甲"));

        let t0 = std::time::Instant::now();

        // 乙 的自动块：甲 在跑也照样交
        auto_conn(&mut app, "乙");
        app.auto_tick(t0);
        app.auto_tick(t0 + AUTO_DEBOUNCE + std::time::Duration::from_millis(1));
        assert_eq!(app.results.count(), 2, "乙 不该被 甲 堵住");

        // 同一句换成 甲：这才真是在排队，先不交
        auto_conn(&mut app, "甲");
        let t1 = t0 + AUTO_DEBOUNCE * 4;
        app.auto_tick(t1);
        app.auto_tick(t1 + AUTO_DEBOUNCE * 4);
        assert_eq!(app.results.count(), 2, "甲 还在跑，这一句先压住");
    }

    #[test]
    fn opening_a_sql_file_turns_on_sql_mode() {
        let (_ctx, mut app, dir) = app_in_folder("ee_sql_mode_txt", "SELECT 1\n");
        std::fs::write(dir.join("q.sql"), "SELECT 1\n").unwrap();
        app.open_file(dir.join("q.sql"), None);
        assert!(
            app.tabs[app.active.unwrap()].sql_mode,
            "扩展名是 .sql 就该按 SQL 对待，不用再手动开"
        );
        // 同一个目录里的 .txt 不受牵连：模式是逐个标签的
        app.open_file(dir.join("a.txt"), None);
        assert!(
            !app.tabs[app.active.unwrap()].sql_mode,
            "普通文本文件不该被顺手当成查询"
        );
    }

    #[test]
    fn a_sql_file_with_no_block_runs_the_whole_file() {
        let (ctx, mut app, dir) = app_in_folder("ee_sql_mode_whole", "SELECT 1\n");
        app.database = mock_database();
        let file = dir.join("q.sql");
        std::fs::write(&file, "SELECT id FROM a;\nSELECT name FROM b;\n").unwrap();
        app.open_file(file, None);
        app.sql_run_block();
        assert_eq!(
            app.results.count(),
            2,
            "没有选区也没有块，SQL 模式该把整份文件当一批交出去"
        );
        assert!(
            app.status.contains("整个文件"),
            "状态栏要说清跑的是全文：{}",
            app.status
        );
        settle_results(&mut app, &ctx);
        let idx = app.active.unwrap();
        assert!(
            !app.tabs[idx].text.contains('['),
            "查询文件没有块头可回写：{}",
            app.tabs[idx].text
        );
    }

    #[test]
    fn a_plain_file_with_no_block_refuses_to_run_everything() {
        let (_ctx, mut app, _dir) = app_in_folder("ee_sql_mode_plain", "hello\n");
        app.database = mock_database();
        app.open_file(_dir.join("a.txt"), None);
        app.sql_run();
        assert_eq!(app.results.count(), 0, "普通文件整篇发出去太危险了");
        assert!(
            app.status.contains("SQL 模式"),
            "拒绝的话得指出出路：{}",
            app.status
        );
    }

    #[test]
    fn new_query_writes_a_bound_block_and_opens_it() {
        let (_ctx, mut app, dir) = app_in_folder("ee_sql_new_query", "x\n");
        app.database = mock_database();
        app.new_query(None);

        let idx = app.active.expect("新建的查询该摊到前台");
        let tab = &app.tabs[idx];
        let name = tab.path.file_name().unwrap().to_string_lossy().to_string();
        assert!(
            name.starts_with("query-") && name.ends_with(".sql"),
            "{name}"
        );
        assert!(tab.sql_mode, "新建的查询文件当然要按 SQL 对待");
        assert!(
            tab.text.starts_with("-- sql 查询") && tab.text.contains("conn=本机"),
            "首行要绑好默认连接：{}",
            tab.text
        );
        assert!(dir.join(&name).is_file(), "文件得真在磁盘上");
        assert!(
            app.status.contains(&format!("已新建 {name}")),
            "{}",
            app.status
        );

        // 同一秒里再点一次不能把刚建的那个盖掉
        let first = tab.path.clone();
        app.new_query(None);
        let second = app.tabs[app.active.unwrap()].path.clone();
        assert_ne!(second, first, "同一秒新建两次要有两个文件");
    }

    #[test]
    fn a_batch_of_results_gets_one_header_row() {
        let (ctx, mut app, _dir) = app_on_sql_block("ee_sql_batch_head");
        app.sql_run_block();
        settle_results(&mut app, &ctx);
        let shown = texts(&frame(&mut app, &ctx, vec![]));
        // 一批一行抬头，两条语句共用；条数写在上面，用户不用自己数面板
        assert_eq!(
            shown
                .iter()
                .filter(|t| t.as_str() == "本机 · 「查用户」 · 共 2 条")
                .count(),
            1,
            "批次抬头该出现正好一次：{shown:?}"
        );
        // 页签上各留一个序号，屏上的「第几条」只有当前这一页
        assert!(
            shown.iter().any(|t| t == "1·SELECT id FROM a")
                && shown.iter().any(|t| t == "2·SELECT name FROM b"),
            "一次跑两条要在页签栏各占一格，序号不能丢：{shown:?}"
        );
        assert!(shown.iter().any(|t| t == "1/2"));
        assert!(!shown.iter().any(|t| t == "2/2"));
        let shown = tab_to(&mut app, &ctx, "2·SELECT name FROM b");
        assert!(
            shown.iter().any(|t| t == "2/2"),
            "换到第二页，抬头里要写这是 2/2：{shown:?}"
        );
    }

    #[test]
    fn the_copy_button_sends_the_result_to_the_clipboard() {
        let (ctx, mut app, _dir) = app_on_sql_block("ee_sql_copy");
        app.sql_run();
        settle_results(&mut app, &ctx);
        let out = frame(&mut app, &ctx, vec![]);
        let out = click_first(&mut app, &ctx, &out, "复制");
        let board = copied(&out);
        assert_eq!(board.len(), 1, "点一下只该写一次剪贴板：{board:?}");
        assert_eq!(
            board[0], "id\tname\n1\t行0\n2\t行1",
            "制表符分隔、带表头，粘进表格软件直接分列"
        );
    }

    /// 行首的 ▸ 不再往表格里塞一块明细：那块收不回去，还老被表格自己的裁剪切掉。
    /// 现在改成在屏幕右边单独开一个窗口：点别处自己收，钉住就一直开着。
    #[test]
    fn the_row_arrow_opens_a_detail_window_on_the_right() {
        let (ctx, mut app, _dir) = app_on_sql_block("ee_sql_expand");
        app.sql_run_block();
        settle_results(&mut app, &ctx);
        let out = frame(&mut app, &ctx, vec![]);
        // 表格每行行首那个 ▸（页签栏和标题行都没有这一串）
        let mut arrows = seen(&out)
            .into_iter()
            .filter(|(t, _)| t == "▸")
            .map(|(_, r)| r.center())
            .collect::<Vec<_>>();
        assert_eq!(arrows.len(), 2, "当前那页两行，就该两个 ▸：{arrows:?}");
        arrows.sort_by(|a, b| a.y.total_cmp(&b.y));
        tap(&mut app, &ctx, arrows[0]);

        let shown = seen(&frame(&mut app, &ctx, vec![]));
        let title = shown.iter().find(|(t, _)| t == "详情 · 第 1 行");
        let Some((_, rect)) = title else {
            panic!(
                "行首 ▸ 没把详情窗口开出来：{:?}",
                shown.iter().map(|(t, _)| t).collect::<Vec<_>>()
            );
        };
        assert!(
            rect.center().x > SCREEN.x / 2.0,
            "详情窗口该钉在屏幕右边，实测标题在 x={}",
            rect.center().x
        );
        let words = shown.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>();
        assert!(
            words.contains(&"id =") && words.contains(&"name ="),
            "窗口里没把「列名 = 完整值」摊出来：{words:?}"
        );
        assert!(
            words.contains(&"复制整行"),
            "窗口里没带拷贝整行那一条：{words:?}"
        );

        // 看完不必回去点那个 ▸：窗口外面随便一下——这里点页签栏——就把它收掉
        let out = frame(&mut app, &ctx, vec![]);
        click_first(&mut app, &ctx, &out, "1·SELECT id FROM a");
        let shown = texts(&frame(&mut app, &ctx, vec![]));
        assert!(
            !shown.iter().any(|t| t.starts_with("详情 · ")),
            "点了窗口外面还开着：{shown:?}"
        );

        // 钉住的那一份不吃这一下：对着两列长值来回比的时候，点哪儿都不该散
        tap(&mut app, &ctx, arrows[0]);
        let out = frame(&mut app, &ctx, vec![]);
        click_first(&mut app, &ctx, &out, "钉住");
        assert!(
            app.results.panels()[0].detail.is_some_and(|d| d.pinned),
            "那一下没勾上「钉住」"
        );
        let out = frame(&mut app, &ctx, vec![]);
        click_first(&mut app, &ctx, &out, "1·SELECT id FROM a");
        let shown = texts(&frame(&mut app, &ctx, vec![]));
        assert!(
            shown.iter().any(|t| t == "详情 · 第 1 行"),
            "钉住了还被点外面收走：{shown:?}"
        );

        // 同一个 ▸ 再点一下照样能手动收掉（没钉住时是 toggling，钉住后也得收）
        tap(&mut app, &ctx, arrows[0]);
        let shown = texts(&frame(&mut app, &ctx, vec![]));
        assert!(
            !shown.iter().any(|t| t.starts_with("详情 · ")),
            "再点同一个 ▸ 没收掉详情：{shown:?}"
        );
    }

    #[test]
    fn the_database_tab_names_the_default_connection() {
        let (ctx, mut app, _dir) = app_on_sql_block("ee_sql_default");
        app.side = SideTab::Database;
        let shown = texts(&frame(&mut app, &ctx, vec![]));
        assert!(
            shown.iter().any(|t| t == "默认连接"),
            "侧栏没交代 SQL 发往哪条：{shown:?}"
        );
        assert_eq!(
            shown.iter().filter(|t| t.as_str() == "默认").count(),
            1,
            "「默认」牌子只能挂一块：{shown:?}"
        );
    }

    #[test]
    fn the_status_bar_shows_where_this_files_sql_goes() {
        let (ctx, mut app, dir) = app_in_folder("ee_sql_chip", "SELECT 1\n");
        app.database = mock_database();
        std::fs::write(dir.join("q.sql"), "SELECT 1\n").unwrap();
        app.open_file(dir.join("q.sql"), None);
        let shown = texts(&frame(&mut app, &ctx, vec![]));
        assert!(
            shown.iter().any(|t| t == "SQL → 本机/test ○"),
            "状态栏要交代 SQL 发往哪条、那条连着没有：{shown:?}"
        );
        // 普通文本、又没块，就别在状态栏挂个跟当前文件无关的尾巴
        app.open_file(dir.join("a.txt"), None);
        let shown = texts(&frame(&mut app, &ctx, vec![]));
        assert!(
            !shown.iter().any(|t| t.starts_with("SQL → ")),
            "这个文件跟数据库没关系：{shown:?}"
        );
    }

    /// 配置文件读坏了不能只打个日志就完事。走真输入管线量两件事：
    /// 弹窗画没画出来，以及「知道了」点下去收不收掉。
    #[test]
    fn a_damaged_config_speaks_up_until_acknowledged() {
        let (ctx, mut app, dir) = app_in_folder("ee_config_trouble", "SELECT 1\n");
        app.config.trouble = None;
        let shown = texts(&frame(&mut app, &ctx, vec![]));
        assert!(
            !shown.iter().any(|t| t == "配置文件没读出来"),
            "配置没毛病时不该弹这个窗：{shown:?}"
        );

        app.config.trouble = Some(config::Trouble::Unparked {
            file: dir.join("config.json"),
            reason: "unknown variant `mysql`, expected `Mysql` at line 1 column 42".into(),
        });
        // 头两帧只在量窗口尺寸，尺寸落定 egui 才让窗口里的东西参与交互
        for _ in 0..3 {
            frame(&mut app, &ctx, vec![]);
        }
        let shot = frame(&mut app, &ctx, vec![]);
        let shown = texts(&shot);
        assert!(
            shown.iter().any(|t| t == "配置文件没读出来"),
            "弹窗标题要在：{shown:?}"
        );
        assert!(
            shown.iter().any(|t| t.contains("unknown variant")),
            "serde 的原话要交给用户，不然他只知道自己改坏了、不知道改哪：{shown:?}"
        );
        assert!(
            shown.iter().any(|t| t.contains("都不会写回")),
            "原文没保住时「这次不落盘」得写在脸上：{shown:?}"
        );

        let ok = find_text(&shot, "知道了")
            .expect("得有「知道了」按钮")
            .center();
        frame(
            &mut app,
            &ctx,
            pointer(ok, egui::PointerButton::Primary, true),
        );
        frame(
            &mut app,
            &ctx,
            pointer(ok, egui::PointerButton::Primary, false),
        );
        // 抬手那一帧弹窗还画着（点完才记下的标记），收掉是下一帧的事
        let shot = frame(&mut app, &ctx, vec![]);
        assert!(
            !texts(&shot).iter().any(|t| t == "配置文件没读出来"),
            "点过以后不该再弹"
        );
        // 收掉弹窗不等于撤销保护：原文没保住就永远不许写回
        assert!(app.config.trouble.as_ref().is_some_and(|t| t.blocks_save()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 在临时根下面单独开一个目录，用完由测试自己删。
    fn temp_dir(dir_name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(dir_name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 侧栏那个文件夹只是入口，不是「能打开哪些文件」的边界。
    ///
    /// 打开工作区之外的文件时，工作区、索引、终端 cwd、已经开着的标签都必须原样不动——
    /// 用户从资源管理器双击一个 Downloads 里的笔记，不该顺手把整个 Downloads 变成工作区。
    #[test]
    fn opening_a_file_outside_the_workspace_leaves_the_workspace_alone() {
        let (_ctx, mut app, dir) = app_in_folder("easy-editor-app-outside-root", "in workspace\n");
        let outside = temp_dir("easy-editor-app-outside-target");
        std::fs::write(outside.join("note.txt"), "elsewhere\n").unwrap();

        let root_before = app.root.clone();
        let index_before = app.index.clone();
        let cwd_before = app.terminal.cwd.clone();

        app.open_file(dir.join("a.txt"), None);
        app.open_file(outside.join("note.txt"), None);

        assert_eq!(app.root, root_before, "打开工作区外的文件把工作区换了");
        assert_eq!(
            app.index, index_before,
            "打开工作区外的文件重扫了索引"
        );
        assert_eq!(app.terminal.cwd, cwd_before, "打开工作区外的文件动了终端 cwd");
        assert_eq!(app.tabs.len(), 2, "工作区外的文件没进标签条");
        assert_eq!(app.tabs[0].path, dir.join("a.txt"), "原有的标签被顶掉了");
        assert_eq!(app.tabs[1].text, "elsewhere\n");

        // 保存写回的是它自己的绝对路径，而不是工作区里同名的位置
        app.tabs[1].text = "edited elsewhere\n".into();
        app.active = Some(1);
        app.save_active();
        assert_eq!(
            std::fs::read_to_string(outside.join("note.txt")).unwrap(),
            "edited elsewhere\n",
            "保存写到了别的地方：{}",
            app.status
        );
        assert!(!dir.join("note.txt").exists(), "反倒在工作区里造了个同名文件");

        std::fs::remove_dir_all(&outside).unwrap();
    }

    /// 命令行带一个文件路径（`.txt` 双击就是这条路）只开这一个文件，不设工作区。
    #[test]
    fn bootstrap_with_a_file_argument_does_not_set_a_workspace() {
        let dir = temp_dir("easy-editor-app-cli-file");
        std::fs::write(dir.join("note.txt"), "double clicked\n").unwrap();

        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(1.0);
        let app = EasyEditorApp::bootstrap(&ctx, Some(dir.join("note.txt")));

        assert!(app.root.is_none(), "双击一个文件却把它的目录设成了工作区");
        assert!(app.tree.is_none(), "双击一个文件却建了文件树");
        assert!(app.index.is_empty(), "双击一个文件却扫了整条目录");
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.tabs[0].path, dir.join("note.txt"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 从资源管理器拖一个文件到窗口上 = 走「打开文件」那条路：只进标签，工作区不动。
    #[test]
    fn dropping_a_file_opens_it_without_touching_the_workspace() {
        let (ctx, mut app, _dir) = app_in_folder("easy-editor-app-drop-file", "in workspace\n");
        let outside = temp_dir("easy-editor-app-drop-file-target");
        std::fs::write(outside.join("note.txt"), "dragged in\n").unwrap();

        let root_before = app.root.clone();
        let index_before = app.index.clone();

        frame_raw(&mut app, &ctx, drop_raw(&[outside.join("note.txt")]));

        assert_eq!(app.root, root_before, "拖进来一个文件却换了工作区");
        assert_eq!(app.index, index_before, "拖进来一个文件却重扫了索引");
        assert_eq!(app.tabs.len(), 1, "拖进来的文件没进标签条：{}", app.status);
        assert_eq!(app.tabs[0].text, "dragged in\n");

        // drop 事件一帧只交一次，下一帧不该再开一遍
        frame(&mut app, &ctx, vec![]);
        assert_eq!(app.tabs.len(), 1, "同一个文件被开了两遍");

        std::fs::remove_dir_all(&outside).unwrap();
    }

    /// 拖一个文件夹进来 = 走「打开文件夹」那条路：换工作区、清标签。
    #[test]
    fn dropping_a_folder_switches_the_workspace() {
        let (ctx, mut app, dir) = app_in_folder("easy-editor-app-drop-folder", "a\n");
        let other = temp_dir("easy-editor-app-drop-folder-target");
        std::fs::write(other.join("b.txt"), "b\n").unwrap();
        app.open_file(dir.join("a.txt"), None);
        assert_eq!(app.tabs.len(), 1);

        frame_raw(&mut app, &ctx, drop_raw(std::slice::from_ref(&other)));

        assert_eq!(
            app.root,
            Some(other.clone()),
            "拖进来的是文件夹，工作区却没换：{}",
            app.status
        );
        assert!(app.tabs.is_empty(), "换了工作区旧标签还在");
        assert_eq!(app.index.len(), 1, "新工作区没重新扫：{:?}", app.index);
        assert!(
            app.status.contains("easy-editor-app-drop-folder-target"),
            "状态栏没说换了工作区：{}",
            app.status
        );

        std::fs::remove_dir_all(&other).unwrap();
    }

    /// 一次拖进来既有文件夹又有文件：先认文件夹再开文件，文件不能转头被
    /// `open_workspace` 清空——不管资源管理器把哪一样排在前面。
    #[test]
    fn dropping_a_folder_with_files_keeps_the_files() {
        for folder_first in [true, false] {
            let (ctx, mut app, _) = app_in_folder("easy-editor-app-drop-mix", "a\n");
            let folder = temp_dir("easy-editor-app-drop-mix-target");
            std::fs::write(folder.join("b.txt"), "b\n").unwrap();
            let file = temp_dir("easy-editor-app-drop-mix-file");
            std::fs::write(file.join("c.txt"), "c\n").unwrap();

            let paths = if folder_first {
                vec![folder.clone(), file.join("c.txt")]
            } else {
                vec![file.join("c.txt"), folder.clone()]
            };
            frame_raw(&mut app, &ctx, drop_raw(&paths));

            assert_eq!(
                app.root,
                Some(folder.clone()),
                "folder_first={folder_first}：文件夹没认成工作区"
            );
            assert_eq!(
                app.tabs.len(),
                1,
                "folder_first={folder_first}：同批拖进来的文件没了，状态={}",
                app.status
            );
            assert_eq!(app.tabs[0].path, file.join("c.txt"));

            std::fs::remove_dir_all(&folder).unwrap();
            std::fs::remove_dir_all(&file).unwrap();
        }
    }

    /// 手还没松开时给一句准信；松手或拖出窗口之后提示得走。
    #[test]
    fn hovering_a_drag_shows_the_drop_hint() {
        let (ctx, mut app, dir) = app_in_folder("easy-editor-app-drop-hint", "a\n");

        // egui 对新出现的 Area 先跑一帧只量不画的 sizing pass，所以第一帧看不到提示
        let paths = [dir.join("a.txt")];
        frame_raw(&mut app, &ctx, hover_raw(&paths));
        let out = frame_raw(&mut app, &ctx, hover_raw(&paths));
        assert!(
            find_text(&out, "松开打开 a.txt").is_some(),
            "悬停一个文件却没有提示：{:?}",
            texts(&out)
        );
        assert!(
            texts(&out)
                .iter()
                .any(|t| t == "文件夹换到工作区，文件进标签条"),
            "提示里没说清楚松手会发生什么：{:?}",
            texts(&out)
        );

        // 一次拖好几样：路径名各写一遍太挤，只报数量
        let out = frame_raw(
            &mut app,
            &ctx,
            hover_raw(&[dir.join("a.txt"), dir.join("b.txt")]),
        );
        assert!(
            find_text(&out, "松开打开 2 项").is_some(),
            "悬停多项时没报数量：{:?}",
            texts(&out)
        );

        let out = frame(&mut app, &ctx, vec![]);
        assert!(
            find_text(&out, "松开打开 a.txt").is_none(),
            "松手以后提示还赖着"
        );
    }

    /// 两个不同目录下的同名文件一起开着，标签条得能认出哪一个是哪一个。
    #[test]
    fn tab_bar_distinguishes_same_named_files() {
        let (ctx, mut app, dir) = app_in_folder("easy-editor-app-dup-names", "a\n");
        let other = temp_dir("easy-editor-app-dup-names-other");
        std::fs::write(dir.join("note.txt"), "in the workspace\n").unwrap();
        std::fs::write(other.join("note.txt"), "outside\n").unwrap();

        app.open_file(dir.join("note.txt"), None);
        app.open_file(other.join("note.txt"), None);
        let out = frame(&mut app, &ctx, vec![]);
        let shown = texts(&out);
        for dir_name in [
            "easy-editor-app-dup-names",
            "easy-editor-app-dup-names-other",
        ] {
            assert!(
                shown
                    .iter()
                    .any(|t| t.ends_with(&format!("…/{dir_name}/note.txt"))),
                "标签没把 {dir_name} 那份 note.txt 区分出来：{shown:?}"
            );
        }

        std::fs::remove_dir_all(&other).unwrap();
    }

    /// 「打开文件夹」「打开文件」各自只出现一次，就是顶栏那一颗。
    ///
    /// 欢迎页和侧栏空态以前都摆过同样一排按钮，等于把同一件事画两三遍；欢迎页那排更糟：
    /// `vertical_centered` 里嵌 `horizontal` 不居中，两颗按钮直接贴到面板左边。
    #[test]
    fn open_buttons_appear_only_in_the_toolbar() {
        let (ctx, mut app, dir) = app_in_folder("easy-editor-app-single-open-entry", "a\n");
        let count = |out: &egui::FullOutput, needle: &str| {
            texts(out)
                .iter()
                .filter(|t| t.as_str() == needle)
                .count()
        };

        // 一个标签都没开：中央是欢迎页
        let out = frame(&mut app, &ctx, vec![]);
        assert_eq!(count(&out, "打开文件"), 1, "欢迎页又摆了一颗「打开文件」");
        assert_eq!(
            count(&out, "打开文件夹"),
            1,
            "欢迎页又摆了一颗「打开文件夹」"
        );

        // 连工作区都没有：侧栏空态只给一句提示
        app.root = None;
        app.tree = None;
        app.index.clear();
        let out = frame(&mut app, &ctx, vec![]);
        assert_eq!(count(&out, "打开文件"), 1, "侧栏空态又摆了一颗「打开文件」");
        assert_eq!(
            count(&out, "打开文件夹"),
            1,
            "侧栏空态又摆了一颗「打开文件夹」"
        );
        assert!(
            texts(&out).iter().any(|t| t.contains("尚未打开文件夹")),
            "侧栏空态那句去哪儿开的提示也得留着"
        );

        // 开了文件以后仍然只有顶栏那一颗
        app.open_file(dir.join("a.txt"), None);
        let out = frame(&mut app, &ctx, vec![]);
        assert_eq!(count(&out, "打开文件"), 1);
        assert_eq!(count(&out, "打开文件夹"), 1);
    }

    /// 关一个没保存的标签时，弹窗光问「note.txt 要保存吗」不够——
    /// 同名文件分散在两个目录时，写错地方比不保存更麻烦，所以整条路径要摊开。
    #[test]
    fn close_confirm_shows_which_file_is_being_saved() {
        let (ctx, mut app, dir) = app_in_folder("easy-editor-app-close-confirm-path", "a\n");
        let other = temp_dir("easy-editor-app-close-confirm-other");
        std::fs::write(dir.join("note.txt"), "in the workspace\n").unwrap();
        std::fs::write(other.join("note.txt"), "outside\n").unwrap();

        app.open_file(dir.join("note.txt"), None);
        app.open_file(other.join("note.txt"), None);
        app.tabs[1].dirty = true;
        app.close_tab(1);
        assert!(app.pending_close.is_some(), "脏标签没弹出确认");

        let out = frame(&mut app, &ctx, vec![]);
        let shown = texts(&out);
        let full = other.join("note.txt").display().to_string();
        assert!(
            shown.iter().any(|t| t.contains(&full)),
            "确认弹窗里没有完整路径 {full}：{shown:?}"
        );
        assert!(
            shown
                .iter()
                .any(|t| t.starts_with("…/easy-editor-app-close-confirm-other/note.txt")),
            "确认弹窗的标题行没区分同名文件：{shown:?}"
        );

        std::fs::remove_dir_all(&other).unwrap();
    }
}
