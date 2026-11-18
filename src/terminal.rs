//! 终端：portable-pty 启动进程，alacritty_terminal 做 VT 解析与屏幕模型。
//!
//! 输入的关键在于 egui 的焦点系统：必须用**同一个** widget id 既注册可聚焦区域
//! 又请求焦点，否则请求会打在一个不存在的 id 上，下一帧就被清理掉，
//! 表现为「终端能显示、点进去也看不出异常，但打字完全没反应」。

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Grid, Scroll};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Config as TermConfig, Term};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, Rgb};
use eframe::egui::{self, Color32, FontId, Rect, Sense, Ui, Vec2};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde::{Deserialize, Serialize};

use crate::theme;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Dims {
    cols: usize,
    rows: usize,
}

/// 滚轮一格滚几行终端历史。
const WHEEL_LINES_PER_NOTCH: f32 = 3.0;
/// 触控板按像素上报滚动量时，多少像素折算成一行。
const WHEEL_PIXELS_PER_LINE: f32 = 14.0;
/// 滚动条可见部分的宽度。
const SB_BAR_W: f32 = 6.0;
/// 滚动条可点范围的宽度（比可见部分宽，好瞄准）。
const SB_HIT_W: f32 = 12.0;
/// 滑块最短高度：历史再长也留一块点得到的地方。
const SB_MIN_THUMB: f32 = 24.0;

/// 终端里的一个格子位置。行号是**绝对行**（含回滚历史），
/// 这样滚动之后选区不会跟着屏幕跑。
#[derive(Clone, Copy, PartialEq, Eq)]
struct SelPoint {
    line: i32,
    col: usize,
}

#[derive(Clone, Copy)]
struct Selection {
    anchor: SelPoint,
    end: SelPoint,
}

impl Selection {
    fn ordered(self) -> (SelPoint, SelPoint) {
        if (self.anchor.line, self.anchor.col) <= (self.end.line, self.end.col) {
            (self.anchor, self.end)
        } else {
            (self.end, self.anchor)
        }
    }
}

/// 一次滚轮/触控板事件折算成多少行。
///
/// 正方向是「往回滚」（Windows 上滚轮向上为正值），和 `Scroll::Delta` 一致：
/// 行数加到 `display_offset` 上，也就是往回滚历史的方向走。
fn wheel_delta_lines(unit: egui::MouseWheelUnit, delta_y: f32, rows: usize) -> f32 {
    match unit {
        egui::MouseWheelUnit::Line => delta_y * WHEEL_LINES_PER_NOTCH,
        egui::MouseWheelUnit::Point => delta_y / WHEEL_PIXELS_PER_LINE,
        egui::MouseWheelUnit::Page => delta_y * rows as f32,
    }
}

/// 攒够一整行才滚：返回（这一帧滚几行，留下多少零头）。
fn take_lines(acc: f32) -> (i32, f32) {
    let steps = acc.trunc() as i32;
    (steps, acc - steps as f32)
}

/// 滚动条滑块的位置和长度。
///
/// 长度按「可见行 / 总行」比例算，位置按回滚进度算：
/// `offset == history` 滚到最上面（滑块贴轨道顶），`offset == 0` 是最新输出（贴轨道底）。
fn thumb_rect(track: Rect, history: usize, offset: usize, rows: usize) -> Rect {
    let total = (history + rows).max(1) as f32;
    let height = (track.height() * (rows as f32 / total))
        .max(SB_MIN_THUMB)
        .min(track.height());
    let span = (track.height() - height).max(0.0);
    let progress = if history == 0 {
        1.0
    } else {
        (history - offset.min(history)) as f32 / history as f32
    };
    Rect::from_min_size(
        egui::pos2(
            track.center().x - SB_BAR_W * 0.5,
            track.top() + progress * span,
        ),
        Vec2::new(SB_BAR_W, height),
    )
}

/// 把滑块的顶边 y 换回 `display_offset`，和 [`thumb_rect`] 互为逆运算。
fn offset_from_thumb_top(track: Rect, history: usize, thumb_h: f32, top: f32) -> usize {
    let span = (track.height() - thumb_h).max(1.0);
    let progress = ((top - track.top()) / span).clamp(0.0, 1.0);
    ((1.0 - progress) * history as f32).round() as usize
}

impl Dimensions for Dims {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// 可选的 shell。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShellKind {
    Pwsh,
    WindowsPowerShell,
    Cmd,
    GitBash,
    Wsl,
}

impl ShellKind {
    /// 列表顺序即偏好顺序。
    pub const ALL: &'static [ShellKind] = &[
        Self::Pwsh,
        Self::WindowsPowerShell,
        Self::Cmd,
        Self::GitBash,
        Self::Wsl,
    ];

    /// 配置文件里用的稳定 id，别和 `label()` 混用——label 是给人看的中文。
    pub fn id(self) -> &'static str {
        match self {
            Self::Pwsh => "pwsh",
            Self::WindowsPowerShell => "powershell",
            Self::Cmd => "cmd",
            Self::GitBash => "gitbash",
            Self::Wsl => "wsl",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.id() == id)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Pwsh => "PowerShell 7",
            Self::WindowsPowerShell => "Windows PowerShell",
            Self::Cmd => "命令提示符",
            Self::GitBash => "Git Bash",
            Self::Wsl => "WSL",
        }
    }

    /// 自动探测到的可执行文件（不含配置里的覆盖路径）。
    pub fn detected(self) -> Option<PathBuf> {
        match self {
            Self::Pwsh => which("pwsh.exe"),
            Self::WindowsPowerShell => which("powershell.exe"),
            Self::Cmd => {
                which("cmd.exe").or_else(|| std::env::var_os("COMSPEC").map(PathBuf::from))
            }
            Self::GitBash => find_git_bash(),
            Self::Wsl => which("wsl.exe"),
        }
    }

    /// 实际要启动的程序：配置里写了就用配置的，否则自动探测。
    fn program(self, settings: &TerminalSettings) -> Option<PathBuf> {
        settings.program(self).or_else(|| self.detected())
    }

    /// 可用的 shell 列表。配置里显式写了路径的种类即使探测不到也算可用，
    /// 这样路径写错时会在启动时报错，而不是静默消失。
    pub fn detect(settings: &TerminalSettings) -> Vec<Self> {
        Self::ALL
            .iter()
            .copied()
            .filter(|k| k.program(settings).is_some())
            .collect()
    }

    fn build(self, cwd: &Path, settings: &TerminalSettings) -> Result<CommandBuilder, String> {
        let program = self.program(settings).ok_or_else(|| {
            format!(
                "找不到 {} 的可执行文件，可在设置「终端」里指定路径",
                self.label()
            )
        })?;
        let mut cmd = CommandBuilder::new(program);
        // 启动参数：配置优先；配置留空才用下面这套内置参数
        if let Some(args) = settings.args(self) {
            for arg in args {
                cmd.arg(arg);
            }
        } else {
            match self {
                Self::Pwsh | Self::WindowsPowerShell => {
                    cmd.arg("-NoLogo");
                }
                Self::GitBash => {
                    cmd.arg("-i");
                    cmd.arg("-l");
                }
                Self::Wsl => {
                    // wsl.exe 不认 Windows 路径，用 --cd 传目录
                    cmd.arg("--cd");
                    cmd.arg(cwd.to_string_lossy().to_string());
                }
                Self::Cmd => {}
            }
        }
        // WSL 的落点靠 --cd，给它设 Windows 工作目录没有意义；其余 shell 直接设 cwd
        if self != Self::Wsl {
            cmd.cwd(cwd);
        }
        cmd.env("TERM", "xterm-256color");
        Ok(cmd)
    }
}

/// 用户自己加的一条 shell：名字、程序路径、启动参数，整条存在 `config.json` 里。
///
/// 内置那几种只能在「程序路径」上覆盖，改不了名字也加不出第六种——
/// 比如装在别处的 MSYS2、busybox、或者想直接进某个项目的 bash。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CustomShell {
    /// 认这条用的稳定 id（`custom-1` 这样）。改名、改路径都不动它，
    /// `terminal_shell` 里存的偏好才不会因为改个显示名就失效。
    pub id: String,
    /// 下拉与面板标题里显示的名字
    pub name: String,
    /// 可执行文件路径
    pub program: String,
    /// 启动参数，空格分隔，带空格的用双引号包住
    pub args: String,
}

impl CustomShell {
    /// 显示用的名字。没填给个占位，否则下拉里会出现一项空白，没法点也没法认。
    pub fn display_name(&self) -> &str {
        let name = self.name.trim();
        if name.is_empty() {
            "未命名 shell"
        } else {
            name
        }
    }
}

/// 一块终端实际用哪个 shell：内置那几种，或者用户自己加的那一条。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Shell {
    Builtin(ShellKind),
    Custom(CustomShell),
}

impl Shell {
    /// 写进 `config.terminal_shell` 的值。自定义带 `custom:` 前缀，和内置的种类 id 撞不上。
    pub fn id(&self) -> String {
        match self {
            Self::Builtin(kind) => kind.id().to_string(),
            Self::Custom(c) => format!("custom:{}", c.id),
        }
    }

    pub fn label(&self) -> String {
        match self {
            Self::Builtin(kind) => kind.label().to_string(),
            Self::Custom(c) => c.display_name().to_string(),
        }
    }

    /// 可用的 shell：内置探测结果 + 用户自己加的每一条。
    ///
    /// 自定义那些**全都**列进来，路径还空着的也列：下拉里看不见就没法接着填，
    /// 起不来会在启动时把话说清楚。这和「路径写错的内置种类也留在列表里」是同一个取向。
    pub fn detect(settings: &TerminalSettings) -> Vec<Self> {
        let mut out: Vec<Self> = ShellKind::detect(settings)
            .into_iter()
            .map(Self::Builtin)
            .collect();
        out.extend(settings.custom.iter().cloned().map(Self::Custom));
        out
    }

    fn build(&self, cwd: &Path, settings: &TerminalSettings) -> Result<CommandBuilder, String> {
        match self {
            Self::Builtin(kind) => kind.build(cwd, settings),
            Self::Custom(c) => {
                let program = c.program.trim();
                if program.is_empty() {
                    return Err(format!(
                        "自定义 shell「{}」还没填程序路径，到设置「终端」里填一下",
                        c.display_name()
                    ));
                }
                let mut cmd = CommandBuilder::new(program);
                for arg in split_args(&c.args) {
                    cmd.arg(arg);
                }
                cmd.cwd(cwd);
                cmd.env("TERM", "xterm-256color");
                Ok(cmd)
            }
        }
    }
}

/// 终端设置：程序路径与启动参数按 shell 种类覆盖，外加用户自己加的 shell，都来自 `config.json`。
/// 缺省即自动探测 + 内置参数，所以配置文件里只会有用户真正改过的条目。
#[derive(Clone, Debug, Default)]
pub struct TerminalSettings {
    /// 种类 id -> 可执行文件路径
    pub programs: BTreeMap<String, String>,
    /// 种类 id -> 启动参数字符串（空格分隔，双引号包住带空格的参数）
    pub args: BTreeMap<String, String>,
    /// 用户自己加的 shell，顺序即下拉里的顺序
    pub custom: Vec<CustomShell>,
}

impl TerminalSettings {
    pub fn from_config(
        programs: &BTreeMap<String, String>,
        args: &BTreeMap<String, String>,
        custom: &[CustomShell],
    ) -> Self {
        Self {
            programs: programs.clone(),
            args: args.clone(),
            custom: custom.to_vec(),
        }
    }

    /// 写回配置，顺手丢掉空串，配置文件才不会积累噪音。
    pub fn to_config(&self) -> (BTreeMap<String, String>, BTreeMap<String, String>, Vec<CustomShell>) {
        let clean = |map: &BTreeMap<String, String>| {
            map.iter()
                .filter(|(_, v)| !v.trim().is_empty())
                .map(|(k, v)| (k.clone(), v.trim().to_string()))
                .collect::<BTreeMap<_, _>>()
        };
        let customs = self
            .custom
            .iter()
            .filter(|c| !c.id.trim().is_empty())
            .map(|c| CustomShell {
                id: c.id.trim().to_string(),
                name: c.name.trim().to_string(),
                program: c.program.trim().to_string(),
                args: c.args.trim().to_string(),
            })
            .collect();
        (clean(&self.programs), clean(&self.args), customs)
    }

    /// 给新加的自定义 shell 取一个没被占用的 id。
    fn next_custom_id(&self) -> String {
        (1..)
            .map(|n| format!("custom-{n}"))
            .find(|id| !self.custom.iter().any(|c| &c.id == id))
            .unwrap_or_else(|| "custom".to_string())
    }

    fn program(&self, kind: ShellKind) -> Option<PathBuf> {
        self.programs
            .get(kind.id())
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
    }

    fn args(&self, kind: ShellKind) -> Option<Vec<String>> {
        let raw = self.args.get(kind.id())?;
        if raw.trim().is_empty() {
            return None;
        }
        Some(split_args(raw))
    }
}

/// 按空白切分参数，双引号里的空格算一个整体：`-c "echo hi"` → `["-c", "echo hi"]`。
pub fn split_args(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut started = false;
    for ch in text.chars() {
        match ch {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            c if c.is_whitespace() && !quoted => {
                if started {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            c => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(cur);
    }
    out
}

fn find_git_bash() -> Option<PathBuf> {
    // Git for Windows 里 bash 一定和 git.exe 同一次安装；顺着 git.exe 往上找比在 PATH 里
    // 瞎摸靠谱 —— PATH 里可能混进 System32 那个 WSL 启动桩，拿它当 Git Bash 起不来。
    if let Some(git) = which("git.exe") {
        for dir in [git.parent(), git.parent().and_then(|p| p.parent())] {
            if let Some(root) = dir {
                let candidate = root.join("bin").join("bash.exe");
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    let candidates = [
        r"C:\Program Files\Git\bin\bash.exe",
        r"C:\Program Files (x86)\Git\bin\bash.exe",
        r"C:\Program Files\Git\usr\bin\bash.exe",
    ];
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
        .or_else(|| which("bash.exe").filter(|p| !looks_like_wsl_stub(p)))
}

/// `C:\Windows\System32\bash.exe` 只是转交给 WSL 的桩，不是 Git Bash。
fn looks_like_wsl_stub(path: &Path) -> bool {
    path.to_string_lossy()
        .replace('/', "\\")
        .to_ascii_lowercase()
        .contains("\\windows\\system32\\")
}

fn which(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

enum Msg {
    Bytes(Vec<u8>),
    PtyWrite(Vec<u8>),
    Title(String),
    Exit,
}

#[derive(Clone)]
struct Listener {
    tx: Sender<Msg>,
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        match event {
            Event::Title(title) => {
                let _ = self.tx.send(Msg::Title(title));
            }
            Event::PtyWrite(text) => {
                let _ = self.tx.send(Msg::PtyWrite(text.into_bytes()));
            }
            Event::Exit | Event::ChildExit(_) => {
                let _ = self.tx.send(Msg::Exit);
            }
            _ => {}
        }
    }
}

pub struct Terminal {
    /// 面板用的稳定 id：分屏之后终端不再靠数组下标定位，
    /// 关掉中间某块时其他面板的 id 不会跟着挪。
    pub id: u64,
    pub title: String,
    /// 这个终端实际使用的 shell
    pub shell: Shell,
    /// 起进程时所在的目录。shell 里再 `cd` 我们跟踪不到，同一扇窗口里再切一格时
    /// 照这个目录起新进程，用户预期就是「旁边那格和我在同一个地方」。
    pub cwd: PathBuf,
    term: Term<Listener>,
    processor: Processor,
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    rx: Receiver<Msg>,
    dims: Dims,
    pub exited: bool,
    /// 滚轮攒下的不足一行的零头
    scroll_acc: f32,
    /// 正在拖滚动条时，指针相对滑块顶边的位置；没拖是 None
    sb_grab: Option<f32>,
    /// 鼠标拖选的范围（绝对行坐标）
    selection: Option<Selection>,
    /// 下一帧请求焦点（新建终端后自动聚焦，省得用户还要点一下）
    pub focus_requested: bool,
}

impl Terminal {
    pub fn spawn(
        id: u64,
        cwd: &Path,
        cols: usize,
        rows: usize,
        shell: Shell,
        settings: &TerminalSettings,
    ) -> Result<Self, String> {
        let cols = cols.max(20);
        let rows = rows.max(5);
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: rows as u16,
                cols: cols as u16,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("创建伪终端失败：{e}"))?;

        let cmd = shell.build(cwd, settings)?;

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| format!("启动终端进程失败：{e}"))?;
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("读取终端失败：{e}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| format!("写入终端失败：{e}"))?;

        let (tx, rx) = channel();
        let reader_tx = tx.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if reader_tx.send(Msg::Bytes(buf[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = reader_tx.send(Msg::Exit);
        });

        let dims = Dims { cols, rows };
        let term = Term::new(TermConfig::default(), &dims, Listener { tx });

        Ok(Self {
            id,
            title: shell.label(),
            cwd: cwd.to_path_buf(),
            shell,
            term,
            processor: Processor::new(),
            writer,
            master: pair.master,
            child,
            rx,
            dims,
            exited: false,
            scroll_acc: 0.0,
            sb_grab: None,
            selection: None,
            focus_requested: false,
        })
    }

    fn drain(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Bytes(bytes) => self.processor.advance(&mut self.term, &bytes),
                Msg::PtyWrite(bytes) => {
                    let _ = self.writer.write_all(&bytes);
                    let _ = self.writer.flush();
                }
                Msg::Title(title) => {
                    // PowerShell / cmd 会把窗口标题设成可执行文件全路径，
                    // 这种标题对用户没意义，保留 shell 名字即可
                    if !title.is_empty() && !looks_like_exe_path(&title) {
                        self.title = title;
                    }
                }
                Msg::Exit => self.exited = true,
            }
        }
    }

    /// 关掉面板时结束子进程。
    ///
    /// 只把 `Terminal` drop 掉是不够的：`Box<dyn Child>` 的析构不会杀进程，
    /// 阅读器线程会因为 `Receiver` 一起被 drop 而退出，于是留下一个
    /// 没有终端可连的孤儿 shell。
    fn kill(&mut self) {
        let _ = self.child.kill();
    }

    fn resize(&mut self, cols: usize, rows: usize) {
        if cols == self.dims.cols && rows == self.dims.rows {
            return;
        }
        self.dims = Dims { cols, rows };
        self.term.resize(self.dims);
        let _ = self.master.resize(PtySize {
            rows: rows as u16,
            cols: cols as u16,
            pixel_width: 0,
            pixel_height: 0,
        });
    }

    fn write(&mut self, bytes: &[u8]) {
        if !bytes.is_empty() {
            let _ = self.writer.write_all(bytes);
            let _ = self.writer.flush();
        }
    }

    /// 直接向终端写入文本（供自检与「运行命令」使用）。
    pub fn send_text(&mut self, text: &str) {
        self.write(text.as_bytes());
        snap_to_bottom(&mut self.term);
    }

    /// 取出当前选区覆盖的文字（按屏幕顺序，行尾空格裁掉）。
    fn selection_text(&self) -> Option<String> {
        let selection = self.selection?;
        extract_selection(self.term.grid(), self.dims.cols, selection)
    }

    /// 画出这块终端。PTY 输出一律由 [`TerminalPanel::pump`] 每帧统一读，
    /// 这里只负责渲染和键盘，所以没被画到的面板也不会积压输出。
    pub fn ui(&mut self, ui: &mut Ui, font_size: f32, dark: bool, id: egui::Id) {
        let font = FontId::monospace(font_size);
        // 与编辑器统一行高，中文不会把行撑高
        let row_h = theme::mono_row_height(font_size, ui.ctx().pixels_per_point());
        let char_w = ui.fonts_mut(|f| f.glyph_width(&font, 'M')).max(1.0);

        let avail = ui.available_size();
        let cols = ((avail.x / char_w).floor() as usize).max(20);
        let rows = ((avail.y / row_h).floor() as usize).max(5);
        self.resize(cols, rows);

        // 先占位，再用同一个 id 注册可聚焦的交互区域
        let (rect, _) = ui.allocate_exact_size(avail, Sense::hover());
        let resp = ui.interact(rect, id, Sense::click_and_drag());
        if std::mem::take(&mut self.focus_requested) {
            resp.request_focus();
        }
        if resp.clicked() || resp.drag_started() {
            resp.request_focus();
        }
        let focused = resp.has_focus();

        // egui 默认把 Tab / 方向键 / Esc 当焦点导航键，终端里一按 Tab 焦点就跳到
        // 别的按钮上，补全和历史都没法用。有焦点时把这些键锁给终端自己。
        if focused {
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    id,
                    egui::EventFilter {
                        tab: true,
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        escape: true,
                    },
                )
            });
        }

        let bg = if dark {
            Color32::from_rgb(24, 26, 30)
        } else {
            Color32::from_rgb(252, 252, 253)
        };
        ui.painter().rect_filled(rect, 0.0, bg);

        let painter = ui.painter_at(rect);

        // 滚动：滚轮一格几行、滚动条拖到哪，都在这里落进 display_offset，
        // 下面才开始照着新的偏移量画，所以同一帧就见效。
        let history = self.term.grid().history_size();
        let wheel = ui.input(|i| {
            let mut lines = 0.0f32;
            for event in &i.events {
                if let egui::Event::MouseWheel { unit, delta, .. } = event {
                    lines += wheel_delta_lines(*unit, delta.y, rows);
                }
            }
            lines
        });
        let mut scroll_to: Option<usize> = None;
        if resp.hovered() && wheel != 0.0 {
            let (steps, rest) = take_lines(self.scroll_acc + wheel);
            self.scroll_acc = rest;
            if steps != 0 {
                let cur = self.term.grid().display_offset() as i32;
                scroll_to = Some((cur + steps).clamp(0, history as i32) as usize);
            }
        }

        // 滚动条：只在真有回滚历史时出现
        let track = (history > 0).then(|| {
            Rect::from_min_max(
                egui::pos2(rect.right() - SB_HIT_W, rect.top()),
                egui::pos2(rect.right(), rect.bottom()),
            )
        });
        // 这一下按没按在滚动条上：按了就不该顺手清掉选区
        let mut bar_press = false;
        if let Some(t) = track {
            let cur = self.term.grid().display_offset();
            let thumb = thumb_rect(t, history, cur, rows);
            let thumb_h = thumb.height();
            if let Some(pos) = ui.input(|i| i.pointer.interact_pos()) {
                if t.contains(pos) && resp.drag_started() {
                    // 按在滑块上就记住手指落在滑块的哪一节，拖动时不跳；
                    // 按在轨道别处则把滑块中心挪到手指下面
                    self.sb_grab = Some(if thumb.contains(pos) {
                        pos.y - thumb.top()
                    } else {
                        thumb_h * 0.5
                    });
                    bar_press = true;
                } else if resp.clicked() && t.contains(pos) && !thumb.contains(pos) {
                    // 单点轨道：滚到那一段。点在滑块上就不动，免得跳半格
                    scroll_to = Some(offset_from_thumb_top(
                        t,
                        history,
                        thumb_h,
                        pos.y - thumb_h * 0.5,
                    ));
                    bar_press = true;
                }
            }
            if let Some(grab) = self.sb_grab {
                if ui.input(|i| !i.pointer.button_down(egui::PointerButton::Primary)) {
                    self.sb_grab = None;
                } else if let Some(pos) = ui.input(|i| i.pointer.interact_pos()) {
                    bar_press = true;
                    scroll_to = Some(offset_from_thumb_top(t, history, thumb_h, pos.y - grab));
                }
            }
        }
        if let Some(to) = scroll_to {
            let cur = self.term.grid().display_offset() as i32;
            if to as i32 != cur {
                self.term.scroll_display(Scroll::Delta(to as i32 - cur));
            }
        }

        let display_offset = self.term.renderable_content().display_offset;
        let grid = self.term.grid();
        let topmost = grid.topmost_line().0;
        let bottommost = grid.bottommost_line().0;

        // 鼠标位置 -> 绝对行/列。行号跟随回滚偏移，滚动后选区仍指向原来的文字。
        let point_at = |pos: egui::Pos2| -> SelPoint {
            let row = (((pos.y - rect.top()) / row_h).floor() as i32).clamp(0, rows as i32 - 1);
            let col = (((pos.x - rect.left()) / char_w).floor() as i32).clamp(0, cols as i32 - 1)
                as usize;
            SelPoint {
                line: (row - display_offset as i32).clamp(topmost, bottommost),
                col,
            }
        };

        if resp.clicked() && !bar_press {
            self.selection = None;
        } else if resp.drag_started() {
            if let Some(pos) = ui.input(|i| i.pointer.interact_pos()) {
                // 按在滚动条上那一下是拿来拖滑块的，不开选区
                if !bar_press {
                    let p = point_at(pos);
                    self.selection = Some(Selection { anchor: p, end: p });
                }
            }
        } else if resp.dragged() && !bar_press {
            if let (Some(pos), Some(sel)) = (
                ui.input(|i| i.pointer.interact_pos()),
                self.selection.as_mut(),
            ) {
                sel.end = point_at(pos);
            }
        }

        let sel_bg = if dark {
            Color32::from_rgba_unmultiplied(82, 139, 255, 80)
        } else {
            Color32::from_rgba_unmultiplied(40, 105, 210, 60)
        };

        // 右键菜单：复制选中内容 / 粘贴。终端不像编辑器那样有内建右键菜单，
        // 不熟悉 Ctrl+Shift+C/V 的用户只能靠这里。
        if resp.secondary_clicked() {
            resp.request_focus();
        }
        let mut menu_copy = false;
        let mut menu_paste = false;
        resp.context_menu(|ui| {
            if ui.button("复制").clicked() {
                menu_copy = true;
                ui.close();
            }
            if ui.button("粘贴").clicked() {
                menu_paste = true;
                ui.close();
            }
        });
        if menu_copy {
            if let Some(text) = self.selection_text() {
                if !text.is_empty() {
                    ui.ctx().copy_text(text);
                }
            }
        }
        if menu_paste {
            // 粘贴走的是 RequestPaste：真正的 Event::Paste 要下一帧才回到 egui 的事件队列。
            // 而刚点完菜单项，键盘焦点已经跟着菜单区走了，下一帧终端不再是 focused，
            // 那份剪贴板内容就会被当成「不归我」的事件丢掉。所以这里先把焦点要回来。
            resp.request_focus();
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::RequestPaste);
        }
        let default_bg = bg;
        let default_fg = if dark {
            Color32::from_rgb(214, 219, 226)
        } else {
            Color32::from_rgb(40, 44, 52)
        };

        for row in 0..rows {
            let line = Line(row as i32 - display_offset as i32);
            let row_cells = &grid[line];
            let y = rect.top() + row as f32 * row_h;

            // 背景色块：合并同色连续单元
            let mut run: Option<(usize, Color32)> = None;
            for col in 0..cols {
                let cell = &row_cells[Column(col)];
                let cell_bg = resolve_color(cell.bg, default_fg, default_bg);
                let is_default = cell_bg == default_bg;
                match run {
                    Some((_, color)) if !is_default && color == cell_bg => {}
                    Some((start, color)) => {
                        if !is_default {
                            painter.rect_filled(
                                Rect::from_min_size(
                                    egui::pos2(rect.left() + start as f32 * char_w, y),
                                    Vec2::new((col - start) as f32 * char_w, row_h),
                                ),
                                0.0,
                                color,
                            );
                        }
                        run = if is_default {
                            None
                        } else {
                            Some((col, cell_bg))
                        };
                    }
                    None => {
                        if !is_default {
                            run = Some((col, cell_bg));
                        }
                    }
                }
            }
            if let Some((start, color)) = run {
                painter.rect_filled(
                    Rect::from_min_size(
                        egui::pos2(rect.left() + start as f32 * char_w, y),
                        Vec2::new((cols - start) as f32 * char_w, row_h),
                    ),
                    0.0,
                    color,
                );
            }

            // 选中区域的底色垫在文字下面
            if let Some(sel) = self.selection {
                let (start, end) = sel.ordered();
                if line.0 >= start.line && line.0 <= end.line {
                    let c0 = if line.0 == start.line { start.col } else { 0 };
                    let c1 = if line.0 == end.line {
                        end.col
                    } else {
                        cols - 1
                    };
                    if c0 <= c1 && c0 < cols {
                        let c1 = c1.min(cols - 1);
                        painter.rect_filled(
                            Rect::from_min_size(
                                egui::pos2(rect.left() + c0 as f32 * char_w, y),
                                Vec2::new((c1 + 1 - c0) as f32 * char_w, row_h),
                            ),
                            0.0,
                            sel_bg,
                        );
                    }
                }
            }

            // 文本：逐 cell 定位，保证宽字符（中文）严格占两格
            for col in 0..cols {
                let cell = &row_cells[Column(col)];
                let flags = cell.flags;
                if flags.intersects(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                let ch = cell.c;
                if ch == '\0' || ch == ' ' {
                    continue;
                }
                let color = if flags.intersects(Flags::HIDDEN) {
                    Color32::TRANSPARENT
                } else {
                    resolve_color(cell.fg, default_fg, default_bg)
                };
                let width = if flags.intersects(Flags::WIDE_CHAR) {
                    char_w * 2.0
                } else {
                    char_w
                };
                let mut buf = [0u8; 4];
                let s = ch.encode_utf8(&mut buf);
                // 以 cell 区域居中，宽字符自然占两格
                painter.text(
                    egui::pos2(
                        rect.left() + col as f32 * char_w + width * 0.5,
                        y + row_h * 0.5,
                    ),
                    egui::Align2::CENTER_CENTER,
                    s,
                    font.clone(),
                    color,
                );
            }
        }

        // 光标
        if focused && !self.exited {
            let cursor = self.term.renderable_content().cursor;
            let point = cursor.point;
            let col = point.column.0 as f32;
            let row = (point.line.0 + display_offset as i32) as f32;
            if row >= 0.0 && (row as usize) < rows && col < cols as f32 {
                let pos = egui::pos2(rect.left() + col * char_w, rect.top() + row * row_h);
                let cursor_color = if dark {
                    Color32::from_rgb(82, 139, 255)
                } else {
                    Color32::from_rgb(60, 110, 220)
                };
                painter.rect_filled(
                    Rect::from_min_size(pos, Vec2::new(char_w, row_h)),
                    0.0,
                    cursor_color.gamma_multiply(0.75),
                );
            }

            // 光标格同时也是输入法锚点。不报给系统的话，winit 压根不把输入法
            // 上下文关联到这个窗口，中文输入直接退化成一个个原始拼音字母。
            // 滚回历史时光标在屏幕外，锚点就钉回可视区，别顺手把输入法卸了。
            let anchor = Rect::from_min_size(
                egui::pos2(
                    rect.left() + col.clamp(0.0, cols as f32 - 1.0) * char_w,
                    rect.top() + row.clamp(0.0, rows as f32 - 1.0) * row_h,
                ),
                Vec2::new(char_w, row_h),
            );
            ui.output_mut(|o| {
                o.ime = Some(egui::output::IMEOutput {
                    purpose: egui::IMEPurpose::Terminal,
                    rect,
                    cursor_rect: anchor,
                    should_interrupt_composition: false,
                })
            });
        }

        // 滚动条画在文字和光标之上：有回滚历史就一直在，指针靠过去才垫一层轨道底色
        if let Some(t) = track {
            let thumb = thumb_rect(t, history, display_offset, rows);
            let dragging = self.sb_grab.is_some();
            let over =
                dragging || ui.input(|i| i.pointer.hover_pos().is_some_and(|p| t.contains(p)));
            if over {
                let track_bg = if dark {
                    Color32::from_rgba_unmultiplied(255, 255, 255, 14)
                } else {
                    Color32::from_rgba_unmultiplied(0, 0, 0, 12)
                };
                painter.rect_filled(t, 0.0, track_bg);
            }
            let alpha = if over { 205 } else { 120 };
            let thumb_bg = if dark {
                Color32::from_rgba_unmultiplied(178, 186, 198, alpha)
            } else {
                Color32::from_rgba_unmultiplied(88, 98, 112, alpha)
            };
            painter.rect_filled(thumb, SB_BAR_W * 0.5, thumb_bg);
        }

        // 键盘输入：只在有焦点时接管，并把文本类事件从队列里摘掉，
        // 否则编辑器的 TextEdit 会再吃一遍同样的字符。
        if focused {
            let mut out: Vec<u8> = Vec::new();
            let mut copy = false;
            let mut paste = false;
            let mut typed = false;
            ui.input_mut(|i| {
                i.events.retain(|event| {
                    match event {
                        // Ctrl+C 会被 winit 转成 Copy 事件，终端里再按惯例
                        // 「有选区就复制，没选区就发中断」。
                        egui::Event::Copy | egui::Event::Cut => {
                            copy = true;
                            false
                        }
                        egui::Event::Key {
                            key,
                            pressed: true,
                            modifiers,
                            ..
                        } if modifiers.ctrl && modifiers.shift && *key == egui::Key::C => {
                            copy = true;
                            false
                        }
                        egui::Event::Key {
                            key,
                            pressed: true,
                            modifiers,
                            ..
                        } if modifiers.ctrl && modifiers.shift && *key == egui::Key::V => {
                            paste = true;
                            false
                        }
                        egui::Event::Text(_) | egui::Event::Key { .. } | egui::Event::Paste(_) => {
                            encode_event(event, &mut out);
                            typed = true;
                            false
                        }
                        // 输入法挑定候选词后整串递交过来，直接写进 PTY。
                        // 组字中的 Preedit 原样留在队列里，候选窗由系统自己画。
                        egui::Event::Ime(egui::ImeEvent::Commit(text)) => {
                            out.extend_from_slice(text.as_bytes());
                            typed = true;
                            false
                        }
                        _ => true,
                    }
                });
            });
            if typed {
                self.selection = None;
                snap_to_bottom(&mut self.term);
            }
            if copy {
                match self.selection_text() {
                    Some(text) if !text.is_empty() => ui.ctx().copy_text(text),
                    _ => out.push(0x03),
                }
            }
            if paste {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::RequestPaste);
            }
            self.write(&out);
        }

        if self.exited {
            painter.text(
                rect.center_bottom() - Vec2::new(0.0, 18.0),
                egui::Align2::CENTER_BOTTOM,
                "进程已退出",
                FontId::proportional(crate::theme::sm(ui)),
                Color32::from_rgb(200, 120, 120),
            );
        }
    }
}

/// 用户一往 PTY 里写东西，就把视口拉回最新一行。
///
/// alacritty 在 `display_offset > 0` 时会把屏幕钉在回滚历史里：新输出全进滚动
/// 缓冲，可视区一个字都不动。不主动归位的话，翻过历史的人会发现屏幕上一直杵着
/// 旧内容，敲了命令也不消失。
fn snap_to_bottom(term: &mut Term<Listener>) {
    if term.grid().display_offset() != 0 {
        term.scroll_display(Scroll::Bottom);
    }
}

/// 从网格里按选区取文字。行号是绝对行，网格索引负责换算回滚历史。
fn extract_selection(grid: &Grid<Cell>, cols: usize, selection: Selection) -> Option<String> {
    let (start, end) = selection.ordered();
    if start.line == end.line && start.col == end.col {
        return None;
    }
    let topmost = grid.topmost_line().0;
    let bottommost = grid.bottommost_line().0;
    let mut out = String::new();
    for line in start.line..=end.line {
        let c0 = if line == start.line { start.col } else { 0 };
        let c1 = if line == end.line {
            end.col
        } else {
            cols.saturating_sub(1)
        };
        let mut text = String::new();
        if line >= topmost && line <= bottommost {
            let row = &grid[Line(line)];
            for col in c0..=c1.min(cols.saturating_sub(1)) {
                let cell = &row[Column(col)];
                // 宽字符的占位格跳过，字符本身已经在上一个格子里取到了
                if cell.flags.intersects(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                // 未填充的格子偶尔会是 NUL，复制出来不能带进去
                text.push(if cell.c == '\0' { ' ' } else { cell.c });
            }
        }
        out.push_str(text.trim_end());
        if line < end.line {
            out.push('\n');
        }
    }
    Some(out)
}

fn encode_event(event: &egui::Event, out: &mut Vec<u8>) {
    match event {
        egui::Event::Text(text) => {
            if !text.is_empty() {
                out.extend_from_slice(text.as_bytes());
            }
        }
        egui::Event::Paste(text) => {
            out.extend_from_slice(text.replace('\n', "\r").as_bytes());
        }
        egui::Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } => {
            if modifiers.ctrl && !modifiers.alt {
                if let Some(byte) = ctrl_byte(*key) {
                    out.push(byte);
                    return;
                }
            }
            match key {
                egui::Key::Enter => out.extend_from_slice(b"\r"),
                egui::Key::Backspace => out.push(0x7f),
                egui::Key::Tab if modifiers.shift => out.extend_from_slice(b"\x1b[Z"),
                egui::Key::Tab => out.extend_from_slice(b"\t"),
                egui::Key::Escape => out.push(0x1b),
                egui::Key::ArrowUp => out.extend_from_slice(b"\x1b[A"),
                egui::Key::ArrowDown => out.extend_from_slice(b"\x1b[B"),
                egui::Key::ArrowRight => out.extend_from_slice(b"\x1b[C"),
                egui::Key::ArrowLeft => out.extend_from_slice(b"\x1b[D"),
                egui::Key::Home => out.extend_from_slice(b"\x1b[H"),
                egui::Key::End => out.extend_from_slice(b"\x1b[F"),
                egui::Key::PageUp => out.extend_from_slice(b"\x1b[5~"),
                egui::Key::PageDown => out.extend_from_slice(b"\x1b[6~"),
                egui::Key::Delete => out.extend_from_slice(b"\x1b[3~"),
                egui::Key::Insert => out.extend_from_slice(b"\x1b[2~"),
                _ => {}
            }
        }
        _ => {}
    }
}

fn ctrl_byte(key: egui::Key) -> Option<u8> {
    use egui::Key;
    let byte = match key {
        Key::A => 0x01,
        Key::B => 0x02,
        Key::C => 0x03,
        Key::D => 0x04,
        Key::E => 0x05,
        Key::F => 0x06,
        Key::G => 0x07,
        Key::H => 0x08,
        Key::I => 0x09,
        Key::J => 0x0a,
        Key::K => 0x0b,
        Key::L => 0x0c,
        Key::M => 0x0d,
        Key::N => 0x0e,
        Key::O => 0x0f,
        Key::P => 0x10,
        Key::Q => 0x11,
        Key::R => 0x12,
        Key::S => 0x13,
        Key::T => 0x14,
        Key::U => 0x15,
        Key::V => 0x16,
        Key::W => 0x17,
        Key::X => 0x18,
        Key::Y => 0x19,
        Key::Z => 0x1a,
        _ => return None,
    };
    Some(byte)
}

fn resolve_color(color: Color, fg: Color32, bg: Color32) -> Color32 {
    match color {
        Color::Named(named) => named_color(named, fg, bg),
        Color::Spec(Rgb { r, g, b }) => Color32::from_rgb(r, g, b),
        Color::Indexed(index) => indexed_color(index),
    }
}

fn named_color(named: NamedColor, fg: Color32, bg: Color32) -> Color32 {
    use NamedColor::*;
    match named {
        Black | DimBlack => Color32::from_rgb(40, 44, 52),
        Red | DimRed => Color32::from_rgb(224, 108, 117),
        Green | DimGreen => Color32::from_rgb(152, 195, 121),
        Yellow | DimYellow => Color32::from_rgb(229, 192, 123),
        Blue | DimBlue => Color32::from_rgb(97, 175, 239),
        Magenta | DimMagenta => Color32::from_rgb(198, 120, 221),
        Cyan | DimCyan => Color32::from_rgb(86, 182, 194),
        White | DimWhite => Color32::from_rgb(220, 223, 228),
        BrightBlack => Color32::from_rgb(92, 99, 112),
        BrightRed => Color32::from_rgb(240, 130, 140),
        BrightGreen => Color32::from_rgb(170, 215, 140),
        BrightYellow => Color32::from_rgb(245, 210, 145),
        BrightBlue => Color32::from_rgb(120, 195, 255),
        BrightMagenta => Color32::from_rgb(215, 145, 240),
        BrightCyan => Color32::from_rgb(110, 205, 215),
        BrightWhite | BrightForeground => Color32::from_rgb(245, 247, 250),
        Foreground => fg,
        Background => bg,
        Cursor => Color32::from_rgb(82, 139, 255),
        DimForeground => fg.gamma_multiply(0.7),
    }
}

fn indexed_color(index: u8) -> Color32 {
    match index {
        0..=15 => {
            const ANSI: [NamedColor; 16] = [
                NamedColor::Black,
                NamedColor::Red,
                NamedColor::Green,
                NamedColor::Yellow,
                NamedColor::Blue,
                NamedColor::Magenta,
                NamedColor::Cyan,
                NamedColor::White,
                NamedColor::BrightBlack,
                NamedColor::BrightRed,
                NamedColor::BrightGreen,
                NamedColor::BrightYellow,
                NamedColor::BrightBlue,
                NamedColor::BrightMagenta,
                NamedColor::BrightCyan,
                NamedColor::BrightWhite,
            ];
            named_color(
                ANSI[index as usize],
                Color32::from_rgb(214, 219, 226),
                Color32::from_rgb(24, 26, 30),
            )
        }
        16..=231 => {
            let i = index - 16;
            let r = i / 36;
            let g = (i % 36) / 6;
            let b = i % 6;
            let to = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            Color32::from_rgb(to(r), to(g), to(b))
        }
        _ => {
            let level = 8 + (index - 232) * 10;
            Color32::from_rgb(level, level, level)
        }
    }
}

/// 所有终端的集合。分屏树里每个终端面板按 `id` 找到自己那一份。
pub struct TerminalPanel {
    terms: Vec<Terminal>,
    next_id: u64,
    pub cwd: PathBuf,
    /// 新建终端时使用的 shell
    pub shell: Shell,
    pub available: Vec<Shell>,
    /// 程序路径 / 启动参数的覆盖项 + 自定义 shell，来自 config.json
    pub settings: TerminalSettings,
}

impl TerminalPanel {
    pub fn new(cwd: PathBuf, settings: TerminalSettings) -> Self {
        let available = Shell::detect(&settings);
        let shell = available
            .first()
            .cloned()
            .unwrap_or(Shell::Builtin(ShellKind::Cmd));
        Self {
            terms: Vec::new(),
            next_id: 1,
            cwd,
            shell,
            available,
            settings,
        }
    }

    /// 套用配置里的 shell 偏好；本机没有这一条就保持自动选择。
    ///
    /// 自定义的那条认的是 `custom:<id>`：id 稳定，用户改显示名不会把偏好弄丢。
    pub fn apply_pref(&mut self, id: &str) {
        if let Some(custom_id) = id.strip_prefix("custom:") {
            if let Some(c) = self.settings.custom.iter().find(|c| c.id == custom_id) {
                self.shell = Shell::Custom(c.clone());
            }
            return;
        }
        if let Some(kind) = ShellKind::from_id(id) {
            if self.available.contains(&Shell::Builtin(kind)) {
                self.shell = Shell::Builtin(kind);
            }
        }
    }

    /// 改了设置后重算可用列表。
    ///
    /// 按 id 重新认领当前选择：刚改过名字的自定义 shell 内容变了但 id 没变，
    /// 选择要跟着走过去；整条被删掉了才退回列表里第一个可用的。
    pub fn refresh(&mut self) {
        self.available = Shell::detect(&self.settings);
        let id = self.shell.id();
        self.shell = self
            .available
            .iter()
            .find(|s| s.id() == id)
            .cloned()
            .unwrap_or_else(|| {
                self.available
                    .first()
                    .cloned()
                    .unwrap_or(Shell::Builtin(ShellKind::Cmd))
            });
    }

    /// 新加一条空的自定义 shell 并选中它，界面紧接着就能填名字和路径。
    pub fn add_custom(&mut self) {
        let shell = CustomShell {
            id: self.settings.next_custom_id(),
            ..Default::default()
        };
        self.settings.custom.push(shell.clone());
        self.shell = Shell::Custom(shell);
        self.refresh();
    }

    /// 改一条自定义 shell 的三项内容，顺带重算可用列表。
    pub fn update_custom(&mut self, id: &str, name: &str, program: &str, args: &str) {
        if let Some(c) = self.settings.custom.iter_mut().find(|c| c.id == id) {
            c.name = name.trim().to_string();
            c.program = program.trim().to_string();
            c.args = args.trim().to_string();
        }
        self.refresh();
    }

    /// 删掉一条自定义 shell。正选着它的话，`refresh` 会把选择退回第一个可用的。
    pub fn remove_custom(&mut self, id: &str) {
        self.settings.custom.retain(|c| c.id != id);
        self.refresh();
    }

    pub fn set_cwd(&mut self, cwd: PathBuf) {
        self.cwd = cwd;
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    pub fn len(&self) -> usize {
        self.terms.len()
    }

    pub fn get(&self, id: u64) -> Option<&Terminal> {
        self.terms.iter().find(|t| t.id == id)
    }

    pub fn get_mut(&mut self, id: u64) -> Option<&mut Terminal> {
        self.terms.iter_mut().find(|t| t.id == id)
    }

    /// 面板标题条上的文字：优先用 shell 自己报的标题，报的是可执行文件路径或空串时
    /// 退回工具栏选的那个 shell 名；进程已经退出时要标出来，否则面板看着像卡住了。
    pub fn pane_title(&self, id: u64) -> Option<String> {
        let term = self.get(id)?;
        let base = if term.title.trim().is_empty() || looks_like_exe_path(&term.title) {
            term.shell.label()
        } else {
            truncate(&term.title, 24)
        };
        Some(if term.exited {
            format!("{base} · 已退出")
        } else {
            base
        })
    }

    /// 在指定目录里起一块终端（新建终端窗口、同窗口再切一格）。
    pub fn create_at(&mut self, cwd: &Path) -> Result<u64, String> {
        let id = self.next_id;
        self.next_id += 1;
        let term = Terminal::spawn(id, cwd, 100, 28, self.shell.clone(), &self.settings)?;
        self.terms.push(term);
        Ok(id)
    }

    /// 关闭并杀掉进程。id 不存在时什么都不做。
    pub fn remove(&mut self, id: u64) {
        let Some(at) = self.terms.iter().position(|t| t.id == id) else {
            return;
        };
        self.terms[at].kill();
        self.terms.remove(at);
    }

    /// 每帧把所有终端的 PTY 输出读进屏幕模型，不管这块面板有没有被画到。
    ///
    /// 只读当前可见终端的话，后台面板的输出会在无界 channel 里一直堆，
    /// 滚回去时看到的是一坨过期内容。
    pub fn pump(&mut self) {
        for term in &mut self.terms {
            term.drain();
        }
    }

    /// 请求下一帧聚焦这块终端。
    pub fn request_focus(&mut self, id: u64) {
        if let Some(term) = self.get_mut(id) {
            term.focus_requested = true;
        }
    }

    /// 向终端写入文本（供自检与「运行命令」使用）。
    pub fn send_text(&mut self, id: u64, text: &str) {
        if let Some(term) = self.get_mut(id) {
            term.send_text(text);
        }
    }

    /// 画一块终端面板。id 不存在（面板已经关掉但树还没更新）时什么都不画。
    pub fn ui_leaf(&mut self, ui: &mut Ui, id: u64, font_size: f32, dark: bool) {
        let Some(term) = self.get_mut(id) else {
            return;
        };
        // 焦点 id 用终端自己的 id，不能用数组下标：关掉中间一块之后
        // 下标会整体前移，焦点就会莫名其妙跳到别的终端上。
        let focus_id = ui.make_persistent_id(("terminal", id));
        term.ui(ui, font_size, dark, focus_id);
    }
}

pub(crate) fn truncate(text: &str, max: usize) -> String {
    let cleaned = text.replace('\n', " ");
    if cleaned.chars().count() <= max {
        cleaned
    } else {
        let mut out: String = cleaned.chars().take(max).collect();
        out.push('…');
        out
    }
}

/// 判断标题是否只是可执行文件路径（`C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe`）。
fn looks_like_exe_path(title: &str) -> bool {
    let lower = title.to_ascii_lowercase();
    if lower.ends_with(".exe") {
        return true;
    }
    lower.contains("\\system32\\") || lower.contains("\\windows\\")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_ids_roundtrip() {
        for kind in ShellKind::ALL {
            let id = kind.id();
            assert!(!id.is_empty());
            assert_eq!(ShellKind::from_id(id), Some(*kind), "{id} 反查失败");
        }
        assert_eq!(ShellKind::from_id("nope"), None);
    }

    #[test]
    fn splits_args_with_quotes() {
        assert_eq!(split_args("-i -l"), ["-i", "-l"]);
        assert_eq!(
            split_args("  -NoProfile   -NoLogo "),
            ["-NoProfile", "-NoLogo"]
        );
        assert_eq!(split_args(r#"--cd "C:\My Dir""#), ["--cd", r"C:\My Dir"]);
        assert_eq!(split_args(""), Vec::<String>::new());
        // 引号里的空串仍然是一个参数
        assert_eq!(split_args(r#"--exec """#), ["--exec", ""]);
    }

    #[test]
    fn config_drops_blank_overrides() {
        let mut s = TerminalSettings::default();
        s.programs
            .insert("gitbash".into(), r"D:\Git\bin\bash.exe".into());
        s.programs.insert("wsl".into(), "   ".into());
        s.custom.push(CustomShell {
            id: "  ".into(),
            name: "没有 id，配置里引用不到".into(),
            ..Default::default()
        });
        s.custom.push(CustomShell {
            id: "custom-1".into(),
            name: " bash ".into(),
            program: " D:\\a\\bash.exe ".into(),
            args: String::new(),
        });
        let (programs, args, customs) = s.to_config();
        assert_eq!(programs.len(), 1);
        assert_eq!(
            programs.get("gitbash").map(String::as_str),
            Some(r"D:\Git\bin\bash.exe")
        );
        assert!(args.is_empty());
        assert_eq!(customs.len(), 1);
        assert_eq!(customs[0].name, "bash");
        assert_eq!(customs[0].program, r"D:\a\bash.exe");
    }

    #[test]
    fn configured_program_beats_detection() {
        let mut s = TerminalSettings::default();
        s.programs.insert(
            "gitbash".into(),
            r"D:\Softwares\GitBash\bin\bash.exe".into(),
        );
        // 路径存在与否交给启动时报错，这里必须认账，否则用户改了配置却静默失效
        assert!(ShellKind::GitBash.program(&s).is_some());
        assert!(ShellKind::detect(&s).contains(&ShellKind::GitBash));
    }

    /// 一条自定义 shell 的设置，`program` 是它自己的程序路径。
    fn settings_with_custom(program: &str) -> TerminalSettings {
        let mut s = TerminalSettings::default();
        s.custom.push(CustomShell {
            id: "custom-1".into(),
            name: "MSYS2 Bash".into(),
            program: program.into(),
            args: "--login -i".into(),
        });
        s
    }

    #[test]
    fn custom_shell_shows_up_in_the_picker() {
        let ids = |s: &TerminalSettings| -> Vec<String> {
            Shell::detect(s).iter().map(|sh| sh.id()).collect()
        };
        let s = settings_with_custom(r"D:\msys64\usr\bin\bash.exe");
        assert!(ids(&s).contains(&"custom:custom-1".to_string()), "{:?}", ids(&s));
        // 路径还空着的那条也要留着：刚点「新增」就找不到它，没地方接着填
        let empty = settings_with_custom("");
        assert!(ids(&empty).contains(&"custom:custom-1".to_string()));
        // 内置的还是按探测结果来，没装的不会凭空冒出来
        assert!(!ids(&TerminalSettings::default()).iter().any(|i| i.starts_with("custom:")));
    }

    /// 自定义 shell 起的是它自己那条路径和参数，跟内置那套内置参数没关系。
    #[test]
    fn custom_shell_builds_its_own_command() {
        let s = settings_with_custom(r"D:\msys64\usr\bin\bash.exe");
        let shell = Shell::detect(&s)
            .into_iter()
            .find(|sh| sh.id() == "custom:custom-1")
            .expect("下拉里没有刚配的那条自定义 shell");
        let cmd = shell.build(Path::new(r"C:\"), &s).expect("该拼出命令");
        let argv: Vec<String> = cmd
            .get_argv()
            .iter()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(argv, [r"D:\msys64\usr\bin\bash.exe", "--login", "-i"]);
        assert_eq!(
            cmd.get_cwd().map(|c| c.to_string_lossy().to_string()).as_deref(),
            Some(r"C:\")
        );

        // 没填路径要说清楚是哪儿没填，别丢一句「找不到可执行文件」让人猜
        let blank = settings_with_custom("   ");
        let err = Shell::Custom(blank.custom[0].clone())
            .build(Path::new(r"C:\"), &blank)
            .expect_err("路径空着不该给出命令");
        assert!(err.contains("程序路径"), "{err}");
        assert!(err.contains("MSYS2 Bash"), "{err}");
    }

    /// 「新增 → 改名 → 删除」这一串走的是 id，中间不能把用户选中的那条弄丢。
    #[test]
    fn adding_and_renaming_a_custom_shell_keeps_the_selection() {
        let mut panel = TerminalPanel::new(std::env::temp_dir(), TerminalSettings::default());
        panel.add_custom();
        let first = panel.shell.id();
        assert!(first.starts_with("custom:"), "新增后没选中新那条：{first}");
        assert!(
            panel.available.iter().any(|s| s.id() == first),
            "新加的没进可用列表"
        );

        let key = first.strip_prefix("custom:").unwrap().to_string();
        panel.update_custom(&key, "bash", r"D:\a\bash.exe", "");
        assert_eq!(panel.shell.id(), first, "改个名字把选择甩掉了");
        assert_eq!(panel.shell.label(), "bash", "下拉里显示的还是旧名字");

        panel.add_custom();
        assert_ne!(panel.shell.id(), first, "两条自定义 shell 撞了 id");

        // 删掉正选着的这条，才退回列表里第一个可用的
        let gone = panel.shell.id();
        let gone_key = gone.strip_prefix("custom:").unwrap().to_string();
        panel.remove_custom(&gone_key);
        assert_ne!(panel.shell.id(), gone, "删掉了还指着它");
        assert!(!panel.shell.id().starts_with("custom:"), "{:?}", panel.shell.id());
    }

    #[test]
    fn pref_can_point_at_a_custom_shell() {
        let mut panel = TerminalPanel::new(
            std::env::temp_dir(),
            settings_with_custom(r"D:\msys64\usr\bin\bash.exe"),
        );
        panel.apply_pref("custom:custom-1");
        assert_eq!(panel.shell.label(), "MSYS2 Bash");
        // 引用了一条配置里没有的：保持原样，别把已经选好的改成别的
        panel.apply_pref("custom:没有这条");
        assert_eq!(panel.shell.id(), "custom:custom-1");
        panel.apply_pref("cmd");
        assert_eq!(panel.shell.id(), "cmd");
    }

    #[test]
    fn wsl_stub_is_not_git_bash() {
        assert!(looks_like_wsl_stub(Path::new(
            r"C:\Windows\System32\bash.exe"
        )));
        assert!(!looks_like_wsl_stub(Path::new(
            r"D:\Softwares\GitBash\bin\bash.exe"
        )));
    }

    fn term_with(text: &str) -> (Term<Listener>, Dims) {
        let dims = Dims { cols: 24, rows: 4 };
        let (tx, _rx) = channel();
        let mut term = Term::new(TermConfig::default(), &dims, Listener { tx });
        let mut processor: Processor = Processor::new();
        processor.advance(&mut term, text.as_bytes());
        (term, dims)
    }

    #[test]
    fn selection_copies_rows() {
        let (term, dims) = term_with("hello\r\nworld\r\n");
        let selection = Selection {
            anchor: SelPoint { line: 0, col: 0 },
            end: SelPoint { line: 1, col: 4 },
        };
        let text = extract_selection(term.grid(), dims.cols, selection).unwrap();
        assert_eq!(text, "hello\nworld");
    }

    #[test]
    fn selection_skips_wide_char_spacer() {
        let (term, dims) = term_with("工作日志\r\n");
        // 每个汉字占两列，选区覆盖四个汉字（列 0..=6，其中奇数位是占位格）
        let selection = Selection {
            anchor: SelPoint { line: 0, col: 0 },
            end: SelPoint { line: 0, col: 6 },
        };
        let text = extract_selection(term.grid(), dims.cols, selection).unwrap();
        assert_eq!(text, "工作日志");
    }

    #[test]
    fn empty_selection_copies_nothing() {
        let (term, dims) = term_with("abc");
        let selection = Selection {
            anchor: SelPoint { line: 0, col: 1 },
            end: SelPoint { line: 0, col: 1 },
        };
        assert!(extract_selection(term.grid(), dims.cols, selection).is_none());
    }

    /// 屏幕上现在看得见的那几行文字。
    fn visible_lines(term: &Term<Listener>, rows: usize, cols: usize) -> Vec<String> {
        let offset = term.grid().display_offset();
        (0..rows)
            .map(|row| {
                let line = &term.grid()[Line(row as i32 - offset as i32)];
                (0..cols)
                    .map(|col| {
                        let c = line[Column(col)].c;
                        if c == '\0' {
                            ' '
                        } else {
                            c
                        }
                    })
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    /// 滚轮一格 3 行；触控板按像素折算；页单位按屏幕行数。
    /// 正值是「往回滚」，和 Windows 上滚轮向上报正值对上。
    #[test]
    fn wheel_units_become_lines() {
        let line = egui::MouseWheelUnit::Line;
        let point = egui::MouseWheelUnit::Point;
        let page = egui::MouseWheelUnit::Page;
        assert_eq!(wheel_delta_lines(line, 1.0, 28), 3.0);
        assert_eq!(wheel_delta_lines(line, -2.0, 28), -6.0);
        assert_eq!(wheel_delta_lines(point, 28.0, 28), 2.0);
        assert_eq!(wheel_delta_lines(point, -7.0, 28), -0.5);
        assert_eq!(wheel_delta_lines(page, 1.0, 28), 28.0);
    }

    /// 凑不满一行的零头要攒着：高分辨率滚轮一格只有 0.2，
    /// 直接截断就永远滚不动。
    #[test]
    fn partial_wheel_notches_accumulate() {
        // 数值都取二进制精确的，免得拿 f32 舍入当断言
        assert_eq!(take_lines(2.25), (2, 0.25));
        assert_eq!(take_lines(-2.25), (-2, -0.25));
        assert_eq!(take_lines(0.5), (0, 0.5));
        assert_eq!(take_lines(0.75 + 0.25), (1, 0.0));
    }

    /// 滚动方向和 alacritty 的 display_offset 约定对齐：正 delta 往回滚历史。
    /// 这条一旦反过来，滚轮就会变成「想往上翻却往下跑」。
    #[test]
    fn positive_scroll_goes_back_in_history() {
        let mut feed = String::new();
        for i in 0..12 {
            feed.push_str(&format!("line{i}\r\n"));
        }
        let (mut term, dims) = term_with(&feed);
        assert!(term.grid().history_size() > 0, "喂了 12 行却没有回滚历史");

        let at_bottom = visible_lines(&term, dims.rows, dims.cols);
        term.scroll_display(Scroll::Delta(2));
        assert_eq!(term.grid().display_offset(), 2);
        let scrolled = visible_lines(&term, dims.rows, dims.cols);
        // 往回滚两行，原来顶上的那行应该掉到第 3 格上
        assert_eq!(scrolled[2], at_bottom[0]);
        assert_ne!(scrolled[0], at_bottom[0]);

        term.scroll_display(Scroll::Delta(-2));
        assert_eq!(visible_lines(&term, dims.rows, dims.cols), at_bottom);
    }

    /// 滑块位置 ↔ 回滚行数互为反函数，否则拖起来会一跳一跳。
    #[test]
    fn scrollbar_maps_both_ways() {
        let track = Rect::from_min_max(egui::pos2(100.0, 0.0), egui::pos2(112.0, 400.0));
        let history = 300usize;
        let rows = 28usize;
        for offset in [0usize, 7, 150, history] {
            let thumb = thumb_rect(track, history, offset, rows);
            assert!(
                thumb.top() >= track.top() - 0.01 && thumb.bottom() <= track.bottom() + 0.01,
                "滑块跑出轨道了：{thumb:?}"
            );
            let back = offset_from_thumb_top(track, history, thumb.height(), thumb.top());
            assert!(back.abs_diff(offset) <= 1, "{offset} 绕一圈变成 {back}");
        }
        // 轨道顶 = 回滚到底，轨道底 = 最新输出
        assert_eq!(thumb_rect(track, history, history, rows).top(), track.top());
        assert_eq!(
            thumb_rect(track, history, 0, rows).bottom(),
            track.bottom(),
            "最新输出时滑块该贴着轨道底"
        );
    }

    /// 滑块长度按「可见行 / 总行」算，两端都要兜住。
    #[test]
    fn scrollbar_thumb_size() {
        let track = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(12.0, 400.0));
        // 没有回滚历史（轨道本来就不显示）时滑块铺满整条轨道
        assert_eq!(thumb_rect(track, 0, 0, 28).height(), track.height());
        // 历史很长时也要留一块点得到的地方
        assert!(thumb_rect(track, 100_000, 0, 28).height() >= SB_MIN_THUMB);
        // 常规比例：28 行可见 / 128 行总共
        let h = thumb_rect(track, 100, 0, 28).height();
        assert!((h - track.height() * 28.0 / 128.0).abs() < 0.01, "{h}");
    }

    /// 终端的中文输入要起真实 shell 才算验证过，所以默认忽略：
    /// `cargo test -- --ignored --nocapture pty_terminal`
    ///
    /// 两件事：向系统报出光标格当输入法锚点（不报窗口就不关联输入法，
    /// 打进去的只剩原始拼音字母），以及把候选词提交的那一整串写进 PTY。
    #[test]
    #[ignore = "要起真实 PTY 进程，手动跑：cargo test -- --ignored --nocapture pty_terminal"]
    fn pty_terminal_publishes_ime_anchor_and_routes_commit() {
        let mut panel = TerminalPanel::new(std::env::temp_dir(), TerminalSettings::default());
        panel.shell = Shell::Builtin(ShellKind::Cmd);
        let cwd = panel.cwd.clone();
        let term_id = panel.create_at(&cwd).unwrap();
        let wait = |ms: u64| std::thread::sleep(std::time::Duration::from_millis(ms));

        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(1.0);
        let draw = |panel: &mut TerminalPanel, events: Vec<egui::Event>| -> egui::FullOutput {
            let raw = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(900.0, 500.0),
                )),
                events,
                ..Default::default()
            };
            let mut out = ctx.run_pass(raw, |ctx| {
                ctx.root_ui(|ui| {
                    panel.get_mut(term_id).unwrap().focus_requested = true;
                    panel.ui_leaf(ui, term_id, 14.0, true);
                });
            });
            // 没有渲染器消费字体图集的增量，epaint 会在 drop 时断言
            out.textures_delta.clear();
            out
        };

        wait(1500);
        panel.pump();
        // 第一帧只是要焦点，`has_focus` 要下一帧才为真
        let _ = draw(&mut panel, vec![]);
        let out = draw(&mut panel, vec![]);
        let ime = out.platform_output.ime.expect("终端没向系统报输入法锚点");
        assert_eq!(ime.purpose, egui::IMEPurpose::Terminal);
        assert!(ime.cursor_rect.is_positive(), "锚点格子面积为空：{ime:?}");

        // 提交给 PTY 的中文会被 shell 回显到屏幕上
        let _ = draw(
            &mut panel,
            vec![egui::Event::Ime(egui::ImeEvent::Commit("中文".into()))],
        );
        wait(800);
        panel.pump();
        let out = draw(&mut panel, vec![]);
        assert!(
            out.platform_output.ime.is_some(),
            "提交中文后输入法锚点反而没了"
        );
        let t = panel.get_mut(term_id).unwrap();
        // 宽字符占两格，第二格是空的占位格，拼回一行时得把它剔掉
        let screen = visible_lines(&t.term, t.dims.rows, t.dims.cols);
        let flat: String = screen
            .iter()
            .flat_map(|l| l.chars().filter(|c| *c != ' '))
            .collect();
        assert!(
            flat.contains("中文"),
            "输入法提交的中文没写进 PTY，屏幕是：{screen:?}"
        );

        // 先攒出回滚历史，才有「钉在历史里」这回事。
        // 前面提交的「中文」还挂在命令行上，先回车把它清掉再发命令。
        panel.send_text(term_id, "\rfor /L %i in (1,1,60) do @echo L%i\r");
        wait(1500);
        panel.pump();
        let _ = draw(&mut panel, vec![]);
        let scrolled = {
            let t = panel.get_mut(term_id).unwrap();
            t.term.scroll_display(Scroll::Delta(3));
            t.term.grid().display_offset()
        };
        assert_ne!(scrolled, 0, "没能滚回历史，后面没法验");
        let _ = draw(&mut panel, vec![key_event(egui::Key::Enter)]);
        let offset = panel.get_mut(term_id).unwrap().term.grid().display_offset();
        assert_eq!(
            offset, 0,
            "敲了键盘却还钉在历史里，屏幕上一直是不动的旧内容"
        );

        panel.remove(term_id);
    }

    fn key_event(key: egui::Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    #[test]
    fn typing_snaps_the_pinned_view_back_to_the_bottom() {
        let (mut term, dims) = term_with("A1\r\nA2\r\nA3\r\nA4\r\nA5\r\n");
        term.scroll_display(Scroll::Delta(2));
        let pinned = visible_lines(&term, dims.rows, dims.cols);
        assert_ne!(term.grid().display_offset(), 0, "没能滚回历史");

        // alacritty 会钉屏：新输出只进滚动缓冲，可视区不动
        let mut p: Processor = Processor::new();
        p.advance(&mut term, b"B1\r\nB2\r\nB3\r\n");
        assert_eq!(
            visible_lines(&term, dims.rows, dims.cols),
            pinned,
            "alacritty 的钉屏行为变了，这个测试的前提不成立了"
        );

        snap_to_bottom(&mut term);
        assert_eq!(
            term.grid().display_offset(),
            0,
            "归位后 display_offset 没清零"
        );
        let after = visible_lines(&term, dims.rows, dims.cols);
        assert_ne!(after, pinned, "归位后屏幕上还是旧的那几行");
        assert!(
            after.iter().any(|l| l.trim_end() == "B3"),
            "归位后没看到最新输出：{after:?}"
        );
    }
}
