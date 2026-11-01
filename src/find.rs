//! 当前文件内的查找与替换。
//!
//! 匹配位置全部用**字节偏移**表示，和编辑器内部的跳转、替换保持一致。
//! 非正则搜索走字节级扫描（不做 `to_lowercase` 分配），大文件也不会卡。

use regex::RegexBuilder;

pub struct FindState {
    pub open: bool,
    pub show_replace: bool,
    pub query: String,
    pub replace: String,
    pub case_sensitive: bool,
    pub regex: bool,
    /// 匹配的字节范围，升序
    pub matches: Vec<(usize, usize)>,
    /// 当前匹配序号
    pub current: usize,
    pub error: Option<String>,
    /// 查询条件变化，需要重新搜索
    pub stale: bool,
    /// 请求把焦点放到查找框
    pub focus_query: bool,
    /// 请求把焦点放到替换框
    pub focus_replace: bool,
}

impl Default for FindState {
    fn default() -> Self {
        Self {
            open: false,
            show_replace: false,
            query: String::new(),
            replace: String::new(),
            case_sensitive: false,
            regex: false,
            matches: Vec::new(),
            current: 0,
            error: None,
            stale: false,
            focus_query: false,
            focus_replace: false,
        }
    }
}

impl FindState {
    pub fn open_find(&mut self, with_replace: bool) {
        self.open = true;
        if with_replace {
            self.show_replace = true;
            self.focus_replace = true;
        } else {
            self.focus_query = true;
        }
        self.stale = true;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.matches.clear();
        self.current = 0;
        self.error = None;
    }

    /// 重新计算匹配。返回匹配数量。
    pub fn recompute(&mut self, text: &str) -> usize {
        self.stale = false;
        self.error = None;
        self.matches.clear();
        self.current = 0;

        if self.query.is_empty() {
            return 0;
        }

        if self.regex {
            let built = RegexBuilder::new(&self.query)
                .case_insensitive(!self.case_sensitive)
                .multi_line(true)
                .build();
            match built {
                Ok(re) => {
                    for m in re.find_iter(text) {
                        if m.start() == m.end() {
                            continue;
                        }
                        self.matches.push((m.start(), m.end()));
                        if self.matches.len() >= 200_000 {
                            break;
                        }
                    }
                }
                Err(e) => {
                    self.error = Some(format!("正则错误：{e}"));
                    return 0;
                }
            }
        } else {
            let needle = self.query.as_bytes();
            let starts = if self.case_sensitive {
                find_all_exact(text.as_bytes(), needle)
            } else {
                find_all_ci(text.as_bytes(), needle)
            };
            self.matches = starts
                .into_iter()
                .map(|s| (s, s + needle.len()))
                .collect();
        }

        self.matches.len()
    }

    pub fn count(&self) -> usize {
        self.matches.len()
    }

    /// 下一个匹配（循环）。
    pub fn next_match(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        self.current = (self.current + 1) % self.matches.len();
    }

    /// 上一个匹配（循环）。
    pub fn prev_match(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        self.current = if self.current == 0 {
            self.matches.len() - 1
        } else {
            self.current - 1
        };
    }

    /// 从某个字节位置开始，选择它之后（含）的第一个匹配。
    pub fn select_from(&mut self, byte: usize) {
        if self.matches.is_empty() {
            return;
        }
        match self.matches.binary_search_by(|m| m.0.cmp(&byte)) {
            Ok(i) => self.current = i,
            Err(i) => self.current = i.min(self.matches.len() - 1),
        }
    }

    pub fn current_range(&self) -> Option<(usize, usize)> {
        self.matches.get(self.current).copied()
    }

    /// 替换当前匹配，返回替换后的光标位置。
    pub fn replace_current(&mut self, text: &mut String) -> Option<usize> {
        let (a, b) = self.current_range()?;
        let replacement = self.expand(&text[a..b]);
        text.replace_range(a..b, &replacement);
        let caret = a + replacement.len();
        // 替换后偏移变化，重新搜索并把当前项定位到替换位置之后
        self.recompute(text);
        self.select_from(caret);
        Some(caret)
    }

    /// 全部替换，返回替换次数。
    pub fn replace_all(&mut self, text: &mut String) -> usize {
        if self.matches.is_empty() {
            return 0;
        }
        let count = self.matches.len();
        if self.regex {
            if let Ok(re) = RegexBuilder::new(&self.query)
                .case_insensitive(!self.case_sensitive)
                .multi_line(true)
                .build()
            {
                let replaced = re.replace_all(text, self.replace.as_str()).into_owned();
                *text = replaced;
                self.recompute(text);
                return count;
            }
            return 0;
        }
        // 非正则：从后往前替换，避免前面的偏移失效
        for (a, b) in self.matches.iter().rev() {
            text.replace_range(*a..*b, &self.replace);
        }
        self.recompute(text);
        count
    }

    /// 把替换串里的 `$1` 展开（仅正则模式有意义）。
    fn expand(&self, matched: &str) -> String {
        if !self.regex || !self.replace.contains('$') {
            return self.replace.clone();
        }
        if let Ok(re) = RegexBuilder::new(&self.query)
            .case_insensitive(!self.case_sensitive)
            .multi_line(true)
            .build()
        {
            if let Some(caps) = re.captures(matched) {
                let mut out = String::new();
                caps.expand(&self.replace, &mut out);
                return out;
            }
        }
        self.replace.clone()
    }

    /// 状态文本：`3/17` 或错误信息。
    pub fn status_text(&self) -> String {
        if let Some(err) = &self.error {
            return err.clone();
        }
        if self.query.is_empty() {
            return String::new();
        }
        if self.matches.is_empty() {
            return "无匹配".into();
        }
        format!("{}/{}", self.current + 1, self.matches.len())
    }
}

/// 精确字节查找。
fn find_all_exact(hay: &[u8], needle: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    if needle.is_empty() || needle.len() > hay.len() {
        return out;
    }
    let mut i = 0;
    while i + needle.len() <= hay.len() {
        match memchr::memmem::find(&hay[i..], needle) {
            Some(off) => {
                let at = i + off;
                out.push(at);
                i = at + 1;
            }
            None => break,
        }
    }
    out
}

/// ASCII 忽略大小写、非 ASCII 精确匹配的字节查找。
///
/// 对中文等 UTF-8 多字节字符按字节精确比较，不会出现拆字问题。
fn find_all_ci(hay: &[u8], needle: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    if needle.is_empty() || needle.len() > hay.len() {
        return out;
    }
    let first = needle[0];
    let mut i = 0;
    let last = hay.len() - needle.len();
    while i <= last {
        // 先用首字节快速筛
        let Some(off) = hay[i..=last].iter().position(|b| eq_ci(*b, first)) else {
            break;
        };
        let start = i + off;
        if hay[start..start + needle.len()]
            .iter()
            .zip(needle)
            .all(|(a, b)| eq_ci(*a, *b))
        {
            out.push(start);
            i = start + 1;
        } else {
            i = start + 1;
        }
    }
    out
}

#[inline]
fn eq_ci(a: u8, b: u8) -> bool {
    a == b || a.to_ascii_lowercase() == b.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(text: &str, q: &str, case: bool, regex: bool) -> FindState {
        let mut s = FindState {
            query: q.into(),
            case_sensitive: case,
            regex,
            ..Default::default()
        };
        s.recompute(text);
        s
    }

    #[test]
    fn finds_ascii_case_insensitively() {
        let s = find("Hello hello HELLO", "hello", false, false);
        assert_eq!(s.count(), 3);
    }

    #[test]
    fn respects_case_sensitivity() {
        let s = find("Hello hello", "hello", true, false);
        assert_eq!(s.count(), 1);
    }

    #[test]
    fn finds_chinese() {
        let s = find("工作日志1\n工作日志2", "工作日志", false, false);
        assert_eq!(s.count(), 2);
        // 字节范围必须落在字符边界
        for (a, b) in &s.matches {
            assert!("工作日志1\n工作日志2".is_char_boundary(*a));
            assert!("工作日志1\n工作日志2".is_char_boundary(*b));
        }
    }

    #[test]
    fn regex_mode_works() {
        let s = find("abc123def456", r"\d+", false, true);
        assert_eq!(s.count(), 2);
    }

    #[test]
    fn replace_all_replaces_everything() {
        let mut text = String::from("foo bar foo");
        let mut s = find(&text, "foo", false, false);
        s.replace = "baz".into();
        assert_eq!(s.replace_all(&mut text), 2);
        assert_eq!(text, "baz bar baz");
    }

    #[test]
    fn replace_current_keeps_offsets_valid() {
        let mut text = String::from("aa aa aa");
        let mut s = find(&text, "aa", false, false);
        s.replace = "bbbb".into();
        s.replace_current(&mut text);
        assert_eq!(text, "bbbb aa aa");
        assert_eq!(s.count(), 2);
    }
}
