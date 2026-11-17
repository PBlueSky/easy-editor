//! syntect 语法高亮，按行缓存成 egui 可直接消费的着色片段。

use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::Arc;

use eframe::egui::Color32;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Style as SynStyle, Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

/// 一行文本的着色片段：(颜色, 文本)
pub type LineSpans = Vec<(Color32, String)>;

pub const LIGHT_THEME: &str = "InspiredGitHub";
pub const DARK_THEME: &str = "base16-ocean.dark";

/// 高亮结果缓存的条数。
///
/// 只能存一条的话，两块面板同屏时每帧都会「你把我挤出去、我把你挤出去」，
/// 两篇文档都全量重算高亮。留够面板数量 + 一点切换余量即可。
const CACHE_ENTRIES: usize = 12;

pub struct Highlighter {
    syntaxes: SyntaxSet,
    themes: ThemeSet,
    pub theme_name: String,
    pub fg: Color32,
    pub bg: Color32,
    /// 超过该大小不再高亮，避免卡顿
    pub max_bytes: usize,
    /// 最近用过的在前，超capacity时丢掉尾部
    cache: Vec<(u64, Arc<Vec<LineSpans>>)>,
}

impl Highlighter {
    pub fn new(dark: bool) -> Self {
        let mut me = Self {
            syntaxes: SyntaxSet::load_defaults_newlines(),
            themes: ThemeSet::load_defaults(),
            theme_name: if dark { DARK_THEME } else { LIGHT_THEME }.to_owned(),
            fg: Color32::from_rgb(200, 200, 200),
            bg: Color32::from_rgb(30, 30, 30),
            max_bytes: 600 * 1024,
            cache: Vec::new(),
        };
        me.set_theme(if dark { DARK_THEME } else { LIGHT_THEME });
        me
    }

    pub fn set_theme(&mut self, name: &str) {
        if let Some(theme) = self.themes.themes.get(name) {
            self.theme_name = name.to_owned();
            let default_fg = syntect::highlighting::Color {
                r: 220,
                g: 220,
                b: 220,
                a: 255,
            };
            let default_bg = syntect::highlighting::Color {
                r: 30,
                g: 32,
                b: 36,
                a: 255,
            };
            self.fg = to_color(theme.settings.foreground.unwrap_or(default_fg));
            self.bg = to_color(theme.settings.background.unwrap_or(default_bg));
            self.cache.clear();
        }
    }

    pub fn theme(&self) -> &Theme {
        self.themes
            .themes
            .get(&self.theme_name)
            .or_else(|| self.themes.themes.values().next())
            .expect("syntect 内置主题集非空")
    }

    pub fn syntax_for<'a>(&'a self, path: Option<&Path>, text: &str) -> &'a SyntaxReference {
        if let Some(ext) = path.and_then(|p| p.extension()).and_then(|e| e.to_str()) {
            if let Some(s) = self.syntaxes.find_syntax_by_extension(ext) {
                return s;
            }
        }
        if let Some(s) = path.and_then(|p| self.syntaxes.find_syntax_for_file(p).ok().flatten()) {
            return s;
        }
        self.syntaxes
            .find_syntax_by_first_line(text)
            .unwrap_or_else(|| self.syntaxes.find_syntax_plain_text())
    }

    pub fn language_name(&self, path: Option<&Path>, text: &str) -> String {
        self.syntax_for(path, text).name.clone()
    }

    fn key_for(&self, path: Option<&Path>, text: &str) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut hasher);
        self.theme_name.hash(&mut hasher);
        path.map(|p| p.extension().map(|e| e.to_owned()))
            .flatten()
            .hash(&mut hasher);
        hasher.finish()
    }

    /// 返回整篇文本按行切分的着色结果（含末尾空行处理）。
    pub fn highlight(&mut self, path: Option<&Path>, text: &str) -> Arc<Vec<LineSpans>> {
        let key = self.key_for(path, text);
        if let Some(at) = self.cache.iter().position(|(k, _)| *k == key) {
            let hit = self.cache.remove(at).1;
            self.cache.insert(0, (key, hit.clone()));
            return hit;
        }
        let lines = Arc::new(self.compute(path, text));
        self.cache.insert(0, (key, lines.clone()));
        self.cache.truncate(CACHE_ENTRIES);
        lines
    }

    fn compute(&self, path: Option<&Path>, text: &str) -> Vec<LineSpans> {
        if text.len() > self.max_bytes {
            return plain_lines(text, self.fg);
        }
        let syntax = self.syntax_for(path, text);
        let theme = self.theme();
        let mut hl = HighlightLines::new(syntax, theme);
        let mut out: Vec<LineSpans> = Vec::new();

        for line in LinesWithEndings::from(text) {
            match hl.highlight_line(line, &self.syntaxes) {
                Ok(ranges) => {
                    let mut spans: LineSpans = Vec::with_capacity(ranges.len());
                    for (style, piece) in ranges {
                        let piece = piece.trim_end_matches('\n').trim_end_matches('\r');
                        if piece.is_empty() {
                            continue;
                        }
                        spans.push((to_color(style.foreground), piece.to_owned()));
                    }
                    out.push(spans);
                }
                Err(_) => return plain_lines(text, self.fg),
            }
        }

        if text.is_empty() || text.ends_with('\n') {
            out.push(Vec::new());
        }
        out
    }
}

fn plain_lines(text: &str, color: Color32) -> Vec<LineSpans> {
    text.split('\n')
        .map(|line| {
            if line.is_empty() {
                Vec::new()
            } else {
                vec![(color, line.to_owned())]
            }
        })
        .collect()
}

fn to_color(c: syntect::highlighting::Color) -> Color32 {
    Color32::from_rgb(c.r, c.g, c.b)
}

#[allow(dead_code)]
fn style_of(s: SynStyle) -> Color32 {
    to_color(s.foreground)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SQL 块的高亮全靠这条：默认语法包里没有 SQL 的话得自己带 .sublime-syntax。
    #[test]
    fn sql_extension_resolves_to_the_sql_grammar() {
        let hl = Highlighter::new(true);
        let syntax = hl.syntax_for(Some(Path::new("q.sql")), "SELECT 1;");
        assert_ne!(syntax.name, "Plain text", "SQL 不能退回纯文本");
        let scope = syntax.scope.to_string();
        assert!(scope.starts_with("source.sql"), "实际 scope = {scope}");
    }

    #[test]
    fn sql_highlighting_emits_more_than_one_span_per_line() {
        let mut hl = Highlighter::new(true);
        let spans = hl.highlight(Some(Path::new("q.sql")), "SELECT id FROM t WHERE x = 1;\n");
        assert!(
            spans[0].len() > 1,
            "关键字/标识符应该被拆成多个着色片段，实际 {} 段",
            spans[0].len()
        );
    }
}
