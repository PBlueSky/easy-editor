//! `.txt` 文件关联：把本程序登记进 Windows 的「打开方式」和「默认应用」。
//!
//! 登记的东西都在 `HKEY_CURRENT_USER` 下面，不需要管理员，也只动当前用户自己的
//! 设置。刻意**不**写 `UserChoice`：那份值由系统哈希保护，第三方程序伪造它是
//! 越权行为，而且哈希一旦不对，Windows 会把整个关联判废。所以这条路的终点是
//! 「右键 .txt → 打开方式 → 选择其他应用 → EasyEditor（可勾始终）」，或者在
//! 设置 → 应用 → 默认应用 里挑一次。
//!
//! 写在 `HKCU\Software\Classes` 而不是 `HKLM`，还有一层：安装目录可能在
//! `Program Files` 里，那里的 Classes 是只读的。

use std::path::Path;

/// `shell\open\command` 里那个「把文件路径递进来」的占位符，Windows 会替掉引号外的部分。
pub const FILE_ARG: &str = "\"%1\"";

/// 我们自己的 ProgID。`Software\Classes\<ProgID>` 是它的主键。
pub const PROGID: &str = "EasyEditor.txt";

/// 「注册的应用」里那个显示名，和 `Software\Clients\TextEditor\` 下的键名一致。
pub const CLIENT: &str = "EasyEditor";

/// 要登记的文件类型。目前只做 `.txt`：别的扩展名各有自己的默认程序在位，
/// 一次只碰一种，出问题也好退回。
pub const EXTENSIONS: &[&str] = &[".txt"];

/// 一条待写的注册表值。`name` 为空串 = 写那个键的默认值。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Str(String),
    Empty,
}

/// 一条待写的注册表值，连它属于哪个键。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// 相对 `HKEY_CURRENT_USER` 的键路径，反斜杠分隔，结尾不带斜杠。
    pub key: String,
    pub name: String,
    pub value: Value,
}

impl Entry {
    fn str(key: &str, name: &str, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            name: name.into(),
            value: Value::Str(value.into()),
        }
    }

    fn empty(key: &str, name: &str) -> Self {
        Self {
            key: key.into(),
            name: name.into(),
            value: Value::Empty,
        }
    }
}

/// 「打开方式」列表里显示的那句话。
fn description() -> String {
    "用 EasyEditor 编辑".into()
}

/// `"<exe>" "%1"` —— 路径带空格必须整段引起来，`C:\Program Files\` 是最常见的坑。
pub fn open_command(exe: &Path) -> String {
    format!("\"{}\" {FILE_ARG}", exe.display())
}

/// 图标取 exe 的第 0 号资源（`build.rs` 用 winresource 把 `.ico` 编进 PE 的那份）。
fn icon(exe: &Path) -> String {
    format!("{},0", exe.display())
}

/// 要写的全部值。纯函数，好处是这台机器上不用真动注册表也能把这份计划审一遍。
pub fn entries(exe: &Path) -> Vec<Entry> {
    let progid = format!(r"Software\Classes\{PROGID}");
    let command = open_command(exe);
    let capabilities = format!(r"Software\Clients\TextEditor\{CLIENT}\Capabilities");
    let mut out = vec![
        Entry::str(&progid, "", description()),
        Entry::str(&format!(r"{progid}\DefaultIcon"), "", icon(exe)),
        Entry::str(&format!(r"{progid}\shell\open\command"), "", &command),
        // 右键「打开方式」的候选名单：只往列表里加一项，不改任何类型的默认值
        Entry::empty(r"Software\Classes\.txt\OpenWithProgids", PROGID),
        // 同一个 exe 也按「应用程序」登记一份，`打开方式` 里那些没 ProgID 的
        // 未知类型才认得到我们（路径按 exe 文件名，和 ProgID 那条互不替代）
        Entry::str(
            &format!(
                r"Software\Classes\Applications\{}\shell\open\command",
                exe_name(exe)
            ),
            "",
            &command,
        ),
        Entry::str(
            &format!(r"Software\Classes\Applications\{}", exe_name(exe)),
            "FriendlyAppName",
            CLIENT,
        ),
        // 下面这一组才是「设置 → 应用 → 默认应用」里能看到我们的原因
        Entry::str(
            &format!(r"Software\Clients\TextEditor\{CLIENT}\DefaultIcon"),
            "",
            icon(exe),
        ),
        Entry::str(&capabilities, "ApplicationName", CLIENT),
        Entry::str(
            &capabilities,
            "ApplicationDescription",
            "EasyEditor：Rust + egui 写的文本编辑器",
        ),
        Entry::str(&capabilities, "FileExtensions", EXTENSIONS.join(" ")),
        Entry::str(r"Software\RegisteredApplications", CLIENT, capabilities),
    ];
    for ext in EXTENSIONS.iter().skip(1) {
        out.push(Entry::empty(
            &format!(r"Software\Classes\{ext}\OpenWithProgids"),
            PROGID,
        ));
    }
    out
}

/// 注册表按文件名认应用程序，路径里那些 `target\debug` 不该跟着进去。
fn exe_name(exe: &Path) -> String {
    exe.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "easy-editor.exe".into())
}

/// 一次登记写了些什么。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Written {
    /// 写成功的值条数
    pub values: usize,
    /// 其中属于**新建**键的条数。0 表示这台机器早就登记过了，
    /// 那就没必要再朝 shell 广播一次「关联变了」。
    pub fresh: usize,
}

/// 登记一次。第一条失败就把 Windows 的话带回去。
#[cfg(windows)]
pub fn register(exe: &Path) -> Result<Written, String> {
    let written = win::write_all(&entries(exe))?;
    if written.fresh > 0 {
        // 不喊这一声，「打开方式」要等 explorer 自己缓过来才看得到新程序，
        // 通常就是注销重登一次。喊完右键菜单立刻有。只在真建新键时喊。
        win::announce_association_changed();
    }
    Ok(written)
}

#[cfg(not(windows))]
pub fn register(_exe: &Path) -> Result<Written, String> {
    // 非 Windows 没有这套注册表，静默跳过：调用点在 `app.rs` 里不加分支。
    Ok(Written::default())
}

#[cfg(windows)]
mod win {
    use super::{Entry, Written};
    use std::ffi::c_void;
    use std::ptr::null_mut;

    type HKey = *mut c_void;

    #[link(name = "advapi32")]
    extern "system" {
        fn RegCreateKeyExW(
            key: HKey,
            sub_key: *const u16,
            reserved: u32,
            class: *mut u16,
            options: u32,
            sam_desired: u32,
            security: *mut c_void,
            result: *mut HKey,
            disposition: *mut u32,
        ) -> u32;
        fn RegSetValueExW(
            key: HKey,
            name: *const u16,
            reserved: u32,
            kind: u32,
            bytes: *const u8,
            len: u32,
        ) -> u32;
        fn RegCloseKey(key: HKey) -> u32;
    }

    #[link(name = "shell32")]
    extern "system" {
        fn SHChangeNotify(event: i32, flags: u32, item1: *const c_void, item2: *const c_void);
    }

    const HKEY_CURRENT_USER: HKey = 0x8000_0001usize as HKey;
    const KEY_SET_VALUE: u32 = 0x0002;
    /// `RegCreateKeyExW` 会连带建出路径上缺的中间键，所以除了写值本身，还得要建子键的权限。
    const KEY_CREATE_SUB_KEY: u32 = 0x0004;
    /// 我们是 64 位程序，但 `Software\Classes` 在 32/64 位之间会重定向；显式走 64 位那一份，
    /// 免得登记到了 WOW 视图里、Windows 的「打开方式」看不见。
    const KEY_WOW64_64KEY: u32 = 0x0100;
    const REG_SZ: u32 = 1;
    const ERROR_SUCCESS: u32 = 0;
    /// `RegCreateKeyExW` 的 disposition：这个键是这次新建的，不是原来就有。
    const REG_CREATED_NEW_KEY: u32 = 0x0000_0001;
    const SHCNE_ASSOCCHANGED: i32 = 0x0800_0000;
    /// 等到 shell 处理完再返回，省得用户紧接着右键就看不见新条目。
    const SHCNF_FLUSH: u32 = 0x1000;

    /// 注册表的宽字符串按 UTF-16 带结尾 NUL；结尾那个 NUL 也得算进长度里。
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn describe(code: u32) -> String {
        match code {
            2 => "键不存在".to_string(),
            5 => "当前用户写不了这个键（ERROR_ACCESS_DENIED）".to_string(),
            other => format!("错误码 {other}"),
        }
    }

    /// 写一条，顺带回报这个键是不是这次新建的。
    fn write(entry: &Entry) -> Result<bool, String> {
        // 键路径、值名、数据全在这函数里活着，指针不会飘到下一帧去
        let sub_key = wide(&entry.key);
        let name = wide(&entry.name);
        let body: Vec<u16> = match &entry.value {
            super::Value::Str(s) => wide(s),
            super::Value::Empty => vec![0],
        };
        let mut key: HKey = null_mut();
        let mut disposition = 0u32;
        // SAFETY: 上面四个缓冲都活到本函数结束，`RegCloseKey` 无条件执行，句柄不漏。
        unsafe {
            let code = RegCreateKeyExW(
                HKEY_CURRENT_USER,
                sub_key.as_ptr(),
                0,
                null_mut(),
                0,
                KEY_SET_VALUE | KEY_CREATE_SUB_KEY | KEY_WOW64_64KEY,
                null_mut(),
                &mut key,
                &mut disposition,
            );
            if code != ERROR_SUCCESS {
                return Err(format!("打开 {} 失败：{}", entry.key, describe(code)));
            }
            let code = RegSetValueExW(
                key,
                name.as_ptr(),
                0,
                REG_SZ,
                body.as_ptr() as *const u8,
                (body.len() * std::mem::size_of::<u16>()) as u32,
            );
            RegCloseKey(key);
            if code != ERROR_SUCCESS {
                return Err(format!(
                    "写 {}\\{} 失败：{}",
                    entry.key,
                    if entry.name.is_empty() {
                        "(默认)"
                    } else {
                        &entry.name
                    },
                    describe(code)
                ));
            }
        }
        Ok(disposition & REG_CREATED_NEW_KEY != 0)
    }

    pub fn write_all(entries: &[Entry]) -> Result<Written, String> {
        let mut out = Written::default();
        for entry in entries {
            if write(entry)? {
                out.fresh += 1;
            }
            out.values += 1;
        }
        Ok(out)
    }

    /// 通知 shell「关联表变了」，让「打开方式」当场生效。
    pub fn announce_association_changed() {
        // SAFETY: `SHCNE_ASSOCCHANGED` 不读那两个 item 参数，传空指针是文档规定的用法。
        unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_FLUSH, null_mut(), null_mut()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    fn exe() -> std::path::PathBuf {
        std::path::PathBuf::from(r"C:\Program Files\EasyEditor\easy-editor.exe")
    }

    #[cfg(not(windows))]
    fn exe() -> std::path::PathBuf {
        std::path::PathBuf::from("/opt/easy-editor/easy-editor.exe")
    }

    fn find(key: &str, name: &str) -> Entry {
        entries(&exe())
            .into_iter()
            .find(|e| e.key == key && e.name == name)
            .unwrap_or_else(|| panic!("计划里没有 {key} 的 {name:?}"))
    }

    /// 路径带空格是这台机器上的常态：不整段引起来，`C:\Program Files\` 会被
    /// 当成两个参数，Windows 拿 "%1" 去开 `C:\Program`。
    #[test]
    fn the_open_command_quotes_a_path_with_spaces() {
        let Entry { value, .. } = find(r"Software\Classes\EasyEditor.txt\shell\open\command", "");
        assert_eq!(
            value,
            Value::Str(r#""C:\Program Files\EasyEditor\easy-editor.exe" "%1""#.into())
        );
    }

    /// 「打开方式」的候选名单是 `.txt\OpenWithProgids` 下面的一项。空值：这个键
    /// 只用来列 progid，值本身 Windows 不看。
    #[test]
    fn txt_lists_us_as_an_open_with_candidate() {
        let Entry { value, .. } = find(r"Software\Classes\.txt\OpenWithProgids", PROGID);
        assert_eq!(value, Value::Empty);
    }

    /// 「设置 → 默认应用」只认 `RegisteredApplications` 指过去的那份
    /// `Software\Clients\TextEditor\<名字>\Capabilities`，两个字符串必须严丝合缝。
    #[test]
    fn the_registered_application_points_at_the_capabilities_it_just_filled() {
        let Entry { value, .. } = find(r"Software\RegisteredApplications", CLIENT);
        let Value::Str(target) = value else {
            panic!("RegisteredApplications 得指一个路径")
        };
        assert_eq!(
            target,
            format!(r"Software\Clients\TextEditor\{CLIENT}\Capabilities")
        );
        // 指过去的地方确实被我们填过，否则设置里是一个空条目
        find(&target, "ApplicationName");
        find(&target, "FileExtensions");
    }

    /// 只登记 `.txt`。多一个扩展名就多抢一类文件的「打开方式」，那是用户没要的。
    #[test]
    fn only_txt_is_offered() {
        let Entry { value, .. } = find(
            r"Software\Clients\TextEditor\EasyEditor\Capabilities",
            "FileExtensions",
        );
        assert_eq!(value, Value::Str(".txt".into()));
        let progids = entries(&exe())
            .into_iter()
            .filter(|e| e.key.ends_with(r"\OpenWithProgids"))
            .map(|e| e.key)
            .collect::<Vec<_>>();
        assert_eq!(
            progids,
            vec![r"Software\Classes\.txt\OpenWithProgids".to_string()]
        );
    }

    /// 默认处理器（`UserChoice`）由系统哈希保护，程序不许写它——写了要么白写，
    /// 要么把用户原来的关联整个判废。这条是硬线，用测试钉住。
    #[test]
    fn nothing_here_claims_a_default_handler() {
        for entry in entries(&exe()) {
            assert!(
                !entry.key.contains("UserChoice") && !entry.name.contains("UserChoice"),
                "碰了 UserChoice：{:?}",
                entry.key
            );
            assert!(
                !entry.key.contains(r"Software\Classes\.txt")
                    || entry.key.ends_with(r"\OpenWithProgids"),
                "动了 .txt 这个类自己的默认值：{:?}",
                entry.key
            );
        }
    }

    /// `Software\Classes\Applications\` 按文件名认程序，路径不该混进去。
    #[test]
    fn the_applications_key_uses_only_the_file_name() {
        find(
            r"Software\Classes\Applications\easy-editor.exe\shell\open\command",
            "",
        );
        for entry in entries(&exe()) {
            assert!(
                !entry.key.contains(r"Program Files"),
                "键路径里出现了安装目录：{:?}",
                entry.key
            );
        }
    }

    /// 图标取 exe 的第 0 号资源，就是 `build.rs` 编进 PE 的那份。
    #[test]
    fn the_icon_references_the_exe_resource() {
        let Entry { value, .. } = find(r"Software\Classes\EasyEditor.txt\DefaultIcon", "");
        assert_eq!(
            value,
            Value::Str(r"C:\Program Files\EasyEditor\easy-editor.exe,0".into())
        );
    }

    /// 键路径为空或以反斜杠结尾，`RegCreateKeyExW` 会打到 `Software\Classes`
    /// 本身上去——那份计划是纯数据，先在这儿卡住形状，比在注册表里看出问题便宜。
    #[test]
    fn every_entry_names_a_real_key() {
        for entry in entries(&exe()) {
            assert!(
                !entry.key.is_empty() && !entry.key.ends_with('\\'),
                "键路径不成立：{:?}",
                entry.key
            );
        }
    }
}
