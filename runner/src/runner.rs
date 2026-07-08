//! Orchestrator: walk `tests/<suite>/**/*.test`, parse each, execute its
//! directives against zeta over the MySQL wire, capture output in
//! MTR-style `.result` format, and diff against the sibling `.result`
//! file.
//!
//! M0 result format (independently chosen — MTR's exact output isn't
//! standardized and varies between MySQL versions):
//!
//!   <statement-echo>;
//!   col1\tcol2\t...        (header row, only if the stmt produced rows)
//!   v1\tv2\t...             (one tab-separated row per result row)
//!
//! Statements that produce no rows just emit the echo line. `--echo`
//! emits its argument verbatim. Errors emit `ERROR <code>: <msg>`.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use mysql_async::prelude::Queryable;
use walkdir::WalkDir;

use crate::mtr_parser::{parse, Directive};
use crate::result_diff::compare;
use crate::skip_list::SkipList;

const TESTS_ROOT: &str = "tests";
const DEFAULT_DB: &str = "test";

pub async fn run_suite(
    suite: &str,
    mysql_url_base: &str,
    filter: Option<&str>,
    skip_list: &SkipList,
) -> Result<()> {
    let test_files = discover_tests(suite, filter)?;
    if test_files.is_empty() {
        eprintln!(
            "no .test files matched suite={suite}{}",
            filter.map(|f| format!(" filter={f}")).unwrap_or_default()
        );
        return Ok(());
    }

    let url = format!("{mysql_url_base}/{DEFAULT_DB}");
    eprintln!("connecting to {url}");

    // Per-test-file connection so DDL state in one file doesn't leak.
    // (Real MTR isolates by spinning up a fresh server per suite; we get
    // away with one zeta + per-file connection because zeta uses an
    // in-memory backend and tests are responsible for their own cleanup.)
    let mut passed = 0usize;
    let mut failed: Vec<(PathBuf, String)> = Vec::new();
    let mut skipped = 0usize;
    for test_path in &test_files {
        if skip_list.should_skip(test_path) {
            eprintln!("→ {} (skipped)", test_path.display());
            skipped += 1;
            continue;
        }
        eprintln!("→ {}", test_path.display());
        match run_one(test_path, &url).await {
            Ok(()) => {
                passed += 1;
                eprintln!("  OK");
            }
            Err(e) => {
                eprintln!("  FAIL\n{e}");
                failed.push((test_path.clone(), e.to_string()));
            }
        }
    }

    eprintln!(
        "\n{passed} passed, {} failed, {} skipped",
        failed.len(),
        skipped
    );
    if !failed.is_empty() {
        return Err(anyhow!("{} .test file(s) failed", failed.len()));
    }
    Ok(())
}

async fn run_one(test_path: &Path, mysql_url: &str) -> Result<()> {
    let result_path = test_path.with_extension("result");
    let test_src = fs::read_to_string(test_path)
        .with_context(|| format!("reading {}", test_path.display()))?;
    let expected = fs::read_to_string(&result_path)
        .with_context(|| format!("reading expected {}", result_path.display()))?;

    let directives =
        parse(&test_src).with_context(|| format!("parsing {}", test_path.display()))?;

    let opts = mysql_async::Opts::from_url(mysql_url)?;
    let mut conn = mysql_async::Conn::new(opts).await?;

    let mut actual = String::new();
    let mut pending_sort = false;
    let mut pending_errors: Vec<u32> = Vec::new();

    for directive in directives {
        match directive {
            Directive::Sql(sql) => {
                writeln!(actual, "{sql};")?;
                let outcome = execute_capturing(&mut conn, &sql).await;
                match outcome {
                    Ok(rendered) => {
                        if !pending_errors.is_empty() {
                            return Err(anyhow!(
                                "expected error in {pending_errors:?} for `{sql}`, but it succeeded"
                            ));
                        }
                        let rendered = if pending_sort {
                            sort_block(&rendered)
                        } else {
                            rendered
                        };
                        actual.push_str(&rendered);
                    }
                    Err(err) => {
                        let (code, msg) = mysql_error_parts(&err);
                        if pending_errors.is_empty() {
                            return Err(anyhow!(
                                "unexpected error from `{sql}`: code={code:?} msg={msg}"
                            ));
                        }
                        let Some(code_num) = code else {
                            return Err(anyhow!(
                                "expected error in {pending_errors:?} for `{sql}`, got non-server error: {msg}"
                            ));
                        };
                        if !pending_errors.contains(&code_num) {
                            return Err(anyhow!(
                                "expected error in {pending_errors:?} for `{sql}`, got {code_num}: {msg}"
                            ));
                        }
                        writeln!(actual, "ERROR {code_num}: {msg}")?;
                    }
                }
                pending_sort = false;
                pending_errors.clear();
            }
            Directive::Echo(text) => {
                writeln!(actual, "{text}")?;
            }
            Directive::SortedResult => {
                pending_sort = true;
            }
            Directive::ExpectError(codes) => {
                pending_errors = codes;
            }
            Directive::Unknown(line) => {
                eprintln!("    [unimplemented directive ignored: {line}]");
            }
        }
    }

    conn.disconnect().await.ok();

    compare(&actual, &expected)?;
    Ok(())
}

async fn execute_capturing(
    conn: &mut mysql_async::Conn,
    sql: &str,
) -> Result<String, mysql_async::Error> {
    if returns_rows(sql) {
        let rows: Vec<mysql_async::Row> = conn.query(sql).await?;
        Ok(format_rows(&rows))
    } else {
        conn.query_drop(sql).await?;
        // No rows, no extra output: just the echo line already written.
        Ok(String::new())
    }
}

fn returns_rows(sql: &str) -> bool {
    let head = sql.trim_start();
    let head = head
        .lines()
        .find(|l| !l.trim().starts_with("--"))
        .unwrap_or("");
    let word = head
        .split(|c: char| c.is_whitespace() || c == '(')
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    matches!(
        word.as_str(),
        "SELECT" | "SHOW" | "EXPLAIN" | "DESCRIBE" | "DESC" | "WITH" | "VALUES" | "TABLE"
    )
}

fn format_rows(rows: &[mysql_async::Row]) -> String {
    let mut out = String::new();
    let Some(first) = rows.first() else {
        return out; // no rows => no header, nothing to emit
    };
    let cols = first.columns_ref();
    // Header line.
    let names: Vec<String> = cols.iter().map(|c| c.name_str().into_owned()).collect();
    out.push_str(&names.join("\t"));
    out.push('\n');
    // Data lines.
    for row in rows {
        let mut cells = Vec::with_capacity(row.len());
        for i in 0..row.len() {
            let v = row.as_ref(i).expect("column index in range");
            cells.push(format_value(v));
        }
        out.push_str(&cells.join("\t"));
        out.push('\n');
    }
    out
}

fn format_value(v: &mysql_async::Value) -> String {
    use mysql_async::Value::*;
    match v {
        NULL => "NULL".to_string(),
        Int(i) => i.to_string(),
        UInt(u) => u.to_string(),
        Float(f) => f.to_string(),
        Double(d) => d.to_string(),
        Bytes(b) => match std::str::from_utf8(b) {
            Ok(s) => s.to_string(),
            Err(_) => format!("0x{}", to_hex(b)),
        },
        Date(y, m, d, hh, mm, ss, _us) => {
            if *hh == 0 && *mm == 0 && *ss == 0 {
                format!("{y:04}-{m:02}-{d:02}")
            } else {
                format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}")
            }
        }
        Time(neg, days, h, m, s, _us) => {
            let sign = if *neg { "-" } else { "" };
            let total = (*days as u32) * 24 + (*h as u32);
            format!("{sign}{total:02}:{m:02}:{s:02}")
        }
    }
}

fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Sort the data rows within a result block. The header row (first line)
/// stays in place; everything below it is sorted lexicographically.
fn sort_block(block: &str) -> String {
    let mut lines = block.lines();
    let Some(header) = lines.next() else {
        return String::new();
    };
    let mut data: Vec<&str> = lines.collect();
    data.sort();
    let mut out = String::with_capacity(block.len());
    out.push_str(header);
    out.push('\n');
    for line in data {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Pull (errno, message) out of a mysql_async error. Non-server errors
/// have no errno.
fn mysql_error_parts(err: &mysql_async::Error) -> (Option<u32>, String) {
    if let mysql_async::Error::Server(se) = err {
        return (Some(se.code as u32), se.message.clone());
    }
    (None, err.to_string())
}

fn discover_tests(suite: &str, filter: Option<&str>) -> Result<Vec<PathBuf>> {
    let root: PathBuf = if suite == "all" {
        PathBuf::from(TESTS_ROOT)
    } else {
        PathBuf::from(TESTS_ROOT).join(suite)
    };
    if !root.exists() {
        return Err(anyhow!(
            "suite directory {} does not exist (cwd={})",
            root.display(),
            std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        ));
    }

    let mut out = Vec::new();
    for entry in WalkDir::new(&root).follow_links(false) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.into_path();
        if path.extension().and_then(|s| s.to_str()) != Some("test") {
            continue;
        }
        if let Some(needle) = filter {
            if !path.to_string_lossy().contains(needle) {
                continue;
            }
        }
        out.push(path);
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sort_block_preserves_header() {
        let input = "id\tname\n3\tc\n1\ta\n2\tb\n";
        let sorted = sort_block(input);
        assert_eq!(sorted, "id\tname\n1\ta\n2\tb\n3\tc\n");
    }

    #[test]
    fn returns_rows_recognizes_select_family() {
        assert!(returns_rows("SELECT 1"));
        assert!(returns_rows("  select x from t"));
        assert!(returns_rows("WITH cte AS (SELECT 1) SELECT * FROM cte"));
        assert!(returns_rows("SHOW TABLES"));
        assert!(!returns_rows("INSERT INTO t VALUES (1)"));
        assert!(!returns_rows("CREATE TABLE t (id INT)"));
    }
}
