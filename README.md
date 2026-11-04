# EasyEditor

一个用 Rust + egui 写的小编辑器。功能不多，但每一样都能日常用：文件树、多终端、文本内工作日志块、光标位置前后跳转、全局搜索、查找替换、语法高亮，快捷键全部可改。

## 技术栈

| 用途 | 库 |
|---|---|
| 窗口框架 | `eframe` 0.37（关闭默认 wgpu，用 glow 后端） |
| UI | `egui` 0.37 / `egui_extras` 0.37 |
| 代码高亮 | `syntect` 5.3 |
| 文件树 | `walkdir` 2.5 |
| 终端 | `alacritty_terminal` 0.26 + `portable-pty` 0.9 |
| 全局搜索 | 外部 `ripgrep` 二进制（缺失时自动回退内置字节级扫描） |
| 字节查找 | `memchr` 2 |

## 编译运行

```bash
cd D:\Codes\easy-editor
cargo run --release
```

调试构建已把依赖设为 `opt-level = 2`，`cargo run` 也不慢。

```bash
cargo run -- D:\Codes\some-project      # 启动并打开文件夹
cargo run -- src\app.rs                 # 启动并打开文件
```

## 构建与打包

```bash
cargo check                             # 只做类型检查，最快
cargo test                              # 50 个单元测试（面板树 / 快捷键 / 日志块解析 / 格式化）
cargo build                             # 调试构建 → target\debug\easy-editor.exe
cargo build --release                   # 发布构建 → target\release\EasyEditor.exe
```

发布构建走 `windows_subsystem = "windows"`（见 `src/main.rs` 首行），双击运行不会带控制台窗口。

打包成可分发的 zip：

```bash
mkdir -p dist/EasyEditor
cp target/release/easy-editor.exe dist/EasyEditor/EasyEditor.exe   # 和窗口标题保持一致
cp README.md dist/EasyEditor/
powershell -NoProfile -Command "Compress-Archive -Path dist\EasyEditor -DestinationPath dist\EasyEditor-0.1.0-windows-x64.zip -Force"
```

产物 `dist/EasyEditor-0.1.0-windows-x64.zip` 约 4.4 MB（解压后单 exe 10.5 MB）。
别用 `tar -a -cf xxx.zip`：Git Bash 里那个 `tar` 是 GNU tar，`-a` 只认 gzip/bzip2/xz，
后缀写 `.zip` 也只会吐一个纯 tar 包（实测前 4 个字节是文件名字面量），Windows 双击打不开。

产物是**单文件**，不需要运行时。可选的 `rg.exe` 有三种放法：留在 PATH 里、和 `easy-editor.exe` 同目录、或用 `EASY_EDITOR_RG` 指定完整路径；都没有时全局搜索自动回退到内置扫描，所以打包时不必带上。

交叉检查（可选）：

```bash
cargo clippy --all-targets              # 需要 rustup component add clippy
cargo fmt --check                       # 需要 rustup component add rustfmt
```

## 功能

### 文件树
`walkdir` 懒加载，默认隐藏 `.` 开头的条目（右键可切换「显示隐藏文件」）。新建文件/文件夹有三个入口：工具栏按钮、文件树顶部按钮、任意条目（含根目录）右键菜单。

文件树顶部有**文件搜索框**：输入名称或相对路径片段就扫描整个工作区（跳过 `.git` / `target` / `node_modules` 等目录），大小写不敏感，最多列 500 条；点文件直接打开，点目录会展开到它在树里的位置并退出搜索。结果按查询串缓存，连续打字不会每帧重新遍历目录。

新建对话框弹出后输入框立刻拿到键盘焦点，可以直接敲文件名：`Enter` 创建、`Esc` 取消，不需要先用鼠标点一下输入框。

配色跟 shell 里 `ls --color` 一个思路，一眼能分清类型：

| 类型 | 浅色主题 | 深色主题 |
|---|---|---|
| 目录 | 深蓝 `#0B62C4` | 亮蓝 `#6CB6FF` |
| 可执行文件 | 深绿 `#117A3D` | 亮绿 `#7EE787` |
| 普通文件 | 正文色 | 正文色 |

可执行文件按扩展名判定（`.exe` `.bat` `.cmd` `.com` `.ps1` `.msi` `.scr` `.vbs` `.sh`），类 Unix 下再看执行位。

### 终端
`portable-pty` 起真实 PTY，`alacritty_terminal` 做 VT 解析，支持滚动、256 色、Ctrl+C 等控制键。

终端不是单独的底部面板，而是和编辑器一样是**面板树里的一块**：`Ctrl+Shift+`` ` 随时新开一块终端，默认切在当前面板**右侧**，可以同时并排好几块，各跑各的 shell。顶部工具栏里的 `shell:` 下拉框选的是「下一个新建终端用哪个 shell」，自动检测本机可用的：PowerShell 7 / Windows PowerShell / 命令提示符 / Git Bash / WSL。已经开着的终端不受影响，各自记住自己的进程和标题。

终端有焦点时，`Tab`（补全）、方向键（历史 / 光标移动）、`Esc` 都由终端自己处理，不会再被 egui 拿去切换控件焦点。鼠标拖选文字后：

- `Ctrl+C`：有选区就复制，没有选区照常发送中断（`^C`）
- `Ctrl+V`：粘贴
- `Ctrl+Shift+C` / `Ctrl+Shift+V`：复制 / 粘贴（与 Windows Terminal 一致）
- 右键菜单里也有「复制」「粘贴」，`Ctrl+Insert` / `Shift+Insert` 同样可用

选区按绝对行坐标记录，滚动回滚历史后仍然指向原来的文字；复制中文等宽字符时不会把占位格算进去，行尾空白会裁掉。

自动探测不一定找得到（比如 Git Bash 没进 PATH）。工具栏的 `⚙` 按钮打开「终端设置」，给当前选中的 shell 单独指定**程序路径**和**启动参数**，留空就回到自动探测 / 内置参数，改动直接写进 `config.json`：

```json
{
  "terminal_shell": "gitbash",
  "terminal_programs": { "gitbash": "D:/Softwares/Git/bin/bash.exe" },
  "terminal_args": { "gitbash": "-l" }
}
```

种类 id 是 `pwsh` / `powershell` / `cmd` / `gitbash` / `wsl`；`terminal_args` 用空格分隔，带空格的参数用双引号包住。

### 工作日志块
不是独立的任务数据库，而是**写在普通文本文件里**的一种约定格式，可以直接和正文混排、跟着文件走、用 git 管理：

```
===================================================工作日志1 2026-11-01 13:09 [待处理]
1、一段文字内容
2、一段文字内容

===================================================工作日志2 2026-11-01 13:10 [已完成]
1、一段文字内容
```

头部 = 51 个 `=` + 标题 + 空格 + `YYYY-MM-DD HH:MM` + 空格 + `[状态]`。

- `Ctrl+J` 在光标处插入一个新块，光标自动落在 `1、` 后面，直接打字即可
- `Ctrl+Alt+↑` / `Ctrl+Alt+↓` 在块之间前后跳转
- `Ctrl+Alt+D` 切换当前块的 `[待处理]` ↔ `[已完成]`
- 侧栏「日志」页签列出当前文件的所有块，点击跳转

### 光标位置历史
像 IDEA 的 Navigate Back / Forward：`Ctrl+Alt+←` 回到上一个停留过的位置，`Ctrl+Alt+→` 再往前走。

不是每按一下方向键就记一条——光标在某个位置**停够 350ms** 才算一个落脚点，这样连按方向键扫过十几行不会把历史刷爆。同一行里左右挪光标也只更新偏移、不新增条目。跳回来之后的那一次光标同步会被忽略，否则会变成来回横跳。历史上限 200 条，在历史中间又走了新路时，后面的分支作废（和浏览器一致）。

状态栏显示 `历史 ←3 →1`，工具栏的 `←` `→` 按钮也能点。

### 快捷键
所有快捷键都可以改：`F1` 打开设置面板，点一下组合键，然后直接按下新的组合即可。

- `Esc` 取消录制，`Backspace` / `Delete` 解绑该动作
- 撞车会标红提示「与「xxx」冲突」
- 每项可以单独「重置」，也可以「全部恢复默认」
- 只把改过的项写进 `config.json`，以后加新动作不会破坏老配置

### 查找与替换
`Ctrl+F` 查找、`Ctrl+H` 替换，输入条固定在编辑区**底部**（状态栏上方）。

键盘操作（不用离开输入框）：

| 按键 | 在查找框里 | 在替换框里 |
|---|---|---|
| `Enter` | 下一个匹配 | 替换当前并跳到下一个 |
| `Shift+Enter` | 上一个匹配 | 上一个匹配 |
| `Ctrl+Enter` | — | 全部替换 |
| `Tab` | 切到替换框 | 切回查找框 |
| `Esc` | 关闭查找栏 | 关闭查找栏 |

支持区分大小写、正则（替换串可用 `$1` `$2`）、上下跳转、单个替换、全部替换；所有匹配都会在编辑器里高亮，当前项用橙色区分。

鼠标选中一个词，全文所有相同词会立刻用淡蓝底标出来（双击一次取整个词）。规则和查找栏一致：选中的词全小写就忽略大小写，含大写就精确匹配；选区正好覆盖一个完整单词时按词边界匹配，选 `id` 不会误标 `width` 里的 `id`；只圈到半截词（比如从 `foo_bar` 里选 `foo`）退回子串匹配。跨行、纯空白、超过 64 字符的选区不参与；出现次数封顶 2000 处，免得为一个单字母把全文扫穿。选词高亮和查找高亮是两套底色，叠在一起时查找优先。

> 实现上有个坑：单行 `TextEdit` 收到 `Enter` 会主动 `surrender_focus`，所以等控件返回后再判断 `has_focus()` 永远是 `false`，快捷键会静默失效。这里的做法是在 `TextEdit` **之前**把按键从事件队列里 `consume` 掉，焦点就不会丢。

非正则查找走字节级扫描（`memchr` + ASCII 忽略大小写），不会像逐行 `to_lowercase()` 那样给每一行分配临时字符串。

### 全局搜索
优先调用 `rg --json`，流式读取输出边搜边显示；找不到 ripgrep 时回退到内置扫描（`regex::bytes` + `memchr` 建行索引，跳过 `.git`/`target`/`node_modules` 等目录和二进制文件）。

结果统计会显示命中数、耗时和使用的引擎，例如 `166 处结果 · 75 ms · ripgrep`。

结果表格的两列交互：

- 「位置」列窄，长路径会截断成 `a_very_long_directory_…`。悬浮在这一格上会浮出**完整绝对路径**，光标变成手形
- 拖动「位置」和「内容」之间的分隔线可以加宽这一列，宽度记在表格里，后续每帧都保持（`Column::auto()` 做不到——表格每帧要跑一遍测量 pass，拖出来的宽度下一帧就被按内容打回原形）
- 点任意一格直接打开对应文件并跳到命中行

### 大文件
| 阈值 | 行为 |
|---|---|
| > 16 MB | 拒绝打开，状态栏提示 |
| > 1 MB 或 > 20000 行 | 进入**只读虚拟渲染**：只画可见行，内存与耗时和文件大小无关 |
| 其他 | 正常编辑 + 语法高亮 |

同时最多打开 40 个标签，超出会提示先关闭。

### 编辑
行号、当前行高亮、syntect 语法高亮、`Ctrl+S` 保存、未保存标记、关闭前确认。

### 分屏
中央区域是**一棵递归的二分面板树**，思路来自 tmux / i3：每一块面板要么在看文件，要么在跑 shell，任意一块都能继续往右或往下切一刀，切多少次都行，所以「左边写代码、右上开一个终端、右下再开一个终端」这种布局是常态。

分屏快捷键默认**和 Windows Terminal 一致**：

- `Alt+Shift+D` 自动拆分：当前面板宽就左右切、高就上下切，切完两块都更接近方形
- `Alt+Shift++` 向右拆分（竖分割线）/ `Alt+Shift+-` 向下拆分（横分割线）
- `Alt+←↑↓→` 按几何位置聚焦左 / 上 / 下 / 右那块面板（对应 WT 的 `moveFocus*`）
- `Ctrl+Shift+W` 关闭当前面板（对应 WT 的 `closePane`；面板里显示的是文件时等价于关闭标签），最外面那一块不会被关掉，只会退回空白面板

拆的是**当前聚焦的那块面板**。编辑面板拆出来的新面板显示同一个文件，方便左右对比；终端面板拆出来的直接是一个新 shell。

- 每块面板顶部有自己的标题条（文件名 + 未保存标记，或 `⌨ shell 标题`），右侧三个小按钮：`左右` `上下` 拆分、`×` 关闭
- 右键标题条可以把这块面板**移到工作区最右 / 最左 / 最上 / 最下方**——例如终端一开始落在下方，右键标题选「移到最右侧」就变成右栏
- 面板之间的分割线可以拖动，比例按当前像素位置换算，限制在 5%~95%
- 聚焦框按整块面板的矩形来画：面板里既没文件也没终端（只剩欢迎页或空白）时，边框照样撑满整块，不会缩成一小条
- `Ctrl+Shift+Tab` / `Ctrl+Alt+Shift+Tab` 循环聚焦（WT 没有这条，留着给多面板时快速轮转）
- 鼠标点在哪块面板上就聚焦哪块——按坐标命中判断，不靠控件的点击冒泡（egui 里容器自己的响应区在子控件**下面**，点在 `TextEdit` 上的点击永远不会传到面板背景，所以只能自己算）

> 和 WT 一样，这些键在全局快捷键这一层就被吃掉了，不会转发给终端。所以 Git Bash / WSL 里习惯用 `Alt+←` `Alt+→` 跳词的话，会在分屏聚焦时被抢走——`F1` 里把 `focus_pane_left/right` 改成别的键或留空解绑即可。

标签是全局的（和 VSCode 一样）：关掉一块面板不会关掉标签，同一个标签也能同时出现在几块面板上，标题条上会标 `×2`。关闭标签时会把所有面板里存的标签下标一起重映射，不会出现下标越界指向别的文件。

### 格式化
`Alt+Shift+F` 格式化**当前面板里的整个文件**：直接在 buffer 上改、标成未保存，按 `Ctrl+S` 才落盘，不会新开面板或新标签。
文件树右键的「格式化」对 JSON / XML 文件有效：该文件已经打开时走上面这条路径，没打开时原地读文件、格式化、写回。
支持 JSON（用 `serde_json`）和 XML（轻量级 token 流缩进），自动根据文件扩展名或内容特征判断格式；只读预览的大文件会拒绝格式化而不是静默失败。

## 快捷键

全部可在 `F1` 里改，下表是默认值。

| 快捷键 | 功能 |
|---|---|
| `Ctrl+O` | 打开文件夹 |
| `Ctrl+S` / `Ctrl+Shift+S` | 保存 / 全部保存 |
| `Ctrl+P` | 快速打开文件 |
| `Ctrl+Shift+F` | 全局搜索 |
| `Ctrl+F` / `Ctrl+H` | 当前文件查找 / 替换 |
| `Ctrl+Alt+←` / `Ctrl+Alt+→` | 光标位置后退 / 前进 |
| `Ctrl+N` / `Ctrl+Shift+N` | 新建文件 / 新建文件夹 |
| `Ctrl+J` | 插入工作日志块 |
| `Ctrl+Alt+↑` / `Ctrl+Alt+↓` | 上一个 / 下一个日志块 |
| `Ctrl+Alt+D` | 切换日志块完成状态 |
| `Ctrl+`` ` | 聚焦终端面板，没有就新开一块 |
| `Ctrl+Shift+`` ` | 新建终端面板 |
| `Ctrl+B` | 显示 / 隐藏侧栏 |
| `Ctrl+W` | 关闭标签 |
| `Ctrl+加号 / 减号` | 字号 |
| `Alt+Shift+D` | 自动拆分当前面板（宽则左右、高则上下） |
| `Alt+Shift++` / `Alt+Shift+-` | 向右 / 向下拆分当前面板 |
| `Ctrl+Shift+W` | 关闭当前面板 |
| `Ctrl+Shift+Tab` / `Ctrl+Alt+Shift+Tab` | 下一个 / 上一个面板 |
| `Alt+← / ↑ / ↓ / →` | 聚焦左 / 上 / 下 / 右侧面板 |
| `Alt+Shift+F` | 格式化（JSON / XML，只改 buffer） |
| `F1` | 快捷键设置 |
| `F12` | 截图（存到 `%APPDATA%\easy-editor\screenshot.bmp`） |

## 中英文对齐

代码区所有文本统一 `line_height = 1.32 × 字号`，并给中文字体加 `FontTweak::y_offset_factor` 校正基线。

原因是 egui 按**字形所属字体自己的 ascent** 定位基线：Consolas 是 0.7427 em，微软雅黑是 1.0581 em，14px 下中文会下沉约 4.4px，看起来就是「英文贴顶、中文掉底」。而且只要某行含中文，该行行高会被撑大，又让同一行里的英文继续下移，导致纯英文行和混排行的基线还不一致。

具体推导写在 `src/theme.rs` 顶部注释里。

## 配置持久化

字号、深浅主题、侧栏展开状态、侧栏页签、是否显示隐藏文件、上次打开的工作目录、终端偏好（用哪个 shell、程序路径、启动参数）、改过的快捷键，
都会自动存到 `%APPDATA%\easy-editor\config.json`，下次启动原样恢复。

写盘有 600ms 防抖（拖字号时不会狂写磁盘），退出时再补一次。

`keymap` 里只记和默认值不一样的项，可以手改：

```json
{
  "font_size": 14.0,
  "dark": false,
  "last_workspace": "D:/Codes/easy-editor",
  "keymap": {
    "nav_back": "Alt+Left",
    "terminal_toggle": ""
  }
}
```

空串表示主动解绑。组合写法是 `Ctrl+Alt+Shift+键名`，键名大小写不敏感，
方向键写 `Left`/`Right`/`Up`/`Down`，其余用 `Enter`、`Tab`、`Esc`、`F1`、`Backtick`、`Plus`、`A`、`0` 这种。

启动时的目录优先级：命令行参数 > 当前目录像个工程（有 `Cargo.toml`/`.git`）> 上次的工作目录。

## 可选配置

- 全局搜索默认在 PATH 里找 `rg.exe`，也可以放在本程序同目录，或用环境变量 `EASY_EDITOR_RG` 指定完整路径。
- 调试用环境变量：`EASY_EDITOR_SHOT=1` 启动后自动截图；`EASY_EDITOR_VERIFY=split|word|nav|enter|keys|findflow|insert|replace` 启动后自动跑对应脚本（`split` 会开两个文件、左右拆分、逐块格式化、再叠两块终端并往终端敲一条命令；`word` 会造一个重复词样本、分别整词和半截选中并打印命中数），最后截图，便于核对。旧前缀 `CODE_EASY_*` 仍然兼容。
