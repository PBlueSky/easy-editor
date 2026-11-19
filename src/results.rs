//! 底部结果区：一条语句一个结果面板，多个面板同时在屏上。
//!
//! 为什么不是一整块一个面板：块里三条 SELECT，第 2 条写错了，第 1 条拿到的结果
//! 还是想看的。按块只能「整块成功」或「整块报错」二选一，所以 [`crate::db::DbHub::run`]
//! 拆句后每条给一个 id，这里一个 id 挂一个面板，谁回来谁落地。
//!
//! 这一层不碰数据库也不碰文件：结果由 `DatabasePanel::take_stray` 转交过来，
//! 「回块」只是把 (路径, 字节) 原样递回给 app，真正的跳转归 app 管。
//!
//! 字号只有两档，都是问 `theme` 那张样式表要的：数据（格子、列名、详情与全文窗口
//! 里的值、语句预览）走 `TextStyle::Monospace`，辅助说明（时间戳、行数、提示、行号）
//! 走 `TextStyle::Small`。以前这一片自己写死 10.5 / 11 / 11.5，比界面其他角落矮一档，
//! 从编辑器（默认 14）抬眼往下看结果就小得发虚；行高、列宽也照旧钉死在 18 和 140，
//! 字号一动就全偏，现在这两样都从当下的字体量出来。

use std::path::PathBuf;

use egui::{Align, Color32, Layout, RichText, ScrollArea, Ui};
use egui_extras::{Column, TableBuilder};

use crate::db::{preview_of, Cell, DbEvent, DbOutcome, StatementResult};

/// 屏上同时最多留几个页签。跑几次多语句块就能把旧的挤掉，留太多没人往下看。
/// 十条是因为页签是横着排的：一次执行常见三四条，两三次执行正好铺满一行。
pub const MAX_PANELS: usize = 10;

/// 一个面板现在有什么可看的。
pub enum Slot {
    /// 送出去了，回执还没来
    Running,
    /// 排在它前面的语句失败了，这条压根没执行
    Skipped,
    Done(StatementResult),
    Failed(String),
}

impl Slot {
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running)
    }
}

/// 这批结果和文件的对应关系。
#[derive(Clone)]
pub struct Origin {
    pub path: PathBuf,
    /// 「回块」跳到这里（块头起始字节；块外选中一段就是选区起点）
    pub jump: usize,
    /// 整块提交才记下块头位置。选中一段执行是 `None`：那一段跑完不代表整块跑完，
    /// 不该把 `[待执行]` 改成 `[已执行]`。
    pub block_head: Option<usize>,
}

/// 一条交出去的语句：`task` 是它的回执 id，`range` 是它在文件里的字节区间。
///
/// 区间用来把报错的那条在编辑器里标红。同一句 SQL 在一个文件里能写两遍，
/// 光靠文本对不回位置，所以提交这一帧就得把区间记死。
pub struct Part {
    pub task: u64,
    pub stmt: String,
    pub range: Option<(usize, usize)>,
}

/// 一个语句结果面板。`task` 是 [`crate::db::DbHub::run`] 给这条语句发的 id。
pub struct Panel {
    pub task: u64,
    /// 同一次执行的面板共用一个 run 号，「关掉整块」按它收
    pub run: u64,
    pub conn: String,
    /// SQL 块标题，块外选中一段执行时是「选中内容」
    pub title: String,
    /// 这批是哪一秒提交的。十份结果挤在一条页签栏里时，这是「哪批是哪批」唯一的凭据
    pub stamp: String,
    /// 这批里的第几条（1 基）/ 一共几条
    pub seq: usize,
    pub total: usize,
    pub stmt: String,
    /// 这条语句在文件里的字节区间，报错时按它标红
    pub stmt_range: Option<(usize, usize)>,
    /// 这条语句出自哪个文件的哪个字节，用来跳回去、跑完回写状态
    pub origin: Option<Origin>,
    pub slot: Slot,
    /// 整行选中的哪几行（0 基，升序去重）。选中只是高亮，动剪贴板的是格子的右键菜单。
    ///
    /// 点行号单选、Ctrl+点 加减一行、Shift+点 连成一段，导 SQL 要的正是「这几行」。
    /// 点表格以外的地方把它清空，见 [`grid`] 末尾那段。
    pub selected: Vec<usize>,
    /// Shift+点 的起点：最近一次不带 Shift 点下去的那一行。清空选中就跟着清空。
    pub anchor: Option<usize>,
    /// 单独在右边看详情的那一行。一行一个面板：摊开的明细比表格还占地方，
    /// 塞在表格下面经常掉到裁剪区外，看着就像点了没反应。
    pub detail: Option<Detail>,
    /// 单独看的那一格（双击格子开出来的弹窗）。列太窄时整行摊开还是不好读，
    /// 就盯着一格看。
    pub peek: Option<Peek>,
}

impl Panel {
    pub fn preview(&self) -> String {
        preview_of(&self.stmt)
    }
}

/// 单独查看整行的详情。
#[derive(Clone, Copy)]
pub struct Detail {
    pub row: usize,
    /// 按下那一帧的光标位置：窗口固定在屏幕右侧，纵向跟着这一下走，之后归用户拖
    pub at: egui::Pos2,
    /// 钉住的那份详情不会被「点别处」关掉。
    ///
    /// 认这一份而不是全局开关：详情窗口是跟着面板走的，一个面板钉住不该影响
    /// 另一个面板新开的详情。
    pub pinned: bool,
    /// 详情窗口这一面摆的是 JSON 而不是逐列表单。换行时留着：
    /// 连着核几行 JSON 正是常见走法，每行都要重按一次就成负担了。
    pub json: bool,
}

/// 单独查看某一格。
#[derive(Clone, Copy)]
pub struct Peek {
    pub row: usize,
    pub col: usize,
    /// 按下那一帧的光标位置：弹窗头一次开在它边上，之后归用户拖
    pub at: egui::Pos2,
    /// 和详情窗口同一条规矩：点别处就收，钉住了才留着
    pub pinned: bool,
}

/// 一批结果全部到家，app 可以拿去做收尾（比如把块头部的 `[待执行]` 改掉）。
pub struct Settled {
    /// 这批结果出自哪个文件的哪个块头。选中一段执行的没有块头，不会出现在这里。
    pub path: PathBuf,
    pub head: usize,
    /// 整批都成功。有一条报错或有一条压根没执行，都算失败。
    pub ok: bool,
}

/// 按了「导出」攒下来的那份 CSV。文本在这儿已经排好，选路径和写文件归 app。
pub struct Export {
    /// 存盘对话框里的默认文件名
    pub name: String,
    pub csv: String,
}

/// 底部那一条结果区。
pub struct ResultsDock {
    /// 新的在最左边（页签是横排的）：一次执行 splice 到 0，同批内部仍按语句顺序。
    panels: Vec<Panel>,
    /// 页签栏当前选中哪一份结果（按语句 id）。屏上只铺这一个面板。
    ///
    /// 为什么只画一个：十个面板竖着叠，结果区再怎么拖高也只看得见头两三个，
    /// 下面的等于没有。页签横着排一次就装十条，切换只要一下。
    /// 叫 `active` 不叫 `selected`：面板里那个 `selected` 是选中的结果行，两个撞名迟早看错。
    active: Option<u64>,
    /// 上一页签栏画的时候亮着哪一份。换了才滚一下，见 `show` 里那条注释。
    active_shown: Option<u64>,
    next_run: u64,
    /// 已提交、还没收尾的那几批。`settled` 只认这里的，报过就摘掉。
    waiting: Vec<u64>,
    /// 结果区显不显示。关掉不丢结果，只是收起来。
    pub open: bool,
    /// 「回块」按下的那一帧记在这里，app 取走去做跳转。
    pub pending_jump: Option<(PathBuf, usize)>,
    /// 单独开窗口看的那份结果（按语句 id）。
    popup: Option<u64>,
    /// 「导出」按下的那一帧攒在这儿，app 每帧取走一次去弹存盘框。
    pending_export: Option<Export>,
    /// 报错语句在文件里的位置，攒着等 app 每帧取走标红。带上语句原文，
    /// app 拿它对一下区间——中间用户改过文本的话区间就不作数了。
    fails: Vec<(PathBuf, usize, usize, String)>,
    /// 导出 SQL 时要不要跳过数据库自己填的那些列（`AUTO_INCREMENT`）。
    ///
    /// 记在结果区这一份、而不是每个面板一份：勾它是为了一段连着几次的走法
    /// （把一批行倒出来喂回测试库），换一份结果就弹回默认等于每份重勾一次。
    export_skip_auto: bool,
}

impl Default for ResultsDock {
    fn default() -> Self {
        Self::new()
    }
}

impl ResultsDock {
    pub fn new() -> Self {
        Self {
            panels: Vec::new(),
            active: None,
            active_shown: None,
            next_run: 1,
            waiting: Vec::new(),
            open: false,
            pending_jump: None,
            popup: None,
            pending_export: None,
            fails: Vec::new(),
            export_skip_auto: false,
        }
    }

    /// 取走攒下的导出请求；没有就返回 None，取一次就清空。
    pub fn take_export(&mut self) -> Option<Export> {
        self.pending_export.take()
    }

    /// 报错的语句在文件里占哪几个字节。app 取走一次就清空，标红归编辑器。
    pub fn take_failures(&mut self) -> Vec<(PathBuf, usize, usize, String)> {
        std::mem::take(&mut self.fails)
    }

    pub fn is_empty(&self) -> bool {
        self.panels.is_empty()
    }

    pub fn count(&self) -> usize {
        self.panels.len()
    }

    /// 页签栏当前亮着哪一份结果（按语句 id）。
    ///
    /// 只有测试从隔壁模块读它；界面自己画的是内部那份状态。
    #[cfg(test)]
    pub fn active_task(&self) -> Option<u64> {
        self.active
    }

    /// 屏上的那几份结果，新的在前。上层只读：动列表都走这里面的方法。
    #[cfg(test)]
    pub fn panels(&self) -> &[Panel] {
        &self.panels
    }

    /// 有没有还在等的回执。app 靠它决定要不要继续重画。
    pub fn running(&self) -> bool {
        self.panels.iter().any(|p| p.slot.is_running())
    }

    /// 每条连接各自还有几条没回来。侧栏那排「执行中 N 条」用它，
    /// 自动执行那道闸也用它——连接之间互不等待，闸就不能看全局。
    pub fn busy_by_conn(&self) -> Vec<(String, usize)> {
        let mut out: Vec<(String, usize)> = Vec::new();
        for panel in &self.panels {
            if !panel.slot.is_running() {
                continue;
            }
            match out.iter_mut().find(|(conn, _)| *conn == panel.conn) {
                Some((_, n)) => *n += 1,
                None => out.push((panel.conn.clone(), 1)),
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// 这条连接此刻排着活没有。同一连接只有一个工作线程，句和句只能排队；
    /// 别的连接各走各的线程，不该被这里挡住。
    pub fn busy_on(&self, conn: &str) -> bool {
        self.panels
            .iter()
            .any(|p| p.slot.is_running() && p.conn == conn)
    }

    /// 一次提交开了哪些面板，逐条挂在这里等回执。返回这批的 run 号。
    pub fn start_run(
        &mut self,
        conn: &str,
        title: &str,
        parts: &[Part],
        origin: Option<Origin>,
    ) -> u64 {
        let run = self.next_run;
        self.next_run += 1;
        let stamp = chrono::Local::now().format("%H:%M:%S").to_string();
        let total = parts.len();
        let batch = parts.iter().enumerate().map(|(i, part)| Panel {
            task: part.task,
            run,
            conn: conn.to_string(),
            title: title.to_string(),
            stamp: stamp.clone(),
            seq: i + 1,
            total,
            stmt: part.stmt.clone(),
            stmt_range: part.range,
            origin: origin.clone(),
            slot: Slot::Running,
            selected: Vec::new(),
            anchor: None,
            detail: None,
            peek: None,
        });
        self.panels.splice(0..0, batch);
        self.waiting.push(run);
        self.trim();
        // 新交的这批直接上台：用户按完 Ctrl+Enter 要等的是它，不是上一次留下的那页
        self.active = self.panels.first().map(|p| p.task);
        self.open = true;
        run
    }

    /// 全部到家、还没收尾的那几批。同一批只报一次。
    ///
    /// 报的是「这一批的面板都不再等回执」，不是「这一批还都在屏上」：
    /// 用户中途把面板关掉就当这批不收了，免得给一个没跑完的块写 `[已执行]`。
    pub fn settled(&mut self) -> Vec<Settled> {
        let mut out = Vec::new();
        let waiting = std::mem::take(&mut self.waiting);
        for run in waiting {
            let mine: Vec<&Panel> = self.panels.iter().filter(|p| p.run == run).collect();
            if mine.iter().any(|p| p.slot.is_running()) {
                self.waiting.push(run);
                continue;
            }
            let Some(first) = mine.first() else {
                continue; // 面板被清空了，这批不再有下文
            };
            // 块外选中一段执行的没有 block_head，不动任何块的状态
            let Some(Origin {
                path,
                block_head: Some(head),
                ..
            }) = first.origin.clone()
            else {
                continue;
            };
            out.push(Settled {
                path,
                head,
                ok: mine.iter().all(|p| matches!(p.slot, Slot::Done(_))),
            });
        }
        out
    }

    /// 认领一条回执。`false` = 这个 id 不归这里，调用方可以转手。
    pub fn on_event(&mut self, event: &DbEvent) -> bool {
        let Some(index) = self.panels.iter().position(|p| p.task == event.id) else {
            return false;
        };
        match &event.outcome {
            Ok(DbOutcome::Statement(one)) => {
                self.panels[index].slot = Slot::Done(one.clone());
            }
            // 库表列表那类回执跑到结果区来了，说明上层认领错了，说清楚总比闷着
            Ok(other) => {
                self.panels[index].slot =
                    Slot::Failed(format!("不认识的返回：{}", other.describe()));
            }
            Err(err) => {
                let run = self.panels[index].run;
                let task = self.panels[index].task;
                self.panels[index].slot = Slot::Failed(err.clone());
                // 报错的那条在文件里占哪几个字节，攒给 app 标红
                if let (Some(Origin { path, .. }), Some((a, b))) = (
                    self.panels[index].origin.as_ref(),
                    self.panels[index].stmt_range,
                ) {
                    let stmt = self.panels[index].stmt.clone();
                    self.fails.push((path.clone(), a, b, stmt));
                }
                // 一条连接只有一个工作线程，前一条报错后面就不送了。
                // 不标出来的话那些面板会永远停在「执行中」。
                for panel in &mut self.panels {
                    if panel.run == run && panel.task > task && panel.slot.is_running() {
                        panel.slot = Slot::Skipped;
                    }
                }
            }
        }
        true
    }

    pub fn close(&mut self, task: u64) {
        self.panels.retain(|p| p.task != task);
    }

    pub fn close_run(&mut self, run: u64) {
        self.panels.retain(|p| p.run != run);
    }

    /// 关掉跑完的，执行中的留着——不然等回执等到一半面板没了。
    pub fn clear_finished(&mut self) {
        self.panels.retain(|p| p.slot.is_running());
    }

    fn trim(&mut self) {
        if self.panels.len() > MAX_PANELS {
            self.panels.truncate(MAX_PANELS);
        }
    }

    /// 页签选中项没了（被挤掉、被关掉）就退到第一份。
    ///
    /// 同一帧内就归位，不留「这一页什么都不画」的那一帧：从旧账翻到新页本该是一次点击的事。
    fn normalize_active(&mut self) {
        if !self
            .active
            .is_some_and(|task| self.panels.iter().any(|p| p.task == task))
        {
            self.active = self.panels.first().map(|p| p.task);
        }
    }

    pub fn show(&mut self, ui: &mut Ui, run_hint: &str) {
        let count = self.panels.len();
        let running = self.running();
        self.normalize_active();

        ui.horizontal(|ui| {
            let head = if running {
                format!("结果 · {count} 个 · 执行中…")
            } else {
                format!("结果 · {count} 个")
            };
            ui.label(RichText::new(head).small().strong());
            ui.label(weak(
                "点页签换一份 · 右键页签一次关一批 · 双击格子看全文 · 点行号选整行 · 右键一格复制或导 SQL",
            ));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if crate::theme::chip(ui, "关闭")
                    .on_hover_text("收起结果区（结果留着，顶栏「数据库 → 结果区」再打开）")
                    .clicked()
                {
                    self.open = false;
                }
                if count > 0
                    && crate::theme::chip(ui, "清空")
                        .on_hover_text("关掉跑完的面板，执行中的留着")
                        .clicked()
                {
                    self.clear_finished();
                    self.normalize_active();
                }
            });
        });
        ui.separator();

        let empty = self.is_empty();
        let mut act = Clicked {
            popup: self.popup,
            ..Default::default()
        };
        // 页签栏走在滚动区外面：它要是跟着内容一起滚，十条排开时点谁都像点在边缘上。
        if !empty {
            // 亮着的换了（新交一批、点了别的页签、这一份被关掉）才把它拉到视野里，
            // 平时不动——用户自己滚到右边看旧结果时，最不该被抢的就是滚动条。
            let into_view = if self.active != self.active_shown {
                self.active
            } else {
                None
            };
            self.active_shown = self.active;
            tab_bar(ui, &self.panels, self.active, into_view, &mut act);
            ui.separator();
        }
        // 铺满底部面板给的那一格，装不下的走这里的滚动条（面板高度用户能拖）。
        // 缩成内容高反而不行：面板先按起始尺寸预留，多出来的那一截掉在窗口下沿
        // 外面，看着就是「只画了第一个面板」。
        // 还没有结果时也得进这个 ScrollArea：底部面板是松手那一帧才按当场量到的
        // 尺寸记账的，空状态只剩一行小字的话，用户往上拖宽它就立刻回弹成那行高。
        ScrollArea::vertical()
            .id_salt("db_result_panels")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if empty {
                    ui.label(weak(format!(
                        "还没有结果。在文件里写一个 SQL 块（顶栏「数据库 → 插入 SQL 块」），光标放进哪句就按 {run_hint} 跑哪句。\
                         选中一段只跑那段，整块一起交出去用 Ctrl+Shift+Enter。"
                    )));
                    return;
                }
                let Some(index) = self
                    .active
                    .and_then(|task| self.panels.iter().position(|p| p.task == task))
                else {
                    return;
                };
                // 页签只留一份在屏上，抬头那行说的就是这一份出自哪一批
                batch_row(ui, &self.panels[index]);
                open_panel(
                    ui,
                    &mut self.panels[index],
                    &mut act,
                    &mut self.export_skip_auto,
                );
            });
        if let Some(task) = act.pick {
            self.active = Some(task);
        }

        // 渲染期间只记账，动列表在这儿
        if let Some(target) = act.jump {
            self.pending_jump = Some(target);
        }
        if let Some(task) = act.zoom_toggle {
            self.popup = if self.popup == Some(task) {
                None
            } else {
                Some(task)
            };
        }
        if act.export.is_some() {
            self.pending_export = act.export;
        }
        for task in act.closable {
            if self.popup == Some(task) {
                self.popup = None; // 面板关了，单独拉出去的那个窗口跟着收
            }
            self.close(task);
        }
        for run in act.close_runs {
            if self
                .popup
                .is_some_and(|task| self.panels.iter().any(|p| p.task == task && p.run == run))
            {
                self.popup = None;
            }
            self.close_run(run);
        }
        self.normalize_active();
        self.draw_windows(ui.ctx());
    }

    /// 单独拉出来的结果窗口：底部那一格窄，宽表放这儿看。
    ///
    /// 归结果区自己管：面板和窗口得是同一份数据，抄一份出去就等着两边对不上。
    pub fn show_popup(&mut self, ctx: &egui::Context) {
        let Some(task) = self.popup else { return };
        let Some(index) = self.panels.iter().position(|p| p.task == task) else {
            self.popup = None;
            return;
        };
        let mut open = true;
        let title = self.panels[index].title.clone();
        egui::Window::new(format!("结果 · {title}"))
            .id(egui::Id::unique(("db_result_popup", task)))
            .default_open(true)
            .default_size(egui::vec2(820.0, 460.0))
            .open(&mut open)
            .show(ctx, |ui| {
                let panel = &mut self.panels[index];
                // 抬头那一行连选中一起报，和底部面板那条同一份文案
                ui.label(weak(status_line(panel)));
                ui.separator();
                let skip_auto = &mut self.export_skip_auto;
                if matches!(&panel.slot, Slot::Done(one) if one.is_query()) {
                    // 窗口给多少用多少，不像底部那样只留五来行
                    let height = ui.available_height() - 12.0;
                    grid(ui, panel, height, skip_auto);
                } else {
                    body(ui, panel, skip_auto);
                }
            });
        if !open {
            self.popup = None;
        }
    }

    /// 网格自己开的那两类小窗：整行详情（钉在屏幕右侧）、单格全文（贴着格子）。
    ///
    /// 为什么都单开窗口：底部那一格窄，长值列宽装不下；贴在表格里展开更不行——
    /// 表格自己带裁剪，展开的那块经常在滚动区外面，等于点了没反应，收回去都做不到。
    /// 窗口只画一份、归结果区管，放大出来的那份结果和底部面板共用同一个状态。
    fn draw_windows(&mut self, ctx: &egui::Context) {
        for panel in &mut self.panels {
            // 只画还在屏上那两份（当前页签 + 放大窗口）的小窗。换页签时留着上一份的
            // 行详情，屏幕上就浮着一屏不属于当前结果的字段，看着像当前这行报的错。
            if self.active != Some(panel.task) && self.popup != Some(panel.task) {
                panel.detail = None;
                panel.peek = None;
                continue;
            }
            draw_row_detail(panel, ctx);
            draw_cell_peek(panel, ctx);
        }
    }
}

/// 详情和单格窗口共用的收尾：窗口还在不在。
///
/// 两条收法。窗口标题栏那个 ✕ 归 egui 记账（`open` 被它写成 false）；
/// 另一条是「点了别处就自己收」——详情多半只瞟一眼，看完手就移回表格，
/// 每次都要瞄着一个小 ✕ 按反而累赘。钉住的那份不参与第二条判断，
/// 因为「一边改 SQL 一边对照这几列」正是要它一直亮着的时候。
///
/// 开窗口的那一下也落在窗口外面，但它的光标位置和 `at` 是同一个值：
/// 拿这一条挡一下，否则刚点开就把自己关掉，换行时也换不过去。
fn stays_open<R>(
    ctx: &egui::Context,
    shown: Option<egui::InnerResponse<Option<R>>>,
    open: &mut bool,
    at: egui::Pos2,
    pinned: bool,
) {
    let Some(inner) = shown else { return };
    if *open && !pinned {
        let clicked = ctx.input(|i| i.pointer.button_clicked(egui::PointerButton::Primary));
        let outside = ctx
            .input(|i| i.pointer.latest_pos())
            .is_some_and(|pos| !inner.response.rect.contains(pos) && pos.distance(at) > 0.75);
        *open = !(clicked && outside);
    }
}

/// 整行详情：单独一个窗口，钉在屏幕右边，一次一行。
///
/// 纵向跟着点下去的那一下走，横向固定贴右——详情比表格还宽，压在格子上就看不见
/// 选中的是哪一行了。换一行只换内容，窗口摆哪儿归用户拖。
fn draw_row_detail(panel: &mut Panel, ctx: &egui::Context) {
    let Some(mut detail) = panel.detail else {
        return;
    };
    // 列名和值先抄成自己拥有的：对 panel.slot 的借读不能跨过整个窗口，
    // 窗口没了还要写回 panel.detail。
    let fields: Vec<Field> = match &panel.slot {
        Slot::Done(one) => one
            .rows
            .get(detail.row)
            .map(|row| row_fields(&one.columns, row))
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    if fields.is_empty() {
        panel.detail = None;
        return;
    }
    // 贴右：宽度给足才装得下「列名 = 完整值」
    let screen = ctx.viewport_rect();
    let x = (screen.max.x - 452.0).max(screen.min.x);
    let y =
        screen.min.y + (detail.at.y - screen.min.y).clamp(8.0, (screen.height() - 130.0).max(8.0));
    let title = format!("详情 · 第 {} 行", detail.row + 1);
    let mut pinned = detail.pinned;
    let mut as_json = detail.json;
    let json_text = as_json.then(|| row_json(&fields));
    let mut open = true;
    let shown = egui::Window::new(title)
        .id(egui::Id::unique(("db_row_detail", panel.task)))
        .default_pos(egui::pos2(x, y))
        .default_size(egui::vec2(430.0, 300.0))
        .open(&mut open)
        .show(ctx, |ui| {
            // 底下这排按钮先按实测高度占住底部一条，剩下的整条高度才归滚动区。
            // 之前是照 `interact_size.y + 14` 估一份预留：估少了，「滚动区 + 按钮」
            // 的总高就比窗口高出一截，egui 每帧拿这个差值去抬 desired_size，
            // 而抬完 available_height 又跟着变大——界面字号一大，看着就是窗口
            // 高度一帧一帧爬到顶满整屏。实测没有这个反馈环。
            egui::Panel::bottom("footer")
                .frame(egui::Frame::NONE)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        // 两面切换：对着表格读用表单，要拿整行喂给别的程序用 JSON
                        if ui
                            .selectable_label(!as_json, "表单")
                            .on_hover_cursor(crate::theme::HAND)
                            .clicked()
                        {
                            as_json = false;
                        }
                        if ui
                            .selectable_label(as_json, "JSON")
                            .on_hover_cursor(crate::theme::HAND)
                            .on_hover_text(
                                "整行排成一段 JSON：数值列是数字、NULL 是 null、\n值本身是对象/数组的那列摊成嵌套。列的顺序照表格。",
                            )
                            .clicked()
                        {
                            as_json = true;
                        }
                        ui.label(weak(format!("{} 列", fields.len())));
                        if as_json {
                            if crate::theme::chip(ui, "复制 JSON")
                                .on_hover_text("整行那段 JSON 原样拷进剪贴板")
                                .clicked()
                            {
                                if let Some(text) = &json_text {
                                    ui.ctx().copy_text(text.clone());
                                }
                            }
                        } else if crate::theme::chip(ui, "复制整行")
                            .on_hover_text("制表符分隔的一行，直接粘进表格软件")
                            .clicked()
                        {
                            let line = tsv_row(fields.iter().map(|f| f.text.as_str().into()));
                            ui.ctx().copy_text(line);
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.checkbox(&mut pinned, "钉住")
                                .on_hover_cursor(crate::theme::HAND)
                                .on_hover_text(
                                    "钉住：点别处它也不关。\n不勾时点了窗口外面任何一处，这一份自己收掉。",
                                );
                        });
                    });
                });
            // 纵向不跟着内容长：窗口多高就是多高，内容多了走滚动。
            // 之前给它 auto_shrink，内容高度会回灌窗口高度、每帧再靠近一点，
            // 看着就是窗口高度在那儿慢慢往上爬。
            ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if let Some(text) = &json_text {
                        ui.add(
                            egui::Label::new(RichText::new(text).monospace()).selectable(true),
                        );
                    } else {
                        for f in &fields {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(RichText::new(format!("{} =", f.name)).small().weak());
                                ui.add(
                                    egui::Label::new(RichText::new(&f.text).monospace())
                                        .selectable(true),
                                );
                            });
                            ui.add_space(1.0);
                        }
                    }
                });
        });
    detail.pinned = pinned;
    detail.json = as_json;
    stays_open(ctx, shown, &mut open, detail.at, pinned);
    panel.detail = open.then_some(detail);
}

/// 双击格子开出来的「这一格全文」小窗。
fn draw_cell_peek(panel: &mut Panel, ctx: &egui::Context) {
    let Some(mut peek) = panel.peek else { return };
    let one = match &panel.slot {
        Slot::Done(one) => one,
        _ => {
            panel.peek = None;
            return;
        }
    };
    let (Some(row), Some(column)) = (one.rows.get(peek.row), one.columns.get(peek.col)) else {
        panel.peek = None;
        return;
    };
    let value = row[peek.col].display().to_string();
    let count = value.chars().count();
    // 一格也可能是几万字节的 JSON 或日志，全铺开会把这帧拖死
    let shown: String = value.chars().take(PEEK_LIMIT).collect();
    let cut = count > PEEK_LIMIT;
    let title = format!("{} · 第 {} 行", column.name, peek.row + 1);
    let type_name = column.type_name.clone();
    let mut open = true;
    let mut pinned = peek.pinned;
    let shown = egui::Window::new(title)
        .id(egui::Id::unique((
            "db_cell_peek",
            panel.task,
            peek.row,
            peek.col,
        )))
        .default_pos(peek.at + egui::vec2(10.0, 12.0))
        .default_size(egui::vec2(460.0, 170.0))
        .open(&mut open)
        .show(ctx, |ui| {
            // 同详情窗口一条规矩：底下这排先量准了占住，剩下的高度才给滚动区，
            // 否则「滚动区 + 按钮」总高略大于窗口，egui 每帧把窗口往上抬一点。
            egui::Panel::bottom("footer")
                .frame(egui::Frame::NONE)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        if !type_name.is_empty() {
                            ui.label(weak(type_name));
                        }
                        ui.label(weak(format!("共 {count} 字")));
                        if crate::theme::chip(ui, "复制本格")
                            .on_hover_text("原文拷进剪贴板，不受上面显示长度的限制")
                            .clicked()
                        {
                            ui.ctx().copy_text(value.clone());
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.checkbox(&mut pinned, "钉住")
                                .on_hover_cursor(crate::theme::HAND)
                                .on_hover_text("钉住：点别处它也不关。\n不勾时点了窗口外面任何一处，这一格自己收掉。");
                        });
                    });
                });
            // 纵向不跟着内容长，否则窗口高度一帧一帧往上爬
            ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(RichText::new(shown).monospace()).selectable(true),
                    );
                    if cut {
                        ui.label(weak(format!(
                            "内容太长，这里只列前 {PEEK_LIMIT} 字。全部请用下面的「复制本格」或导出 CSV。"
                        )));
                    }
                });
        });
    peek.pinned = pinned;
    stays_open(ctx, shown, &mut open, peek.at, pinned);
    panel.peek = open.then_some(peek);
}

/// 一帧里从各个面板收上来的点击。渲染期间不动列表，账先记这儿。
#[derive(Default)]
struct Clicked {
    /// 按了哪个页签的 ✕，或者哪个页签要上台
    pick: Option<u64>,
    /// 这一帧该关掉的那些（✕ 一下、或者右键菜单里「关掉右侧全部」那类一次报一串）
    closable: Vec<u64>,
    close_runs: Vec<u64>,
    jump: Option<(PathBuf, usize)>,
    /// 这帧按了哪个面板的「放大」。谁都不按就是 None，窗口状态原样留着。
    zoom_toggle: Option<u64>,
    /// 当前开着窗口的那个面板，只拿来给按钮画按下态
    popup: Option<u64>,
    /// 按了「导出」，这帧现成的 CSV 文本先记下来，落盘归 app 管
    export: Option<Export>,
}

/// 一批结果上面那一行：几点跑的、发到哪、出自哪个块、一共几条。
/// 十份结果挤在一条页签栏里时，标题会重复、语句长得像，只有时间戳分得清哪批是哪批。
fn batch_row(ui: &mut Ui, panel: &Panel) {
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        let accent = ui.visuals().hyperlink_color;
        ui.label(RichText::new(&panel.stamp).small().strong().color(accent));
        ui.label(weak(format!(
            "{} · 「{}」 · 共 {} 条",
            panel.conn, panel.title, panel.total
        )));
    });
}

fn open_panel(ui: &mut Ui, panel: &mut Panel, act: &mut Clicked, skip_auto: &mut bool) {
    let dark = ui.visuals().dark_mode;
    let line = if dark {
        Color32::from_rgb(70, 76, 86)
    } else {
        Color32::from_rgb(214, 218, 224)
    };
    let inner = egui::Frame::new()
        .stroke(egui::Stroke::new(1.0, line))
        .corner_radius(4.0)
        .inner_margin(egui::Margin::symmetric(6, 2))
        .show(ui, |ui| {
            header(ui, panel, act);
            ui.separator();
            body(ui, panel, skip_auto);
        });
    // 左边一条状态色。面板连着排的时候，标题和语句都长得差不多，
    // 扫一眼颜色才分得出哪几条成了、哪条砸了。
    let rect = inner.response.rect;
    if rect.height() > 6.0 {
        let bar = egui::Rect::from_min_max(
            rect.left_top(),
            egui::pos2(rect.left() + 2.5, rect.bottom()),
        );
        ui.painter().rect_filled(bar, 2.0, dot_color(panel));
    }
}

/// 页签宽到这一格就截断：十条排开时一句长 SQL 能把整栏顶出屏。
const TAB_LIMIT: usize = 26;

/// 页签栏那一横排。
///
/// 为什么是页签：一次执行三四条语句、连跑几批就是十几份结果，竖着叠只看得见
/// 头两三个，剩下的得先拖高结果区再滚动——等于没有。横排一页签一次装十条，
/// 换一份只要一下，和 DBeaver 那个结果栏一个用法。
///
/// 为什么内容还得自己点名横排：egui 0.37 的 `ScrollArea` 只管滚，不管排布方向，
/// 内容区沿用外面那一格的竖排。`ScrollArea::horizontal()` 这个名字看着像把方向
/// 也定了，其实没有——十条页签就这样一屏一行摞起来，跑得越多栏子越高。
fn tab_bar(
    ui: &mut Ui,
    panels: &[Panel],
    active: Option<u64>,
    into_view: Option<u64>,
    act: &mut Clicked,
) {
    ScrollArea::horizontal()
        .id_salt("db_result_tabs")
        .auto_shrink([false, true])
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 3.0;
                for (index, panel) in panels.iter().enumerate() {
                    let on = active == Some(panel.task);
                    tab(
                        ui,
                        panel,
                        index,
                        panels,
                        on,
                        into_view == Some(panel.task),
                        act,
                    );
                }
            });
        });
}

/// 一个页签：状态点 + 语句预览 + ✕。
///
/// 整块矩形自己画、自己命中，为的是「亮着的那一格」和「能点的那一格」严丝合缝。
/// 用 `selectable_label` 反而不行：它选中的底色和结果表里选中行的底色是同一个，
/// 测试就分不出屏上那块亮究竟是一页签还是一行。
fn tab(
    ui: &mut Ui,
    panel: &Panel,
    index: usize,
    panels: &[Panel],
    on: bool,
    into_view: bool,
    act: &mut Clicked,
) {
    let accent = ui.visuals().hyperlink_color;
    let hovered = ui.visuals().widgets.hovered.weak_bg_fill;
    let small = egui::TextStyle::Small.resolve(ui.style());
    let text = tab_label(panel);
    let galley = ui.painter().layout_no_wrap(
        text.clone(),
        small.clone(),
        if on {
            accent
        } else {
            ui.visuals().text_color()
        },
    );
    let dot_w = 16.0;
    let close_w = 15.0;
    let size = egui::vec2(
        dot_w + galley.size().x + close_w + 8.0,
        ui.spacing().interact_size.y.max(20.0),
    );
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    let fill = if on {
        accent.gamma_multiply(0.15)
    } else if resp.hovered() {
        hovered
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, 3.0, fill);
    ui.painter().circle_filled(
        egui::pos2(rect.left() + 9.0, rect.center().y),
        3.0,
        dot_color(panel),
    );
    ui.painter().galley(
        egui::pos2(rect.left() + dot_w, rect.center().y - galley.size().y * 0.5),
        galley,
        if on {
            accent
        } else {
            ui.visuals().text_color()
        },
    );
    let close = egui::Rect::from_min_max(
        egui::pos2(rect.right() - close_w, rect.top()),
        rect.right_bottom(),
    );
    ui.painter().text(
        close.center(),
        egui::Align2::CENTER_CENTER,
        "✕",
        small,
        if resp.hovered() || on {
            ui.visuals().text_color()
        } else {
            ui.visuals().text_color().gamma_multiply(0.4)
        },
    );
    let hover = format!(
        "{}\n{}\n点一下只看这一份，点 ✕ 关掉它\n右键：一次关掉一批",
        panel.preview(),
        summary(panel)
    );
    let pos = resp.interact_pointer_pos();
    let resp = resp
        .on_hover_cursor(crate::theme::HAND)
        .on_hover_text(hover);
    if resp.clicked() {
        if pos.is_some_and(|p| close.contains(p)) {
            act.closable.push(panel.task);
        } else {
            act.pick = Some(panel.task);
        }
    }
    if into_view {
        // 一批十条时栏子放不下，新上台的那一份可能在屏外。
        // 只在「亮着的换了」那一帧滚，用户自己手滚的时候别抢。
        resp.scroll_to_me(Some(Align::Min));
    }
    resp.context_menu(|ui| tab_menu(ui, panels, index, act));
}

/// 页签菜单认的是 task，不是面板：按下的那一帧只记账，动列表等这一帧画完再说。
fn tasks<'a>(list: impl IntoIterator<Item = &'a Panel>) -> Vec<u64> {
    list.into_iter().map(|p| p.task).collect()
}

/// 页签上的右键菜单：一次关掉一批。
///
/// 为什么在这儿：一批跑完十几份结果，✕ 一个一个按太累，而「这批不要了，只留这一份
/// 对账」是连着五六下才能做完的事。左边还是右边按屏上位置算，跟新旧无关——
/// 新的永远插在最左边，所以「关掉右侧全部」正好是「关掉比它更早的那几批」。
fn tab_menu(ui: &mut Ui, panels: &[Panel], index: usize, act: &mut Clicked) {
    let me = &panels[index];
    // 菜单一拉开，标题最长那一条把它撑到底下，宽度跟着抖。先钉住。
    ui.set_min_width(150.0);
    close_row(ui, "关闭这个", "", &[me.task], act);
    close_row(
        ui,
        "关闭左侧全部",
        "这一份左边",
        &tasks(&panels[..index]),
        act,
    );
    close_row(
        ui,
        "关闭右侧全部",
        "这一份右边",
        &tasks(&panels[index + 1..]),
        act,
    );
    close_row(
        ui,
        "关闭其他",
        "除了这一份",
        &tasks(panels.iter().filter(|p| p.task != me.task)),
        act,
    );
    ui.separator();
    let batch = tasks(panels.iter().filter(|p| p.run == me.run));
    if batch.len() > 1 {
        close_row(ui, "关掉这一批", "同一次执行", &batch, act);
    }
    close_row(ui, "关闭全部", "屏上所有", &tasks(panels), act);
}

/// 菜单里的一条：能关几个就写在括号里（「关闭这个」写个（1）是废话，就不写），
/// 一个都没有就压灰——按下去没反应最招人。
fn close_row(ui: &mut Ui, text: &str, where_: &str, targets: &[u64], act: &mut Clicked) {
    let n = targets.len();
    let label = if n > 1 {
        format!("{text}（{n}）")
    } else {
        text.to_string()
    };
    let resp = ui.add_enabled(n > 0, egui::Button::new(label));
    let resp = if n == 0 {
        resp.on_disabled_hover_text(if where_.is_empty() {
            "只剩这一份了".to_string()
        } else {
            format!("{where_}已经没有结果")
        })
    } else {
        resp.on_hover_cursor(crate::theme::HAND)
    };
    if resp.clicked() {
        act.closable.extend_from_slice(targets);
        ui.close();
    }
}

/// 页签上的字：同一批的第几条，加上截到 {TAB_LIMIT} 字的语句。
fn tab_label(panel: &Panel) -> String {
    let head: String = panel.preview().chars().take(TAB_LIMIT).collect();
    let cut = panel.preview().chars().count() > TAB_LIMIT;
    let stmt = if cut { format!("{head}…") } else { head };
    if panel.total > 1 {
        format!("{}·{stmt}", panel.seq)
    } else {
        stmt
    }
}

/// 标题行。右边那组按钮先排，再把剩下的宽度整块交给左组——
/// 语句预览要的是「铺满有余」的 truncate，`ui.horizontal` 里放最后已经太晚了。
///
/// `row_h` 那块矩形是必须的：`Layout::right_to_left(Align::Center)` 在非换行布局里
/// 会把父级剩的高度整块吃掉（egui `Layout::next_frame_ignore_wrap`），一行标题
/// 直接变成一百多像素，第二个面板被顶到裁剪区外面，画都画不出来。
fn header(ui: &mut Ui, panel: &mut Panel, act: &mut Clicked) {
    let row_h = ui.text_style_height(&egui::TextStyle::Body) + 4.0;
    let width = ui.available_width().max(80.0);
    ui.allocate_ui_with_layout(
        egui::vec2(width, row_h),
        Layout::right_to_left(Align::Center),
        |ui| {
            if crate::theme::chip(ui, "✕")
                .on_hover_text("关掉这个结果面板")
                .clicked()
            {
                act.closable.push(panel.task);
            }
            if crate::theme::chip(ui, "关掉整块")
                .on_hover_text("关掉这次执行的所有面板")
                .clicked()
            {
                act.close_runs.push(panel.run);
            }
            if matches!(&panel.slot, Slot::Done(one) if one.is_query()) {
                // 窗口开着就换成「收起」，否则点了没反应的样子会让人反复按
                let text = if act.popup == Some(panel.task) { "收起" } else { "放大" };
                if crate::theme::chip(ui, text)
                    .on_hover_text("单独开一个窗口看这份结果，宽表不受底部那一格的宽度限制")
                    .clicked()
                {
                    act.zoom_toggle = Some(panel.task);
                }
                if crate::theme::chip(ui, "复制")
                    .on_hover_text("整份结果按制表符分隔拷进剪贴板，直接粘进表格软件。\n单个格子右键也有复制。")
                    .clicked()
                {
                    if let Slot::Done(one) = &panel.slot {
                        ui.ctx().copy_text(to_tsv(one));
                    }
                }
                if crate::theme::chip(ui, "导出")
                    .on_hover_text("把这份结果存成 CSV 文件")
                    .clicked()
                {
                    if let Slot::Done(one) = &panel.slot {
                        act.export = Some(Export {
                            name: csv_name(&panel.title, panel.task),
                            csv: to_csv(one),
                        });
                    }
                }
            }
            if let Some(origin) = panel.origin.clone() {
                if crate::theme::chip(ui, "回块")
                    .on_hover_text("跳回文件里这个 SQL 块")
                    .clicked()
                {
                    act.jump = Some((origin.path, origin.jump));
                }
            }
            ui.label(weak(status_line(panel)));
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                ui.painter()
                    .circle_filled(rect.center(), 3.0, dot_color(panel));
                ui.label(weak(badge(panel)));
                if panel.total > 1 {
                    // 一次跑三条，得看得出这是第几条——不然三个面板看着像同一句重复了三遍
                    ui.label(weak(format!("{}/{}", panel.seq, panel.total)));
                }
                let text = panel.preview();
                ui.add(
                    egui::Label::new(RichText::new(text).monospace())
                        .truncate()
                        .selectable(false),
                )
                .on_hover_text(panel.stmt.clone());
            });
        },
    );
}

fn body(ui: &mut Ui, panel: &mut Panel, skip_auto: &mut bool) {
    if matches!(&panel.slot, Slot::Done(one) if one.is_query()) {
        // 列表高度跟着这一格剩下的空间走，不钉死在一个数上：底栏拖到多高就能一次看
        // 多少行。以前写死 120 pt，把底栏拉高一截，多出来的地方全空着，
        // 而那份结果明明还有十几行在滚动条后面。
        // 留 12 pt 是给这一格自己的横向滚动条的，不留就两个人抢最后一行高。
        let height = (ui.available_height() - 12.0).max(80.0);
        grid(ui, panel, height, skip_auto);
        return;
    }
    match &panel.slot {
        Slot::Running => {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(11.0));
                ui.label(weak("执行中…"));
            });
        }
        Slot::Skipped => {
            ui.label(
                RichText::new("排在前面的语句失败了，这条没有执行。")
                    .small()
                    .weak()
                    .italics(),
            );
        }
        Slot::Failed(err) => {
            // 可选中：报错原文要能整段拷出去查
            ui.add(
                egui::Label::new(
                    RichText::new(err)
                        .color(Color32::from_rgb(196, 60, 52))
                        .strong(),
                )
                .selectable(true),
            );
        }
        Slot::Done(one) => {
            ui.label(write_line(one));
        }
    }
}

/// 结果网格。`height` 是这块最多能占到多高，超出行数走它自己的滚动条：
/// 底部那一格拿当下剩下的空间（结果区拖多高就看多少行），放大窗口给整屏。
/// 列比这一格还宽时走外面那根横向滚动条。
///
/// 每格都是截断显示，装不下的那一截有两条路看全：双击格子把这一格单独弹出来
/// （见 [`draw_cell_peek`]），行首 ▸ 在屏幕右边单独开一个窗口看整行
/// （见 [`draw_row_detail`]）。点行号选行——裸点一行、Ctrl+点 加减一行、Shift+点 一段，
/// 右键任意一格把这一行（或选中的那几行）复制成文本、导出成 SQL。
/// 点表格以外的地方自动收掉选中，所以没有「取消选中」那一个按钮。
///
/// `skip_auto` 是结果区那一份「导出时跳过数据库自己填的列」，右键菜单里那个勾直接改它。
fn grid(ui: &mut Ui, panel: &mut Panel, height: f32, skip_auto: &mut bool) {
    // 拆字段借：网格读 `slot` 里的结果，选中和详情要写回面板
    let Panel {
        task,
        slot,
        stmt,
        selected,
        anchor,
        detail,
        peek,
        ..
    } = panel;
    let Slot::Done(one) = slot else { return };
    let task = *task;
    let one = &*one;
    // 换了结果行数就不一样了，旧行号别留着，一点对界就是一次 panic
    selected.retain(|i| *i < one.rows.len());
    *anchor = anchor.filter(|i| *i < one.rows.len());
    *detail = detail.filter(|d| d.row < one.rows.len());

    // 字号问样式表要，行高与列宽再由这个字号量出来。以前 11 的字配 18 的行、140 的列，
    // 三个数各写各的：样式表哪天调一档，格子就压字、列就少摆几个字。
    let mono = egui::TextStyle::Monospace.resolve(ui.style());
    let ch = ui.fonts_mut(|f| f.glyph_width(&mono, '0')).max(1.0);
    let row_h = ui.text_style_height(&egui::TextStyle::Monospace) + 5.0;

    // 导出 SQL 要的表名从这条语句里认，认出来菜单里才摆导出那几条。
    // 结果里有没有数据库自己填的列（自增）也在这儿问一次，别一格一问。
    // 叫 `table_name`：`table` 已经被下面那个 `TableBuilder` 占了。
    let table_name = table_of(stmt.as_str());
    let has_auto = one.columns.iter().any(|col| col.auto_increment);

    // 横向滚动条只能自己套：egui_extras 的表体内部写死 `ScrollArea::new([false, vscroll])`，
    // 压根没有横向这一维，列一多就只能看着被裁。表头和表体一起放进这个滚动区——
    // 表头是画在表体的滚动区外面的，分开包两半会列错位。
    let before = ui.cursor();
    ScrollArea::horizontal()
        .id_salt(("db_grid_x", task))
        .auto_shrink([false, true])
        .show(ui, |ui| {
            let table = TableBuilder::new(ui)
                .id_salt(("db_result", task))
                .striped(true)
                // 这儿往里量的可用宽度是无限，横向必须缩到内容宽才有真实的溢出边界，
                // 水平滚动条才出得来（列少时也不会凭空多一根条）。纵向照旧封顶。
                .auto_shrink([true, true])
                // 行数少就照内容收，多了封顶走这块自己的滚动条。不封顶的话几份结果叠起来，
                // 结果区再怎么拖也装不下两条完整结果。
                .min_scrolled_height(0.0)
                .max_scroll_height(height)
                .cell_layout(Layout::left_to_right(Align::Center))
                // 行号那一格够摆「▸ + 三四位」，数据那格先给 20 个字宽
                .column(Column::initial(6.0 * ch).at_least(4.5 * ch).clip(true))
                .columns(
                    Column::initial(20.0 * ch)
                        .at_least(8.0 * ch)
                        .resizable(true)
                        .clip(true),
                    one.columns.len(),
                );
            table
                .header(row_h, |mut header| {
                    header.col(|ui| {
                        ui.label(RichText::new("#").monospace().strong());
                    });
                    for col in &one.columns {
                        let name = col.name.clone();
                        let ty = col.type_name.clone();
                        header.col(|ui| {
                            let resp = ui.add(
                                egui::Label::new(RichText::new(name).monospace().strong())
                                    .truncate()
                                    .selectable(false),
                            );
                            if !ty.is_empty() {
                                resp.on_hover_text(ty);
                            }
                        });
                    }
                })
                .body(|body| {
                    body.rows(row_h, one.rows.len(), |mut row| {
                        let index = row.index();
                        // `set_selected` 只管它之后加进来的格子，所以第一格之前调。
                        // 也正因为这样，点下去那一帧不改亮哪一行，下一帧才跟上。
                        row.set_selected(selected.contains(&index));
                        row.col(|ui| {
                            let opened = detail.is_some_and(|d| d.row == index);
                            let arrow = if opened { "▾" } else { "▸" };
                            let resp = ui.add(
                                egui::Label::new(RichText::new(arrow).small().weak())
                                    .selectable(false)
                                    .sense(egui::Sense::click()),
                            );
                            let at = resp
                                .interact_pointer_pos()
                                .unwrap_or_else(|| resp.rect.center());
                            if resp
                                .on_hover_cursor(crate::theme::HAND)
                                .on_hover_text("在右边单独开一个窗口看这一行")
                                .clicked()
                            {
                                // 钉住的那份换行时保持钉住：对着几行来回看正是钉的理由。
                                // JSON 那一面同理——连着核几行，不该每行重按一次。
                                let pin = detail.is_some_and(|d| d.pinned);
                                let json = detail.is_some_and(|d| d.json);
                                *detail = (!opened).then_some(Detail {
                                    row: index,
                                    at,
                                    pinned: pin,
                                    json,
                                });
                            }
                            let mut num = RichText::new(format!("{}", index + 1)).small();
                            num = if selected.contains(&index) {
                                num.strong()
                            } else {
                                num.weak()
                            };
                            let resp = ui.add(
                                egui::Label::new(num)
                                    .selectable(false)
                                    .sense(egui::Sense::click()),
                            );
                            if resp
                                .on_hover_cursor(crate::theme::HAND)
                                .on_hover_text(
                                    "点一下选中这一行\nCtrl+点：加减一行\nShift+点：从上次点的那行连成一段",
                                )
                                .clicked()
                            {
                                pick(selected, anchor, index, ui.input(|i| i.modifiers));
                            }
                        });
                        for (col_index, cell) in one.rows[index].iter().enumerate() {
                            let text = cell.display().to_string();
                            let null = cell.is_null();
                            let mut rt = RichText::new(text.clone()).monospace();
                            if null {
                                rt = rt.weak().italics();
                            }
                            row.col(|ui| {
                                let resp = ui
                                    .add(egui::Label::new(rt).truncate().selectable(true))
                                    .on_hover_cursor(egui::CursorIcon::Text);
                                // 只有真被列宽裁掉的格子才给悬浮，短值多弹一个框反而碍事。
                                // 表格只给可见行跑这段，量一行的宽度不贵。
                                let full = ui
                                    .painter()
                                    .layout_no_wrap(text.clone(), mono.clone(), Color32::WHITE)
                                    .size()
                                    .x;
                                let resp = if full <= resp.rect.width() - 6.0 {
                                    resp
                                } else {
                                    resp.on_hover_text(hover_preview(&text))
                                };
                                let at = resp
                                    .interact_pointer_pos()
                                    .unwrap_or_else(|| resp.rect.right_bottom());
                                if resp.double_clicked() {
                                    *peek = Some(Peek {
                                        row: index,
                                        col: col_index,
                                        at,
                                        pinned: peek.is_some_and(|p| p.pinned),
                                    });
                                }
                                // egui 的右键菜单默认「点一下就关」，那个「去掉自动生成列」的勾
                                // 因此会把菜单抖掉，下一次导出得重新右键。改成只认菜单外的那一下：
                                // 勾完接着点导出，动作项各自用 ui.close() 收。
                                let menu = egui::Popup::context_menu(&resp).close_behavior(
                                    egui::PopupCloseBehavior::CloseOnClickOutside,
                                );
                                menu.show(|ui| {
                                    if ui
                                        .button("查看完整内容")
                                        .on_hover_text(
                                            "单独开一个窗口放这一格：能换行、能滚动、整段选中拷贝",
                                        )
                                        .clicked()
                                    {
                                        *peek = Some(Peek {
                                            row: index,
                                            col: col_index,
                                            at,
                                            pinned: peek.is_some_and(|p| p.pinned),
                                        });
                                        ui.close();
                                    }
                                    if ui
                                        .button("复制单元格")
                                        .on_hover_cursor(crate::theme::HAND)
                                        .clicked()
                                    {
                                        ui.ctx().copy_text(text.clone());
                                        ui.close();
                                    }
                                    ui.separator();
                                    if ui
                                        .button("复制整行")
                                        .on_hover_text("制表符分隔的一行，直接粘进表格软件")
                                        .on_hover_cursor(crate::theme::HAND)
                                        .clicked()
                                    {
                                        let line =
                                            tsv_row(one.rows[index].iter().map(Cell::display));
                                        ui.ctx().copy_text(line);
                                        ui.close();
                                    }
                                    // 选中的不止这一行时，整批那一条和这一行那一条并排摆着：
                                    // 导完这一行接着导整批，不用先回表格里把行重新点一遍
                                    if selected.len() > 1 {
                                        let rows = picked_rows(one, selected);
                                        if ui
                                            .button(format!("复制选中 {} 行", rows.len()))
                                            .on_hover_text("选中的几行各占一行，还是制表符分隔")
                                            .on_hover_cursor(crate::theme::HAND)
                                            .clicked()
                                        {
                                            let body = rows
                                                .iter()
                                                .map(|row| tsv_row(row.iter().map(Cell::display)))
                                                .collect::<Vec<_>>()
                                                .join("\n");
                                            ui.ctx().copy_text(body);
                                            ui.close();
                                        }
                                    }
                                    if ui
                                        .button("复制整表")
                                        .on_hover_cursor(crate::theme::HAND)
                                        .clicked()
                                    {
                                        ui.ctx().copy_text(to_tsv(one));
                                        ui.close();
                                    }
                                    ui.separator();
                                    match &table_name {
                                        // 语句里认不出表名就只留一句说明——拼出
                                        // `INSERT INTO (SELECT` 那种半截话，比不给这一条更难查
                                        None => {
                                            ui.label(weak("认不出表名，导不了 SQL"));
                                        }
                                        Some(name) => {
                                            export_menu(
                                                ui,
                                                name,
                                                &one.columns,
                                                &[one.rows[index].as_slice()],
                                                "这一行",
                                                *skip_auto,
                                            );
                                            if selected.len() > 1 {
                                                let rows = picked_rows(one, selected);
                                                let head = format!("选中 {} 行", rows.len());
                                                export_menu(
                                                    ui, name, &one.columns, &rows, &head,
                                                    *skip_auto,
                                                );
                                            }
                                            if has_auto {
                                                ui.separator();
                                                ui.checkbox(skip_auto, "去掉自动生成列")
                                                    .on_hover_text(
                                                        "勾上后 INSERT 的列名和值、UPDATE 的 SET 里\
                                                         不再带这些列——它们的值由数据库自己填，\
                                                         写进语句里反而容易撞主键。\n\
                                                         只认 AUTO_INCREMENT：生成列和 \
                                                         DEFAULT CURRENT_TIMESTAMP 在结果元数据里\
                                                         没有标志位，认不出来，那两种得自己从语句里删。\n\
                                                         UPDATE 的 WHERE 照旧用整行的值去配，不受这个勾影响。",
                                                    );
                                            }
                                        }
                                    }
                                });
                            });
                        }
                    });
                });
        });

    // 点表格以外的地方就把选中收掉。以前那一排「取消选中」在格子底下，
    // 数据一多它自己就滚出屏了——收一次选中比重选一遍还费劲。
    // 落在弹出层上的那一下不算「点旁边」：右键菜单里点一条导出，顺手洗掉选中的话，
    // 下一条导出的就不是这批了。整行详情、单格全文那两扇同理。
    let strip = egui::Rect::from_min_max(
        before.min,
        egui::pos2(before.max.x, ui.cursor().min.y.max(before.min.y)),
    );
    let pressed = ui.ctx().input(|i| {
        i.pointer
            .button_pressed(egui::PointerButton::Primary)
            .then(|| i.pointer.latest_pos())
            .flatten()
    });
    if let Some(pos) = pressed {
        let floating = ui
            .ctx()
            .layer_id_at(pos)
            .is_some_and(|layer| layer.order != egui::Order::Background);
        if !strip.contains(pos) && !floating {
            selected.clear();
            *anchor = None;
        }
    }
    if one.truncated {
        ui.label(weak(
            "行数超过上限，只留了前面这些；要全量请在 SQL 里自己分页。",
        ));
    }
}

/// 点行号以后选中的是哪几行、Shift 的起点落在哪一行。
///
/// 三种点法各管一件事：裸点重设（再点一次同一行等于取消），Ctrl+点 加减一行，
/// Shift+点 从起点连到这一行。连着按 Shift 只换终点、起点不动，这才是表格软件里
/// 框一段的手感；起点是最近一次不带 Shift 的那一下。
fn pick(
    selected: &mut Vec<usize>,
    anchor: &mut Option<usize>,
    index: usize,
    mods: egui::Modifiers,
) {
    if mods.shift {
        let from = anchor.unwrap_or(index);
        let (lo, hi) = (from.min(index), from.max(index));
        *selected = (lo..=hi).collect();
    } else if mods.command || mods.ctrl {
        // 和 keymap 那条判定一样认两样：Windows/Linux 上 eframe 两个都置位，
        // 测试里手写的 `Modifiers::CTRL` 只有 ctrl 那一份
        match selected.iter().position(|r| *r == index) {
            Some(at) => {
                selected.remove(at);
            }
            None => {
                selected.push(index);
                selected.sort_unstable();
            }
        }
        *anchor = Some(index);
    } else if selected.as_slice() == [index] {
        selected.clear();
        *anchor = None;
    } else {
        *selected = vec![index];
        *anchor = Some(index);
    }
}

/// 抬头那一行说选中的是什么：一行报行号，几行报数量。选了 40 行还挨个列出来，
/// 只会把那一格挤跑。
fn picked_label(selected: &[usize]) -> String {
    match selected {
        [only] => format!("已选中第 {} 行", only + 1),
        many => format!("已选中 {} 行", many.len()),
    }
}

/// 选中的那几行，按行号从小到大。行号对不上号的跳过——换一份结果时旧行号可能还在。
fn picked_rows<'a>(one: &'a StatementResult, selected: &[usize]) -> Vec<&'a [Cell]> {
    selected
        .iter()
        .filter_map(|i| one.rows.get(*i).map(Vec::as_slice))
        .collect()
}

/// 右键菜单里那组导出项。`head` 说的是对象是哪几行（「这一行」或「选中 5 行」），
/// 文案和拼 SQL 都跟着它走；单行和批量两组共用这一条路。
fn export_menu(
    ui: &mut Ui,
    table: &str,
    columns: &[crate::db::Column],
    rows: &[&[Cell]],
    head: &str,
    skip_auto: bool,
) {
    // 那句「按菜单里的勾去掉了」是必要的：同一格右键，勾与不勾导出来的列不一样，
    // 不说清就像按钮坏了
    let off = if skip_auto {
        "\n自增列已按菜单下面那个勾去掉（列名和值一起少一列），WHERE 那一段不受影响。"
    } else {
        ""
    };
    if ui
        .button(format!("导出{head} INSERT"))
        .on_hover_cursor(crate::theme::HAND)
        .on_hover_text(format!(
            "把{head}拼成一条 INSERT INTO {table}（列名照结果里的顺序），拷进剪贴板。\n\
             数值列裸写、文本列带引号、NULL 就是 NULL、二进制列走 0x…。\n\
             多行合成一条语句：VALUES 后面一行一组，一次提交。{off}",
        ))
        .clicked()
    {
        ui.ctx().copy_text(insert_sql(table, columns, rows, skip_auto));
        ui.close();
    }
    if ui
        .button(format!("导出{head} UPDATE"))
        .on_hover_cursor(crate::theme::HAND)
        .on_hover_text(format!(
            "把{head}每一行各拼成一条 UPDATE，拷进剪贴板。\n\
             结果里不带主键信息，WHERE 只能拿整行的列值去配（NULL 那列写 IS NULL），\n\
             每条末尾再加 LIMIT 1 兜住多行。改过值的行导出来配不上原行，先回表格确认。{off}",
        ))
        .clicked()
    {
        ui.ctx().copy_text(update_sql(table, columns, rows, skip_auto));
        ui.close();
    }
}

/// 制表符分隔的一行。粘进表格软件天然分列，比 CSV 少一层引号转义。
fn tsv_row<'a>(cells: impl Iterator<Item = std::borrow::Cow<'a, str>>) -> String {
    let mut out = String::new();
    for (i, field) in cells.enumerate() {
        if i > 0 {
            out.push('\t');
        }
        // 制表符和换行会把一行撑成几行、把列挤歪，就地换成空格
        out.push_str(&field.replace(['\t', '\r', '\n'], " "));
    }
    out
}

/// 整份结果（含表头）排成制表符分隔，给「复制」按钮。
fn to_tsv(one: &StatementResult) -> String {
    let mut out = tsv_row(one.columns.iter().map(|c| c.name.as_str().into()));
    for row in &one.rows {
        out.push('\n');
        out.push_str(&tsv_row(row.iter().map(Cell::display)));
    }
    out
}

/// 结果排成 CSV（RFC 4180）：只拼文本，选路径和写文件归 app。
///
/// NULL 落成空字段——表格软件就这么表达空，导出成 "NULL" 反而会被当成字符串。
/// BLOB 给十六进制，网格里那句「‹N 字节›」是给人看的占位符，写进文件就是假数据。
fn to_csv(one: &StatementResult) -> String {
    let mut out = String::new();
    csv_row(&mut out, one.columns.iter().map(|c| c.name.clone()));
    for row in &one.rows {
        csv_row(
            &mut out,
            row.iter().map(|cell| match cell {
                crate::db::Cell::Null => String::new(),
                crate::db::Cell::Text(text) => text.clone(),
                crate::db::Cell::Blob(bytes) => hex(bytes),
            }),
        );
    }
    out
}

fn csv_row(out: &mut String, fields: impl Iterator<Item = String>) {
    for (i, field) in fields.enumerate() {
        if i > 0 {
            out.push(',');
        }
        // 逗号、引号、换行有一样就得整块包起来，字段里的引号翻倍
        let messy = field.contains([',', '"', '\n', '\r']);
        // 存进 Excel 会被当公式算的字段，前面钉一个单引号按文本处理。
        // 只认 = @ 开头和 + - 开头但整个不是数字的：`-2` 在 Excel 里就是个数，
        // 给它钉引号等于把整列数值导成文本，用户求和时才发现。
        let formula = field.starts_with(['=', '@', '\t'])
            || (field.starts_with(['+', '-']) && field[1..].trim().parse::<f64>().is_err());
        if formula {
            out.push('\'');
        }
        if messy || formula {
            out.push('"');
            out.push_str(&field.replace('"', "\"\""));
            out.push('"');
        } else {
            out.push_str(&field);
        }
    }
    // RFC 4180 要求 CRLF，Windows 那边的表格软件也只认这个
    out.push_str("\r\n");
}

/// 存盘框里的默认文件名。块标题可以随便写，得替文件系统把字符筛一遍。
fn csv_name(title: &str, task: u64) -> String {
    let cleaned: String = title
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let head = cleaned.trim();
    if head.is_empty() {
        format!("result_{task}.csv")
    } else {
        format!("{head}.csv")
    }
}

// ---------- 选中行导出：SQL 与 JSON ----------

/// 二进制列排成十六进制。CSV、SQL 字面量和 JSON 都用这一份。
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 这一列是不是数值类型。
///
/// `type_name` 是 mysql 那边 `ColumnType` 的 Debug 文本（`MYSQL_TYPE_LONGLONG`），
/// MockDriver 用的是短名，所以先把前缀剥掉再比。
fn numeric_type(type_name: &str) -> bool {
    let t = type_name
        .strip_prefix("MYSQL_TYPE_")
        .unwrap_or(type_name)
        .to_ascii_uppercase();
    matches!(
        t.as_str(),
        "DECIMAL" | "TINY" | "SHORT" | "LONG" | "LONGLONG" | "INT24" | "NEWDECIMAL" | "FLOAT"
            | "DOUBLE" | "YEAR" | "BIT"
    )
}

/// 一格的 SQL 字面量。
///
/// 数值列裸写、文本列带引号、NULL 就是 NULL、二进制走 `0x…`。转义按 MySQL 默认
/// sql_mode（反斜杠是转义符）来；开了 `NO_BACKSLASH_ESCAPES` 的库这两句得自己改。
fn sql_literal(cell: &Cell, type_name: &str) -> String {
    match cell {
        Cell::Null => "NULL".into(),
        Cell::Blob(bytes) => format!("0x{}", hex(bytes)),
        Cell::Text(text) if numeric_type(type_name) && text.parse::<f64>().is_ok() => {
            text.clone()
        }
        Cell::Text(text) => format!("'{}'", sql_escape(text)),
    }
}

fn sql_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        match c {
            '\'' => out.push_str("\\'"),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            // MySQL 自己认这个：读文件时 ^Z 是文件尾
            '\x1a' => out.push_str("\\Z"),
            c => out.push(c),
        }
    }
    out
}

/// 标识符两边钉反引号，`库.表` 一段一段钉，段里的反引号翻倍。
fn sql_ident(name: &str) -> String {
    name.split('.')
        .map(|part| format!("`{}`", part.replace('`', "``")))
        .collect::<Vec<_>>()
        .join(".")
}

/// 表名那一词能出现的字符。多一个少一个都算认不出来。
fn is_table_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '$' | '.' | '`')
}

/// 从语句里认出表名。
///
/// 只认 `FROM` / `UPDATE` / `INSERT INTO` 后面那一个词，取先碰到的那个。行注释先剥掉：
/// 注释掉的句子和真句子在同一份文本里，而且多半排在前面。子查询（`FROM (SELECT …)`）、
/// 多表（`FROM a, b`）、被空格断开的引号表名都归为认不出来，返回 None——
/// 这时候导出按钮该消失，而不是拼出一句 `INSERT INTO (SELECT`。
fn table_of(stmt: &str) -> Option<String> {
    let words: Vec<&str> = stmt
        .lines()
        .flat_map(|line| {
            let line = match line.find("--") {
                Some(at) => &line[..at],
                None => line,
            };
            line.split_ascii_whitespace()
        })
        .collect();
    for pair in words.windows(2) {
        // 关键字那一头要一字不差：`'from` 是字符串里的一段话，不是子句开头
        if !matches!(
            pair[0].to_ascii_lowercase().as_str(),
            "from" | "update" | "into"
        ) {
            continue;
        }
        let raw = pair[1].trim_end_matches(';');
        if raw.is_empty() || !raw.chars().all(is_table_char) {
            return None;
        }
        let parts: Vec<&str> = raw.split('.').collect();
        // 每一段都得是完整的一个词：反引号只包上一头，说明表名里带空格、
        // 被上面那次按空白拆开了——这种认不准，宁可不给
        let ok = parts.iter().all(|part| {
            let quoted = part.starts_with('`') && part.ends_with('`') && part.len() > 1;
            (quoted || !part.contains('`')) && !part.trim_matches('`').is_empty()
        });
        return ok.then(|| {
            parts
                .iter()
                .map(|part| part.trim_matches('`'))
                .collect::<Vec<_>>()
                .join(".")
        });
    }
    None
}

/// 选中的几行拼成一条 INSERT：列名和值都按结果里的列顺序。
///
/// 多行合成一条语句（`VALUES` 后一行一组），不摆几条 `INSERT` 并排——一次提交，
/// 单行时拼出来的和以前一个字不差。
///
/// `skip_auto` 是右键菜单里那个勾：数据库自己填的那些列（自增）不进列名表，
/// 值那一组也跟着少一个。全都自增时无列可写，`INSERT INTO t () VALUES ()` 不是合法
/// SQL，那就照旧全写。
fn insert_sql(
    table: &str,
    columns: &[crate::db::Column],
    rows: &[&[Cell]],
    skip_auto: bool,
) -> String {
    let keep = kept_columns(columns, skip_auto);
    let names: Vec<String> = keep.iter().map(|(_, col)| sql_ident(&col.name)).collect();
    let groups: Vec<String> = rows
        .iter()
        .map(|row| {
            let values: Vec<String> = keep
                .iter()
                .map(|(at, col)| sql_literal(&row[*at], &col.type_name))
                .collect();
            format!("({})", values.join(", "))
        })
        .collect();
    format!(
        "INSERT INTO {} ({}) VALUES {};\n",
        sql_ident(table),
        names.join(", "),
        groups.join(",\n")
    )
}

/// 选中的几行拼成 UPDATE，一行一条。
///
/// 不合成一条：一条 UPDATE 只能有一套 `SET`，几行各自的新值拼不到一块儿去。
///
/// 结果里不带主键信息（文本协议不返回索引），WHERE 只能拿整行的列值去配，NULL 那列
/// 写 `IS NULL`——`= NULL` 在 SQL 里永远不成立。整行值配不上第二行时 `LIMIT 1` 兜住。
///
/// `skip_auto` 只作用在 `SET` 那一段：那一列的值本来就由数据库填，写进去等于手动改主键。
/// WHERE 照旧用整行的值，跳过它反倒配不准行。
fn update_sql(
    table: &str,
    columns: &[crate::db::Column],
    rows: &[&[Cell]],
    skip_auto: bool,
) -> String {
    let keep = kept_columns(columns, skip_auto);
    rows.iter()
        .map(|row| {
            let sets: Vec<String> = keep
                .iter()
                .map(|(at, col)| {
                    format!(
                        "{} = {}",
                        sql_ident(&col.name),
                        sql_literal(&row[*at], &col.type_name)
                    )
                })
                .collect();
            let wheres: Vec<String> = columns
                .iter()
                .zip(*row)
                .map(|(c, cell)| {
                    if cell.is_null() {
                        format!("{} IS NULL", sql_ident(&c.name))
                    } else {
                        format!(
                            "{} = {}",
                            sql_ident(&c.name),
                            sql_literal(cell, &c.type_name)
                        )
                    }
                })
                .collect();
            format!(
                "UPDATE {} SET {} WHERE {} LIMIT 1;\n",
                sql_ident(table),
                sets.join(", "),
                wheres.join(" AND ")
            )
        })
        .collect()
}

/// 导出时要写的列：`(结果里的下标, 列)`。
///
/// 按下标留着是因为值在行里按下摆放，挑掉几列以后得知道每列取行里第几个。
fn kept_columns(
    columns: &[crate::db::Column],
    skip_auto: bool,
) -> Vec<(usize, &crate::db::Column)> {
    let keep: Vec<(usize, &crate::db::Column)> = columns
        .iter()
        .enumerate()
        .filter(|(_, col)| !(skip_auto && col.auto_increment))
        .collect();
    // 一列都不剩的话这个勾等于没勾：没有列名的 INSERT / 没有 SET 的 UPDATE 都跑不了
    if keep.is_empty() {
        columns.iter().enumerate().collect()
    } else {
        keep
    }
}

/// 详情窗口里的一栏。
///
/// 值先抄成自己拥有的：详情窗口那一整帧不能一直借着 `panel.slot`，窗口关掉还要写回
/// `panel.detail`。
struct Field {
    name: String,
    /// 表单那一面的显示文本
    text: String,
    /// JSON 那一面怎么摆这一格
    json: JsonCell,
}

/// 一格的 JSON 摆法。
enum JsonCell {
    Null,
    /// 数值列裸写：`42` 和 `"42"` 在 JSON 里是两种东西
    Number(String),
    /// 二进制列 JSON 里没有对应类型，给十六进制串
    Hex(String),
    /// 值本身是个对象或数组（MySQL 的 json 列多半长这样），摊成嵌套
    Nested(String),
    /// 其余按字符串，引号和转义交给 serde_json
    Text(String),
}

/// 一行抄成详情窗口能自己拿着用的那份。
fn row_fields(columns: &[crate::db::Column], row: &[Cell]) -> Vec<Field> {
    columns
        .iter()
        .zip(row)
        .map(|(col, cell)| Field {
            name: col.name.clone(),
            text: cell.display().to_string(),
            json: json_cell(cell, &col.type_name),
        })
        .collect()
}

fn json_cell(cell: &Cell, type_name: &str) -> JsonCell {
    match cell {
        Cell::Null => JsonCell::Null,
        Cell::Blob(bytes) => JsonCell::Hex(hex(bytes)),
        Cell::Text(text) => {
            if numeric_type(type_name) && text.parse::<f64>().is_ok() {
                return JsonCell::Number(text.clone());
            }
            // 只认对象和数组：`123`、`true` 这类裸值当字符串更贴近列里实际存的东西，
            // 一列 varchar 里全是 "1"，摆成一堆 1 反而像是在说它是数字
            match serde_json::from_str::<serde_json::Value>(text) {
                Ok(value) if value.is_object() || value.is_array() => JsonCell::Nested(
                    serde_json::to_string_pretty(&value).unwrap_or_else(|_| json_quote(text)),
                ),
                _ => JsonCell::Text(text.clone()),
            }
        }
    }
}

/// 整行排成一段带缩进的 JSON。
///
/// 顶层自己拼，不走 `serde_json::Map`：没开 `preserve_order`，它的对象按键名排序，
/// 列的顺序就没了——一行数据先按字母重排一遍，对着表格看反而更累。
fn row_json(fields: &[Field]) -> String {
    let mut out = String::from("{\n");
    for (i, f) in fields.iter().enumerate() {
        out.push_str("  ");
        out.push_str(&json_quote(&f.name));
        out.push_str(": ");
        out.push_str(&indent_tail(&json_text(&f.json), "  "));
        if i + 1 < fields.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push('}');
    out
}

/// 一个值在 JSON 里的写法，不含键名。
fn json_text(cell: &JsonCell) -> String {
    match cell {
        JsonCell::Null => "null".to_string(),
        JsonCell::Number(n) => n.clone(),
        JsonCell::Hex(h) => json_quote(h),
        JsonCell::Text(t) => json_quote(t),
        JsonCell::Nested(v) => v.clone(),
    }
}

fn json_quote(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into())
}

/// 嵌套那一段 pretty 打印从第 0 列起算，第二行起补一层缩进才不看着像塌了一半。
fn indent_tail(text: &str, pad: &str) -> String {
    if !text.contains('\n') {
        return text.to_string();
    }
    text.replace('\n', &format!("\n{pad}"))
}

/// 标题行左边那个词。查询/写操作用块标题，异常状态单独说。
fn badge(panel: &Panel) -> String {
    match &panel.slot {
        Slot::Running => "执行中".into(),
        Slot::Skipped => "未执行".into(),
        Slot::Failed(_) => "失败".into(),
        Slot::Done(_) => panel.title.clone(),
    }
}

fn dot_color(panel: &Panel) -> Color32 {
    match &panel.slot {
        Slot::Running => Color32::from_rgb(90, 130, 200),
        Slot::Skipped => Color32::from_rgb(130, 136, 146),
        Slot::Failed(_) => Color32::from_rgb(196, 60, 52),
        Slot::Done(one) if one.is_query() => Color32::from_rgb(70, 160, 100),
        Slot::Done(_) => Color32::from_rgb(200, 160, 60),
    }
}

fn summary(panel: &Panel) -> String {
    let line = match &panel.slot {
        Slot::Running => "等待回执".into(),
        Slot::Skipped => "没执行".into(),
        Slot::Failed(_) => "报错".into(),
        Slot::Done(one) if one.is_query() => {
            let mut text = format!("{} 行 · {} ms", one.row_count(), one.elapsed_ms);
            if one.truncated {
                text.push_str("（截断）");
            }
            text
        }
        Slot::Done(one) => format!("影响 {} 行 · {} ms", one.affected_rows, one.elapsed_ms),
    };
    format!("{} · {}", panel.conn, line)
}

/// 抬头那一行：结果本身，外加「现在选中的是哪几行」。
///
/// 格子底下那一排按钮撤了以后，选中只剩底色一个凭据，而右键批量导出的对象恰恰就是
/// 这几行——看不见它，导完才发现少选了一行。
fn status_line(panel: &Panel) -> String {
    let line = summary(panel);
    match &panel.slot {
        Slot::Done(one) if one.is_query() && !panel.selected.is_empty() => {
            format!("{line} · {}", picked_label(&panel.selected))
        }
        _ => line,
    }
}

fn write_line(one: &StatementResult) -> String {
    let mut text = format!("影响 {} 行", one.affected_rows);
    if one.last_insert_id > 0 {
        text.push_str(&format!("，最后插入 id {}", one.last_insert_id));
    }
    if one.warnings > 0 {
        text.push_str(&format!("，警告 {} 条", one.warnings));
    }
    text
}

fn weak(text: impl Into<String>) -> RichText {
    RichText::new(text.into()).small().weak()
}

/// 「这一格全文」窗口最多铺多少字。
const PEEK_LIMIT: usize = 20_000;

/// 悬浮框里的那一截。
///
/// 悬浮框是跟着光标走的，几万字的 JSON 全塞进去能把整片结果区盖住，
/// 所以只列个开头；要看全或要拷贝，双击格子开的那个窗口才是干这个的。
fn hover_preview(text: &str) -> String {
    const HOVER_LIMIT: usize = 400;
    let head: String = text.chars().take(HOVER_LIMIT).collect();
    if text.chars().count() > HOVER_LIMIT {
        format!("{head}…\n\n太长，悬浮框只列前 {HOVER_LIMIT} 字。双击格子开窗口看全文。")
    } else {
        format!("{head}\n\n双击格子单独看这一格。")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Cell, Column, DbError};

    fn event(id: u64, outcome: Result<DbOutcome, String>) -> DbEvent {
        DbEvent {
            id,
            conn: "本机".into(),
            dead: false,
            outcome,
        }
    }

    fn query(id: u64, rows: usize) -> DbOutcome {
        let cells = (0..rows)
            .map(|n| {
                if n == 1 {
                    vec![Cell::Text("x".into()), Cell::Null]
                } else {
                    vec![Cell::Text(format!("a{n}")), Cell::Text(format!("{n}"))]
                }
            })
            .collect();
        DbOutcome::Statement(StatementResult {
            columns: vec![
                Column {
                    name: "name".into(),
                    type_name: "varchar(64)".into(),
                    auto_increment: false,
                },
                Column {
                    name: "n".into(),
                    type_name: "int".into(),
                    auto_increment: false,
                },
            ],
            rows: cells,
            affected_rows: rows as u64,
            last_insert_id: 0,
            warnings: 0,
            truncated: false,
            elapsed_ms: 3,
            preview: format!("SELECT {id}"),
        })
    }

    fn dock() -> ResultsDock {
        let mut dock = ResultsDock::new();
        dock.start_run(
            "本机",
            "查用户",
            &parts(&["SELECT 1", "SELECT 2", "SELECT 3"]),
            Some(block_origin(12)),
        );
        dock
    }

    /// 把整个结果区画一帧。底部面板给的位置换成一块够大的中央面板，
    /// 免得网格被裁到看不见。
    fn show_pass(
        dock: &mut ResultsDock,
        ctx: &egui::Context,
        events: Vec<egui::Event>,
        time: f64,
    ) -> egui::FullOutput {
        show_pass_at(dock, ctx, events, time, egui::vec2(900.0, 620.0))
    }

    /// 上面那个的「给结果区多高说得上话」版：结果列表要跟着这一格的高度走，
    /// 只能拿两格不同高的屏幕量才知道它到底跟没跟。
    fn show_pass_at(
        dock: &mut ResultsDock,
        ctx: &egui::Context,
        events: Vec<egui::Event>,
        time: f64,
        screen: egui::Vec2,
    ) -> egui::FullOutput {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, screen)),
            time: Some(time),
            events,
            ..Default::default()
        };
        let mut out = ctx.run_pass(raw, |ctx| {
            ctx.root_ui(|ui| {
                egui::CentralPanel::default().show(ui, |ui| dock.show(ui, "Ctrl+Enter"));
            });
        });
        // 没有真的渲染器消费字体图集的增量，epaint 会在 drop 时断言
        out.textures_delta.clear();
        out
    }

    fn walk_text(shape: &egui::Shape, v: &mut Vec<(String, egui::Rect)>) {
        match shape {
            egui::Shape::Text(text) => v.push((
                text.galley.job.text.clone(),
                text.galley.rect.translate(text.pos.to_vec2()),
            )),
            egui::Shape::Vec(shapes) => {
                for s in shapes {
                    walk_text(s, v);
                }
            }
            _ => {}
        }
    }

    fn seen(out: &egui::FullOutput) -> Vec<(String, egui::Rect)> {
        let mut v = Vec::new();
        for group in &out.shapes {
            walk_text(&group.shape, &mut v);
        }
        v
    }

    /// 看得见的那几行：文字落在自己那块裁剪区里面才算数。
    ///
    /// `seen()` 只看 shape 在不在——egui 对完全裁掉的文字照样留一条 shape，
    /// 于是「列表只铺了六行、剩下的都在裁剪区外」在它眼里和四十行都在屏上一模一样。
    /// 要数这一格到底装了几行，只能连 clip 一起量。
    fn visible(out: &egui::FullOutput) -> Vec<String> {
        fn walk(shape: &egui::Shape, clip: egui::Rect, v: &mut Vec<String>) {
            match shape {
                egui::Shape::Text(text) => {
                    let rect = text.galley.rect.translate(text.pos.to_vec2());
                    if clip.intersects(rect) {
                        v.push(text.galley.job.text.clone());
                    }
                }
                egui::Shape::Vec(shapes) => {
                    for s in shapes {
                        walk(s, clip, v);
                    }
                }
                _ => {}
            }
        }
        let mut v = Vec::new();
        for group in &out.shapes {
            walk(&group.shape, group.clip_rect, &mut v);
        }
        v
    }

    /// 在结果区里连点两下某段文字（按段落中心命中），点完再多跑几帧让窗口把内容画出来。
    ///
    /// 按下、松开各占一帧，两次才算双击。Context 由调用方拿着：窗口是画在这个
    /// Context 上的， helper 里自建一个，函数一返回窗口就没了。
    fn double_click(dock: &mut ResultsDock, ctx: &egui::Context, needle: &str) -> bool {
        let pos = match seen(&show_pass(dock, ctx, vec![], 0.0))
            .into_iter()
            .find(|(t, _)| t == needle)
        {
            Some((_, rect)) => rect.center(),
            None => return false,
        };
        let at = |pressed: bool| {
            vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                },
            ]
        };
        for (step, pressed) in [(1.00, true), (1.05, false), (1.10, true), (1.15, false)] {
            show_pass(dock, ctx, at(pressed), step);
        }
        // 新开的窗口头几帧只在量尺寸、摆位置，尺寸落定才轮到画内容
        for step in [1.20, 1.25, 1.30] {
            show_pass(dock, ctx, vec![], step);
        }
        true
    }

    fn on_screen(dock: &mut ResultsDock, ctx: &egui::Context) -> Vec<String> {
        seen(&show_pass(dock, ctx, vec![], 9.0))
            .into_iter()
            .map(|(t, _)| t)
            .collect()
    }

    /// 某一帧屏上所有的字。拿已画好的 `FullOutput` 问，比 `on_screen` 多跑一帧便宜，
    /// 也不会把时间线跳到 9.0 之后去（同一轮里后面的帧数就得比它大）。
    fn texts(out: &egui::FullOutput) -> Vec<String> {
        seen(out).into_iter().map(|(t, _)| t).collect()
    }

    /// 列太窄，长值在网格里只看得到开头。双击把那一条完整内容开成一个小窗口，
    /// 里面能换行、能滚动、能整段选中——底部那一格里头没有第二个地方办得到。
    #[test]
    fn double_clicking_a_cell_puts_the_whole_value_in_a_window() {
        let mut dock = ResultsDock::new();
        let long = "备注".repeat(60);
        let batch = parts(&["SELECT note FROM t"]);
        dock.start_run("本机", "查备注", &batch, None);
        let one = table(&["note"], vec![vec![Cell::Text(long.clone())]]);
        dock.on_event(&event(1, Ok(DbOutcome::Statement(one))));

        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(1.0);
        assert!(
            double_click(&mut dock, &ctx, &long),
            "长值那一格没画出来，双击无从下手"
        );
        let shown = on_screen(&mut dock, &ctx);
        assert!(
            shown.iter().any(|t| t == "note · 第 1 行"),
            "双击没开出这一格的窗口：{shown:?}"
        );
        assert!(
            shown.iter().any(|t| t == "复制本格"),
            "窗口里没带拷贝的那一条：{shown:?}"
        );
        assert_eq!(
            shown.iter().filter(|t| *t == &long).count(),
            2,
            "一份在网格里（裁过），一份在窗口里（全文）"
        );

        // 关掉窗口的那一帧记在面板上，别留一个开着的弹窗在状态里
        dock.panels[0].peek = None;
        assert!(!on_screen(&mut dock, &ctx).contains(&"note · 第 1 行".to_string()));
    }

    /// 按一下（按下、松开各占一帧，再跑一帧让「下一帧才跟上」的高亮落地）。
    ///
    /// 返回松手那一帧的输出：剪贴板这类一次性请求只出现在处理点击的那一帧里。
    fn click(
        dock: &mut ResultsDock,
        ctx: &egui::Context,
        pos: egui::Pos2,
        t: f64,
    ) -> egui::FullOutput {
        click_mod(dock, ctx, pos, t, egui::Modifiers::NONE)
    }

    /// 按住修饰键按一下。
    ///
    /// 修饰键另外发一条 `ModifiersChanged`：egui 的 `InputState::modifiers`（界面问
    /// 「这会儿按住了什么」要的那个）只由这条更新，`PointerButton` 自带的那一份只用来
    /// 判双击。收尾再发一条空修饰，别让它一直挂到下一帧。
    fn click_mod(
        dock: &mut ResultsDock,
        ctx: &egui::Context,
        pos: egui::Pos2,
        t: f64,
        mods: egui::Modifiers,
    ) -> egui::FullOutput {
        let mut at = |pressed: bool, step: f64| {
            show_pass(
                dock,
                ctx,
                vec![
                    egui::Event::ModifiersChanged(mods),
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: mods,
                    },
                ],
                step,
            )
        };
        at(true, t);
        let out = at(false, t + 0.05);
        show_pass(
            dock,
            ctx,
            vec![egui::Event::ModifiersChanged(egui::Modifiers::NONE)],
            t + 0.10,
        );
        out
    }

    /// 右键某处，按完多跑一帧。
    ///
    /// 多出来那一帧不是保险丝：菜单开出来的第一帧 egui 只拿它量尺寸（`sizing_pass`），
    /// 一个字都不落笔，当场断言必然看不见菜单项。
    fn right_click(
        dock: &mut ResultsDock,
        ctx: &egui::Context,
        pos: egui::Pos2,
        t: f64,
    ) -> egui::FullOutput {
        let mut at = |pressed: bool, step: f64| {
            show_pass(
                dock,
                ctx,
                vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Secondary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                step,
            )
        };
        at(true, t);
        at(false, t + 0.02);
        show_pass(dock, ctx, vec![], t + 0.04)
    }

    /// 屏上某段文字最靠左的那一格。同一列里行号和格子内容会撞字，靠左的是行号。
    fn spot(out: &egui::FullOutput, needle: &str) -> Option<egui::Pos2> {
        seen(out)
            .into_iter()
            .filter(|(t, _)| t == needle)
            .min_by(|a, b| a.1.min.x.total_cmp(&b.1.min.x))
            .map(|(_, rect)| rect.center())
    }

    /// 整行选中时一格一格铺出来的选中底色有几块。
    fn lit(out: &egui::FullOutput, want: Color32) -> usize {
        fn walk(shape: &egui::Shape, want: Color32, n: &mut usize) {
            match shape {
                egui::Shape::Rect(r) if r.fill == want => *n += 1,
                egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| walk(s, want, n)),
                _ => {}
            }
        }
        let mut n = 0;
        for group in &out.shapes {
            walk(&group.shape, want, &mut n);
        }
        n
    }

    fn copied(out: &egui::FullOutput) -> Vec<String> {
        out.platform_output
            .commands
            .iter()
            .filter_map(|cmd| match cmd {
                egui::OutputCommand::CopyText(text) => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    /// 列一多，底部那一格压根装不下，而 egui_extras 的表体只给竖向滚动。
    /// 外面这层横向滚动区就是补这一维的：溢出之后滚得动，最右那一列滚得回来。
    #[test]
    fn a_wide_table_can_be_scrolled_sideways() {
        let mut dock = ResultsDock::new();
        let cols = ["c1", "c2", "c3", "c4", "c5", "c6", "c7", "c8"];
        dock.start_run("本机", "宽表", &parts(&["SELECT * FROM wide"]), None);
        let row = cols
            .iter()
            .map(|c| Cell::Text((*c).into()))
            .collect::<Vec<_>>();
        let one = table(&cols, vec![row]);
        dock.on_event(&event(1, Ok(DbOutcome::Statement(one))));

        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(1.0);
        let out = show_pass(&mut dock, &ctx, vec![], 0.0);
        let edge = 900.0;
        // 8 列 × 140 早就越过 900 宽的结果区了。裁在外面的那些压根不往 shapes 里
        // 落笔（格子 clip 后矩形为空，epaint 直接丢），所以屏上只剩左边六列。
        let shown = texts(&out);
        assert!(
            shown.contains(&"c1".to_string()),
            "第一列就该在屏上：{shown:?}"
        );
        assert!(
            !shown.contains(&"c8".to_string()),
            "最右边那一列不该还没滚就看得见：{shown:?}"
        );
        assert!(
            (1..=8).filter(|i| shown.contains(&format!("c{i}"))).count() < 8,
            "八列全挤得下，那这轮根本没造出溢出：{shown:?}"
        );

        let over = spot(&out, "c1").expect("第一列没画出来");
        let mut t = 1.0;
        show_pass(&mut dock, &ctx, vec![egui::Event::PointerMoved(over)], t);
        t += 0.1;
        // `delta` 说的是「内容往哪儿挪」，所以要往右滚得给负的 x
        show_pass(
            &mut dock,
            &ctx,
            vec![egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: egui::vec2(-600.0, 0.0),
                phase: egui::TouchPhase::Move,
                source: egui::MouseWheelSource::Wheel,
                modifiers: egui::Modifiers::NONE,
            }],
            t,
        );
        t += 0.1;
        let mut out = show_pass(&mut dock, &ctx, vec![], t);
        // 滚动带惯性，几帧才停得住
        for _ in 0..6 {
            t += 0.1;
            out = show_pass(&mut dock, &ctx, vec![], t);
        }
        let shown = texts(&out);
        assert!(
            shown.contains(&"c8".to_string()),
            "滚过去了最右那一列还是没进屏：{shown:?}"
        );
        assert!(
            !shown.contains(&"c1".to_string()),
            "滚到头了第一列还赖在屏上，说明压根没滚：{shown:?}"
        );
        let c8 = seen(&out)
            .into_iter()
            .find(|(t, _)| t == "c8")
            .map(|(_, rect)| rect.min.x)
            .expect("c8 又不见了");
        assert!(c8 < edge, "c8 落在了屏外：{c8:.0}");
    }

    /// 点行号选中整行：底色铺满这一行，抬头跟着报「已选中第几行」。
    ///
    /// 拷贝走格子上的右键菜单而不是 Ctrl+C——键盘那一下归编辑器，结果区抢过去
    /// 会把用户正打算拷的那段文字换掉。
    #[test]
    fn clicking_the_row_number_selects_the_whole_row() {
        let mut dock = ResultsDock::new();
        dock.start_run("本机", "查用户", &parts(&["SELECT note FROM t"]), None);
        let one = table(
            &["note"],
            vec![vec![Cell::Text("甲".into())], vec![Cell::Text("乙".into())]],
        );
        dock.on_event(&event(1, Ok(DbOutcome::Statement(one))));

        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(1.0);
        let mark = egui::Style::default().visuals.selection.bg_fill;
        let out = show_pass(&mut dock, &ctx, vec![], 0.0);
        assert_eq!(lit(&out, mark), 0, "一帧都没点，不该有选中底色");
        let row2 = spot(&out, "2").expect("第二行的行号没画出来");

        click(&mut dock, &ctx, row2, 1.0);
        // `set_selected` 只管它之后加进来的格子，所以要点完下一帧才亮
        let out = show_pass(&mut dock, &ctx, vec![], 1.2);
        assert!(lit(&out, mark) >= 2, "整行没亮：两格至少该铺两块底色");
        let shown = texts(&out);
        assert!(
            shown.iter().any(|t| t.contains("已选中第 2 行")),
            "选中了抬头却没报：{shown:?}"
        );

        let cell = spot(&out, "乙").expect("第二行那格没画出来");
        let out = right_click(&mut dock, &ctx, cell, 2.0);
        let pos = spot(&out, "复制整行").expect("右键菜单里没有「复制整行」");
        let out = click(&mut dock, &ctx, pos, 2.5);
        assert_eq!(
            copied(&out),
            vec!["乙".to_string()],
            "拷的该是选中那一行的原文"
        );
        assert!(
            lit(&out, mark) >= 2,
            "点了菜单里的一条就把选中洗掉了，下一条复制的就不是这批"
        );

        // 勾那种「记下选择」的条目不动菜单，动作条目还是点完就收
        let out = show_pass(&mut dock, &ctx, vec![], 2.7);
        assert_eq!(spot(&out, "复制整表"), None, "点完一条菜单还赖着不走");

        // 再点一次行号取消：底色跟着收
        click(&mut dock, &ctx, row2, 3.0);
        let out = show_pass(&mut dock, &ctx, vec![], 3.2);
        assert_eq!(lit(&out, mark), 0, "取消选中了还亮着");
        let shown = texts(&out);
        assert!(
            !shown.iter().any(|t| t.contains("已选中第 2 行")),
            "取消选中了抬头还在报：{shown:?}"
        );
    }

    /// 右键一格里就有这两个导出：表名从这条语句里认，认出来才给那两条，
    /// 拼好的 SQL 直接进剪贴板——存盘框那一套得先把「拷一段」这条路挤掉，不值。
    #[test]
    fn a_selected_row_exports_itself_as_sql() {
        let mut dock = ResultsDock::new();
        dock.start_run(
            "本机",
            "查用户",
            &parts(&["SELECT id, note FROM users"]),
            None,
        );
        let one = StatementResult {
            columns: typed(&[("id", "LONGLONG"), ("note", "VAR_STRING")]),
            rows: vec![
                vec![Cell::Text("7".into()), Cell::Text("甲".into())],
                vec![Cell::Text("9".into()), Cell::Null],
            ],
            affected_rows: 2,
            last_insert_id: 0,
            warnings: 0,
            truncated: false,
            elapsed_ms: 1,
            preview: "SELECT".into(),
        };
        dock.on_event(&event(1, Ok(DbOutcome::Statement(one))));

        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(1.0);
        let out = show_pass(&mut dock, &ctx, vec![], 0.0);
        let row2 = spot(&out, "2").expect("第二行的行号没画出来");
        click(&mut dock, &ctx, row2, 1.0);
        let out = show_pass(&mut dock, &ctx, vec![], 1.2);
        let cell = spot(&out, "9").expect("第二行那格没画出来");

        let out = right_click(&mut dock, &ctx, cell, 2.0);
        let shown = texts(&out);
        assert!(
            !shown.iter().any(|t| t.starts_with("导出选中")),
            "只选了一行，菜单里不该摆批量那两条：{shown:?}"
        );
        let at = spot(&out, "导出这一行 INSERT").expect("右键菜单里没有「导出这一行 INSERT」");
        let out = click(&mut dock, &ctx, at, 2.5);
        assert_eq!(
            copied(&out),
            vec!["INSERT INTO `users` (`id`, `note`) VALUES (9, NULL);\n".to_string()],
            "导的该是右键那一行，NULL 就是 NULL"
        );

        let out = right_click(&mut dock, &ctx, cell, 3.0);
        let at = spot(&out, "导出这一行 UPDATE").expect("右键菜单里没有「导出这一行 UPDATE」");
        let out = click(&mut dock, &ctx, at, 3.5);
        assert_eq!(
            copied(&out),
            vec![
                "UPDATE `users` SET `id` = 9, `note` = NULL \
                 WHERE `id` = 9 AND `note` IS NULL LIMIT 1;\n"
                    .to_string()
            ],
            "WHERE 里 NULL 那列得写 IS NULL"
        );
    }

    /// 三种点法各管一件事，全在 `pick` 这一处判，别散到格子的 closures 里。
    #[test]
    fn picking_rows_follows_the_three_click_modes() {
        let mut sel = Vec::new();
        let mut anchor = None;
        let none = egui::Modifiers::NONE;
        let ctrl = egui::Modifiers::CTRL;
        let shift = egui::Modifiers::SHIFT;

        pick(&mut sel, &mut anchor, 3, none);
        assert_eq!(sel, [3], "裸点只留这一行");
        assert_eq!(anchor, Some(3), "起点跟着裸点走");
        pick(&mut sel, &mut anchor, 3, none);
        assert!(sel.is_empty(), "再点同一行等于取消");
        assert_eq!(anchor, None, "取消了就不留起点");

        pick(&mut sel, &mut anchor, 6, ctrl);
        pick(&mut sel, &mut anchor, 1, ctrl);
        assert_eq!(sel, [1, 6], "Ctrl+点 加行，行号排升序");
        pick(&mut sel, &mut anchor, 6, ctrl);
        assert_eq!(sel, [1], "Ctrl+点 已有的那行是摘掉，不是再塞一份");

        pick(&mut sel, &mut anchor, 6, none);
        pick(&mut sel, &mut anchor, 2, shift);
        assert_eq!(sel, [2, 3, 4, 5, 6], "Shift+点 从起点连到终点");
        assert_eq!(anchor, Some(6), "Shift 不动起点");
        pick(&mut sel, &mut anchor, 8, shift);
        assert_eq!(sel, [6, 7, 8], "连着按 Shift 只换终点，起点还在 6");
    }

    /// 选中多行后右键：这一行和整批各摆一组导出。INSERT 合成一条、UPDATE 一行一条。
    #[test]
    fn several_selected_rows_export_together() {
        let mut dock = ResultsDock::new();
        dock.start_run(
            "本机",
            "查用户",
            &parts(&["SELECT id, note FROM users"]),
            None,
        );
        let one = StatementResult {
            columns: typed(&[("id", "LONGLONG"), ("note", "VAR_STRING")]),
            rows: vec![
                vec![Cell::Text("7".into()), Cell::Text("甲".into())],
                vec![Cell::Text("8".into()), Cell::Text("乙".into())],
                vec![Cell::Text("9".into()), Cell::Null],
                vec![Cell::Text("10".into()), Cell::Text("丁".into())],
            ],
            affected_rows: 4,
            last_insert_id: 0,
            warnings: 0,
            truncated: false,
            elapsed_ms: 1,
            preview: "SELECT".into(),
        };
        dock.on_event(&event(1, Ok(DbOutcome::Statement(one))));

        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(1.0);
        let mark = egui::Style::default().visuals.selection.bg_fill;
        let out = show_pass(&mut dock, &ctx, vec![], 0.0);
        let row2 = spot(&out, "2").expect("第二行的行号没画出来");
        let row4 = spot(&out, "4").expect("第四行的行号没画出来");
        click(&mut dock, &ctx, row2, 1.0);
        click_mod(&mut dock, &ctx, row4, 1.5, egui::Modifiers::CTRL);

        let out = show_pass(&mut dock, &ctx, vec![], 2.0);
        let shown = texts(&out);
        assert!(
            shown.iter().any(|t| t.contains("已选中 2 行")),
            "选了两行报的还是行号：{shown:?}"
        );
        assert!(
            lit(&out, mark) >= 4,
            "两行都该亮，每行两格"
        );

        let cell = spot(&out, "乙").expect("第二行那格没画出来");
        let out = right_click(&mut dock, &ctx, cell, 2.5);
        let shown = texts(&out);
        for want in [
            "导出这一行 INSERT",
            "导出这一行 UPDATE",
            "导出选中 2 行 INSERT",
            "导出选中 2 行 UPDATE",
            "复制选中 2 行",
        ] {
            assert!(
                shown.iter().any(|t| t == want),
                "选了也右键了，菜单里却没有「{want}」：{shown:?}"
            );
        }

        let at = spot(&out, "导出选中 2 行 INSERT").expect("菜单里没找到批量 INSERT");
        let out = click(&mut dock, &ctx, at, 3.0);
        assert_eq!(
            copied(&out),
            vec![
                "INSERT INTO `users` (`id`, `note`) VALUES (8, '乙'),\n(10, '丁');\n".to_string()
            ],
            "两行合成一条语句，VALUES 一行一组"
        );

        let out = right_click(&mut dock, &ctx, cell, 3.5);
        let at = spot(&out, "导出选中 2 行 UPDATE").expect("菜单里没找到批量 UPDATE");
        let out = click(&mut dock, &ctx, at, 4.0);
        assert_eq!(
            copied(&out),
            vec![
                "UPDATE `users` SET `id` = 8, `note` = '乙' WHERE `id` = 8 AND `note` = '乙' LIMIT 1;\n\
                 UPDATE `users` SET `id` = 10, `note` = '丁' WHERE `id` = 10 AND `note` = '丁' LIMIT 1;\n"
                    .to_string()
            ],
            "UPDATE 一套 SET 配一行，只能一行一条"
        );

        let out = right_click(&mut dock, &ctx, cell, 4.5);
        let at = spot(&out, "复制选中 2 行").expect("菜单里该有「复制选中 2 行」");
        let out = click(&mut dock, &ctx, at, 5.0);
        assert_eq!(
            copied(&out),
            vec!["8\t乙\n10\t丁".to_string()],
            "选中的两行各占一行，还是制表符分隔"
        );

        // 裸点把选中重设成第 4 行，再按 Shift 点第 2 行——从起点往回框一段
        click(&mut dock, &ctx, row4, 6.0);
        click_mod(&mut dock, &ctx, row2, 6.5, egui::Modifiers::SHIFT);
        let out = show_pass(&mut dock, &ctx, vec![], 7.0);
        assert!(
            texts(&out).iter().any(|t| t.contains("已选中 3 行")),
            "从第 4 行往回框到第 2 行，该是 3 行：{:?}",
            texts(&out)
        );
    }

    /// 右键菜单里那个「去掉自动生成列」：INSERT 的列名和值、UPDATE 的 SET 都少一列。
    /// WHERE 照旧用整行——那一截是拿去配行的，不是拿去改的，跟着少一列就配不准了。
    #[test]
    fn the_auto_column_switch_takes_it_out_of_the_sql() {
        let mut dock = ResultsDock::new();
        dock.start_run(
            "本机",
            "查用户",
            &parts(&["SELECT id, note FROM users"]),
            None,
        );
        let one = StatementResult {
            columns: auto_incremented(&[("id", "LONGLONG"), ("note", "VAR_STRING")], &["id"]),
            rows: vec![vec![Cell::Text("7".into()), Cell::Text("甲".into())]],
            affected_rows: 1,
            last_insert_id: 0,
            warnings: 0,
            truncated: false,
            elapsed_ms: 1,
            preview: "SELECT".into(),
        };
        dock.on_event(&event(1, Ok(DbOutcome::Statement(one))));

        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(1.0);
        let out = show_pass(&mut dock, &ctx, vec![], 0.0);
        let cell = spot(&out, "甲").expect("那一格没画出来");

        let out = right_click(&mut dock, &ctx, cell, 1.0);
        let shown = texts(&out);
        assert!(
            shown.iter().any(|t| t == "去掉自动生成列"),
            "列里有自增的，菜单就该给这个勾：{shown:?}"
        );
        let at = spot(&out, "去掉自动生成列").expect("菜单里没有那个勾");
        let out = click(&mut dock, &ctx, at, 1.5);
        // 勾一条不关菜单，接着在同一份菜单里点导出——不用重开一次
        let at = spot(&out, "导出这一行 INSERT")
            .unwrap_or_else(|| panic!("点了勾菜单就没了：{:?}", texts(&out)));
        let out = click(&mut dock, &ctx, at, 2.0);
        assert_eq!(
            copied(&out),
            vec!["INSERT INTO `users` (`note`) VALUES ('甲');\n".to_string()],
            "自增那列的列名和值都该跟着去掉：{:?}",
            texts(&out)
        );

        let out = right_click(&mut dock, &ctx, cell, 2.5);
        let at = spot(&out, "导出这一行 UPDATE").expect("菜单里没找到 UPDATE");
        let out = click(&mut dock, &ctx, at, 3.0);
        assert_eq!(
            copied(&out),
            vec![
                "UPDATE `users` SET `note` = '甲' WHERE `id` = 7 AND `note` = '甲' LIMIT 1;\n"
                    .to_string()
            ],
            "SET 里少一列，WHERE 照旧用整行"
        );
    }

    /// 表格以外那一截空白点一下就收选中。以前「取消选中」那一排在格子底下，
    /// 数据一多它自己先滚出屏，收一次选中比重选一遍还费劲。
    #[test]
    fn clicking_on_the_blank_clears_the_selection() {
        let mut dock = ResultsDock::new();
        dock.start_run("本机", "查用户", &parts(&["SELECT note FROM t"]), None);
        let one = table(
            &["note"],
            vec![vec![Cell::Text("甲".into())], vec![Cell::Text("乙".into())]],
        );
        dock.on_event(&event(1, Ok(DbOutcome::Statement(one))));

        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(1.0);
        let mark = egui::Style::default().visuals.selection.bg_fill;
        let out = show_pass(&mut dock, &ctx, vec![], 0.0);
        let row2 = spot(&out, "2").expect("第二行的行号没画出来");
        click(&mut dock, &ctx, row2, 1.0);
        let out = show_pass(&mut dock, &ctx, vec![], 1.2);
        assert!(lit(&out, mark) >= 2, "这一下先选中一行");

        // 表格底下那一截空白：两行结果，620 高的屏上剩下的都是它
        let out = click(&mut dock, &ctx, egui::pos2(300.0, 560.0), 2.0);
        assert_eq!(lit(&out, mark), 0, "点了旁边的空白却还亮着");
        let shown = texts(&out);
        assert!(
            !shown.iter().any(|t| t.contains("已选中第 2 行")),
            "选中收掉了抬头还在报：{shown:?}"
        );
    }

    /// 详情窗口的高度该一次到位，不跟着内容一帧帧往上爬。
    ///
    /// 字号推到 40 是故意的：底部那排按钮跟着长高，旧写法按 `interact_size.y + 14`
    /// 估算预留，估少的那几十字每帧加到窗口高度上，一路够到屏幕上限（egui 的
    /// `Resize` 只把 desired_size 往 `last_content_size` 上抬，而滚动区拿的高度
    /// 又跟着窗口变，就是个正反馈）。现在改成先量准底部再让滚动区吃满剩余。
    #[test]
    fn row_detail_window_height_does_not_climb() {
        let mut dock = ResultsDock::new();
        dock.start_run("本机", "查用户", &parts(&["SELECT note FROM t"]), None);
        let long = "很长的值abcdefgh".repeat(80);
        let one = table(
            &["note"],
            vec![vec![Cell::Text(long)], vec![Cell::Text("乙".into())]],
        );
        dock.on_event(&event(1, Ok(DbOutcome::Statement(one))));

        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(1.0);
        ctx.all_styles_mut(|s| {
            for sty in [
                egui::TextStyle::Body,
                egui::TextStyle::Monospace,
                egui::TextStyle::Small,
                egui::TextStyle::Button,
            ] {
                s.text_styles.insert(sty, egui::FontId::proportional(40.0));
            }
        });
        let id = egui::Id::unique(("db_row_detail", 1u64));
        let out = show_pass(&mut dock, &ctx, vec![], 0.0);
        let arrow = spot(&out, "▸").expect("没有 ▸");
        click(&mut dock, &ctx, arrow, 1.0);

        let first = ctx
            .memory(|m| m.area_rect(id))
            .expect("详情窗口没开出来");
        for i in 0..14 {
            let t = 2.0 + i as f64 * 0.1;
            show_pass(&mut dock, &ctx, vec![], t);
            let now = ctx.memory(|m| m.area_rect(id)).expect("窗口掉了");
            assert_eq!(
                now.height(),
                first.height(),
                "帧 {i} 高度从 {} 长到了 {}，又在跟着内容爬",
                first.height(),
                now.height()
            );
            assert!(
                now.height() < 0.6 * ctx.viewport_rect().height(),
                "帧 {i} 窗口高 {}，屏幕才 {}，都快顶满整个应用了",
                now.height(),
                ctx.viewport_rect().height()
            );
        }
    }

    /// 单元格全文窗口同一条规矩。
    #[test]
    fn cell_peek_window_height_does_not_climb() {
        let mut dock = ResultsDock::new();
        dock.start_run("本机", "查用户", &parts(&["SELECT note FROM t"]), None);
        let long = "很长的值abcdefgh".repeat(80);
        let one = table(&["note"], vec![vec![Cell::Text(long.clone())]]);
        dock.on_event(&event(1, Ok(DbOutcome::Statement(one))));

        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(1.0);
        ctx.all_styles_mut(|s| {
            for sty in [
                egui::TextStyle::Body,
                egui::TextStyle::Monospace,
                egui::TextStyle::Small,
                egui::TextStyle::Button,
            ] {
                s.text_styles.insert(sty, egui::FontId::proportional(40.0));
            }
        });
        assert!(
            double_click(&mut dock, &ctx, &long),
            "长值那一格没画出来，双击无从下手"
        );
        let id = egui::Id::unique(("db_cell_peek", 1u64, 0usize, 0usize));

        let first = ctx
            .memory(|m| m.area_rect(id))
            .expect("双击没开出全文窗口");
        for i in 0..14 {
            let t = 2.0 + i as f64 * 0.1;
            show_pass(&mut dock, &ctx, vec![], t);
            let now = ctx.memory(|m| m.area_rect(id)).expect("窗口掉了");
            assert_eq!(
                now.height(),
                first.height(),
                "帧 {i} 高度从 {} 长到了 {}，又在跟着内容爬",
                first.height(),
                now.height()
            );
        }
    }

    /// 语句里认不出表名（选中一段跑的、或者压根不带表名的），那两条导出就不该出现。
    #[test]
    fn no_table_name_means_no_sql_export() {
        let mut dock = ResultsDock::new();
        dock.start_run("本机", "选中内容", &parts(&["SELECT 1 + 1"]), None);
        let one = table(&["n"], vec![vec![Cell::Text("2".into())]]);
        dock.on_event(&event(1, Ok(DbOutcome::Statement(one))));

        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(1.0);
        let out = show_pass(&mut dock, &ctx, vec![], 0.0);
        let cell = spot(&out, "2").expect("那一格没画出来");
        let out = right_click(&mut dock, &ctx, cell, 1.0);
        let shown = texts(&out);
        assert!(
            shown.iter().any(|t| t == "复制整行"),
            "认不出表名不影响复制，那几条照旧：{shown:?}"
        );
        assert!(
            shown.iter().any(|t| t == "认不出表名，导不了 SQL"),
            "认不出表名时该说一句，而不是闷着不给这一条：{shown:?}"
        );
        assert_eq!(spot(&out, "导出这一行 INSERT"), None);
        assert_eq!(spot(&out, "导出这一行 UPDATE"), None);
    }

    /// 行首的 ▸ 开的是右边那个独立窗口，不往表格里塞块——塞进去那块收不回去。
    #[test]
    fn the_row_arrow_opens_a_detail_window_and_closes_it_again() {
        let mut dock = ResultsDock::new();
        dock.start_run("本机", "查用户", &parts(&["SELECT * FROM t"]), None);
        let one = table(
            &["name", "note"],
            vec![vec![
                Cell::Text("甲".into()),
                Cell::Text("很长的一段备注".into()),
            ]],
        );
        dock.on_event(&event(1, Ok(DbOutcome::Statement(one))));

        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(1.0);
        let out = show_pass(&mut dock, &ctx, vec![], 0.0);
        let arrow = seen(&out)
            .into_iter()
            .find(|(t, _)| t == "▸")
            .map(|(_, rect)| rect.center())
            .expect("行首那个 ▸ 没画出来");

        click(&mut dock, &ctx, arrow, 1.0);
        let mut out = show_pass(&mut dock, &ctx, vec![], 1.2);
        // 新开的窗口头几帧只在量尺寸、摆位置，尺寸落定才轮到画内容
        for step in [1.25, 1.3] {
            out = show_pass(&mut dock, &ctx, vec![], step);
        }
        let shown = texts(&out);
        assert!(
            shown.iter().any(|t| t == "详情 · 第 1 行"),
            "点 ▸ 没把详情窗口开出来：{shown:?}"
        );
        assert!(
            shown.iter().any(|t| t == "note =") && shown.iter().any(|t| t == "很长的一段备注"),
            "详情窗口里没摊开「列名 = 完整值」：{shown:?}"
        );
        // 详情在屏幕右边，不会把表格本身盖住
        let title = seen(&out)
            .into_iter()
            .find(|(t, _)| t == "详情 · 第 1 行")
            .map(|(_, rect)| rect)
            .expect("窗口标题没了");
        assert!(
            title.center().x > 900.0 / 2.0,
            "详情窗口该贴着屏幕右边，实测标题在 x={:.0}",
            title.center().x
        );

        // 收窗口点的是同一个位置：行首那个字换成了 ▾，但按字去找会先撞到面板
        // 标题上那枚同字的折叠钮，把整块收起来。格子没动过，坐标直接复用。
        click(&mut dock, &ctx, arrow, 2.0);
        let out = show_pass(&mut dock, &ctx, vec![], 2.2);
        let shown = texts(&out);
        assert!(
            !shown.iter().any(|t| t.starts_with("详情 · ")),
            "再点一次 ▾ 没把窗口收掉：{shown:?}"
        );
        assert!(
            dock.panels[0].detail.is_none(),
            "窗口收掉了，状态里还留着那一行"
        );
    }

    /// 详情窗口的另一面：整行排成 JSON，给别的程序用。
    #[test]
    fn the_detail_window_has_a_json_side() {
        let mut dock = ResultsDock::new();
        dock.start_run("本机", "查用户", &parts(&["SELECT * FROM t"]), None);
        let one = table(
            &["name", "note"],
            vec![vec![
                Cell::Text("甲".into()),
                Cell::Text("很长的一段备注".into()),
            ]],
        );
        dock.on_event(&event(1, Ok(DbOutcome::Statement(one))));

        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(1.0);
        let out = show_pass(&mut dock, &ctx, vec![], 0.0);
        let arrow = spot(&out, "▸").expect("行首那个 ▸ 没画出来");
        click(&mut dock, &ctx, arrow, 1.0);
        let mut out = show_pass(&mut dock, &ctx, vec![], 1.2);
        for step in [1.25, 1.3] {
            out = show_pass(&mut dock, &ctx, vec![], step);
        }
        let shown = texts(&out);
        assert!(
            shown.iter().any(|t| t == "note ="),
            "默认那一面该是逐列表单：{shown:?}"
        );

        let at = spot(&out, "JSON").expect("底下那排没有「JSON」");
        click(&mut dock, &ctx, at, 2.0);
        let out = show_pass(&mut dock, &ctx, vec![], 2.2);
        let shown = texts(&out);
        assert!(
            shown
                .iter()
                .any(|t| t.contains("\"note\": \"很长的一段备注\"")),
            "切到 JSON 那面没排出来：{shown:?}"
        );
        assert!(
            !shown.iter().any(|t| t == "note ="),
            "切过去了，表单那一面还留着"
        );
        // 切过去之后，底下那颗按钮换人高亮，点「表单」回得去
        assert!(shown.iter().any(|t| t == "表单"));

        let at = spot(&out, "复制 JSON").expect("JSON 那一面该给「复制 JSON」");
        let out = click(&mut dock, &ctx, at, 3.0);
        assert_eq!(
            copied(&out),
            vec!["{\n  \"name\": \"甲\",\n  \"note\": \"很长的一段备注\"\n}".to_string()],
            "拷出去的就是屏上那段，别再加一层引号或转义"
        );
    }

    /// 详情窗口那一帧的外框：窗口底色画出来的最大的那块矩形。屏幕底色跟它同色，
    /// 但占满整屏，按宽度和横坐标筛掉。
    fn detail_frame(out: &egui::FullOutput) -> Option<egui::Rect> {
        fn walk(shape: &egui::Shape, want: Color32, v: &mut Vec<egui::Rect>) {
            match shape {
                egui::Shape::Rect(r) if r.fill == want => v.push(r.rect),
                egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| walk(s, want, v)),
                _ => {}
            }
        }
        let want = egui::Style::default().visuals.window_fill;
        let mut v = Vec::new();
        for group in &out.shapes {
            walk(&group.shape, want, &mut v);
        }
        v.into_iter()
            .filter(|r| r.width() < 800.0 && r.min.x > 400.0)
            .max_by(|a, b| a.area().total_cmp(&b.area()))
    }

    /// 「窗口高度慢慢出现」那一次：纵向跟着内容缩，内容高度就反过来定了窗口高度——
    /// 两列的行开出来一小条，十四列的行一大条，换一行、值长到要折行，它都得重新凑
    /// 一遍，凑的活儿是按帧干的，看着就是高度在那儿慢慢长。
    /// 滚动区纵向不缩之后：窗口多高就是多高，第一帧落定，装不下的走滚动。
    #[test]
    fn the_detail_window_takes_its_size_once() {
        let mut firsts: Vec<f32> = Vec::new();
        for n in [2usize, 14] {
            let mut dock = ResultsDock::new();
            dock.start_run("本机", "查用户", &parts(&["SELECT * FROM t"]), None);
            let names: Vec<String> = (0..n).map(|i| format!("c{i}")).collect();
            let refs: Vec<&str> = names.iter().map(String::as_str).collect();
            let row = (0..n).map(|i| Cell::Text(format!("值{i}"))).collect();
            let one = table(&refs, vec![row]);
            dock.on_event(&event(1, Ok(DbOutcome::Statement(one))));

            let ctx = egui::Context::default();
            ctx.set_pixels_per_point(1.0);
            let out = show_pass(&mut dock, &ctx, vec![], 0.0);
            let arrow = spot(&out, "▸").expect("行首那个 ▸ 没画出来");
            click(&mut dock, &ctx, arrow, 1.0);

            // 1.25 是能量到外框的第一帧：窗口开出来的头几帧只在量尺寸、摆位置
            let heights: Vec<f32> = [1.25, 1.3, 1.35, 1.4, 1.5, 1.7, 2.0]
                .into_iter()
                .map(|step| detail_frame(&show_pass(&mut dock, &ctx, vec![], step)))
                .map(|r| r.expect("详情窗口的外框没画出来").height().round())
                .collect();
            assert!(
                heights.windows(2).all(|w| (w[0] - w[1]).abs() < 0.6),
                "{n} 列：窗口高度在逐帧凑，量到 {heights:?}"
            );
            firsts.push(heights[0]);
        }
        assert_eq!(
            firsts[0], firsts[1],
            "窗口高度跟着列数走：2 列 {firsts:?}——尺寸该归窗口管，内容多了走滚动"
        );
    }

    /// 整块提交：块头在第 12 字节，回块也跳那儿
    fn block_origin(head: usize) -> Origin {
        Origin {
            path: PathBuf::from("d:/a.sql"),
            jump: head,
            block_head: Some(head),
        }
    }

    /// 最后一次回执到家的那一帧，settled 才把这批报出去，而且只报一次
    #[test]
    fn settled_reports_a_finished_run_once() {
        let mut dock = dock();
        dock.on_event(&event(1, Ok(query(1, 2))));
        dock.on_event(&event(2, Ok(query(2, 2))));
        assert!(dock.settled().is_empty(), "第三条还在等，整批就还不算跑完");
        dock.on_event(&event(3, Ok(query(3, 2))));
        let done = dock.settled();
        assert_eq!(done.len(), 1);
        assert!(done[0].ok, "三条都成功");
        assert_eq!(done[0].head, 12, "回写状态要知道改哪个块头");
        assert!(dock.settled().is_empty(), "同一批不该报第二次");
    }

    #[test]
    fn settled_marks_a_run_with_a_failure_inside_it() {
        let mut dock = dock();
        // 一条连接只有一个工作线程，回执按语句顺序回来，所以 2 到家时 1 必然已经到家
        dock.on_event(&event(1, Ok(query(1, 2))));
        dock.on_event(&event(2, Err(DbError::new("断了").to_string())));
        let done = dock.settled();
        assert_eq!(
            done.len(),
            1,
            "报错会把排在后面的一起标掉，整批当场就有下文了"
        );
        assert!(!done[0].ok, "有一条失败，整块就该标 [失败]");
    }

    #[test]
    fn selection_runs_write_no_block_status() {
        let mut dock = ResultsDock::new();
        let picked = Origin {
            path: PathBuf::from("d:/a.sql"),
            jump: 40,
            block_head: None,
        };
        dock.start_run("本机", "选中内容", &parts(&["SELECT 1"]), Some(picked));
        dock.on_event(&event(1, Ok(query(1, 1))));
        assert!(
            dock.settled().is_empty(),
            "只跑了块里挑出来的一段，不该替整块宣布 [已执行]"
        );
    }

    #[test]
    fn closing_every_panel_of_a_run_drops_it() {
        let mut dock = dock();
        dock.close_run(1);
        assert!(dock.settled().is_empty(), "用户把面板关了就别再回写状态");
    }

    /// 语句 id 从 `base + 1` 递增，模拟 hub 连续发号
    fn parts(stmts: &[&str]) -> Vec<Part> {
        offset_parts(stmts, 0)
    }

    fn offset_parts(stmts: &[&str], base: u64) -> Vec<Part> {
        stmts
            .iter()
            .enumerate()
            .map(|(i, s)| Part {
                task: base + i as u64 + 1,
                stmt: s.to_string(),
                range: None,
            })
            .collect()
    }

    /// 「谁在跑」只能按连接报：每条连接一个自己的线程。
    ///
    /// 全局一个「执行中」的牌子会在两条同时跑时只亮一次，看着像乙在等甲——
    /// 侧栏那个「⏳ N」和自动执行的闸都靠这份按连接分的账。
    #[test]
    fn busy_is_accounted_per_connection() {
        let mut dock = ResultsDock::new();
        dock.start_run(
            "甲",
            "块",
            &offset_parts(&["SELECT 1", "SELECT 2"], 1),
            None,
        );
        dock.start_run("乙", "块", &offset_parts(&["SELECT 3"], 10), None);
        assert_eq!(
            dock.busy_by_conn(),
            vec![("乙".to_string(), 1), ("甲".to_string(), 2)],
            "两条连接各报各的排队数（按名字排，侧栏那一列才对得上）"
        );
        assert!(dock.busy_on("甲"));
        assert!(dock.busy_on("乙"));
        assert!(!dock.busy_on("没配过的"), "没人排队就别拦自动执行");

        dock.on_event(&event(2, Ok(query(2, 1))));
        dock.on_event(&event(11, Ok(query(11, 1))));
        assert_eq!(
            dock.busy_by_conn(),
            vec![("甲".to_string(), 1)],
            "回了执的不该还算在排队"
        );
        assert!(!dock.busy_on("乙"));
    }

    #[test]
    fn one_panel_per_statement() {
        let dock = dock();
        assert_eq!(dock.count(), 3, "三条语句就该有三个面板");
        assert!(dock.running());
        assert!(dock.open, "提交了就该把结果区亮出来");
    }

    #[test]
    fn results_land_in_their_own_panel() {
        let mut dock = dock();
        assert!(dock.on_event(&event(2, Ok(query(2, 4)))));
        // 第 2 条到家不该惊动别的面板
        assert!(dock.panels[0].slot.is_running());
        assert!(matches!(dock.panels[1].slot, Slot::Done(_)));
        assert!(dock.panels[2].slot.is_running());
        assert_eq!(summary(&dock.panels[1]), "本机 · 4 行 · 3 ms");
    }

    #[test]
    fn unknown_id_is_handed_back() {
        let mut dock = dock();
        assert!(!dock.on_event(&event(99, Ok(query(99, 1)))));
        assert!(dock.panels.iter().all(|p| p.slot.is_running()));
    }

    #[test]
    fn failure_skips_the_statements_behind_it() {
        let mut dock = dock();
        dock.on_event(&event(1, Ok(query(1, 2))));
        dock.on_event(&event(2, Err(DbError::new("1064 · 语法错了").to_string())));
        assert!(
            matches!(dock.panels[0].slot, Slot::Done(_)),
            "先跑完的得留着"
        );
        assert!(matches!(&dock.panels[1].slot, Slot::Failed(t) if t.contains("语法错了")));
        assert!(matches!(dock.panels[2].slot, Slot::Skipped));
        assert!(!dock.running(), "没跑的那些标成未执行，不许继续转圈");
    }

    #[test]
    fn newest_run_goes_on_top() {
        let mut dock = dock();
        dock.start_run("本机", "第二块", &offset_parts(&["SELECT 9"], 3), None);
        assert_eq!(dock.panels[0].title, "第二块");
        assert_eq!(dock.panels[0].task, 4);
        assert_eq!(dock.panels[1].title, "查用户", "同批内部仍按语句顺序");
        // 第一块的回执照样认领得到，只是位置被挤下去了
        assert!(dock.on_event(&event(1, Ok(query(1, 1)))));
        assert!(matches!(dock.panels[1].slot, Slot::Done(_)));
        assert_eq!(dock.count(), 4);
    }

    #[test]
    fn old_panels_get_evicted() {
        let mut dock = ResultsDock::new();
        for run in 0..5 {
            let batch: Vec<Part> = (0..3)
                .map(|i| Part {
                    task: run * 10 + i + 1,
                    stmt: format!("SELECT {run}_{i}"),
                    range: None,
                })
                .collect();
            dock.start_run("本机", "块", &batch, None);
        }
        assert_eq!(dock.count(), MAX_PANELS, "最多留 {MAX_PANELS} 个");
        assert_eq!(
            dock.panels[0].task, 41,
            "新交的排在头里，被挤掉的该是最旧的"
        );
    }

    #[test]
    fn a_failure_hands_back_the_span_it_came_from() {
        let mut dock = ResultsDock::new();
        let file = PathBuf::from("d:/a.sql");
        let owned = |stmts: &[&str]| -> Vec<Part> {
            stmts
                .iter()
                .enumerate()
                .map(|(i, s)| Part {
                    task: i as u64 + 1,
                    stmt: s.to_string(),
                    // 假装有区间：每条往后挪 10 个字节
                    range: Some((i * 10 + 7, i * 10 + 7 + s.len())),
                })
                .collect()
        };
        let origin = Origin {
            path: file.clone(),
            jump: 0,
            block_head: None,
        };
        dock.start_run(
            "本机",
            "块",
            &owned(&["SELECT 1;", "BAD SYNTAX"]),
            Some(origin),
        );
        assert!(dock.take_failures().is_empty(), "回执还没来，红底无从谈起");
        dock.on_event(&event(1, Ok(query(1, 1))));
        assert!(dock.take_failures().is_empty(), "跑对的那条不该标");
        dock.on_event(&event(2, Err(DbError::new("语法错了").to_string())));
        let fails = dock.take_failures();
        assert_eq!(fails.len(), 1);
        assert_eq!(fails[0].0, file);
        assert_eq!(fails[0].1, 17);
        assert_eq!(fails[0].2, 27);
        assert_eq!(fails[0].3, "BAD SYNTAX");
        assert!(
            dock.take_failures().is_empty(),
            "取走一次就清空，红底不能每帧重铺一遍"
        );
    }

    #[test]
    fn clear_keeps_the_waiting_ones() {
        let mut dock = dock();
        dock.on_event(&event(1, Ok(query(1, 2))));
        dock.on_event(&event(2, Err(DbError::new("断了").to_string())));
        // 1 出结果、2 报错、3 被连累标成「未执行」：这一批都不会再产出什么，清空就该全走
        dock.clear_finished();
        assert_eq!(dock.count(), 0);
        dock.start_run(
            "本机",
            "还在跑",
            &offset_parts(&["SELECT 5", "SELECT 6"], 10),
            None,
        );
        dock.on_event(&event(11, Ok(query(11, 1))));
        dock.clear_finished();
        assert_eq!(dock.count(), 1, "还在等的得留下");
        assert!(dock.panels[0].slot.is_running());
    }

    #[test]
    fn close_run_takes_the_whole_batch() {
        let mut dock = dock();
        dock.start_run("本机", "另一块", &offset_parts(&["SELECT 8"], 3), None);
        assert_eq!(dock.count(), 4);
        dock.close_run(1);
        assert_eq!(dock.count(), 1);
        assert_eq!(dock.panels[0].title, "另一块");
    }

    /// 页签换的是「屏上留哪一份」。一次跑三条，竖着叠只看得见头一两个，
    /// 剩下的得先拖高结果区再滚——等于没有。
    #[test]
    fn tabs_switch_which_result_is_shown() {
        let mut dock = dock();
        for id in 1..=3 {
            dock.on_event(&event(id, Ok(query(id, 2))));
        }
        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(1.0);

        let first = show_pass(&mut dock, &ctx, vec![], 1.0);
        assert_eq!(dock.active, Some(1), "一批结果先亮第一条");
        let shown = texts(&first);
        for label in ["1·SELECT 1", "2·SELECT 2", "3·SELECT 3"] {
            assert!(
                shown.iter().any(|t| t == label),
                "页签栏少了「{label}」：{shown:?}"
            );
        }
        // 屏上正文那一句只有一份：抬头行里的语句预览（纯 "SELECT n"）和页签的
        // 「n·SELECT n」是两个不同的字串，正好用来分辨「列在页签上」和「画在屏上」。
        let bodies: Vec<&String> = shown.iter().filter(|t| t.starts_with("SELECT ")).collect();
        assert_eq!(bodies, vec!["SELECT 1"], "页签一次只留一份在屏上");

        let at = spot(&first, "3·SELECT 3").expect("第三条那个页签没画出来");
        click(&mut dock, &ctx, at, 2.0);
        assert_eq!(dock.active, Some(3), "点页签没换过去");
        // 换页签那一帧画的还是旧的一份，新的一份下一帧才跟上
        let third = texts(&show_pass(&mut dock, &ctx, vec![], 2.20));
        let bodies: Vec<&String> = third.iter().filter(|t| t.starts_with("SELECT ")).collect();
        assert_eq!(bodies, vec!["SELECT 3"], "点完只留第三条");
    }

    fn ids(dock: &ResultsDock) -> Vec<u64> {
        dock.panels.iter().map(|p| p.task).collect()
    }

    /// 十条结果排成横的一行，不是一屏一行往下摞。
    ///
    /// 这条盯的是 `ScrollArea::horizontal()`：它只管滚，内容区沿用外面那一格的竖排，
    /// 少了里面那句 `ui.horizontal` 就是「页签越跑越多、栏子越摞越高」那个样子。
    /// 屏只有 900 宽，十条排到屏外也照排——横排不许换行，溢出交给滚动条。
    #[test]
    fn ten_tabs_line_up_in_one_row() {
        let mut dock = ResultsDock::new();
        let stmts: Vec<String> = (1..=10).map(|i| format!("SELECT {i}")).collect();
        let list: Vec<&str> = stmts.iter().map(String::as_str).collect();
        dock.start_run("本机", "查十条", &parts(&list), None);
        for id in 1..=10 {
            dock.on_event(&event(id, Ok(query(id, 1))));
        }
        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(1.0);
        let out = show_pass(&mut dock, &ctx, vec![], 1.0);

        let rows: Vec<egui::Rect> = seen(&out)
            .into_iter()
            .filter(|(t, _)| (1..=10).any(|i| *t == format!("{i}·SELECT {i}")))
            .map(|(_, rect)| rect)
            .collect();
        assert_eq!(rows.len(), 10, "十条页签没画全：{rows:?}");
        let y = rows[0].center().y;
        assert!(
            rows.iter().all(|r| (r.center().y - y).abs() < 0.5),
            "页签排成了好几行：{rows:?}"
        );
        let mut lefts: Vec<f32> = rows.iter().map(|r| r.min.x).collect();
        lefts.sort_by(f32::total_cmp);
        assert!(
            lefts.windows(2).all(|w| w[1] - w[0] > 1.0),
            "页签互相压着或者叠在同一格：{lefts:?}"
        );
    }

    /// 结果列表的高度跟着结果区那一格走，不钉死在 120 pt 上。
    ///
    /// 同一份四十行的结果，底栏给 240 和给 620 该摊开不一样的行数。以前写死一个数，
    /// 把结果区拖到多高都只亮那一小截，多出来的地方整块空着，其余的行排在
    /// 列表自己的滚动条后面——那根滚动条本来该是没地方去时才用上的。
    #[test]
    fn the_grid_takes_the_height_it_is_given() {
        let mut dock = ResultsDock::new();
        dock.start_run("本机", "查用户", &parts(&["SELECT name FROM t"]), None);
        let rows = (0..40)
            .map(|n| vec![Cell::Text(format!("行 {n}"))])
            .collect();
        dock.on_event(&event(1, Ok(DbOutcome::Statement(table(&["name"], rows)))));

        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(1.0);
        let lined_up =
            |out: &egui::FullOutput| visible(out).iter().filter(|t| t.starts_with("行 ")).count();
        let thin = show_pass_at(&mut dock, &ctx, vec![], 1.0, egui::vec2(900.0, 240.0));
        let tall = show_pass_at(&mut dock, &ctx, vec![], 2.0, egui::vec2(900.0, 620.0));
        let (short, long) = (lined_up(&thin), lined_up(&tall));
        assert!(short >= 2, "240 那一格连两行都没摊开：{short}");
        // 620 那一格扣掉抬头、页签栏、面板标题这些，剩下的全是列表的：四十行里
        // 该看得见大半（量下来是 22 行），而不是像以前那样只亮 120 pt 那六行。
        assert!(
            long >= short * 2 && long >= 18,
            "结果区高了一倍半，列表还是只亮那么几行：矮的 {short} 行 / 高的 {long} 行"
        );
    }

    /// 字号问样式表要：数据走 `Monospace`，辅助说明走 `Small`，行高跟着字号长。
    ///
    /// 这轮之前结果区自己写死 10.5 / 11 / 11.5，比界面其他角落矮一档，从编辑器
    /// （默认 14）抬眼看下来只觉得结果那一小片字发虚。这里得用应用真的那套样式量，
    /// `Context::default()` 的 `Small` 是 9pt，拿它量出来的数说明不了界面长什么样。
    #[test]
    fn the_grid_uses_the_text_sizes_the_rest_of_the_app_uses() {
        let mut dock = ResultsDock::new();
        dock.start_run("本机", "查用户", &parts(&["SELECT name FROM t"]), None);
        let rows = (0..8)
            .map(|n| vec![Cell::Text(format!("行 {n}"))])
            .collect();
        dock.on_event(&event(1, Ok(DbOutcome::Statement(table(&["name"], rows)))));

        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(1.0);
        crate::theme::apply_style(&ctx, true, crate::theme::UI_FONT_DEFAULT);
        let out = show_pass(&mut dock, &ctx, vec![], 1.0);
        // 字体的量法得等第一帧跑完才拿得到，样式表里那两档就是这一帧用的
        let style = ctx.global_style();
        let cell_font = style.text_styles[&egui::TextStyle::Monospace].clone();
        let small_font = style.text_styles[&egui::TextStyle::Small].clone();
        drop(style);
        let line = |id: &egui::FontId| ctx.fonts_mut(|f| f.row_height(id));
        assert!(
            line(&cell_font) > line(&small_font) + 0.5,
            "两档字号量出来一样高，样式表怕是没生效"
        );

        let marks = seen(&out);
        let rect_of = |want: &str| {
            marks
                .iter()
                .find(|(t, _)| t == want)
                .map(|(_, r)| *r)
                .unwrap_or_else(|| {
                    let all: Vec<&String> = marks.iter().map(|(t, _)| t).collect();
                    panic!("「{want}」没画出来：{all:?}")
                })
        };

        // 格子里的值是数据，和列名同一档
        let value = rect_of("行 0");
        assert!(
            (value.height() - line(&cell_font)).abs() < 1.0,
            "格子按 {} 排，样式表要的是 {}",
            value.height(),
            line(&cell_font)
        );
        // 行高跟着字号走，不是从前那个 18：字撑到 13 而行没动，下一行就压上来了
        let pitch = rect_of("行 1").center().y - value.center().y;
        assert!(
            pitch >= value.height() + 4.0,
            "行间距 {pitch:.1} 挤不下 {} 高的字",
            value.height()
        );

        // 辅助那一档：抬头那行提示（按开头认，省得改一个字就得同步一遍测试）
        let hint = marks
            .iter()
            .find(|(t, _)| t.starts_with("点页签换一份"))
            .map(|(_, r)| *r)
            .unwrap_or_else(|| {
                let all: Vec<&String> = marks.iter().map(|(t, _)| t).collect();
                panic!("提示行没画出来：{all:?}")
            });
        assert!(
            (hint.height() - line(&small_font)).abs() < 1.0,
            "提示行按 {} 排，样式表要的是 {}",
            hint.height(),
            line(&small_font)
        );
    }

    /// 右键页签：一次关掉一批。左还是右按屏上位置算，不按新旧——
    /// 新的永远插在最左边，所以「关掉右侧全部」等于「关掉比它更早的那几份」。
    #[test]
    fn right_clicking_a_tab_closes_a_whole_side() {
        let mut dock = dock(); // 一批三条：task 1、2、3
        for id in 1..=3 {
            dock.on_event(&event(id, Ok(query(id, 1))));
        }
        dock.start_run(
            "本机",
            "再查两条",
            &offset_parts(&["SELECT nine", "SELECT ten"], 3),
            None,
        );
        assert_eq!(ids(&dock), vec![4, 5, 1, 2, 3], "新的该排在最左边");

        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(1.0);
        let out = show_pass(&mut dock, &ctx, vec![], 1.0);
        let tab = spot(&out, "1·SELECT 1").expect("第三条那个页签没画出来");
        let out = right_click(&mut dock, &ctx, tab, 2.0);
        let shown = texts(&out);
        for want in [
            "关闭左侧全部（2）",
            "关闭右侧全部（2）",
            "关闭其他（4）",
            "关掉这一批（3）",
            "关闭全部（5）",
        ] {
            assert!(
                shown.iter().any(|t| t == want),
                "右键菜单少了「{want}」：{shown:?}"
            );
        }

        let pos = spot(&out, "关闭右侧全部（2）").expect("菜单里点不到「关闭右侧全部」");
        click(&mut dock, &ctx, pos, 3.0);
        assert_eq!(ids(&dock), vec![4, 5, 1], "右边那两份没跟着关掉");

        // 最左边那个页签左边没东西了：那一条压灰、括号里也不报数
        let out = show_pass(&mut dock, &ctx, vec![], 4.0);
        let first = spot(&out, "1·SELECT nine").expect("第一条页签没画出来");
        let shown = texts(&right_click(&mut dock, &ctx, first, 5.0));
        assert!(
            shown.iter().any(|t| t == "关闭左侧全部"),
            "没得关的时候这条也该在菜单里，只是压灰：{shown:?}"
        );
        assert!(
            !shown.iter().any(|t| t.starts_with("关闭左侧全部（")),
            "左边一个都没有还报了数：{shown:?}"
        );
    }

    /// 「关闭全部」和 ✕ 一个一个按到底是一样的收场：屏上空了，选中项也一起归零，
    /// 不留一个指着已关页签的 `active` 在状态里。
    #[test]
    fn closing_every_tab_from_the_menu_empties_the_dock() {
        let mut dock = dock();
        for id in 1..=3 {
            dock.on_event(&event(id, Ok(query(id, 1))));
        }
        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(1.0);
        let out = show_pass(&mut dock, &ctx, vec![], 1.0);
        let tab = spot(&out, "2·SELECT 2").expect("第二条那个页签没画出来");
        let out = right_click(&mut dock, &ctx, tab, 2.0);
        let pos = spot(&out, "关闭全部（3）").expect("菜单里点不到「关闭全部」");
        click(&mut dock, &ctx, pos, 3.0);
        assert!(dock.is_empty(), "说好的全部关闭，还剩 {:?}", ids(&dock));
        assert_eq!(dock.active, None, "关空了不该还指着一条");
        let shown = texts(&show_pass(&mut dock, &ctx, vec![], 4.0));
        assert!(
            shown.iter().any(|t| t.starts_with("还没有结果")),
            "关空了得回到空状态：{shown:?}"
        );
    }

    /// 页签选中项被挤掉、被关掉，同一帧就得退到还有结果的那一格，
    /// 不能留一帧「屏上什么都没有」。
    #[test]
    fn closing_the_active_result_falls_back_to_the_first() {
        let mut dock = dock();
        for id in 1..=3 {
            dock.on_event(&event(id, Ok(query(id, 2))));
        }
        dock.active = Some(2);
        dock.close(2);
        dock.normalize_active();
        assert_eq!(dock.active, Some(1), "关掉了当前那份，得自动退到第一份");

        dock.clear_finished();
        dock.normalize_active();
        assert_eq!(dock.active, None, "全清空了不该还指着一条");
    }

    #[test]
    fn write_statement_reads_as_affected_rows() {
        let mut dock = dock();
        let outcome = DbOutcome::Statement(StatementResult {
            columns: Vec::new(),
            rows: Vec::new(),
            affected_rows: 7,
            last_insert_id: 42,
            warnings: 1,
            truncated: false,
            elapsed_ms: 5,
            preview: "DELETE".into(),
        });
        dock.on_event(&event(1, Ok(outcome)));
        assert!(
            matches!(&dock.panels[0].slot, Slot::Done(one) if !one.is_query()),
            "写操作没有网格"
        );
        assert_eq!(summary(&dock.panels[0]), "本机 · 影响 7 行 · 5 ms");
        let Slot::Done(one) = &dock.panels[0].slot else {
            panic!("该有结果");
        };
        assert_eq!(write_line(one), "影响 7 行，最后插入 id 42，警告 1 条");
    }

    fn table(columns: &[&str], rows: Vec<Vec<Cell>>) -> StatementResult {
        StatementResult {
            columns: columns
                .iter()
                .map(|name| Column {
                    name: (*name).to_string(),
                    type_name: String::new(),
                    auto_increment: false,
                })
                .collect(),
            affected_rows: rows.len() as u64,
            last_insert_id: 0,
            warnings: 0,
            truncated: false,
            elapsed_ms: 1,
            preview: "SELECT".into(),
            rows,
        }
    }

    #[test]
    fn csv_keeps_the_shape_the_grid_shows() {
        let one = table(
            &["name", "n"],
            vec![
                vec![Cell::Text("张三".into()), Cell::Text("1".into())],
                vec![Cell::Text("a,b".into()), Cell::Null],
                vec![Cell::Text("他说\"嗨\"".into()), Cell::Text("3".into())],
                vec![Cell::Blob(vec![0x00, 0xff]), Cell::Text("4".into())],
            ],
        );
        let csv = to_csv(&one);
        assert_eq!(
            csv,
            "name,n\r\n\
             张三,1\r\n\
             \"a,b\",\r\n\
             \"他说\"\"嗨\"\"\",3\r\n\
             00ff,4\r\n",
            "逗号要包、引号要翻倍、NULL 留空、BLOB 走十六进制：{csv:?}"
        );
    }

    #[test]
    fn csv_quotes_lines_that_span_rows() {
        let one = table(&["log"], vec![vec![Cell::Text("第一行\n第二行".into())]]);
        let csv = to_csv(&one);
        assert_eq!(
            csv, "log\r\n\"第一行\n第二行\"\r\n",
            "字段里的换行不包起来，读的人就数不清有几行了：{csv:?}"
        );
    }

    #[test]
    fn csv_neutralises_fields_excel_would_evaluate() {
        let one = table(
            &["cmd"],
            vec![
                vec![Cell::Text("=1+1".into())],
                vec![Cell::Text("-250*cmd|'/C calc'!A0".into())],
                vec![Cell::Text("@SUM(1,2)".into())],
                vec![Cell::Text("-2".into())],
            ],
        );
        let csv = to_csv(&one);
        assert_eq!(
            csv,
            "cmd\r\n\
             \'\"=1+1\"\r\n\
             \'\"-250*cmd|\'/C calc\'!A0\"\r\n\
             \'\"@SUM(1,2)\"\r\n\
             -2\r\n",
            "公式要打回文本，但 -2 这种纯数字不能被弄成文本：{csv:?}"
        );
    }

    #[test]
    fn csv_name_survives_a_title_that_is_not_a_filename() {
        assert_eq!(csv_name("查 用户", 3), "查 用户.csv");
        assert_eq!(csv_name("a/b:c*d", 3), "a_b_c_d.csv");
        assert_eq!(csv_name("  ", 3), "result_3.csv", "空标题也得有个文件名");
    }

    /// 一次执行三条：每个面板都得知道自己排第几、一共几条、是哪一批。
    /// 光靠标题分不出来——同一个块里的语句预览长得很像。
    #[test]
    fn each_panel_of_a_batch_knows_its_own_place() {
        let mut dock = dock();
        assert_eq!(dock.panels.len(), 3);
        assert!(
            dock.panels.iter().all(|p| p.total == 3),
            "同一批每条都该写着「共 3 条」"
        );
        let seq = dock.panels.iter().map(|p| p.seq).collect::<Vec<usize>>();
        assert_eq!(seq, vec![1, 2, 3], "面板从上到下就是语句顺序");
        let first = dock.panels[0].stamp.clone();
        assert!(!first.is_empty(), "批次抬头要有一个时间戳");
        assert!(
            dock.panels.iter().skip(1).all(|p| p.stamp == first),
            "一批一次提交，抬头只有一个时间"
        );

        // 第二次执行是另一批：单条不该显示「1/3」这种分数
        dock.start_run("本机", "查订单", &parts(&["SELECT 9"]), None);
        assert_eq!(dock.panels[0].total, 1);
        assert_ne!(dock.panels[0].run, dock.panels[1].run);
    }

    /// 「复制」按钮交出去的那份文本：粘进表格软件天然分列，
    /// 所以字段里再出现制表符和换行必须压掉，不然一行撑成几行。
    #[test]
    fn copy_hands_over_the_whole_result_as_one_tab_separated_block() {
        let one = table(
            &["name", "n"],
            vec![
                vec![Cell::Text("张三\t工程师".into()), Cell::Text("1".into())],
                vec![Cell::Text("第一行\n第二行".into()), Cell::Null],
            ],
        );
        assert_eq!(
            to_tsv(&one),
            "name\tn\n张三 工程师\t1\n第一行 第二行\tNULL",
            "表头在首行，NULL 跟网格里写的一样"
        );
    }

    /// 带类型名的列。上面那个 `table()` 把类型名留空了，而导出 SQL、排 JSON 全靠
    /// 类型名判断这格该裸写还是该加引号，所以这儿单开一个。
    ///
    /// 自增标志一律给 false——要测「去掉自动生成列」那条走 `auto_incremented`。
    fn typed(names: &[(&str, &str)]) -> Vec<Column> {
        names
            .iter()
            .map(|(name, ty)| Column {
                name: (*name).to_string(),
                type_name: (*ty).to_string(),
                auto_increment: false,
            })
            .collect()
    }

    /// 同上，另外点名哪几列是数据库自己填的（`AUTO_INCREMENT`）。
    fn auto_incremented(names: &[(&str, &str)], auto: &[&str]) -> Vec<Column> {
        typed(names)
            .into_iter()
            .map(|mut col| {
                col.auto_increment = auto.contains(&col.name.as_str());
                col
            })
            .collect()
    }

    #[test]
    fn insert_covers_every_column_in_the_order_they_came() {
        let cols = typed(&[
            ("id", "MYSQL_TYPE_LONGLONG"),
            ("name", "MYSQL_TYPE_VAR_STRING"),
            ("note", "MYSQL_TYPE_VAR_STRING"),
            ("pic", "MYSQL_TYPE_BLOB"),
        ]);
        let row = vec![
            Cell::Text("7".into()),
            Cell::Text("O'Brien".into()),
            Cell::Null,
            Cell::Blob(vec![0x0d, 0xff]),
        ];
        assert_eq!(
            insert_sql("users", &cols, &[row.as_slice()], false),
            "INSERT INTO `users` (`id`, `name`, `note`, `pic`) VALUES (7, 'O\\'Brien', NULL, 0x0dff);\n",
            "数值裸写、文本带引号、NULL 就是 NULL、二进制走 0x…"
        );
    }

    #[test]
    fn update_matches_the_row_by_every_value_it_carries() {
        let cols = typed(&[("id", "LONGLONG"), ("note", "VAR_STRING")]);
        let row = vec![Cell::Text("7".into()), Cell::Null];
        assert_eq!(
            update_sql("my.users", &cols, &[row.as_slice()], false),
            "UPDATE `my`.`users` SET `id` = 7, `note` = NULL \
             WHERE `id` = 7 AND `note` IS NULL LIMIT 1;\n",
            "结果里不带主键，WHERE 只能整行列值；NULL 那列必须写 IS NULL，\
             `= NULL` 在 SQL 里永远不成立"
        );
    }

    #[test]
    fn a_value_is_bare_only_when_the_column_is_numeric() {
        // varchar 里存数字照样是字符串：导出去变成另一个类型，回填就错了
        assert_eq!(
            sql_literal(&Cell::Text("42".into()), "MYSQL_TYPE_VAR_STRING"),
            "'42'"
        );
        assert_eq!(
            sql_literal(&Cell::Text("42".into()), "MYSQL_TYPE_LONGLONG"),
            "42"
        );
        // 数值列里读出不是数字的东西（驱动偶尔会这样），宁可加引号也别拼出半截语法
        assert_eq!(
            sql_literal(&Cell::Text("abc".into()), "LONGLONG"),
            "'abc'"
        );
        // 反斜杠在 MySQL 默认 sql_mode 下是转义符，不翻倍就把它后面的引号吃了
        assert_eq!(
            sql_literal(&Cell::Text("C:\\a'b\n".into()), "VAR_STRING"),
            "'C:\\\\a\\'b\\n'"
        );
    }

    #[test]
    fn table_of_reads_the_names_statements_carry() {
        assert_eq!(
            table_of("SELECT * FROM users WHERE id = 1").as_deref(),
            Some("users")
        );
        assert_eq!(table_of("update `my`.Orders o set o.a = 1").as_deref(), Some("my.Orders"));
        assert_eq!(table_of("INSERT INTO t VALUES (1);").as_deref(), Some("t"));
        // 注释掉的句子排在前面，不能拿它当表名
        assert_eq!(
            table_of("-- sql 查订单\n-- SELECT * FROM users;\nSELECT * FROM orders;").as_deref(),
            Some("orders")
        );
        // 认不出来的这几种都得让按钮消失，而不是拼半截
        assert_eq!(table_of("SELECT * FROM (SELECT 1) x"), None);
        assert_eq!(table_of("SELECT * FROM a, b"), None);
        assert_eq!(table_of("WHERE note = 'from users'"), None);
        assert_eq!(table_of("SELECT 1"), None);
    }

    #[test]
    fn json_keeps_the_column_order_the_table_has() {
        let cols = typed(&[
            ("zeta", "MYSQL_TYPE_VAR_STRING"),
            ("alpha", "MYSQL_TYPE_LONGLONG"),
        ]);
        let fields = row_fields(
            &cols,
            &[Cell::Text("z".into()), Cell::Text("3".into())],
        );
        assert_eq!(
            row_json(&fields),
            "{\n  \"zeta\": \"z\",\n  \"alpha\": 3\n}",
            "serde_json 的对象按键名排序，列的顺序就没了——顶层只能自己拼"
        );
    }

    #[test]
    fn json_tells_null_number_and_binary_apart() {
        let cols = typed(&[
            ("n", "LONGLONG"),
            ("t", "VAR_STRING"),
            ("z", "VAR_STRING"),
            ("b", "BLOB"),
        ]);
        let fields = row_fields(
            &cols,
            &[
                Cell::Text("12".into()),
                Cell::Text("12".into()),
                Cell::Null,
                Cell::Blob(vec![0x01, 0xab]),
            ],
        );
        assert_eq!(
            row_json(&fields),
            "{\n  \"n\": 12,\n  \"t\": \"12\",\n  \"z\": null,\n  \"b\": \"01ab\"\n}"
        );
    }

    #[test]
    fn a_json_column_opens_up_instead_of_staying_a_string() {
        let cols = typed(&[("meta", "JSON"), ("flag", "VAR_STRING")]);
        let fields = row_fields(
            &cols,
            &[
                Cell::Text(r#"{"a":1,"b":[1,2]}"#.into()),
                // 裸值不当嵌套：varchar 里一列 "1" 全摆成 1，反而像在说它是数字
                Cell::Text("true".into()),
            ],
        );
        assert_eq!(
            row_json(&fields),
            "{\n  \"meta\": {\n    \"a\": 1,\n    \"b\": [\n      1,\n      2\n    ]\n  },\n  \"flag\": \"true\"\n}",
            "嵌套那一段第二行起要补一层缩进，不然看着像塌了一半"
        );
    }
}
