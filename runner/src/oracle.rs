//! Differential mode: run every statement of a `.test` file against zeta and compare its outcome with MySQL's.
//!
//!
//! The normal mode compares zeta with the sibling `.result` file, which was written for zeta, so a pass there
//! means "unchanged", not "agrees with MySQL". Here the `.result` files are not read.
//!
//! Unlike the normal mode this does not stop at the first failing statement: it runs the whole file and records,
//! per statement, whether the two sides agree. A statement that diverges can change the state the later ones
//! see, so the actionable number per file is the FIRST divergence (everything before it ran on identical
//! state); the statement-level totals are a secondary measure.
//!
//! Safety: statements that could damage a shared server or leak state between files (SHUTDOWN, SET GLOBAL,
//! INTO OUTFILE, account management, ...) are skipped, counted and listed (`hazard.rs`). Every network step has
//! a time budget; a statement that outlives its budget is killed on the server (`KILL CONNECTION`), not just
//! abandoned (`side.rs`).
//!
//! Output carries only counts, statement indexes and mechanical labels on stdout. The optional report file
//! also holds statement text and both sides' output, so it is for local reading and must not be committed.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use walkdir::WalkDir;

use crate::harness::ZetaServer;
use crate::hazard::{normalize, HazardFilter};
use crate::mtr_parser::{parse, Directive};
use crate::outcome::{compare, Kind};
use crate::side::{Side, MAX_RESTARTS_IN_A_ROW};

/// What every mode takes: which test files, and where to write the local report.
pub struct Common<'a> {
    pub suites: &'a [String],
    pub filter: Option<&'a str>,
    pub exclude: &'a [String],
    pub report: Option<&'a Path>,
}

/// One statement of a `.test` file, with the `--sorted_result` that precedes it.
struct Stmt {
    sql: String,
    sorted: bool,
    /// The statement has a top-level ORDER BY, so a different row order is a defect, not noise.
    ordered: bool,
}

/// True if the statement that returns the compared result set (the first of a `;`-separated text) has an ORDER BY
/// outside any parentheses, so not inside a subquery, a window or GROUP_CONCAT. `ORDER BY RAND()` asks for no
/// particular order.
fn has_top_level_order_by(sql: &str) -> bool {
    let s = normalize(sql);
    let s = s.split(';').next().unwrap_or("");
    let b = s.as_bytes();
    let mut depth = 0i32;
    let mut in_backticks = false;
    for i in 0..b.len() {
        match b[i] {
            b'`' => in_backticks = !in_backticks,
            _ if in_backticks => {}
            b'(' => depth += 1,
            b')' => depth -= 1,
            b'O' if depth == 0
                && s[i..].starts_with("ORDER BY")
                && (i == 0 || !b[i - 1].is_ascii_alphanumeric() && b[i - 1] != b'_') =>
            {
                return !s[i + 8..].trim_start().starts_with("RAND(");
            }
            _ => {}
        }
    }
    false
}

/// A row-order-only difference on a statement that asked for an order is a real divergence.
fn settle(kind: Kind, stmt: &Stmt) -> Kind {
    if kind == Kind::RowOrder && stmt.ordered && !stmt.sorted {
        Kind::Values
    } else {
        kind
    }
}

fn statements(directives: Vec<Directive>) -> Vec<Stmt> {
    let mut out = Vec::new();
    let mut sorted = false;
    for d in directives {
        match d {
            Directive::SortedResult => sorted = true,
            Directive::Sql(sql) => {
                let ordered = has_top_level_order_by(&sql);
                out.push(Stmt {
                    sql,
                    sorted,
                    ordered,
                });
                sorted = false;
            }
            _ => {}
        }
    }
    out
}

#[derive(Default)]
struct FileStats {
    /// Statements that ran and were compared.
    total: usize,
    /// Statements not run on either side (hazardous), by class.
    skipped_by: BTreeMap<String, usize>,
    by_kind: BTreeMap<Kind, usize>,
    first: Option<(usize, Kind)>,
    /// First divergence that points at a defect (see `Kind::is_hard`): a result that differs on a statement
    /// MySQL accepted, an error from zeta where MySQL succeeded, or a transport failure.
    first_hard: Option<(usize, Kind)>,
    /// Statements MySQL executed without error, how many of those zeta matched exactly, and how many it
    /// matched up to row order.
    oracle_ok: usize,
    oracle_ok_same: usize,
    oracle_ok_order: usize,
}

impl FileStats {
    fn skipped(&self) -> usize {
        self.skipped_by.values().sum()
    }
}

#[derive(Default)]
struct Totals {
    files: usize,
    files_unparsed: usize,
    files_same: usize,
    files_hard_clean: usize,
    /// Files that had hazardous statements skipped: their agreement covers only the rest.
    files_with_skips: usize,
    statements: usize,
    oracle_ok: usize,
    oracle_ok_same: usize,
    oracle_ok_order: usize,
    by_kind: BTreeMap<Kind, usize>,
    first_hist: BTreeMap<Kind, usize>,
    skipped_by: BTreeMap<String, usize>,
    zeta_restarts: usize,
    completed: bool,
}

const ROW_HEADER: &str = "file\tstatements\tsame\tfirst_divergence\tfirst_kind\tmysql_ok\tmysql_ok_same\tfirst_hard\tfirst_hard_kind\tdiverging_kinds\tskipped\tmysql_ok_order";

/// A full-width row for a file that was not compared, with the reason in the kind columns.
fn print_unusable_row(path: &Path, why: &str) {
    println!(
        "{}\t-\t-\t-\t{why}\t-\t-\t-\t{why}\t{why}=1\t0\t0",
        path.display()
    );
}

impl Totals {
    fn add_file(&mut self, st: &FileStats) {
        self.statements += st.total;
        self.oracle_ok += st.oracle_ok;
        self.oracle_ok_same += st.oracle_ok_same;
        self.oracle_ok_order += st.oracle_ok_order;
        if st.first_hard.is_none() {
            self.files_hard_clean += 1;
        }
        if st.skipped() > 0 {
            self.files_with_skips += 1;
        }
        for (k, n) in &st.by_kind {
            *self.by_kind.entry(*k).or_default() += n;
        }
        for (k, n) in &st.skipped_by {
            *self.skipped_by.entry(k.clone()).or_default() += n;
        }
        match st.first {
            // A file with skipped statements is not "identical on every statement": some never ran.
            None if st.skipped() == 0 => self.files_same += 1,
            None => {}
            Some((_, k)) => *self.first_hist.entry(k).or_default() += 1,
        }
    }

    fn print(&self) {
        let same = self.by_kind.get(&Kind::Same).copied().unwrap_or(0);
        if !self.completed {
            eprintln!("\nRUN ABORTED: the totals below cover only the files that finished");
        }
        eprintln!(
            "\nfiles: {} total, {} identical to MySQL on every statement",
            self.files + self.files_unparsed,
            self.files_same
        );
        eprintln!(
            "statements: {} total, {} agree ({:.1}%)",
            self.statements,
            same,
            100.0 * same as f64 / self.statements.max(1) as f64
        );
        eprintln!(
            "statements MySQL accepted: {}, zeta matched {} ({:.1}%), a further {} up to row order; files with no hard divergence on a statement MySQL accepted: {}",
            self.oracle_ok,
            self.oracle_ok_same,
            100.0 * self.oracle_ok_same as f64 / self.oracle_ok.max(1) as f64,
            self.oracle_ok_order,
            self.files_hard_clean
        );
        let skipped: usize = self.skipped_by.values().sum();
        eprintln!(
            "skipped on both sides (hazardous, not in the totals above): {skipped}, in {} files (those files are never counted as identical)",
            self.files_with_skips
        );
        for (h, n) in &self.skipped_by {
            eprintln!("  {h:<42} {n}");
        }
        if self.files_unparsed > 0 {
            eprintln!(
                "files the parser rejected (not run, not in the totals): {}",
                self.files_unparsed
            );
        }
        if self.zeta_restarts > 0 {
            eprintln!("zeta restarts: {}", self.zeta_restarts);
        }
        eprintln!("first divergence per file, by kind:");
        for (k, n) in &self.first_hist {
            eprintln!("  {:<14} {n}", k.label());
        }
        eprintln!("all diverging statements, by kind:");
        for (k, n) in &self.by_kind {
            if *k != Kind::Same {
                eprintln!("  {:<14} {n}", k.label());
            }
        }
    }
}

fn collect_files(c: &Common<'_>) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for suite in c.suites {
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
            if let Some(f) = c.filter {
                if !p.to_string_lossy().contains(f) {
                    continue;
                }
            }
            if c.exclude
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
    Ok(files)
}

/// The oracle drops databases: refuse a server that is not on this machine unless the operator said so.
fn check_oracle_host(oracle_base: &str, allow_remote: bool) -> Result<()> {
    let opts =
        mysql_async::Opts::from_url(oracle_base).map_err(|e| anyhow!("bad --oracle-url: {e}"))?;
    let host = opts.ip_or_hostname().to_string();
    let local = matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1" | "[::1]");
    if !local && !allow_remote {
        bail!(
            "--oracle-url host `{host}` is not this machine, and the run DROPS every non-system database on it; \
             pass --allow-remote-oracle if that is really what you want"
        );
    }
    Ok(())
}

/// The zeta server under test, and what it takes to keep it usable across files.
struct ZetaCtl<'a> {
    server: &'a mut ZetaServer,
    bin: &'a Path,
    side: Side,
    restarts: usize,
}

impl<'a> ZetaCtl<'a> {
    fn new(server: &'a mut ZetaServer, bin: &'a Path) -> Self {
        let side = Side::new(
            "zeta",
            &format!("mysql://root@127.0.0.1:{}", server.port()),
            true,
        );
        ZetaCtl {
            server,
            bin,
            side,
            restarts: 0,
        }
    }

    async fn restart(&mut self) -> Result<()> {
        let _ = self.server.shutdown().await;
        *self.server = ZetaServer::start(self.bin).await?;
        self.side.base = format!("mysql://root@127.0.0.1:{}", self.server.port());
        self.side.conn = None;
        self.side.conn_id = None;
        self.side.tainted = false;
        self.side.fresh = true;
        self.side.undroppable.clear();
        Ok(())
    }

    /// zeta must be up, idle and empty before a file. A dead or wedged server is restarted, a failed reset retried
    /// on a fresh server; a server that cannot be brought back ends the run.
    async fn prepare(&mut self, path: &Path) -> Result<()> {
        let mut failures = 0u32;
        loop {
            if !self.server.is_alive() || self.side.tainted {
                eprintln!("restarting zeta before {}", path.display());
                self.restart().await.context("restarting zeta")?;
                self.restarts += 1;
            }
            match self.side.reset().await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    failures += 1;
                    eprintln!("zeta reset failed before {}: {e:#}", path.display());
                    if failures >= MAX_RESTARTS_IN_A_ROW {
                        bail!("zeta could not be reset {failures} times in a row; giving up");
                    }
                    self.side.tainted = true;
                }
            }
        }
    }
}

/// Where the oracle's answers come from.
enum Source<'a> {
    Live {
        oracle: &'a mut Side,
        hazards: &'a mut HazardFilter,
    },
}

/// Run one file's statements on zeta and compare each with the oracle's answer.
async fn compare_file(
    path: &Path,
    stmts: &[Stmt],
    zeta: &mut Side,
    mut src: Source<'_>,
    rep: &mut Option<fs::File>,
) -> Result<FileStats> {
    let mut st = FileStats::default();
    for (n, stmt) in stmts.iter().enumerate() {
        let idx = n + 1; // position in the file, skipped statements included
        let sql = &stmt.sql;
        let (z, o, kind) = match &mut src {
            Source::Live { oracle, hazards } => {
                if let Some(why) = hazards.check(sql) {
                    *st.skipped_by.entry(why.to_string()).or_default() += 1;
                    continue;
                }
                let (had_z, had_o) = (zeta.conn.is_some(), oracle.conn.is_some());
                let z = zeta.run(sql).await;
                let o = oracle.run(sql).await;
                // A side that lost its session (timeout, dropped connection) reconnects with a fresh one. Give the
                // other side a fresh one too, so that SET variables, temporary tables and open transactions are
                // lost on both and do not themselves cause later divergences.
                if had_z && zeta.conn.is_none() && oracle.conn.is_some() {
                    oracle.drop_conn().await;
                } else if had_o && oracle.conn.is_none() && zeta.conn.is_some() {
                    zeta.drop_conn().await;
                }
                if oracle.tainted {
                    bail!(
                        "the oracle could not stop a timed-out statement; not continuing against a busy server"
                    );
                }
                let kind = settle(compare(&z, &o, stmt.sorted), stmt);
                (z, o, kind)
            }
        };
        st.total += 1;
        *st.by_kind.entry(kind).or_default() += 1;
        let oracle_accepted = o.accepted();
        if oracle_accepted {
            st.oracle_ok += 1;
            match kind {
                Kind::Same => st.oracle_ok_same += 1,
                Kind::RowOrder => st.oracle_ok_order += 1,
                _ => {}
            }
        }
        if kind != Kind::Same {
            if st.first.is_none() {
                st.first = Some((idx, kind));
            }
            if kind.is_hard(oracle_accepted) && st.first_hard.is_none() {
                st.first_hard = Some((idx, kind));
            }
            if let Some(f) = rep.as_mut() {
                let sql_short: String = sql.split_whitespace().collect::<Vec<_>>().join(" ");
                let sql_short: String = sql_short.chars().take(300).collect();
                writeln!(
                    f,
                    "{}\t{}\t{}\t{}\tZETA: {}\tMYSQL: {}",
                    path.display(),
                    idx,
                    kind.label(),
                    sql_short,
                    z.brief().replace(['\t', '\n'], " ⏎ "),
                    o.brief().replace(['\t', '\n'], " ⏎ ")
                )?;
            }
        }
    }
    Ok(st)
}

fn print_file_row(path: &Path, st: &FileStats) {
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
        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
        path.display(),
        st.total,
        same,
        fi,
        fk,
        st.oracle_ok,
        st.oracle_ok_same,
        hi,
        hk,
        kinds.join(","),
        st.skipped(),
        st.oracle_ok_order
    );
}

/// Parse a test file; on failure print the full-width row (no statement text) and say so.
fn parse_or_report(path: &Path, totals: &mut Totals) -> Result<Option<Vec<Stmt>>> {
    let src = match fs::read(path)
        .with_context(|| format!("reading {}", path.display()))
        .map(String::from_utf8)
    {
        Ok(Ok(s)) => s,
        Ok(Err(_)) => {
            eprintln!("parse error in {}: not valid UTF-8", path.display());
            print_unusable_row(path, "parse-error");
            totals.files_unparsed += 1;
            return Ok(None);
        }
        Err(e) => return Err(e),
    };
    match parse(&src) {
        Ok(d) => Ok(Some(statements(d))),
        Err(e) => {
            // The parser's message may quote the statement, so only its first clause (up to the first colon)
            // is shown, and only on stderr.
            let why = e.to_string();
            eprintln!(
                "parse error in {}: {}",
                path.display(),
                why.split(':').next().unwrap_or("")
            );
            print_unusable_row(path, "parse-error");
            totals.files_unparsed += 1;
            Ok(None)
        }
    }
}

fn open_report(c: &Common<'_>) -> Result<Option<fs::File>> {
    Ok(match c.report {
        Some(p) => Some(fs::File::create(p).with_context(|| format!("creating {}", p.display()))?),
        None => None,
    })
}

/// Run `fut`, but stop on Ctrl-C or SIGTERM with an error, so that the summary of what finished is still printed
/// and the caller can stop what is still running on the servers.
async fn interruptible(fut: impl std::future::Future<Output = Result<()>>) -> Result<()> {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("installing the SIGTERM handler")?;
    tokio::select! {
        r = fut => r,
        _ = tokio::signal::ctrl_c() => Err(anyhow!("interrupted")),
        _ = term.recv() => Err(anyhow!("terminated")),
    }
}

/// After an aborted run a statement may still be executing on a server (the future that sent it was dropped, the
/// server was not told): stop it, so that it neither outlives the run nor blocks the next one.
async fn stop_running(side: &mut Side) {
    if side.conn.is_some() {
        side.kill_running().await;
    }
}

// ---------------------------------------------------------------------------------------------- live

/// zeta against a live oracle.
pub async fn run_diff(
    zserver: &mut ZetaServer,
    zeta_bin: &Path,
    c: Common<'_>,
    oracle_base: &str,
    allow_remote_oracle: bool,
) -> Result<()> {
    check_oracle_host(oracle_base, allow_remote_oracle)?;
    let files = collect_files(&c)?;
    let mut rep = open_report(&c)?;
    let mut zeta = ZetaCtl::new(zserver, zeta_bin);
    let mut oracle = Side::new("oracle", oracle_base, false);
    println!("{ROW_HEADER}");
    let mut totals = Totals::default();
    let res = interruptible(live_files(
        &files,
        &mut zeta,
        &mut oracle,
        &mut rep,
        &mut totals,
    ))
    .await;
    if res.is_err() {
        stop_running(&mut oracle).await;
        stop_running(&mut zeta.side).await;
    }
    totals.zeta_restarts = zeta.restarts;
    totals.completed = res.is_ok();
    totals.print();
    res
}

async fn live_files(
    files: &[PathBuf],
    zeta: &mut ZetaCtl<'_>,
    oracle: &mut Side,
    rep: &mut Option<fs::File>,
    totals: &mut Totals,
) -> Result<()> {
    let mut hazards = HazardFilter::new();
    let mut header_done = false;
    for path in files {
        let Some(stmts) = parse_or_report(path, totals)? else {
            continue;
        };
        zeta.prepare(path).await?;
        oracle.reset().await.context(
            "the oracle could not be reset; not continuing against a server in an unknown state",
        )?;
        if !header_done {
            header_done = true;
            eprintln!("{}", zeta.side.describe().await);
            eprintln!("{}", oracle.describe().await);
        }
        hazards.new_file();
        totals.files += 1;
        let st = compare_file(
            path,
            &stmts,
            &mut zeta.side,
            Source::Live {
                oracle: &mut *oracle,
                hazards: &mut hazards,
            },
            rep,
        )
        .await?;
        print_file_row(path, &st);
        totals.add_file(&st);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_by_is_found_only_at_the_top_level() {
        assert!(has_top_level_order_by("SELECT a FROM t ORDER BY a"));
        assert!(has_top_level_order_by(
            "select a from t order  by a limit 2"
        ));
        assert!(has_top_level_order_by(
            "(SELECT 1) UNION (SELECT 2) ORDER BY 1"
        ));
        assert!(!has_top_level_order_by(
            "SELECT a FROM (SELECT a FROM t ORDER BY a LIMIT 2) x"
        ));
        assert!(!has_top_level_order_by(
            "SELECT GROUP_CONCAT(a ORDER BY a) FROM t"
        ));
        assert!(!has_top_level_order_by(
            "SELECT ROW_NUMBER() OVER (ORDER BY a) FROM t"
        ));
        assert!(!has_top_level_order_by("SELECT 'ORDER BY' FROM t"));
        assert!(!has_top_level_order_by("SELECT a FROM t RECORDER BY a"));
    }

    #[test]
    fn order_by_detection_corner_cases() {
        // Only the first statement of a text returns the compared result set.
        assert!(!has_top_level_order_by(
            "SELECT 1; SELECT a FROM t ORDER BY a"
        ));
        // A parenthesis inside a quoted identifier does not change the nesting.
        assert!(has_top_level_order_by("SELECT `a(b` FROM t ORDER BY 1"));
        // A random order is no order.
        assert!(!has_top_level_order_by("SELECT a FROM t ORDER BY RAND()"));
        assert!(has_top_level_order_by("SELECT a FROM t ORDER BY a, RAND()"));
    }

    #[test]
    fn row_order_is_defect_only_where_an_order_was_asked_for() {
        let plain = Stmt {
            sql: String::new(),
            sorted: false,
            ordered: false,
        };
        let ordered = Stmt {
            ordered: true,
            ..Stmt {
                sql: String::new(),
                sorted: false,
                ordered: false,
            }
        };
        let sorted = Stmt {
            sorted: true,
            ..Stmt {
                sql: String::new(),
                sorted: false,
                ordered: true,
            }
        };
        assert_eq!(settle(Kind::RowOrder, &plain), Kind::RowOrder);
        assert_eq!(settle(Kind::RowOrder, &ordered), Kind::Values);
        assert_eq!(settle(Kind::RowOrder, &sorted), Kind::RowOrder);
        assert_eq!(settle(Kind::Same, &ordered), Kind::Same);
    }

    #[test]
    fn a_file_with_skipped_statements_is_never_identical() {
        let mut t = Totals::default();
        let clean = FileStats {
            total: 3,
            ..Default::default()
        };
        t.add_file(&clean);
        assert_eq!(
            (t.files_same, t.files_hard_clean, t.files_with_skips),
            (1, 1, 0)
        );
        let mut skipped = FileStats {
            total: 3,
            ..Default::default()
        };
        skipped.skipped_by.insert("KILL".into(), 1);
        t.add_file(&skipped);
        assert_eq!(
            (t.files_same, t.files_hard_clean, t.files_with_skips),
            (1, 2, 1)
        );
        let mut diverges = FileStats {
            total: 3,
            ..Default::default()
        };
        diverges.first = Some((2, Kind::Values));
        diverges.first_hard = Some((2, Kind::Values));
        t.add_file(&diverges);
        assert_eq!((t.files_same, t.files_hard_clean), (1, 2));
        assert_eq!(t.first_hist.get(&Kind::Values), Some(&1));
    }

    #[test]
    fn statement_numbers_count_every_statement() {
        let d = vec![
            Directive::Sql("SELECT 1".into()),
            Directive::SortedResult,
            Directive::Sql("SELECT 2 ORDER BY 1".into()),
            Directive::Echo("x".into()),
            Directive::Sql("SHUTDOWN".into()),
        ];
        let s = statements(d);
        assert_eq!(s.len(), 3);
        assert!(!s[0].sorted && s[1].sorted && s[1].ordered && !s[2].sorted);
    }
}
