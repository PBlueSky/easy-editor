//! 用户配置：字号、主题、上次打开的工作目录、数据库连接，
//! 持久化到 `%APPDATA%\easy-editor\config.json`。

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
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
    /// 编辑器字号（`Ctrl+=` 调的就是它）
    pub font_size: f32,
    /// 界面字号：顶栏、侧栏、页签、结果区这些非编辑区文字一起走这个数。
    /// 档位怎么由它铺开，见 `theme` 模块头「字号只有一处来源」。
    pub ui_font_size: f32,
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
            font_size: crate::theme::EDITOR_FONT_DEFAULT,
            ui_font_size: crate::theme::UI_FONT_DEFAULT,
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

    pub fn load() -> Loaded {
        load_from(&Self::path())
    }

    /// 挡住手工改坏的值。配置是这个文件同用户都能编辑的 JSON，改坏了不能把应用带崩。
    fn sanitize(&mut self) {
        self.font_size = self
            .font_size
            .clamp(crate::theme::EDITOR_FONT_MIN, crate::theme::EDITOR_FONT_MAX);
        // 界面字号越界不只是难看：Body 撑到 30 时顶栏那一排会换行、面板会被字撑破
        self.ui_font_size = self
            .ui_font_size
            .clamp(crate::theme::UI_FONT_MIN, crate::theme::UI_FONT_MAX);
        prune_connections(&mut self.db_connections);
    }
}

/// 一次读盘的结果：能用的配置，外加「原文件哪儿不对劲」的交代。
pub struct Loaded {
    pub config: Config,
    pub trouble: Option<Trouble>,
}

/// 配置文件没能照原样用上。
///
/// 有了这个类型才有「先留档、再回退」这一步：以前解析失败直接当没有配置，
/// 接着防抖落盘就把用户手改的连接和快捷键顶掉了 —— 一个错字清空整份配置，
/// 还不留一句话。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trouble {
    /// 内容不是合法配置，原文已经整份搬到 `backup`。
    Malformed { backup: PathBuf, reason: String },
    /// 内容不是合法配置，可连留档都没搬成。这时候再落盘就把原文顶掉了。
    Unparked { file: PathBuf, reason: String },
    /// 文件明明在，内容却读不出来（被占用、权限）。
    Unreadable { file: PathBuf, reason: String },
}

impl Trouble {
    /// 这次读盘之后还能不能安全写回。只有原文已经保住的情况下才允许。
    pub fn blocks_save(&self) -> bool {
        !matches!(self, Self::Malformed { .. })
    }

    /// 该动手去改哪一个文件。留档成功时是留档那份，否则是原地没动的那份。
    pub fn file(&self) -> &Path {
        match self {
            Self::Malformed { backup, .. } => backup,
            Self::Unparked { file, .. } | Self::Unreadable { file, .. } => file,
        }
    }

    /// 弹窗正文，一句一行。路径单独走 [`Self::file`]，别混进来 ——
    /// `C:\Users\...` 这种串没有可折行的空格，混在句子里会把窗口顶穿。
    pub fn lines(&self) -> Vec<String> {
        let (intro, reason) = match self {
            Self::Malformed { .. } => (
                "配置文件没读出来，这次按默认值启动了。原来那份一个字没丢，整搬过去留档了：",
                self.reason(),
            ),
            Self::Unparked { .. } => (
                "配置文件没读出来，这次按默认值启动了。更要紧的是原文没能搬去留档，\
                 所以在你自己把它改对之前，应用不会再往这个文件写东西：",
                self.reason(),
            ),
            Self::Unreadable { .. } => (
                "配置文件在那儿，但读不出内容。这次按默认值启动，也不会往它写东西，\
                 免得把你手改的配置覆盖掉：",
                self.reason(),
            ),
        };
        let mut out = vec![intro.into(), format!("它卡在这儿：{reason}")];
        if matches!(self, Self::Malformed { .. }) {
            out.push("把留档那份改回能用的样子，放回原位再启动就还在。".into());
        }
        out
    }

    fn reason(&self) -> &str {
        match self {
            Self::Malformed { reason, .. }
            | Self::Unparked { reason, .. }
            | Self::Unreadable { reason, .. } => reason,
        }
    }
}

fn load_from(path: &Path) -> Loaded {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        // 文件不在是首次启动，本来就该用默认值，没什么好交代的
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Loaded {
                config: Config::default(),
                trouble: None,
            }
        }
        Err(e) => {
            return Loaded {
                config: Config::default(),
                trouble: Some(Trouble::Unreadable {
                    file: path.to_path_buf(),
                    reason: e.to_string(),
                }),
            }
        }
    };
    // 空文件按「没配置」处理：编辑器和同步盘都留下过 0 字节的 config.json，
    // 那不算用户的配置写坏了，没必要拿留档和弹窗去烦人。
    if text.trim().is_empty() {
        return Loaded {
            config: Config::default(),
            trouble: None,
        };
    }
    match serde_json::from_str::<Config>(&text) {
        Ok(mut cfg) => {
            cfg.sanitize();
            Loaded {
                config: cfg,
                trouble: None,
            }
        }
        Err(e) => {
            let reason = e.to_string();
            match park(path) {
                Some(backup) => Loaded {
                    config: Config::default(),
                    trouble: Some(Trouble::Malformed { backup, reason }),
                },
                None => Loaded {
                    config: Config::default(),
                    trouble: Some(Trouble::Unparked {
                        file: path.to_path_buf(),
                        reason,
                    }),
                },
            }
        }
    }
}

/// 把读不懂的那份挪到一边。目标名占了就加序号，绝不覆盖已有的留档。
fn park(path: &Path) -> Option<PathBuf> {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty())?;
    let name = path.file_name()?.to_string_lossy().to_string();
    let mut target = dir.join(format!("{name}.broken"));
    let mut n = 1;
    while target.exists() {
        target = dir.join(format!("{name}.broken.{n}"));
        n += 1;
    }
    std::fs::rename(path, &target).ok().map(|_| target)
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
    /// 这次启动有没有把配置文件读坏。原文件没保住之前，落盘一直是停的，
    /// 免得用默认值把用户手改的那份顶掉。
    pub trouble: Option<Trouble>,
    dirty: bool,
    last_save: Instant,
}

impl ConfigStore {
    pub fn load() -> Self {
        let Loaded { config, trouble } = Config::load();
        Self {
            data: config,
            trouble,
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
        if self.trouble.as_ref().is_some_and(|t| t.blocks_save()) {
            return;
        }
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
        let mut list = vec![
            named("本地"),
            named(""),
            named("   "),
            named("本地"),
            named("线上"),
        ];
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

    /// 每个用例各占一个临时目录，跑完就清掉。绝不碰 %APPDATA% 里那份真的。
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("easy-editor-config-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_missing_config_is_just_a_first_run() {
        let dir = scratch("missing");
        let loaded = load_from(&dir.join("config.json"));
        assert!(loaded.trouble.is_none());
        assert_eq!(loaded.config.font_size, 14.0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_empty_config_does_not_count_as_damaged() {
        // 同步盘、编辑器都留下过 0 字节的 config.json，那不用留档也不用弹窗
        let dir = scratch("empty");
        let path = dir.join("config.json");
        std::fs::write(&path, "").unwrap();
        let loaded = load_from(&path);
        assert!(loaded.trouble.is_none());
        assert!(!dir.join("config.json.broken").exists(), "空文件不该被搬走");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_malformed_config_is_parked_rather_than_discarded() {
        let dir = scratch("malformed");
        let path = dir.join("config.json");
        // kind 写成了小写：DbKind 认的是 "Mysql"，整份配置因此解不动
        std::fs::write(
            &path,
            r#"{"db_connections":[{"name":"本地","kind":"mysql"}]}"#,
        )
        .unwrap();

        let loaded = load_from(&path);
        // 回退到默认值可以，但原文必须一个字不落地留着
        assert!(loaded.config.db_connections.is_empty());
        let backup = match &loaded.trouble {
            Some(Trouble::Malformed { backup, reason }) => {
                assert!(
                    reason.contains("Mysql"),
                    "serde 的话该带上认的写法：{reason}"
                );
                backup.clone()
            }
            other => panic!("应该是 Malformed，实际 {other:?}"),
        };
        assert!(!path.exists(), "搬走以后原位置得是空的");
        assert_eq!(
            std::fs::read_to_string(&backup).unwrap(),
            r#"{"db_connections":[{"name":"本地","kind":"mysql"}]}"#
        );
        assert!(!blocks_save_of(&loaded));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn parking_keeps_the_earlier_backup() {
        let dir = scratch("park");
        let path = dir.join("config.json");
        std::fs::write(&path, "{ nope").unwrap();
        let first = park(&path).expect("第一次留档");
        assert_eq!(first, dir.join("config.json.broken"));
        std::fs::write(&path, "{ nope again").unwrap();
        let second = park(&path).expect("第二次留档");
        assert_eq!(second, dir.join("config.json.broken.1"));
        // 上一份还在原地：修坏了两次也不该丢第一次
        assert!(std::fs::read_to_string(&first).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn park_needs_a_directory_to_put_the_copy_in() {
        // 没有目录可放，就只能认栽：load_from 走 Unparked，落盘随之停掉
        assert_eq!(park(Path::new("config.json")), None);
        let dir = scratch("park-none");
        assert_eq!(
            park(&dir.join("没有这个文件.json")),
            None,
            "文件都不在，谈不上留档"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn trouble_knows_when_writing_is_safe() {
        let here = PathBuf::from("config.json");
        assert!(!Trouble::Malformed {
            backup: here.join("x"),
            reason: "x".into(),
        }
        .blocks_save());
        assert!(Trouble::Unparked {
            file: here.clone(),
            reason: "x".into(),
        }
        .blocks_save());
        assert!(Trouble::Unreadable {
            file: here,
            reason: "x".into(),
        }
        .blocks_save());
    }

    #[test]
    fn trouble_points_at_the_file_to_fix() {
        // 弹窗里要给出「改哪个文件」，路径不能混进正文那几句里
        let t = Trouble::Malformed {
            backup: PathBuf::from(r"C:\app\config.json.broken"),
            reason: "unknown variant at line 1 column 4".into(),
        };
        assert_eq!(t.file(), Path::new(r"C:\app\config.json.broken"));
        let text = t.lines().join("");
        assert!(
            !text.contains("config.json.broken"),
            "路径单独一行，别混进句子"
        );
        assert!(text.contains("unknown variant"), "serde 的原话要在：{text}");
    }

    fn blocks_save_of(loaded: &Loaded) -> bool {
        loaded.trouble.as_ref().is_some_and(|t| t.blocks_save())
    }
}
