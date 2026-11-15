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

/// 一个语句结果面板。`task` 是 [`crate::db::DbHub::run`] 给这条语句发的 id。
pub struct Panel {
    pub task: u64,
    /// 同一次执行的面板共用一个 run 号，「关掉整块」按它收
    pub run: u64,
    pub conn: String,
    /// SQL 块标题，块外选中一段执行时是「选中内容」
    pub title: String,
    pub stmt: String,
    /// 这条语句出自哪个文件的哪个字节，用来跳回去
    pub origin: Option<(PathBuf, usize)>,
    pub slot: Slot,
    pub collapsed: bool,
}

impl Panel {
    pub fn preview(&self) -> String {
        preview_of(&self.stmt)
    }
}

/// 底部那一条结果区。
pub struct ResultsDock {
    /// 新的在上面：一次执行 splice 到 0，同批内部仍按语句顺序。
    panels: Vec<Panel>,
    next_run: u64,
    /// 结果区显不显示。关掉不丢结果，只是收起来。
    pub open: bool,
    /// 「回块」按下的那一帧记在这里，app 取走去做跳转。
    pub pending_jump: Option<(PathBuf, usize)>,
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
            open: false,
            pending_jump: None,
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

    /// 一次提交开了哪些面板，逐条挂在这里等回执。
    pub fn start_run(
        &mut self,
        conn: &str,
        title: &str,
        parts: &[(u64, String)],
        origin: Option<(PathBuf, usize)>,
    ) {
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
        self.trim();
        self.open = true;
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

        let mut closable: Vec<u64> = Vec::new();
        let mut close_runs: Vec<u64> = Vec::new();
        let mut jump: Option<(PathBuf, usize)> = None;
        // 铺满底部面板给的那一格，装不下的走这里的滚动条（面板高度用户能拖）。
        // 缩成内容高反而不行：面板先按起始尺寸预留，多出来的那一截掉在窗口下沿
        // 外面，看着就是「只画了第一个面板」。
        ScrollArea::vertical()
            .id_salt("db_result_panels")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for panel in &mut self.panels {
                    if panel.collapsed {
                        collapsed_row(ui, panel, &mut closable, &mut close_runs);
                    } else {
                        open_panel(ui, panel, &mut closable, &mut close_runs, &mut jump);
                    }
                    ui.add_space(3.0);
                }
            });

        // 渲染期间只记账，动列表在这儿
        if let Some(target) = jump {
            self.pending_jump = Some(target);
        }
        for task in closable {
            self.close(task);
        }
        for run in close_runs {
            self.close_run(run);
        }
    }
}

fn open_panel(
    ui: &mut Ui,
    panel: &mut Panel,
    closable: &mut Vec<u64>,
    close_runs: &mut Vec<u64>,
    jump: &mut Option<(PathBuf, usize)>,
) {
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
            header(ui, panel, closable, close_runs, jump);
            ui.separator();
            body(ui, panel);
        });
}

fn collapsed_row(
    ui: &mut Ui,
    panel: &mut Panel,
    closable: &mut Vec<u64>,
    close_runs: &mut Vec<u64>,
) {
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
            header(ui, panel, closable, close_runs, &mut None);
        });
}

/// 标题行。右边那组按钮先排，再把剩下的宽度整块交给左组——
/// 语句预览要的是「铺满有余」的 truncate，`ui.horizontal` 里放最后已经太晚了。
///
/// `row_h` 那块矩形是必须的：`Layout::right_to_left(Align::Center)` 在非换行布局里
/// 会把父级剩的高度整块吃掉（egui `Layout::next_frame_ignore_wrap`），一行标题
/// 直接变成一百多像素，第二个面板被顶到裁剪区外面，画都画不出来。
fn header(
    ui: &mut Ui,
    panel: &mut Panel,
    closable: &mut Vec<u64>,
    close_runs: &mut Vec<u64>,
    jump: &mut Option<(PathBuf, usize)>,
) {
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
                closable.push(panel.task);
            }
            if ui
                .add(egui::Button::new(RichText::new("关掉整块").size(11.0)).frame(false))
                .on_hover_text("关掉这次执行的所有面板")
                .clicked()
            {
                close_runs.push(panel.run);
            }
            if panel.origin.is_some()
                && ui
                    .add(egui::Button::new(RichText::new("回块").size(11.0)).frame(false))
                    .on_hover_text("跳回文件里这个 SQL 块")
                    .clicked()
            {
                *jump = panel.origin.clone();
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
        Slot::Done(one) if one.is_query() => grid(ui, one, panel.task),
        Slot::Done(one) => {
            ui.label(RichText::new(write_line(one)).size(11.0));
        }
    }
}

fn grid(ui: &mut Ui, one: &StatementResult, task: u64) {
    let table = TableBuilder::new(ui)
        .id_salt(("db_result", task))
        .striped(true)
        .auto_shrink([false, true])
        // 行数少就照内容收，多了封顶走这块自己的滚动条。不封顶的话六个面板叠起来，
        // 结果区再怎么拖也装不下两条完整结果。
        .min_scrolled_height(0.0)
        .max_scroll_height(120.0)
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
            Some((PathBuf::from("d:/a.sql"), 12)),
        );
        dock
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
