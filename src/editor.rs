//! 代码编辑器控件：行号 + 语法高亮 + 可编辑 + 跳转 + 大文件虚拟渲染。

use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui::text::{LayoutJob, TextFormat};
use eframe::egui::{self, Color32, FontId, Galley, Rect, ScrollArea, Sense, TextEdit, Ui, Vec2};

use crate::syntax::{Highlighter, LineSpans};
use crate::theme;

/// 超过这个大小就不再用 TextEdit 全量排版，改走只读虚拟渲染。
pub const LARGE_FILE_BYTES: usize = 1024 * 1024;
/// 超过这个行数同样按大文件处理。
pub const LARGE_FILE_LINES: usize = 20_000;
/// 超过这个大小直接拒绝打开，避免把内存吃光。
pub const MAX_FILE_BYTES: usize = 16 * 1024 * 1024;
/// 同时打开的标签上限。
pub const MAX_TABS: usize = 40;

/// 外部发起的一次文本编辑（用于插入日志块、替换等）。
pub struct PendingEdit {
    /// 替换的字节范围；None 表示在光标处插入。
    pub range: Option<(usize, usize)>,
    pub insert: String,
    /// 编辑完成后光标所在的字节偏移。
    pub caret: Option<usize>,
}

pub struct OpenFile {
    pub path: PathBuf,
    pub text: String,
    pub dirty: bool,
    pub line: usize,
    pub col: usize,
    /// 待跳转的字符偏移（来自搜索结果）
    pub pending_jump: Option<usize>,
    /// 跳转时不要把焦点从别的控件（比如查找框）抢过来
    pub jump_without_focus: bool,
    /// 外部发起的编辑
    pub pending_edit: Option<PendingEdit>,
    /// 大文件：只读虚拟渲染
    pub large: bool,
    /// 每行起始字节偏移，大文件模式下按需切片
    line_offsets: Vec<usize>,
    /// 查找匹配（全局字节范围），用于高亮。用 Arc 共享，避免每帧深拷贝。
    pub matches: Arc<Vec<(usize, usize)>>,
    pub current_match: Option<usize>,
    pub bytes_len: usize,
    /// 当前光标字节偏移，供外部插入使用
    pub caret_byte: usize,
    /// 当前选中的文本（没有选区就是空串）。
    /// 用于 `Ctrl+F` 时自动把选中的词填进查找框。
    pub selection: String,
    /// 最近一次加载/保存时的修改时间，用于外部改动检测
    pub mtime: Option<std::time::SystemTime>,
}

impl OpenFile {
    pub fn new(path: PathBuf, text: String) -> Self {
        let bytes_len = text.len();
        let line_offsets = compute_line_offsets(&text);
        let large = bytes_len > LARGE_FILE_BYTES || line_offsets.len() > LARGE_FILE_LINES;
        Self {
            path,
            text,
            dirty: false,
            line: 1,
            col: 1,
            pending_jump: None,
            jump_without_focus: false,
            pending_edit: None,
            large,
            line_offsets,
            matches: Arc::new(Vec::new()),
            current_match: None,
            bytes_len,
            caret_byte: 0,
            selection: String::new(),
            mtime: None,
        }
    }

    pub fn title(&self) -> String {
        self.path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| self.path.to_string_lossy().to_string())
    }

    pub fn line_count(&self) -> usize {
        self.line_offsets.len()
    }

    /// 重新计算行索引（文本变化后调用）。
    pub fn reindex(&mut self) {
        self.line_offsets = compute_line_offsets(&self.text);
        self.bytes_len = self.text.len();
    }

    pub fn line_text(&self, index: usize) -> &str {
        let Some(&start) = self.line_offsets.get(index) else {
            return "";
        };
        let end = self
            .line_offsets
            .get(index + 1)
            .copied()
            .unwrap_or(self.text.len());
        self.text[start..end].trim_end_matches(['\r', '\n'])
    }

    /// 字节偏移 -> 行号（0 基）
    pub fn line_of_byte(&self, byte: usize) -> usize {
        match self.line_offsets.binary_search(&byte) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        }
    }

    /// 应用一次外部编辑，返回是否成功。
    pub fn apply_edit(&mut self, edit: PendingEdit) {
        let (start, end) = match edit.range {
            Some((a, b)) => (a.min(self.text.len()), b.min(self.text.len())),
            None => (self.caret_byte.min(self.text.len()), self.caret_byte.min(self.text.len())),
        };
        let (start, end) = if start <= end { (start, end) } else { (end, start) };
        self.text.replace_range(start..end, &edit.insert);
        self.reindex();
        self.dirty = true;
        self.matches = Arc::new(Vec::new());
        self.current_match = None;
        if let Some(caret) = edit.caret {
            let caret = caret.min(self.text.len());
            self.caret_byte = caret;
            self.pending_jump = Some(caret);
        }
    }
}

pub fn compute_line_offsets(text: &str) -> Vec<usize> {
    let mut offsets = Vec::with_capacity(text.len() / 32 + 1);
    offsets.push(0);
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            offsets.push(i + 1);
        }
    }
    offsets
}

pub struct EditorOutput {
    pub changed: bool,
}

fn char_index_to_pos(text: &str, char_idx: usize) -> (usize, usize) {
    let mut line = 1usize;
    let mut col = 1usize;
    for (i, ch) in text.chars().enumerate() {
        if i >= char_idx {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

fn byte_to_char_index(text: &str, byte: usize) -> usize {
    text[..byte.min(text.len())].chars().count()
}

/// 字符偏移 -> 字节偏移
pub fn char_to_byte(text: &str, char_idx: usize) -> usize {
    text.char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(text.len())
}

/// 画一个编辑面板。
///
/// `pane` 是这块面板的身份，用来给滚动位置和光标做持久化 id。
/// 同一个文件可能在两块面板里出现（对比 / 边看边写），
/// 少了这个参数两边就会共用一份 state，光标互相打架。
pub fn show(
    ui: &mut Ui,
    file: &mut OpenFile,
    hl: &mut Highlighter,
    font_size: f32,
    dark: bool,
    pane: egui::Id,
) -> EditorOutput {
    if file.large {
        return show_large(ui, file, font_size, dark, pane);
    }

    let spans: Arc<Vec<LineSpans>> = hl.highlight(Some(&file.path), &file.text);
    let line_count = spans.len().max(1).max(file.line_count());
    let font_id = FontId::monospace(font_size);
    // 与 theme 保持一致的固定行高，避免中英文混排时行高抖动
    let row_height = theme::mono_row_height(font_size);
    let char_width = ui.fonts_mut(|f| f.glyph_width(&font_id, '0')).max(1.0);

    let digits = line_count.to_string().len().max(2);
    let gutter_width = char_width * digits as f32 + 18.0;

    let num_color = if dark {
        Color32::from_rgb(108, 116, 128)
    } else {
        Color32::from_rgb(150, 156, 166)
    };
    let active_num_color = if dark {
        Color32::from_rgb(215, 220, 226)
    } else {
        Color32::from_rgb(38, 44, 52)
    };
    let current_line_bg = if dark {
        Color32::from_rgba_unmultiplied(255, 255, 255, 12)
    } else {
        Color32::from_rgba_unmultiplied(0, 0, 0, 9)
    };
    let match_bg = if dark {
        Color32::from_rgb(90, 78, 20)
    } else {
        Color32::from_rgb(255, 226, 140)
    };
    let cur_match_bg = if dark {
        Color32::from_rgb(190, 140, 20)
    } else {
        Color32::from_rgb(255, 170, 40)
    };

    let scroll_id = ui.make_persistent_id(("editor_scroll", pane, file.path.clone()));
    let edit_id = ui.make_persistent_id(("editor_text", pane, file.path.clone()));

    // 外部编辑（插入日志块 / 替换）
    if let Some(edit) = file.pending_edit.take() {
        file.apply_edit(edit);
    }

    // 外部跳转：设置光标位置并滚动到对应行。
    // 注意首帧 TextEdit 还没有 state，此时不能把 pending_jump 丢掉，
    // 否则「查找跳到首个匹配」这类刚打开就发起的跳转会静默失效。
    let mut jump_line: Option<usize> = None;
    if let Some(byte) = file.pending_jump {
        let byte = byte.min(file.text.len());
        let char_idx = byte_to_char_index(&file.text, byte);
        if let Some(mut state) = TextEdit::load_state(ui.ctx(), edit_id) {
            state.cursor.set_char_range(Some(
                egui::text::CCursorRange::one(egui::text::CCursor::new(char_idx)),
            ));
            state.store(ui.ctx(), edit_id);
            file.pending_jump = None;
            file.caret_byte = byte;
            jump_line = Some(file.line_of_byte(byte));
            // 查找时不能抢焦点，否则用户打第二个字符就被弹回编辑器
            if !std::mem::take(&mut file.jump_without_focus) {
                ui.memory_mut(|m| m.request_focus(edit_id));
            }
        }
    }

    let mut scroll_offset = None;
    if let Some(line) = jump_line {
        let target = (line as f32) * row_height - ui.available_height() * 0.4;
        scroll_offset = Some(target.max(0.0));
    }

    let mut area = ScrollArea::vertical()
        .id_salt(scroll_id)
        .auto_shrink([false, false]);
    if let Some(off) = scroll_offset {
        area = area.vertical_scroll_offset(off);
    }

    let mut changed = false;

    area.show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        ui.horizontal_top(|ui| {
            let gutter_height = row_height * line_count as f32 + 8.0;
            let (gutter_rect, _) =
                ui.allocate_exact_size(Vec2::new(gutter_width, gutter_height), Sense::hover());

            let fg = hl.fg;
            let spans_for_layout = spans.clone();
            let matches_for_layout = file.matches.clone();
            let current_match = file.current_match;
            let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, _wrap: f32| -> Arc<Galley> {
                build_galley(
                    ui,
                    buf.as_str(),
                    &spans_for_layout,
                    &matches_for_layout,
                    current_match,
                    &font_id,
                    fg,
                    row_height,
                    match_bg,
                    cur_match_bg,
                )
            };

            let editor = TextEdit::multiline(&mut file.text)
                .id(edit_id)
                .code_editor()
                .font(font_id.clone())
                .desired_width(f32::INFINITY)
                .desired_rows(line_count)
                .frame(egui::Frame::NONE)
                .margin(egui::Margin::symmetric(6, 4))
                .layouter(&mut layouter);

            let response = ui.add(editor);
            changed = response.changed();

            let text_top = response.rect.top() + 4.0;
            let painter = ui.painter_at(gutter_rect);

            // 只绘制可视范围内的行号
            let clip = ui.clip_rect();
            let first = (((clip.top() - text_top) / row_height).floor().max(0.0)) as usize;
            let last = ((((clip.bottom() - text_top) / row_height).ceil()) as usize).min(line_count);

            for i in first..last {
                let y = text_top + i as f32 * row_height;
                let is_current = i + 1 == file.line;
                painter.text(
                    egui::pos2(gutter_rect.right() - 8.0, y + row_height * 0.5),
                    egui::Align2::RIGHT_CENTER,
                    (i + 1).to_string(),
                    FontId::monospace(font_size * 0.88),
                    if is_current { active_num_color } else { num_color },
                );
            }

            // 当前行底色
            if file.line >= 1 && file.line <= line_count {
                let y = text_top + (file.line - 1) as f32 * row_height;
                let rect = Rect::from_min_size(
                    egui::pos2(response.rect.left(), y),
                    Vec2::new(response.rect.width(), row_height),
                );
                ui.painter().rect_filled(rect, 0.0, current_line_bg);
            }

            // 同步光标行列到状态栏
            if let Some(state) = TextEdit::load_state(ui.ctx(), edit_id) {
                if let Some(range) = state.cursor.char_range() {
                    let byte = char_to_byte(&file.text, range.primary.index.0);
                    file.caret_byte = byte;
                    let (l, c) = char_index_to_pos(&file.text, range.primary.index.0);
                    file.line = l;
                    file.col = c;

                    // 选区文本：Ctrl+F 时可以直接拿去搜。
                    // 只在真的有选区时才分配字符串。
                    if range.is_empty() {
                        file.selection.clear();
                    } else {
                        let [min, max] = range.sorted_cursors();
                        let (a, b) = (
                            char_to_byte(&file.text, min.index.0),
                            char_to_byte(&file.text, max.index.0),
                        );
                        if a < b && b <= file.text.len() {
                            file.selection.clear();
                            file.selection.push_str(&file.text[a..b]);
                        }
                    }
                }
            }
        });
    });

    if changed {
        file.dirty = true;
        file.reindex();
        file.matches = Arc::new(Vec::new());
        file.current_match = None;
    }

    EditorOutput { changed }
}

/// 大文件：只读虚拟渲染，只画可见行，内存和耗时都与文件大小无关。
fn show_large(
    ui: &mut Ui,
    file: &mut OpenFile,
    font_size: f32,
    dark: bool,
    pane: egui::Id,
) -> EditorOutput {
    let font_id = FontId::monospace(font_size);
    let row_height = theme::mono_row_height(font_size);
    let char_width = ui.fonts_mut(|f| f.glyph_width(&font_id, '0')).max(1.0);
    let line_count = file.line_count();
    let digits = line_count.to_string().len().max(2);
    let gutter_width = char_width * digits as f32 + 18.0;

    let fg = if dark {
        Color32::from_rgb(212, 217, 224)
    } else {
        Color32::from_rgb(40, 44, 52)
    };
    let num_color = if dark {
        Color32::from_rgb(100, 108, 120)
    } else {
        Color32::from_rgb(150, 156, 166)
    };

    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(format!(
                "大文件只读预览 · {} 行 · {} · 仅渲染可见行",
                line_count,
                human_size(file.bytes_len)
            ))
            .size(11.5)
            .weak(),
        );
    });

    let mut jump = file.pending_jump.take();
    let mut area = ScrollArea::vertical()
        .id_salt(ui.make_persistent_id(("large_scroll", pane, file.path.clone())))
        .auto_shrink([false, false]);
    if let Some(byte) = jump.take() {
        let line = file.line_of_byte(byte.min(file.text.len()));
        area = area.vertical_scroll_offset((line as f32 * row_height - 80.0).max(0.0));
    }

    area.show_rows(ui, row_height, line_count, |ui, range| {
        for i in range {
            let text = file.line_text(i);
            let y = ui.cursor().top();
            let (rect, _) =
                ui.allocate_exact_size(Vec2::new(ui.available_width(), row_height), Sense::hover());
            let painter = ui.painter_at(rect);
            painter.text(
                egui::pos2(rect.left() + gutter_width - 8.0, y + row_height * 0.5),
                egui::Align2::RIGHT_CENTER,
                (i + 1).to_string(),
                FontId::monospace(font_size * 0.88),
                num_color,
            );
            if !text.is_empty() {
                painter.text(
                    egui::pos2(rect.left() + gutter_width + 6.0, y + row_height * 0.5),
                    egui::Align2::LEFT_CENTER,
                    text,
                    font_id.clone(),
                    fg,
                );
            }
        }
    });

    EditorOutput { changed: false }
}

pub fn human_size(bytes: usize) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    let b = bytes as f64;
    if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

#[allow(clippy::too_many_arguments)]
fn build_galley(
    ui: &Ui,
    text: &str,
    spans: &[LineSpans],
    matches: &[(usize, usize)],
    current: Option<usize>,
    font_id: &FontId,
    fallback: Color32,
    line_height: f32,
    match_bg: Color32,
    cur_match_bg: Color32,
) -> Arc<Galley> {
    let mut job = LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;

    let base = TextFormat {
        font_id: font_id.clone(),
        color: fallback,
        // 固定行高：中英文混排时行高不抖动，基线由字体的 y_offset_factor 校正
        line_height: Some(line_height),
        valign: egui::Align::BOTTOM,
        ..Default::default()
    };

    let total_lines = text.split('\n').count();
    let mut line_start = 0usize;
    // 匹配范围已排序，用游标推进避免逐行扫描
    let mut match_cursor = 0usize;

    for (i, line) in text.split('\n').enumerate() {
        let line_end = line_start + line.len();
        while match_cursor < matches.len() && matches[match_cursor].1 <= line_start {
            match_cursor += 1;
        }

        match spans.get(i) {
            Some(parts) if !parts.is_empty() => {
                let mut offset = line_start;
                for (color, piece) in parts {
                    let piece_start = offset;
                    let piece_end = offset + piece.len();
                    offset = piece_end;
                    append_with_matches(
                        &mut job,
                        piece,
                        piece_start,
                        piece_end,
                        *color,
                        &base,
                        matches,
                        match_cursor,
                        current,
                        match_bg,
                        cur_match_bg,
                    );
                }
                // 高亮结果可能丢掉了行尾空白，补齐保证行宽正确
                if offset < line_end {
                    append_with_matches(
                        &mut job,
                        &text[offset..line_end],
                        offset,
                        line_end,
                        fallback,
                        &base,
                        matches,
                        match_cursor,
                        current,
                        match_bg,
                        cur_match_bg,
                    );
                }
            }
            _ => {
                if !line.is_empty() {
                    append_with_matches(
                        &mut job,
                        line,
                        line_start,
                        line_end,
                        fallback,
                        &base,
                        matches,
                        match_cursor,
                        current,
                        match_bg,
                        cur_match_bg,
                    );
                }
            }
        }

        if i + 1 < total_lines {
            job.append("\n", 0.0, base.clone());
        }
        line_start = line_end + 1;
    }

    ui.fonts_mut(|f| f.layout_job(job))
}

#[allow(clippy::too_many_arguments)]
fn append_with_matches(
    job: &mut LayoutJob,
    piece: &str,
    piece_start: usize,
    piece_end: usize,
    color: Color32,
    base: &TextFormat,
    matches: &[(usize, usize)],
    mut cursor: usize,
    current: Option<usize>,
    match_bg: Color32,
    cur_match_bg: Color32,
) {
    // 找出与本片段相交的所有匹配
    let mut local: Vec<(usize, usize, bool)> = Vec::new();
    let mut idx = cursor;
    while idx < matches.len() {
        let (ms, me) = matches[idx];
        if ms >= piece_end {
            break;
        }
        if me > piece_start {
            let a = ms.max(piece_start) - piece_start;
            let b = me.min(piece_end) - piece_start;
            local.push((a, b, Some(idx) == current));
        }
        idx += 1;
    }
    cursor = idx;
    let _ = cursor;

    if local.is_empty() {
        let mut f = base.clone();
        f.color = color;
        job.append(piece, 0.0, f);
        return;
    }

    // 按匹配边界切分，保证 UTF-8 边界安全
    local.sort_by_key(|x| x.0);
    let mut pos = 0usize;
    for (a, b, is_current) in local {
        if a > pos {
            let mut f = base.clone();
            f.color = color;
            job.append(&piece[pos..a], 0.0, f);
        }
        let s = a.max(pos);
        let e = b.min(piece.len());
        if s < e {
            let mut f = base.clone();
            f.color = color;
            f.background = if is_current { cur_match_bg } else { match_bg };
            job.append(&piece[s..e], 0.0, f);
        }
        pos = pos.max(b);
    }
    if pos < piece.len() {
        let mut f = base.clone();
        f.color = color;
        job.append(&piece[pos..], 0.0, f);
    }
}

pub fn cursor_text(file: &OpenFile) -> String {
    format!("行 {}, 列 {}", file.line, file.col)
}
