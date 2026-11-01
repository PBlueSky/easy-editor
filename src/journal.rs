//! 文本内工作日志块。
//!
//! 不是独立的任务面板，而是直接写在任意普通文本文件里的一种约定格式：
//!
//! ```text
//! ===================================================工作日志1 2026-11-01 13:09 [待处理]
//! 1、一段文字内容
//! 2、一段文字内容
//!
//! ===================================================工作日志2 2026-11-01 13:10 [已完成]
//! 1、一段文字内容
//! ```
//!
//! 头部 = 51 个 `=` + 标题 + 空格 + `YYYY-MM-DD HH:MM` + 空格 + `[状态]`。
//! 本模块只负责：扫描块、生成块、块间跳转、切换状态。

use std::sync::OnceLock;

use chrono::Local;
use regex::Regex;

/// 头部前的分隔线：51 个等号。
pub const RULE: &str = "===================================================";

pub const STATUS_TODO: &str = "待处理";
/// 预留：三态时使用，当前只在切换时产生 待处理 / 已完成
#[allow(dead_code)]
pub const STATUS_DOING: &str = "进行中";
pub const STATUS_DONE: &str = "已完成";

#[derive(Clone, Debug)]
pub struct Block {
    /// 头部所在行（0 基）
    pub line: usize,
    /// 头部行起始字节
    pub head_start: usize,
    /// 头部行结束字节（不含换行）
    pub head_end: usize,
    pub title: String,
    pub date: String,
    pub time: String,
    pub status: String,
    /// 正文结束字节（下一个块头之前，或文本末尾）
    pub body_end: usize,
}

impl Block {
    pub fn is_done(&self) -> bool {
        self.status == STATUS_DONE
    }
}

fn head_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // 标题可能为空，日期时间可选，状态可选，尽量宽松
        Regex::new(
            r"^(?:=+)\s*(?P<title>.*?)\s*(?:(?P<date>\d{4}-\d{2}-\d{2})\s+)?(?:(?P<time>\d{1,2}:\d{2})\s*)?(?:\[(?P<status>[^\]]*)\])?\s*$",
        )
        .expect("日志头部正则合法")
    })
}

/// 判断某一行是否是日志块头部。
pub fn parse_head(line: &str) -> Option<(String, String, String, String)> {
    let trimmed = line.trim_end_matches(['\r', '\n']);
    let eq = trimmed.chars().take_while(|c| *c == '=').count();
    if eq < 3 {
        return None;
    }
    let caps = head_regex().captures(trimmed)?;
    let title = caps.name("title").map(|m| m.as_str().trim()).unwrap_or("");
    let date = caps.name("date").map(|m| m.as_str()).unwrap_or("");
    let time = caps.name("time").map(|m| m.as_str()).unwrap_or("");
    let status = caps.name("status").map(|m| m.as_str().trim()).unwrap_or("");
    let status = if status.is_empty() {
        STATUS_TODO.to_string()
    } else {
        status.to_string()
    };
    // 只有一个等号也算分隔线，但标题全空且无日期时视为普通分隔线，忽略
    if title.is_empty() && date.is_empty() {
        return None;
    }
    Some((
        title.to_string(),
        date.to_string(),
        time.to_string(),
        status,
    ))
}

/// 扫描整篇文本里的所有日志块。
pub fn scan(text: &str) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut line_no = 0usize;
    let mut byte = 0usize;

    for raw in text.split_inclusive('\n') {
        let line_end = byte + raw.trim_end_matches(['\r', '\n']).len();
        if let Some((title, date, time, status)) = parse_head(raw) {
            if let Some(prev) = blocks.last_mut() {
                prev.body_end = byte;
            }
            blocks.push(Block {
                line: line_no,
                head_start: byte,
                head_end: line_end,
                title,
                date,
                time,
                status,
                body_end: text.len(),
            });
        }
        byte += raw.len();
        line_no += 1;
    }
    blocks
}

pub fn now_stamp() -> (String, String) {
    let now = Local::now();
    (
        now.format("%Y-%m-%d").to_string(),
        now.format("%H:%M").to_string(),
    )
}

/// 生成一个块头（不含正文）。
pub fn make_head(title: &str, status: &str) -> String {
    let (date, time) = now_stamp();
    format!("{RULE}{title} {date} {time} [{status}]")
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// 在指定字节位置插入一个新的日志块。
///
/// 返回 (新文本, 光标字节位置)。光标落在正文首行 `1、` 之后，可以直接输入内容。
pub fn insert_block(text: &str, at: usize, title: &str, _index: usize) -> (String, usize) {
    let at = floor_char_boundary(text, at.min(text.len()));

    let mut out = String::with_capacity(text.len() + 256);
    out.push_str(&text[..at]);

    // 保证块头从新行开始
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    // 块之间留一个空行
    if !out.is_empty() && !out.ends_with("\n\n") {
        out.push('\n');
    }

    let head = make_head(title, STATUS_TODO);
    out.push_str(&head);
    out.push('\n');
    out.push_str("1、");
    let caret = out.len();

    // 正文后接空行，再接原有内容
    out.push('\n');
    let rest = &text[at..];
    if !rest.is_empty() && !rest.starts_with('\n') {
        out.push('\n');
    }
    out.push_str(rest);

    (out, caret)
}

/// 下一个块（严格在当前行之后）。
pub fn next_block(blocks: &[Block], line: usize) -> Option<&Block> {
    blocks.iter().find(|b| b.line > line)
}

/// 上一个块（严格在当前行之前）。
pub fn prev_block(blocks: &[Block], line: usize) -> Option<&Block> {
    blocks.iter().rev().find(|b| b.line < line)
}

/// 当前光标所在的块。
pub fn block_at(blocks: &[Block], line: usize) -> Option<&Block> {
    blocks.iter().rev().find(|b| b.line <= line)
}

/// 生成「切换状态」的编辑：返回 (替换范围, 新头部文本)。
pub fn toggle_edit(block: &Block) -> (usize, usize, String) {
    let next = if block.status == STATUS_DONE {
        STATUS_TODO
    } else {
        STATUS_DONE
    };
    let head = format!(
        "{RULE}{} {} {} [{}]",
        block.title, block.date, block.time, next
    );
    (block.head_start, block.head_end, head)
}

/// 在块之间跳转时给状态栏用的提示。
pub fn describe(blocks: &[Block], current: usize) -> String {
    if blocks.is_empty() {
        return "当前文件没有日志块".into();
    }
    let idx = blocks
        .iter()
        .position(|b| b.line == current)
        .map(|i| i + 1)
        .unwrap_or(0);
    if idx == 0 {
        format!("共 {} 个日志块", blocks.len())
    } else {
        format!("第 {}/{} 个日志块", idx, blocks.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_reference_file_format() {
        let text = "\
===================================================工作日志1的标题 2026-11-01 13:09 [待处理]
1、一段文字内容
2、一段文字内容

===================================================工作日志2的标题 2026-11-01 13:10 [待处理]
1、一段文字内容aaa
";
        let blocks = scan(text);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].title, "工作日志1的标题");
        assert_eq!(blocks[0].date, "2026-11-01");
        assert_eq!(blocks[0].time, "13:09");
        assert_eq!(blocks[0].status, STATUS_TODO);
        assert_eq!(blocks[1].title, "工作日志2的标题");
        assert_eq!(blocks[0].body_end, blocks[1].head_start);
    }

    #[test]
    fn insert_then_scan_roundtrip() {
        let text = "第一行\n";
        let (out, caret) = insert_block(text, 0, "工作日志1", 1);
        assert!(out.contains(RULE));
        assert!(out.contains("工作日志1"));
        assert!(out.contains("[待处理]"));
        let blocks = scan(&out);
        assert_eq!(blocks.len(), 1);
        // 光标应落在正文首行末尾
        assert_eq!(&out[..caret], out[..caret].trim_end_matches('\n'));
        assert!(out[..caret].ends_with("1、"));
    }

    #[test]
    fn toggle_changes_status() {
        let text = format!("{}工作 2026-11-01 10:00 [待处理]\n1、x\n", RULE);
        let blocks = scan(&text);
        let (a, b, head) = toggle_edit(&blocks[0]);
        let mut new_text = text.clone();
        new_text.replace_range(a..b, &head);
        let blocks2 = scan(&new_text);
        assert_eq!(blocks2[0].status, STATUS_DONE);
    }

    #[test]
    fn ignores_plain_separator_line() {
        let blocks = scan("=====\n普通内容\n");
        assert!(blocks.is_empty());
    }
}
