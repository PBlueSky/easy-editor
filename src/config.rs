//! 用户配置：字号、主题、上次打开的工作目录，持久化到 `%APPDATA%\easy-editor\config.json`。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// 应用数据目录（配置、截图都放这里）。
pub fn app_data_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("easy-editor")
}

/// 读环境变量，新名字优先，兼容旧的 `CODE_EASY_*`。
pub fn env_var(name: &str) -> Option<String> {
    std::env::var(format!("EASY_EDITOR_{name}"))
        .ok()
        .or_else(|| std::env::var(format!("CODE_EASY_{name}")).ok())
}

pub fn env_flag(name: &str) -> bool {
    env_var(name).is_some_and(|v| !v.is_empty() && v != "0")
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    /// 编辑器字号
    pub font_size: f32,
    /// 是否深色主题
    pub dark: bool,
    /// 上次打开的工作目录
    pub last_workspace: Option<String>,
    /// 侧栏是否展开
    pub show_side: bool,
    /// 文件树是否显示点开头的隐藏条目
    pub show_hidden: bool,
    /// 上次停留的侧栏页签
    pub side_tab: String,
    /// 用户改过的快捷键：动作 id -> 组合字符串。
    /// 只存改过的项，缺的用内置默认值补齐，这样以后加新动作不会破坏老配置。
    pub keymap: BTreeMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            font_size: 14.0,
            dark: false,
            last_workspace: None,
            show_side: true,
            show_hidden: false,
            side_tab: "files".into(),
            keymap: BTreeMap::new(),
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        app_data_dir().join("config.json")
    }

    pub fn load() -> Self {
        let path = Self::path();
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        match serde_json::from_str::<Self>(&text) {
            Ok(mut cfg) => {
                // 挡住手工改坏的值
                cfg.font_size = cfg.font_size.clamp(10.0, 28.0);
                cfg
            }
            Err(_) => Self::default(),
        }
    }
}

/// 带防抖的配置容器：改一下就标记，攒 600ms 再落盘，避免拖动字号时狂写磁盘。
pub struct ConfigStore {
    pub data: Config,
    dirty: bool,
    last_save: Instant,
}

impl ConfigStore {
    pub fn load() -> Self {
        Self {
            data: Config::load(),
            dirty: false,
            last_save: Instant::now(),
        }
    }

    /// 标记配置已变更。
    pub fn touch(&mut self) {
        self.dirty = true;
    }

    pub fn maybe_save(&mut self) {
        if self.dirty && self.last_save.elapsed() > Duration::from_millis(600) {
            self.save_now();
        }
    }

    pub fn save_now(&mut self) {
        let path = Config::path();
        if let Some(dir) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(dir) {
                eprintln!("[config] 创建配置目录失败：{e}");
                return;
            }
        }
        match serde_json::to_string_pretty(&self.data) {
            Ok(text) => match std::fs::write(&path, text) {
                Ok(_) => {
                    self.dirty = false;
                    self.last_save = Instant::now();
                }
                Err(e) => eprintln!("[config] 写入 {} 失败：{e}", path.display()),
            },
            Err(e) => eprintln!("[config] 序列化失败：{e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_sane() {
        let cfg = Config::default();
        assert!((10.0..=28.0).contains(&cfg.font_size));
        assert!(cfg.show_side);
    }

    #[test]
    fn roundtrip_json() {
        let cfg = Config {
            font_size: 17.0,
            dark: true,
            last_workspace: Some(r"D:\Codes\demo".into()),
            ..Default::default()
        };
        let text = serde_json::to_string(&cfg).unwrap();
        let back: Config = serde_json::from_str(&text).unwrap();
        assert_eq!(back.font_size, 17.0);
        assert!(back.dark);
        assert_eq!(back.last_workspace.as_deref(), Some(r"D:\Codes\demo"));
    }

    #[test]
    fn missing_fields_fall_back_to_default() {
        // 老版本配置文件缺少新字段时不能整体失效
        let back: Config = serde_json::from_str(r#"{"font_size": 20.0}"#).unwrap();
        assert_eq!(back.font_size, 20.0);
        assert!(back.show_side);
        assert!(back.last_workspace.is_none());
    }
}
