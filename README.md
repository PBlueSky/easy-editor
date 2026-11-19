# EasyEditor

**English** | [中文](README.zh-CN.md)

A Windows desktop editor written in **Rust + egui**. It does not have many features — it has the ones you use every day: file tree, terminal tabs that sit in the same strip as your file tabs, recursive split panes, journal blocks inside plain text files, SQL blocks that run against MySQL with the results sitting right under the text, cursor position back/forward, project-wide search, find and replace, syntax highlighting, and every keybinding is yours to change.

**Current version** v0.7.0 | **Platform** Windows 10 / 11 x64 | **License** MIT

The release package is a **single exe** (about 12 MB, 5.2 MB zipped). No .NET, no Electron, no runtime of any kind.

## Table of contents

- [Highlights](#highlights)
- [Quick start](#quick-start)
- [Usage](#usage)
- [Configuration](#configuration)
- [Tech stack](#tech-stack)
- [Code layout](#code-layout)
- [Build, test and packaging](#build-test-and-packaging)
- [Known limitations](#known-limitations)
- [Implementation notes](#implementation-notes)
- [Contributing and license](#contributing-and-license)

## Highlights

| Feature | In one line | Details |
|---|---|---|
| Code editing | Line numbers, current-line highlight, syntect grammar highlighting; long lines either wrap or scroll sideways | [Editing](#editing) |
| Terminals are tabs | Real PTYs via `portable-pty`, VT parsing via `alacritty_terminal`; every new terminal is one more tab beside your file tabs instead of a pane squeezing in next to the code, and each tab is a grid you can keep splitting — one shell per cell. Five shells are auto-detected, anything else (MSYS2, busybox) you add yourself | [Terminal](#terminal) |
| Recursive split panes | The tmux / i3 binary pane tree — one for the editor, another inside every terminal tab. Split as deep as you like, default keys match Windows Terminal | [Split panes](#split-panes) |
| Journal blocks | Task blocks written directly into ordinary text files, mixed with prose and versioned with git | [Journal blocks](#journal-blocks) |
| SQL blocks | Jupyter-style: the query lives in a plain text file, `Ctrl+Enter` runs it and the result lands below the editor. `Ctrl+Alt+N` starts a fresh `query-<timestamp>.sql` with the connection already bound | [SQL blocks](#sql-blocks) |
| Multiple results at once | One tab per statement in a horizontal tab bar, so a broken second query does not throw away the first result; up to 10 results, right-click a tab to close a whole side; batch headers, pop-out windows, copy as TSV, export as CSV, sideways-scrolling wide tables, click any cell to pick its whole row (which also opens that row in a window on the right), double-click a cell for its full value | [Results dock](#results-dock) |
| Several connections at once | One worker thread per connection, so a slow query on one never blocks another; the auto-run gate is per connection too | [Connections](#connections) |
| MySQL connections | Named connections in `config.json`, browsed as connection → database → table, passwords via `env:` so no secret sits on disk. Nothing connects at start-up; `✓ 已连接` / `○ 未连接` / `⏳ 正在连接` / `✗ 连不上` show wherever the connection appears, with an explicit connect / disconnect button and idle reaping | [Connections](#connections) |
| Interaction feedback | A pointing hand over everything clickable, hover highlights that do not shift layout, and a tooltip wherever the meaning is not obvious | [Interaction feedback](#interaction-feedback) |
| Cursor position history | Like IDEA's Navigate Back / Forward; a position only counts as a stop after the cursor sits there for 350 ms | [Cursor position history](#cursor-position-history) |
| Find and replace | Case sensitivity, regex, all matches highlighted, hands stay in the input box | [Find and replace](#find-and-replace) |
| Project-wide search | Streams `rg --json` first, falls back to a built-in byte-level scanner when ripgrep is absent | [Project-wide search](#project-wide-search) |
| Files from anywhere | `Ctrl+Shift+O` (or the toolbar `Open file`) picks one or many files, wherever they live; the workspace, the index, the terminal's cwd and your open tabs stay put | [Opening files](#opening-files) |
| Drag and drop | Drag files or a folder in from Explorer; while your button is still down a hint floats in the middle of the window | [Opening files](#opening-files) |
| `.txt` file association | Registers itself into the Windows "Open with" list and Settings → Default apps at start-up, and opens whatever file you double-clicked | [.txt file association](#txt-file-association) |
| Word occurrence highlight | Select a word with the mouse and every identical word in the file is marked immediately | [Find and replace](#find-and-replace) |
| JSON / XML formatting | Edits the in-memory buffer only; nothing touches the disk until `Ctrl+S` | [Formatting](#formatting) |
| East Asian input methods | Pinyin and Japanese work in the new-file dialog, the rename box, the editor body and the terminal | [Input methods](#input-methods) |
| CJK / Latin alignment | Baselines are corrected, so Chinese glyphs do not sink to the bottom of the line | [CJK baseline](#cjk-baseline) |
| Light and dark themes | Repaints the Windows system title bar colour and button glyphs along with them | [System title bar](#the-system-title-bar-has-to-be-repainted-by-hand) |
| Two font-size knobs | *UI font size* moves every piece of text outside the editor — top bar, sidebar, tabs, results dock, every pop-up window; *editor font size* covers only the editor and terminal body, and is what `Ctrl+=` changes. Row heights, column widths and button sizes are measured from the font rather than written down, and the sliders only take effect when you press `应用` — dragging them live made the whole page flicker | [Settings window](#settings-window) |
| One settings window | Left-hand sections: Appearance / Keybindings / Terminal / System / Database. The old keybinding panel, terminal-settings window and the `.txt` switch all moved inside; the top bar's `?` and `⚙` are just shortcuts to a section | [Settings window](#settings-window) |
| Custom keybindings | `F1` opens the settings window on the Keybindings section: click, press the new chord, done. Conflicts are flagged in red | [Customizing keybindings](#customizing-keybindings) |
| Persistent settings | Both font sizes, theme, terminal preferences and your key changes all live in `%APPDATA%\easy-editor\config.json` | [Configuration](#configuration) |

## Quick start

### Download

Grab one of the packages from the [Releases](https://github.com/PBlueSky/easy-editor/releases) page, unzip it, and double-click `EasyEditor/EasyEditor.exe`:

| Package | Contents | For whom |
|---|---|---|
| `EasyEditor-<version>-windows-x64.zip` | The exe + both READMEs (zh / en) + LICENSE | Most people |
| `EasyEditor-<version>-windows-x64-with-rg.zip` | The above plus `rg.exe` and `THIRD-PARTY-NOTICES.txt` | Anyone who wants project search at full speed |

Both packages work: without `rg.exe`, project-wide search silently falls back to the built-in scanner — a bit slower on large repos. Each package ships with a matching `.sha256.txt`; verify it:

```powershell
Get-FileHash .\EasyEditor-0.7.0-windows-x64.zip -Algorithm SHA256
```

### Run from source

Install a Rust stable toolchain (`rustup`), then:

```bash
git clone https://github.com/PBlueSky/easy-editor.git
cd easy-editor
cargo run --release       # first run compiles dependencies, after that it is fast
```

The debug profile already pins dependencies to `opt-level = 2` (see `Cargo.toml`), so plain `cargo run` is not sluggish either.

```bash
cargo run -- D:\path\to\some-project    # start and open a folder
cargo run -- src\app.rs                 # start and open a file
```

With no arguments, the startup folder is chosen by: **command-line argument > current directory if it looks like a project (`Cargo.toml` or `.git`) > last workspace you opened**.

## Usage

### Interface

![EasyEditor main window: file tree on the left, split editor panes in the middle, status bar at the bottom](assets/screenshot.png)

```
┌──────────────── Top toolbar (wraps to a second row when it runs out of room) ─────────┐
├──────────┬─────── Centre: tab strip + pane tree — whichever tab is up owns it ────────┤
│ Sidebar  │  main.rs ×  data.json ×  >_ cmd ×2  ← click it: a terminal grid            │
│ Files    │  the tab you pick decides what that whole block draws                      │
│ Search   ├──────────────────── Bottom: results dock (SQL) ────────────────────────────┤
│ Journal  ├──────────────────── Bottom: status bar + find/replace bar ─────────────────┤
│ Database │                                                                            │
└──────────┴───────────────────────────────────────────────────────────────────────────┘
```

A terminal does not open another OS window and it does not squeeze in beside the code: `+Terminal` adds one more tab to that same strip, and clicking the `>_ …` one swaps the whole centre area for that group's grid. Click back to a file tab and the editor comes back exactly as you left it — one of the two is on screen at a time.

- **Top toolbar**, left to right: `Open folder` `Open file` `Save` `←` `→` (cursor history back / forward) `New file` `New folder` `Sidebar` `Wrap` `Terminal` (switches between the editor and the terminal tabs — the last group comes back to the front, or a new tab opens when there is none) `+Terminal` (a new terminal tab) `‖ Split` `═ Split` `Quick open` `Project search` `Find` `Journal block` `Database`. The `Database` entry is a dropdown (default connection line / `new query` / `insert SQL block` / `run SQL` / `results dock` / `connection manager…` / `sidebar database tree` / `this file is SQL`) — that is the only new button the whole database feature costs. The right-hand group, right to left, is `?` (jumps to the Keybindings section), `Dark` / `Light` (theme), the `shell:` dropdown, `⚙` (the settings window), the editor font size `- 14 +`, and the workspace name.
- **Sidebar** has four tabs: `Files` (the tree), `Search` (project-wide results), `Journal` (blocks in the current file), `Database` (connections → databases → tables → columns). `Ctrl+B` shows or hides it.
- Narrow the window until one row no longer fits and the remaining buttons wrap onto a second row — they never squash together and text is never overdrawn. The right-hand group still anchors right-to-left after wrapping. The file-tree header does the same trick in one row that cannot wrap: `刷新` / `新文件夹` / `新文件` are laid right-to-left first, then the workspace name is placed in the rectangle that is left over and truncated there — a long folder name used to draw straight through `新文件`. Too little room and the name is dropped rather than overlapped; hover it for the full path.

### Opening files

The folder in the sidebar is an **entry point**, not a boundary on what you are allowed to read. `Ctrl+Shift+O` and the toolbar `Open file` button are the same thing: they open the system file picker (multi-select) and put the chosen files into tabs. That is the only place a button for it lives — the welcome page and the empty sidebar offer a line of text instead of a second row of buttons. The other way in is dragging from Explorer, covered below.

Opening a file that lives outside the workspace **leaves the workspace completely alone** — the tree still shows the same folder, the quick-open index is not rescanned, running terminals keep their cwd, and not one existing tab is closed. If you want the neighbourhood too, that is a separate `Open folder` (`Ctrl+O`). The picker starts in the current workspace; with no workspace open it specifies nothing, so Windows remembers where it was last.

Saving writes back to the file's own absolute path. No same-named copy appears inside the workspace.

A `.txt` double-clicked in Explorer arrives as a command-line path and follows the same rule: that one file becomes a tab, and its parent folder is *not* promoted to workspace. Otherwise every double-click on a note in Downloads would index all of Downloads and clobber the working directory you were restoring (see [.txt file association](#txt-file-association)).

**Dropping things from Explorer** walks those same two roads, and it does not matter where in the window you drop — panes are not targets. Files behave like `Open file`: they land in the tab strip and the workspace is left completely alone. A folder behaves like `Open folder`: it becomes the workspace, tabs and all, exactly as if you had clicked the button. Drop both kinds at once and the folder is settled first, so the files in the same batch are not swept away with the old tabs; several folders at once means the first one wins. While the button is still down, a hint floats in the middle of the window — `松开打开 <name>`, or just a count for a batch — and it disappears when you drop or drag out.

**Same-named files** (`D:\docs\note.txt` next to `D:\work\note.txt`) no longer collapse into one identical tab label. Only the colliding group gets extra parent directories spelled out, one segment at a time, until its members differ; everything else stays a bare filename so the tab strip does not grow for no reason. The `…/` prefix marks the part that is still hidden. Pane titles and the close-confirmation dialog use the same names, and the dialog adds the full path on a second line — "Save and close" landing in the wrong folder is worse than not saving.

### File tree

Lazy-loaded with `walkdir`; dotfiles are hidden by default. Three entry points create a file or folder: the toolbar button, the button at the top of the tree, and the context menu on any entry. The context menu also offers "Format" (files only, JSON / XML only), "Rename" (files only), "Reveal in Explorer" and "Open terminal here". The root row's context menu adds "Show hidden files" and "Refresh".

When the create dialog opens, the input box has keyboard focus immediately — just type the name. `Enter` creates, `Esc` cancels; there is no need to click the box first. Pinyin and Japanese input methods work normally (see [Input methods](#input-methods)).

The "Rename" box is drawn on the entry's own row, pre-filled with the current name: `Enter` commits, `Esc` cancels, clicking elsewhere commits. Names containing `/ \ : * ? " < > |`, empty names, `.` and `..`, and names that collide with an existing file are rejected with a message at the bottom of the tree — the original file is never touched. **Files already open in a tab follow the new path**, so unsaved changes are written to the new name instead of resurrecting a copy under the old one, and the quick-open index is rescanned. Renaming directories is not supported yet.

"Open terminal here" opens a new terminal **tab** whose shell starts in the folder you clicked (or the parent folder, for a file). It does not change the default directory chosen in the toolbar, and it never adds a pane to the editor area.

There is a **search box** at the top of the tree: type part of a name or relative path and it scans the whole workspace (skipping `.git` / `target` / `node_modules` and similar), case-insensitively, listing up to 500 hits. Click a file to open it; click a directory to expand the tree to its location and leave search mode. Results are cached per query string, so typing continuously does not re-walk the tree every frame.

`Ctrl+P` "Quick open" takes a different path: the index is built once when the workspace opens (up to 50,000 entries), input is scored by **fuzzy subsequence match**, and you pick with the arrow keys and open with `Enter`.

Colours follow the same idea as `ls --color` in a shell: directories blue, executables green, plain files in the body colour (exact values in [File tree colours](#file-tree-colours)).

### Editing

Line numbers, current-line highlight, syntect grammar highlighting, `Ctrl+S` to save, unsaved markers, confirmation before closing. Up to 40 tabs at once.

Long lines have two display modes, toggled by the **Wrap** toolbar button or `Alt+Z`; the one you pick is saved to config:

- **No wrap** (default): long lines extend past the pane and a horizontal scrollbar appears below. The gutter is drawn *outside* the scroll area, so it stays pinned left however far you scroll. While drag-selecting with the left button held, moving the pointer within 28 pt of the viewport's left or right edge pushes the body the other way (14 pt per frame), so a selection can run off-screen.
- **Wrap**: long lines fold into the pane width and the horizontal scrollbar goes away on its own. One logical line becomes several visual ones, and the line numbers plus the current-line background cover all of those visual rows instead of continuing to highlight "one row per line".

Jumping (project search, find/replace, journal back/forward, cursor history) scrolls to *the line* and leaves the horizontal position alone, so a jump never throws away your horizontal scroll offset.

Font size uses `Ctrl` + `+` / `-` and changes only the editor and terminal text — it does not zoom the whole window.

### Find and replace

`Ctrl+F` to find, `Ctrl+H` to replace. The input bar is pinned to the **bottom** of the editor area, above the status bar. It is keyboard-driven, so your hands stay in the input box:

| Key | In the find box | In the replace box |
|---|---|---|
| `Enter` | Next match | Replace current, then jump to next |
| `Shift+Enter` | Previous match | Previous match |
| `Ctrl+Enter` | — | Replace all |
| `Tab` | Move to the replace box | Move back to the find box |
| `Esc` | Close the find bar | Close the find bar |

Case sensitivity, regex (replacements may use `$1` `$2`), next/previous navigation, replace one, replace all. Every match is highlighted and the current one is marked in orange.

**Word occurrence highlight**: select a word with the mouse and every identical word in the file is immediately marked with a pale blue background (a double-click grabs the whole word). The rules match the find bar — an all-lowercase selection ignores case, a selection containing a capital matches exactly; when the selection covers a complete word, matching is word-bounded, so selecting `id` does not mark the `id` inside `width`; a partial selection (`foo` inside `foo_bar`) falls back to substring matching. Multi-line selections, whitespace-only selections and selections longer than 64 characters are ignored, and hits are capped at 2000 so a single letter does not scan the whole file. Occurrence highlighting and find highlighting use different backgrounds; where they overlap, find wins.

### Project-wide search

`Ctrl+Shift+F`. Runs `rg --json` and reads it as a stream, showing results as they arrive. When ripgrep cannot be found, it falls back to the built-in scanner (`regex::bytes` + `memchr` for line indexing, skipping `.git` / `target` / `node_modules` and binary files).

The summary line reports hit count, elapsed time and which engine ran, e.g. `166 results · 75 ms · ripgrep`.

- The "Location" column is narrow, so long paths truncate to `a_very_long_directory_…`; hovering that cell reveals the **full absolute path**
- Drag the divider between "Location" and "Content" to widen a column; the width sticks
- Click any cell to open the file and jump to the matching line
- `Enter` in the query box starts the search — but **only on the frame that box has focus**. Once you are back in the editor, `Enter` is just a newline again, and while the IME is composing, `Enter` belongs to the candidate window. Repeated `Enter` inside the box re-runs the search, which is the quickest way to refresh after editing files

### Journal blocks

Not a separate task database — a convention **written into ordinary text files**, so it mixes with prose, travels with the file and is managed by git:

```
===================================================工作日志1 2026-11-01 13:09 [待处理]
1、一段文字内容
2、一段文字内容

===================================================工作日志2 2026-11-01 13:10 [已完成]
1、一段文字内容
```

The markers are Chinese and stay Chinese in the file: `工作日志` = "work log", `待处理` = "open", `已完成` = "done", and `1、` is the Chinese enumeration comma.

The header is 51 `=` characters + title + space + `YYYY-MM-DD HH:MM` + space + `[status]`.

- `Ctrl+J` (or the `Journal block` toolbar button, or the insert button in the sidebar's `Journal` tab) inserts a new block at the cursor, which lands right after `1、` so you can start typing; the view **stays where it was**, so a file full of wrapped lines does not fling you back to the top
- `Ctrl+Alt+↑` / `Ctrl+Alt+↓` jump between blocks
- `Ctrl+Alt+D` toggles the current block between `[待处理]` (open) and `[已完成]` (done)
- The sidebar's `Journal` tab lists every block in the current file; click one to jump

### SQL blocks

The same convention applied to queries, so a saved piece of SQL is still just a text file. The header is a legal MySQL comment, which means the whole file can be piped into the `mysql` client unchanged:

```text
-- sql 活跃用户 2026-11-15 14:03 [待执行] conn=demo
SELECT id, name FROM users
WHERE status = 1
LIMIT 50;
```

Header = `-- sql` + title + `YYYY-MM-DD HH:MM` + `[status]` + tail keys. Two tail keys are recognised today: `conn=<name>` binds the block to a named connection, and a bare `auto` allows automatic execution. Anything unrecognised in the tail is ignored, so adding a field later cannot break old files.

- `Ctrl+Alt+N` (or `Database ▸ New query`, or right-click a folder → `新建 SQL 查询`) creates `query-20261115-140312.sql`, already holding an empty block bound to the default connection, opens it and parks the caret on the line below the header. Two in the same second get `-2` appended rather than overwriting each other
- `.sql` files open in **SQL mode**; any other file can switch it on from `Database ▸ 本文件按 SQL 处理`. SQL mode is per-tab and does not touch the file: the buffer is highlighted as SQL (`Ctrl+Enter` runs statements, not lines), and it is what unlocks running a file that has no blocks in it
- `Ctrl+Enter` runs **the selection if there is one, otherwise only the one statement the caret is sitting on** — split by semicolons inside the block (or the whole file, in SQL mode). The status bar reports `已提交 n 条语句到「conn」（第 2/3 句）`, so you always know which slice went out. `Ctrl+Shift+Enter` is the deliberate "hand over the whole block" key
- `Ctrl+Shift+J` (or `Database ▸ Insert SQL block`) drops a block at the cursor, already carrying today's date / time and the default connection
- `Ctrl+/` (or `Database ▸ 切换行注释`) toggles `-- ` line comments over whatever lines the selection touches — or the caret's own line if there is no selection. If any touched line is uncommented the whole group gets commented; all of them commented and the group opens again, so the key alternates. The marker goes *after* the indentation (`    -- SELECT`), the caret stays where it was in the line, and a selection survives the edit so you can press it twice. `-- sql …` block headers are never touched — they look like comments but they are the structure the whole scanner reads, and a select-all that ate them would make the file unrunnable. Files that are not in SQL mode refuse the key and say so in the status bar
- Statements are split by a scanner that knows `'…'`, `"…"`, `` `…` ``, `--`, `#` and `/* … */`, so a semicolon inside a string or a comment does not cut a statement in half; the same scanner produces both the panel titles and the byte spans, which is why the two can never disagree
- Once every statement of a **whole-block** run (`Ctrl+Shift+Enter`) is back, the header status is rewritten `[待执行]` → `[已执行]` / `[失败]` (pending → done / failed). Running one statement with `Ctrl+Enter` deliberately leaves the status alone — one statement of the block finishing is not the block finishing
- A statement that the server rejects gets a translucent red background **in the file itself**, so you can see which one to fix instead of counting panels. The span is validated against the text before it is painted (a mark whose bytes no longer match that statement is dropped rather than placed somewhere wrong), it slides along when the header write-back shortens the block, and typing clears it
- `Ctrl+Shift+R` shows or hides the results dock; `Database ▸ Results dock (n)` does the same

### Results dock

**One statement, one tab**, and up to 10 results stay on the books at once. Per-statement is deliberate: in a block with three `SELECT`s where the second one is broken, the first result is still worth reading, so it is not thrown away with the error. Statements queued behind a failure are marked `未执行` ("not run") instead of spinning forever, and the oldest result is the one evicted.

Ten results in one strip would squeeze the editor out, so only one of them is on screen: a tab bar laid out **horizontally**, one tab per statement, labelled `3·SELECT name FROM u` with a status dot on the left (green done / red failed / blue waiting / grey not run) and its own `✕` on the right. Click a tab to show only that result; the others stay available. Closing the active tab falls back to the first one still in the row, so the dock is never left pointing at nothing.

- Long labels truncate at 26 characters — hover gives the full statement plus rows and timing; when the row has more tabs than fit it scrolls sideways, and whichever tab you just switched to rolls itself into view (only on the frame it changes, so your own scrolling is never hijacked)
- **Right-click a tab to close a whole run of them**: `关闭这个` (this one), `关闭左侧全部` / `关闭右侧全部` (everything left / right of it), `关闭其他` (all the others), `关掉这一批` (this submission's statements) and `关闭全部` (every tab). Each one says how many tabs it would take; an option with nothing to close is greyed out. Left and right mean screen position — a new submission lands at the far left, so "close everything to the right" is also "close everything older"
- **One header row per submission** stays above the tabs — `14:03:12  demo · 「活跃用户」 · 共 3 条`. When two blocks produce nearly identical statements, the timestamp is the only thing that says which batch is which
- **`1/3`, `2/3`, `3/3`** in each panel title, so the order within a batch survives switching tabs

- Each panel header carries `回块` (jump back to the block it came from), `放大` / `收起` (put *this* result into its own resizable window — the way to read a wide table that the bottom strip cannot stretch to fit), `复制` (the whole result as tab-separated text, ready to paste into a spreadsheet), `导出` (CSV), `关掉整块` (close every tab of this submission) and `✕`
- Grid cells are single-line and truncated — that is what keeps a wide table readable. Right-clicking any cell is the one place for copying that cell, its row or the whole table, and for the SQL export below
- **Wide tables scroll sideways.** `egui_extras` gives a table body a vertical bar and nothing else, so the grid is wrapped in its own horizontal scroll area — header and body travel together, so columns cannot slip out of alignment. Each column starts 20 characters wide, keeps an 8-character floor and is still drag-resizable; the `#` column starts at 4½ characters with a 3-character floor (it narrowed once the `▸` at the head of the row went away) and slides along with the rest. One caveat the nesting brings: the table's vertical bar now sits at the right edge of the *content*, so it can be off-screen while you are scrolled left — the wheel still scrolls vertically there
- **The results strip asks the shared style table for its text sizes.** Cell values, column names and the body text of the pop-out / row-detail / full-cell windows all render at `TextStyle::Monospace` (13 pt); timestamps, row counts, row numbers and the hint line under the tabs use `TextStyle::Small` (11.5 pt) — the same table `theme.rs` sets for the toolbar and the side panels. This area used to carry its own 10.5 / 11 / 11.5, one size per call site, so it sat a whole step below everything else in the window and a result looked tiny right after you had been reading SQL at 14 pt. Row pitch and column widths are measured from that size too: one row is the glyph height plus 5 pt, one column is 20 glyph widths, so the numbers cannot drift apart when the table changes. `Ctrl` + `+` / `-` still only moves the editor and terminal text
- **Clicking a cell opens that row in its own window, pinned to the right of the screen**: one `column = full value` field per line, wrapped, selectable, with `复制整行` at the bottom. Clicking the already-solo row closes it again, one window per panel, and it reopens where you left it. That entry point used to be the `▸` at the head of every row — the only way to open that window, while picking the row itself was a separate click on the row number, one job split across two places. The chevron is gone: clicking anywhere on a row now means picking it *and* opening it, and the row-number cell means exactly what every other cell means. Before *that* it unfolded a block inside the grid, which could not be folded back and kept getting clipped by the table's own viewport
- **That window closes itself when you click anywhere else** — on a tab, another cell, the editor — so hunting for a `✕` is no longer part of reading a result. The `钉住` checkbox in its top-right turns that off while you compare a few rows; pinning follows the result, so switching tabs and back keeps it pinned
- **That window takes its size once.** Its height used to be read back out of the content, so a wide row had to grow for a few frames before you could read it, and a 2-column row and a 14-column row each re-derived the shape. The body is now a scroll area that fills the window instead of shrinking to its contents: 430×300 from the first frame (or whatever you dragged it to), with a long row scrolling. The footer button row is booked at its *measured* height before that scroll area gets laid out, and that booking is what kills the creep: reserving an estimate instead let a large UI font push the content a few points taller than the window every frame, `Resize` reached for that, and the taller window made the next frame short by the same few points — so it climbed until it was as tall as the whole app. `表单` / `JSON` in its footer flip the same row between the per-column form and one JSON object — numeric columns as numbers, `NULL` as `null`, BLOBs as hex, a `json` column opened up into real nesting, and the columns in grid order rather than alphabetical. `复制 JSON` puts that object on the clipboard, and the face you picked survives switching rows
- **Click any cell — the row-number cell counts too — to pick rows.** A plain click leaves just that row selected (clicking the already-solo row clears it) and opens that row in the window on the right; `Ctrl` + click adds or drops one row, `Shift` + click selects the run from the row you last clicked without `Shift`, and those two only stack up a batch — they never swap the window. Every picked row lights up cell by cell and the panel header ends with who is picked — `已选中第 N 行` for one row, `已选中 N 行` for several. **Clicking any blank space outside the table drops the selection**, so there is no `取消选中` button to reach for any more: that used to sit under the grid, which scrolls off-screen exactly when the result is long enough that you need it. Copying lives on the cell's right-click menu — `复制单元格`, `复制整行`, `复制整表`, plus `复制选中 N 行` (one TSV line per row) while several rows are picked — rather than on `Ctrl+C` on purpose — that keystroke belongs to the editor, and stealing it would swap whatever you were about to copy there
- **The bottom half of that same right-click menu is the SQL export**, and it reads the table name out of the statement itself (`FROM` / `UPDATE` / `INSERT INTO`), so it only appears when there is a real table behind the result; a `SELECT 1` or a subquery says `认不出表名，导不了 SQL` instead of pasting together half a statement. The row you right-clicked gets `导出这一行 INSERT` / `导出这一行 UPDATE`, and while several rows are picked a second group `导出选中 N 行 INSERT` / `导出选中 N 行 每行一条 INSERT` / `导出选中 N 行 UPDATE` sits next to it — exporting one row then the whole batch never means re-picking rows in the grid. The first `INSERT` covers every picked row in *one* statement — `VALUES` then one parenthesised group per row, so a batch goes over in one round trip — and lists every column the result has, in result order. `每行一条 INSERT` spells the very same columns and values out N times as N standalone statements: one row that collides with a unique key no longer takes the whole batch down with it, and the statement that errors is the row that is wrong. With a single row the two spellings are byte-identical, which is why the second one is not offered there. The `UPDATE` cannot know your primary key — the text protocol reports no indexes — so it is one statement per row (a single `SET` can only carry one row's new values); each `WHERE` matches on *all* of that row's values (`IS NULL` where a value is null) and ends in `LIMIT 1`. That is spelled out in its tooltip, and a row you have since edited will simply match nothing
- **A `去掉自动生成列` tick-box appears at the end of the menu when the result carries an `AUTO_INCREMENT` column.** Tick it and the id columns leave both the `INSERT` (names *and* values) and the `UPDATE` `SET` — the database fills those itself, and writing them out is how a re-import collides with a primary key. The `WHERE` never follows the tick: that half is what finds the row. The choice is remembered on the results dock, not per panel, so it survives a new run and a restart. What it can see is all of what the protocol reports: the text protocol's column flags carry `AUTO_INCREMENT` and nothing else, so generated columns (`AS … VIRTUAL/STORED`) and `DEFAULT CURRENT_TIMESTAMP` stay in the statement and you delete them yourself — the box's tooltip says so rather than promising more
- A cell that is genuinely too narrow for its content shows a hover tooltip with the head of the value, and **double-clicking it opens that one cell in its own window** — wrapped, scrollable, select-all, with `复制本格` putting the *untruncated* value on the clipboard (the same item is on the right-click menu as `查看完整内容`). Same-grid, same-data: the window reads the panel it came from. Like the row window, it closes on any click outside and can be pinned. **A click belongs to the row, a double-click to the cell**: the second press of that double-click opens only the cell window and never runs the picking logic again — otherwise it would read as "clicking the already-solo row again", i.e. deselect the row you had just picked — and the row window on the right steps aside so the same value is not on screen twice
- The bottom strip is resizable even before anything has run — dragging the divider up used to spring back to the default height, because an empty dock only measured one line of text and the panel booked that
- The dock bar has `全部收起 / 全部展开`, `清空` (closes finished panels and keeps the ones still waiting for a reply) and `关闭`
- A query prints `connection · N 行 · M ms` above a scrollable grid — **the list takes the height the dock has**: drag the top edge of the results strip up and it shows more rows, drag it back and it shows fewer, instead of leaving a tall empty area under a short list (the strip itself is resizable between 120 and 600 pt and starts at 320). Few rows means a short table; the popped-out window gives it whatever it asks for. Writes print `影响 N 行` plus the last insert id and the warning count when MySQL reports them
- More than 1000 rows are cut off on purpose and labelled `（截断）`: a forgotten `LIMIT` on a big table should not pin a thread and a few hundred megabytes to a side panel. A read-only statement also carries a 30 s server-side cap and connections a 5 s TCP timeout, so a typo in the host field does not hang for two minutes

`导出` writes RFC 4180 CSV: CRLF line endings, fields containing `,` `"` CR or LF quoted with inner quotes doubled, `NULL` as an empty field, BLOBs as lowercase hex, and a UTF-8 BOM so Excel reads Chinese correctly. Fields that Excel would *execute* — a leading `=`, `@` or tab, or `+` / `-` followed by anything that is not a number — get a `'` prefix deliberately; real negative numbers are left numeric. The file name comes from the block title (illegal characters replaced), falling back to `result_<id>.csv`.

### Auto-execution

Put `auto` in the header and stop typing: 700 ms after the last keystroke the block runs by itself, which is the Jupyter part of the brief. Three gates keep it from biting:

- only blocks explicitly marked `auto`, and **only when every statement in the body is read-only** — `INSERT` / `UPDATE` / `DELETE` / `DROP` and friends never run themselves, whatever the header says
- one body runs once: the pair (block, text) is remembered, so you have to change the SQL to get another pass; there is no polling loop
- nothing new is submitted to a connection while it still has replies outstanding, so a slow server cannot stack batches on it. The gate is per connection: another connection being slow never holds this block back

Writes still need a human `Ctrl+Enter`.

### Connections

Everything database-related hangs off the single `Database` dropdown in the toolbar — inserting and running blocks, the results dock, the connection manager and the sidebar tree.

**`Database ▸ Connection manager…`** (window title 数据库连接; the settings window's Database section has a button for it too) lists every saved connection in the upper half: `新建` to add, click a row to edit it, `删除` to drop it, and `测试连接` in the form to really open the server and read back its version. Renaming clears the cached tree and the worker thread of both the old and the new name and keeps the default marker on the renamed connection — but a block still written `conn=旧名` does **not** follow, so edit the header too. A connection is `name` / `kind` / `host` / `port` / `user` / `password` / `database`, and it is stored verbatim in `config.json`:

```json
{
  "db_connections": [
    {
      "name": "demo",
      "kind": "mysql",
      "host": "127.0.0.1",
      "port": 3306,
      "user": "root",
      "password": "env:DEMO_PW",
      "database": "shop"
    }
  ]
}
```

`password` accepts `env:VAR`, resolved from `VAR` and then from `EASY_EDITOR_VAR` when the connection is actually opened, so the config file does not have to hold the secret. A literal password works too — remember that `config.json` is readable by your Windows account.

**You used to be unable to tell what was clickable in that window.** The list and the form sat on the same background, and the field outline was the widget stroke the style sheet reserves for pale grey button borders — laid on a white window it is invisible, so a text field read as a line of bare text, while the labels carried the same weight as the fields they belonged to. The window is now two bordered cards: the saved-connections list on the faint background, the form on the window's own fill, so at a glance there are two things rather than one wall of text. Inside the form each label sits **above** its field in the small, weak rung, and the fields are laid out in two columns — kind / port, host / database, user / password — whose widths and row heights are computed from the window's locked body width and the current font size. Within that card the field and combo outlines are deliberately darkened one step and turn accent-coloured on hover; on focus egui already draws its ring with `selection.stroke`, so no second focus ring was invented. That darkening is scoped to the card's own `Ui` (`Ui::visuals_mut` goes through `Arc::make_mut`), so nothing outside the card changes and buttons elsewhere keep their original frame.

**Whether a session is up is said out loud.** This used to be "a small grey line next to `测试连接`", which on a screen this busy meant "did nothing". The mark now says what the *session* is doing, not what some test found earlier, and it shows everywhere the connection appears:

| Mark | Meaning |
| --- | --- |
| `○ 未连接` | nothing has been said to this server. **Every connection starts here** |
| `⏳ 正在连接…` | the handshake is in flight — it gives up and turns `✗` after 20 s, so it never strands a spinner |
| `✓ 已连接 · 8.0.33` | a session is up, version read back |
| `✗ 连不上 · 2003 …` | the server's own words, selectable so you can copy them |
| `⚠ 配置改过了` | a session *was* established, but the host / port / user / password / database behind it has since changed |

**Nothing connects when the app starts.** Ten configured connections of which you use two should not mean ten threads and ten TCP handshakes. Startup opens zero — `startup_connects_nothing` makes that a test, not a habit. A session appears by exactly three routes: the `连接` chip in the sidebar, `测试连接` in the form, or simply `Ctrl+Enter`, which connects the block's own connection on demand. The sidebar footer says `已连接 2 / 10`, so "configured" and "connected" are never confused again.

Sessions do not linger either: a connection idle for 5 minutes (`IDLE_LINK`) has its worker thread reaped and its mark back to `○` — one that is mid-statement is left alone. `断开连接` drops the thread and the cached tree right away, and says so in the status line ("the next run reconnects by itself"). When the server kills a session (`2006` / `2013` / `2055`) the mark flips to `✗` and the next run reconnects; a plain SQL error does not count as a break — the server answered, so the session is still good.

The mark is claimed by a hash of the connection's fields (`ConnectionSpec::stamp()`), so editing a field turns `✓` into `⚠` on that frame instead of continuing to vouch for a server you have never contacted since. Renaming *does* reset the mark to `○`: the name is the worker thread's key, and a verdict may not survive onto a session that never shook hands. Verdicts belonging to a connection you deleted are dropped, not replayed onto someone else.

**Several connections run at the same time, really.** Each connection has its own background thread and replies are matched by (connection, statement id), so a three-minute report on one connection never stops another from answering `SELECT 1`. A `⏳N` badge on a sidebar row is how many statements that connection still owes. The auto-run gate follows the same split: it only holds when *this* connection still has outstanding replies.

**`Database ▸ Sidebar database tree`** opens the `Database` tab: connection → databases → tables, each level fetched lazily the first time you expand it, with a `重试` (retry) button on the row that failed. Double-click a table to copy its qualified name; its context menu also copies a ready-made `SELECT * FROM db.table LIMIT 100;`.

The tree is three columns of meaning, not a wall of labels: a `▸` / `▾` chevron in its own column (expand only — clicking it never changes what a block would hit), one glyph per level (`◆` connection, `▤` database, `▦` table) tinted by the accent when that row is the live one, and the name. The whole row is the button, so the hit target is a row rather than 14 px of text, and the selected row gets a filled background instead of a colour the theme may already be using elsewhere.

Two rows answer "where does my SQL go", and they answer it separately:

- **connection** — click the name and it becomes the default; a block without `conn=` is sent there
- **database** — click the name and it becomes *that connection's* current database, overriding the `database` field in `config.json` for everything sent to it. Nothing is written to disk: it is a session choice, and it is what `effective_spec()` builds the outgoing connection from, so the tree, the status chip and the wire cannot disagree

Both are restated at the head of the tab — `默认连接 demo · root@127.0.0.1:3306` and `当前库 shop` — because a tree that only shows the connection is a tree that hides half the answer.

That card carries a third line, `已连接 2 / 10`, and a chip that changes with the state: `连接` when the session is down, `断开连接` when it is up. It is deliberately not hidden in the right-click menu — when the mark says "not connected", the same screen should say how to change that. The context menu offers the same thing.

### Interaction feedback

Whatever you can click should look clickable, and this round pushed that through the whole app rather than only the database screens:

- **The cursor becomes a pointing hand over anything actionable** — buttons, checkboxes, result tabs, tree rows, every cell of the search results, file tree rows, pane titles, journal block titles. Two layers do it: egui's own `visuals.interact_cursor` is set globally, which covers every `Button` (including `small_button` and the inline chips), and the hand-painted rectangles — result tabs, sidebar rows, file tree rows, the `钉住` checkbox in the row window — each get `on_hover_cursor(HAND)` because the global style does not reach them
- **Hover lights up, without moving anything.** Those inline controls that look like text but are buttons (`回块`, `放大`, `复制`, `导出`, `连接` / `断开连接`) use egui's "hide the frame while inactive": the background appears only on hover and press, while the padding is always reserved, so pressing one does not shove the row sideways
- **The whole row is the hit target** in the sidebar tree and the file tree — you aim at a row, not at fourteen pixels of text — while the `▸` / `▾` column stays expand-only, so it can never quietly change where your SQL goes
- **Every hover has a sentence.** The tab gives the full statement plus rows and timing, a clipped cell gives the head of the untruncated value, a tree row spells out what clicking the name, the chevron or right-clicking each does, and the `SQL → …` status chip says which database this keystroke would hit

**Which connection a block goes to is never a guess.** That fact is written in three places at once: the `默认连接` / `当前库` card at the head of the sidebar (above), a `默认` badge on the connection row itself, and the `SQL → demo ✓` chip in the status bar while the file you are looking at is SQL (by extension, by block, or by mode). The chip is a button: click it and the sidebar switches to the `Database` tab. Before any click at all, the default is the first connection in `config.json`, and that is what the badge shows — the display follows `active_conn()`, the same function `Ctrl+Enter` asks, so the two cannot drift apart.

**Adding another engine** is meant to be a small change: `src/db/mod.rs` is the engine-agnostic protocol (spec → driver, statements in, events out) and `src/db/mysql.rs` is the only file that knows MySQL. A new database means one `DbKind` variant, one branch in `connect()` and one driver module — the block parser, the results dock and the CSV layer do not change. Everything runs on a background thread per connection and reports through a channel; this program has no async runtime and gets none from this feature.

### Terminal

A terminal is **one more tab in the strip that already holds your files** — browser style. `+Terminal` (or ``Ctrl+Shift+` ``) opens a new one, and each tab is a *group* of shells. ``Ctrl+` `` flips between the editor and the terminal tabs: a terminal tab in front replaces the whole centre area with that group's grid, and clicking a file tab gives the editor back with its panes and focus untouched. Right-clicking a folder in the file tree — or a file inside one — and choosing "Open terminal here" starts a new tab's shell in that directory.

Inside one tab nothing is lost: it has its own pane tree, so splitting, divider dragging and directional focus work exactly as they do in the editor area. The tab is named after the first shell in it that is still alive, and `×2` / `×3` behind the name counts the cells. Two ways to close, and they mean different things:

- **A cell's `×`** ends that one shell and leaves the rest of the group alone; closing the last cell retires the tab with it
- **A tab's `×`** closes the whole group at once — selection then moves to the group that is still there, or back to the editor when nothing is left

A terminal tab that is not in front keeps running: the PTY stays up, output keeps landing in its scrollback, and flipping back shows everything that arrived in the meantime. A shell only ends when you close its cell or its group.

Closing really does end it. The shell's own children are torn down with it (deepest first), so nothing keeps a port or a file handle open behind a tab that is no longer there, and quitting the app sweeps whatever shells are still open.

The `shell:` dropdown in the toolbar picks *which shell the next terminal uses*, auto-detecting PowerShell 7 / Windows PowerShell / cmd / Git Bash / WSL on your machine plus any custom entry you added in settings; already-open terminals are unaffected.

When a terminal has focus, `Tab` (completion), the arrow keys (history / cursor movement) and `Esc` are handled by the terminal itself and are not stolen by egui to move widget focus. After selecting text with the mouse:

- `Ctrl+C` copies if there is a selection, otherwise sends the interrupt (`^C`) as usual
- `Ctrl+V` pastes
- `Ctrl+Shift+C` / `Ctrl+Shift+V` copy / paste (same as Windows Terminal)
- The context menu offers Copy and Paste; `Ctrl+Insert` / `Shift+Insert` work too

Selections are recorded in absolute row coordinates, so they still point at the same text after you scroll through the backlog. Copying CJK text does not drag along placeholder cells, and trailing whitespace is trimmed.

Scrollback: the mouse wheel moves **3 rows per notch** (raw wheel events; trackpads are converted at 14 px per row). Whenever there is scrolled-back output, a floating scrollbar appears on the right edge of the pane — drag the thumb, click the track, or use the wheel. It is painted over the text and takes no layout width. Once you scroll up into history the screen stays pinned to the old content (alacritty's behaviour); pressing any key — even a pasted character — snaps the view back to the newest line.

Resizing is conservative in one specific way: a pane only draws rows the PTY has actually accepted. When the panel grows — dragging the divider, resizing the window — the new row count goes to `ResizePseudoConsole` first, and if that call does not land the size stays pending and is retried on the next frame. Spare pixels at the bottom of the pane stay **blank** rather than showing rows the shell does not know it has: a screen model taller than the shell's own leaves those extra rows with nobody to repaint them, and `cls` or PSReadLine redrawing the command line — both of which work on absolute row numbers — would only clear the shell's rows, so the leftover strip at the bottom could never be erased.

Auto-detection does not always find things (Git Bash missing from PATH, for example). The settings window's Terminal section gives each shell its own **program path** and **startup arguments**; leave them empty to go back to auto-detection / built-in args. Changes are written straight into `config.json`:

```json
{
  "terminal_shell": "gitbash",
  "terminal_programs": { "gitbash": "C:/Program Files/Git/bin/bash.exe" },
  "terminal_args": { "gitbash": "-l" }
}
```

Shell ids are `pwsh` / `powershell` / `cmd` / `gitbash` / `wsl`; `terminal_args` is space-separated, quote arguments that contain spaces.

**Anything outside those five can be added.** If the bash you actually use is not one of them — MSYS2, Cygwin, a git-bash installed somewhere unusual, busybox sh — the `+ 新增自定义 shell` button at the bottom of the section adds an entry that appears in the picker immediately; selecting it swaps the form to **display name** / **program path** / **startup arguments**, and changes apply as you type. `删除这条` removes only the record, never the executable.

```json
{
  "terminal_shell": "custom:custom-1",
  "terminal_custom_shells": [
    { "id": "custom-1", "name": "MSYS2 Bash", "program": "D:/msys64/usr/bin/bash.exe", "args": "--login -i" }
  ]
}
```

Custom entries are referenced by `id`, so `terminal_shell` stores `custom:<id>` rather than the display name — renaming "MSYS2 Bash" does not orphan the preference or the panes already running it. A blank name shows as "未命名 shell" so the row is still clickable and legible. A blank path deliberately *stays* in the list: filtering out unusable entries would make a just-added row vanish before you have anywhere to type the path, so instead the form states that the path is still empty and starting a terminal with it reports the name and the missing path rather than silently falling back to cmd. Entries with a blank or duplicate `id` are dropped when the config is loaded — nothing can reference the first, and the second puts the same name in the picker twice.

### Split panes

What the centre area shows is always **a recursive binary pane tree**, in the tmux / i3 tradition: any pane can be split again left/right or up/down, so "code on the left, notes on the right" is the normal state, not a special case. With a file tab up you see the editor tree, which only ever holds editors; with a terminal tab up you see that tab's own tree, one per tab. All of them share one set of keybindings, one grid and one set of commands, and a command is routed by *which tree is in front* — pane numbers are counted per tree, and the widgets' persistent ids are salted by that scope, so focus and scroll positions cannot leak from one group into the next when you switch tabs.

Default keys **match Windows Terminal**:

| Key | What it does |
|---|---|
| `Alt+Shift+D` | Auto-split: vertical if the pane is wide, horizontal if it is tall, so both halves end up closer to square |
| `Alt+Shift++` / `Alt+Shift+-` | Split right / split down |
| `Alt+←↑↓→` | Focus the pane to the left / above / below / right, by geometry |
| `Ctrl+Shift+W` | Close the current pane (the last one is never closed; it falls back to an empty pane) |
| `Ctrl+Shift+Tab` / `Ctrl+Alt+Shift+Tab` | Cycle focus to the next / previous pane |

The split applies to **the currently focused pane** — and to whichever tree was in front when you pressed the key. Splitting an editor pane shows the same file in both, which is what you want for side-by-side comparison; splitting a cell in a terminal tab gives you a fresh shell in the same directory.

- Each pane has its own title bar (file name + unsaved marker, or `>_ shell title` in a terminal tab) with three small buttons on the right: split left/right, split up/down, and `×` to close
- Right-click a pane's title bar to move it to the **far right / far left / top / bottom** of the workspace
- Dividers are draggable; the ratio is computed from live pixel geometry and clamped to 5%–95%
- Clicking a pane focuses it
- Tabs are global (like VSCode): closing a pane does not close the tab, and the same tab can be visible in several panes at once, marked `×2` in the title bar

> As in Windows Terminal, `Alt+←` and friends are consumed at the global keybinding layer and never reach the terminal. If you rely on them for word-jumping in Git Bash / WSL, rebind or unbind `focus_pane_left/right` in `F1`.

### Cursor position history

Like IDEA's Navigate Back / Forward: `Ctrl+Alt+←` returns to the previous position you rested on, `Ctrl+Alt+→` walks forward again.

Not every arrow press is recorded — the cursor has to **sit still for 350 ms** before a position counts as a stop, so raking through a dozen lines does not flood the history. Moving left/right within the same line updates the offset instead of adding an entry. History is capped at 200 entries, and taking a new route from the middle of history discards the branch ahead of you (same as a browser). The status bar shows the history depth (`历史 ←3 →1`, where `历史` is "history"), and the toolbar `←` `→` buttons are clickable.

### Formatting

`Alt+Shift+F` formats **the whole file in the current pane**: it edits the buffer directly, marks it unsaved, and only reaches the disk when you press `Ctrl+S`. It never opens a new pane or tab. "Format" in the file tree context menu works on JSON / XML: an already-open file goes through the path above, a closed one is read, formatted and written back in place. JSON (`serde_json`) and XML (lightweight token-stream indentation) are detected by extension or content signature. Read-only large-file previews are refused explicitly rather than failing silently.

### Input methods

Pinyin and Japanese input work in the **new-file dialog**, the **rename box in the file tree**, the **editor body** and the **terminal**. The dialogs and Quick open request focus only when the input box genuinely does not have it (requesting focus interrupts an in-progress composition). While composing, `Enter` / `Esc` belong to the IME (commit candidate / cancel composition) and are not mistaken for "create the file"; the arrow keys in Quick open likewise yield to candidate paging. The terminal has to publish its cursor cell as the IME anchor and then write the whole committed string into the PTY — without the anchor, the system never associates the IME with this window at all.

### Settings window

Opened by the top bar's `⚙`, by `?`, or by `F1` — a section list on the left, that section's contents on the right:

| Section | What is in it |
|---|---|
| Appearance | **UI font size** (10–20, in 0.5 steps) and **editor font size** (10–28), both staged behind `应用` / `放弃改动`; light / dark, long-line word wrap |
| Keybindings | The whole action table: click a row, then type the new chord. Conflicts turn red; each row has `reset`, and there is `restore all defaults` |
| Terminal | Which shell a new terminal uses; the five built-in shells can have their path and arguments overridden, and `+ 新增自定义 shell` adds one the picker does not know |
| System | The `.txt` association switch, the config file path and a "reveal in folder" button |
| Database | The button that opens the connection manager, plus "N saved connections, the default is X" and what `conn=` does |

**The two font sizes each own their half.** There used to be one adjustable size, and it only covered the editor and terminal body: every other piece of text in the app had its own literal written at the call site, so "I raised the font size and nothing outside the editor moved" was simply true. The UI slider now drives the style table's five rungs (Body / Button / Small / Heading / Monospace), and everything outside the editor takes its size from those rungs. Geometry has to follow the font or the text ends up drawn on itself, so the keybinding table measures its two columns from the widest label at the current size, row heights come from `Ui::text_style_height`, and window widths and the wrap threshold in the top bar are converted through `theme::scale`.

**Which is exactly why the sliders do not apply live.** Dragging one used to repaint the whole app per event: re-rasterising the font atlas, then re-laying out every panel, every row height and every button — and the slider itself moves under the pointer while you are holding it. That reads as the page strobing. Dragging now writes `pending_ui_font` / `pending_font`, and only `应用` (disabled while nothing has changed) hands them to the setters that re-apply the style and persist it; `放弃改动` pulls the staged values back, and so does closing the window, so a slider can never sit at a value that is quietly not in effect. The muted line under them spells out what is pending ("UI 13.5 → 15.0, editor 14 → 16").

Two things deliberately stayed live: the top bar's `- 14 +` and `Ctrl+=` step one notch per press, which produces no event storm and which you reach for constantly, and the light/dark and word-wrap switches, which change no glyph metrics.

The window is resizable from its bottom-right corner and stays where you drag it: it only asks to be centred during the first few frames, because an `anchor()` applied every frame pins the window dead centre and it can no longer be moved.

### Default keybindings

Everything below is the default and everything is rebindable in `F1`.

| Key | Action |
|---|---|
| `Ctrl+O` | Open folder |
| `Ctrl+Shift+O` | Open file(s) from anywhere, workspace unchanged ([Opening files](#opening-files)) |
| `Ctrl+S` / `Ctrl+Shift+S` | Save / save all |
| `Ctrl+P` | Quick open file |
| `Ctrl+Shift+F` | Project-wide search |
| `Ctrl+F` / `Ctrl+H` | Find / replace in the current file |
| `Ctrl+Alt+←` / `Ctrl+Alt+→` | Cursor history back / forward |
| `Ctrl+N` / `Ctrl+Shift+N` | New file / new folder |
| `Ctrl+J` | Insert a journal block |
| `Ctrl+Alt+↑` / `Ctrl+Alt+↓` | Previous / next journal block |
| `Ctrl+Alt+D` | Toggle the journal block's done state |
| `Ctrl+Alt+N` | New query file (`query-<timestamp>.sql`, block header already bound to the default connection) |
| `Ctrl+Shift+J` | Insert a SQL block at the cursor |
| `Ctrl+/` | Toggle `-- ` line comments over the lines the selection (or the caret) touches — SQL mode only |
| `Ctrl+Enter` | Run the selection, or just the statement the caret is on (split by `;`) — in SQL mode with no block, that reads from the whole file |
| `Ctrl+Shift+Enter` | Hand the entire block (or the whole file in SQL mode) to the server and write the block status back |
| `Ctrl+Shift+R` | Show / hide the results dock |
| `` Ctrl+` `` | Flip between the editor and the terminal tabs — a terminal tab in front goes back to the editor, otherwise the last group comes forward, and only a new one when there is none |
| `` Ctrl+Shift+` `` | New terminal tab |
| `Ctrl+B` | Show / hide the sidebar |
| `Ctrl+W` | Close tab |
| `Ctrl+Shift+W` | Close the current pane |
| `Ctrl` + `+` / `-` | Font size (editor and terminal text only) |
| `Alt+Z` | Toggle word wrap |
| `Alt+Shift+D` | Auto-split the current pane |
| `Alt+Shift++` / `Alt+Shift+-` | Split right / split down |
| `Alt+← / ↑ / ↓ / →` | Focus the left / top / bottom / right pane |
| `Ctrl+Shift+Tab` / `Ctrl+Alt+Shift+Tab` | Next / previous pane |
| `Alt+Shift+F` | Format (JSON / XML, buffer only) |
| `F1` | Keybinding settings |
| `F12` | Screenshot, saved to `%APPDATA%\easy-editor\screenshot.bmp` |

### Customizing keybindings

`F1` (or the top bar's `?`) opens the settings window on its Keybindings section. Click a chord, then press the new combination.

- `Esc` cancels recording; `Backspace` / `Delete` unbinds the action
- Conflicts are highlighted in red with "conflicts with 'xxx'"
- Each action can be reset individually, or you can restore all defaults
- Only entries that differ from the defaults are written to `config.json`, so adding new actions later never corrupts old configs

### .txt file association

On Windows, start-up registers EasyEditor as a program that can open `.txt`, so you get either of these:

- right-click a `.txt` → **Open with** → **Choose another app** → EasyEditor (tick "Always" there if you want it to stick), or
- **Settings → Apps → Default apps → EasyEditor → `.txt`**

Registration writes `HKCU\Software\Classes` only — the current user, no administrator, no machine-wide change. What it deliberately does **not** do is claim the default handler: that lives in a `UserChoice` value the system protects with a hash. A third-party program cannot compute it, and writing a wrong one does not merely fail — Windows invalidates the whole association and the file type loses its icon and its default program. So picking EasyEditor as the default stays a click you make, and the app never overrides a choice you already made.

The behaviour is switched by `txt_association` in `config.json` (default `true`). Set it to `false` if you would rather this exe did not appear in the list at all — for example a development build under `target\debug\`, which would otherwise offer Windows a path that disappears on the next `cargo clean`:

```json
{ "txt_association": false }
```

Every key it writes is under its own names (`EasyEditor.txt`, `Applications\<exe name>`, `Clients\TextEditor\EasyEditor`, one line in `RegisteredApplications`), so removing it afterwards is a matter of deleting those — and none of them is shared with another editor. A new key also gets one `SHChangeNotify(SHCNE_ASSOCCHANGED)` broadcast, which is why the entry shows up in the right-click menu right away instead of after the next sign-in. Double-clicking a file works the way you would expect: the path arrives as the first argument and EasyEditor opens that file as a tab. It does **not** promote the file's parent folder to the workspace — no re-index, and the working directory you were restoring survives (see [Opening files](#opening-files)).

## Configuration

Font size (`font_size` is the editor and terminal body, `ui_font_size` is everything else on screen), theme, sidebar visibility, active sidebar tab, hidden-file visibility, word wrap, the `.txt` association switch, last workspace, terminal preferences (including the `terminal_custom_shells` you added yourself), database connections and your keybinding changes are stored automatically in `%APPDATA%\easy-editor\config.json` and restored on the next start. Fields that are not written fall back to defaults (`ui_font_size: 13.5`, `font_size: 14.0`, `word_wrap: false` i.e. long lines scroll sideways; `txt_association: true`; `db_connections: []`; `terminal_custom_shells: []`). Writes are debounced by 600 ms and flushed once more on exit; the font sliders in the settings window only write when you press `应用`, so dragging never touches the file at all.

```json
{
  "ui_font_size": 13.5,
  "font_size": 14.0,
  "dark": false,
  "last_workspace": "D:/Codes/demo",
  "keymap": {
    "nav_back": "Alt+Left",
    "terminal_toggle": ""
  }
}
```

`keymap` records only the entries that differ from the defaults, and you may hand-edit it. An empty string means "deliberately unbound". Chords are written `Ctrl+Alt+Shift+Key`; key names are case-insensitive, arrow keys are `Left`/`Right`/`Up`/`Down`, and everything else looks like `Enter`, `Tab`, `Esc`, `F1`, `Backtick`, `Plus`, `A`, `0`.

**When that file cannot be read**

Hand-editing JSON is easy to get wrong — a missing quote, or an enum spelled in lowercase (`db_connections[].kind` accepts `"Mysql"`, not `"mysql"`). The app then starts with defaults, but it does **not** write those defaults over your file:

- the unreadable file is renamed to `config.json.broken` in the same folder (if one is already there, the next one gets a number — an earlier backup is never overwritten);
- a dialog appears in the middle of the screen saying where it tripped (serde's own message, with line and column), where the backup went, and offering to copy the path. Dismissing it stops the dialog, not the protection;
- if even the rename failed, or the file could not be read at all, the app **does not save for the rest of the session**: font size changes and new connections are dropped on exit rather than written over your hand-written config.

A zero-byte `config.json` counts as "nothing configured yet", not as damage — no backup, no dialog.

**Optional environment variables**

| Variable | Effect |
|---|---|
| `EASY_EDITOR_RG` | Full path to `rg.exe`. Otherwise PATH is searched first, then the program's own directory; if neither has it, the built-in scanner runs |
| `EASY_EDITOR_SHOT=1` | Take one screenshot right after startup (debugging) |
| `EASY_EDITOR_VERIFY=<script>` | Run a scripted interaction and screenshot after startup: `split` / `word` / `nav` / `enter` / `keys` / `findflow` / `insert` / `replace` (debugging; the legacy `CODE_EASY_*` prefix still works) |

## Tech stack

| Purpose | Library |
|---|---|
| Windowing | `eframe` 0.37 (default wgpu disabled, `glow` backend) |
| UI | `egui` 0.37 / `egui_extras` 0.37 |
| Syntax highlighting | `syntect` 5.3 |
| MySQL client | `mysql` 28, with the default `derive` and C compression backends switched off — every query is a plain synchronous call from a worker thread, so there is no tokio anywhere in this program |
| File tree | `walkdir` 2.5 |
| Terminal | `alacritty_terminal` 0.26 + `portable-pty` 0.9 |
| Project search | External `ripgrep` binary, falling back to `regex` 1 + `memchr` 2 |
| East Asian widths | `unicode-width` 0.2 |
| Config / serialization | `serde` 1 + `serde_json` 1 |
| Timestamps | `chrono` 0.4 |
| Native file dialogs | `rfd` 0.15 |
| Windows title bar colours | `raw-window-handle` 0.6 + `DwmSetWindowAttribute` |
| `.txt` association | `RegCreateKeyExW` / `RegSetValueExW` from `advapi32`, declared by hand like the title-bar calls — no registry crate |
| exe icon and version info | `winresource` 0.1 (build dependency) |

Everything is immediate-mode GUI: no QSS, no CSS, no webview. Widget state lives in states keyed by `egui::Id`. Rendering goes through glow (OpenGL) and repaints are on demand — with no terminal running and no cursor blinking, a still interface does not draw new frames.

## Code layout

About 26,900 lines of Rust in a single binary crate, 24 source files:

| File | Responsibility |
|---|---|
| `src/main.rs` | Entry point, `windows_subsystem`, window builder and icon |
| `src/app.rs` | The application itself: layout, keybindings, open/save, project search, find/replace, journal blocks, terminal tabs (opening, splitting, closing, switching), SQL submitting, per-tab SQL mode and new-query files |
| `src/pane.rs` | tmux-style recursive binary pane tree (`Leaf` / `Split`); layout only, no content. One instance for the editor, one per terminal tab |
| `src/editor.rs` | The editor widget: line numbers + highlighting + editing + jumping + read-only virtual rendering for large files + overlay painting |
| `src/terminal.rs` | PTY I/O, VT parsing, keyboard/mouse routing, selection, scrollbar and size sync |
| `src/procs.rs` | Process-tree cleanup: a Toolhelp32 snapshot finds what a shell spawned and terminates it deepest-first (closing a cell, a tab, or the app) |
| `src/filetree.rs` | Directory tree, lazy loading, file search, type colours, tab-label disambiguation |
| `src/find.rs` | Find/replace within the current file (match positions are byte offsets) |
| `src/search.rs` | Project-wide search: ripgrep first, built-in scanner as the fallback |
| `src/journal.rs` | Journal block parsing and insertion |
| `src/sql.rs` | SQL block text layer: scan and build blocks, split statements **with byte spans**, pick out the one a byte offset falls in, decide what is read-only, and work out what `Ctrl+/` rewrites. Touches neither database nor UI |
| `src/database.rs` | Sidebar `Database` tab and the connection manager window: cached tree, in-flight requests, the connection draft, and the live session state (`○` / `⏳` / `✓` / `✗`) with the fingerprint that retires it |
| `src/db/mod.rs` | Engine-agnostic protocol: `ConnectionSpec` → `Driver`, one worker thread per connection, events back out, plus `MockDriver` so the UI is testable without a server |
| `src/db/mysql.rs` | The only file that knows MySQL: connect options, the text protocol → cells, one statement per call |
| `src/results.rs` | Results dock: one panel per statement, batch headers, pop-out window, status write-back, TSV for `复制`, RFC 4180 CSV for `导出`, sideways scroll, multi-row selection (click any cell / `Ctrl` / `Shift`, blank click clears), per-row detail window with a form/JSON side, single-row and batch `导出…INSERT` — one statement or one per row — and `导出…UPDATE` on the cell's right-click menu, with the auto-increment columns optional |
| `src/nav.rs` | Cursor position history |
| `src/format.rs` | JSON / XML formatting |
| `src/keymap.rs` | Configurable keybindings: action ids, serialization, conflict detection |
| `src/config.rs` | Config read/write with debouncing |
| `src/theme.rs` | Fonts, colours, CJK baseline derivation |
| `src/syntax.rs` | syntect highlighting, cached per line into egui galley spans |
| `src/ime.rs` | IME composition state, anchor handling |
| `src/titlebar.rs` | System title bar colours (Windows only) |
| `src/assoc.rs` | `.txt` association: the registry values to write as a plain list (unit-testable without touching this machine's registry), plus the thin `advapi32` call that writes them |

## Build, test and packaging

### Daily commands

```bash
cargo check                             # type checking only, fastest
cargo test                              # 345 unit tests
cargo build                             # debug build → target/debug/easy-editor.exe
cargo build --release                   # release build → target/release/easy-editor.exe
cargo clippy --all-targets              # requires rustup component add clippy
cargo fmt --check                       # requires rustup component add rustfmt
```

Release builds use `windows_subsystem = "windows"` (first line of `src/main.rs`), so double-clicking the exe does not spawn a console.

Tests cover the pane tree, keybinding parsing, journal and SQL block parsing, statement splitting, the database protocol and results dock, CSV quoting, formatting, title bar colours, icon composition, terminal scrolling and selection, highlight background alignment, edge auto-scroll, IME anchors, the order a process tree is harvested and more. A batch of them are **headless UI tests**: they feed a constructed sequence of input events through `Context::run_pass` and inspect the resulting layers and `PlatformOutput`. No window is ever opened, so the whole suite runs unattended in one go. Everything database-related is tested against a `MockDriver`, so `cargo test` needs no MySQL server.

```bash
cargo test --bin easy-editor            # this is a binary crate, there is no lib target
cargo test -- --ignored --nocapture     # two tests that really spawn processes: a cmd.exe PTY, and a cmd grandchild that must die
```

### Release packages

```bash
mkdir -p dist/EasyEditor
cp target/release/easy-editor.exe dist/EasyEditor/EasyEditor.exe   # keep it identical to the window title
cp README.md README.zh-CN.md LICENSE dist/EasyEditor/
powershell -NoProfile -Command "Compress-Archive -Path dist\EasyEditor -DestinationPath dist\EasyEditor-0.7.0-windows-x64.zip -Force"
```

For the `-with-rg` variant, drop `rg.exe` and `THIRD-PARTY-NOTICES.txt` into `dist\EasyEditor\` and compress again. Each package gets its own `.sha256.txt`.

> Do not run those two in the other order: `Compress-Archive` zips whatever sits in `dist\EasyEditor\`, not a list of file types it recognises. Add `rg.exe` first and the plain package carries 5 MB of it while its name says the opposite.

> Do not use `tar -a -cf xxx.zip`: the `tar` in Git Bash is GNU tar, and its `-a` only understands gzip/bzip2/xz. With a `.zip` suffix it still emits a plain tar archive, which Windows will not open on double-click.

### App icon

The single source of truth is `assets/easy-editor.svg` (1024 grid, colours taken from the editor's base16-ocean dark theme). Regenerate after editing:

```bash
powershell -NoProfile -ExecutionPolicy Bypass -File tools/render-icon.ps1
```

The script uses a headless Chrome/Edge screenshot to render one 1024 master image; every other size (16/24/32/48/64/128 plus the 256 used at runtime) is downscaled from that master by area-weighted sampling in `tools/make-ico.mjs`, then assembled into `assets/easy-editor.ico` (small sizes are classic 32bpp DIB + 1bpp AND mask, the 256 slot carries a PNG, and the script encodes both itself with no library dependencies). Intermediates land in `assets/build/` and are not versioned; `assets/easy-editor.ico` and `easy-editor-256.png` are committed.

The two icons cover different surfaces:

- `assets/easy-editor.ico` is compiled into the exe's PE resources by `build.rs` (build dependency `winresource`) and drives Explorer and shortcuts;
- `assets/easy-editor-256.png` is embedded into `src/main.rs` with `include_bytes!`, handed to `ViewportBuilder::with_icon` at runtime, and drives the title bar and taskbar button.

`winresource` only writes a manifest when explicitly asked, and this build does not ask, so DPI awareness and the linker defaults are exactly the same as without an icon; it does write the `Cargo.toml` version into the exe's properties page. Explorer picks up a new icon through the icon cache — if nothing changes locally, run `ie4uinit.exe -show` or try a different file name.

## Known limitations

- **Only Windows 10 / 11 x64 has been used day to day.** Windows-specific code (system title bar colours, exe resources, console switches) is wrapped in `#[cfg(windows)]`; other platforms should compile, but nobody has verified them.
- **`.txt` association is registration, not takeover.** EasyEditor appears in "Open with" and in Settings → Default apps; it does not quietly become your default text editor, and on Win10/11 a program cannot make itself one — `UserChoice` is hash-protected ([why](#txt-file-association)).
- Terminal shell auto-detection covers PowerShell 7 / Windows PowerShell / cmd / Git Bash / WSL. If it misses one, set the path by hand in the settings window's Terminal section — or add the shell itself with `+ 新增自定义 shell` if it is not one of those five flavours.
- Large files rendered in read-only virtual mode (> 1 MB or > 20,000 lines) neither wrap nor scroll horizontally — widen the pane to see a long line. These files also refuse formatting.
- Files over 16 MB are refused outright, with a message in the status bar.
- CJK glyph widths depend on `unicode-width` plus Microsoft YaHei; switch fonts and the baseline alignment parameters (see [CJK baseline](#cjk-baseline)) probably need recomputing.
- **MySQL is the only engine** that has a driver, and the sidebar tree speaks `SHOW DATABASES` / `SHOW TABLES`. The connection form has no TLS, SSH-tunnel or `init_connect` settings either, so what you get on the wire is whatever the `mysql` crate defaults to.
- A statement is capped at 1000 rows and the dock at 10 results; the cap is labelled `（截断）` rather than hidden. `DELIMITER` blocks (stored procedures) are out of scope — the splitter cuts on `;`, so split those by hand.
- Automatic execution only ever runs blocks marked `auto` **and** read-only; anything that writes needs a deliberate `Ctrl+Enter`.
- **SQL mode is a property of the tab, not of the file.** Switching it on for `notes.txt` is remembered while that tab is open and forgotten when you close it; reopening the file falls back to whatever its extension says. Nothing is written into the file to mark it.
- A literal password in `config.json` is readable by anyone with your Windows account — write `env:VAR` instead if that matters.
- No plugin system, no LSP, no Git integration. Deliberately so: this is an editor someone uses every day, not a platform.

## Implementation notes

This section records *why the code looks the way it does*. Every entry is a real problem — worth a read before touching the related code.

### CJK baseline

All text in the code area uses `line_height = 1.32 × font size`, and the Chinese font carries a `FontTweak::y_offset_factor` to correct its baseline.

egui positions a glyph's baseline from **the ascent of the font that glyph resolved to**: Consolas is 0.7427 em, Microsoft YaHei is 1.0581 em. At 14 px Chinese sinks about 4.4 px, which reads as "Latin hugs the top, Chinese falls to the bottom". Worse, any line containing Chinese gets a taller line height, which pushes the Latin on that same line down again — so pure-Latin lines and mixed lines end up on different baselines. The full derivation is in the comment block at the top of `src/theme.rs`.

### Match highlight rectangles must not use the text format background

egui draws that rectangle one line-height below "baseline minus the primary font's ascent", and the primary font (Consolas) has a noticeably shorter ascent than the Chinese font. Measured at size 14, the rectangle sits 0.9 pt low on Latin lines and 4.9 pt low on Chinese lines, so the whole highlight lands on top of the next line. It is now inserted per visual row into the layout grid after galleys are built (`editor.rs::paint_overlay`), with coordinates `0 … line_height` inside the row — no longer related to any font ascent. Insertion happens before the glyph triangles, so it covers the row but never the text. Multi-line matches (wrapped lines, selections containing newlines) get one rectangle per row.

### A single-line TextEdit drops focus when it receives Enter

So checking `has_focus()` after the widget returns is always `false`, and the find bar's shortcuts fail silently. The fix is to `consume` the key from the event queue **before** the `TextEdit`, which keeps focus intact.

The same fact has a second consequence, and this one is about who owns the key at all. The sidebar's search box used to treat "`lost_focus()` and an `Enter` in the queue" as "press Enter to search again". But `Enter` in the editor is a newline, and a key press still sitting in the queue is not evidence that anyone asked for a search — the very next `Enter` you type into a document re-ran the project search instead of breaking the line. Both input boxes now read focus **before** adding the widget (`ui.memory(|m| m.has_focus(query_id))`), skip the frame entirely while the IME is composing (`ime::composing`, because that `Enter` confirms a candidate word), and only then `consume_key`. The id comes from `egui::Id::unique`, not `ui.id()`, which in 0.37 is `Ui::scope_id` and would change with the layout.

### Registering `.txt` happens in `new`, not in `bootstrap`

`bootstrap` is what the headless tests construct the app with — it never touches `eframe`, and that is exactly why the registry write lives in `EasyEditorApp::new` instead: otherwise every `cargo test` run would register the *test binary* sitting in `target\debug\deps\` as this machine's text editor. `assoc::entries()` returns the whole registry plan as plain data, so the shape of it (quoting a path with spaces, `RegisteredApplications` pointing at the `Capabilities` key it just filled, only `.txt` offered, nothing touching `UserChoice`) is covered by tests that never open a key.

### Requesting focus interrupts IME composition

egui's `Memory::request_focus` documents that it calls `interrupt_ime`. Two consequences:

- Dialogs and Quick open must call `request_focus` **only when the box does not have focus**. Calling it unconditionally every frame clears the pinyin candidate string as soon as it appears, and Chinese simply cannot be typed (`ime::focus_input_box`).
- While drag-selecting, egui calls `request_focus` on the already-focused `TextEdit` every frame, which also flags "interrupt composition". egui-winit responds with `set_ime_allowed(false)` immediately followed by `(true)` — the IME context is detached from the window and reattached every single frame, which shows up as the taskbar IME icon flickering. You are not typing while holding the mouse button anyway, so those frames simply do not publish an IME anchor, and it comes back on its own the frame you release (`ime::hide_ime_anchor_while_pointer_held`).

### Ask for focus after TextEdit has interacted

On the frame you click a button such as `Journal block` in the toolbar, there is both a mouse click and a pointer that is not over the text, so egui's `interact` calls `surrender_focus` on the previously focused widget it did not hit. Focus requested before `ui.add(editor)` is erased in the same frame: the block gets inserted, but the cursor is gone and you cannot type. The `request_focus` that sets the caret or selection after insertion must therefore run after `ui.add`.

### The terminal must publish its own IME anchor

egui only publishes `PlatformOutput.ime` while a `TextEdit` has focus. The terminal is not a `TextEdit`; without an explicit anchor, `allow_IME` stays `false`, winit detaches the IME context from the window, and everything you type arrives as raw pinyin letters. The terminal now reports its cursor cell as an `IMEOutput` (`purpose = Terminal`) and writes the whole `ImeEvent::Commit` string into the PTY.

> `set_ime_purpose` is a no-op on Windows, so reporting `Terminal` only matters on other platforms — but publishing an anchor at all is required everywhere.

### Alacritty pins you to scrollback

While `display_offset > 0`, all new output goes into the scroll buffer and the viewport does not move. If nothing resets it, anyone who scrolled up sees stale content sitting on screen while commands run underneath. Anything written to the PTY now triggers `scroll_display(Scroll::Bottom)`.

### Pane focus is hit-tested by geometry, not by click bubbling

In egui a container's own response area sits *behind* its children, so a click on a `TextEdit` never reaches the pane background. "Click a pane, focus that pane" is therefore computed from rectangles by hand.

### The zoom shortcut collided with egui's whole-window zoom

egui ships `Ctrl+=` / `Ctrl+-` / `Ctrl+0` that change `zoom_factor` (the entire UI scales), which clashes with this app's font size binding: zooming in went through whole-window scaling while zooming out only moved the font size, leaving the interface stuck enlarged. `bootstrap` now sets `Options::zoom_with_keyboard` to false — font size affects only the editor and terminal text, and whole-window scaling is left to system DPI. Also, `+` and `=` share one physical key, so the comparison is done on the physical key (`keymap::same_physical_key`).

### A shortcut has to swallow the character it drags along

One keystroke on Windows puts *two* events into the queue via egui-winit: the `Event::Key`, and immediately after it an `Event::Text` holding the character that key produces on the current layout. egui-winit skips that character only when Ctrl is down — Alt chords still send it. `keymap::take` used to call `consume_key`, which removes the key press and leaves the character behind, and whatever panel drew next would eat it: `Alt+Shift+D` split the pane *and* put a `D` into the text, and with a terminal tab in front that `D` went straight into the PTY. The claim now walks the queue and drops the character that directly follows the key press — only the next event, so if anything else sits between them the text was ordinary typing and stays. The same pass clears every matching press in the frame, so holding a chord down and getting auto-repeat still performs the action once.

### The system title bar has to be repainted by hand

The minimize / maximize / close row is drawn by the Windows DWM, which egui does not reach. `winit` only flags uxtheme **at window creation** using the theme of that moment, and it uses an attribute the DWM does not consult — so the title bar background and the button glyphs end up following two different palettes: white glyphs on a light background make the buttons invisible, and switching themes does not update them. `src/titlebar.rs` therefore re-sends three `DwmSetWindowAttribute` calls at startup and on every theme switch: `USE_IMMERSIVE_DARK_MODE` (which decides whether button glyphs take the light or dark variant), `CAPTION_COLOR` and `TEXT_COLOR`. The background comes from `theme::panel_fill`, the same value as the toolbar, so there is no visible seam between title bar and toolbar. The last two attributes require Windows 11 22000+; on older systems the calls fail and are ignored, leaving only the dark-mode switch. Now that terminals are tabs there is no second OS window to claim, so this runs for the main window alone: once at startup and once per theme switch.

### Why terminals became tabs — and why closing a tab keeps shells alive

Cramming terminals next to the code meant every terminal pane was paid for out of the editing area, and glancing at output cost a slice of the screen. The version before this one went the other way — Windows Terminal style, one shell per OS window — and that turned out to be worse in practice: a row of identically-named windows on the taskbar that you tell apart by reading titles, and the trick only pays off if you actually have a second monitor to throw one at. Tabs are the browser answer: a terminal takes one slot in the strip that already holds the files, `` Ctrl+` `` flips the centre area over to it and back, and the group inside a tab splits as much as you like. Only one side is ever on screen, so neither has to give up room.

Flipping a tab to the back does not stop the shell: PTY, scrollback and selection all live in `TerminalPanel`, and `pump()` drains *every* terminal rather than only the visible one (a background shell whose output piles up in an unbounded channel is both a memory leak and a stale screen when you return). A shell ends only when you close its cell or its whole group.

Ending one, though, had two holes. `Child::kill()` only calls `TerminateProcess` on the shell itself — the `vim`, `node` or half-finished build it spawned carry on holding ports and file handles after the tab is gone. So `Terminal::kill()` first walks the process tree through a Toolhelp32 snapshot (`src/procs.rs`) and terminates it **deepest first**, so a parent cannot respawn what you just killed, then hands the root to portable-pty. The second hole was `on_exit`: closing the window quit the app and left every shell behind, orphaned.

A job object (one shell per job, `TerminateJobObject` on close, dissolved automatically when the app exits) is the tidier design and was not chosen: attaching requires `CREATE_SUSPENDED` and a hand on the child before it resumes, and portable-pty's builder exposes only program path, arguments, environment and window size — no hook for creation flags, which means bypassing it and rebuilding the spawn path. Rewriting PTY startup to save a few dozen lines is not worth it when the snapshot walk has to exist on Windows anyway.

Each tab owns one `PaneTree`, so a group's split layout travels with the tab and survives a trip through the file tabs. The one thing that needs care across trees: the editor tree and a terminal tree both number their `PaneId`s from 1, and egui keys widget state to ids — so `draw_grid` mixes the scope (`PaneScope::Main` / `Term(tab id)`) into every persistent id. Without that salt, switching tabs would carry focus and scroll position from one group into another.

### File tree colours

Same idea as `ls --color` in a shell:

| Type | Light theme | Dark theme |
|---|---|---|
| Directory | Deep blue `#0B62C4` | Bright blue `#6CB6FF` |
| Executable | Deep green `#117A3D` | Bright green `#7EE787` |
| Plain file | Body colour | Body colour |

Executables are decided by extension (`.exe` `.bat` `.cmd` `.com` `.ps1` `.msi` `.scr` `.vbs` `.sh`); on Unix-like systems the executable bit is checked too.

### Small icon sizes cannot be captured by Chrome directly

Headless Chrome is unreliable at small window sizes: `--window-size=128,128` and `160,160` consistently produce fully transparent output, and `256,256` shifts the whole drawing right and clips it — hand the title bar or taskbar such an image and you get "half an icon". So Chrome renders only the 1024 master and every other size is scaled locally. `tools/render-icon.ps1` measures the opaque region's margins after rendering, and `window_icon_is_centered_and_not_clipped` in `src/main.rs` re-checks the copy embedded in the binary — off-centre composition or clipping turns the test red.

### A red statement has to survive the header write-back

The byte spans painted in the editor come from the same scan that produced the statements (`sql::split_ranges`), so panel *N* and span *N* cannot disagree. They are measured against the text at submit time, and that text then changes twice: the `[待执行]` → `[失败]` write-back makes the block three bytes shorter, and the user starts editing. `OpenFile::apply_edit` therefore shifts every stored span by the edit's delta and drops the ones the edit cut through, and the app re-checks a span against the text before painting it (`&text[a..b]` has to *be* that statement) — a mark that no longer matches is thrown away rather than guessed at. Error segments go at the **front** of the overlay list because `paint_overlay` is first-wins and a find highlight must not cover the line you are about to fix.

### A freshly opened egui window paints nothing

`Window::show` spends its first frames measuring and placing itself; content only appears once the size has settled. That is egui's business, but it is a trap for headless tests: the pop-out result window test has to burn two frames after the click before asserting that the grid text is on screen.

### A wide table needs its own scroll area, and the tests see the culling

`egui_extras` hard-codes its table body as `ScrollArea::new([false, vscroll])` — there is no horizontal dimension at all, so past the point where the columns stop fitting they are simply cut off. The fix is an outer `ScrollArea::horizontal()` around the *whole* table: the header is painted outside the body's scroll area, so wrapping only one of the two leaves the header's columns and the body's columns misaligned.

Inside that outer scroll area, the available width is unbounded, which rules out the obvious approaches: column widths cannot be derived from `available_width()` (they would be computed from infinity), and `TableState` freezes a resizable column's width after the first frame, so a width that adapts to the panel never arrives. Hence the widths come from the font instead — 20 `0`-glyphs to start, 8 as the floor, which is what those fixed points were really standing in for.

The same nesting changes what a headless test can observe. A cell whose clip rect has degenerated to nothing (fully scrolled out) is dropped by `Painter::add` before it ever reaches `FullOutput.shapes` — it is not painted off-screen, it does not exist in the output. So `a_wide_table_can_be_scrolled_sideways` asserts `c8` is *absent* before the wheel event and *present* after it, instead of comparing x coordinates across the two passes.

### Session state is claimed by a fingerprint, keyed by a name

A reply arrives a few hundred milliseconds later, on another thread, and by then the form may have been edited, renamed, saved or cancelled. So the session state lives in one `LinkRecord` per connection *name* — `○ 未连接` simply means the map has no record — alongside `ConnectionSpec::stamp()`, a hash of host / port / user / password / database / kind, and every renderer compares the stored stamp with the live config's before showing `✓`. Editing any field downgrades it to `⚠ 配置改过了` on the same frame, which is honest, instead of leaving a green tick on a server that has never been contacted since.

`name` is deliberately *not* in that hash, because the records are already keyed by name — hashing it too would turn a cosmetic rename into "stale" with the parameters untouched. Renaming still resets the mark, for a different and correct reason: the name is the worker thread's key, `save_draft` calls `forget()` on both the old and the new name, and a verdict may not go on living in a session that never shook hands. The hash is computed over **trimmed** fields, because saving trims them too — a fingerprint carrying the spaces the user typed would report a change that saving then erased.

### SQL highlighting borrows the `.sql` extension

`SyntaxRegistry` picks a grammar from the file's extension, and SQL mode is a property of the *tab*, not of the path on disk. Rather than threading a "treat this as SQL" flag through the highlighter and its cache key, `editor.rs` asks for the syntax of `path.with_extension("sql")` when `sql_mode` is on. The `PathBuf` never touches the filesystem — it is a lookup key only — so the cache stays keyed on something that genuinely describes the buffer.

### One header row, two directions

The file-tree header has a name that grows and three buttons that must not move, in a row egui will never wrap. Left-to-right cannot do it: the layout does not know how wide the button cluster is until it has laid it out, so a long workspace name simply draws over `新文件`. Right-to-left fixes the buttons but then the *name* is the thing being pushed, and a bare `with_layout(right_to_left)` on the panel ui also claims every pixel of height the panel has left and vertically centres the row in the middle of the sidebar.

So the row is `ui.horizontal` (one line of height) containing a right-to-left scope for the buttons, whose used rect is then subtracted from the panel width, and the name is drawn in a second scope whose `max_rect` is that leftover rectangle. The height of that rect has to be pinned by hand: `available_rect_before_wrap` returns the whole remaining panel, and egui books the child's *used* rect — give the name the panel's full height and the search box and every file row below it are pushed off-panel.

### An unreadable config is not the same as no config

`Config::load()` used to end in `Err(_) => Self::default()`, which looks robust and is the worst kind of robust: a parse failure took the same path as a first run, and the debounced write then put defaults back into the file. Whatever the user had hand-written in `db_connections` or `keymap` was gone, with nothing said about it — all it took was one typo (`"kind": "mysql"` where the enum wants `"Mysql"`).

`load()` now hands back `Loaded { config, trouble }`: the bad file is `rename`d to `config.json.broken` first (numbers appended if that name is taken), and only a successful move lets saving continue. If the move failed, or the file could not be read at all, `Trouble::blocks_save()` pins `save_now()` shut — losing this session's tweaks beats overwriting the user's own edits. The parsing lives in `load_from(&Path)` so the tests run against a temporary directory and never touch the real `%APPDATA%`; the dialog itself is covered by one of those headless interface tests built on `frame()`, which checks that it paints and that 「知道了」 dismisses it.

An empty file is treated as "nothing configured yet": sync clients and editors have both left a zero-byte `config.json` behind, and that does not deserve a dialog.

### A horizontal scroll area does not lay its contents out horizontally

The result tab bar sits in a `ScrollArea::horizontal()`, yet ten tabs stacked one per row and the strip grew taller with every run. In egui 0.37 a `ScrollArea` only scrolls — the content `Ui` keeps the layout of whatever contains it, and the bottom panel is a vertical one, so every tab got a row to itself. That also means the horizontal scrollbar never appeared: the content was never wider than the viewport, it was simply longer. The name makes it sound like the direction is part of the deal; it is not, and the row still has to ask for `ui.horizontal(...)` itself. `Layout::left_to_right` does not wrap in 0.37, so the tabs that run past the edge are exactly what that scrollbar is for.

The context menu on a tab stepped on the neighbour of a known trap: the first frame an egui popup opens is a sizing pass, which measures and paints nothing, so a headless test has to run one empty frame after the right-click before the items are on screen. Same reason as 「A freshly opened egui window paints nothing」 above.

### A cell has to take a click and still let you drag-select

Every grid cell is now a hot zone for "pick this row", while the value inside it still has to be drag-selectable — one `Response` cannot serve both. The trick is to stack the two on the same rectangle: the inner widget is `ui.put(rect, Label::selectable(true))`, which keeps owning text selection, and the outer registers `ui.interact(rect, id, Sense::click())` on top of it, which only ever records clicks. egui hands the drag to the widget that is further inside, so dragging out a word and clicking to pick a row stopped competing.

The second thing is how a click and a double-click divide the work. On the second press of a double-click egui reports `clicked()` *and* `double_clicked()` in the same frame, and the picking rules contain "clicking the already-solo row clears the selection". Testing `clicked()` first means the first press picks the row and the second press immediately undoes it — the cell window opens and the row you double-clicked is no longer selected. So the cell checks `double_clicked()` first and never enters the picking logic on that press.

And one that is hard to predict: the row window closes itself on "clicked outside the window", with an exemption — a click that lands on the very point that opened it does not count as "outside" (otherwise the opening click would close it again in the same frame). The second press of a double-click lands on exactly that point, so it falls inside the exemption and cannot close the row window by itself; the cell window therefore clears it directly, rather than leaving two copies of the same value on screen.

Each cell's `Id` comes from `ui.make_persistent_id(("db_pick_cell", row, col))`: `Id::new` is deprecated in 0.37 and `Id::unique` mints a fresh number every frame, which remembers no hover or click state at all. `make_persistent_id` is `scope_id().with(salt)`, and the bottom dock and the pop-out window have different `scope_id`s, so the same row and column in the two grids cannot tread on each other.

### The list asks how much room is left, instead of being told

The result grid used a hard-wired 120 pt: however tall you dragged the results strip, six rows lit up and the space under them stayed empty while the rest of the result sat behind the grid's own scrollbar. It now asks `ui.available_height()`. What makes that measurement mean anything is the layer outside it — `ScrollArea::vertical().auto_shrink([false, false])` — so the panel fixes its own slot first and "how much is left" answers with what the panel was given, not with what the content would like. Two numbers stay: `- 12` hands a row to this area's own horizontal scrollbar (without it the two fight over the last row), and `.max(80)` is a floor for when someone squeezes the strip down to a crack, so the table never collapses to zero rows. Going the other way, it must not simply grow with the content either — a thousand rows would be a strip thousands of points tall — so `max_scroll_height` still caps it.

### Sizes that were copied into the results strip a second time

`results.rs` used to carry its own type scale at each call site: 11 pt for a cell, 10.5 for a row number, 11.5 for the hint line, 11 for a column name, 11.5 again in the full-cell window. Each of those looks reasonable alone; together they put the whole strip a step below the rest of the window, even though `theme.rs` had already declared `Small 11.5` / `Body 13.5` / `Monospace 13.0` for everything else. Cells and the value windows now say `.monospace()`, secondary lines say `.small()`, and no size is written down — a literal is only meaningful where it is decided, and copying it somewhere else is how the two stop agreeing.

Two other numbers had to stop being literals for that to hold: the row pitch was fixed at 18 pt, which fit 11 pt text and would sit on top of the next row at 13, so it is now the measured glyph height plus 5; the columns were 140 / 56 / 46 pt, which was "twenty-three characters at 11 pt" in disguise, so they are 20 / 8 / 6 times the current `0` advance and keep the intent that was actually being expressed.

One note for whoever writes the next headless test here: `Context::default()` ships `Small` at 9 pt, not the app's 11.5, so a test that measures type sizes has to run `theme::apply_style` first or it is measuring a style nobody sees. And `ctx.fonts_mut(...)` panics with "No fonts available" before the first `run_pass`, so glyph heights can only be read after a frame has been painted.

### Making one font-size knob actually move the whole UI

After the results strip was unified, the rest of the app still held a hundred-odd `.size(11.0)` / `.size(12.5)` literals across the top bar, sidebar, tabs and windows. Each one looked fine on its own; together they were the reason "the font size only changes the editor" was true — nothing abstracted them into rungs, and the one number in `config.json` that was adjustable covered only the editor and terminal body.

The new number is `ui_font_size`, spread by `theme::apply_style` over the style table's five rungs; everywhere outside the editor is now only allowed to take a rung (`theme::sm` / `md` / `xs` / `display`, all derived from Body). What made this more than a find-and-replace was **geometry**: raise the font and the row heights, column widths, button sizes and window widths all have to follow, or the text draws on itself. So the keybinding table's two columns are now "measure the widest label in this batch at the current size", list rows take `Ui::text_style_height`, and window widths plus the top bar's wrap threshold go through `theme::scale` — leaving that at a literal 380 pt meant a large UI font pushed the right-hand group straight through the left-hand buttons.

Two traps for the tests. The `保存` button is a usable probe because its height is font-derived, so the assertion is "raise the UI font and that button grows by more than 15%, stays on screen, and no two pieces of top-bar text overlap". And headless tests read the **user's own** `config.json`, where `font_size` happens to be 15 rather than the default 14 — so "changing the UI font must not touch the editor font" has to snapshot the current value instead of comparing against `EDITOR_FONT_DEFAULT`. "The top bar fits on one row at 1400 pt" turned out to drift with the UI font too, for the same reason: any assertion that talks about concrete pixel widths has to start with `set_ui_font_size(theme::UI_FONT_DEFAULT, &ctx)` rather than hoping the user's file happens to be default.

### A slider that applies live flickers right under the pointer

The font sliders used to take effect on every drag event, and the report was that the page strobed. That is not slow drawing: `ui_font_size` drives all five style rungs, so each event re-rasterises the font atlas and re-lays out every panel — row heights, column widths, button sizes and card splits are all functions of the font — and the slider you are holding gets re-laid out from under your finger.

Dragging now writes `pending_ui_font` / `pending_font`; only the `应用` button (disabled while nothing is pending) calls the real setters, which re-apply the style and mirror the committed value back. Three details carry the weight: `恢复默认` also stages rather than commits, so "reset" is still a suggestion you have to accept; `放弃改动` **and closing the window** pull the staged values back to the live ones, because otherwise the slider would sit at a value that silently is not in effect and the next time you open the window you would be editing a ghost; and the muted line under the sliders states the pending change explicitly.

The top bar's `- 14 +` and `Ctrl+=` stayed live on purpose — one notch per press produces no event storm, and they are the controls you reach for without thinking, so routing them through a button would make them worse. Theme and word wrap likewise stay live; neither touches glyph metrics.

### Custom shells are claimed by id, not by name

The five built-in shells are a `Copy` enum and the settings could only override their paths — there was no way to get a sixth. MSYS2, Cygwin and busybox need the user to name a program, and an enum is not a closed set any more, so the model became `Shell::Builtin(ShellKind)` / `Shell::Custom(CustomShell)` and `Shell` stopped being `Copy`: selection, pane titles and `spawn` all go through clones and references now.

What actually cost something was **losing the selection while renaming**. `refresh()` recomputes the available list and then re-claims the current shell *by id*, which is why `terminal_shell` stores `custom:custom-1` instead of the display name — rename "MSYS2 Bash" to whatever you like and the picker still points at it, and the ids come from the first unused `custom-N` so deleting and re-adding cannot collide.

One deliberate counter-intuition: **a custom entry with an empty path stays in the picker.** Filtering the list down to usable entries would make the row you just added disappear before you had anywhere to type the path, so it is listed, the form says plainly that the path is still empty, and starting a terminal with it fails with the entry's name and the missing path instead of quietly spawning cmd.

### Asking "how much width is left" inside an `auto_sized` window asks for infinity

The connection manager is a `Window::auto_sized()`. Turning its form into two columns produced an immediate assertion failure inside `Ui::allocate`: `max_rect is NaN: [[NaN 666.0] - [NaN 1145.0]]`. An `auto_sized` window measures its contents first, and during that pass the content `Ui` is given **unbounded** width — so `Ui::columns(2)`, and any arithmetic subtracting from `available_width()`, computes infinity minus infinity and hands egui a NaN. Same root cause as the note above about column widths in a horizontal scroll area.

The fix is that this window's body width is a locked constant (460 pt at the default UI font, pushed into both `set_min_width` and `set_max_width`), and every column, card cell and list row is computed downward from that single number through `theme::scale` — never from what the layout claims is left. Locking the width earns its keep twice over: long paths and server error strings used to expand an `auto_sized` window until it ran off the screen.

One more thing the headless test had to learn here: an `auto_sized` window **grows** when the form opens after you click a connection, so assertions about the form have to filter by the `area_rect` read after that click. Using the rect captured before it reports every newly grown row as "not painted".

The third trap was text from *behind* the window leaking into the results. The welcome page's big headings sit inside the floating window's rect, so "no two labels in the window overlap" was failing on pairs that were never in the window. Filtering by clip rect is the right idea but not with `area.contains_rect(c.clip_rect)`: egui's clip rects only extend down-and-right to the screen edge — the window's own content pass is `[464,294] - [1400,880]` while the window rect is `[463,293] - [937,588]` — so `contains_rect` rejects the window's own text and the test flips to complaining that the list never painted `本机`. The corner you can trust is the top-left: content belonging to an area never starts its clip outside it, hence `area.contains(c.clip_rect.min)`.

## Contributing and license

- Found a bug, want a different default chord, or think a passage of this document is unclear? Open an [Issue](https://github.com/PBlueSky/easy-editor/issues).
- Before sending a PR, run `cargo test` and `cargo clippy --all-targets`; where behaviour changes, add one of those headless UI tests.

MIT, see [LICENSE](LICENSE).
