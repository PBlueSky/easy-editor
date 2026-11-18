//! 代码编辑器控件：行号 + 语法高亮 + 可编辑 + 跳转 + 大文件虚拟渲染。

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use eframe::egui::epaint::text::Row;
use eframe::egui::text::{CCursor, LayoutJob, TextFormat};
use eframe::egui::{
    self, pos2, Color32, FontId, Galley, Rect, ScrollArea, Sense, TextEdit, Ui, Vec2,
};
use unicode_width::UnicodeWidthChar as _;

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
/// 选词高亮的选区长度上限（字符数）。超过这个长度通常是整段代码，标出来没意义。
const MAX_TERM_CHARS: usize = 64;

/// 一段高亮：字节范围 + 底色。
type Seg = (usize, usize, Color32);

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
    /// 外部请求的选区（字节范围）。写进 TextEdit 状态，效果和鼠标拖选一样，
    /// 因此会照常触发选词高亮。
    pub pending_select: Option<(usize, usize)>,
    /// 大文件：只读虚拟渲染
    pub large: bool,
    /// 每行起始字节偏移，大文件模式下按需切片
    line_offsets: Vec<usize>,
    /// 查找匹配（全局字节范围），用于高亮。用 Arc 共享，避免每帧深拷贝。
    pub matches: Arc<Vec<(usize, usize)>>,
    pub current_match: Option<usize>,
    /// 选词高亮：当前选中的词在全文里的所有出现（字节范围）。
    pub occurrences: Arc<Vec<(usize, usize)>>,
    /// SQL 里报错的那几条语句（字节范围）。执行结果回来时才写，跟查找无关。
    pub sql_errors: Arc<Vec<(usize, usize)>>,
    /// 本文件按 SQL 对待：`Ctrl+Enter` 没选中也没块时可以整文件执行。
    /// 打开 `.sql` 自动为真，其他文件在「数据库」菜单里手动开。
    pub sql_mode: bool,
    /// 上一次高亮用的 `(词, 是否整词)`；选区没变就不重扫全文。
    occurrence_term: Option<(String, bool)>,
    pub bytes_len: usize,
    /// 当前光标字节偏移，供外部插入使用
    pub caret_byte: usize,
    /// 当前选中的文本（没有选区就是空串）。
    /// 用于 `Ctrl+F` 时自动把选中的词填进查找框。
    pub selection: String,
    /// 选区的字节范围，没有选区就是 None。
    /// `selection` 只是给人看的文本，拿去定位会撞上重复片段；执行 SQL 要的是范围。
    pub selection_range: Option<(usize, usize)>,
    /// 最近一次加载/保存时的修改时间，用于外部改动检测
    pub mtime: Option<std::time::SystemTime>,
    /// 上次打开/保存时的内容指纹。`dirty` 按它算，撤销回原样后标记会自己消失。
    saved_hash: u64,
}

impl OpenFile {
    /// 扩展名算不算 SQL（大小写都认，`.SQL` / `.sql` 一样）。
    pub fn is_sql_path(path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("sql"))
    }

    pub fn new(path: PathBuf, text: String) -> Self {
        let bytes_len = text.len();
        let line_offsets = compute_line_offsets(&text);
        let large = bytes_len > LARGE_FILE_BYTES || line_offsets.len() > LARGE_FILE_LINES;
        let saved_hash = text_hash(&text);
        let sql_mode = Self::is_sql_path(&path);
        Self {
            path,
            text,
            dirty: false,
            line: 1,
            col: 1,
            pending_jump: None,
            jump_without_focus: false,
            pending_edit: None,
            pending_select: None,
            large,
            line_offsets,
            matches: Arc::new(Vec::new()),
            current_match: None,
            occurrences: Arc::new(Vec::new()),
            sql_errors: Arc::new(Vec::new()),
            sql_mode,
            occurrence_term: None,
            bytes_len,
            caret_byte: 0,
            selection: String::new(),
            selection_range: None,
            mtime: None,
            saved_hash,
        }
    }

    /// 按「当前内容是否和上次打开/保存时一致」重算 `dirty`。
    /// 撤销回原样、替换 0 处这类操作虽然会触发 TextEdit 的 changed，
    /// 但文件其实没变，不该继续挂着未保存标记。
    pub fn refresh_dirty(&mut self) {
        self.dirty = text_hash(&self.text) != self.saved_hash;
    }

    /// 落盘成功后调用：记下新内容，清掉未保存标记。
    pub fn mark_saved(&mut self) {
        self.saved_hash = text_hash(&self.text);
        self.dirty = false;
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
        // 文本一动，所有字节偏移的旧匹配立刻作废，下一帧按新选区重算
        self.occurrences = Arc::new(Vec::new());
        self.occurrence_term = None;
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
            None => (
                self.caret_byte.min(self.text.len()),
                self.caret_byte.min(self.text.len()),
            ),
        };
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        self.text.replace_range(start..end, &edit.insert);
        self.reindex();
        self.refresh_dirty();
        self.matches = Arc::new(Vec::new());
        self.current_match = None;
        // 块头回写 `[待执行]`→`[失败]` 会让后面所有字节前移，红底得跟着挪，
        // 不然下一次打开文件它就骑在别的语句上了
        if !self.sql_errors.is_empty() {
            let moved = shift_marks(&self.sql_errors, start, end, edit.insert.len());
            self.sql_errors = Arc::new(moved);
        }
        if let Some(caret) = edit.caret {
            let caret = caret.min(self.text.len());
            self.caret_byte = caret;
            self.pending_jump = Some(caret);
        }
    }
}

/// 文本改了一段，标错的区间跟着挪。
///
/// 整段在改动之前的原样留下，整段在之后的按增量平移，改动穿过的那条丢掉——
/// 那片红说的是老句子，挂到新文本上只会指错地方。
fn shift_marks(
    ranges: &[(usize, usize)],
    start: usize,
    end: usize,
    insert_len: usize,
) -> Vec<(usize, usize)> {
    let delta = insert_len as i64 - (end - start) as i64;
    ranges
        .iter()
        .filter_map(|&(a, b)| {
            if b <= start {
                Some((a, b))
            } else if a >= end {
                Some(((a as i64 + delta) as usize, (b as i64 + delta) as usize))
            } else {
                None
            }
        })
        .collect()
}

/// 内容指纹。只用来和「上次保存的内容」比较，碰撞概率可以忽略。
fn text_hash(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
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

/// 判断一次选区适不适合当作一个「词」来全文高亮。
///
/// 返回 `(词, 是否整词匹配)`：选区两端都贴着单词字符说明只圈到了半截词
/// （比如从 `foo_bar` 里选了 `foo`），这时退回子串匹配更符合直觉。
/// 跨行、纯空白、过长的选区返回 None——那些通常是整段代码，标不出来。
fn selection_term(text: &str, a: usize, b: usize) -> Option<(String, bool)> {
    if a >= b || b > text.len() {
        return None;
    }
    let raw = &text[a..b];
    if raw.contains('\n') {
        return None;
    }
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.chars().count() > MAX_TERM_CHARS {
        return None;
    }
    // trim 掉的前导空白会让左端点右移，边界判断要用 trim 后的位置
    let ta = a + (raw.len() - raw.trim_start().len());
    let tb = ta + trimmed.len();
    let glued = text[..ta]
        .chars()
        .next_back()
        .is_some_and(crate::find::is_word_char)
        || text[tb..]
            .chars()
            .next()
            .is_some_and(crate::find::is_word_char);
    Some((trimmed.to_string(), !glued))
}

/// 用上一帧的选区刷新选词高亮。
///
/// 必须在排版之前读：`TextEdit` 这一帧才刚把鼠标选区写进 state，
/// 而 galley 已经排完了，放到后面就只能等下一帧。
fn sync_occurrences(ui: &Ui, file: &mut OpenFile, edit_id: egui::Id) {
    let plan = match TextEdit::load_state(ui.ctx(), edit_id) {
        Some(state) => state
            .cursor
            .char_range()
            .filter(|range| !range.is_empty())
            .map(|range| {
                let [min, max] = range.sorted_cursors();
                (min.index.0, max.index.0)
            })
            .and_then(|(a, b)| {
                selection_term(
                    &file.text,
                    char_to_byte(&file.text, a),
                    char_to_byte(&file.text, b),
                )
            }),
        None => None,
    };
    if plan == file.occurrence_term {
        return;
    }
    let list = match &plan {
        Some((term, whole)) => crate::find::find_occurrences(&file.text, term, *whole),
        None => Vec::new(),
    };
    file.occurrence_term = plan;
    file.occurrences = Arc::new(list);
    // 高亮变了要保证再画一帧，否则鼠标停下后的最后一次选区可能永远不生效
    ui.ctx().request_repaint();
}

/// 把查找匹配和选词出现合并成一份按起点升序的着色区间，交给排版器一次扫完。
///
/// 两者重叠时保留查找的底色：`append_with_matches` 先到先得，
/// 所以查找段要排在选词段前面，靠稳定排序保持这个次序。
fn merge_overlay(
    matches: &[(usize, usize)],
    current: Option<usize>,
    occurrences: &[(usize, usize)],
    match_bg: Color32,
    cur_match_bg: Color32,
    occ_bg: Color32,
) -> Vec<Seg> {
    let mut out = Vec::with_capacity(matches.len() + occurrences.len());
    for (i, (a, b)) in matches.iter().enumerate() {
        let bg = if Some(i) == current {
            cur_match_bg
        } else {
            match_bg
        };
        out.push((*a, *b, bg));
    }
    out.extend(occurrences.iter().map(|(a, b)| (*a, *b, occ_bg)));
    out.sort_by_key(|seg| seg.0);
    out
}

/// 画一个编辑面板。
///
/// `pane` 是这块面板的身份，用来给滚动位置和光标做持久化 id。
/// 同一个文件可能在两块面板里出现（对比 / 边看边写），
/// 少了这个参数两边就会共用一份 state，光标互相打架。
/// `wrap` 是自动换行开关：开着长行折进视口，关着长行横向滚出去。
pub fn show(
    ui: &mut Ui,
    file: &mut OpenFile,
    hl: &mut Highlighter,
    font_size: f32,
    dark: bool,
    pane: egui::Id,
    wrap: bool,
) -> EditorOutput {
    if file.large {
        return show_large(ui, file, font_size, dark, pane);
    }

    // 外部编辑（插入日志块 / 替换）先落地，再量行数。
    // 顺序反了的话，这一帧的行数还是旧文本的，而排版表已经按新文本排好了，
    // 两边对不上，跳转只能退回「行号 × 行高」的粗算——折行的文件里能差出上百行，
    // 插完日志块就被甩回文件上面去。
    if let Some(edit) = file.pending_edit.take() {
        file.apply_edit(edit);
    }

    let line_count = file.line_count().max(1);
    let font_id = FontId::monospace(font_size);
    // 与 theme 保持一致的固定行高，避免中英文混排时行高抖动
    let row_height = theme::mono_row_height(font_size, ui.ctx().pixels_per_point());
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
    // 选词高亮要比查找淡一档，两种高亮同时在场时不能互相盖掉
    let occ_bg = if dark {
        Color32::from_rgb(45, 68, 88)
    } else {
        Color32::from_rgb(190, 215, 240)
    };
    // SQL 报错的语句：半透明红，笔画还得能看清，这条语句人是要照着改的
    let err_bg = if dark {
        Color32::from_rgba_unmultiplied(196, 60, 52, 96)
    } else {
        Color32::from_rgba_unmultiplied(224, 82, 72, 78)
    };

    let scroll_id = ui.make_persistent_id(("editor_scroll", pane, file.path.clone()));
    let edit_id = ui.make_persistent_id(("editor_text", pane, file.path.clone()));

    // 外部跳转：设置光标位置并滚动到对应行。
    // 注意首帧 TextEdit 还没有 state，此时不能把 pending_jump 丢掉，
    // 否则「查找跳到首个匹配」这类刚打开就发起的跳转会静默失效。
    let mut jump_line: Option<usize> = None;
    // 焦点要等 TextEdit 自己 interact 完再要：见下面 `want_focus` 的注释。
    let mut want_focus = false;
    if let Some(byte) = file.pending_jump {
        let byte = byte.min(file.text.len());
        let char_idx = byte_to_char_index(&file.text, byte);
        if let Some(mut state) = TextEdit::load_state(ui.ctx(), edit_id) {
            state
                .cursor
                .set_char_range(Some(egui::text::CCursorRange::one(
                    egui::text::CCursor::new(char_idx),
                )));
            state.store(ui.ctx(), edit_id);
            file.pending_jump = None;
            file.caret_byte = byte;
            jump_line = Some(file.line_of_byte(byte));
            // 查找时不能抢焦点，否则用户打第二个字符就被弹回编辑器
            want_focus = !std::mem::take(&mut file.jump_without_focus);
        }
    }

    // 外部发起的选区（脚本、以后可能的「跳到定义并选中」）：
    // 和鼠标拖选一样写进 state，所以选词高亮会照常触发。
    // state 还没建好的首帧要留着，下一帧再试。
    // request_focus 不能省：TextEdit 在没有焦点的那一帧会把选区压回单光标。
    if let Some((a, b)) = file.pending_select {
        let range = (a.min(file.text.len()), b.min(file.text.len()));
        if let Some(mut state) = TextEdit::load_state(ui.ctx(), edit_id) {
            let ca = byte_to_char_index(&file.text, range.0);
            let cb = byte_to_char_index(&file.text, range.1);
            state
                .cursor
                .set_char_range(Some(egui::text::CCursorRange::two(
                    egui::text::CCursor::new(cb),
                    egui::text::CCursor::new(ca),
                )));
            state.store(ui.ctx(), edit_id);
            want_focus = true;
            file.pending_select = None;
        }
    }

    // 选中一个词就把全文的相同词一起标出来，方便肉眼定位
    sync_occurrences(ui, file, edit_id);

    // 折行之后一个逻辑行会占好几视觉行，行号和当前行底色都得按视觉行重新对齐。
    // 这张 (每行起始 y, 正文总高) 的表由排版器每帧从排好的 galley 里抽出来。
    let row_tops: RefCell<(Vec<f32>, f32)> = RefCell::new((Vec::new(), 0.0));

    // 不折行时长行会比面板还宽，这时候要能横向滚出去；行号栏在滚动区外面，
    // 滚多远都钉在左边不动。折行时正文铺满视口宽度就不会超出，横向条自己不会出现。
    let mut area = if wrap {
        ScrollArea::vertical()
    } else {
        ScrollArea::both()
    };
    area = area.id_salt(scroll_id).auto_shrink([false, false]);

    let mut changed = false;

    ui.spacing_mut().item_spacing.y = 0.0;
    ui.horizontal_top(|ui| {
        // 行号栏先占掉左边那一格，剩下的整块都给滚动区
        let (gutter_rect, _) = ui.allocate_exact_size(
            Vec2::new(gutter_width, ui.available_height()),
            Sense::hover(),
        );

        let out = area.show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;

            let fg = hl.fg;
            // SQL 模式让任意扩展名都按 SQL 上色：语法选取只看扩展名，
            // 换个名字拿去问比给高亮器一路加标志干净。只用于取语法，不碰磁盘。
            let path = if file.sql_mode {
                file.path.with_extension("sql")
            } else {
                file.path.clone()
            };
            // 高亮必须跟着 layouter 实际拿到的文本算：TextEdit 会先应用本帧的编辑
            // （外部插入日志块、键盘输入）再回调 layouter，提前算好的 spans
            // 一旦字节数对不上就会切坏 UTF-8 字符。
            let matches = file.matches.clone();
            let occurrences = file.occurrences.clone();
            let sql_errors = file.sql_errors.clone();
            let current_match = file.current_match;
            let mut layouter =
                |ui: &Ui, buf: &dyn egui::TextBuffer, wrap_width: f32| -> Arc<Galley> {
                    let text = buf.as_str();
                    let spans = hl.highlight(Some(&path), text);
                    // 报错的语句排在最前：`paint_overlay` 先到先得，
                    // 红底不能被查找 / 选词高亮盖掉——那条语句才是这一帧要人看的地方。
                    let mut overlay: Vec<Seg> =
                        sql_errors.iter().map(|(a, b)| (*a, *b, err_bg)).collect();
                    overlay.extend(merge_overlay(
                        &matches,
                        current_match,
                        &occurrences,
                        match_bg,
                        cur_match_bg,
                        occ_bg,
                    ));
                    // wrap_width 是 TextEdit 这一帧真正能给到文字的宽度：开了横向滚动它是
                    // 无穷大（不折行时本来就要保持原样），折行时正好就是视口宽度。
                    let galley = build_galley(
                        ui,
                        text,
                        &spans,
                        &overlay,
                        &font_id,
                        fg,
                        row_height,
                        if wrap { wrap_width } else { f32::INFINITY },
                    );
                    *row_tops.borrow_mut() = (line_row_tops(&galley), galley.rect.height());
                    galley
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

            // 焦点只能在 TextEdit 自己 interact 之后再要。
            // 点顶栏「日志块」这类按钮时，这一帧既有鼠标点击、鼠标又不在正文上，
            // egui 的 interact 会对「点了别处」的那个已聚焦控件 surrender_focus；
            // 在 add 之前要来的焦点会被它当场抹掉，表现为插完块光标不见了、字也打不了。
            // 已经有焦点就不要再要：`request_focus` 顺带打断输入法组字。
            if want_focus && !ui.memory(|m| m.has_focus(edit_id)) {
                ui.memory_mut(|m| m.request_focus(edit_id));
            }

            // 按住左拖到视口左右边缘时把内容横向推过去，不然选区永远圈不出屏幕。
            if !wrap {
                auto_scroll_x(ui, &response);
            }

            // TextEdit 的 response.rect 已经是去掉 margin 的文字区，
            // 不能再加 4.0，否则行号/当前行底色会比文字低 4pt。
            let text_top = response.rect.top();
            let (tops, text_height) = &*row_tops.borrow();

            // 当前行底色画在滚动区里面，这样它才跟着正文一起滚、被视口裁掉。
            // 折行时这一行可能占了三四视觉行，底色要一起盖住。
            if file.line >= 1 && file.line <= line_count {
                let i = file.line - 1;
                let y = line_top(tops, i, line_count, row_height);
                let h = line_bottom(tops, i, line_count, row_height, *text_height) - y;
                let rect = Rect::from_min_size(
                    egui::pos2(response.rect.left(), text_top + y),
                    Vec2::new(response.rect.width(), h),
                );
                ui.painter().rect_filled(rect, 0.0, current_line_bg);
            }

            // 外部跳转：滚到目标行。折行后行与行的间距不一样，只能查刚排好的表。
            // 横向范围直接给当前视口，等于不动，免得跳一次行把水平位置也甩掉。
            if let Some(line) = jump_line {
                let y = text_top + line_top(tops, line, line_count, row_height);
                ui.scroll_to_rect(
                    Rect::from_x_y_ranges(ui.clip_rect().x_range(), y..=y + row_height),
                    Some(egui::Align::Center),
                );
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
                        file.selection_range = None;
                    } else {
                        let [min, max] = range.sorted_cursors();
                        let (a, b) = (
                            char_to_byte(&file.text, min.index.0),
                            char_to_byte(&file.text, max.index.0),
                        );
                        if a < b && b <= file.text.len() {
                            file.selection.clear();
                            file.selection.push_str(&file.text[a..b]);
                            file.selection_range = Some((a, b));
                        } else {
                            file.selection_range = None;
                        }
                    }
                }
            }

            (response.changed(), text_top)
        });
        changed = out.inner.0;

        // 行号画在滚动区外面，可见范围得自己按视口算；clip 也收到视口那一格，
        // 不然最下面半行数字会印到横向滚动条上去。
        let (tops, _) = std::mem::take(&mut *row_tops.borrow_mut());
        let view = Rect::from_x_y_ranges(gutter_rect.x_range(), out.inner_rect.y_range());
        let painter = ui.painter_at(view);
        // 正文顶边是带滚动偏移的绝对 y，从滚动区里面带出来，行号照它对齐
        let text_top = out.inner.1;
        let (first, last) = visible_lines(&tops, line_count, text_top, view, row_height);

        for i in first..last {
            let y = text_top + line_top(&tops, i, line_count, row_height);
            let is_current = i + 1 == file.line;
            painter.text(
                egui::pos2(gutter_rect.right() - 8.0, y + row_height * 0.5),
                egui::Align2::RIGHT_CENTER,
                (i + 1).to_string(),
                FontId::monospace(font_size * 0.88),
                if is_current {
                    active_num_color
                } else {
                    num_color
                },
            );
        }
    });

    if changed {
        file.refresh_dirty();
        file.reindex();
        file.matches = Arc::new(Vec::new());
        file.current_match = None;
        // SQL 报错的区间是按提交那一刻的文本算的，改过就不作数了
        file.sql_errors = Arc::new(Vec::new());
    }

    EditorOutput { changed }
}

/// 从排好的 galley 里抽出每个逻辑行的起始 y（相对正文顶边）。
///
/// `ends_with_newline` 正好标在段落（也就是逻辑行）的最后一视觉行上，
/// 顺着它切段就能把「第 i 行折成了哪几视觉行」还原出来。
fn line_row_tops(galley: &Galley) -> Vec<f32> {
    let mut tops = Vec::new();
    let mut starts_line = true;
    for row in &galley.rows {
        if starts_line {
            tops.push(row.pos.y);
            starts_line = false;
        }
        if row.ends_with_newline {
            starts_line = true;
        }
    }
    tops
}

/// 逻辑行 `i` 的起始 y。表和行数对不上（刚改完文本、还没重排）就退回一行一视觉行。
fn line_top(tops: &[f32], i: usize, line_count: usize, row_height: f32) -> f32 {
    if tops.len() != line_count {
        return i as f32 * row_height;
    }
    tops[i.min(line_count - 1)]
}

/// 逻辑行 `i` 的下边界：下一行的起点，最后一行接到正文底部。
fn line_bottom(
    tops: &[f32],
    i: usize,
    line_count: usize,
    row_height: f32,
    text_height: f32,
) -> f32 {
    if tops.len() != line_count {
        return line_top(tops, i, line_count, row_height) + row_height;
    }
    if i + 1 < line_count {
        tops[i + 1]
    } else {
        text_height.max(tops[i] + row_height)
    }
}

/// 视口里能看到哪些逻辑行 `[first, last)`。
///
/// 折行后行距不一样，不能再拿「偏移 ÷ 行高」当行号，得在表上找；
/// 表对不上时才退回等距算法。
fn visible_lines(
    tops: &[f32],
    line_count: usize,
    text_top: f32,
    view: Rect,
    row_height: f32,
) -> (usize, usize) {
    if tops.len() != line_count {
        let first = ((view.top() - text_top) / row_height).floor().max(0.0) as usize;
        let last = (((view.bottom() - text_top) / row_height).ceil()).max(0.0) as usize;
        return (first.min(line_count), last.min(line_count));
    }
    let top = (view.top() - text_top).max(0.0);
    let bottom = view.bottom() - text_top;
    let first = tops.partition_point(|t| *t <= top).saturating_sub(1);
    let last = tops.partition_point(|t| *t < bottom).min(line_count);
    (first, last.max(first))
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
    let row_height = theme::mono_row_height(font_size, ui.ctx().pixels_per_point());
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
            .size(crate::theme::sm(ui))
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

/// 拖选靠近视口左右边缘时自动横向滚动。
///
/// egui 的选区只跟着指针走，而指针出不了视口，所以长行一超出屏幕就没法继续往外面圈。
/// 这里在按住不放且指针贴边时逐帧推一格，并且主动要下一帧——指针停在边上是不发事件的。
fn auto_scroll_x(ui: &Ui, response: &egui::Response) {
    /// 离边缘多近开始滚（pt）
    const EDGE: f32 = 28.0;
    /// 每帧滚多少（pt）
    const STEP: f32 = 14.0;

    if !response.is_pointer_button_down_on() {
        return;
    }
    let Some(pos) = ui.input(|i| i.pointer.interact_pos()) else {
        return;
    };
    let clip = ui.clip_rect();
    // 已经滚到头就别再刷帧了：正文右缘抵着视口右缘时，没有东西可以再滚出来
    let dx = if pos.x >= clip.right() - EDGE && response.rect.right() > clip.right() + STEP {
        -STEP
    } else if pos.x <= clip.left() + EDGE && response.rect.left() < clip.left() - STEP {
        STEP
    } else {
        return;
    };
    // scroll_with_delta 给的是「内容往哪儿挪」，符号和偏移量是反的
    ui.scroll_with_delta_animation(Vec2::new(dx, 0.0), egui::style::ScrollAnimation::none());
    ui.ctx().request_repaint();
}

fn build_galley(
    ui: &Ui,
    text: &str,
    spans: &[LineSpans],
    overlay: &[Seg],
    font_id: &FontId,
    fallback: Color32,
    line_height: f32,
    max_width: f32,
) -> Arc<Galley> {
    let mut job = LayoutJob::default();
    // 横向滚动的代价是不折行：这里给无穷大，一行就排一个视觉行；
    // 折行时给的是 TextEdit 实际可用的视口宽度。
    job.wrap.max_width = max_width;

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

    // 等宽字体里 ASCII 一格约 0.55em，而 CJK 字形按 1em 排，中文只占 1.82 格，
    // 后面的英文列就会整体前移。给每个双宽字符补一点前导空白，让它正好占两格；
    // leading_space 不占字符，光标和选区的下标不受影响。
    let (cell_width, wide_advance) =
        ui.fonts_mut(|f| (f.glyph_width(font_id, '0'), f.glyph_width(font_id, '中')));
    let wide_pad = (2.0 * cell_width - wide_advance).max(0.0);

    for (i, line) in text.split('\n').enumerate() {
        let line_end = line_start + line.len();

        match spans.get(i) {
            Some(parts) if !parts.is_empty() => {
                let mut offset = line_start;
                for (color, piece) in parts {
                    append_piece(&mut job, piece, &colored(&base, *color), wide_pad);
                    offset += piece.len();
                }
                // 高亮结果可能丢掉了行尾空白，补齐保证行宽正确
                if offset < line_end {
                    append_piece(
                        &mut job,
                        &text[offset..line_end],
                        &colored(&base, fallback),
                        wide_pad,
                    );
                }
            }
            _ => {
                if !line.is_empty() {
                    append_piece(&mut job, line, &colored(&base, fallback), wide_pad);
                }
            }
        }

        if i + 1 < total_lines {
            job.append("\n", 0.0, base.clone());
        }
        line_start = line_end + 1;
    }

    let mut galley = ui.fonts_mut(|f| f.layout_job(job));
    paint_overlay(&mut galley, text, overlay);
    galley
}

fn colored(base: &TextFormat, color: Color32) -> TextFormat {
    let mut f = base.clone();
    f.color = color;
    f
}

/// 把命中底色画进每一行的字形网格里。
///
/// 不用 `TextFormat::background`：egui 那个矩形是「基线减主字体 ascent」往下数一个
/// 行高，而我们的行高是硬指定的，主字体（Consolas）又比中文矮一截。实测 14 号字，
/// 英文行底色比正文低 0.9pt，中文行直接低 4.9pt——高亮整条压在下一行头上。
/// 自己按视觉行画就没有这个问题，次序上也和 egui 的选区一样排在字形之前，不会盖字。
fn paint_overlay(galley: &mut Arc<Galley>, text: &str, overlay: &[Seg]) {
    let ranges = char_ranges(text, overlay);
    if ranges.is_empty() {
        return;
    }
    let galley = Arc::make_mut(galley);
    for (from, to, color) in ranges {
        let min = galley.layout_from_cursor(CCursor::new(from));
        let max = galley.layout_from_cursor(CCursor::new(to));
        for ri in min.row..=max.row {
            let placed = &mut galley.rows[ri];
            let row = Arc::make_mut(&mut placed.row);
            // 中间几行是被折出来的，整行都要上色
            let left = if ri == min.row {
                row.x_offset(min.column)
            } else {
                0.0
            };
            let right = if ri == max.row {
                row.x_offset(max.column)
            } else {
                row.size.x
            };
            let y = row.size.y;
            if right <= left || y <= 0.0 {
                continue;
            }
            insert_rect(
                row,
                Rect::from_min_max(pos2(left, 0.0), pos2(right, y)),
                color,
            );
        }
    }
}

/// 往一行的网格里插一个底色矩形，插在最靠前的字形三角形之前，这样它压在文字底下。
fn insert_rect(row: &mut Row, rect: Rect, color: Color32) {
    let mesh = &mut row.visuals.mesh;
    let start = row.visuals.glyph_index_start;
    let before = mesh.indices.len();
    mesh.add_colored_rect(rect, color);
    if mesh.indices.len() != before + 6 {
        // 不是「一个矩形 6 个索引」的网格（换了 epaint 版本才会这样），放弃这块底色
        return;
    }
    let tri: Vec<u32> = mesh.indices[before..before + 6].to_vec();

    for i in (start..before).rev() {
        mesh.indices.swap(i, i + 6);
    }
    mesh.indices[start..start + 6].clone_from_slice(&tri);
    row.visuals.mesh_bounds = mesh.calc_bounds();
}

/// 字节区间 -> 字符区间：epaint 的游标数的是字符，不是字节。
///
/// 端点先夹回字符边界（区间来自上一帧，文本可能已经变了），再一遍扫描全文换算，
/// 避免每个命中都从头数一遍字符。
fn char_ranges(text: &str, overlay: &[Seg]) -> Vec<(usize, usize, Color32)> {
    let mut clipped: Vec<(usize, usize, Color32)> = Vec::with_capacity(overlay.len());
    let mut marks: Vec<usize> = Vec::with_capacity(overlay.len() * 2);
    for &(a, b, color) in overlay {
        let (a, b) = (snap_up(text, a), snap_down(text, b));
        if a >= b {
            continue;
        }
        clipped.push((a, b, color));
        marks.push(a);
        marks.push(b);
    }
    if clipped.is_empty() {
        return Vec::new();
    }
    marks.sort_unstable();
    marks.dedup();
    let offsets = char_offsets(text, &marks);
    let at = |byte: usize| -> usize {
        marks
            .binary_search(&byte)
            .map(|k| offsets[k])
            .unwrap_or_default()
    };
    clipped
        .into_iter()
        .map(|(a, b, color)| (at(a), at(b), color))
        .collect()
}

/// `marks` 里每个字节位置对应的字符下标（调用方保证都已落在字符边界上）。
fn char_offsets(text: &str, marks: &[usize]) -> Vec<usize> {
    let mut out = vec![0usize; marks.len()];
    let mut k = 0usize;
    let mut chars = 0usize;
    for (byte, _) in text.char_indices() {
        while k < marks.len() && marks[k] <= byte {
            out[k] = chars;
            k += 1;
        }
        chars += 1;
    }
    // 剩下的都是 text.len() 这类「文末」位置
    for slot in out.iter_mut().skip(k) {
        *slot = chars;
    }
    out
}

/// 写入一段文本。含双宽字符时把它们各自切成一段并补前导空白，
/// 使中文、全角标点都正好占两个等宽格。
fn append_piece(job: &mut LayoutJob, text: &str, format: &TextFormat, wide_pad: f32) {
    if wide_pad <= 0.0 || !text.chars().any(is_wide) {
        job.append(text, 0.0, format.clone());
        return;
    }

    let mut run_start = 0usize;
    for (i, ch) in text.char_indices() {
        if !is_wide(ch) {
            continue;
        }
        if run_start < i {
            job.append(&text[run_start..i], 0.0, format.clone());
        }
        let end = i + ch.len_utf8();
        job.append(&text[i..end], wide_pad, format.clone());
        run_start = end;
    }
    if run_start < text.len() {
        job.append(&text[run_start..], 0.0, format.clone());
    }
}

/// East Asian Wide / Fullwidth 字符（中文、全角标点、日韩文等）。
fn is_wide(ch: char) -> bool {
    ch.width() == Some(2)
}

/// 向后落到最近的字符边界（越界则停在末尾）。
fn snap_up(piece: &str, at: usize) -> usize {
    let mut at = at.min(piece.len());
    while at < piece.len() && !piece.is_char_boundary(at) {
        at += 1;
    }
    at
}

/// 向前落到最近的字符边界。
fn snap_down(piece: &str, at: usize) -> usize {
    let mut at = at.min(piece.len());
    while at > 0 && !piece.is_char_boundary(at) {
        at -= 1;
    }
    at
}

pub fn cursor_text(file: &OpenFile) -> String {
    format!("行 {}, 列 {}", file.line, file.col)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 块头从 `[待执行]` 回写成 `[失败]` 会让正文整体前移几个字节，
    /// 红底要跟着挪；盖在改动里的那条说的是老句子，留着只会指错地方。
    #[test]
    fn an_edit_shifts_the_error_marks() {
        let marks = [(0, 2), (10, 12), (20, 35)];
        assert_eq!(
            shift_marks(&marks, 2, 14, 11),
            vec![(0, 2), (19, 34)],
            "改动之前的原样留下，之后的按增量平移，穿过去的丢掉"
        );
        assert_eq!(
            shift_marks(&marks, 0, 0, 6),
            vec![(6, 8), (16, 18), (26, 41)],
            "块头前面插一行，红底整体后移"
        );
    }

    /// 查找/选词高亮的底色必须严丝合缝盖住自己那一视觉行。
    ///
    /// 以前用 `TextFormat::background`，egui 那个矩形是从「基线减去主字体 ascent」
    /// 往下数一个行高，行高又是我们硬指定的：实测 14 号字英文行低 0.9pt，中文行低
    /// 4.9pt，整条底色压在下一行头上。改成自己往行网格里插矩形之后，矩形就该正好是
    /// `[0, 行高]`——旧实现里根本不会在网格里出现这块颜色，所以断言能挡住回归。
    #[test]
    fn overlay_highlight_covers_exactly_its_visual_row() {
        let ctx = egui::Context::default();
        let bg = Color32::from_rgb(9, 8, 7);
        let text = "aaa find aaa\n中文 查找 命中\n";
        let overlay = vec![
            (4usize, 8usize, bg),
            (
                text.find("查找").unwrap(),
                text.find("查找").unwrap() + "查找".len(),
                bg,
            ),
        ];
        let font_id = FontId::monospace(14.0);
        let line_height = theme::mono_row_height(14.0, 1.0);

        let mut captured: Option<Arc<Galley>> = None;
        let raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            ..Default::default()
        };
        let mut out = ctx.run_pass(raw, |ctx| {
            ctx.root_ui(|ui| {
                captured = Some(build_galley(
                    ui,
                    text,
                    &[],
                    &overlay,
                    &font_id,
                    Color32::LIGHT_GRAY,
                    line_height,
                    f32::INFINITY,
                ));
            });
        });
        // 没有真的渲染器消费字体图集的增量，epaint 会在 drop 时断言
        out.textures_delta.clear();
        let galley = captured.expect("跑了一帧就有 galley");

        for (from, to, _) in &overlay {
            let min = galley.layout_from_cursor(CCursor::new(text[..*from].chars().count()));
            let max = galley.layout_from_cursor(CCursor::new(text[..*to].chars().count()));
            assert_eq!(min.row, max.row, "命中不该跨行");
            let placed = &galley.rows[min.row];
            let row = &placed.row;

            let hit: Vec<egui::Pos2> = row
                .visuals
                .mesh
                .vertices
                .iter()
                .filter(|v| v.color == bg)
                .map(|v| v.pos)
                .collect();
            assert_eq!(hit.len(), 4, "底色矩形的四个角");
            let left = hit.iter().map(|p| p.x).fold(f32::INFINITY, f32::min);
            let right = hit.iter().map(|p| p.x).fold(f32::MIN, f32::max);
            let top = hit.iter().map(|p| p.y).fold(f32::INFINITY, f32::min);
            let bottom = hit.iter().map(|p| p.y).fold(f32::MIN, f32::max);

            // 行内坐标：上沿贴着行顶，下沿正好一个行高
            assert!(top.abs() < 0.01, "底色顶边偏了 {top}");
            assert!((bottom - row.size.y).abs() < 0.01, "底色底边偏了");
            assert!(
                (left - row.x_offset(min.column)).abs() < 0.01,
                "底色左边偏了 {left}"
            );
            assert!(
                (right - row.x_offset(max.column)).abs() < 0.01,
                "底色右边偏了 {right}"
            );

            // 排在字形三角形之前，才不会盖住文字
            let idx = &row.visuals.mesh.indices;
            let start = row.visuals.glyph_index_start;
            let corners = row.visuals.mesh.vertices.len() - 4;
            assert!(
                idx[start..start + 6].iter().all(|i| *i as usize >= corners),
                "底色没有画在字形底下"
            );
            assert!(
                row.visuals.mesh_bounds.top() <= 0.01,
                "mesh_bounds 把底色裁掉了"
            );
        }
    }

    #[test]
    fn whole_word_selection_matches_whole_words() {
        let text = "fn foo() { foo(); }";
        let (term, whole) = selection_term(text, 11, 14).unwrap();
        assert_eq!(term, "foo");
        assert!(whole);
    }

    #[test]
    fn half_word_selection_falls_back_to_substring() {
        // 从 foo_bar 里只圈走 foo，右边还粘着下划线
        let (term, whole) = selection_term("foo_bar", 0, 3).unwrap();
        assert_eq!(term, "foo");
        assert!(!whole);
    }

    #[test]
    fn ignored_selections_return_none() {
        assert!(selection_term("a\nb", 0, 3).is_none());
        assert!(selection_term("hello", 2, 2).is_none());
        assert!(selection_term("   ", 0, 3).is_none());
        let long = "x".repeat(MAX_TERM_CHARS + 1);
        assert!(selection_term(&long, 0, long.len()).is_none());
    }

    #[test]
    fn padded_selection_uses_trimmed_bounds() {
        // 选区带了前后空格，整词判断要用 trim 之后的边界
        let text = "foo bar foo";
        let (term, whole) = selection_term(text, 3, 7).unwrap();
        assert_eq!(term, "bar");
        assert!(whole);
    }

    #[test]
    fn overlay_puts_find_before_occurrence() {
        let segs = merge_overlay(
            &[(4, 7)],
            Some(0),
            &[(0, 3), (4, 7)],
            Color32::RED,
            Color32::GREEN,
            Color32::BLUE,
        );
        assert_eq!(
            segs,
            vec![
                (0, 3, Color32::BLUE),
                (4, 7, Color32::GREEN),
                (4, 7, Color32::BLUE),
            ]
        );
    }
}
