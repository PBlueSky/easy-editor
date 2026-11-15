//! 数据库访问层：引擎无关的数据类型 + 一条连接一个后台线程。
//!
//! 分三层：
//! - `DbKind` / `ConnectionSpec`：连哪个库、怎么连，能序列化进 config.json；
//! - `Driver`：一个引擎要实现的最小接口，目前只有 MySQL（`db::mysql`）；
//! - `DbHub`：按连接名常驻后台线程，把查询丢过去、每帧把完成的结果收回来。
//!
//! 为什么是「一条连接一个线程」而不是 `Arc<Mutex<dyn Driver>>`：MySQL 的连接对象
//! 不是 `Sync`，而且它本身就是一个有状态的会话（事务、当前库、字符集）。锁起来让几个
//! 线程轮流用同一个会话是假并发，还会互相串味；一个线程独占一个会话既能复用 TCP 和
//! 认证握手，又天然把同一连接上的查询排成串行。
//!
//! 本模块不碰 egui，也不管 SQL 块的文本格式（那在 `crate::sql`）。

use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, Sender};

use serde::{Deserialize, Serialize};

pub mod mysql;

/// 一个结果面板最多显示多少行。再多没人看得完，还白占内存；
/// 要全量就自己在 SQL 里分页。
///
/// 注意这截的是**显示**行数，不是网络传输量：mysql 客户端库在结果集被 drop 时
/// 必须把剩下的行读完才能让连接回到可用状态。所以 `SELECT *` 一个千万行的表，
/// 就算只画 1000 行，该等的传输还是得等。真要限量只能在 SQL 里写 LIMIT。
pub const MAX_ROWS: usize = 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DbKind {
    Mysql,
}

impl DbKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Mysql => "MySQL",
        }
    }

    /// 加新引擎时：这里加一项，`connect` 里加一条分支，其余全不用动。
    pub fn all() -> &'static [DbKind] {
        &[Self::Mysql]
    }

    pub fn default_port(self) -> u16 {
        match self {
            Self::Mysql => 3306,
        }
    }
}

/// 一个命名连接。`name` 就是 SQL 块里 `conn=` 引用的那个键。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConnectionSpec {
    pub name: String,
    pub kind: DbKind,
    pub host: String,
    pub port: u16,
    pub user: String,
    /// 密码。推荐写成 `env:变量名`，这样 config.json 里不落明文；
    /// 直接写明文也能用，但那个文件是同用户可读的，自己权衡。
    pub password: String,
    pub database: String,
}

impl Default for ConnectionSpec {
    fn default() -> Self {
        Self {
            name: String::new(),
            kind: DbKind::Mysql,
            host: "127.0.0.1".into(),
            port: 3306,
            user: "root".into(),
            password: String::new(),
            database: String::new(),
        }
    }
}

impl ConnectionSpec {
    /// 把 `env:` 间接引用还原成真正的密码。
    ///
    /// 变量名先原样查，查不到再试应用统一前缀 `EASY_EDITOR_`，
    /// 所以 `env:DB_PW` 既认 `DB_PW` 也认 `EASY_EDITOR_DB_PW`。
    pub fn resolve_password(&self) -> String {
        let Some(raw) = self.password.strip_prefix("env:") else {
            return self.password.clone();
        };
        let key = raw.trim();
        if key.is_empty() {
            return String::new();
        }
        std::env::var(key)
            .ok()
            .or_else(|| crate::config::env_var(key))
            .unwrap_or_default()
    }

    /// 给人看的连接串，绝不含密码。
    pub fn target(&self) -> String {
        let db = if self.database.is_empty() {
            String::new()
        } else {
            format!("/{}", self.database)
        };
        format!("{}@{}:{}{db}", self.user, self.host, self.port)
    }

    /// 配置是否够连。返回 None 表示没问题，否则是缺哪一项——
    /// 给表单直接告诉他该填哪儿，比只回一个 bool 有用。
    pub fn missing(&self) -> Option<&'static str> {
        if self.name.trim().is_empty() {
            Some("名称")
        } else if self.host.trim().is_empty() {
            Some("主机")
        } else if self.user.trim().is_empty() {
            Some("用户名")
        } else {
            None
        }
    }

    pub fn is_complete(&self) -> bool {
        self.missing().is_none()
    }
}

/// 一列的定义。`type_name` 只做表头提示，不参与渲染逻辑。
#[derive(Clone, Debug, PartialEq)]
pub struct Column {
    pub name: String,
    pub type_name: String,
}

/// 一个单元格。
///
/// 刻意不带数字类型：文本协议下 MySQL 把所有列都送回字节串，这里再造一套
/// 「按列类型解析成 f64」的活儿只会引入精度争议。网格和 CSV 都只要文本，
/// 但必须保住两条信息——NULL 不等于空串，二进制列不该被硬塞进 String。
#[derive(Clone, Debug, PartialEq)]
pub enum Cell {
    Null,
    Text(String),
    /// 非 UTF-8 的原始字节（BLOB、BIT 之类）
    Blob(Vec<u8>),
}

impl Cell {
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    /// 网格里显示的文本。NULL 显示成灰色是 UI 的事，这里只给占位文字。
    pub fn display(&self) -> std::borrow::Cow<'_, str> {
        match self {
            Self::Null => "NULL".into(),
            Self::Text(text) => text.as_str().into(),
            Self::Blob(bytes) => format!("‹{} 字节›", bytes.len()).into(),
        }
    }

    /// 从数据库拿到的字节造一个单元格：能按 UTF-8 解释的当文本，否则当二进制。
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        match String::from_utf8(bytes) {
            Ok(text) => Self::Text(text),
            Err(err) => Self::Blob(err.into_bytes()),
        }
    }
}

/// 一条语句的执行结果。一个块里的多条语句各出一个，正好对应「同时展示多个结果」。
#[derive(Clone, Debug)]
pub struct StatementResult {
    /// 空 = 这不是查询（写操作），只按 `affected_rows` 报数
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Cell>>,
    /// 查询是返回行数，写操作是受影响行数
    pub affected_rows: u64,
    pub last_insert_id: u64,
    pub warnings: u16,
    /// 因为 `MAX_ROWS` 被截断过
    pub truncated: bool,
    pub elapsed_ms: u128,
    /// 这条语句的开头几个字，给结果面板当标题
    pub preview: String,
}

impl StatementResult {
    pub fn is_query(&self) -> bool {
        !self.columns.is_empty()
    }

    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// 没有语句可跑时的占位结果，头部信息全空。
    pub fn empty(preview: &str) -> Self {
        Self {
            columns: Vec::new(),
            rows: Vec::new(),
            affected_rows: 0,
            last_insert_id: 0,
            warnings: 0,
            truncated: false,
            elapsed_ms: 0,
            preview: preview.to_string(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct DbError {
    /// 服务端原文已经够长了，不再包一层
    pub message: String,
    /// MySQL 错误码，1045 / 2003 之类。UI 靠它区分「密码错」和「连不上」
    pub code: Option<u16>,
    /// 这个错意味着会话不可信，必须重连才能继续
    pub needs_reconnect: bool,
}

impl DbError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: None,
            needs_reconnect: false,
        }
    }
}

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.code {
            Some(code) => write!(f, "{code} · {}", self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

impl std::error::Error for DbError {}

/// 一个后台任务的产出。
#[derive(Clone, Debug)]
pub enum DbOutcome {
    /// 按语句顺序，一条一个结果
    Statements(Vec<StatementResult>),
    /// `SHOW DATABASES` / 表名列表这类只回一串名字的
    Names(Vec<String>),
    /// 「测试连接」的回执，一句能证明连上了的话，比如版本号
    Message(String),
}

impl DbOutcome {
    pub fn describe(&self) -> String {
        match self {
            Self::Statements(list) => {
                let rows: usize = list.iter().map(|s| s.row_count()).sum();
                let affected: u64 = list.iter().map(|s| s.affected_rows).sum();
                let ms: u128 = list.iter().map(|s| s.elapsed_ms).sum();
                format!("{} 条语句 · {rows} 行 · 影响 {affected} 行 · {ms} ms", list.len())
            }
            Self::Names(names) => format!("{} 项", names.len()),
            Self::Message(text) => text.clone(),
        }
    }
}

/// 一个引擎要实现的最小接口。调用方保证同一实例只被一个线程用。
pub trait Driver: Send {
    /// 逐条执行，结果按语句顺序返回。中途失败就停，已经跑掉的语句不回滚——
    /// 块级事务不是这一层能替用户猜的，要事务就自己在 SQL 里写 `START TRANSACTION`。
    fn execute(&mut self, sql: &str, max_rows: usize) -> Result<Vec<StatementResult>, DbError>;

    fn list_databases(&mut self) -> Result<Vec<String>, DbError>;
    fn list_tables(&mut self, database: &str) -> Result<Vec<String>, DbError>;

    /// 「测试连接」按钮用：回一句能证明连上了的话，比如版本号。
    fn probe(&mut self) -> Result<String, DbError>;
}

/// 建驱动的工厂。刻意做成函数指针而不是让 `DbHub` 直接认 `mysql::connect`：
/// 测试要能把它换成 `MockDriver`，线程编排才能在没有 MySQL 的机器上跑。
type ConnectFn = fn(&ConnectionSpec) -> Result<Box<dyn Driver>, DbError>;

enum Work {
    Execute { sql: String, max_rows: usize },
    Databases,
    Tables { database: String },
    Probe,
}

struct Job {
    id: u64,
    work: Work,
}

/// 后台线程送回的消息
enum Msg {
    Done {
        id: u64,
        conn: String,
        outcome: DbOutcome,
    },
    Failed {
        id: u64,
        conn: String,
        error: String,
    },
}

/// 轮询到的一条完成事件。
#[derive(Debug)]
pub struct DbEvent {
    /// `submit` 的返回值，UI 用它把结果认领到对应的结果面板
    pub id: u64,
    pub conn: String,
    pub outcome: Result<DbOutcome, String>,
}

/// 按连接名常驻的后台线程集合。
pub struct DbHub {
    connect: ConnectFn,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    /// 连接名 -> (建线程时用的配置, 该工作线程的收件箱)。
    /// 配置改过就重开一条线程，省掉「往旧线程发重连指令」这套额外协议。
    workers: HashMap<String, (ConnectionSpec, Sender<Job>)>,
    next_id: u64,
}

impl DbHub {
    pub fn new() -> Self {
        // 写全 self:: 是为了不和外部 crate `mysql` 混：本文件里两个名字都在指东西
        Self::with_driver(self::mysql::connect)
    }

    pub fn with_driver(connect: ConnectFn) -> Self {
        let (tx, rx) = channel();
        Self {
            connect,
            tx,
            rx,
            workers: HashMap::new(),
            next_id: 1,
        }
    }

    /// 提交一次执行，返回任务 id。
    pub fn run(&mut self, spec: &ConnectionSpec, sql: String, max_rows: usize) -> Result<u64, DbError> {
        self.submit(
            spec,
            Work::Execute {
                sql,
                max_rows: max_rows.clamp(1, MAX_ROWS),
            },
        )
    }

    pub fn databases(&mut self, spec: &ConnectionSpec) -> Result<u64, DbError> {
        self.submit(spec, Work::Databases)
    }

    pub fn tables(&mut self, spec: &ConnectionSpec, database: String) -> Result<u64, DbError> {
        self.submit(spec, Work::Tables { database })
    }

    pub fn probe(&mut self, spec: &ConnectionSpec) -> Result<u64, DbError> {
        self.submit(spec, Work::Probe)
    }

    fn submit(&mut self, spec: &ConnectionSpec, work: Work) -> Result<u64, DbError> {
        if let Some(field) = spec.missing() {
            return Err(DbError::new(format!("连接配置缺少{field}")));
        }
        let id = self.next_id;
        self.next_id += 1;

        let stale = self
            .workers
            .get(&spec.name)
            .is_some_and(|(used, _)| used != spec);
        if stale {
            // 配置改过了。丢掉旧线程的发送端，它会因为通道断开自己退出。
            self.workers.remove(&spec.name);
        }
        if !self.workers.contains_key(&spec.name) {
            let tx = self.spawn(spec.clone());
            self.workers.insert(spec.name.clone(), (spec.clone(), tx));
        }
        let tx = self.workers[&spec.name].1.clone();
        // 工作线程只可能在 hub 被 drop 后退出，那时整个应用也要走了，不特别处理
        let _ = tx.send(Job { id, work });
        Ok(id)
    }

    fn spawn(&self, spec: ConnectionSpec) -> Sender<Job> {
        let (job_tx, job_rx) = channel::<Job>();
        let reply = self.tx.clone();
        let connect = self.connect;
        std::thread::spawn(move || {
            let mut driver: Option<Box<dyn Driver>> = None;
            while let Ok(job) = job_rx.recv() {
                // 连不上不算致命：把失败报回去，下个任务再试一次，
                // 免得一时的网络抖动把这条连接的线程整个带走。
                if driver.is_none() {
                    match connect(&spec) {
                        Ok(fresh) => driver = Some(fresh),
                        Err(err) => {
                            let _ = reply.send(Msg::Failed {
                                id: job.id,
                                conn: spec.name.clone(),
                                error: err.to_string(),
                            });
                            continue;
                        }
                    }
                }
                let target = match driver.as_mut() {
                    Some(d) => d,
                    None => continue,
                };
                let outcome = match job.work {
                    Work::Execute { sql, max_rows } => {
                        target.execute(&sql, max_rows).map(DbOutcome::Statements)
                    }
                    Work::Databases => target.list_databases().map(DbOutcome::Names),
                    Work::Tables { database } => target.list_tables(&database).map(DbOutcome::Names),
                    Work::Probe => target.probe().map(DbOutcome::Message),
                };
                let sent = match outcome {
                    Ok(outcome) => Msg::Done {
                        id: job.id,
                        conn: spec.name.clone(),
                        outcome,
                    },
                    Err(err) => {
                        // 会话级错误之后这个连接不可信，丢掉等下个任务重连。
                        // SQL 本身的错误（语法、字段不存在）不在此列，连接照常能用。
                        if err.needs_reconnect {
                            driver = None;
                        }
                        Msg::Failed {
                            id: job.id,
                            conn: spec.name.clone(),
                            error: err.to_string(),
                        }
                    }
                };
                let _ = reply.send(sent);
            }
        });
        job_tx
    }

    /// 每帧调用：把后台完成的结果搬回前台。
    ///
    /// 这里不需要像全局搜索那样限流：一条消息就是一整个结果，行数已经在上游截过，
    /// 一帧能攒下的最多是「同时按了几个执行」。
    pub fn poll(&mut self) -> Vec<DbEvent> {
        let mut out = Vec::new();
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Done { id, conn, outcome } => out.push(DbEvent {
                    id,
                    conn,
                    outcome: Ok(outcome),
                }),
                Msg::Failed { id, conn, error, .. } => {
                    out.push(DbEvent {
                        id,
                        conn,
                        outcome: Err(error),
                    })
                }
            }
        }
        out
    }

    pub fn has_connection(&self, name: &str) -> bool {
        self.workers.contains_key(name)
    }

    /// 当前挂着工作线程的连接名。
    pub fn connections(&self) -> Vec<String> {
        self.workers.keys().cloned().collect()
    }

    /// 关掉某个连接的工作线程（删除或改名连接配置时用）。
    pub fn close(&mut self, name: &str) {
        self.workers.remove(name);
    }
}

impl Default for DbHub {
    fn default() -> Self {
        Self::new()
    }
}

/// 语句开头 60 个字符，压掉换行，给结果面板当标题。
pub fn preview_of(sql: &str) -> String {
    const LIMIT: usize = 60;
    let flat = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= LIMIT {
        return flat;
    }
    let mut cut: String = flat.chars().take(LIMIT - 1).collect();
    cut.push('…');
    cut
}

/// 假驱动：不连任何库，用来在没有 MySQL 的机器上测线程编排和结果渲染。
///
/// 内容刻意照着真 MySQL 的文本协议：所有值都是 `Bytes`，NULL 就是 NULL。
pub struct MockDriver {
    /// 非空时所有调用都失败
    pub fail: Option<String>,
    /// 失败时是否声明「这条会话废了」，用来测 hub 的重连分支
    pub fail_reconnects: bool,
    pub rows: usize,
    pub executed: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl MockDriver {
    pub fn new() -> Self {
        Self {
            fail: None,
            fail_reconnects: false,
            rows: 2,
            executed: Default::default(),
        }
    }

    pub fn failing(message: &str) -> Self {
        Self {
            fail: Some(message.to_string()),
            ..Self::new()
        }
    }

    fn err(&self) -> DbError {
        DbError {
            message: self.fail.clone().unwrap_or_default(),
            code: Some(1064),
            needs_reconnect: self.fail_reconnects,
        }
    }
}

impl Default for MockDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl Driver for MockDriver {
    fn execute(&mut self, sql: &str, max_rows: usize) -> Result<Vec<StatementResult>, DbError> {
        if self.fail.is_some() {
            return Err(self.err());
        }
        self.executed
            .lock()
            .expect("MockDriver 的记账锁不会中毒")
            .push(sql.to_string());
        Ok(crate::sql::split_statements(sql)
            .into_iter()
            .map(|stmt| {
                let is_read = crate::sql::classify(&stmt).is_read_only();
                let total = self.rows.min(max_rows);
                StatementResult {
                    columns: if is_read {
                        vec![
                            Column {
                                name: "id".into(),
                                type_name: "LONGLONG".into(),
                            },
                            Column {
                                name: "name".into(),
                                type_name: "VARCHAR".into(),
                            },
                        ]
                    } else {
                        Vec::new()
                    },
                    rows: (0..total)
                        .map(|i| {
                            vec![
                                Cell::Text((i + 1).to_string()),
                                // 第 3 行留一个 NULL，让渲染层有机会显示灰色 NULL
                                if i == 2 {
                                    Cell::Null
                                } else {
                                    Cell::Text(format!("行{i}"))
                                },
                            ]
                        })
                        .collect(),
                    affected_rows: if is_read { total as u64 } else { self.rows as u64 },
                    last_insert_id: if is_read { 0 } else { 42 },
                    warnings: 0,
                    truncated: self.rows > max_rows,
                    elapsed_ms: 1,
                    preview: preview_of(&stmt),
                }
            })
            .collect())
    }

    fn list_databases(&mut self) -> Result<Vec<String>, DbError> {
        if self.fail.is_some() {
            return Err(self.err());
        }
        Ok(vec!["information_schema".into(), "demo".into()])
    }

    fn list_tables(&mut self, database: &str) -> Result<Vec<String>, DbError> {
        if self.fail.is_some() {
            return Err(self.err());
        }
        Ok(vec![format!("{database}_users"), "orders".into()])
    }

    fn probe(&mut self) -> Result<String, DbError> {
        if self.fail.is_some() {
            return Err(self.err());
        }
        Ok("8.0.33-mock".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn cell_keeps_null_and_binary_apart() {
        assert!(Cell::Null.is_null());
        assert_eq!(Cell::Null.display(), "NULL");
        assert_eq!(Cell::Text(String::new()).display(), "");
        // 空串和 NULL 必须在网格里区分得开
        assert_ne!(Cell::Text(String::new()), Cell::Null);
        let blob = Cell::from_bytes(vec![0xff, 0xfe]);
        assert_eq!(blob, Cell::Blob(vec![0xff, 0xfe]));
        assert_eq!(blob.display(), "‹2 字节›");
        assert_eq!(Cell::from_bytes("中文".into()), Cell::Text("中文".into()));
    }

    #[test]
    fn target_never_leaks_the_password() {
        let s = ConnectionSpec {
            password: "hunter2".into(),
            ..spec("demo")
        };
        assert_eq!(s.target(), "root@127.0.0.1:3306/demo");
        assert!(!s.target().contains("hunter2"));
        let no_db = ConnectionSpec {
            database: String::new(),
            ..s
        };
        assert_eq!(no_db.target(), "root@127.0.0.1:3306");
    }

    #[test]
    fn env_password_indirection() {
        std::env::set_var("EASY_EDITOR_TEST_DB_PW", "from-env");
        let s = ConnectionSpec {
            password: "env:TEST_DB_PW".into(),
            ..spec("demo")
        };
        assert_eq!(s.resolve_password(), "from-env");
        std::env::set_var("BARE_TEST_DB_PW", "bare");
        let bare = ConnectionSpec {
            password: "env:BARE_TEST_DB_PW".into(),
            ..spec("demo")
        };
        assert_eq!(bare.resolve_password(), "bare", "不带前缀的变量名也要认");
        // 变量不存在时给空串，而不是把 "env:XXX" 本身当密码发出去
        let missing = ConnectionSpec {
            password: "env:NOT_SET_ANYWHERE".into(),
            ..spec("demo")
        };
        assert_eq!(missing.resolve_password(), "");
        let empty = ConnectionSpec {
            password: "env:".into(),
            ..spec("demo")
        };
        assert_eq!(empty.resolve_password(), "");
        let plain = ConnectionSpec {
            password: "plain".into(),
            ..spec("demo")
        };
        assert_eq!(plain.resolve_password(), "plain");
        std::env::remove_var("EASY_EDITOR_TEST_DB_PW");
        std::env::remove_var("BARE_TEST_DB_PW");
    }

    #[test]
    fn incomplete_spec_names_the_missing_field() {
        assert_eq!(spec("demo").missing(), None);
        assert!(spec("demo").is_complete());
        assert_eq!(
            ConnectionSpec {
                host: String::new(),
                ..spec("demo")
            }
            .missing(),
            Some("主机")
        );
        assert!(!ConnectionSpec {
            user: "  ".into(),
            ..spec("demo")
        }
        .is_complete());
        assert_eq!(
            ConnectionSpec::default().missing(),
            Some("名称"),
            "默认配置的 name 是空的"
        );
    }

    #[test]
    fn spec_roundtrips_through_json_with_defaults() {
        let s = ConnectionSpec {
            name: "本地".into(),
            ..Default::default()
        };
        let text = serde_json::to_string(&s).unwrap();
        let back: ConnectionSpec = serde_json::from_str(&text).unwrap();
        assert_eq!(back, s);
        // 老配置缺字段时不能整体解析失败
        let partial: ConnectionSpec = serde_json::from_str(r#"{"name":"a"}"#).unwrap();
        assert_eq!(partial.host, "127.0.0.1");
        assert_eq!(partial.port, 3306);
        assert_eq!(partial.kind, DbKind::Mysql);
    }

    #[test]
    fn preview_collapses_whitespace_and_truncates() {
        assert_eq!(
            preview_of("SELECT\n  a,\n  b\nFROM t WHERE x = 1"),
            "SELECT a, b FROM t WHERE x = 1"
        );
        let long = format!("SELECT {}", "x".repeat(200));
        let cut = preview_of(&long);
        assert!(cut.chars().count() <= 60, "实际 {}", cut.chars().count());
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn hub_runs_a_query_and_reports_it_back() {
        let mut hub = DbHub::with_driver(|_| Ok(Box::new(MockDriver::new())));
        let id = hub
            .run(&spec("demo"), "SELECT 1; SELECT 2".into(), 100)
            .unwrap();
        let events = wait(&mut hub);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, id);
        assert_eq!(events[0].conn, "demo");
        match &events[0].outcome {
            Ok(DbOutcome::Statements(list)) => {
                assert_eq!(list.len(), 2, "两条语句要出两个结果");
                assert!(list[0].is_query());
                assert_eq!(list[0].columns[0].name, "id");
                assert_eq!(list[0].row_count(), 2);
                assert_eq!(list[0].preview, "SELECT 1");
                assert_eq!(list[1].preview, "SELECT 2");
            }
            other => panic!("期望语句结果，拿到 {other:?}"),
        }
    }

    #[test]
    fn a_failing_task_does_not_take_the_worker_thread_down() {
        let mut hub = DbHub::with_driver(|_| Ok(Box::new(MockDriver::failing("语法错了"))));
        let bad = hub.run(&spec("demo"), "SELECT".into(), 100).unwrap();
        let events = wait(&mut hub);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, bad);
        assert!(
            matches!(&events[0].outcome, Err(text) if text.contains("语法错了")),
            "{:?}",
            events[0].outcome
        );

        // 同一个连接还能继续派活：线程要是被带走，第二个任务根本不会有回音
        let next = hub.databases(&spec("demo")).unwrap();
        let events = wait(&mut hub);
        assert_eq!(events.len(), 1, "线程还活着才回得了第二个任务");
        assert_eq!(events[0].id, next);
        assert!(
            events[0].outcome.is_err(),
            "这个假驱动每条语句都失败，重点是线程没死"
        );
    }

    #[test]
    fn hub_rejects_an_incomplete_spec_before_spawning() {
        let mut hub = DbHub::with_driver(|_| Ok(Box::new(MockDriver::new())));
        let err = hub
            .run(&ConnectionSpec::default(), "SELECT 1".into(), 100)
            .unwrap_err();
        assert!(err.message.contains("名称"), "实际 {err}");
        assert!(wait(&mut hub).is_empty());
        assert!(hub.connections().is_empty(), "配置不全就不该起线程");
    }

    #[test]
    fn one_worker_per_connection_is_reused_and_replaced_on_edit() {
        // 数一下建了几次驱动，验证「同配置复用、改了配置才重开」
        static BUILDS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        fn build(_: &ConnectionSpec) -> Result<Box<dyn Driver>, DbError> {
            BUILDS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Box::new(MockDriver::new()))
        }
        let mut hub = DbHub::with_driver(build);
        hub.run(&spec("demo"), "SELECT 1".into(), 100).unwrap();
        wait(&mut hub);
        hub.databases(&spec("demo")).unwrap();
        wait(&mut hub);
        assert_eq!(
            BUILDS.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "同配置的两次任务应该共用一条连接线程"
        );

        let mut edited = spec("demo");
        edited.database = "other".into();
        hub.databases(&edited).unwrap();
        wait(&mut hub);
        assert_eq!(
            BUILDS.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "改了配置应该换一条新线程"
        );
        assert_eq!(hub.connections(), vec!["demo".to_string()]);

        hub.close("demo");
        assert!(!hub.has_connection("demo"));
    }

    #[test]
    fn two_connections_get_two_threads_and_stay_separate() {
        let mut hub = DbHub::with_driver(|_| Ok(Box::new(MockDriver::new())));
        let a = hub.databases(&spec("甲")).unwrap();
        let b = hub.tables(&spec("乙"), "demo".into()).unwrap();
        let events = wait_all(&mut hub, 2);
        let ids: Vec<u64> = events.iter().map(|e| e.id).collect();
        assert!(ids.contains(&a) && ids.contains(&b), "两个任务都要回来：{ids:?}");
        assert_eq!(hub.connections().len(), 2);
    }

    #[test]
    fn reconnect_error_drops_the_session() {
        // 失败带 needs_reconnect 时，hub 应该丢驱动，下个任务重新建
        static BUILDS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        fn build(_: &ConnectionSpec) -> Result<Box<dyn Driver>, DbError> {
            BUILDS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Box::new(FlakyDriver))
        }
        struct FlakyDriver;
        impl Driver for FlakyDriver {
            fn execute(&mut self, _: &str, _: usize) -> Result<Vec<StatementResult>, DbError> {
                let n = CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n == 0 {
                    Err(DbError {
                        message: "Lost connection".into(),
                        code: Some(2013),
                        needs_reconnect: true,
                    })
                } else {
                    Ok(vec![StatementResult::empty("SELECT 1")])
                }
            }
            fn list_databases(&mut self) -> Result<Vec<String>, DbError> {
                Ok(vec![])
            }
            fn list_tables(&mut self, _: &str) -> Result<Vec<String>, DbError> {
                Ok(vec![])
            }
            fn probe(&mut self) -> Result<String, DbError> {
                Ok("ok".into())
            }
        }

        let mut hub = DbHub::with_driver(build);
        hub.run(&spec("demo"), "SELECT 1".into(), 10).unwrap();
        let events = wait(&mut hub);
        assert!(events[0].outcome.is_err());
        hub.run(&spec("demo"), "SELECT 1".into(), 10).unwrap();
        let events = wait(&mut hub);
        assert!(
            matches!(&events[0].outcome, Ok(DbOutcome::Statements(_))),
            "断连后下个任务应该自动重连再跑：{:?}",
            events[0].outcome
        );
        assert_eq!(BUILDS.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn row_cap_is_enforced_at_submit() {
        let mut hub = DbHub::with_driver(|_| {
            Ok(Box::new(MockDriver {
                rows: 5000,
                ..MockDriver::new()
            }))
        });
        hub.run(&spec("demo"), "SELECT * FROM big".into(), 999_999)
            .unwrap();
        let events = wait(&mut hub);
        match &events[0].outcome {
            Ok(DbOutcome::Statements(list)) => {
                assert_eq!(list[0].row_count(), MAX_ROWS);
                assert!(list[0].truncated, "截断过就要标出来，别让用户以为拿全了");
            }
            other => panic!("期望语句结果，拿到 {other:?}"),
        }
    }

    #[test]
    fn probe_comes_back_as_a_message() {
        let mut hub = DbHub::with_driver(|_| Ok(Box::new(MockDriver::new())));
        hub.probe(&spec("demo")).unwrap();
        let events = wait(&mut hub);
        assert!(
            matches!(&events[0].outcome, Ok(DbOutcome::Message(text)) if text.contains("mock")),
            "{:?}",
            events[0].outcome
        );
    }

    #[test]
    fn comment_only_block_yields_no_results() {
        // 只有注释的块不该占一个结果面板：sql 层已经把注释片段丢了
        let mut mock = MockDriver::new();
        let list = mock
            .execute("-- 只是说明\n/* 也没语句 */", 100)
            .unwrap();
        assert!(list.is_empty(), "实际 {} 条", list.len());
    }

    #[test]
    fn mock_write_statement_has_no_grid() {
        let mut mock = MockDriver::new();
        let list = mock
            .execute("DELETE FROM t WHERE id = 1", 100)
            .unwrap();
        assert_eq!(list.len(), 1);
        assert!(!list[0].is_query());
        assert_eq!(list[0].affected_rows, 2);
        assert_eq!(list[0].last_insert_id, 42);
        assert_eq!(list[0].preview, "DELETE FROM t WHERE id = 1");
    }

    #[test]
    fn mock_records_the_whole_block_once() {
        let shared: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let mut mock = MockDriver {
            executed: shared.clone(),
            ..MockDriver::new()
        };
        mock.execute("SELECT 1; SELECT 2", 100).unwrap();
        let sent = shared.lock().unwrap().clone();
        assert_eq!(sent.len(), 1, "整块只该交给驱动一次，不是每条语句一次");
        assert_eq!(sent[0], "SELECT 1; SELECT 2");
    }

    #[test]
    fn outcome_describes_itself_for_the_status_bar() {
        let mut mock = MockDriver::new();
        let list = mock.execute("SELECT 1; SELECT 2", 100).unwrap();
        assert_eq!(
            DbOutcome::Statements(list).describe(),
            "2 条语句 · 4 行 · 影响 4 行 · 2 ms"
        );
        assert_eq!(
            DbOutcome::Names(vec!["a".into(), "b".into()]).describe(),
            "2 项"
        );
        assert_eq!(
            DbOutcome::Statements(Vec::new()).describe(),
            "0 条语句 · 0 行 · 影响 0 行 · 0 ms"
        );
    }

    #[test]
    fn error_display_prefixes_the_server_code() {
        assert_eq!(DbError::new("网络断了").to_string(), "网络断了");
        let coded = DbError {
            message: "Access denied".into(),
            code: Some(1045),
            needs_reconnect: false,
        };
        assert_eq!(coded.to_string(), "1045 · Access denied");
    }

    /// 后台线程要跑一会儿，轮询到第一条为止。
    fn wait(hub: &mut DbHub) -> Vec<DbEvent> {
        wait_all(hub, 1)
    }

    fn wait_all(hub: &mut DbHub, count: usize) -> Vec<DbEvent> {
        let mut out = Vec::new();
        for _ in 0..2000 {
            out.extend(hub.poll());
            if out.len() >= count {
                return out;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        out
    }
}
