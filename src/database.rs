//! 「数据库」侧栏页签 + 连接管理窗口。
//!
//! 边界：这一层只管「有哪些连接、连不连得上、里面有什么」。
//! SQL 块头部的文本格式在 `crate::sql`，引擎与线程编排在 `crate::db`，
//! 结果网格在 `crate::app` 的底部结果区。
//!
//! 树只有三层（连接 → 库 → 表），每层的键还各不相同，所以这里按层写死，
//! 不套文件树那种递归渲染：递归的代价是每层都要造一个路径式的 id，
//! 三层固定结构换不来任何东西。

use std::collections::{HashMap, HashSet};

use eframe::egui::{self, RichText, Ui};

use crate::db::{ConnectionSpec, DbHub, DbKind, DbOutcome};

/// 子层缩进（库 1 格、表 2 格）。
const INDENT: f32 = 14.0;

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
    /// 库名 -> 该库的表
    tables: HashMap<String, Load>,
}

/// 已发出、等回执的后台任务分别该落到哪儿。
enum Pending {
    Databases(String),
    Tables { conn: String, database: String },
    /// 连接管理窗口里的「测试连接」
    Probe,
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
    /// 「测试连接」的回执：`None` = 没测过
    probe: Option<String>,
}

pub struct DatabasePanel {
    hub: DbHub,
    connections: Vec<ConnectionSpec>,
    trees: HashMap<String, ConnTree>,
    /// 侧栏里选中的连接。SQL 块头部没写 `conn=` 时发到这里。
    active: Option<String>,
    pending: HashMap<u64, Pending>,
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

    pub fn open_manager(&mut self) {
        self.show_manager = true;
        self.center_manager = true;
    }

    /// 每帧调用：把后台回来的库表列表和测试回执归位。
    pub fn poll(&mut self) {
        for event in self.hub.poll() {
            let Some(pending) = self.pending.remove(&event.id) else {
                // 连接在这一帧之前被改过或删掉了，结果没人认领
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
                Pending::Probe => {
                    if let Some(draft) = self.draft.as_mut() {
                        draft.probe = Some(text.clone());
                    }
                    self.status = match &event.outcome {
                        Ok(_) => format!("连接可用：{text}"),
                        Err(_) => format!("连接失败：{text}"),
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
        let Some(spec) = self.connection(name).cloned() else {
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
        let Some(spec) = self.connection(name).cloned() else {
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

    fn connection(&self, name: &str) -> Option<&ConnectionSpec> {
        self.connections.iter().find(|c| c.name == name)
    }

    /// 发一次「测试连接」。回执走 `poll`，所以这里只登记任务。
    fn test_connection(&mut self, spec: &ConnectionSpec) {
        if let Some(field) = spec.missing() {
            if let Some(draft) = self.draft.as_mut() {
                draft.probe = Some(format!("请先填写{field}"));
            }
            return;
        }
        match self.hub.probe(spec) {
            Ok(id) => {
                self.pending.insert(id, Pending::Probe);
                if let Some(draft) = self.draft.as_mut() {
                    draft.probe = Some("连接中…".into());
                }
                self.status = format!("正在测试 {}…", spec.name);
            }
            Err(err) => {
                let text = err.to_string();
                if let Some(draft) = self.draft.as_mut() {
                    draft.probe = Some(text.clone());
                }
                self.status = format!("连接失败：{text}");
            }
        }
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
            probe: None,
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
            probe: None,
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
        }
    }

    fn delete(&mut self, index: usize) {
        let Some(spec) = self.connections.get(index).cloned() else {
            return;
        };
        self.connections.remove(index);
        self.forget(&spec.name);
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
        ui.horizontal(|ui| {
            ui.add_space(2.0);
            ui.label(RichText::new("数据库连接").strong().size(12.0));
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
        let selected = self.active.as_deref() == Some(name.as_str());
        let rows_width = (ui.available_width() - 70.0).max(60.0);

        ui.horizontal(|ui| {
            ui.add_space(2.0);
            let arrow = if expanded { "▾" } else { "▸" };
            if ui
                .add(egui::Button::new(RichText::new(arrow).size(11.0)).frame(false))
                .on_hover_text(if expanded { "收起" } else { "展开库列表" })
                .clicked()
            {
                self.toggle_conn(&name);
            }
            let row = ui
                .add(
                    egui::Button::selectable(selected, RichText::new(&name).size(12.5).strong())
                        .min_size(egui::vec2(rows_width, 0.0)),
                )
                .on_hover_text(format!(
                    "{} · {}\n点行 = 设为默认连接（SQL 块没写 conn= 就发到这里）",
                    spec.kind.label(),
                    spec.target()
                ));
            if row.clicked() {
                self.active = Some(name.clone());
            }
            row.context_menu(|ui| {
                if ui.button("刷新库列表").clicked() {
                    self.refresh(&name);
                    ui.close();
                }
                if ui.button("测试连接").clicked() {
                    let saved = self.connection(&name).cloned();
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
            Load::Failed(err) => {
                ui.horizontal(|ui| {
                    ui.add_space(INDENT);
                    let color = ui.visuals().error_fg_color;
                    ui.label(RichText::new(err.clone()).size(11.0).color(color));
                    if ui.small_button("重试").clicked() {
                        self.load_databases(&name);
                    }
                });
            }
            Load::Ready(list) => {
                for database in list {
                    self.db_rows(ui, spec, database, open.contains(database));
                }
            }
        }
    }

    fn db_rows(&mut self, ui: &mut Ui, spec: &ConnectionSpec, database: &str, is_open: bool) {
        let conn = spec.name.clone();
        let db = database.to_string();
        let is_default = spec.database == db;
        let rows_width = (ui.available_width() - 84.0).max(60.0);

        ui.horizontal(|ui| {
            ui.add_space(INDENT);
            let arrow = if is_open { "▾" } else { "▸" };
            if ui
                .add(egui::Button::new(RichText::new(arrow).size(10.5)).frame(false))
                .clicked()
            {
                self.toggle_db(&conn, &db);
            }
            let row = ui
                .add(
                    egui::Button::selectable(is_open, RichText::new(&db).size(12.0))
                        .min_size(egui::vec2(rows_width, 0.0)),
                )
                .on_hover_text(if is_default {
                    "这个连接默认打开的库；展开看它的表"
                } else {
                    "展开看这个库里的表"
                });
            if row.clicked() {
                self.toggle_db(&conn, &db);
            }
        });

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
            Load::Failed(err) => {
                ui.horizontal(|ui| {
                    ui.add_space(INDENT * 2.0);
                    let color = ui.visuals().error_fg_color;
                    ui.label(RichText::new(err.clone()).size(11.0).color(color));
                    if ui.small_button("重试").clicked() {
                        self.load_tables(&conn, &db);
                    }
                });
            }
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
            if let Some(probe) = &draft.probe {
                ui.label(RichText::new(probe.clone()).size(11.0).weak());
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

/// 表名一行。双击或右键复制，暂时不碰编辑器——结果区是下一步的事。
fn table_row(ui: &mut Ui, database: &str, table: &str) {
    let qualified = format!("`{database}`.`{table}`");
    ui.horizontal(|ui| {
        ui.add_space(INDENT * 2.0);
        let row = ui
            .label(RichText::new(table).size(12.0))
            .on_hover_text(format!("双击复制 {qualified}；右键可复制查询语句"));
        if row.double_clicked() {
            ui.ctx().copy_text(qualified.clone());
        }
        row.context_menu(|ui| {
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
    });
}

fn muted_row(ui: &mut Ui, depth: usize, text: &str) {
    ui.horizontal(|ui| {
        ui.add_space(INDENT * depth as f32);
        ui.label(RichText::new(text).size(11.0).weak().italics());
    });
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
    fn probe_result_lands_in_the_draft() {
        let mut panel = panel(&["测试连接"]);
        panel.begin_edit(0);
        let draft_spec = panel.draft.clone().expect("草稿").spec;
        panel.test_connection(&draft_spec);
        settle(&mut panel);
        let draft = panel.draft.as_ref().expect("草稿还在");
        assert_eq!(draft.probe.as_deref(), Some("8.0.33-mock"));
        assert!(panel.status.contains("连接可用"), "状态栏也要说：{}", panel.status);
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
}
