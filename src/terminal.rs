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
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config as TermConfig, Term};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, Rgb};
use eframe::egui::{self, Color32, FontId, Rect, Sense, Ui, Vec2};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

use crate::theme;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Dims {
    cols: usize,
    rows: usize,
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
            Self::Cmd => which("cmd.exe").or_else(|| std::env::var_os("COMSPEC").map(PathBuf::from)),
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

    fn build(self, cwd: &Path, settings: &TerminalSettings) -> Option<CommandBuilder> {
        let program = self.program(settings)?;
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
        Some(cmd)
    }
}

/// 终端设置：程序路径与启动参数按 shell 种类覆盖，都来自 `config.json`。
/// 缺省即自动探测 + 内置参数，所以配置文件里只会有用户真正改过的条目。
#[derive(Clone, Debug, Default)]
pub struct TerminalSettings {
    /// 种类 id -> 可执行文件路径
    pub programs: BTreeMap<String, String>,
    /// 种类 id -> 启动参数字符串（空格分隔，双引号包住带空格的参数）
    pub args: BTreeMap<String, String>,
}

impl TerminalSettings {
    pub fn from_config(programs: &BTreeMap<String, String>, args: &BTreeMap<String, String>) -> Self {
        Self {
            programs: programs.clone(),
            args: args.clone(),
        }
    }

    /// 写回配置，顺手丢掉空串，配置文件才不会积累噪音。
    pub fn to_config(&self) -> (BTreeMap<String, String>, BTreeMap<String, String>) {
        let clean = |map: &BTreeMap<String, String>| {
            map.iter()
                .filter(|(_, v)| !v.trim().is_empty())
                .map(|(k, v)| (k.clone(), v.trim().to_string()))
                .collect::<BTreeMap<_, _>>()
        };
        (clean(&self.programs), clean(&self.args))
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
    pub shell: ShellKind,
    term: Term<Listener>,
    processor: Processor,
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    rx: Receiver<Msg>,
    dims: Dims,
    pub exited: bool,
    scroll_acc: f32,
    /// 下一帧请求焦点（新建终端后自动聚焦，省得用户还要点一下）
    pub focus_requested: bool,
}

impl Terminal {
    pub fn spawn(
        id: u64,
        cwd: &Path,
        cols: usize,
        rows: usize,
        shell: ShellKind,
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

        let cmd = shell.build(cwd, settings).ok_or_else(|| {
            format!("找不到 {} 的可执行文件，可在终端设置里指定路径", shell.label())
        })?;

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
            title: shell.label().to_string(),
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
    }

    /// 画出这块终端。PTY 输出一律由 [`TerminalPanel::pump`] 每帧统一读，
    /// 这里只负责渲染和键盘，所以没被画到的面板也不会积压输出。
    pub fn ui(&mut self, ui: &mut Ui, font_size: f32, dark: bool, id: egui::Id) {
        let font = FontId::monospace(font_size);
        // 与编辑器统一行高，中文不会把行撑高
        let row_h = theme::mono_row_height(font_size);
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

        let bg = if dark {
            Color32::from_rgb(24, 26, 30)
        } else {
            Color32::from_rgb(252, 252, 253)
        };
        ui.painter().rect_filled(rect, 0.0, bg);

        let painter = ui.painter_at(rect);

        let display_offset = self.term.renderable_content().display_offset;
        let grid = self.term.grid();
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
                        run = if is_default { None } else { Some((col, cell_bg)) };
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
                    egui::pos2(rect.left() + col as f32 * char_w + width * 0.5, y + row_h * 0.5),
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
        }

        // 滚动
        if resp.hovered() {
            let delta = ui.input(|i| i.smooth_scroll_delta.y);
            if delta != 0.0 {
                self.scroll_acc += delta;
                let steps = (self.scroll_acc / 30.0) as i32;
                if steps != 0 {
                    self.scroll_acc -= steps as f32 * 30.0;
                    self.term.scroll_display(Scroll::Delta(steps));
                }
            }
        }

        // 键盘输入：只在有焦点时接管，并把文本类事件从队列里摘掉，
        // 否则编辑器的 TextEdit 会再吃一遍同样的字符。
        if focused {
            let mut out: Vec<u8> = Vec::new();
            ui.input_mut(|i| {
                i.events.retain(|event| {
                    match event {
                        egui::Event::Text(_) | egui::Event::Key { .. } | egui::Event::Paste(_) => {
                            encode_event(event, &mut out);
                            false
                        }
                        _ => true,
                    }
                });
            });
            self.write(&out);
        }

        if self.exited {
            painter.text(
                rect.center_bottom() - Vec2::new(0.0, 18.0),
                egui::Align2::CENTER_BOTTOM,
                "进程已退出",
                FontId::proportional(12.0),
                Color32::from_rgb(200, 120, 120),
            );
        }
    }
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
    pub shell: ShellKind,
    pub available: Vec<ShellKind>,
    /// 程序路径 / 启动参数的覆盖项，来自 config.json
    pub settings: TerminalSettings,
}

impl TerminalPanel {
    pub fn new(cwd: PathBuf, settings: TerminalSettings) -> Self {
        let available = ShellKind::detect(&settings);
        let shell = available.first().copied().unwrap_or(ShellKind::Cmd);
        Self {
            terms: Vec::new(),
            next_id: 1,
            cwd,
            shell,
            available,
            settings,
        }
    }

    /// 套用配置里的 shell 偏好；本机没有这个种类就保持自动选择。
    pub fn apply_pref(&mut self, id: &str) {
        if let Some(kind) = ShellKind::from_id(id) {
            if self.available.contains(&kind) {
                self.shell = kind;
            }
        }
    }

    /// 改了设置后重算可用列表；首选被挪走时退回自动选择。
    pub fn refresh(&mut self) {
        self.available = ShellKind::detect(&self.settings);
        if !self.available.contains(&self.shell) {
            self.shell = self.available.first().copied().unwrap_or(ShellKind::Cmd);
        }
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
            term.shell.label().to_string()
        } else {
            truncate(&term.title, 24)
        };
        Some(if term.exited {
            format!("{base} · 已退出")
        } else {
            base
        })
    }
    /// 起一个新终端，返回它的 id。
    pub fn create(&mut self) -> Result<u64, String> {
        let id = self.next_id;
        self.next_id += 1;
        let term = Terminal::spawn(id, &self.cwd, 100, 28, self.shell, &self.settings)?;
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
        assert_eq!(split_args("  -NoProfile   -NoLogo "), ["-NoProfile", "-NoLogo"]);
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
        let (programs, args) = s.to_config();
        assert_eq!(programs.len(), 1);
        assert_eq!(
            programs.get("gitbash").map(String::as_str),
            Some(r"D:\Git\bin\bash.exe")
        );
        assert!(args.is_empty());
    }

    #[test]
    fn configured_program_beats_detection() {
        let mut s = TerminalSettings::default();
        s.programs
            .insert("gitbash".into(), r"D:\Softwares\GitBash\bin\bash.exe".into());
        // 路径存在与否交给启动时报错，这里必须认账，否则用户改了配置却静默失效
        assert!(ShellKind::GitBash.program(&s).is_some());
        assert!(ShellKind::detect(&s).contains(&ShellKind::GitBash));
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
}
