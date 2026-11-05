//! 可配置快捷键。
//!
//! 每个动作有一个稳定的 id（直接当 `config.json` 里的键名，方便手工改），
//! 一个中文说明、一个分组、一个默认组合。
//!
//! 组合统一表示成 `Ctrl+Alt+Shift+Key` 这种形式，解析和格式化都走 [`parse`] / [`format`]，
//! 保证写进配置的和显示给用户的是同一份字符串。

use std::collections::BTreeMap;

use eframe::egui::{self, Key, Modifiers};

pub struct ActionDef {
    pub id: &'static str,
    pub label: &'static str,
    pub group: &'static str,
    pub default: &'static str,
}

/// 全部可绑定动作。顺序即界面里的显示顺序。
pub const ACTIONS: &[ActionDef] = &[
    // 文件
    ActionDef { id: "open_folder", label: "打开文件夹", group: "文件", default: "Ctrl+O" },
    ActionDef { id: "save", label: "保存", group: "文件", default: "Ctrl+S" },
    ActionDef { id: "save_all", label: "全部保存", group: "文件", default: "Ctrl+Shift+S" },
    ActionDef { id: "quick_open", label: "快速打开文件", group: "文件", default: "Ctrl+P" },
    ActionDef { id: "new_file", label: "新建文件", group: "文件", default: "Ctrl+N" },
    ActionDef { id: "new_folder", label: "新建文件夹", group: "文件", default: "Ctrl+Shift+N" },
    ActionDef { id: "tab_close", label: "关闭当前标签", group: "文件", default: "Ctrl+W" },
    // 导航
    ActionDef { id: "nav_back", label: "光标位置后退", group: "导航", default: "Ctrl+Alt+Left" },
    ActionDef { id: "nav_forward", label: "光标位置前进", group: "导航", default: "Ctrl+Alt+Right" },
    ActionDef { id: "global_search", label: "全局搜索", group: "导航", default: "Ctrl+Shift+F" },
    ActionDef { id: "find", label: "当前文件查找", group: "导航", default: "Ctrl+F" },
    ActionDef { id: "replace", label: "当前文件替换", group: "导航", default: "Ctrl+H" },
    // 日志
    ActionDef { id: "journal_insert", label: "插入日志块", group: "日志", default: "Ctrl+J" },
    ActionDef { id: "journal_prev", label: "上一个日志块", group: "日志", default: "Ctrl+Alt+Up" },
    ActionDef { id: "journal_next", label: "下一个日志块", group: "日志", default: "Ctrl+Alt+Down" },
    ActionDef { id: "journal_toggle", label: "切换完成状态", group: "日志", default: "Ctrl+Alt+D" },
    // 视图
    ActionDef { id: "side_toggle", label: "显示 / 隐藏侧栏", group: "视图", default: "Ctrl+B" },
    ActionDef { id: "terminal_toggle", label: "聚焦终端面板", group: "视图", default: "Ctrl+Backtick" },
    ActionDef { id: "terminal_new", label: "新建终端面板", group: "视图", default: "Ctrl+Shift+Backtick" },
    ActionDef { id: "font_inc", label: "放大字号", group: "视图", default: "Ctrl+Plus" },
    ActionDef { id: "font_dec", label: "缩小字号", group: "视图", default: "Ctrl+Minus" },
    ActionDef { id: "wrap_toggle", label: "切换自动换行", group: "视图", default: "Alt+Z" },
    ActionDef { id: "help", label: "快捷键设置", group: "视图", default: "F1" },
    ActionDef { id: "screenshot", label: "截图", group: "视图", default: "F12" },
    // 分屏：默认键抄 Windows Terminal（Alt+Shift+D 自动拆 / + 左右 / - 上下，Alt+方向键切焦点）
    ActionDef { id: "split_auto", label: "自动拆分面板", group: "分屏", default: "Alt+Shift+D" },
    ActionDef { id: "split_col", label: "向右拆分面板", group: "分屏", default: "Alt+Shift+Plus" },
    ActionDef { id: "split_row", label: "向下拆分面板", group: "分屏", default: "Alt+Shift+Minus" },
    ActionDef { id: "pane_close", label: "关闭当前面板", group: "分屏", default: "Ctrl+Shift+W" },
    ActionDef { id: "pane_next", label: "下一个面板", group: "分屏", default: "Ctrl+Shift+Tab" },
    ActionDef { id: "pane_prev", label: "上一个面板", group: "分屏", default: "Ctrl+Alt+Shift+Tab" },
    ActionDef { id: "focus_pane_left", label: "聚焦左侧面板", group: "分屏", default: "Alt+Left" },
    ActionDef { id: "focus_pane_right", label: "聚焦右侧面板", group: "分屏", default: "Alt+Right" },
    ActionDef { id: "focus_pane_up", label: "聚焦上方面板", group: "分屏", default: "Alt+Up" },
    ActionDef { id: "focus_pane_down", label: "聚焦下方面板", group: "分屏", default: "Alt+Down" },
    // 格式化
    ActionDef { id: "format", label: "格式化", group: "编辑", default: "Alt+Shift+F" },
];

pub fn default_binding(action: &str) -> &'static str {
    ACTIONS
        .iter()
        .find(|a| a.id == action)
        .map(|a| a.default)
        .unwrap_or("")
}

pub fn default_map() -> BTreeMap<String, String> {
    ACTIONS
        .iter()
        .map(|a| (a.id.to_string(), a.default.to_string()))
        .collect()
}

/// 解析 `Ctrl+Alt+Left` 这样的组合。
///
/// 键名用 egui 自己的名字（`Left` / `Enter` / `Backtick` / `F1` / `A` / `0`），
/// 也接受 `Esc`、`Up` 之类的别名。
pub fn parse(text: &str) -> Option<(Modifiers, Key)> {
    let mut mods = Modifiers::NONE;
    let mut key: Option<Key> = None;
    for raw in text.split('+') {
        let part = raw.trim();
        if part.is_empty() {
            continue;
        }
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods |= Modifiers::CTRL,
            "shift" => mods |= Modifiers::SHIFT,
            "alt" | "option" | "opt" => mods |= Modifiers::ALT,
            "cmd" | "command" | "super" | "win" => mods |= Modifiers::COMMAND,
            _ => {
                // 已经取到键名又冒出一个，说明写法有问题
                if key.is_some() {
                    return None;
                }
                key = Some(key_from_name(part)?);
            }
        }
    }
    Some((mods, key?))
}

/// 按键名解析，大小写不敏感（`left` / `Left` 都认）。
fn key_from_name(name: &str) -> Option<Key> {
    if let Some(key) = Key::ALL
        .iter()
        .copied()
        .find(|k| k.name().eq_ignore_ascii_case(name))
    {
        return Some(key);
    }
    // 兜底：egui 自己的别名表（`Esc`、`Return`、`Up` 之类）。
    // 这里要按首字母大写 + 其余小写再试一次，否则 `ESC` 对不上 `Esc`。
    Key::from_name(name).or_else(|| {
        let mut chars = name.chars();
        let first = chars.next()?;
        let titled = first.to_uppercase().collect::<String>() + &chars.as_str().to_ascii_lowercase();
        Key::from_name(&titled)
    })
}

/// 组合的规范写法：`Ctrl+Alt+Shift+Key`。
pub fn format(mods: Modifiers, key: Key) -> String {
    let mut s = String::new();
    if mods.ctrl || mods.command {
        s.push_str("Ctrl+");
    }
    if mods.alt {
        s.push_str("Alt+");
    }
    if mods.shift {
        s.push_str("Shift+");
    }
    s.push_str(key.name());
    s
}

/// 把任意写法归一化成规范写法；解析不了就原样返回（保留用户写错的字符串便于排查）。
pub fn normalize(text: &str) -> String {
    match parse(text) {
        Some((mods, key)) => format(mods, key),
        None => text.trim().to_string(),
    }
}

/// 从这一帧的输入里抓一个可以绑定的组合（录制快捷键时用）。
pub fn capture(ctx: &egui::Context) -> Option<String> {
    let mut out = None;
    ctx.input(|i| {
        for event in &i.events {
            if let egui::Event::Key {
                key, pressed: true, modifiers, ..
            } = event
            {
                out = Some(format(*modifiers, *key));
                break;
            }
        }
    });
    out
}

#[derive(Clone, Debug)]
pub struct Keymap {
    map: BTreeMap<String, String>,
}

impl Default for Keymap {
    fn default() -> Self {
        Self { map: default_map() }
    }
}

impl Keymap {
    /// 用配置里的覆盖默认值；配置里出现的未知动作名直接忽略。
    pub fn from_config(saved: &BTreeMap<String, String>) -> Self {
        let mut map = default_map();
        for (id, binding) in saved {
            if ACTIONS.iter().any(|a| a.id == id) {
                map.insert(id.clone(), normalize(binding));
            }
        }
        Self { map }
    }

    pub fn to_config(&self) -> BTreeMap<String, String> {
        self.map.clone()
    }

    pub fn get(&self, action: &str) -> &str {
        self.map.get(action).map(|s| s.as_str()).unwrap_or("")
    }

    /// 设置绑定。空串表示解绑。
    pub fn set(&mut self, action: &str, binding: &str) {
        let binding = if binding.trim().is_empty() {
            String::new()
        } else {
            normalize(binding)
        };
        self.map.insert(action.to_string(), binding);
    }

    pub fn reset(&mut self, action: &str) {
        self.map
            .insert(action.to_string(), default_binding(action).to_string());
    }

    pub fn reset_all(&mut self) {
        self.map = default_map();
    }

    pub fn is_default(&self, action: &str) -> bool {
        self.get(action) == default_binding(action)
    }

    /// 这个组合是否已经被**别的**动作占用，返回占用者的说明文字。
    pub fn owner_of(&self, action: &str, binding: &str) -> Option<&'static str> {
        let binding = normalize(binding);
        if binding.is_empty() {
            return None;
        }
        ACTIONS.iter().find_map(|a| {
            if a.id == action {
                return None;
            }
            let other = normalize(self.get(a.id));
            if !other.is_empty() && other == binding {
                Some(a.label)
            } else {
                None
            }
        })
    }

    /// 当前绑定是否和别的动作撞了。
    pub fn conflict_of(&self, action: &str) -> Option<&'static str> {
        self.owner_of(action, self.get(action))
    }

    /// 这一帧是否按下了某个动作的快捷键；按下就把它从事件队列里摘掉，避免编辑器重复消费。
    ///
    /// 归属由 [`Self::owns`] 决定，不靠调用顺序：egui 的宽松匹配会让 `Ctrl+W`
    /// 把 `Ctrl+Shift+W` 那一下抢先吃掉，`pane_close` 就永远轮不到了。
    pub fn consume(&self, ctx: &egui::Context, action: &str) -> bool {
        let binding = self.get(action);
        if binding.is_empty() {
            return false;
        }
        let Some((mods, key)) = parse(binding) else {
            return false;
        };
        if self.take(ctx, action, mods, key) {
            return true;
        }
        // 键盘上 `Ctrl+=` 和 `Ctrl++` 是同一个物理键，两种写法都认
        let twin = match key {
            Key::Plus => Key::Equals,
            Key::Equals => Key::Plus,
            _ => return false,
        };
        self.take(ctx, action, mods, twin)
    }

    fn take(&self, ctx: &egui::Context, action: &str, mods: Modifiers, key: Key) -> bool {
        let owned = ctx.input(|i| {
            i.events.iter().any(|event| {
                matches!(event, egui::Event::Key { key: k, pressed: true, modifiers, .. }
                    if *k == key && self.owns(action, key, *modifiers))
            })
        });
        owned && ctx.input_mut(|i| i.consume_key(mods, key))
    }

    /// 一个键上可能挂着多个动作（`Ctrl+W` 关标签 / `Ctrl+Shift+W` 关面板）。
    /// 在所有能宽松匹配上这次按键的绑定里，挑修饰键最吻合的那个：
    /// 完全吻合的优先，其次才轮到多按了 Shift/Alt 的宽松吻合。
    fn owns(&self, action: &str, key: Key, pressed: Modifiers) -> bool {
        let mut best: Option<&str> = None;
        let mut best_exact = false;
        for (id, binding) in &self.map {
            let Some((mods, k)) = parse(binding) else {
                continue;
            };
            if k != key || !pressed.matches_logically(mods) {
                continue;
            }
            let exact = pressed.matches_exact(mods);
            if best.is_none() || (exact && !best_exact) {
                best = Some(id);
                best_exact = exact;
            }
        }
        best.is_some_and(|owner| owner == action)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_formats_roundtrip() {
        for def in ACTIONS {
            let (mods, key) = parse(def.default).unwrap_or_else(|| panic!("{} 解析失败", def.default));
            assert_eq!(format(mods, key), def.default, "{} 归一化不一致", def.default);
        }
    }

    #[test]
    fn accepts_aliases_and_lowercase() {
        assert_eq!(normalize("ctrl+alt+left"), "Ctrl+Alt+Left");
        assert_eq!(normalize("CTRL+SHIFT+ESC"), "Ctrl+Shift+Escape");
        assert_eq!(normalize("shift+ctrl+s"), "Ctrl+Shift+S");
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse("Ctrl+").is_none());
        assert!(parse("Ctrl+NoSuchKey").is_none());
        assert!(parse("Ctrl+A+B").is_none());
    }

    #[test]
    fn detects_conflicts() {
        let mut km = Keymap::default();
        assert!(km.conflict_of("nav_back").is_none());
        km.set("nav_back", "Ctrl+F");
        assert_eq!(km.conflict_of("nav_back"), Some("当前文件查找"));
    }

    #[test]
    fn config_only_overrides_known_actions() {
        let mut saved = BTreeMap::new();
        saved.insert("save".to_string(), "Ctrl+K".to_string());
        saved.insert("no_such_action".to_string(), "Ctrl+Q".to_string());
        let km = Keymap::from_config(&saved);
        assert_eq!(km.get("save"), "Ctrl+K");
        assert_eq!(km.get("open_folder"), "Ctrl+O");
    }

    #[test]
    fn unbound_action_never_fires() {
        let mut km = Keymap::default();
        km.set("find", "");
        assert_eq!(km.get("find"), "");
    }

    /// egui 的 `consume_key` 会忽略多按的 Shift/Alt，所以同一个键上挂着两个动作时
    /// 必须挑出修饰键最吻合的那个，否则 `Ctrl+Shift+W` 会被 `Ctrl+W` 抢走。
    #[test]
    fn most_specific_binding_wins() {
        let km = Keymap::default();
        let ctrl = Modifiers::CTRL;
        let ctrl_shift = Modifiers::CTRL | Modifiers::SHIFT;
        let ctrl_alt_shift = Modifiers::CTRL | Modifiers::ALT | Modifiers::SHIFT;

        assert!(km.owns("pane_close", Key::W, ctrl_shift));
        assert!(!km.owns("tab_close", Key::W, ctrl_shift));
        // 没按 Shift 时不该反过来
        assert!(km.owns("tab_close", Key::W, ctrl));
        assert!(!km.owns("pane_close", Key::W, ctrl));

        assert!(km.owns("save_all", Key::S, ctrl_shift));
        assert!(!km.owns("save", Key::S, ctrl_shift));
        assert!(km.owns("new_folder", Key::N, ctrl_shift));
        assert!(km.owns("terminal_new", Key::Backtick, ctrl_shift));
        assert!(km.owns("pane_prev", Key::Tab, ctrl_alt_shift));
        assert!(!km.owns("pane_next", Key::Tab, ctrl_alt_shift));
        assert!(km.owns("pane_next", Key::Tab, ctrl_shift));
        assert!(km.owns("global_search", Key::F, ctrl_shift));
        assert!(!km.owns("find", Key::F, ctrl_shift));
    }
}
