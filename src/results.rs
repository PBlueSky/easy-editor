//! 底部结果区：一条语句一个结果面板，多个面板同时在屏上。
//!
//! 为什么不是一整块一个面板：块里三条 SELECT，第 2 条写错了，第 1 条拿到的结果
//! 还是想看的。按块只能「整块成功」或「整块报错」二选一，所以 [`crate::db::DbHub::run`]
//! 拆句后每条给一个 id，这里一个 id 挂一个面板，谁回来谁落地。
//!
//! 这一层不碰数据库也不碰文件：结果由 `DatabasePanel::take_stray` 转交过来，
//! 「回块」只是把 (路径, 字节) 原样递回给 app，真正的跳转归 app 管。

use std::path::PathBuf;

use egui::{Align, Color32, Layout, RichText, ScrollArea, Ui};
use egui_extras::{Column, TableBuilder};

use crate::db::{preview_of, DbEvent, DbOutcome, StatementResult};

/// 屏上同时最多留几个面板。跑几次多语句块就能把旧的挤掉，留太多没人往下看。
pub const MAX_PANELS: usize = 6;

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

/// 一个语句结果面板。`task` 是 [`crate::db::DbHub::run`] 给这条语句发的 id。
pub struct Panel {
    pub task: u64,
    /// 同一次执行的面板共用一个 run 号，「关掉整块」按它收
    pub run: u64,
    pub conn: String,
    /// SQL 块标题，块外选中一段执行时是「选中内容」
    pub title: String,
    pub stmt: String,
    /// 这条语句出自哪个文件的哪个字节，用来跳回去、跑完回写状态
    pub origin: Option<Origin>,
    pub slot: Slot,
    pub collapsed: bool,
}

impl Panel {
    pub fn preview(&self) -> String {
        preview_of(&self.stmt)
    }
}

/// 一批结果全部到家，app 可以拿去做收尾（比如把块头部的 `[待执行]` 改掉）。
pub struct Settled {
    /// 这批结果出自哪个文件的哪个块头。选中一段执行的没有块头，不会出现在这里。
    pub path: PathBuf,
    pub head: usize,
    /// 整批都成功。有一条报错或有一条压根没执行，都算失败。
    pub ok: bool,
}

/// 底部那一条结果区。
pub struct ResultsDock {
    /// 新的在上面：一次执行 splice 到 0，同批内部仍按语句顺序。
    panels: Vec<Panel>,
    next_run: u64,
    /// 已提交、还没收尾的那几批。`settled` 只认这里的，报过就摘掉。
    waiting: Vec<u64>,
    /// 结果区显不显示。关掉不丢结果，只是收起来。
    pub open: bool,
    /// 「回块」按下的那一帧记在这里，app 取走去做跳转。
    pub pending_jump: Option<(PathBuf, usize)>,
    /// 单独开窗口看的那份结果（按语句 id）。
    popup: Option<u64>,
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
            next_run: 1,
            waiting: Vec::new(),
            open: false,
            pending_jump: None,
            popup: None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.panels.is_empty()
    }

    pub fn count(&self) -> usize {
        self.panels.len()
    }

    /// 有没有还在等的回执。app 靠它决定要不要继续重画。
    pub fn running(&self) -> bool {
        self.panels.iter().any(|p| p.slot.is_running())
    }

    /// 一次提交开了哪些面板，逐条挂在这里等回执。返回这批的 run 号。
    pub fn start_run(
        &mut self,
        conn: &str,
        title: &str,
        parts: &[(u64, String)],
        origin: Option<Origin>,
    ) -> u64 {
        let run = self.next_run;
        self.next_run += 1;
        let batch = parts.iter().map(|(task, stmt)| Panel {
            task: *task,
            run,
            conn: conn.to_string(),
            title: title.to_string(),
            stmt: stmt.clone(),
            origin: origin.clone(),
            slot: Slot::Running,
            collapsed: false,
        });
        self.panels.splice(0..0, batch);
        self.waiting.push(run);
        self.trim();
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

    pub fn set_all_collapsed(&mut self, collapsed: bool) {
        for panel in &mut self.panels {
            panel.collapsed = collapsed;
        }
    }

    fn trim(&mut self) {
        if self.panels.len() > MAX_PANELS {
            self.panels.truncate(MAX_PANELS);
        }
    }

    pub fn show(&mut self, ui: &mut Ui, run_hint: &str) {
        let count = self.panels.len();
        let running = self.running();
        let any_open = self.panels.iter().any(|p| !p.collapsed);

        ui.horizontal(|ui| {
            let head = if running {
                format!("结果 · {count} 个 · 执行中…")
            } else {
                format!("结果 · {count} 个")
            };
            ui.label(RichText::new(head).size(11.5).strong());
            ui.label(weak("一条语句一个面板，点标题行收起"));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .add(egui::Button::new(RichText::new("关闭").size(11.5)).frame(false))
                    .on_hover_text("收起结果区（结果留着，顶栏「数据库 → 结果区」再打开）")
                    .clicked()
                {
                    self.open = false;
                }
                if count > 0
                    && ui
                        .add(egui::Button::new(RichText::new("清空").size(11.5)).frame(false))
                        .on_hover_text("关掉跑完的面板，执行中的留着")
                        .clicked()
                {
                    self.clear_finished();
                }
                if count > 1 {
                    let text = if any_open { "全部收起" } else { "全部展开" };
                    if ui
                        .add(egui::Button::new(RichText::new(text).size(11.5)).frame(false))
                        .clicked()
                    {
                        self.set_all_collapsed(any_open);
                    }
                }
            });
        });
        ui.separator();

        // 这里重新读一次，不用上头算好的 count：这一帧刚点「清空」的话，
        // 该当场把引导文案亮出来，而不是留一片空白。
        if self.is_empty() {
            ui.label(weak(format!(
                "还没有结果。在文件里写一个 SQL 块（顶栏「数据库 → 插入 SQL 块」），光标放进去按 {run_hint}。\
                 选中一段 SQL 也能按它，那时只跑选中的部分。"
            )));
            return;
        }

        let mut act = Clicked {
            popup: self.popup,
            ..Default::default()
        };
        // 铺满底部面板给的那一格，装不下的走这里的滚动条（面板高度用户能拖）。
        // 缩成内容高反而不行：面板先按起始尺寸预留，多出来的那一截掉在窗口下沿
        // 外面，看着就是「只画了第一个面板」。
        ScrollArea::vertical()
            .id_salt("db_result_panels")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for panel in &mut self.panels {
                    if panel.collapsed {
                        collapsed_row(ui, panel, &mut act);
                    } else {
                        open_panel(ui, panel, &mut act);
                    }
                    ui.add_space(3.0);
                }
            });

        // 渲染期间只记账，动列表在这儿
        if let Some(target) = act.jump {
            self.pending_jump = Some(target);
        }
        if let Some(task) = act.zoom_toggle {
            self.popup = if self.popup == Some(task) { None } else { Some(task) };
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
    }

    /// 单独拉出来的结果窗口：底部那一格窄，宽表放这儿看。
    ///
    /// 归结果区自己管：面板和窗口得是同一份数据，抄一份出去就等着两边对不上。
    pub fn show_popup(&mut self, ctx: &egui::Context) {
        let Some(task) = self.popup else { return };
        let Some(panel) = self.panels.iter().find(|p| p.task == task) else {
            self.popup = None;
            return;
        };
        let mut open = true;
        egui::Window::new(format!("结果 · {}", panel.title))
            .id(egui::Id::unique(("db_result_popup", task)))
            .default_open(true)
            .default_size(egui::vec2(820.0, 460.0))
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(weak(summary(panel)));
                ui.separator();
                match &panel.slot {
                    Slot::Done(one) if one.is_query() => {
                        // 窗口给多少用多少，不像底部那样只留五来行
                        grid(ui, one, task, ui.available_height() - 12.0);
                    }
                    _ => body(ui, panel),
                }
            });
        if !open {
            self.popup = None;
        }
    }
}

/// 一帧里从各个面板收上来的点击。渲染期间不动列表，账先记这儿。
#[derive(Default)]
struct Clicked {
    closable: Vec<u64>,
    close_runs: Vec<u64>,
    jump: Option<(PathBuf, usize)>,
    /// 这帧按了哪个面板的「放大」。谁都不按就是 None，窗口状态原样留着。
    zoom_toggle: Option<u64>,
    /// 当前开着窗口的那个面板，只拿来给按钮画按下态
    popup: Option<u64>,
}

fn open_panel(ui: &mut Ui, panel: &mut Panel, act: &mut Clicked) {
    let dark = ui.visuals().dark_mode;
    let line = if dark {
        Color32::from_rgb(70, 76, 86)
    } else {
        Color32::from_rgb(214, 218, 224)
    };
    egui::Frame::new()
        .stroke(egui::Stroke::new(1.0, line))
        .corner_radius(4.0)
        .inner_margin(egui::Margin::symmetric(6, 2))
        .show(ui, |ui| {
            header(ui, panel, act);
            ui.separator();
            body(ui, panel);
        });
}

fn collapsed_row(ui: &mut Ui, panel: &mut Panel, act: &mut Clicked) {
    let dark = ui.visuals().dark_mode;
    let bg = if dark {
        Color32::from_rgba_unmultiplied(255, 255, 255, 10)
    } else {
        Color32::from_rgba_unmultiplied(0, 0, 0, 8)
    };
    egui::Frame::new()
        .fill(bg)
        .corner_radius(4.0)
        .inner_margin(egui::Margin::symmetric(6, 1))
        .show(ui, |ui| {
            header(ui, panel, act);
        });
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
            if ui
                .add(egui::Button::new(RichText::new("✕").size(11.0)).frame(false))
                .on_hover_text("关掉这个结果面板")
                .clicked()
            {
                act.closable.push(panel.task);
            }
            if ui
                .add(egui::Button::new(RichText::new("关掉整块").size(11.0)).frame(false))
                .on_hover_text("关掉这次执行的所有面板")
                .clicked()
            {
                act.close_runs.push(panel.run);
            }
            if matches!(&panel.slot, Slot::Done(one) if one.is_query()) {
                // 窗口开着就换成「收起」，否则点了没反应的样子会让人反复按
                let text = if act.popup == Some(panel.task) { "收起" } else { "放大" };
                if ui
                    .add(egui::Button::new(RichText::new(text).size(11.0)).frame(false))
                    .on_hover_text("单独开一个窗口看这份结果，宽表不受底部那一格的宽度限制")
                    .clicked()
                {
                    act.zoom_toggle = Some(panel.task);
                }
            }
            if let Some(origin) = panel.origin.clone() {
                if ui
                    .add(egui::Button::new(RichText::new("回块").size(11.0)).frame(false))
                    .on_hover_text("跳回文件里这个 SQL 块")
                    .clicked()
                {
                    act.jump = Some((origin.path, origin.jump));
                }
            }
            ui.label(weak(summary(panel)));
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                let arrow = if panel.collapsed { "▸" } else { "▾" };
                if ui
                    .add(
                        egui::Label::new(RichText::new(arrow).size(11.0))
                            .selectable(false)
                            .sense(egui::Sense::click()),
                    )
                    .on_hover_text("收起 / 展开")
                    .clicked()
                {
                    panel.collapsed = !panel.collapsed;
                }
                let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                ui.painter()
                    .circle_filled(rect.center(), 3.0, dot_color(panel));
                ui.label(weak(badge(panel)));
                let text = panel.preview();
                ui.add(
                    egui::Label::new(RichText::new(text).size(11.0).monospace())
                        .truncate()
                        .selectable(false),
                )
                .on_hover_text(panel.stmt.clone());
            });
        },
    );
}

fn body(ui: &mut Ui, panel: &Panel) {
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
                    .size(11.0)
                    .weak()
                    .italics(),
            );
        }
        Slot::Failed(err) => {
            // 可选中：报错原文要能整段拷出去查
            ui.add(
                egui::Label::new(
                    RichText::new(err)
                        .size(11.0)
                        .color(Color32::from_rgb(196, 60, 52))
                        .strong(),
                )
                .selectable(true),
            );
        }
        Slot::Done(one) if one.is_query() => grid(ui, one, panel.task, 120.0),
        Slot::Done(one) => {
            ui.label(RichText::new(write_line(one)).size(11.0));
        }
    }
}

/// 结果网格。`height` 是这块最多能占到多高，超出行数走它自己的滚动条：
/// 底部那一格给 120，放大窗口给整屏。
fn grid(ui: &mut Ui, one: &StatementResult, task: u64, height: f32) {
    let table = TableBuilder::new(ui)
        .id_salt(("db_result", task))
        .striped(true)
        .auto_shrink([false, true])
        // 行数少就照内容收，多了封顶走这块自己的滚动条。不封顶的话六个面板叠起来，
        // 结果区再怎么拖也装不下两条完整结果。
        .min_scrolled_height(0.0)
        .max_scroll_height(height)
        .cell_layout(Layout::left_to_right(Align::Center))
        .column(Column::initial(40.0).at_least(28.0).clip(true))
        .columns(
            Column::initial(140.0)
                .at_least(56.0)
                .resizable(true)
                .clip(true),
            one.columns.len(),
        );
    table
        .header(18.0, |mut header| {
            header.col(|ui| {
                ui.label(RichText::new("#").size(11.0).strong());
            });
            for col in &one.columns {
                let name = col.name.clone();
                let ty = col.type_name.clone();
                header.col(|ui| {
                    let resp = ui.add(
                        egui::Label::new(RichText::new(name).size(11.0).strong())
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
            body.rows(18.0, one.rows.len(), |mut row| {
                let index = row.index();
                row.col(|ui| {
                    ui.label(weak(format!("{}", index + 1)));
                });
                for cell in &one.rows[index] {
                    let text = cell.display().to_string();
                    row.col(|ui| {
                        let mut rt = RichText::new(text).size(11.0).monospace();
                        if cell.is_null() {
                            rt = rt.weak().italics();
                        }
                        let resp = ui.add(egui::Label::new(rt).truncate().selectable(true));
                        if !cell.is_null() {
                            resp.on_hover_cursor(egui::CursorIcon::Text);
                        }
                    });
                }
            });
        });
    if one.truncated {
        ui.label(weak(
            "行数超过上限，只留了前面这些；要全量请在 SQL 里自己分页。",
        ));
    }
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
    RichText::new(text.into()).size(10.5).weak()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Cell, Column, DbError};

    fn event(id: u64, outcome: Result<DbOutcome, String>) -> DbEvent {
        DbEvent {
            id,
            conn: "本机".into(),
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
                },
                Column {
                    name: "n".into(),
                    type_name: "int".into(),
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
        assert!(
            dock.settled().is_empty(),
            "第三条还在等，整批就还不算跑完"
        );
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
        assert_eq!(done.len(), 1, "报错会把排在后面的一起标掉，整批当场就有下文了");
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
    fn parts(stmts: &[&str]) -> Vec<(u64, String)> {
        offset_parts(stmts, 0)
    }

    fn offset_parts(stmts: &[&str], base: u64) -> Vec<(u64, String)> {
        stmts
            .iter()
            .enumerate()
            .map(|(i, s)| (base + i as u64 + 1, s.to_string()))
            .collect()
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
        dock.on_event(&event(
            2,
            Err(DbError::new("1064 · 语法错了").to_string()),
        ));
        assert!(matches!(dock.panels[0].slot, Slot::Done(_)), "先跑完的得留着");
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
        for run in 0..3 {
            let batch: Vec<(u64, String)> = (0..3)
                .map(|i| (run * 10 + i + 1, format!("SELECT {run}_{i}")))
                .collect();
            dock.start_run("本机", "块", &batch, None);
        }
        assert_eq!(dock.count(), MAX_PANELS, "最多留 {MAX_PANELS} 个");
        assert_eq!(dock.panels[0].task, 21, "挤剩的该是最早那批的尾巴");
    }

    #[test]
    fn clear_keeps_the_waiting_ones() {
        let mut dock = dock();
        dock.on_event(&event(1, Ok(query(1, 2))));
        dock.on_event(&event(
            2,
            Err(DbError::new("断了").to_string()),
        ));
        // 1 出结果、2 报错、3 被连累标成「未执行」：这一批都不会再产出什么，清空就该全走
        dock.clear_finished();
        assert_eq!(dock.count(), 0);
        dock.start_run("本机", "还在跑", &offset_parts(&["SELECT 5", "SELECT 6"], 10), None);
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

    #[test]
    fn collapse_switches_every_panel() {
        let mut dock = dock();
        dock.set_all_collapsed(true);
        assert!(dock.panels.iter().all(|p| p.collapsed));
        dock.set_all_collapsed(false);
        assert!(dock.panels.iter().all(|p| !p.collapsed));
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
}
