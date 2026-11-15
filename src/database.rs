//! 「数据库」侧栏页签 + 连接管理窗口。
//!
//! 边界：这一层只管「有哪些连接、连不连得上、里面有什么」。
//! SQL 块头部的文本格式在 `crate::sql`，引擎与线程编排在 `crate::db`，
//! 结果网格在 `crate::results` 的底部结果区。
//!
//! 树只有三层（连接 → 库 → 表），每层的键还各不相同，所以这里按层写死，
//! 不套文件树那种递归渲染：递归的代价是每层都要造一个路径式的 id，
//! 三层固定结构换不来任何东西。

use std::collections::{HashMap, HashSet};

use eframe::egui::{self, RichText, Ui};

use crate::db::{ConnectionSpec, DbEvent, DbHub, DbKind, DbOutcome, MAX_ROWS};

/// 子层缩进（库 1 格、表 2 格）。
const INDENT: f32 = 18.0;
/// 树里一行的高。整行都能点，行高给足才好命中。
const ROW_H: f32 = 22.0;
/// 行首那个 ▸/▾ 占的宽度，图标和名字都排在它后面。
const ARROW_W: f32 = 14.0;
/// 层图标占的宽度。
const ICON_W: f32 = 17.0;

/// 树每层的图标。选中态只改颜色，不换字形，位置才不会跳。
const CONN_ICON: &str = "◆";
const DB_ICON: &str = "▤";
const TABLE_ICON: &str = "▦";

/// 一层列表（库名、表名）的加载状态。
///
/// `Idle` 和 `Loading` 分开是有意的：展开一次连接只该发一条 `SHOW DATABASES`，
/// 而侧栏每帧都重画，没有 `Loading` 就会每帧问一次服务器。
#[derive(Clone, Debug, Default)]
enum Load {
    /// 还没展开过，不去打扰服务器
    #[default]
    Idle,
    /// 请求在路上
    Loading,
    Ready(Vec<String>),
    Failed(String),
}

impl Load {
    /// 只有「已在路上」和「已有结果」不该再问。失败可以再问：
    /// 网络抖一次之后点一下就该真的重试，而不是永远停在那句错误上。
    fn needs_fetch(&self) -> bool {
        !matches!(self, Self::Loading | Self::Ready(_))
    }
}

/// 一条连接在侧栏树里的状态。
#[derive(Debug, Default)]
struct ConnTree {
    expanded: bool,
    databases: Load,
    /// 展开的库
    open: HashSet<String>,
    /// 树上点选过的当前库。`None` 表示跟着配置里的默认库走。
    ///
    /// 只影响本进程的发送目标，不写回 config.json：在树里点两下就把用户的
    /// 配置改了，比不改更糟。
    current_db: Option<String>,
    /// 库名 -> 该库的表
    tables: HashMap<String, Load>,
}

/// 树的一行骨架：整行矩形、命中、悬停底色、以及祖先各层的淡竖线。
///
/// 以前每层靠 `add_space` 推空格，父子关系全凭数格子，行与行还高低不齐；
/// 现在一次 `allocate_exact_size` 把整行拿到手，内容按固定列摆，
/// 缩进处再画一根引导线，一眼看出这行挂在谁下面。
/// 返回的 `x` 是这一层箭头列的左边界。
fn tree_row(ui: &mut Ui, depth: usize) -> (egui::Response, f32) {
    let width = ui.available_width().max(120.0);
    let (rect, resp) = ui.allocate_exact_size(egui::Vec2::new(width, ROW_H), egui::Sense::click());
    paint_row_shell(ui, &resp, depth);
    (resp, rect.left() + 4.0 + INDENT * depth as f32)
}

/// 整行的底色与缩进引导线，`tree_row` 和只读的 `muted_row` 共用。
fn paint_row_shell(ui: &Ui, resp: &egui::Response, depth: usize) {
    let rect = resp.rect;
    if rect.width() <= 0.0 {
        return;
    }
    if resp.hovered() {
        ui.painter()
            .rect_filled(rect, 3.0, ui.visuals().widgets.hovered.weak_bg_fill);
    }
    let left = rect.left() + 4.0;
    let guide = ui.visuals().text_color().gamma_multiply(0.22);
    for level in 0..depth {
        let x = left + INDENT * level as f32 + ARROW_W * 0.5;
        ui.painter().line_segment(
            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
            (1.0, guide),
        );
    }
}

/// 箭头列的命中区间：点在箭头上是「展开 / 收起」，点在名字那一截是「选中」。
fn hit_arrow(resp: &egui::Response, x: f32) -> bool {
    match resp.interact_pointer_pos() {
        Some(pos) => pos.x < x + ARROW_W,
        // 键盘触发的点击没有指针位置，按选中处理
        None => false,
    }
}

/// ▸ / ▾。整行都能点之后它只报状态，不再是个单独的按钮。
fn arrow_glyph(ui: &Ui, cy: f32, x: f32, expanded: bool) {
    ui.painter().text(
        egui::pos2(x + ARROW_W * 0.5, cy),
        egui::Align2::CENTER_CENTER,
        if expanded { "▾" } else { "▸" },
        egui::FontId::proportional(9.5),
        ui.visuals().text_color().gamma_multiply(0.7),
    );
}

/// 行图标。当前那一支染主色，其余压暗，层级同时靠字形和颜色分开。
fn tree_icon(ui: &Ui, cy: f32, x: f32, glyph: &str, on: bool, accent: egui::Color32) {
    let color = if on {
        accent
    } else {
        ui.visuals().text_color().gamma_multiply(0.55)
    };
    ui.painter().text(
        egui::pos2(x, cy),
        egui::Align2::LEFT_CENTER,
        glyph,
        egui::FontId::proportional(10.5),
        color,
    );
}

/// 把名字摆进 `[x, right]` 那一格：太长就截断，绝不换行——
/// 行一高，后面所有行的缩进和底色就全错位了。
fn put_name(
    ui: &mut Ui,
    resp: &egui::Response,
    x: f32,
    right: f32,
    name: &str,
    size: f32,
    strong: bool,
) -> egui::Response {
    let rect = egui::Rect::from_min_max(
        egui::pos2(x, resp.rect.top()),
        egui::pos2(right.max(x + 24.0), resp.rect.bottom()),
    );
    let text = RichText::new(name.to_owned()).size(size);
    let text = if strong { text.strong() } else { text };
    left_label(ui, rect, text)
}

/// 在给定那一格里摆一段左对齐、放不下就截断的文字。
///
/// 不能用 `Ui::put`：它是「居中 + 撑满」，名字会被推到行的正中间，
/// 整棵树的列就歪了。这里换成左起排的横向布局，截断宽度也按这一格算。
fn left_label(ui: &mut Ui, rect: egui::Rect, text: RichText) -> egui::Response {
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
        |ui| {
            ui.add(
                egui::Label::new(text)
                    .truncate()
                    .halign(egui::Align::LEFT)
                    .selectable(false),
            )
        },
    )
    .inner
}

/// 行最右边的记号，从右往左一个个摆。返回下一个记号该摆到的 x。
fn trail_label(
    ui: &Ui,
    resp: &egui::Response,
    right: f32,
    text: &str,
    color: egui::Color32,
) -> f32 {
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), egui::FontId::proportional(9.5), color);
    let w = galley.size().x;
    ui.painter().galley(
        egui::pos2(right - w, resp.rect.center().y - galley.size().y * 0.5),
        galley,
        color,
    );
    right - w - 8.0
}

/// 读取失败那一行：说清哪儿坏了，旁边留一个真的能点的重试。
fn failed_row(ui: &mut Ui, depth: usize, err: &str, retry: impl FnOnce()) {
    let width = ui.available_width().max(120.0);
    let (rect, resp) = ui.allocate_exact_size(egui::Vec2::new(width, ROW_H), egui::Sense::hover());
    paint_row_shell(ui, &resp, depth);
    let color = ui.visuals().error_fg_color;
    let btn_w = 44.0;
    left_label(
        ui,
        egui::Rect::from_min_max(
            egui::pos2(rect.left() + 4.0 + INDENT * depth as f32, rect.top()),
            egui::pos2(rect.right() - btn_w - 4.0, rect.bottom()),
        ),
        RichText::new(err.to_owned()).size(11.0).color(color),
    );
    if ui
        .put(
            egui::Rect::from_min_size(
                egui::pos2(rect.right() - btn_w - 2.0, rect.center().y - 9.0),
                egui::vec2(btn_w, 18.0),
            ),
            egui::Button::new(RichText::new("重试").size(11.0)),
        )
        .clicked()
    {
        retry();
    }
}

/// 已发出、等回执的后台任务分别该落到哪儿。
enum Pending {
    Databases(String),
    Tables { conn: String, database: String },
    /// 连接管理窗口（或侧栏右键）里点的「测试连接」，按连接名落回 `probes`
    Probe(String),
}

/// 一次「测试连接」的结论。
///
/// 侧栏和编辑表单看的是同一份：从前只有表单里那行灰字，右键测完只在状态栏闪一下，
/// 到底连上没有得自己猜。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Probe {
    Running,
    Ok(String),
    Failed(String),
}

/// 结论 + 测它的时候那份配置的指纹（[`ConnectionSpec::stamp`]）。
/// 指纹对不上就说明中间改过字段，那个绿勾不能再接着亮。
#[derive(Clone, Debug)]
struct ProbeRecord {
    stamp: u64,
    state: Probe,
}

/// 管理窗口里正在编辑的那一条。
///
/// 编辑的是副本而不是 `connections` 里的原件：连接列表同时被侧栏树读着，
/// 边填表单边改原件会让树的键和行对不上。
#[derive(Clone)]
struct Draft {
    /// 改的是 `connections` 里第几条；`None` 表示新建，还没落地
    index: Option<usize>,
    spec: ConnectionSpec,
}

pub struct DatabasePanel {
    hub: DbHub,
    connections: Vec<ConnectionSpec>,
    trees: HashMap<String, ConnTree>,
    /// 侧栏里选中的连接。SQL 块头部没写 `conn=` 时发到这里。
    active: Option<String>,
    pending: HashMap<u64, Pending>,
    /// 连接名 -> 最近一次测试连接。改了配置要跟着重测，见 [`ProbeRecord`]。
    probes: HashMap<String, ProbeRecord>,
    /// 不属于这层的回执（SQL 块的结果），底部结果区每帧来取。
    ///
    /// 语句结果和库表列表共用一条通道、一套 id，这里只认领自己发出去的那些，
    /// 剩下的原样转交——不然得给 hub 再造一条通道，或者让两边抢着 poll。
    stray: Vec<DbEvent>,
    draft: Option<Draft>,
    show_manager: bool,
    /// 这次打开管理窗口时要不要摆回屏幕正中
    center_manager: bool,
    /// 给状态栏和管理窗口底部共用的一行反馈
    status: String,
}

impl DatabasePanel {
    pub fn new(connections: Vec<ConnectionSpec>) -> Self {
        Self::with_hub(DbHub::new(), connections)
    }

    /// 注入 hub：测试把驱动换成 `MockDriver`，生产走真实 MySQL。
    pub fn with_hub(hub: DbHub, connections: Vec<ConnectionSpec>) -> Self {
        let active = connections.first().map(|c| c.name.clone());
        Self {
            hub,
            connections,
            trees: HashMap::new(),
            active,
            pending: HashMap::new(),
            probes: HashMap::new(),
            stray: Vec::new(),
            draft: None,
            show_manager: false,
            center_manager: false,
            status: "SQL 块头部：-- SQL 标题 conn=连接名".into(),
        }
    }

    pub fn connections(&self) -> &[ConnectionSpec] {
        &self.connections
    }

    /// 有没有请求还挂在后台。界面靠它决定要不要继续重画——不然结果回来了也没人看。
    pub fn busy(&self) -> bool {
        !self.pending.is_empty()
    }

    /// 这条连接现在选中的是哪条：侧栏点过的那条，否则列表第一条。
    ///
    /// SQL 块头部没写 `conn=` 时，执行就发到这里。
    pub fn active_conn(&self) -> Option<&str> {
        self.active
            .as_deref()
            .or_else(|| self.connections.first().map(|c| c.name.as_str()))
    }

    /// 这条连接现在实际发往哪个库：树上点过谁就是谁，没点过则是配置里的默认库。
    ///
    /// 树、下拉、状态栏都说这一个数，免得三处各摆一个牌子却指向不同的库。
    pub fn current_db(&self, conn: &str) -> String {
        self.trees
            .get(conn)
            .and_then(|t| t.current_db.clone())
            .or_else(|| self.spec_of(conn).map(|s| s.database.clone()))
            .unwrap_or_default()
    }

    /// 实际交给驱动的那份配置。
    ///
    /// 树上换过当前库就按换后的发：hub 按整份配置判断连接是否过期，
    /// 库名不同会重开一条连接，这正是「发往另一个库」该有的意思。
    pub fn effective_spec(&self, conn: &str) -> Option<ConnectionSpec> {
        let base = self.spec_of(conn).cloned()?;
        Some(match self.trees.get(conn).and_then(|t| t.current_db.clone()) {
            Some(db) if db != base.database => ConnectionSpec { database: db, ..base },
            _ => base,
        })
    }

    /// 树上点库名：把它认作这条连接的当前库。不碰 config.json。
    fn set_current_db(&mut self, conn: &str, database: &str) {
        self.trees
            .entry(conn.to_string())
            .or_default()
            .current_db = Some(database.to_string());
        self.status = format!("「{conn}」的当前库已切到「{database}」");
    }

    /// 把一个块交给某条连接执行，返回 `Vec[(任务 id, 语句原文)]`（按执行顺序）。
    ///
    /// 切分交给 hub：它返回的语句就是实际送进服务器的那些，结果面板的标题、
    /// 「未执行」的个数都以它为准，两边不会各切各的数出不同的条数。
    pub fn run(&mut self, conn: &str, block: &str) -> Result<Vec<(u64, String)>, String> {
        let Some(spec) = self.effective_spec(conn) else {
            return Err(if self.connections.is_empty() {
                "还没配数据库连接，先在顶栏「数据库 → 连接管理」里加一个".into()
            } else {
                let list = self
                    .connections
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>()
                    .join("、");
                format!("没有叫「{conn}」的连接，现有：{list}")
            });
        };
        let parts = self
            .hub
            .run(&spec, block, MAX_ROWS)
            .map_err(|err| err.to_string())?;
        if !parts.is_empty() {
            self.status = format!("{} · 已提交 {} 条语句", conn, parts.len());
        }
        Ok(parts)
    }

    /// 取走不属于这一层的回执（也就是 SQL 块的语句结果），交给底部结果区。
    pub fn take_stray(&mut self) -> Vec<DbEvent> {
        std::mem::take(&mut self.stray)
    }

    pub fn open_manager(&mut self) {
        self.show_manager = true;
        self.center_manager = true;
    }

    /// 每帧调用：把后台回来的库表列表和测试回执归位。
    pub fn poll(&mut self) {
        for event in self.hub.poll() {
            let Some(pending) = self.pending.remove(&event.id) else {
                // 不是这层发出去的（SQL 块的结果），或者是连接在这帧之前被改过、
                // 删掉了 —— 都原样搁下，让上层取走认领得了的那部分。
                self.stray.push(event);
                continue;
            };
            let text = match &event.outcome {
                Ok(outcome) => outcome.describe(),
                Err(err) => err.clone(),
            };
            match pending {
                Pending::Databases(conn) => {
                    if let Some(tree) = self.trees.get_mut(&conn) {
                        tree.databases = to_load(&event.outcome);
                    }
                    self.status = format!("{conn} · {text}");
                }
                Pending::Tables { conn, database } => {
                    if let Some(tree) = self.trees.get_mut(&conn) {
                        if let Some(slot) = tree.tables.get_mut(&database) {
                            *slot = to_load(&event.outcome);
                        }
                    }
                    self.status = format!("{conn} / {database} · {text}");
                }
                Pending::Probe(conn) => {
                    let state = match &event.outcome {
                        Ok(_) => Probe::Ok(text.clone()),
                        Err(_) => Probe::Failed(text.clone()),
                    };
                    // 测完到收回来之间连接可能被删掉，那就没人在等这条结论了
                    if let Some(record) = self.probes.get_mut(&conn) {
                        record.state = state;
                    }
                    self.status = match &event.outcome {
                        Ok(_) => format!("「{conn}」可用：{text}"),
                        Err(_) => format!("「{conn}」连不上：{text}"),
                    };
                }
            }
        }
    }

    /// 展开或收起一条连接；第一次展开时才去问库列表。
    fn toggle_conn(&mut self, name: &str) {
        let open = {
            let tree = self.trees.entry(name.to_string()).or_default();
            tree.expanded = !tree.expanded;
            tree.expanded
        };
        if open {
            self.load_databases(name);
        }
    }

    fn toggle_db(&mut self, name: &str, database: &str) {
        let open = {
            let tree = self.trees.entry(name.to_string()).or_default();
            if tree.open.remove(database) {
                false
            } else {
                tree.open.insert(database.to_string());
                true
            }
        };
        if open {
            self.load_tables(name, database);
        }
    }

    fn load_databases(&mut self, name: &str) {
        let Some(spec) = self.spec_of(name).cloned() else {
            return;
        };
        {
            let tree = self.trees.entry(name.to_string()).or_default();
            if !tree.databases.needs_fetch() {
                return;
            }
            tree.databases = Load::Loading;
        }
        match self.hub.databases(&spec) {
            Ok(id) => {
                self.pending.insert(id, Pending::Databases(name.to_string()));
            }
            Err(err) => {
                // 配置不全这种事不用等后台线程绕一圈，就地写回错误
                let text = err.to_string();
                if let Some(tree) = self.trees.get_mut(name) {
                    tree.databases = Load::Failed(text.clone());
                }
                self.status = format!("{name} · {text}");
            }
        }
    }

    fn load_tables(&mut self, name: &str, database: &str) {
        let Some(spec) = self.spec_of(name).cloned() else {
            return;
        };
        let database = database.to_string();
        {
            let tree = self.trees.entry(name.to_string()).or_default();
            let slot = tree.tables.entry(database.clone()).or_default();
            if !slot.needs_fetch() {
                return;
            }
            *slot = Load::Loading;
        }
        match self.hub.tables(&spec, database.clone()) {
            Ok(id) => {
                self.pending.insert(
                    id,
                    Pending::Tables {
                        conn: name.to_string(),
                        database,
                    },
                );
            }
            Err(err) => {
                let text = err.to_string();
                if let Some(tree) = self.trees.get_mut(name) {
                    tree.tables.insert(database, Load::Failed(text.clone()));
                }
                self.status = format!("{name} · {text}");
            }
        }
    }

    /// 丢掉某条连接的缓存和工作线程。配置改了、删了、手工刷新都用它。
    fn forget(&mut self, name: &str) {
        self.trees.remove(name);
        self.hub.close(name);
    }

    /// 重新问服务器要库和表。展开状态得保留：刷完树自己收了很难用。
    fn refresh(&mut self, name: &str) {
        let expanded = self.trees.get(name).is_some_and(|t| t.expanded);
        self.hub.close(name);
        let tree = ConnTree { expanded, ..Default::default() };
        self.trees.insert(name.to_string(), tree);
        if expanded {
            self.load_databases(name);
        }
    }

    /// 按名字找连接配置。SQL 块头部的 `conn=` 和侧栏树的键都是这个名字。
    pub fn spec_of(&self, name: &str) -> Option<&ConnectionSpec> {
        self.connections.iter().find(|c| c.name == name)
    }

    /// 发一次「测试连接」。回执走 `poll`，所以这里只登记任务。
    fn test_connection(&mut self, spec: &ConnectionSpec) {
        let name = spec.name.clone();
        let stamp = spec.stamp();
        let record = |state: Probe| ProbeRecord { stamp, state };
        if let Some(field) = spec.missing() {
            // 没填全就别去麻烦后台线程，直接把该填哪一项说在结论那一格里
            self.probes.insert(name, record(Probe::Failed(format!("请先填写{field}"))));
            return;
        }
        match self.hub.probe(spec) {
            Ok(id) => {
                self.pending.insert(id, Pending::Probe(spec.name.clone()));
                self.probes.insert(name, record(Probe::Running));
                self.status = format!("正在测试 {}…", spec.name);
            }
            Err(err) => {
                let text = err.to_string();
                self.probes.insert(name, record(Probe::Failed(text.clone())));
                self.status = format!("连接失败：{text}");
            }
        }
    }

    /// 这条连接现在的测试结论。只在配置和当初测的那份一模一样时才给——
    /// 改过主机、密码之类还不重测就接着显示绿勾，比不显示更坏。
    fn probe_of(&self, spec: &ConnectionSpec) -> Option<&Probe> {
        self.probes
            .get(&spec.name)
            .filter(|record| record.stamp == spec.stamp())
            .map(|record| &record.state)
    }

    /// 给上层（工具栏、状态栏）看的测试结论。
    pub fn probe_state(&self, name: &str) -> Option<Probe> {
        self.spec_of(name).and_then(|spec| self.probe_of(spec).cloned())
    }

    /// 有过结论，但配置已经变了。
    fn probe_stale(&self, spec: &ConnectionSpec) -> bool {
        self.probes
            .get(&spec.name)
            .is_some_and(|record| record.stamp != spec.stamp())
    }

    /// 这条连接现在该显示的测试结论，图标、文案、颜色一起给。
    ///
    /// 侧栏的行和表单里那一块走同一份口径：同一个连接在两处说法不一，
    /// 用户只会当成都在瞎说。没有结论就没有这一行——从没测过的连接不该被暗示「坏了」。
    fn probe_view(
        &self,
        ui: &Ui,
        spec: &ConnectionSpec,
    ) -> Option<(&'static str, String, egui::Color32)> {
        let visuals = ui.visuals();
        let green = egui::Color32::from_rgb(70, 160, 100);
        Some(match self.probe_of(spec) {
            Some(Probe::Running) => ("⏳", "正在测试…".into(), visuals.hyperlink_color),
            Some(Probe::Ok(info)) => ("✓", format!("连接可用 · {info}"), green),
            Some(Probe::Failed(err)) => ("✗", format!("连不上：{err}"), visuals.error_fg_color),
            // 测过之后又改了字段：与其把上次的绿勾接着亮，不如直说那个结果不作数了
            None if self.probe_stale(spec) => (
                "⚠",
                "配置改过了，上次结果不作数".into(),
                visuals.warn_fg_color,
            ),
            None => return None,
        })
    }

    /// 表单里那一行测试结论，整块铺色。
    ///
    /// 单独占一行是有原因的：从前它挤在「测试连接」按钮边上，11 号灰字，
    /// 用户点完根本不知道有没有生效。这一屏上「连上了没」是唯一要紧的答案。
    fn probe_line(&self, ui: &mut Ui, spec: &ConnectionSpec) {
        let Some((icon, text, color)) = self.probe_view(ui, spec) else {
            return;
        };
        let dark = ui.visuals().dark_mode;
        let alpha = if dark { 40 } else { 28 };
        let fill = egui::Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha);
        egui::Frame::new()
            .fill(fill)
            .corner_radius(4.0)
            .inner_margin(egui::Margin::symmetric(6, 3))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(icon).size(12.0).strong().color(color));
                    // 报错原因往往一大串，给成可选中才能直接抄去查
                    ui.label(RichText::new(text).size(11.5).strong().color(color));
                });
            });
    }

    fn name_taken(&self, name: &str, except: Option<usize>) -> bool {
        self.connections
            .iter()
            .enumerate()
            .any(|(i, c)| Some(i) != except && c.name == name)
    }

    fn begin_new(&mut self) {
        // 给个不重名的默认名：空名会直接卡在必填项上，而多数人懒得改
        let spec = ConnectionSpec {
            name: format!("连接{}", self.connections.len() + 1),
            ..Default::default()
        };
        self.draft = Some(Draft {
            index: None,
            spec,
        });
        self.show_manager = true;
    }

    fn begin_edit(&mut self, index: usize) {
        let Some(spec) = self.connections.get(index).cloned() else {
            return;
        };
        self.draft = Some(Draft {
            index: Some(index),
            spec,
        });
    }

    /// 把草稿落成配置。校验不过就原地返回，表单里的东西一个都不丢。
    fn save_draft(&mut self) {
        let Some(draft) = self.draft.clone() else {
            return;
        };
        let spec = ConnectionSpec {
            name: draft.spec.name.trim().to_string(),
            kind: draft.spec.kind,
            host: draft.spec.host.trim().to_string(),
            port: draft.spec.port,
            user: draft.spec.user.trim().to_string(),
            password: draft.spec.password.clone(),
            database: draft.spec.database.trim().to_string(),
        };
        if let Some(field) = spec.missing() {
            self.status = format!("连接配置缺少{field}");
            return;
        }
        if self.name_taken(&spec.name, draft.index) {
            self.status = format!("已经有名叫「{}」的连接", spec.name);
            return;
        }
        let old = match draft.index {
            Some(i) => {
                let old = self.connections[i].clone();
                self.connections[i] = spec.clone();
                old
            }
            None => {
                self.connections.push(spec.clone());
                spec.clone()
            }
        };
        // 改过配置的连接，旧线程和旧列表都不再作数（改名时新旧名字一起收）
        self.forget(&old.name);
        self.forget(&spec.name);
        // 测试结论是按名字存的：改名后把它挪到新名字上，绿勾不该因为一次改名就消失。
        // 但结论本身按配置指纹认领，存完再对着新配置核验，改过密码那份 ✓ 自然不亮。
        let tested = self
            .probes
            .remove(&draft.spec.name)
            .or_else(|| self.probes.remove(&old.name));
        self.probes.remove(&old.name);
        if let Some(record) = tested {
            self.probes.insert(spec.name.clone(), record);
        }
        if self.active.as_deref() == Some(old.name.as_str()) {
            self.active = Some(spec.name.clone());
        }
        self.draft = None;
        self.status = format!("已保存连接「{}」· {}", spec.name, spec.target());
    }

    fn cancel_draft(&mut self) {
        let Some(draft) = self.draft.take() else {
            return;
        };
        // 没保存过的连接也可能被「测试连接」起了线程，顺手收掉
        if !self.connections.iter().any(|c| c.name == draft.spec.name) {
            self.hub.close(&draft.spec.name);
            // 结论也别留着：这个名字下面马上会长出另一条不相干的连接
            self.probes.remove(&draft.spec.name);
        }
    }

    fn delete(&mut self, index: usize) {
        let Some(spec) = self.connections.get(index).cloned() else {
            return;
        };
        self.connections.remove(index);
        self.forget(&spec.name);
        self.probes.remove(&spec.name);
        if self.active.as_deref() == Some(spec.name.as_str()) {
            self.active = self.connections.first().map(|c| c.name.clone());
        }
        // 删除让后面的下标整体前移，草稿记得指向哪条要跟着修
        let drop_draft = self.draft.as_ref().is_some_and(|d| d.index == Some(index));
        if drop_draft {
            self.draft = None;
        } else if let Some(draft) = self.draft.as_mut() {
            if let Some(i) = draft.index {
                if i > index {
                    draft.index = Some(i - 1);
                }
            }
        }
        self.status = format!("已删除连接「{}」", spec.name);
    }

    /// 侧栏「数据库」页签的内容。
    pub fn sidebar(&mut self, ui: &mut Ui) {
        // 标题行和文件树那头一个写法：按钮先占住右边，标题最后拿剩下的宽度。
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add(egui::Button::new(RichText::new("管理").size(11.5)).frame(false))
                    .on_hover_text("增删改连接，配置写在 config.json")
                    .clicked()
                {
                    self.open_manager();
                }
                let active = self.active.clone();
                if ui
                    .add(egui::Button::new(RichText::new("刷新").size(11.5)).frame(false))
                    .on_hover_text("重新读取当前选中连接的库和表")
                    .clicked()
                {
                    if let Some(name) = active {
                        self.refresh(&name);
                    }
                }
                ui.label(RichText::new("数据库连接").strong().size(12.0));
            });
        });
        ui.separator();

        if self.connections.is_empty() {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("还没有数据库连接").weak());
                if ui.button("新建连接").clicked() {
                    self.begin_new();
                }
            });
            self.status_row(ui);
            return;
        }

        // SQL 到底发去哪条连接的哪个库，是这棵树最该一眼看见的两件事，
        // 所以单独框一块，而不是靠某一行底色去暗示。
        // 认 `active_conn()` 而不是 `active`：一条都没点过时，发号处其实落在列表第一条，
        // 这一块必须和那个事实一致，否则「看得见的连接」和「真正那条」会劈叉。
        if let Some(active) = self.active_conn().map(str::to_string) {
            // 说的是「生效的那份配置」：树上换过当前库，这一行得跟着换，
            // 不然它指着配置里的旧库，树里却亮着另一个。
            let eff = self.effective_spec(&active);
            let target = eff.as_ref().map(|s| s.target()).unwrap_or_default();
            let database = self.current_db(&active);
            let color = ui.visuals().hyperlink_color;
            egui::Frame::group(ui.style())
                .inner_margin(egui::Margin::same(6))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("默认连接").size(10.5).weak());
                            ui.label(
                                RichText::new(format!("{active} · {target}"))
                                    .size(11.0)
                                    .strong()
                                    .color(color),
                            );
                        });
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("当前库").size(10.5).weak());
                            ui.label(
                                RichText::new(if database.is_empty() {
                                    "（配置里没填库）".to_owned()
                                } else {
                                    database.clone()
                                })
                                .size(11.0)
                                .strong()
                                .color(color),
                            );
                        });
                    });
                });
            ui.add_space(2.0);
        }

        egui::ScrollArea::vertical()
            .id_salt("db_tree")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // 整份克隆出来再遍历：行渲染里要改 self，借着一份 Vec 改不动
                let specs = self.connections.clone();
                for spec in specs {
                    self.conn_rows(ui, &spec);
                }
            });
        self.status_row(ui);
    }

    /// 树底下的一行反馈。后台线程说的话总得有个地方露出来，
    /// 不然「测试连接」点下去像没反应。
    fn status_row(&self, ui: &mut Ui) {
        ui.separator();
        ui.label(RichText::new(self.status.clone()).size(10.5).weak());
    }

    fn conn_rows(&mut self, ui: &mut Ui, spec: &ConnectionSpec) {
        let name = spec.name.clone();
        let expanded = self.trees.get(&name).is_some_and(|t| t.expanded);
        let selected = self.active_conn() == Some(name.as_str());
        let probe = self.probe_view(ui, spec);

        let (resp, x) = tree_row(ui, 0);
        let cy = resp.rect.center().y;
        let accent = ui.visuals().hyperlink_color;
        if selected {
            // 底色、主色图标、右边的牌子，三处一起指认「SQL 就发到这里」
            ui.painter()
                .rect_filled(resp.rect, 3.0, accent.gamma_multiply(0.13));
        }
        arrow_glyph(ui, cy, x, expanded);
        tree_icon(ui, cy, x + ARROW_W, CONN_ICON, selected, accent);

        // 行尾记号从右往左摆，名字拿剩下的那一截：再长的连接名也不会把牌子挤出面板
        let mut right = resp.rect.right() - 8.0;
        if let Some((icon, _, color)) = &probe {
            right = trail_label(ui, &resp, right, icon, *color);
        }
        if selected {
            right = trail_label(ui, &resp, right, "默认", accent);
        }
        let hover = format!(
            "{} · {}\n点名字 = 设为默认连接（SQL 块没写 conn= 就发到这里）\n点箭头 = {}",
            spec.kind.label(),
            spec.target(),
            if expanded { "收起库列表" } else { "展开库列表" }
        );
        put_name(ui, &resp, x + ARROW_W + ICON_W, right, &name, 12.5, true)
            .on_hover_text(hover);

        if resp.clicked() {
            if hit_arrow(&resp, x) {
                self.toggle_conn(&name);
            } else {
                self.active = Some(name.clone());
                self.status = format!("已把「{name}」设为默认连接");
                if !expanded {
                    self.toggle_conn(&name);
                }
            }
        }
        resp.context_menu(|ui| {
            if ui.button("刷新库列表").clicked() {
                self.refresh(&name);
                ui.close();
            }
            if ui.button("测试连接").clicked() {
                let saved = self.spec_of(&name).cloned();
                ui.close();
                if let Some(saved) = saved {
                    self.test_connection(&saved);
                }
            }
            ui.separator();
            if ui.button("编辑连接…").clicked() {
                let index = self.connections.iter().position(|c| c.name == name);
                ui.close();
                if let Some(index) = index {
                    self.begin_edit(index);
                    self.open_manager();
                }
            }
        });

        if !expanded {
            return;
        }
        let (databases, open) = {
            let Some(tree) = self.trees.get(&name) else {
                return;
            };
            (tree.databases.clone(), tree.open.clone())
        };
        match &databases {
            // 刚展开时 `toggle_conn` 已经发过请求，这里没什么可显示失败的
            Load::Idle => {}
            Load::Loading => muted_row(ui, 1, "读取库列表…"),
            Load::Failed(err) => failed_row(ui, 1, err, || self.load_databases(&name)),
            Load::Ready(list) => {
                let current = self.current_db(&name);
                for database in list {
                    let is_current = *database == current;
                    self.db_rows(ui, spec, database, open.contains(database), is_current);
                }
            }
        }
    }

    fn db_rows(
        &mut self,
        ui: &mut Ui,
        spec: &ConnectionSpec,
        database: &str,
        is_open: bool,
        is_current: bool,
    ) {
        let conn = spec.name.clone();
        let db = database.to_string();

        let (resp, x) = tree_row(ui, 1);
        let cy = resp.rect.center().y;
        let accent = ui.visuals().hyperlink_color;
        if is_current {
            ui.painter()
                .rect_filled(resp.rect, 3.0, accent.gamma_multiply(0.10));
        }
        arrow_glyph(ui, cy, x, is_open);
        tree_icon(ui, cy, x + ARROW_W, DB_ICON, is_current, accent);
        let mut right = resp.rect.right() - 8.0;
        if is_current {
            right = trail_label(ui, &resp, right, "当前", accent);
        }
        let hover = format!(
            "{}\n点名字 = 设为这条连接的当前库（不写 conn= 的 SQL 就发到这里）\n点箭头 = {}",
            db,
            if is_open { "收起表列表" } else { "展开表列表" }
        );
        put_name(ui, &resp, x + ARROW_W + ICON_W, right, &db, 12.0, false)
            .on_hover_text(hover);

        if resp.clicked() {
            if hit_arrow(&resp, x) {
                self.toggle_db(&conn, &db);
            } else {
                if !is_current {
                    self.set_current_db(&conn, &db);
                }
                if !is_open {
                    self.toggle_db(&conn, &db);
                }
            }
        }

        if !is_open {
            return;
        }
        let state = self
            .trees
            .get(&conn)
            .and_then(|t| t.tables.get(&db))
            .cloned()
            .unwrap_or_default();
        match &state {
            Load::Idle | Load::Loading => muted_row(ui, 2, "读取表列表…"),
            Load::Failed(err) => failed_row(ui, 2, err, || self.load_tables(&conn, &db)),
            Load::Ready(list) => {
                for table in list {
                    table_row(ui, &db, table);
                }
            }
        }
    }

    /// 「连接管理」窗口。
    pub fn manager_window(&mut self, ctx: &egui::Context) {
        if !self.show_manager {
            return;
        }
        let mut open = true;
        let win_id = egui::Id::unique("db_manager");
        let mut window = egui::Window::new("数据库连接")
            .id(win_id)
            .open(&mut open)
            .collapsible(false)
            .auto_sized();
        // 只在刚打开的那一帧居中，之后窗口归用户摆。理由见 terminal_settings_window。
        if self.center_manager && ctx.memory(|m| m.area_rect(win_id).is_some()) {
            window = window.anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO);
            self.center_manager = false;
        }
        window.show(ctx, |ui| {
            // 路径和错误串都可能很长，不锁死宽度会把 auto_sized 的窗口撑到屏幕外
            ui.set_max_width(460.0);
            ui.set_min_width(460.0);
            self.manager_list(ui);
            ui.separator();
            self.manager_form(ui);
        });
        self.show_manager = open;
    }

    fn manager_list(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("已保存的连接（{}）", self.connections.len()))
                    .strong()
                    .size(12.0),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("新建").clicked() {
                    self.begin_new();
                }
            });
        });
        if self.connections.is_empty() {
            muted_row(ui, 0, "还没有连接。点「新建」加一条。");
            return;
        }
        egui::ScrollArea::vertical()
            .id_salt("db_manager_list")
            .max_height(150.0)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let rows: Vec<(usize, String, String)> = self
                    .connections
                    .iter()
                    .enumerate()
                    .map(|(i, c)| (i, c.name.clone(), c.target()))
                    .collect();
                let editing = self.draft.as_ref().and_then(|d| d.index);
                let rows_width = (ui.available_width() - 70.0).max(80.0);
                for (index, name, target) in rows {
                    ui.horizontal(|ui| {
                        let row = ui
                            .add(
                                egui::Button::selectable(
                                    editing == Some(index),
                                    RichText::new(&name).size(12.0),
                                )
                                .min_size(egui::vec2(rows_width, 0.0)),
                            )
                            .on_hover_text(&target);
                        if row.clicked() {
                            self.begin_edit(index);
                        }
                        if ui.small_button("删除").clicked() {
                            self.delete(index);
                        }
                    });
                }
            });
    }

    fn manager_form(&mut self, ui: &mut Ui) {
        let Some(draft) = self.draft.clone() else {
            muted_row(ui, 0, "选中上面一条连接来编辑，或点「新建」。");
            return;
        };
        let mut spec = draft.spec;
        let mut secret = split_secret(&spec.password);

        let title = if draft.index.is_some() {
            "编辑连接"
        } else {
            "新建连接"
        };
        ui.label(RichText::new(title).strong().size(12.0));

        ui.label(weak("名称（SQL 块里 conn= 用的就是这个名字）"));
        ui.add(
            egui::TextEdit::singleline(&mut spec.name)
                .hint_text("本地开发库")
                .desired_width(f32::INFINITY),
        );

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(weak("引擎"));
            egui::ComboBox::from_id_salt("db_kind")
                .selected_text(spec.kind.label())
                .show_ui(ui, |ui| {
                    for kind in DbKind::all() {
                        if ui.selectable_label(spec.kind == *kind, kind.label()).clicked() {
                            spec.kind = *kind;
                            spec.port = kind.default_port();
                        }
                    }
                });
            ui.label(weak("主机"));
            ui.add(
                egui::TextEdit::singleline(&mut spec.host)
                    .hint_text("127.0.0.1")
                    .desired_width(120.0),
            );
            ui.label(weak("端口"));
            ui.add(egui::DragValue::new(&mut spec.port).range(1..=65535).speed(1));
        });

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(weak("用户"));
            ui.add(
                egui::TextEdit::singleline(&mut spec.user)
                    .hint_text("root")
                    .desired_width(110.0),
            );
            ui.label(weak("密码"));
            ui.add(
                egui::TextEdit::singleline(&mut secret.1)
                    .hint_text(if secret.0 { "变量名" } else { "留空 = 空密码" })
                    .password(!secret.0)
                    .desired_width(150.0),
            );
        });
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            let mut from_env = secret.0;
            if ui
                .checkbox(&mut from_env, "密码从环境变量读")
                .on_hover_text("勾上后配置里只存变量名，明文密码不落盘")
                .changed()
            {
                // 从明文切到变量名：把原来的密码清掉，别让它躺在明文输入框里
                if from_env {
                    secret.1.clear();
                }
                secret.0 = from_env;
            }
        });

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(weak("默认库"));
            ui.add(
                egui::TextEdit::singleline(&mut spec.database)
                    .hint_text("留空 = 连上后看 SHOW DATABASES")
                    .desired_width(220.0),
            );
        });

        if let Some(field) = spec.missing() {
            ui.colored_label(ui.visuals().error_fg_color, format!("请填写：{field}"));
        }
        if !spec.password.is_empty() && !spec.password.starts_with("env:") {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                "密码是明文，会原样写进 config.json",
            );
        }

        ui.add_space(6.0);
        self.probe_line(ui, &spec);

        ui.add_space(2.0);
        let mut test = false;
        let mut save = false;
        let mut cancel = false;
        ui.horizontal(|ui| {
            if ui
                .button("测试连接")
                .on_hover_text("只问一句版本号，不动任何数据")
                .clicked()
            {
                test = true;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("取消").clicked() {
                    cancel = true;
                }
                if ui.button("保存").clicked() {
                    save = true;
                }
            });
        });

        // 先写回草稿再动按钮：「测试连接」和「保存」用的都该是这一帧刚填的值
        spec.password = join_secret(secret.0, &secret.1);
        if let Some(current) = self.draft.as_mut() {
            current.spec = spec.clone();
        }
        if test {
            self.test_connection(&spec);
        }
        if save {
            self.save_draft();
        }
        if cancel {
            self.cancel_draft();
        }

        ui.add_space(2.0);
        muted_row(
            ui,
            0,
            &format!("配置写在 {}", crate::config::Config::path().display()),
        );
    }
}

/// 表名一行。整行只有名字，双击或右键复制，暂时不碰编辑器——结果区是下一步的事。
fn table_row(ui: &mut Ui, database: &str, table: &str) {
    let qualified = format!("`{database}`.`{table}`");
    let (resp, x) = tree_row(ui, 2);
    let cy = resp.rect.center().y;
    tree_icon(ui, cy, x + ARROW_W, TABLE_ICON, false, ui.visuals().hyperlink_color);
    put_name(
        ui,
        &resp,
        x + ARROW_W + ICON_W,
        resp.rect.right() - 8.0,
        table,
        12.0,
        false,
    )
    .on_hover_text(format!("双击复制 {qualified}；右键可复制查询语句"));
    if resp.double_clicked() {
        ui.ctx().copy_text(qualified.clone());
    }
    resp.context_menu(|ui| {
        if ui.button("复制表名").clicked() {
            ui.ctx().copy_text(qualified.clone());
            ui.close();
        }
        if ui.button("复制 SELECT 语句").clicked() {
            ui.ctx()
                .copy_text(format!("SELECT * FROM {qualified} LIMIT 100;"));
            ui.close();
        }
    });
}

/// 树里那句不带操作的话：正在读、还没读出来、或者压根没有连接。
fn muted_row(ui: &mut Ui, depth: usize, text: &str) {
    let width = ui.available_width().max(120.0);
    let (rect, resp) = ui.allocate_exact_size(egui::Vec2::new(width, ROW_H), egui::Sense::hover());
    paint_row_shell(ui, &resp, depth);
    ui.painter().text(
        egui::pos2(rect.left() + 4.0 + INDENT * depth as f32, rect.center().y),
        egui::Align2::LEFT_CENTER,
        text,
        egui::FontId::proportional(11.0),
        ui.visuals().weak_text_color(),
    );
}

fn weak(text: &str) -> RichText {
    RichText::new(text.to_string()).size(11.0).weak()
}

/// 密码框拆成「是否 env 间接」+「框里显示的内容」。
fn split_secret(password: &str) -> (bool, String) {
    match password.strip_prefix("env:") {
        Some(rest) => (true, rest.to_string()),
        None => (false, password.to_string()),
    }
}

fn join_secret(from_env: bool, value: &str) -> String {
    if from_env {
        format!("env:{}", value.trim())
    } else {
        value.to_string()
    }
}

/// 后台回执落到哪一层列表。只有一串名字的产出才用得上。
fn to_load(outcome: &Result<DbOutcome, String>) -> Load {
    match outcome {
        Ok(DbOutcome::Names(names)) => Load::Ready(names.clone()),
        Ok(other) => Load::Failed(other.describe()),
        Err(err) => Load::Failed(err.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{DbError, Driver, MockDriver};
    use std::sync::{Arc, Mutex, OnceLock};

    /// 只有加载成功才有名字可看。断言用得上，生产代码用不上（渲染是直接 match 的）。
    fn loaded(load: &Load) -> Option<&[String]> {
        match load {
            Load::Ready(list) => Some(list),
            _ => None,
        }
    }

    fn spec(name: &str) -> ConnectionSpec {
        ConnectionSpec {
            name: name.into(),
            host: "127.0.0.1".into(),
            port: 3306,
            user: "root".into(),
            password: "env:MOCK_NEVER_SET".into(),
            database: "demo".into(),
            kind: DbKind::Mysql,
        }
    }

    /// 假驱动的记账表，按连接名分栏。
    ///
    /// 为什么放静态：`DbHub` 收的 `ConnectFn` 是函数指针，闭包带不了捕获，
    /// 共享状态只能挂在连接名上。每个测试用自己独有的名字，互不串台。
    /// 连接名 -> 那条连接的记账表。类型写出来免得一行里塞四层嵌套。
    type Sinks = Mutex<HashMap<String, Arc<Mutex<Vec<String>>>>>;

    fn sink(name: &str) -> Arc<Mutex<Vec<String>>> {
        static SINKS: OnceLock<Sinks> = OnceLock::new();
        let table = SINKS.get_or_init(Default::default);
        let mut guard = table.lock().expect("记账表不中毒");
        guard.entry(name.to_string()).or_default().clone()
    }

    fn sent(name: &str) -> Vec<String> {
        sink(name).lock().expect("记账锁不中毒").clone()
    }

    /// 主机以 `bad` 开头的连接一律失败，用来测错误落回树里。
    fn make_driver(config: &ConnectionSpec) -> Result<Box<dyn Driver>, DbError> {
        let mut mock = MockDriver::new();
        mock.executed = sink(&config.name);
        if config.host.starts_with("bad") {
            mock.fail = Some("2003 · 连接被拒绝".into());
            mock.fail_reconnects = true;
        }
        Ok(Box::new(mock))
    }

    fn panel(names: &[&str]) -> DatabasePanel {
        DatabasePanel::with_hub(
            DbHub::with_driver(make_driver),
            names.iter().map(|n| spec(n)).collect(),
        )
    }

    /// 等后台线程把话说完。轮询而不是睡死，慢机器上也不会偶发失败。
    fn settle(panel: &mut DatabasePanel) {
        for _ in 0..2000 {
            panel.poll();
            if panel.pending.is_empty() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        panic!("后台任务始终没有回执");
    }

    #[test]
    fn expanding_and_collapsing_fetches_databases_once() {
        let mut panel = panel(&["树展开"]);
        panel.toggle_conn("树展开");
        panel.toggle_conn("树展开");
        panel.toggle_conn("树展开");
        settle(&mut panel);
        // 展开→收起→再展开，只该问一次服务器
        assert_eq!(sent("树展开"), vec!["SHOW DATABASES".to_string()]);
        let tree = panel.trees.get("树展开").expect("展开过就该有树");
        // 奇数次点击，最后一次是展开；请求只发过一次才是重点
        assert!(tree.expanded);
        assert_eq!(
            loaded(&tree.databases).map(|l| l.join(",")).as_deref(),
            Some("information_schema,demo")
        );
    }

    #[test]
    fn tables_are_fetched_per_database() {
        let mut panel = panel(&["库和表"]);
        panel.toggle_conn("库和表");
        settle(&mut panel);
        panel.toggle_db("库和表", "demo");
        settle(&mut panel);
        let tree = panel.trees.get("库和表").expect("树在");
        let demo = tree.tables.get("demo").expect("demo 的表列表该在树上");
        assert_eq!(
            loaded(demo).map(|l| l.join(",")).as_deref(),
            Some("demo_users,orders")
        );
        assert_eq!(sent("库和表")[1], "SHOW TABLES IN demo");
    }

    #[test]
    fn a_failing_fetch_lands_in_the_tree_and_can_retry() {
        let mut panel = panel(&[]);
        panel.connections.push(ConnectionSpec {
            host: "bad-host".into(),
            ..spec("坏连接")
        });
        panel.load_databases("坏连接");
        settle(&mut panel);
        let tree = panel.trees.get("坏连接").expect("树在");
        match &tree.databases {
            Load::Failed(text) => assert!(text.contains("连接被拒绝"), "错误串要原样留着：{text}"),
            other => panic!("失败该记在树上，不然界面永远转圈：{other:?}"),
        }
        assert!(tree.databases.needs_fetch(), "失败之后必须能重试");
    }

    #[test]
    fn probe_result_is_claimed_by_the_connection_that_asked() {
        let mut panel = panel(&["测试连接"]);
        panel.begin_edit(0);
        let draft_spec = panel.draft.clone().expect("草稿").spec;
        panel.test_connection(&draft_spec);
        assert!(
            matches!(panel.probe_of(&draft_spec), Some(Probe::Running)),
            "点下去那一帧就该有反应，不然等回执的几秒像没点上"
        );
        settle(&mut panel);
        assert_eq!(
            panel.probe_of(&draft_spec),
            Some(&Probe::Ok("8.0.33-mock".into()))
        );
        assert!(
            panel.status.contains("可用"),
            "状态栏也要说：{}",
            panel.status
        );
    }

    #[test]
    fn a_failed_probe_stays_visible_as_a_failure() {
        let mut panel = panel(&[]);
        let bad = ConnectionSpec {
            host: "bad-host".into(),
            ..spec("坏连接")
        };
        panel.connections.push(bad.clone());
        panel.test_connection(&bad);
        settle(&mut panel);
        match panel.probe_of(&bad) {
            Some(Probe::Failed(text)) => assert!(text.contains("连接被拒绝"), "{text}"),
            other => panic!("失败得留在结论里，不能只在状态栏闪一下：{other:?}"),
        }
    }

    #[test]
    fn editing_the_config_retires_the_previous_verdict() {
        let mut panel = panel(&["测试连接"]);
        let saved = spec("测试连接");
        panel.test_connection(&saved);
        settle(&mut panel);
        assert!(panel.probe_of(&saved).is_some());
        let mut edited = saved.clone();
        edited.host = "10.0.0.1".into();
        assert!(
            panel.probe_of(&edited).is_none(),
            "换了主机还亮绿勾等于骗人"
        );
        assert!(
            panel.probe_stale(&edited),
            "但得说清那是过期，不是从没测过"
        );
    }

    #[test]
    fn renaming_keeps_the_verdict_and_saving_moves_it_to_the_new_name() {
        let mut panel = panel(&["旧名"]);
        let saved = spec("旧名");
        panel.test_connection(&saved);
        settle(&mut panel);
        panel.begin_edit(0);
        panel.draft.as_mut().expect("草稿").spec.name = "新名".into();
        panel.save_draft();
        assert!(
            panel.probe_of(&spec("新名")).is_some(),
            "改个显示名不该把测过的结论丢掉"
        );
        assert!(!panel.probes.contains_key("旧名"), "旧名字下不该留孤儿");
    }

    #[test]
    fn cancelling_an_unsaved_draft_drops_its_verdict() {
        let mut panel = panel(&[]);
        panel.begin_new();
        panel.draft.as_mut().expect("草稿").spec = spec("临时连接");
        let draft = panel.draft.clone().expect("草稿").spec;
        panel.test_connection(&draft);
        settle(&mut panel);
        assert!(panel.probe_of(&draft).is_some(), "先测出个结论来");
        panel.cancel_draft();
        assert!(
            panel.probes.is_empty(),
            "没保存的连接，结论留着只会坑下一条同名连接"
        );
    }

    #[test]
    fn deleting_a_connection_drops_its_verdict() {
        let mut panel = panel(&["测试连接"]);
        let saved = spec("测试连接");
        panel.test_connection(&saved);
        settle(&mut panel);
        panel.delete(0);
        assert!(panel.probes.is_empty(), "连接没了，结论也该跟着走");
    }

    #[test]
    fn a_probe_on_a_dead_connection_is_dropped_not_replayed() {
        // 回执还在路上的时候连接被删了：那条结论不能凭空造出一个孤儿
        let mut panel = panel(&["测试连接"]);
        let saved = spec("测试连接");
        panel.test_connection(&saved);
        panel.delete(0);
        settle(&mut panel);
        assert!(panel.probes.is_empty(), "孤儿结论：{:?}", panel.probes);
    }

    #[test]
    fn incomplete_form_is_refused_before_it_reaches_the_hub() {
        let mut panel = panel(&[]);
        panel.begin_new();
        panel.draft.as_mut().expect("草稿").spec.name.clear();
        panel.save_draft();
        assert!(panel.connections.is_empty(), "缺名称不该能保存");
        assert!(panel.status.contains("名称"), "状态要说清缺什么");
        assert!(panel.draft.is_some(), "保存失败得留着草稿，让人接着填");
        assert!(panel.hub.connections().is_empty(), "校验没过就不该起线程");
    }

    #[test]
    fn duplicate_names_are_refused_and_rename_follows_active() {
        let mut panel = panel(&["旧名", "线上"]);
        panel.active = Some("旧名".into());
        panel.begin_edit(0);
        panel.draft.as_mut().expect("草稿").spec.name = "线上".into();
        panel.save_draft();
        assert_eq!(panel.connections.len(), 2);
        assert_eq!(panel.connections[0].name, "旧名", "重名要挡在保存这一步");

        panel.begin_edit(0);
        panel.draft.as_mut().expect("草稿").spec.name = "新名".into();
        panel.save_draft();
        assert!(panel.draft.is_none(), "保存完草稿该关掉");
        assert_eq!(panel.active.as_deref(), Some("新名"), "改了名，选中的连接要跟着走");
        assert!(!panel.trees.contains_key("新名"), "改名后旧缓存不能接着画");
        assert!(!panel.hub.has_connection("旧名"), "旧名字的工作线程要收掉");
    }

    #[test]
    fn deleting_a_row_reindexes_the_open_draft() {
        let mut panel = panel(&["甲", "乙", "丙"]);
        panel.begin_edit(2);
        panel.delete(0);
        assert_eq!(panel.draft.as_ref().expect("草稿").index, Some(1));
        assert_eq!(panel.connections[1].name, "丙");
        panel.delete(1);
        assert!(panel.draft.is_none(), "删掉正在编辑的那条，草稿就该关");
        assert_eq!(panel.connections.len(), 1);
    }

    #[test]
    fn password_switches_between_plaintext_and_env_name() {
        assert_eq!(split_secret("env:DB_PW"), (true, "DB_PW".into()));
        assert_eq!(split_secret("hunter2"), (false, "hunter2".into()));
        assert_eq!(join_secret(true, " DB_PW "), "env:DB_PW");
        assert_eq!(join_secret(false, "hunter2"), "hunter2");
        // 存了再读回来不能变形，否则用户填一次就坏一次
        let (from_env, value) = split_secret("env:DB_PW");
        assert_eq!(join_secret(from_env, &value), "env:DB_PW");
    }

    #[test]
    fn saving_trims_the_fields() {
        let mut panel = panel(&[]);
        panel.begin_new();
        {
            let draft = panel.draft.as_mut().expect("草稿");
            draft.spec.name = "  本地  ".into();
            draft.spec.host = "  10.1.1.1 ".into();
            draft.spec.user = " app ".into();
            draft.spec.database = " demo ".into();
        }
        panel.save_draft();
        let saved = &panel.connections[0];
        assert_eq!((saved.name.as_str(), saved.host.as_str()), ("本地", "10.1.1.1"));
        assert_eq!((saved.user.as_str(), saved.database.as_str()), ("app", "demo"));
    }

    #[test]
    fn cancelled_draft_closes_the_thread_it_probed() {
        let mut panel = panel(&[]);
        panel.begin_new();
        panel.draft.as_mut().expect("草稿").spec.name = "临时".into();
        let draft_spec = panel.draft.clone().expect("草稿").spec;
        panel.test_connection(&draft_spec);
        assert!(panel.hub.has_connection("临时"));
        panel.cancel_draft();
        assert!(!panel.hub.has_connection("临时"), "没保存的连接不能留个线程");
    }

    #[test]
    fn refresh_keeps_the_tree_expanded_and_asks_again() {
        let mut panel = panel(&["刷新"]);
        panel.toggle_conn("刷新");
        settle(&mut panel);
        assert!(panel.trees["刷新"].expanded);
        panel.refresh("刷新");
        settle(&mut panel);
        assert!(panel.trees["刷新"].expanded, "刷完树还该是展开的");
        assert_eq!(sent("刷新"), vec!["SHOW DATABASES", "SHOW DATABASES"]);
    }

    #[test]
    fn deleting_a_connection_takes_its_tree_and_worker_with_it() {
        let mut panel = panel(&["删我", "留着"]);
        panel.toggle_conn("删我");
        settle(&mut panel);
        assert!(panel.hub.has_connection("删我"));
        panel.delete(0);
        assert!(!panel.trees.contains_key("删我"));
        assert!(!panel.hub.has_connection("删我"));
        assert_eq!(panel.active.as_deref(), Some("留着"), "选中的连接该顺延到下一条");
    }

    /// 侧栏单独画一帧。指针状态在 begin_pass 就算好了，只能事前塞进 RawInput。
    fn sidebar_pass(
        ctx: &egui::Context,
        panel: &mut DatabasePanel,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(420.0, 620.0),
            )),
            events,
            ..Default::default()
        };
        let mut out = ctx.run_pass(raw, |ctx| {
            ctx.root_ui(|ui| panel.sidebar(ui));
        });
        // 没有真的渲染器消费字体图集的增量，epaint 会在 drop 时断言
        out.textures_delta.clear();
        out
    }

    fn walk_text(shape: &egui::Shape, v: &mut Vec<(String, egui::Rect)>) {
        match shape {
            egui::Shape::Text(text) => {
                v.push((
                    text.galley.job.text.clone(),
                    text.galley.rect.translate(text.pos.to_vec2()),
                ))
            }
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

    /// 把侧栏单独画一帧，收它往屏上丢的所有文字。
    ///
    /// 测试结论、默认连接这些最后都落成一个记号加一行小字。
    /// 只断言 `probes` 里有条目证明不了用户看得见——画没画出来得看这一帧。
    fn sidebar_texts(panel: &mut DatabasePanel) -> Vec<String> {
        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(1.0);
        let out = sidebar_pass(&ctx, panel, vec![]);
        seen(&out).into_iter().map(|(t, _)| t).collect()
    }

    /// 在侧栏里点中某段文字（按段落中心命中）。点到返回 true。
    ///
    /// 按下和松开必须分两帧，且用的是同一个 Context，不然 egui 认不出这是一次点击。
    fn sidebar_click(panel: &mut DatabasePanel, needle: &str) -> bool {
        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(1.0);
        let mut point = |what: &str| {
            seen(&sidebar_pass(&ctx, panel, vec![]))
                .into_iter()
                .find(|(t, _)| t == what)
                .map(|(_, r)| r.center())
        };
        let Some(pos) = point(needle) else {
            return false;
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
        sidebar_pass(&ctx, panel, at(true));
        sidebar_pass(&ctx, panel, at(false));
        true
    }

    #[test]
    fn the_sidebar_marks_the_connection_sql_will_actually_use() {
        let mut panel = panel(&["一库", "二库"]);
        let texts = sidebar_texts(&mut panel);
        assert!(
            texts.iter().any(|t| t == "默认连接"),
            "侧栏没说是哪条连接在收 SQL：{texts:?}"
        );
        // 一条都没点过时，发号处落在列表第一条——牌子和 header 都得跟着这个事实
        assert_eq!(
            texts.iter().filter(|t| *t == "默认").count(),
            1,
            "「默认」牌子只能有一块：{texts:?}"
        );
        assert!(
            texts.iter().any(|t| t.starts_with("一库")),
            "第一条没进屏：{texts:?}"
        );

        panel.active = Some("二库".into());
        let texts = sidebar_texts(&mut panel);
        assert_eq!(texts.iter().filter(|t| *t == "默认").count(), 1);
        assert!(
            texts.iter().any(|t| t.starts_with("二库 · ")),
            "header 没跟着换：{texts:?}"
        );
    }

    #[test]
    fn a_tested_connection_shows_its_verdict_in_the_sidebar() {
        let mut panel = panel(&["结论上屏"]);
        let good = panel.connections[0].clone();
        assert!(
            !sidebar_texts(&mut panel).iter().any(|t| t == "✓"),
            "没测过的连接不该提前打勾"
        );
        panel.test_connection(&good);
        settle(&mut panel);
        assert!(
            sidebar_texts(&mut panel).iter().any(|t| t == "✓"),
            "测通了侧栏却看不到，用户只能再点一次"
        );

        // 改过配置，绿勾得当场降级成「不作数」
        panel.connections[0].database = "别的库".into();
        let texts = sidebar_texts(&mut panel);
        assert!(
            texts.iter().any(|t| t == "⚠") && !texts.iter().any(|t| t == "✓"),
            "改了库名还挂着绿勾：{texts:?}"
        );
    }

    /// 树里点库名：当前库换了、牌子上屏、发往的配置也跟着换。
    ///
    /// 「看得见」和「真正发往」必须是同一个数，所以三条一起断言。
    #[test]
    fn clicking_a_database_in_the_tree_moves_where_sql_goes() {
        let mut panel = panel(&["点库"]);
        panel.toggle_conn("点库");
        settle(&mut panel);
        assert_eq!(panel.current_db("点库"), "demo", "没点过就该跟着配置走");

        assert!(
            sidebar_click(&mut panel, "information_schema"),
            "树里没点到库名"
        );
        assert_eq!(panel.current_db("点库"), "information_schema");
        assert_eq!(
            panel
                .effective_spec("点库")
                .expect("连接在")
                .database
                .as_str(),
            "information_schema",
            "树上的当前库没影响到真正发出去的配置"
        );
        assert_eq!(
            panel.spec_of("点库").expect("连接在").database,
            "demo",
            "点两下就把 config.json 的默认库改了，比不改更糟"
        );
        let texts = sidebar_texts(&mut panel);
        assert!(
            texts.iter().any(|t| t == "当前库") && texts.iter().any(|t| t == "information_schema"),
            "卡片没说当前库是谁：{texts:?}"
        );
        assert_eq!(
            texts.iter().filter(|t| *t == "当前").count(),
            1,
            "行尾的「当前」牌子只能有一块：{texts:?}"
        );
    }

    /// 箭头那一列只管展开收起，不该顺手把当前库换掉。
    #[test]
    fn the_arrow_toggles_without_touching_the_current_database() {
        let mut panel = panel(&["点箭头"]);
        panel.toggle_conn("点箭头");
        settle(&mut panel);
        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(1.0);
        let out = sidebar_pass(&ctx, &mut panel, vec![]);
        let db_line = seen(&out)
            .into_iter()
            .find(|(t, _)| t == "information_schema")
            .map(|(_, r)| r)
            .expect("库名该在屏上");
        // 箭头列在这一行最左边，横向坐标就是它
        let pos = egui::pos2(db_line.min.x - 24.0, db_line.center().y);
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
        sidebar_pass(&ctx, &mut panel, at(true));
        sidebar_pass(&ctx, &mut panel, at(false));
        // 箭头那一列只做展开这一件事：库列表读出来了，当前库却一个字节没动
        assert!(
            panel
                .trees
                .get("点箭头")
                .expect("树在")
                .open
                .contains("information_schema"),
            "点在箭头上却没展开表列表"
        );
        assert_eq!(
            panel.current_db("点箭头"),
            "demo",
            "点箭头不该把当前库换掉"
        );
    }

    #[test]
    fn a_failed_probe_shows_a_cross_in_the_sidebar() {
        let mut panel = panel(&[]);
        let bad = ConnectionSpec {
            host: "bad-host".into(),
            ..spec("坏了的连接")
        };
        panel.connections.push(bad.clone());
        panel.test_connection(&bad);
        settle(&mut panel);
        assert!(
            sidebar_texts(&mut panel).iter().any(|t| t == "✗"),
            "连不上得一眼看见"
        );
    }
}
