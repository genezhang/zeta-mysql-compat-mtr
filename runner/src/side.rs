//! One server connection of the differential mode: connect, reset to a known-empty state, run a statement
//! within a time budget (killing it on the server when it outlives the budget), describe the server.

use std::collections::BTreeSet;
use std::future::Future;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use mysql_async::prelude::Queryable;

use crate::outcome::{exec, Outcome};

pub const DB: &str = "test";
pub const STATEMENT_BUDGET: Duration = Duration::from_secs(20);
pub const CONNECT_BUDGET: Duration = Duration::from_secs(10);
pub const ADMIN_BUDGET: Duration = Duration::from_secs(20);
pub const KILL_BUDGET: Duration = Duration::from_secs(5);
const SYSTEM_DBS: [&str; 4] = ["information_schema", "mysql", "performance_schema", "sys"];
/// After this many failed connects in a row within one file, the rest of that file's statements on that side
/// are answered "unavailable" without trying again (each try can cost the whole connect budget).
pub const MAX_CONNECT_FAILURES: u32 = 3;
/// A server that cannot be brought back this many times in a row ends the run.
pub const MAX_RESTARTS_IN_A_ROW: u32 = 3;

/// Run `fut` within `budget`; fold a timeout and an error into one error naming the step.
pub async fn within<T, E: std::fmt::Display>(
    budget: Duration,
    what: &str,
    fut: impl Future<Output = std::result::Result<T, E>>,
) -> Result<T> {
    match tokio::time::timeout(budget, fut).await {
        Err(_) => bail!("{what}: no answer within {budget:?}"),
        Ok(Err(e)) => bail!("{what}: {e}"),
        Ok(Ok(v)) => Ok(v),
    }
}

/// The names are exact: with `lower_case_table_names=0` a database called `Mysql` is the user's, not the server's.
fn is_system_db(db: &str) -> bool {
    SYSTEM_DBS.contains(&db)
}

pub struct Side {
    pub name: &'static str,
    pub base: String,
    /// zeta keeps a default database it refuses to drop; the oracle must come back with nothing but `test`.
    pub tolerate_undroppable: bool,
    pub undroppable: BTreeSet<String>,
    pub conn: Option<mysql_async::Conn>,
    pub conn_id: Option<u64>,
    pub connect_failures: u32,
    /// A statement timed out and the server could not be told to stop it: its work may still be running.
    pub tainted: bool,
    /// Statements every new session of this side runs first, after `SET NAMES` (the recorder shifts the oracle's
    /// clock with one in later passes).
    pub session_init: Vec<String>,
    /// No reset has succeeded since this side was (re)started. Only then is a database that refuses to be
    /// dropped accepted as permanent (zeta's default database); later it is an error.
    pub fresh: bool,
}

impl Side {
    pub fn new(name: &'static str, base: &str, tolerate_undroppable: bool) -> Self {
        Side {
            name,
            base: base.to_string(),
            tolerate_undroppable,
            undroppable: BTreeSet::new(),
            conn: None,
            conn_id: None,
            connect_failures: 0,
            tainted: false,
            session_init: Vec::new(),
            fresh: true,
        }
    }

    /// Connection options for the base URL with the given default database (none = no database named, which is
    /// what the administrative steps need before `test` exists).
    pub fn opts(&self, db: Option<&str>) -> Result<mysql_async::Opts> {
        let o = mysql_async::Opts::from_url(&self.base)
            .map_err(|e| anyhow!("bad URL for {}: {e}", self.name))?;
        // No unix-socket upgrade (it would silently talk to whatever local server owns the path the oracle reports,
        // bypassing the loopback check), and the packet/timeout limits given up front so that connecting sends no
        // `SELECT @@max_allowed_packet, ...` of its own: the file's first statement must see a session no statement
        // of ours has touched (FOUND_ROWS(), ROW_COUNT(), ...).
        Ok(mysql_async::OptsBuilder::from_opts(o)
            .db_name(db.map(str::to_owned))
            // mysql_async's handshake announces utf8mb4_general_ci; real clients (JDBC, mysql2, the mysql CLI) end up on
            // the server's default collation, utf8mb4_0900_ai_ci (NO PAD, unlike general_ci). Ask for that, on both
            // sides. SET NAMES is not a SELECT, so it leaves FOUND_ROWS() and friends alone.
            .init(
                std::iter::once("SET NAMES utf8mb4".to_string())
                    .chain(self.session_init.iter().cloned())
                    .collect::<Vec<_>>(),
            )
            .prefer_socket(false)
            .max_allowed_packet(Some(64 * 1024 * 1024))
            .wait_timeout(Some(28800))
            .into())
    }

    pub async fn admin(&self) -> Result<mysql_async::Conn> {
        within(
            CONNECT_BUDGET,
            &format!("{}: admin connect", self.name),
            mysql_async::Conn::new(self.opts(None)?),
        )
        .await
    }

    pub async fn drop_conn(&mut self) {
        if let Some(c) = self.conn.take() {
            // Best effort: a connection stuck in a statement never answers the goodbye.
            let _ = tokio::time::timeout(KILL_BUDGET, c.disconnect()).await;
        }
        self.conn_id = None;
    }

    /// Drop every non-system database, recreate `test`, and check that the side really is empty. Over a
    /// connection that names no database. An error here means the side is not in a known state.
    pub async fn reset(&mut self) -> Result<()> {
        self.drop_conn().await;
        self.connect_failures = 0;
        let mut admin = self.admin().await?;
        let name = self.name;
        let mut failed: Vec<String> = Vec::new();
        for attempt in 0..2 {
            let dbs: Vec<String> = within(
                ADMIN_BUDGET,
                &format!("{name}: SHOW DATABASES"),
                admin.query("SHOW DATABASES"),
            )
            .await?;
            failed.clear();
            let todo: Vec<String> = dbs
                .into_iter()
                .filter(|d| !is_system_db(d) && !self.undroppable.contains(d))
                .collect();
            for db in todo {
                let stmt = format!("DROP DATABASE `{}`", db.replace('`', "``"));
                if let Err(e) = within(
                    ADMIN_BUDGET,
                    &format!("{name}: DROP DATABASE `{db}`"),
                    admin.query_drop(stmt),
                )
                .await
                {
                    failed.push(format!("`{db}` ({e})"));
                    // A statement cancelled by the time budget leaves this connection mid-conversation: never
                    // reuse it. A fresh one carries the retry and every later check.
                    admin = self.admin().await?;
                    if attempt == 1 && self.tolerate_undroppable && self.fresh {
                        eprintln!(
                            "note: {name} cannot drop database `{db}`; treating it as permanent"
                        );
                        self.undroppable.insert(db);
                    }
                }
            }
            if failed.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let tolerated = self.tolerate_undroppable && self.fresh;
        if !failed.is_empty() && !tolerated {
            bail!("{name}: could not drop {}", failed.join(", "));
        }
        within(
            ADMIN_BUDGET,
            &format!("{name}: CREATE DATABASE {DB}"),
            admin.query_drop(format!("CREATE DATABASE {DB}")),
        )
        .await?;
        // A database this side refuses to drop must at least come back empty.
        for db in self.undroppable.clone() {
            let q = |d: &str| d.replace('`', "``");
            let tables: Vec<String> = within(
                ADMIN_BUDGET,
                &format!("{name}: SHOW TABLES FROM `{db}`"),
                admin.query(format!("SHOW TABLES FROM `{}`", q(&db))),
            )
            .await?;
            for t in &tables {
                let target = format!("`{}`.`{}`", q(&db), q(t));
                // Tables and views are dropped by different statements; the wrong one just errors.
                let _ = within(
                    ADMIN_BUDGET,
                    "DROP TABLE",
                    admin.query_drop(format!("DROP TABLE IF EXISTS {target}")),
                )
                .await;
                let _ = within(
                    ADMIN_BUDGET,
                    "DROP VIEW",
                    admin.query_drop(format!("DROP VIEW IF EXISTS {target}")),
                )
                .await;
            }
            let left: Vec<String> = within(
                ADMIN_BUDGET,
                &format!("{name}: SHOW TABLES FROM `{db}`"),
                admin.query(format!("SHOW TABLES FROM `{}`", q(&db))),
            )
            .await?;
            if !left.is_empty() {
                bail!("{name}: `{db}` cannot be dropped and still holds {left:?}");
            }
        }
        // Parity check: exactly `test` (plus what this side is known to keep) must be left, and it must be empty.
        let after: Vec<String> = within(
            ADMIN_BUDGET,
            &format!("{name}: SHOW DATABASES"),
            admin.query("SHOW DATABASES"),
        )
        .await?;
        let stray: Vec<&String> = after
            .iter()
            .filter(|d| d.as_str() != DB && !is_system_db(d) && !self.undroppable.contains(*d))
            .collect();
        if !stray.is_empty() {
            bail!("{name}: databases survived the reset: {stray:?}");
        }
        let tables: Vec<String> = within(
            ADMIN_BUDGET,
            &format!("{name}: SHOW TABLES"),
            admin.query(format!("SHOW TABLES FROM {DB}")),
        )
        .await?;
        if !tables.is_empty() {
            bail!("{name}: `{DB}` is not empty after the reset: {tables:?}");
        }
        within(CONNECT_BUDGET, "disconnect", admin.disconnect())
            .await
            .ok();
        // The file's session starts here and must see no statement of ours before the file's own.
        self.connect().await?;
        self.fresh = false;
        Ok(())
    }

    pub async fn connect(&mut self) -> Result<()> {
        let conn = within(
            CONNECT_BUDGET,
            &format!("{}: connect", self.name),
            mysql_async::Conn::new(self.opts(Some(DB))?),
        )
        .await?;
        // The id (from the handshake, no statement needed: a probe would change what FOUND_ROWS(), ROW_COUNT()
        // and friends return for the file's first statement) is what lets a timed-out statement be killed.
        self.conn_id = Some(u64::from(conn.id()));
        self.conn = Some(conn);
        Ok(())
    }

    /// Ask the server to end the session that is stuck in a statement. Dropping our end of the socket does not
    /// stop a statement that is already running.
    pub async fn kill_running(&mut self) {
        let killed = async {
            let id = self
                .conn_id
                .ok_or_else(|| anyhow!("connection id unknown"))?;
            let mut admin = self.admin().await?;
            // 1094 (unknown thread id): the session is already gone, which is what was wanted.
            if let Err(e) = within(
                KILL_BUDGET,
                "KILL CONNECTION",
                admin.query_drop(format!("KILL CONNECTION {id}")),
            )
            .await
            {
                if !e.to_string().contains("1094") {
                    return Err(e);
                }
            }
            let _ = tokio::time::timeout(KILL_BUDGET, admin.disconnect()).await;
            Ok::<(), anyhow::Error>(())
        };
        if let Err(e) = killed.await {
            eprintln!(
                "warning: {} could not stop a timed-out statement ({e}); its work may still be running",
                self.name
            );
            self.tainted = true;
        }
    }

    pub async fn run(&mut self, sql: &str) -> Outcome {
        if self.conn.is_none() {
            if self.connect_failures >= MAX_CONNECT_FAILURES {
                return Outcome::Err {
                    code: None,
                    msg: format!(
                        "{} unavailable: {} connect attempts failed in a row",
                        self.name, self.connect_failures
                    ),
                };
            }
            if let Err(e) = self.connect().await {
                self.connect_failures += 1;
                return Outcome::Err {
                    code: None,
                    msg: format!("cannot connect: {e}"),
                };
            }
            self.connect_failures = 0;
        }
        let out = exec(
            self.conn.as_mut().expect("connected"),
            sql,
            STATEMENT_BUDGET,
        )
        .await;
        match out {
            Outcome::Timeout => {
                self.kill_running().await;
                self.drop_conn().await;
            }
            // A dead connection leaves the session unusable: the next statement reconnects (session state
            // such as SET variables is lost, as it would be).
            Outcome::Err { code: None, .. } => self.drop_conn().await,
            _ => {}
        }
        out
    }

    /// Version and the settings that decide how statements behave, asked over an administrative connection so
    /// the file's own session is untouched. A value that cannot be read is shown as `<error>`.
    pub async fn info(&mut self) -> Vec<(&'static str, String)> {
        let mut parts = Vec::new();
        let mut admin = match self.admin().await {
            Ok(a) => a,
            Err(e) => return vec![("error", e.to_string())],
        };
        for (label, expr) in [
            ("version", "VERSION()"),
            ("sql_mode", "@@sql_mode"),
            ("lower_case_table_names", "@@lower_case_table_names"),
            ("time_zone", "@@time_zone"),
            ("collation_connection", "@@collation_connection"),
        ] {
            let v = match exec(&mut admin, &format!("SELECT {expr}"), KILL_BUDGET).await {
                Outcome::Rows { rows, .. } => rows
                    .first()
                    .and_then(|r| r.first())
                    .map(|c| c.clone().unwrap_or_else(|| "NULL".into()))
                    .unwrap_or_default(),
                other => format!("<{}>", other.brief().replace('\n', " ")),
            };
            parts.push((label, v));
        }
        let _ = tokio::time::timeout(KILL_BUDGET, admin.disconnect()).await;
        parts
    }

    /// `name: version=.. sql_mode=.. lower_case_table_names=.. time_zone=..`, for the run header.
    pub async fn describe(&mut self) -> String {
        let parts: Vec<String> = self
            .info()
            .await
            .into_iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        format!("{}: {}", self.name, parts.join(" "))
    }
}
