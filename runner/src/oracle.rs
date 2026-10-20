//! Differential mode: run every statement of a `.test` file against zeta and
//! against a real MySQL server (the oracle) and compare the two outcomes.
//!
//! The normal mode compares zeta with the sibling `.result` file, which was
//! written for zeta, so a pass there means "unchanged", not "agrees with
//! MySQL". Here the `.result` files are not read at all.
//!
//! Unlike the normal mode this does not stop at the first failing statement:
//! it runs the whole file and records, per statement, whether the two servers
//! agree. A statement that diverges can change the state the later ones see,
//! so the actionable number per file is the FIRST divergence (everything
//! before it ran on identical state); the statement-level totals are a
//! secondary measure.
//!
//! Output carries only counts, statement indexes and mechanical labels on
//! stdout. The optional report file also holds statement text and the two
//! servers' output, so it is for local reading and must not be committed.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use mysql_async::prelude::Queryable;
use walkdir::WalkDir;

use crate::mtr_parser::{parse, Directive};
use crate::runner::format_value;

const DB: &str = "test";
const STATEMENT_BUDGET: Duration = Duration::from_secs(20);
const SYSTEM_DBS: [&str; 4] = ["information_schema", "mysql", "performance_schema", "sys"];

#[derive(Debug, Clone, PartialEq)]
enum Outcome {
    Rows {
        cols: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    Ok {
        affected: u64,
    },
    Err {
        code: Option<u32>,
        msg: String,
    },
    Timeout,
}

impl Outcome {
    fn brief(&self) -> String {
        let s = match self {
            Outcome::Rows { cols, rows } => {
                let mut s = cols.join("|");
                for r in rows.iter().take(6) {
                    let _ = write!(s, "\n{}", r.join("|"));
                }
                if rows.len() > 6 {
                    let _ = write!(s, "\n... {} rows", rows.len());
                }
                s
            }
            Outcome::Ok { affected } => format!("OK affected={affected}"),
            Outcome::Err { code, msg } => format!("ERROR {code:?}: {msg}"),
            Outcome::Timeout => "TIMEOUT".to_string(),
        };
        s.chars().take(400).collect()
    }
}

/// Why two outcomes differ. `Same` is the only agreeing label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Same,
    /// zeta returns an error, MySQL succeeds: missing feature or defect.
    ZetaError,
    /// zeta succeeds, MySQL returns an error: missing validation.
    ZetaAccepts,
    /// Both error, different error numbers.
    ErrorCode,
    /// Same rows in a different order.
    RowOrder,
    /// Same data, different column names.
    ColumnNames,
    /// Different number of rows.
    RowCount,
    /// Same shape, different cell values.
    Values,
    /// One side returned a result set, the other did not.
    Shape,
    /// Both succeeded without a result set but report different affected rows.
    Affected,
    /// Different number of columns.
    ColumnCount,
    /// zeta did not answer within the budget.
    Timeout,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Same => "same",
            Kind::ZetaError => "zeta-error",
            Kind::ZetaAccepts => "zeta-accepts",
            Kind::ErrorCode => "error-code",
            Kind::RowOrder => "row-order",
            Kind::ColumnNames => "column-names",
            Kind::RowCount => "row-count",
            Kind::Values => "values",
            Kind::Shape => "shape",
            Kind::Affected => "affected-rows",
            Kind::ColumnCount => "column-count",
            Kind::Timeout => "timeout",
        }
    }
}

fn compare(zeta: &Outcome, oracle: &Outcome, sorted: bool) -> Kind {
    use Outcome::*;
    match (zeta, oracle) {
        (Timeout, _) => Kind::Timeout,
        (Err { .. }, Err { code: oc, .. }) => {
            let Err { code: zc, .. } = zeta else {
                unreachable!()
            };
            if zc == oc {
                Kind::Same
            } else {
                Kind::ErrorCode
            }
        }
        (Err { .. }, _) => Kind::ZetaError,
        (_, Err { .. }) => Kind::ZetaAccepts,
        (Ok { affected: a }, Ok { affected: b }) => {
            if a == b {
                Kind::Same
            } else {
                Kind::Affected
            }
        }
        (Rows { cols: zc, rows: zr }, Rows { cols: oc, rows: or }) => {
            if zc.len() != oc.len() {
                return Kind::ColumnCount;
            }
            let (mut zr2, mut or2) = (zr.clone(), or.clone());
            if sorted {
                zr2.sort();
                or2.sort();
            }
            if zr2 == or2 {
                return if zc == oc {
                    Kind::Same
                } else {
                    Kind::ColumnNames
                };
            }
            if zr.len() != or.len() {
                return Kind::RowCount;
            }
            let (mut zs, mut os) = (zr.clone(), or.clone());
            zs.sort();
            os.sort();
            if zs == os {
                Kind::RowOrder
            } else {
                Kind::Values
            }
        }
        _ => Kind::Shape,
    }
}

async fn exec(conn: &mut mysql_async::Conn, sql: &str) -> Outcome {
    let fut = async {
        let mut res = conn.query_iter(sql).await?;
        let cols: Vec<String> = res
            .columns_ref()
            .iter()
            .map(|c| c.name_str().into_owned())
            .collect();
        let affected = res.affected_rows();
        let rows: Vec<mysql_async::Row> = res.collect().await?;
        Ok::<_, mysql_async::Error>((cols, affected, rows))
    };
    match tokio::time::timeout(STATEMENT_BUDGET, fut).await {
        Err(_) => Outcome::Timeout,
        Ok(Err(mysql_async::Error::Server(se))) => Outcome::Err {
            code: Some(se.code as u32),
            msg: se.message,
        },
        Ok(Err(e)) => Outcome::Err {
            code: None,
            msg: e.to_string(),
        },
        Ok(Ok((cols, affected, rows))) => {
            if cols.is_empty() {
                Outcome::Ok { affected }
            } else {
                let rows = rows
                    .iter()
                    .map(|r| {
                        (0..r.len())
                            .map(|i| format_value(r.as_ref(i).expect("column in range")))
                            .collect()
                    })
                    .collect();
                Outcome::Rows { cols, rows }
            }
        }
    }
}

struct Side {
    base: String,
    conn: Option<mysql_async::Conn>,
}

impl Side {
    fn new(base: &str) -> Self {
        Side {
            base: base.to_string(),
            conn: None,
        }
    }

    /// Drop every non-system database and recreate `test`, over a connection
    /// that names no database.
    async fn reset(&mut self) -> Result<()> {
        if let Some(c) = self.conn.take() {
            c.disconnect().await.ok();
        }
        let mut admin = mysql_async::Conn::new(mysql_async::Opts::from_url(&self.base)?).await?;
        let dbs: Vec<String> = admin.query("SHOW DATABASES").await.unwrap_or_default();
        for db in dbs {
            if SYSTEM_DBS.contains(&db.to_ascii_lowercase().as_str()) {
                continue;
            }
            admin.query_drop(format!("DROP DATABASE `{db}`")).await.ok();
        }
        admin.query_drop(format!("CREATE DATABASE {DB}")).await?;
        admin.disconnect().await.ok();
        self.connect().await
    }

    async fn connect(&mut self) -> Result<()> {
        let url = format!("{}/{DB}", self.base);
        self.conn = Some(mysql_async::Conn::new(mysql_async::Opts::from_url(&url)?).await?);
        Ok(())
    }

    async fn run(&mut self, sql: &str) -> Outcome {
        if self.conn.is_none() && self.connect().await.is_err() {
            return Outcome::Err {
                code: None,
                msg: "cannot connect".into(),
            };
        }
        let out = exec(self.conn.as_mut().expect("connected"), sql).await;
        // A dead connection or a timeout leaves the session unusable: drop it so the next
        // statement reconnects (session state such as SET variables is lost, as it would be).
        if matches!(out, Outcome::Timeout | Outcome::Err { code: None, .. }) {
            self.conn = None;
        }
        out
    }
}

#[derive(Default)]
struct FileStats {
    total: usize,
    by_kind: BTreeMap<Kind, usize>,
    first: Option<(usize, Kind)>,
    /// First divergence on a statement MySQL itself accepted: a real defect, as opposed to
    /// zeta being lenient or both sides erroring with different codes.
    first_hard: Option<(usize, Kind)>,
    /// Statements MySQL executed without error, and how many of those zeta matched.
    oracle_ok: usize,
    oracle_ok_same: usize,
}

pub async fn run_diff(
    suites: &[String],
    filter: Option<&str>,
    exclude: &[String],
    zeta_base: &str,
    oracle_base: &str,
    report: Option<&Path>,
) -> Result<()> {
    let mut files = Vec::new();
    for suite in suites {
        let root = if suite == "all" {
            Path::new("tests").to_path_buf()
        } else {
            Path::new("tests").join(suite)
        };
        for entry in WalkDir::new(&root).follow_links(false) {
            let entry = entry?;
            let p = entry.into_path();
            if p.extension().and_then(|s| s.to_str()) != Some("test") {
                continue;
            }
            if let Some(f) = filter {
                if !p.to_string_lossy().contains(f) {
                    continue;
                }
            }
            if exclude
                .iter()
                .any(|x| p.to_string_lossy().contains(x.as_str()))
            {
                continue;
            }
            files.push(p);
        }
    }
    files.sort();
    files.dedup();

    let mut rep = match report {
        Some(p) => Some(fs::File::create(p).with_context(|| format!("creating {}", p.display()))?),
        None => None,
    };
    let mut zeta = Side::new(zeta_base);
    let mut oracle = Side::new(oracle_base);

    println!(
        "file\tstatements\tsame\tfirst_divergence\tfirst_kind\tmysql_ok\tmysql_ok_same\tfirst_hard\tfirst_hard_kind\tdiverging_kinds"
    );
    let mut grand = FileStats::default();
    let mut files_same = 0usize;
    let mut files_hard_clean = 0usize;
    let mut first_hist: BTreeMap<Kind, usize> = BTreeMap::new();
    for path in &files {
        let src =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let directives = match parse(&src) {
            Ok(d) => d,
            Err(e) => {
                println!("{}\t-\t-\t-\tparse-error\t{e}", path.display());
                continue;
            }
        };
        zeta.reset().await.ok();
        oracle.reset().await.context("resetting the oracle")?;

        let mut st = FileStats::default();
        let mut sorted = false;
        for d in directives {
            let sql = match d {
                Directive::SortedResult => {
                    sorted = true;
                    continue;
                }
                Directive::Sql(s) => s,
                _ => continue,
            };
            let z = zeta.run(&sql).await;
            let o = oracle.run(&sql).await;
            let kind = compare(&z, &o, sorted);
            sorted = false;
            st.total += 1;
            *st.by_kind.entry(kind).or_default() += 1;
            let oracle_accepted = !matches!(o, Outcome::Err { .. });
            if oracle_accepted {
                st.oracle_ok += 1;
                if kind == Kind::Same {
                    st.oracle_ok_same += 1;
                }
            }
            if kind != Kind::Same {
                if st.first.is_none() {
                    st.first = Some((st.total, kind));
                }
                if oracle_accepted && st.first_hard.is_none() {
                    st.first_hard = Some((st.total, kind));
                }
                if let Some(f) = rep.as_mut() {
                    let sql_short: String = sql.split_whitespace().collect::<Vec<_>>().join(" ");
                    let sql_short: String = sql_short.chars().take(300).collect();
                    writeln!(
                        f,
                        "{}\t{}\t{}\t{}\tZETA: {}\tMYSQL: {}",
                        path.display(),
                        st.total,
                        kind.label(),
                        sql_short,
                        z.brief().replace(['\t', '\n'], " ⏎ "),
                        o.brief().replace(['\t', '\n'], " ⏎ ")
                    )?;
                }
            }
        }
        let same = st.by_kind.get(&Kind::Same).copied().unwrap_or(0);
        let kinds: Vec<String> = st
            .by_kind
            .iter()
            .filter(|(k, _)| **k != Kind::Same)
            .map(|(k, n)| format!("{}={n}", k.label()))
            .collect();
        let (fi, fk) = match st.first {
            Some((i, k)) => (i.to_string(), k.label()),
            None => ("-".to_string(), "-"),
        };
        let (hi, hk) = match st.first_hard {
            Some((i, k)) => (i.to_string(), k.label()),
            None => ("-".to_string(), "-"),
        };
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            path.display(),
            st.total,
            same,
            fi,
            fk,
            st.oracle_ok,
            st.oracle_ok_same,
            hi,
            hk,
            kinds.join(",")
        );
        grand.total += st.total;
        grand.oracle_ok += st.oracle_ok;
        grand.oracle_ok_same += st.oracle_ok_same;
        if st.first_hard.is_none() {
            files_hard_clean += 1;
        }
        for (k, n) in &st.by_kind {
            *grand.by_kind.entry(*k).or_default() += n;
        }
        match st.first {
            None => files_same += 1,
            Some((_, k)) => *first_hist.entry(k).or_default() += 1,
        }
    }

    let same = grand.by_kind.get(&Kind::Same).copied().unwrap_or(0);
    eprintln!(
        "\nfiles: {} total, {} identical to MySQL on every statement",
        files.len(),
        files_same
    );
    eprintln!(
        "statements: {} total, {} agree ({:.1}%)",
        grand.total,
        same,
        100.0 * same as f64 / grand.total.max(1) as f64
    );
    eprintln!(
        "statements MySQL accepted: {}, zeta matched {} ({:.1}%); files with no divergence on a statement MySQL accepted: {}",
        grand.oracle_ok,
        grand.oracle_ok_same,
        100.0 * grand.oracle_ok_same as f64 / grand.oracle_ok.max(1) as f64,
        files_hard_clean
    );
    eprintln!("first divergence per file, by kind:");
    for (k, n) in &first_hist {
        eprintln!("  {:<14} {n}", k.label());
    }
    eprintln!("all diverging statements, by kind:");
    for (k, n) in &grand.by_kind {
        if *k != Kind::Same {
            eprintln!("  {:<14} {n}", k.label());
        }
    }
    Ok(())
}
