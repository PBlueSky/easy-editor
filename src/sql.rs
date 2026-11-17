//! 文本内 SQL 块。
//!
//! 和日志块同一个思路：块直接写在普通文本文件里，跟着文件走、用 git 管理。
//! 区别在于头部用的是合法的 SQL 注释，所以整份文件原样丢进 mysql 客户端也能跑：
//!
//! ```text
//! -- sql 活跃用户 2026-11-15 14:03 [待执行] conn=demo
//! SELECT id, name FROM users
//! WHERE status = 1
//! LIMIT 50;
//! ```
//!
//! 头部 = `-- sql` + 标题 + `YYYY-MM-DD HH:MM` + `[状态]` + 尾部键值对。
//! 尾部现在只认两个：`conn=名字`（绑定命名连接）和裸标记 `auto`（允许自动执行，
//! 且只对只读语句生效）。认不出的尾巴一律忽略，以后加字段不会读坏老文件。
//!
//! 本模块只负责文本：扫描块、生成块、切语句、判断语句是否只读。不碰数据库也不碰 UI。

use std::sync::OnceLock;

use chrono::Local;
use regex::Regex;

/// 块头前缀。MySQL 要求 `--` 后面必须跟空白才算注释，所以这里不留余地。
pub const PREFIX: &str = "-- sql ";

pub const STATUS_PENDING: &str = "待执行";
pub const STATUS_DONE: &str = "已执行";
pub const STATUS_FAILED: &str = "失败";

/// 允许自动执行的裸标记。
pub const FLAG_AUTO: &str = "auto";

#[derive(Clone, Debug)]
pub struct Block {
    /// 头部所在行（0 基）
    pub line: usize,
    /// 头部行起始字节
    pub head_start: usize,
    /// 头部行结束字节（不含换行）
    pub head_end: usize,
    /// 正文起始字节（头部行的下一行开头）
    pub body_start: usize,
    /// 正文结束字节（下一个块头之前，或文本末尾）
    pub body_end: usize,
    pub title: String,
    pub date: String,
    pub time: String,
    pub status: String,
    /// 绑定的连接名，空串表示用当前默认连接
    pub conn: String,
    /// 头部写了 `auto`
    pub auto: bool,
}

impl Block {
    /// 块正文，也就是要送进数据库的那段 SQL。
    pub fn sql<'a>(&self, text: &'a str) -> &'a str {
        text[self.body_start..self.body_end].trim()
    }

    /// 结果侧存的东西按 (文件, 块 id) 归档。
    ///
    /// id 里刻意不含状态：执行完要把 `[待执行]` 回写成 `[已执行]`，含状态的话 id
    /// 会跟着变，刚拿到的结果就找不到主人了。conn 可以进 id —— 回写时原样保留，
    /// 而用户手改 conn 本来就该让旧结果脱钩（那是另一个库的同一句 SQL）。
    /// 时间戳精确到分钟，同一分钟内重建的同名块共用一个 id，这是有意的：重跑即覆盖。
    pub fn id(&self) -> String {
        format!("{}|{} {}|{}", self.title, self.date, self.time, self.conn)
    }
}

/// 头部解析出来的字段，尚未落到具体字节位置。
#[derive(Clone, Debug, Default)]
pub struct Head {
    pub title: String,
    pub date: String,
    pub time: String,
    pub status: String,
    pub conn: String,
    pub auto: bool,
}

fn head_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // 标题可能为空，日期时间可选，状态可选，尽量宽松；尾巴单独再解析一轮。
        // `(?i)` 让 `-- SQL` 也认，但 `--` 后必须有空白，否则在客户端里就不是注释了。
        // tail 必须以下一个空白起头：不这么写，懒惰的 title 会直接取空串、
        // 后面每个可选组都跳过、tail 一口吞掉整行，日期和状态全都解析不到。
        Regex::new(r"(?i)^[ \t]*--[ \t]+sql[ \t]+(?P<title>.*?)\s*(?:(?P<date>\d{4}-\d{2}-\d{2})[ \t]+)?(?:(?P<time>\d{1,2}:\d{2})[ \t]*)?(?:\[(?P<status>[^\]]*)\])?(?P<tail>[ \t].*)?$")
            .expect("SQL 块头部正则合法")
    })
}

/// 判断某一行是否是 SQL 块头部。
pub fn parse_head(line: &str) -> Option<Head> {
    let trimmed = line.trim_end_matches(['\r', '\n']);
    let caps = head_regex().captures(trimmed)?;
    let name = |k: &str| caps.name(k).map(|m| m.as_str()).unwrap_or("");

    let mut head = Head {
        title: name("title").trim().to_string(),
        date: name("date").to_string(),
        time: name("time").to_string(),
        status: name("status").trim().to_string(),
        ..Default::default()
    };
    if head.status.is_empty() {
        head.status = STATUS_PENDING.to_string();
    }
    for token in name("tail").split_whitespace() {
        if let Some(value) = token.strip_prefix("conn=") {
            head.conn = value.to_string();
        } else if token.eq_ignore_ascii_case(FLAG_AUTO) {
            head.auto = true;
        }
    }
    // 光一句 `-- sql` 不算块：那是别人随手写的注释，别抢过来当块管
    if head.title.is_empty() && head.date.is_empty() {
        return None;
    }
    Some(head)
}

/// 扫描整篇文本里的所有 SQL 块。
pub fn scan(text: &str) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut byte = 0usize;

    for (line_no, raw) in text.split_inclusive('\n').enumerate() {
        let line_end = byte + raw.trim_end_matches(['\r', '\n']).len();
        if let Some(head) = parse_head(raw) {
            let body_start = byte + raw.len();
            if let Some(prev) = blocks.last_mut() {
                prev.body_end = byte;
            }
            blocks.push(Block {
                line: line_no,
                head_start: byte,
                head_end: line_end,
                body_start,
                body_end: text.len(),
                title: head.title,
                date: head.date,
                time: head.time,
                status: head.status,
                conn: head.conn,
                auto: head.auto,
            });
        }
        byte += raw.len();
    }
    blocks
}

/// 光标所在的那个块。块头之前的区域不属于任何块，返回 None。
pub fn block_at(blocks: &[Block], line: usize) -> Option<&Block> {
    blocks.iter().rev().find(|b| b.line <= line)
}

pub fn now_stamp() -> (String, String) {
    let now = Local::now();
    (
        now.format("%Y-%m-%d").to_string(),
        now.format("%H:%M").to_string(),
    )
}

/// 拼一个完整头部。字段顺序固定，尾巴只在用到时才出现。
pub fn head_text(
    title: &str,
    date: &str,
    time: &str,
    status: &str,
    conn: &str,
    auto: bool,
) -> String {
    let mut out = format!("{PREFIX}{title} {date} {time} [{status}]");
    if !conn.is_empty() {
        out.push_str(&format!(" conn={conn}"));
    }
    if auto {
        out.push_str(&format!(" {FLAG_AUTO}"));
    }
    out
}

/// 新建块时用的头部，时间戳取当前时间。
pub fn make_head(title: &str, conn: &str, status: &str) -> String {
    let (date, time) = now_stamp();
    head_text(title, &date, &time, status, conn, false)
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

/// 在指定字节位置插入一个新的 SQL 块。
///
/// 返回 (新文本, 光标字节位置)。光标落在正文起始处，直接开始写 SQL。
pub fn insert_block(text: &str, at: usize, title: &str, conn: &str) -> (String, usize) {
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

    out.push_str(&make_head(title, conn, STATUS_PENDING));
    out.push('\n');
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

/// 生成「改写块状态」的编辑：返回 (替换范围, 新头部文本)。
///
/// 标题、时间戳、conn 和 auto 原样带过去，只换中括号里那一段。
pub fn status_edit(block: &Block, status: &str) -> (usize, usize, String) {
    let head = head_text(
        &block.title,
        &block.date,
        &block.time,
        status,
        &block.conn,
        block.auto,
    );
    (block.head_start, block.head_end, head)
}

/// 在块之间跳转时给状态栏用的提示。
pub fn describe(blocks: &[Block], current: usize) -> String {
    if blocks.is_empty() {
        return "当前文件没有 SQL 块".into();
    }
    let idx = blocks
        .iter()
        .position(|b| b.line == current)
        .map(|i| i + 1)
        .unwrap_or(0);
    if idx == 0 {
        format!("共 {} 个 SQL 块", blocks.len())
    } else {
        format!("第 {}/{} 个 SQL 块", idx, blocks.len())
    }
}

/// 语句是不是只读的。判断不了的一律算写：宁可让用户手动放开，不能默认放行。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatementKind {
    Read,
    Write,
}

impl StatementKind {
    pub fn is_read_only(self) -> bool {
        matches!(self, Self::Read)
    }
}

/// MySQL 规定 `--` 后必须紧跟空白或控制字符才算注释，否则 `1--2` 是两次减法。
/// 判错的话会把分号一起吃掉，语句就悄悄少了一截。
fn is_line_dash(bytes: &[u8], i: usize) -> bool {
    bytes.get(i + 1) == Some(&b'-')
        && matches!(
            bytes.get(i + 2),
            None | Some(&b' ') | Some(&b'\t') | Some(&b'\n') | Some(&b'\r')
        )
}

/// 跳过前导空白和注释，取第一个关键字（小写）。整段没有代码就返回 None。
pub fn first_keyword(sql: &str) -> Option<String> {
    let bytes = sql.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c == b'#' {
            i = skip_line(bytes, i);
            continue;
        }
        if is_line_dash(bytes, i) {
            i = skip_line(bytes, i);
            continue;
        }
        if c == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i = skip_block_comment(bytes, i);
            continue;
        }
        if c == b'(' {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
            i += 1;
        }
        if start == i {
            return None;
        }
        return Some(sql[start..i].to_lowercase());
    }
    None
}

fn skip_line(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && bytes[i] != b'\n' {
        i += 1;
    }
    i + 1
}

fn skip_block_comment(bytes: &[u8], mut i: usize) -> usize {
    i += 2;
    while i < bytes.len() {
        if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
            return i + 2;
        }
        i += 1;
    }
    i
}

/// 判断一条语句的读写性质。
pub fn classify(sql: &str) -> StatementKind {
    match first_keyword(sql).as_deref() {
        Some("select") | Some("show") | Some("describe") | Some("desc") | Some("explain") => {
            StatementKind::Read
        }
        _ => StatementKind::Write,
    }
}

/// 一批语句是否全部只读。空列表算只读（没什么可拦的）。
pub fn all_read_only(stmts: &[String]) -> bool {
    stmts.iter().all(|s| classify(s).is_read_only())
}

/// 这个块能不能自动跑：头部写了 `auto`，正文切出来有语句，而且条条只读。
///
/// 自动执行没有确认这一步，所以门槛宁可高一点：`INSERT`/`UPDATE`/`DELETE` 就算
/// 标了 `auto` 也只走人手 Ctrl+Enter。只有注释、没有语句的块同样不算，
/// 那种正文跑不出结果，自动执行会白占一个面板。
pub fn auto_runnable(block: &Block, text: &str) -> bool {
    if !block.auto {
        return false;
    }
    let stmts = split_statements(block.sql(text));
    !stmts.is_empty() && all_read_only(&stmts)
}

/// 按顶层分号切分语句，跳过引号、反引号和注释里的分号。
///
/// 只做切分，不做语句合法性判断。注释块和 `DELIMITER` 改分隔符的存储过程写法
/// 不在支持范围内——那种文件请人工拆开执行。
pub fn split_statements(sql: &str) -> Vec<String> {
    split_ranges(sql)
        .into_iter()
        .map(|(a, b)| sql[a..b].to_string())
        .collect()
}

/// 和 `split_statements` 同一把尺子，但给的是字节区间而不是文本。
///
/// 报错的语句要在文件里标红，光有文本定位不了——同一句 `SELECT 1` 能写两遍。
/// 两边都走这里，所以第 i 个区间必然对应第 i 条语句。
pub fn split_ranges(sql: &str) -> Vec<(usize, usize)> {
    #[derive(PartialEq)]
    enum State {
        Normal,
        Quote(u8),
        LineComment,
        BlockComment,
    }

    let bytes = sql.as_bytes();
    let mut out: Vec<(usize, usize)> = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    let mut state = State::Normal;

    while i < bytes.len() {
        let c = bytes[i];
        match state {
            State::Normal => {
                // 非 ASCII 字节不可能是分隔符，UTF-8 续字节都 >= 0x80，按字节扫是安全的
                match c {
                    b'\'' | b'"' | b'`' => state = State::Quote(c),
                    b'#' => state = State::LineComment,
                    b'-' if is_line_dash(bytes, i) => state = State::LineComment,
                    b'/' if bytes.get(i + 1) == Some(&b'*') => {
                        state = State::BlockComment;
                        i += 1;
                    }
                    b';' => {
                        push_range(&mut out, sql, start, i);
                        start = i + 1;
                    }
                    _ => {}
                }
                i += 1;
            }
            State::Quote(quote) => {
                if c == b'\\' && quote != b'`' {
                    i += 2;
                    continue;
                }
                // '' 是引号自身的转义写法
                if c == quote && quote == b'\'' && bytes.get(i + 1) == Some(&b'\'') {
                    i += 2;
                    continue;
                }
                if c == quote {
                    state = State::Normal;
                }
                i += 1;
            }
            State::LineComment => {
                if c == b'\n' {
                    state = State::Normal;
                }
                i += 1;
            }
            State::BlockComment => {
                if c == b'*' && bytes.get(i + 1) == Some(&b'/') {
                    state = State::Normal;
                    i += 2;
                    continue;
                }
                i += 1;
            }
        }
    }
    push_range(&mut out, sql, start, sql.len());
    out
}

/// 收下一段候选语句：掐头去尾的空白不算进去，纯注释直接丢掉。
fn push_range(out: &mut Vec<(usize, usize)>, sql: &str, start: usize, end: usize) {
    let raw = &sql[start..end];
    let lead = raw.len() - raw.trim_start().len();
    let stmt = raw.trim();
    if stmt.is_empty() {
        return;
    }
    // 纯注释不是语句，别让它占一个结果面板
    if first_keyword(stmt).is_none() {
        return;
    }
    let a = start + lead;
    out.push((a, a + stmt.len()));
}

/// 从切好的语句里挑出「和光标那一行有关系」的那一条，返回它在 `ranges` 里的下标。
///
/// 判据先看行：光标停在第 2 句那一行，就只该跑第 2 句，而不是把整块端出去。
/// 同一行写了好几条时，优先光标落在其中的那条；光标卡在两句中间就往后取。
/// 这一行压根没有语句（空行、纯注释行）时，取它下面最近的一条；下面也没了才回头给最后一条。
/// 没有任何语句时返回 `None`，让上层去说「这里没东西可跑」，而不是默默跑点别的。
pub fn statement_index_at(sql: &str, ranges: &[(usize, usize)], caret: usize) -> Option<usize> {
    if ranges.is_empty() {
        return None;
    }
    let caret = caret.min(sql.len());
    let line_start = sql[..caret].rfind('\n').map_or(0, |i| i + 1);
    let line_end = sql[caret..].find('\n').map_or(sql.len(), |i| caret + i);

    let on_line: Vec<usize> = ranges
        .iter()
        .enumerate()
        .filter(|&(_, &(a, b))| a < line_end && line_start < b)
        .map(|(i, _)| i)
        .collect();
    // 光标落在句子里头（含首尾那个字符）最没有歧义，先认它
    if let Some(index) = on_line
        .iter()
        .copied()
        .find(|&i| ranges[i].0 <= caret && caret <= ranges[i].1)
    {
        return Some(index);
    }
    // 卡在两句中间的空白里算下一句：光标往右挪一格就开始写它了
    if let Some(index) = on_line.iter().copied().find(|&i| caret < ranges[i].0) {
        return Some(index);
    }
    if let Some(&index) = on_line.last() {
        return Some(index);
    }
    // 这一行压根没有语句：往下找最近的一条，底下没了才回头给最后一条
    Some(
        ranges
            .iter()
            .position(|&(a, _)| caret < a)
            .unwrap_or(ranges.len() - 1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_header() {
        let head = parse_head("-- sql 活跃用户 2026-11-15 14:03 [待执行] conn=demo auto").unwrap();
        assert_eq!(head.title, "活跃用户");
        assert_eq!(head.date, "2026-11-15");
        assert_eq!(head.time, "14:03");
        assert_eq!(head.status, STATUS_PENDING);
        assert_eq!(head.conn, "demo");
        assert!(head.auto);
    }

    #[test]
    fn tolerates_missing_status_and_conn() {
        let head = parse_head("-- sql 探测 2026-11-15 09:00").unwrap();
        assert_eq!(head.status, STATUS_PENDING);
        assert_eq!(head.conn, "");
        assert!(!head.auto);
    }

    #[test]
    fn unknown_tail_tokens_are_ignored() {
        // 以后加新字段时，老版本读到的就是这种尾巴，不能整个块解析失败
        let head =
            parse_head("-- sql x 2026-11-15 09:00 [已执行] conn=a limit=10 whatever").unwrap();
        assert_eq!(head.conn, "a");
        assert_eq!(head.status, STATUS_DONE);
    }

    #[test]
    fn ordinary_sql_comment_is_not_a_block() {
        let text = "-- 说明：下面的查询按 id 排序\n-- sql\nSELECT 1;\n";
        assert!(scan(text).is_empty());
    }

    #[test]
    fn scan_collects_body_up_to_next_head() {
        let text = "\
-- sql 第一块 2026-11-15 14:03 [待执行] conn=demo
SELECT 1;

-- sql 第二块 2026-11-15 14:04 [待执行]
SELECT 2;
";
        let blocks = scan(text);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].sql(text), "SELECT 1;");
        assert_eq!(blocks[1].sql(text), "SELECT 2;");
        assert_eq!(blocks[0].body_end, blocks[1].head_start);
        assert_eq!(blocks[0].line, 0);
        assert_eq!(blocks[1].line, 3);
    }

    #[test]
    fn body_start_is_next_line() {
        let text = "-- sql a 2026-11-15 14:03\nSELECT 1;\n";
        let blocks = scan(text);
        assert_eq!(blocks[0].body_start, "-- sql a 2026-11-15 14:03\n".len());
    }

    #[test]
    fn insert_then_scan_roundtrip() {
        let (out, caret) = insert_block("第一行\n", 5, "工作块", "demo");
        assert!(out[..caret].ends_with('\n'));
        assert!(out.contains("-- sql 工作块"));
        assert!(out.contains("[待执行] conn=demo"));
        let blocks = scan(&out);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].conn, "demo");
        assert_eq!(blocks[0].body_start, caret);
    }

    #[test]
    fn status_edit_keeps_conn_and_auto() {
        let text = "-- sql x 2026-11-15 14:03 [待执行] conn=demo auto\nSELECT 1;\n";
        let blocks = scan(text);
        let (a, b, head) = status_edit(&blocks[0], STATUS_FAILED);
        let mut new_text = text.to_string();
        new_text.replace_range(a..b, &head);
        let again = scan(&new_text);
        assert_eq!(again[0].status, STATUS_FAILED);
        assert_eq!(again[0].conn, "demo");
        assert!(again[0].auto);
        assert_eq!(new_text[again[0].body_start..].trim(), "SELECT 1;");
    }

    #[test]
    fn split_ranges_points_at_the_statement_not_its_padding() {
        let sql = "  SELECT 1 ;\n\n  DELETE FROM t ;  ";
        let ranges = split_ranges(sql);
        assert_eq!(ranges.len(), 2);
        // 区间正好圈住语句本身：前后的空白和分号都不沾，标红才不会糊到行尾
        assert_eq!(&sql[ranges[0].0..ranges[0].1], "SELECT 1");
        assert_eq!(&sql[ranges[1].0..ranges[1].1], "DELETE FROM t");
        assert_eq!(
            split_statements(sql),
            vec!["SELECT 1".to_string(), "DELETE FROM t".to_string()],
            "两条尺子得量出同样的东西"
        );
    }

    #[test]
    fn auto_runnable_only_takes_read_only_bodies() {
        let cases = [
            ("-- sql x 2026-11-15 14:03 auto\nSELECT 1;\n", true),
            (
                "-- sql x 2026-11-15 14:03 auto\nSELECT 1;\nSHOW TABLES;\n",
                true,
            ),
            (
                "-- sql x 2026-11-15 14:03 auto\nSELECT 1;\nDELETE FROM t;\n",
                false,
            ),
            ("-- sql x 2026-11-15 14:03 auto\nUPDATE t SET a=1;\n", false),
            // 没标 auto 的块，正文再只读也不许自动跑
            ("-- sql x 2026-11-15 14:03\nSELECT 1;\n", false),
            // 只有注释：跑不出结果，别白占一个面板
            ("-- sql x 2026-11-15 14:03 auto\n-- 只是注释\n", false),
        ];
        for (text, want) in cases {
            let blocks = scan(text);
            assert_eq!(
                auto_runnable(&blocks[0], text),
                want,
                "「{text}\" 期望自动执行={want}"
            );
        }
    }

    #[test]
    fn block_at_ignores_region_before_first_block() {
        let text = "前言\n\n-- sql x 2026-11-15 14:03\nSELECT 1;\n";
        let blocks = scan(text);
        assert!(block_at(&blocks, 0).is_none());
        assert_eq!(block_at(&blocks, 2).unwrap().title, "x");
        assert_eq!(block_at(&blocks, 9).unwrap().title, "x");
    }

    #[test]
    fn split_keeps_semicolons_inside_quotes_and_comments() {
        let sql = "SELECT 'a;b', \"c;d\", `e;f` -- tail ; comment\nFROM t; SELECT 2;";
        let stmts = split_statements(sql);
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].starts_with("SELECT 'a;b'"));
        assert!(stmts[0].ends_with("FROM t"));
        assert_eq!(stmts[1], "SELECT 2");
    }

    #[test]
    fn split_handles_escaped_and_doubled_quotes() {
        let stmts = split_statements(r"SELECT 'a\;b', 'c'';d'; SELECT 1");
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0], r"SELECT 'a\;b', 'c'';d'");
    }

    #[test]
    fn split_skips_block_comments_and_hash_comments() {
        let stmts = split_statements("SELECT 1 /* ; not a split */; # hash ; end\nSELECT 2");
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].contains("/* ; not a split */"));
    }

    #[test]
    fn split_drops_comment_only_fragments() {
        let stmts = split_statements("-- 只是注释\n/* 也只是注释 */");
        assert!(stmts.is_empty());
    }

    #[test]
    fn block_id_survives_status_writeback() {
        let text = "-- sql x 2026-11-15 14:03 [待执行] conn=demo\nSELECT 1;\n";
        let blocks = scan(text);
        let id = blocks[0].id();
        assert!(id.contains("demo"), "conn 要能区分同一个块指向的不同库");

        let (a, b, head) = status_edit(&blocks[0], STATUS_DONE);
        let mut new_text = text.to_string();
        new_text.replace_range(a..b, &head);
        let after = scan(&new_text);
        assert_eq!(after[0].status, STATUS_DONE);
        // 回写状态后 id 必须不变，否则底部结果区就找不到对应的块了
        assert_eq!(after[0].id(), id);
    }

    #[test]
    fn bare_double_dash_is_arithmetic_not_a_comment() {
        // MySQL 只在 `--` 后紧跟空白时才算注释，`1--2` 里的 `--` 不能当注释吞掉
        let stmts = split_statements("SELECT 1--2 AS x; SELECT 2");
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0], "SELECT 1--2 AS x");
        assert!(classify("SELECT 1--2").is_read_only());
        assert_eq!(first_keyword("SELECT 1--2").as_deref(), Some("select"));
    }

    #[test]
    fn classify_reads_and_writes() {
        assert!(classify("SELECT 1").is_read_only());
        assert!(classify("  -- c\n  SHOW TABLES").is_read_only());
        assert!(classify("(SELECT 1) UNION (SELECT 2)").is_read_only());
        assert!(!classify("INSERT INTO t VALUES (1)").is_read_only());
        assert!(!classify("WITH x AS (SELECT 1) INSERT INTO t SELECT * FROM x").is_read_only());
        assert!(!classify("USE mydb").is_read_only());
        assert_eq!(classify("SELECT 1"), StatementKind::Read);
    }

    #[test]
    fn all_read_only_gate() {
        let ok = vec!["SELECT 1".to_string(), "SHOW TABLES".to_string()];
        assert!(all_read_only(&ok));
        let mixed = vec!["SELECT 1".to_string(), "DELETE FROM t".to_string()];
        assert!(!all_read_only(&mixed));
        assert!(all_read_only(&[]));
    }

    #[test]
    fn generated_head_is_a_valid_mysql_comment() {
        let head = make_head("标题", "demo", STATUS_PENDING);
        // `--` 后面紧跟空白才是注释，否则整份文件在客户端里就报错了
        assert!(head.starts_with("-- sql "), "{head}");
        assert!(parse_head(&head).is_some());
    }

    /// 光标停在哪个字节，`statement_index_at` 该给出第几句。
    fn pick(sql: &str, caret: usize) -> Option<usize> {
        statement_index_at(sql, &split_ranges(sql), caret)
    }

    #[test]
    fn caret_line_picks_its_own_statement() {
        let sql = "SELECT 1;\nSELECT 2;\nSELECT 3;";
        // 第一句里的任意一处（开头、中间、句尾的分号）都只认第一句
        assert_eq!(pick(sql, 0), Some(0));
        assert_eq!(sql[..9].get(7..), Some("1;"));
        assert_eq!(pick(sql, 1), Some(0));
        assert_eq!(pick(sql, 8), Some(0), "句尾的分号还算第一句");
        assert_eq!(pick(sql, 10), Some(1));
        assert_eq!(pick(sql, 17), Some(1), "第二句的末尾");
        assert_eq!(pick(sql, 29), Some(2), "最后一句之后的那一格");
    }

    #[test]
    fn one_line_with_several_statements_still_splits() {
        let sql = "SELECT 1; SELECT 2;";
        assert_eq!(pick(sql, 2), Some(0), "光标在第一句里面");
        assert_eq!(pick(sql, 8), Some(0), "第一句末尾的分号算第一句");
        assert_eq!(pick(sql, 9), Some(1), "夹在两句中间的空格算下一句");
        assert_eq!(pick(sql, 16), Some(1));
    }

    #[test]
    fn blank_and_comment_lines_fall_through_to_the_nearest_one() {
        let sql = "SELECT 1;\n\n-- 说明\nSELECT 2;\n";
        // 空行往下找：落在 SELECT 2，而不是把两条一起端出去
        assert_eq!(pick(sql, 10), Some(1), "空行该抓下面最近的那句");
        assert_eq!(
            pick(sql, sql.len()),
            Some(1),
            "尾行没有语句，回头给最后一条"
        );
        // 光标在第一句自带的那行注释里：还在第一句范围内
        let head = "-- 说明\nSELECT 1;\nSELECT 2;";
        assert_eq!(pick(head, 3), Some(0));
    }

    #[test]
    fn no_statement_means_nothing_to_run() {
        let sql = "\n-- 只是注释\n";
        assert_eq!(pick(sql, 4), None);
        assert_eq!(pick("", 0), None);
    }

    #[test]
    fn multiline_statement_is_taken_whole() {
        let sql = "SELECT a,\n       b\n  FROM t;\nSELECT 2;";
        assert_eq!(pick(sql, 12), Some(0), "跨行语句的中间一行也归它");
        assert_eq!(pick(sql, 27), Some(0), "结束的分号");
        assert_eq!(pick(sql, 29), Some(1));
    }
}
