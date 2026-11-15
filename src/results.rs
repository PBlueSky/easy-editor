//! 底部结果区：一条语句一个结果面板，多个面板同时在屏上。
//!
//! 为什么不是一整块一个面板：块里三条 SELECT，第 2 条写错了，第 1 条拿到的结果
//! 还是想看的。按块只能「整块成功」或「整块报错」二选一，所以 [`crate::db::DbHub::run`]
//! 拆句后每条给一个 id，这里一个 id 挂一个面板，谁回来谁落地。
//!
//! 这一层不碰数据库也不碰文件：结果由 `DatabasePanel::take_stray` 转交过来，
//! 「回块」只是把 (路径, 字节) 原样递回给 app，真正的跳转归 app 管。

use std::collections::HashSet;
use std::path::PathBuf;

use egui::{Align, Color32, Layout, RichText, ScrollArea, Ui};
use egui_extras::{Column, TableBuilder};

use crate::db::{preview_of, Cell, DbEvent, DbOutcome, StatementResult};

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
    /// 这批是哪一秒提交的。六个面板叠在一起时，这是「哪批是哪批」唯一的凭据
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
    pub collapsed: bool,
    /// 摊开看完整内容的行号（0 基）。窄格子里被裁掉的那一截在这儿看全。
    pub expanded: HashSet<usize>,
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

/// 按了「导出」攒下来的那份 CSV。文本在这儿已经排好，选路径和写文件归 app。
pub struct Export {
    /// 存盘对话框里的默认文件名
    pub name: String,
    pub csv: String,
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
    /// 「导出」按下的那一帧攒在这儿，app 每帧取走一次去弹存盘框。
    pending_export: Option<Export>,
    /// 报错语句在文件里的位置，攒着等 app 每帧取走标红。带上语句原文，
    /// app 拿它对一下区间——中间用户改过文本的话区间就不作数了。
    fails: Vec<(PathBuf, usize, usize, String)>,
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
            pending_export: None,
            fails: Vec::new(),
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

    /// 有没有还在等的回执。app 靠它决定要不要继续重画。
    pub fn running(&self) -> bool {
        self.panels.iter().any(|p| p.slot.is_running())
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
        let batch = parts
            .iter()
            .enumerate()
            .map(|(i, part)| Panel {
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
                collapsed: false,
                expanded: HashSet::new(),
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
                // 同批面板在列表里连着排（一批 splice 到 0，批内按语句顺序），
                // 所以只要盯住 run 变了没，就能在每批开头插一行抬头。
                let mut last_run = 0u64;
                for panel in &mut self.panels {
                    if panel.run != last_run {
                        last_run = panel.run;
                        batch_row(ui, panel);
                    }
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
                ui.label(weak(summary(panel)));
                ui.separator();
                if matches!(&panel.slot, Slot::Done(one) if one.is_query()) {
                    // 窗口给多少用多少，不像底部那样只留五来行
                    let height = ui.available_height() - 12.0;
                    grid(ui, panel, height);
                } else {
                    body(ui, panel);
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
    /// 按了「导出」，这帧现成的 CSV 文本先记下来，落盘归 app 管
    export: Option<Export>,
}

/// 一批结果上面那一行：几点跑的、发到哪、出自哪个块、一共几条。
/// 六个面板叠在一起时，标题会重复、语句长得像，只有时间戳分得清哪批是哪批。
fn batch_row(ui: &mut Ui, panel: &Panel) {
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        let accent = ui.visuals().hyperlink_color;
        ui.label(RichText::new(&panel.stamp).size(10.5).strong().color(accent));
        ui.label(weak(format!(
            "{} · 「{}」 · 共 {} 条",
            panel.conn, panel.title, panel.total
        )));
    });
}

/// 展开的那一行摊开成「列名 = 完整值」。
///
/// 表格里每格都是截断显示，长文本看不出头尾；这里不换行限制、可选中，
/// 要拷一段出来也在这儿拷。
fn detail(ui: &mut Ui, one: &StatementResult, index: usize) {
    let dark = ui.visuals().dark_mode;
    let bg = if dark {
        Color32::from_rgba_unmultiplied(255, 255, 255, 12)
    } else {
        Color32::from_rgba_unmultiplied(0, 0, 0, 10)
    };
    egui::Frame::new()
        .fill(bg)
        .corner_radius(3.0)
        .inner_margin(egui::Margin::symmetric(6, 2))
        .show(ui, |ui| {
            ui.label(weak(format!("第 {} 行", index + 1)).strong());
            for (col, cell) in one.columns.iter().zip(&one.rows[index]) {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(format!("{} =", col.name)).size(10.5).weak());
                    ui.add(
                        egui::Label::new(
                            RichText::new(cell.display().to_string())
                                .size(11.0)
                                .monospace(),
                        )
                        .selectable(true),
                    );
                });
            }
        });
    ui.add_space(2.0);
}

fn open_panel(ui: &mut Ui, panel: &mut Panel, act: &mut Clicked) {
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
            body(ui, panel);
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
                if ui
                    .add(egui::Button::new(RichText::new("复制").size(11.0)).frame(false))
                    .on_hover_text("整份结果按制表符分隔拷进剪贴板，直接粘进表格软件。\n单个格子右键也有复制。")
                    .clicked()
                {
                    if let Slot::Done(one) = &panel.slot {
                        ui.ctx().copy_text(to_tsv(one));
                    }
                }
                if ui
                    .add(egui::Button::new(RichText::new("导出").size(11.0)).frame(false))
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
                if panel.total > 1 {
                    // 一次跑三条，得看得出这是第几条——不然三个面板看着像同一句重复了三遍
                    ui.label(weak(format!("{}/{}", panel.seq, panel.total)));
                }
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

fn body(ui: &mut Ui, panel: &mut Panel) {
    if matches!(&panel.slot, Slot::Done(one) if one.is_query()) {
        grid(ui, panel, 120.0);
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
        Slot::Done(one) => {
            ui.label(RichText::new(write_line(one)).size(11.0));
        }
    }
}

/// 结果网格。`height` 是这块最多能占到多高，超出行数走它自己的滚动条：
/// 底部那一格给 120，放大窗口给整屏。
///
/// 每格都是截断显示，装不下的那一截靠行首的 ▸ 摊到表格下面看全（见 [`detail`]）。
fn grid(ui: &mut Ui, panel: &mut Panel, height: f32) {
    // 拆字段借：网格读 `slot` 里的结果，展开状态要写回 `expanded`
    let Panel {
        task,
        slot,
        expanded,
        ..
    } = panel;
    let Slot::Done(one) = slot else { return };
    let task = *task;
    let one = &*one;
    let table = TableBuilder::new(ui)
        .id_salt(("db_result", task))
        .striped(true)
        .auto_shrink([false, true])
        // 行数少就照内容收，多了封顶走这块自己的滚动条。不封顶的话六个面板叠起来，
        // 结果区再怎么拖也装不下两条完整结果。
        .min_scrolled_height(0.0)
        .max_scroll_height(height)
        .cell_layout(Layout::left_to_right(Align::Center))
        .column(Column::initial(46.0).at_least(34.0).clip(true))
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
                    let arrow = if expanded.contains(&index) { "▾" } else { "▸" };
                    let resp = ui.add(
                        egui::Label::new(RichText::new(arrow).size(10.0).weak())
                            .selectable(false)
                            .sense(egui::Sense::click()),
                    );
                    if resp.on_hover_text("展开看这一行的完整内容").clicked()
                        && !expanded.remove(&index)
                    {
                        expanded.insert(index);
                    }
                    ui.label(weak(format!("{}", index + 1)));
                });
                for cell in &one.rows[index] {
                    let text = cell.display().to_string();
                    let null = cell.is_null();
                    let mut rt = RichText::new(text.clone()).size(11.0).monospace();
                    if null {
                        rt = rt.weak().italics();
                    }
                    row.col(|ui| {
                        let resp = ui
                            .add(egui::Label::new(rt).truncate().selectable(true))
                            .on_hover_cursor(egui::CursorIcon::Text);
                        resp.context_menu(|ui| {
                                if ui.button("复制单元格").clicked() {
                                    ui.ctx().copy_text(text.clone());
                                    ui.close();
                                }
                                if ui.button("复制整行").clicked() {
                                    let line = tsv_row(one.rows[index].iter().map(Cell::display));
                                    ui.ctx().copy_text(line);
                                    ui.close();
                                }
                                if ui.button("复制整表").clicked() {
                                    ui.ctx().copy_text(to_tsv(one));
                                    ui.close();
                                }
                            });
                    });
                }
            });
        });
    if one.truncated {
        ui.label(weak(
            "行数超过上限，只留了前面这些；要全量请在 SQL 里自己分页。",
        ));
    }
    // 展开的行统一摊在表格下面：表格自己带裁剪，行中间塞不进去一整块明细
    let mut open: Vec<usize> = expanded
        .iter()
        .copied()
        .filter(|i| *i < one.rows.len())
        .collect();
    open.sort_unstable();
    for index in open {
        detail(ui, one, index);
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
                crate::db::Cell::Blob(bytes) => {
                    bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
                }
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
        assert_eq!(dock.panels[0].task, 21, "挤剩的该是最早那批的尾巴");
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

    fn table(columns: &[&str], rows: Vec<Vec<Cell>>) -> StatementResult {
        StatementResult {
            columns: columns
                .iter()
                .map(|name| Column {
                    name: (*name).to_string(),
                    type_name: String::new(),
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
        let one = table(
            &["log"],
            vec![vec![Cell::Text("第一行\n第二行".into())]],
        );
        let csv = to_csv(&one);
        assert_eq!(
            csv,
            "log\r\n\"第一行\n第二行\"\r\n",
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
        let seq = dock
            .panels
            .iter()
            .map(|p| p.seq)
            .collect::<Vec<usize>>();
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
}
