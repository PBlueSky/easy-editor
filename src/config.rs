//! 用户配置：字号、主题、上次打开的工作目录、数据库连接，
//! 持久化到 `%APPDATA%\easy-editor\config.json`。

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::db::ConnectionSpec;

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
    /// 新建终端默认用的 shell 种类 id（`pwsh` / `powershell` / `cmd` / `gitbash` / `wsl`）。
    /// 空串 = 自动挑本机第一个可用的。
    pub terminal_shell: String,
    /// 终端程序路径覆盖：种类 id -> 绝对路径。
    /// 装在非标准目录（比如 `D:\Softwares\GitBash`）时靠这个指过去。
    pub terminal_programs: BTreeMap<String, String>,
    /// 终端启动参数覆盖：种类 id -> 参数字符串（空格分隔，带空格的用双引号包住）。
    pub terminal_args: BTreeMap<String, String>,
    /// 编辑器自动换行：长行折到面板宽度，而不是横向滚出去。
    pub word_wrap: bool,
    /// 启动时把自己登记进 Windows 的 `.txt`「打开方式」/「默认应用」列表。
    /// 只写当前用户的 `HKCU\Software\Classes`，不动任何文件类型的默认处理器；
    /// 不想让这个 exe 出现在列表里就改成 false。
    pub txt_association: bool,
    /// 数据库连接列表。`name` 是 SQL 块头部 `conn=` 引用到的键。
    pub db_connections: Vec<ConnectionSpec>,
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
            terminal_shell: String::new(),
            terminal_programs: BTreeMap::new(),
            terminal_args: BTreeMap::new(),
            word_wrap: false,
            txt_association: true,
            db_connections: Vec::new(),
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
                cfg.sanitize();
                cfg
            }
            Err(_) => Self::default(),
        }
    }

    /// 挡住手工改坏的值。配置是这个文件同用户都能编辑的 JSON，改坏了不能把应用带崩。
    fn sanitize(&mut self) {
        self.font_size = self.font_size.clamp(10.0, 28.0);
        prune_connections(&mut self.db_connections);
    }
}

/// 连接列表进界面前的两道关：没名字的不收，重名的只留第一条。
///
/// 侧栏的树和后台线程都拿名字当键。两条同名连接会让 A 的结果画到 B 的面板上，
/// 而「看着对、其实是另一个库的结果」比直接报错危险得多。
fn prune_connections(list: &mut Vec<ConnectionSpec>) {
    let mut seen: HashSet<String> = HashSet::new();
    list.retain(|c| !c.name.trim().is_empty() && seen.insert(c.name.clone()));
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
        assert!(back.db_connections.is_empty());
    }

    #[test]
    fn connection_fields_are_optional_too() {
        // 手改 JSON 时只写想改的几项，其余按默认补齐
        let back: Config =
            serde_json::from_str(r#"{"db_connections":[{"name":"本地","host":"10.0.0.5"}]}"#)
                .unwrap();
        assert_eq!(back.db_connections.len(), 1);
        assert_eq!(back.db_connections[0].host, "10.0.0.5");
        assert_eq!(back.db_connections[0].port, 3306);
        assert_eq!(back.db_connections[0].user, "root");
    }

    #[test]
    fn prune_drops_nameless_and_duplicate_connections() {
        let named = |name: &str| ConnectionSpec {
            name: name.into(),
            ..Default::default()
        };
        let mut list = vec![named("本地"), named(""), named("   "), named("本地"), named("线上")];
        prune_connections(&mut list);
        let names: Vec<&str> = list.iter().map(|c| c.name.as_str()).collect();
        // 重名只留第一条：留哪条都行，但绝不能留两条让结果串台
        assert_eq!(names, ["本地", "线上"]);
    }

    #[test]
    fn sanitize_clamps_a_hand_edited_font_size() {
        let mut cfg = Config {
            font_size: 99.0,
            db_connections: vec![ConnectionSpec {
                name: String::new(),
                ..Default::default()
            }],
            ..Default::default()
        };
        cfg.sanitize();
        assert_eq!(cfg.font_size, 28.0);
        assert!(cfg.db_connections.is_empty());
    }
}
