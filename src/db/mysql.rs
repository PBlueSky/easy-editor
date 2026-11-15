//! MySQL 驱动。
//!
//! 选同步的 `mysql` crate 而不是 sqlx：这个程序里所有并发都是 `std::thread` + mpsc，
//! 为一串同步查询拖进整套 tokio 不划算。
//!
//! 全程走文本协议（`query_iter`），不用预处理语句：
//! - 文本协议把所有列都送回字节串，`Cell` 只需区分 NULL 和非 UTF-8，日期与数字的
//!   格式化交给服务端，结果天然和 `mysql` 命令行看到的一致；
//! - 预处理语句要自己管占位符，而用户写的 SQL 里 `?` 到处都是。
//!
//! 一个块里的多条语句由 `DbHub::run` 用 `crate::sql::split_statements` 切开，
//! 逐条送进 `execute`。刻意不合并成一个结果集：一个块里三条 SELECT 就该出三个面板。

use std::time::Duration;

use mysql::prelude::Queryable;
use mysql::{Conn, Error as MyError, OptsBuilder, Value};

use super::{
    preview_of, Cell, Column, ConnectionSpec, DbError, DbKind, Driver, StatementResult, MAX_ROWS,
};

/// 建连超时。没有它的话，主机写错会让工作线程卡在 TCP 上两分钟。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// 只读语句的服务端执行上限：一条忘了加 LIMIT 的全表扫描不该永远占着这条连接。
const STATEMENT_TIMEOUT_MS: u128 = 30_000;

pub struct MysqlDriver {
    conn: Conn,
}

impl Driver for MysqlDriver {
    /// 跑一条语句。切分在上层（`DbHub::run`），所以这里不需要循环，
    /// 也就不会出现「第 2 条报错把第 1 条的结果一起丢掉」。
    fn execute(&mut self, stmt: &str, max_rows: usize) -> Result<StatementResult, DbError> {
        self.run_one(stmt, max_rows.clamp(1, MAX_ROWS))
    }

    fn list_databases(&mut self) -> Result<Vec<String>, DbError> {
        self.names("SHOW DATABASES")
    }

    fn list_tables(&mut self, database: &str) -> Result<Vec<String>, DbError> {
        // 库名没法参数化，只能按 MySQL 的规则把反引号翻倍后包起来
        let quoted = quote_identifier(database);
        self.names(&format!("SHOW TABLES IN {quoted}"))
    }

    fn probe(&mut self) -> Result<String, DbError> {
        let kind = DbKind::Mysql.label();
        let version: Option<String> = self
            .conn
            .query_first("SELECT VERSION()")
            .map_err(to_db_error)?;
        Ok(match version {
            Some(text) => format!("{kind} {text}"),
            None => kind.to_string(),
        })
    }
}

impl MysqlDriver {
    fn run_one(&mut self, stmt: &str, max_rows: usize) -> Result<StatementResult, DbError> {
        let started = std::time::Instant::now();
        let mut result = self
            .conn
            .query_iter(stmt)
            .map_err(|err| with_statement(err, stmt))?;
        let columns: Vec<Column> = result
            .columns()
            .as_ref()
            .iter()
            .map(|col| Column {
                name: col.name_str().into_owned(),
                type_name: format!("{:?}", col.column_type()),
            })
            .collect();

        let mut rows: Vec<Vec<Cell>> = Vec::with_capacity(max_rows.min(64));
        let mut truncated = false;
        for row in result.by_ref() {
            let row = row.map_err(|err| with_statement(err, stmt))?;
            if rows.len() >= max_rows {
                truncated = true;
                break;
            }
            rows.push((0..row.len()).map(|i| to_cell(row.as_ref(i))).collect());
        }
        // 这几个数要等结果集读完才是终值，所以放在循环之后取
        let affected_rows = result.affected_rows();
        let warnings = result.warnings();
        let last_insert_id = result.last_insert_id().unwrap_or(0);
        // 截断时剩下的行还得由 result 的 Drop 抽干才能让连接回到可用状态。
        // 主动在这儿等一下：耗时要把这段算进去，否则界面显示「12 ms」而线程还在
        // 后台默默读几百万行，用户看到的就是「早就好了但一直转圈」。
        drop(result);
        let elapsed_ms = started.elapsed().as_millis();

        Ok(StatementResult {
            columns,
            rows,
            affected_rows,
            last_insert_id,
            warnings,
            truncated,
            elapsed_ms,
            preview: preview_of(stmt),
        })
    }

    /// 跑一条只回一列名字的语句（`SHOW DATABASES` 之类）。
    fn names(&mut self, sql: &str) -> Result<Vec<String>, DbError> {
        let rows: Vec<Value> = self
            .conn
            .query(sql)
            .map_err(|err| with_statement(err, sql))?;
        Ok(rows
            .iter()
            .map(|value| to_cell(Some(value)).display().into_owned())
            .collect())
    }
}

/// MySQL 的标识符引用：反引号翻倍，和 `mysql` 命令行一致。
fn quote_identifier(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

fn to_cell(value: Option<&Value>) -> Cell {
    match value {
        None | Some(Value::NULL) => Cell::Null,
        Some(Value::Bytes(bytes)) => Cell::from_bytes(bytes.clone()),
        // 文本协议只会碰到上面两个分支；剩下的是给以后换二进制协议留的兜底。
        // 宁可显示得难看，也不能把值悄悄变成 NULL
        Some(Value::Int(n)) => Cell::Text(n.to_string()),
        Some(Value::UInt(n)) => Cell::Text(n.to_string()),
        Some(Value::Float(n)) => Cell::Text(n.to_string()),
        Some(Value::Double(n)) => Cell::Text(n.to_string()),
        Some(Value::Date(year, month, day, hour, minute, second, micro)) => Cell::Text(format!(
            "{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}.{micro:06}"
        )),
        Some(Value::Time(negative, days, hour, minute, second, micro)) => {
            // MySQL 的 TIME 允许超过 24 小时，天数得并进小时
            let hours = *hour as i64 + *days as i64 * 24;
            let sign = if *negative { "-" } else { "" };
            Cell::Text(format!(
                "{sign}{hours:02}:{minute:02}:{second:02}.{micro:06}"
            ))
        }
    }
}

/// 把驱动错误翻成引擎无关的错误，并标出会断会话的错。
fn to_db_error(err: MyError) -> DbError {
    let code = match &err {
        MyError::MySqlError(server) => Some(server.code),
        _ => None,
    };
    DbError {
        message: err.to_string(),
        code,
        // 传输层的错：会话状态不可知，留着下次继续用会出怪结果
        needs_reconnect: err.is_connectivity_error()
            || matches!(code, Some(2006) | Some(2013) | Some(2055)),
    }
}

/// 执行失败时补上是哪条语句炸了。一个块里三条语句，不标出来根本不知道该改哪条。
fn with_statement(err: MyError, sql: &str) -> DbError {
    let mut out = to_db_error(err);
    out.message = format!("{} ← {}", out.message, preview_of(sql));
    out
}

/// 建一条连接。这里不碰任何 UI，失败就如实返回错误。
pub fn connect(spec: &ConnectionSpec) -> Result<Box<dyn Driver>, DbError> {
    let mut builder = OptsBuilder::default()
        .ip_or_hostname(Some(spec.host.as_str()))
        .tcp_port(spec.port)
        .user(Some(spec.user.as_str()))
        .pass(Some(spec.resolve_password()))
        // 命名管道只对本机默认实例有意义，表单里填了主机和端口就该老实走 TCP
        .prefer_socket(false)
        .tcp_connect_timeout(Some(CONNECT_TIMEOUT))
        // 读超时要比建连超时宽得多：查询在服务端跑着，客户端不能先把 socket 读断
        .read_timeout(Some(CONNECT_TIMEOUT * 6))
        .write_timeout(Some(CONNECT_TIMEOUT * 6));
    if !spec.database.is_empty() {
        builder = builder.db_name(Some(spec.database.as_str()));
    }

    let mut conn = Conn::new(builder).map_err(to_db_error)?;
    // MariaDB 没有这个变量（它叫 max_statement_time），SET 失败不能连不上库，
    // 查询保护只是尽力而为。
    let _ = conn.query_drop(format!(
        "SET SESSION MAX_EXECUTION_TIME = {STATEMENT_TIMEOUT_MS}"
    ));
    Ok(Box::new(MysqlDriver { conn }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifier_quoting_doubles_backticks() {
        assert_eq!(quote_identifier("demo"), "`demo`");
        assert_eq!(quote_identifier("my`db"), "`my``db`");
    }

    #[test]
    fn text_protocol_values_become_cells() {
        assert_eq!(to_cell(None), Cell::Null);
        assert_eq!(to_cell(Some(&Value::NULL)), Cell::Null);
        assert_eq!(
            to_cell(Some(&Value::Bytes("中文".as_bytes().to_vec()))),
            Cell::Text("中文".into())
        );
        assert_eq!(
            to_cell(Some(&Value::Bytes(vec![0xff, 0x00]))),
            Cell::Blob(vec![0xff, 0x00]),
            "非 UTF-8 不能坏一个格子，更不该把整条结果丢掉"
        );
        assert_eq!(to_cell(Some(&Value::Int(-7))), Cell::Text("-7".into()));
        assert_eq!(to_cell(Some(&Value::UInt(70000))), Cell::Text("70000".into()));
    }

    #[test]
    fn time_cells_read_like_mysqldump() {
        // MySQL 的 TIME 可以超过 24 小时：days=2 hour=3 -> 51 小时
        assert_eq!(
            to_cell(Some(&Value::Time(false, 2, 3, 4, 5, 0))).display(),
            "51:04:05.000000"
        );
        assert_eq!(
            to_cell(Some(&Value::Time(true, 0, 1, 0, 0, 500_000))).display(),
            "-01:00:00.500000"
        );
        assert_eq!(
            to_cell(Some(&Value::Date(2026, 11, 15, 14, 3, 9, 0))).display(),
            "2026-11-15 14:03:09.000000"
        );
    }

    #[test]
    fn statement_text_is_attached_to_the_error() {
        let server = mysql::MySqlError {
            state: "42000".into(),
            message: "You have an error in your SQL syntax".into(),
            code: 1064,
        };
        let err = with_statement(MyError::MySqlError(server), "SELECT FROM");
        assert_eq!(err.code, Some(1064));
        assert!(err.message.contains("SELECT FROM"), "实际 {err}");
        assert!(
            !err.needs_reconnect,
            "语法错不该让 hub 把连接丢掉：{err}"
        );
    }

    #[test]
    fn connectivity_errors_ask_for_a_reconnect() {
        let io = MyError::IoError(std::io::Error::other("boom"));
        assert!(to_db_error(io).needs_reconnect);

        let gone = MyError::MySqlError(mysql::MySqlError {
            state: "HY000".into(),
            message: "MySQL server has gone away".into(),
            code: 2006,
        });
        assert!(to_db_error(gone).needs_reconnect);

        let denied = MyError::MySqlError(mysql::MySqlError {
            state: "28000".into(),
            message: "Access denied for user".into(),
            code: 1045,
        });
        assert!(
            !to_db_error(denied).needs_reconnect,
            "密码错重连一百次也没用"
        );
    }

    #[test]
    fn db_kind_describes_itself() {
        assert_eq!(DbKind::Mysql.label(), "MySQL");
        assert_eq!(DbKind::Mysql.default_port(), 3306);
        assert_eq!(DbKind::all().len(), 1, "加引擎时记得同时加进 all()");
        let spec = ConnectionSpec {
            name: "demo".into(),
            ..Default::default()
        };
        assert_eq!(spec.kind, DbKind::Mysql);
    }

    /// 真连一个库跑一遍。本机没起 MySQL 时跳过。
    /// 手动跑：先配好 config.json 里那个叫「本机」的连接，再
    /// `cargo test -- --ignored --nocapture talks_to_a_real_server`
    #[test]
    #[ignore = "需要真实 MySQL 服务"]
    fn talks_to_a_real_server() {
        let spec = ConnectionSpec {
            name: "本机".into(),
            user: "root".into(),
            database: "mysql".into(),
            ..Default::default()
        };
        let mut driver = connect(&spec).expect("连不上本机 MySQL，先确认服务在跑");
        assert!(driver.probe().unwrap().contains("MySQL"));

        let dbs = driver.list_databases().unwrap();
        assert!(dbs.contains(&"mysql".to_string()), "实际 {dbs:?}");

        // 驱动一次只吃一条语句（切分在 DbHub），这里就逐条喂
        let first = driver.execute("SELECT 1 AS one", 100).unwrap();
        assert_eq!(first.columns[0].name, "one");
        assert_eq!(first.rows[0][0], Cell::Text("1".into()));
        let second = driver.execute("SELECT 2 AS two", 100).unwrap();
        assert_eq!(second.columns[0].name, "two");
        assert_eq!(second.rows[0][0], Cell::Text("2".into()));

        let capped = driver.execute("SELECT * FROM mysql.user", 1).unwrap();
        assert!(
            capped.truncated || capped.rows.len() < 2,
            "max_rows=1 要么截断要么本来就只有一行"
        );
    }
}
