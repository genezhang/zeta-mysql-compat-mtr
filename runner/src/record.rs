//! Stored oracle outcomes: what MySQL did for every statement of a `.test` file, kept in a committed file
//! next to the test (`tests/<suite>/<name>.oracle.jsonl`) so that zeta can be compared against MySQL
//! without a MySQL server (replay), and so that the answer to "does zeta agree with MySQL" is a reviewable,
//! versioned artefact instead of a live server's mood.
//!
//! File format (JSON Lines, one object per line, UTF-8, keys in the order written here):
//!
//! ```text
//! {"format":"zeta-mysql-oracle-record","version":1,"mysql_version":"8.4.6","sql_mode":"...","time_zone":"+00:00",
//!  "lower_case_table_names":"0","passes":2,"statements":3}
//! {"i":1,"h":"1a2b3c4d","k":"rows","cols":["a","b"],"rows":[["1",null],["x","y"]]}
//! {"i":2,"h":"...","k":"ok","affected":1}
//! {"i":3,"h":"...","k":"err","code":1146,"msg":"Table 'test.t' doesn't exist"}
//! ```
//!
//! `i` is the 1-based position of the statement in the file counting every statement (so a skipped one keeps
//! its number), `h` a short hash of the statement text that detects a record that no longer matches its `.test`.
//! `k` is `rows`, `ok`, `err`, `timeout` (the oracle gave no answer: never counted), `skip` (a hazardous
//! statement that is not run on either server, see `hazard.rs`) or `excluded` (the oracle's answer is not
//! stable, so there is nothing to compare: zeta still runs the statement, to keep its state aligned, but the
//! outcome is not counted). A cell is a JSON string, or `null` for SQL NULL. Errors are compared by `code`;
//! `msg` is informational.
//!
//! Nondeterminism is handled in the open, never silently. A statement that is nondeterministic by its text
//! (`now()`, `rand()`, `@@hostname`, `SHOW STATUS`, ...) carries `"nd":"volatile","class":"<class>"`: the shape
//! (column names, row count, error number, affected rows) is compared, the cell values are not. The recorder
//! runs the whole corpus more than once, against a restored oracle and in a different file order, and
//! compares: a statement whose result differs only in some cells carries `"nd":"cells","masked":[[row,col],..]`
//! (those cells are not compared); one whose rows differ only in order carries `"nd":"order"` (compared as
//! sorted); one that differs in any other way becomes `excluded`. Masking is by COLUMN (`"masked":[1,3]`): the
//! passes may return the rows in different orders, so a position in a row list means nothing. Masked values are
//! stored as the literal `<masked>` (and the rows of an order-insensitive answer in one canonical order) so that
//! recording twice gives the same file. Replay reports how many statements fall in each class.
//!
//! The passes also differ in the oracle's clock: every pass after the first runs its sessions with `SET timestamp`
//! shifted by weeks, so a value derived from the date (a `DEFAULT (CURRENT_DATE)` column, a stored routine that
//! calls `NOW()`) differs between the passes and is masked instead of being recorded as a constant that goes stale
//! at midnight.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::hazard::normalize;
use crate::outcome::{compare, Cell, Kind, Outcome};

pub const FORMAT: &str = "zeta-mysql-oracle-record";
pub const VERSION: u32 = 1;
/// Placed in both sides of a comparison where a cell is masked.
const MASK: &str = "\u{0}masked";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Header {
    pub format: String,
    pub version: u32,
    pub mysql_version: String,
    pub sql_mode: String,
    pub time_zone: String,
    pub lower_case_table_names: String,
    /// How many times the corpus was run to detect nondeterminism.
    pub passes: u32,
    /// Number of statement lines that follow.
    pub statements: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "k", rename_all = "lowercase")]
pub enum Body {
    Rows {
        cols: Vec<String>,
        rows: Vec<Vec<Cell>>,
    },
    Ok {
        affected: u64,
    },
    Err {
        /// Absent when the oracle connection failed without a server error (never an agreement target).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<u32>,
        msg: String,
    },
    Timeout,
    Skip {
        why: String,
    },
    Excluded {
        why: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rec {
    pub i: usize,
    pub h: String,
    #[serde(flatten)]
    pub body: Body,
    /// `volatile`, `cells` or `order` (see the module doc); absent = deterministic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nd: Option<String>,
    /// For `volatile`: the class (`time`, `random`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
    /// For `cells`: the columns whose values are not compared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub masked: Option<Vec<usize>>,
    /// The rows are compared as a set: their order is not part of the answer.
    #[serde(default, skip_serializing_if = "is_false")]
    pub unordered: bool,
    /// The test asked for `--sorted_result` here. Part of what the record was made for: a change invalidates it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub sorted: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// FNV-1a 64 of the statement text (line endings normalised, surrounding whitespace dropped, everything else
/// exact: a space inside a string literal is part of the statement), as 8 hex digits. The low 32 bits are plenty:
/// this only has to notice a changed `.test`, it is not a security boundary.
pub fn stmt_hash(sql: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in sql.trim().replace("\r\n", "\n").bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{:08x}", h as u32 ^ (h >> 32) as u32)
}

impl Rec {
    pub fn new(i: usize, sql: &str, body: Body) -> Self {
        Rec {
            i,
            h: stmt_hash(sql),
            body,
            nd: None,
            class: None,
            masked: None,
            unordered: false,
            sorted: false,
        }
    }

    #[cfg(test)]
    pub fn from_outcome(i: usize, sql: &str, o: &Outcome) -> Self {
        Rec::new(i, sql, Body::from(o))
    }

    pub fn skipped(i: usize, sql: &str, why: &str) -> Self {
        Rec::new(i, sql, Body::Skip { why: why.into() })
    }

    /// The recorded outcome; `None` for a skipped statement.
    pub fn outcome(&self) -> Option<Outcome> {
        Some(match &self.body {
            Body::Rows { cols, rows } => Outcome::Rows {
                cols: cols.clone(),
                rows: rows.clone(),
            },
            Body::Ok { affected } => Outcome::Ok {
                affected: *affected,
            },
            Body::Err { code, msg } => Outcome::Err {
                code: *code,
                msg: msg.clone(),
            },
            Body::Timeout => Outcome::Timeout,
            Body::Skip { .. } | Body::Excluded { .. } => return None,
        })
    }

    /// The oracle's answer is not stable: zeta runs the statement, nothing is compared.
    pub fn is_excluded(&self) -> bool {
        matches!(self.body, Body::Excluded { .. })
    }

    /// Compare zeta's outcome with this record. `None` = the record cannot be compared (excluded).
    /// `sorted` is the test's `--sorted_result`.
    pub fn compare(&self, zeta: &Outcome, sorted: bool) -> Option<Kind> {
        if self.is_excluded() {
            return None;
        }
        let oracle = self.outcome()?;
        let zeta = shrink(zeta);
        let sorted = sorted || self.unordered || self.nd.as_deref() == Some("order");
        let mask_columns = |o: &Outcome, which: &dyn Fn(usize) -> bool| match o {
            Outcome::Rows { cols, rows } => Outcome::Rows {
                cols: cols.clone(),
                rows: rows
                    .iter()
                    .map(|r| {
                        r.iter()
                            .enumerate()
                            .map(|(c, v)| {
                                if which(c) {
                                    Some(MASK.into())
                                } else {
                                    v.clone()
                                }
                            })
                            .collect()
                    })
                    .collect(),
            },
            other => other.clone(),
        };
        let (z, o) = match self.nd.as_deref() {
            // By its text the statement's values cannot be trusted: the shape (column names, row count) is compared.
            Some("volatile") => (
                mask_columns(&zeta, &|_| true),
                mask_columns(&oracle, &|_| true),
            ),
            Some("cells") => {
                let cols = self.masked.clone().unwrap_or_default();
                let which = move |c: usize| cols.contains(&c);
                (mask_columns(&zeta, &which), mask_columns(&oracle, &which))
            }
            _ => (zeta, oracle),
        };
        Some(compare(&z, &o, sorted))
    }
}

impl From<&Outcome> for Body {
    fn from(o: &Outcome) -> Self {
        match o {
            Outcome::Rows { cols, rows } => Body::Rows {
                cols: cols.clone(),
                rows: rows.clone(),
            },
            Outcome::Ok { affected } => Body::Ok {
                affected: *affected,
            },
            Outcome::Err { code, msg } => Body::Err {
                code: *code,
                msg: msg.clone(),
            },
            Outcome::Timeout => Body::Timeout,
        }
    }
}

/// Where the record of `test` lives under `root`: `root/<suite>/<name>.oracle.jsonl` for
/// `tests/<suite>/<name>.test` (a test deeper than that keeps its subdirectories).
pub fn record_path(root: &Path, test: &Path) -> PathBuf {
    let rel = test.strip_prefix("tests").unwrap_or(test);
    root.join(rel).with_extension("oracle.jsonl")
}

pub fn write(path: &Path, header: &Header, recs: &[Rec]) -> Result<()> {
    let mut out = String::new();
    out.push_str(&serde_json::to_string(header)?);
    out.push('\n');
    for r in recs {
        out.push_str(&serde_json::to_string(r)?);
        out.push('\n');
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, out).with_context(|| format!("writing {}", path.display()))
}

pub fn read(path: &Path) -> Result<(Header, Vec<Rec>)> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut lines = text.lines();
    let header: Header = serde_json::from_str(lines.next().ok_or_else(|| anyhow!("empty record"))?)
        .with_context(|| format!("{}: header", path.display()))?;
    if header.format != FORMAT {
        bail!("{}: not a {FORMAT} file", path.display());
    }
    if header.version != VERSION {
        bail!(
            "{}: record format version {} (this runner reads {VERSION})",
            path.display(),
            header.version
        );
    }
    let mut recs = Vec::new();
    for (n, l) in lines.enumerate() {
        let r: Rec = serde_json::from_str(l)
            .with_context(|| format!("{}: line {}", path.display(), n + 2))?;
        recs.push(r);
    }
    if recs.len() != header.statements {
        bail!(
            "{}: header says {} statements, file has {}",
            path.display(),
            header.statements,
            recs.len()
        );
    }
    Ok((header, recs))
}

/// Why a record cannot be used for `statements` (the file's parsed SQL and its `--sorted_result` flags), or `None`
/// if it matches.
pub fn stale_reason(recs: &[Rec], statements: &[(String, bool)]) -> Option<String> {
    if recs.len() != statements.len() {
        return Some(format!(
            "record has {} statements, the test now has {}",
            recs.len(),
            statements.len()
        ));
    }
    for (r, (sql, sorted)) in recs.iter().zip(statements) {
        if r.h != stmt_hash(sql) {
            return Some(format!(
                "statement {} changed since the record was made",
                r.i
            ));
        }
        if r.sorted != *sorted {
            return Some(format!(
                "--sorted_result on statement {} changed since the record was made",
                r.i
            ));
        }
    }
    None
}

/// Statements that are nondeterministic by their text, by class. The text is blanked of comments and string
/// literals first, so a quoted `'NOW()'` does not count. Values that merely change from one run to the next
/// (`create_time` columns, auto-generated timestamps) are not listed here: the recorder finds those by
/// comparing passes.
pub fn volatile_class(sql: &str) -> Option<&'static str> {
    use std::sync::OnceLock;
    static RULES: OnceLock<Vec<(&'static str, regex::Regex)>> = OnceLock::new();
    let rules = RULES.get_or_init(|| {
        let r = |label, pat: &str| (label, regex::Regex::new(pat).expect("static regex"));
        vec![
            r(
                "time",
                r"\b(NOW|SYSDATE|CURDATE|CURTIME|CURRENT_DATE|CURRENT_TIME|CURRENT_TIMESTAMP|LOCALTIME|LOCALTIMESTAMP|UTC_DATE|UTC_TIME|UTC_TIMESTAMP)\b|\bUNIX_TIMESTAMP\s*\(\s*\)",
            ),
            r("random", r"\b(RAND|UUID|UUID_SHORT|RANDOM_BYTES)\s*\("),
            r("session", r"\b(CONNECTION_ID|PS_CURRENT_THREAD_ID|PS_THREAD_ID|IS_USED_LOCK)\s*\("),
            r(
                "server",
                r"\bVERSION\s*\(|@@\s*((GLOBAL|SESSION|LOCAL)\s*\.\s*)?(VERSION\w*|HOSTNAME|PORT|DATADIR|BASEDIR|SOCKET|PID_FILE|SERVER_UUID|LOG_ERROR|TMPDIR|PLUGIN_DIR|LC_MESSAGES_DIR|CHARACTER_SETS_DIR|SECURE_FILE_PRIV|GENERAL_LOG_FILE|SLOW_QUERY_LOG_FILE|SYSTEM_TIME_ZONE|REPORT_HOST|BIND_ADDRESS|INIT_CONNECT|CONNECTION_ID|PSEUDO_THREAD_ID|MYSQLX_\w+|INNODB_\w*(DIR|PATH|FILE)\w*|SSL_\w+|TLS_\w+|PROXY_USER|EXTERNAL_USER|TIMESTAMP)\b",
            ),
            r(
                "server-state",
                r"^SHOW\s+(FULL\s+)?(GLOBAL\s+|SESSION\s+)?(STATUS|VARIABLES|PROCESSLIST|PLUGINS|ENGINE|OPEN\s+TABLES|PROFILES?|BINARY\s+LOGS|BINLOG\s+EVENTS|MASTER\s+STATUS|REPLICA\s+STATUS|SLAVE\s+STATUS|REPLICAS|PRIVILEGES)\b|\b(PERFORMANCE_SCHEMA|SYS)\s*\.|\bINFORMATION_SCHEMA\s*\.\s*`?(PROCESSLIST|GLOBAL_STATUS|SESSION_STATUS|GLOBAL_VARIABLES|SESSION_VARIABLES|INNODB_\w+|PLUGINS|FILES)\b",
            ),
        ]
    });
    let s = normalize(sql);
    rules.iter().find(|(_, r)| r.is_match(&s)).map(|(l, _)| *l)
}

/// True if the statement that returns the compared result set (the first of a `;`-separated text) has an ORDER BY
/// outside any parentheses, so not inside a subquery, a window or GROUP_CONCAT. `ORDER BY RAND()` asks for no
/// particular order.
pub fn has_top_level_order_by(sql: &str) -> bool {
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

const PLACEHOLDER: &str = "<masked>";

/// Columns that hold the storage engine's estimates or clock readings, whatever statement returns them (`SHOW INDEX`,
/// `SHOW TABLE STATUS`, `information_schema.TABLES / STATISTICS / PARTITIONS`): InnoDB samples them, so they differ
/// between server restarts, and recording the value of one pass would make the record flaky. Masked in every record
/// that has such a column, whether or not the passes happened to differ.
const ESTIMATE_COLUMNS: &[&str] = &[
    "CARDINALITY",
    "TABLE_ROWS",
    "AVG_ROW_LENGTH",
    "DATA_LENGTH",
    "MAX_DATA_LENGTH",
    "INDEX_LENGTH",
    "DATA_FREE",
    "AUTO_INCREMENT",
    "CREATE_TIME",
    "UPDATE_TIME",
    "CHECK_TIME",
    "MAX_INDEX_LENGTH",
    "SUB_PART",
];

/// The positions of the estimate columns in a result set. `Rows` only counts in the shape of `SHOW TABLE STATUS`.
fn estimate_columns(cols: &[String]) -> Vec<usize> {
    let upper: Vec<String> = cols.iter().map(|c| c.to_ascii_uppercase()).collect();
    let table_status = upper.iter().any(|c| c == "AVG_ROW_LENGTH");
    upper
        .iter()
        .enumerate()
        .filter(|(_, c)| {
            ESTIMATE_COLUMNS.contains(&c.as_str()) || (table_status && c.as_str() == "ROWS")
        })
        .map(|(i, _)| i)
        .collect()
}

/// A query over `information_schema` without an ORDER BY: MySQL documents its row order as undefined, and it
/// does change between server restarts.
fn unordered_by_definition(sql: &str) -> bool {
    normalize(sql).contains("INFORMATION_SCHEMA") && !has_top_level_order_by(sql)
}

/// ... and with a LIMIT, WHICH rows come back is undefined too: only the shape of the answer is stable.
fn unordered_limit(sql: &str) -> Option<&'static str> {
    let n = normalize(sql);
    (unordered_by_definition(sql) && n.split(';').next().is_some_and(|f| f.contains(" LIMIT ")))
        .then_some("unordered-limit")
}

/// A cell longer than this is stored as its length and a hash: one `REPEAT('x', 10000000)` would otherwise put
/// ten megabytes into a committed file. zeta's cells are shrunk the same way before they are compared.
const BIG_CELL: usize = 4096;

fn fnv64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn shrink_cell(c: &Cell) -> Cell {
    match c {
        Some(s) if s.len() > BIG_CELL => Some(format!(
            "<big:len={}:fnv={:016x}>",
            s.len(),
            fnv64(s.as_bytes())
        )),
        _ => c.clone(),
    }
}

/// The outcome as it is stored and compared: oversized cells replaced by a length and a hash.
pub fn shrink(o: &Outcome) -> Outcome {
    match o {
        Outcome::Rows { cols, rows }
            if rows
                .iter()
                .flatten()
                .any(|c| matches!(c, Some(s) if s.len() > BIG_CELL)) =>
        {
            Outcome::Rows {
                cols: cols.clone(),
                rows: rows
                    .iter()
                    .map(|r| r.iter().map(shrink_cell).collect())
                    .collect(),
            }
        }
        _ => o.clone(),
    }
}

fn excluded(i: usize, sql: &str, why: &str) -> Rec {
    Rec::new(i, sql, Body::Excluded { why: why.into() })
}

/// Compare two passes' result sets: the columns whose values differ (compared as sets, so a different row order is
/// not a difference) and whether the rows come in another order. `Err` = the answers differ beyond that.
fn merge_rows(
    a: &[Vec<Cell>],
    b: &[Vec<Cell>],
) -> std::result::Result<(Vec<usize>, bool), &'static str> {
    if a.len() != b.len() {
        return Err("row count differs between passes");
    }
    let width = a.first().map_or(0, Vec::len);
    if a.iter().chain(b).any(|r| r.len() != width) {
        return Err("row width differs between passes");
    }
    fn column(rows: &[Vec<Cell>], c: usize) -> Vec<&Cell> {
        let mut v: Vec<&Cell> = rows.iter().map(|r| &r[c]).collect();
        v.sort();
        v
    }
    let masked: Vec<usize> = (0..width)
        .filter(|&c| column(a, c) != column(b, c))
        .collect();
    let project = |rows: &[Vec<Cell>]| -> Vec<Vec<Cell>> {
        rows.iter()
            .map(|r| {
                r.iter()
                    .enumerate()
                    .filter(|(c, _)| !masked.contains(c))
                    .map(|(_, v)| v.clone())
                    .collect()
            })
            .collect()
    };
    let (pa, pb) = (project(a), project(b));
    let (mut sa, mut sb) = (pa.clone(), pb.clone());
    sa.sort();
    sb.sort();
    if sa != sb {
        return Err("rows differ between passes beyond the columns that vary");
    }
    Ok((masked, pa != pb))
}

/// Merge the outcomes of one statement over several passes into its record. The first pass supplies the stored
/// answer; where the passes disagree the disagreement is made explicit (see the module doc) and the disagreeing
/// values are replaced by a fixed placeholder, so that recording twice gives the same file.
pub fn merge_passes(i: usize, sql: &str, passes: &[&Outcome]) -> Rec {
    let first = passes[0];
    let mut body = Body::from(first);
    let mut masked: Vec<usize> = Vec::new();
    let mut reordered = false;
    let mut msg_varies = false;
    for other in &passes[1..] {
        match (first, *other) {
            (a, b) if a == b => {}
            // Same error number, other text: the text is informational.
            (Outcome::Err { code: a, .. }, Outcome::Err { code: b, .. }) if a == b => {
                msg_varies = true
            }
            (Outcome::Rows { cols: ac, rows: ar }, Outcome::Rows { cols: bc, rows: br })
                if ac == bc =>
            {
                match merge_rows(ar, br) {
                    Ok((cols, order)) => {
                        masked.extend(cols);
                        reordered |= order;
                    }
                    Err(why) => return excluded(i, sql, why),
                }
            }
            (a, b) => {
                return excluded(
                    i,
                    sql,
                    &format!(
                        "outcome differs between passes: {} vs {}",
                        describe(a),
                        describe(b)
                    ),
                )
            }
        }
    }
    if msg_varies {
        if let Body::Err { msg, .. } = &mut body {
            *msg = "<varies>".into();
        }
    }
    if let Body::Rows { cols, rows } = &body {
        if !rows.is_empty() {
            masked.extend(estimate_columns(cols));
        }
    }
    masked.sort();
    masked.dedup();
    let mut rec = Rec::new(i, sql, body);
    if let Body::Rows { rows, .. } = &mut rec.body {
        let unordered = reordered || unordered_by_definition(sql);
        if let Some(class) = volatile_class(sql).or_else(|| unordered_limit(sql)) {
            // By its text the statement's values cannot be trusted: shape only.
            for row in rows.iter_mut() {
                for c in row.iter_mut() {
                    *c = Some(PLACEHOLDER.into());
                }
            }
            rec.nd = Some("volatile".into());
            rec.class = Some(class.into());
        } else if !masked.is_empty() {
            for row in rows.iter_mut() {
                for &c in &masked {
                    row[c] = Some(PLACEHOLDER.into());
                }
            }
            rec.nd = Some("cells".into());
            rec.masked = Some(masked);
            rec.unordered = unordered;
        } else if unordered {
            rec.nd = Some("order".into());
            rec.unordered = true;
        }
        if rec.unordered {
            // The order is not part of the answer: store one canonical order so recording twice gives the same
            // file, and compare as a set.
            rows.sort();
        }
    }
    rec
}

fn describe(o: &Outcome) -> String {
    match o {
        Outcome::Rows { rows, .. } => format!("{} rows", rows.len()),
        Outcome::Ok { affected } => format!("ok({affected})"),
        Outcome::Err { code, .. } => match code {
            Some(c) => format!("err {c}"),
            None => "err".into(),
        },
        Outcome::Timeout => "timeout".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(data: &[&[Option<&str>]]) -> Outcome {
        Outcome::Rows {
            cols: vec!["a".into(), "b".into()],
            rows: data
                .iter()
                .map(|r| r.iter().map(|c| c.map(str::to_string)).collect())
                .collect(),
        }
    }

    #[test]
    fn hash_ignores_only_surrounding_whitespace() {
        assert_eq!(stmt_hash("  SELECT 1\n"), stmt_hash("SELECT 1"));
        assert_ne!(stmt_hash("SELECT   1"), stmt_hash("SELECT 1"));
        assert_ne!(stmt_hash("SELECT 1"), stmt_hash("SELECT 2"));
    }

    #[test]
    fn records_round_trip_through_json() {
        let r = Rec::from_outcome(3, "SELECT 1", &rows(&[&[Some("1"), None]]));
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.starts_with(r#"{"i":3,"h":""#), "{s}");
        assert!(
            s.contains(r#""k":"rows""#) && s.contains(r#"[["1",null]]"#),
            "{s}"
        );
        assert_eq!(serde_json::from_str::<Rec>(&s).unwrap(), r);
        let e = Rec::from_outcome(
            4,
            "x",
            &Outcome::Err {
                code: Some(1146),
                msg: "no".into(),
            },
        );
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains(r#""k":"err","code":1146,"msg":"no""#), "{s}");
        assert_eq!(serde_json::from_str::<Rec>(&s).unwrap(), e);
        for k in [
            Rec::skipped(5, "SHUTDOWN", "SHUTDOWN / RESTART"),
            excluded(6, "q", "why"),
        ] {
            assert_eq!(
                serde_json::from_str::<Rec>(&serde_json::to_string(&k).unwrap()).unwrap(),
                k
            );
        }
    }

    #[test]
    fn merge_classifies_passes() {
        let a = rows(&[&[Some("1"), Some("x")], &[Some("2"), Some("y")]]);
        let r = merge_passes(1, "q", &[&a, &a]);
        assert_eq!((r.nd.clone(), r.body.clone()), (None, Body::from(&a)));
        // One cell differs: its column is masked, and the stored cells of that column are the placeholder.
        let b = rows(&[&[Some("1"), Some("z")], &[Some("2"), Some("y")]]);
        let r = merge_passes(1, "q", &[&a, &b]);
        assert_eq!(r.nd.as_deref(), Some("cells"));
        assert_eq!(r.masked, Some(vec![1]));
        assert!(!r.unordered);
        assert_eq!(
            r.outcome().unwrap(),
            rows(&[
                &[Some("1"), Some("<masked>")],
                &[Some("2"), Some("<masked>")]
            ])
        );
        // The rows moved AND a column varies (information_schema.tables with create_time): the varying column is
        // masked, the rest is compared as a set, and a wrong value in the stable column is still caught.
        let t1 = rows(&[&[Some("a"), Some("T1")], &[Some("b"), Some("T2")]]);
        let t2 = rows(&[&[Some("b"), Some("U1")], &[Some("a"), Some("U2")]]);
        let r = merge_passes(1, "SELECT n, ts FROM t", &[&t1, &t2]);
        assert_eq!(
            (r.nd.as_deref(), r.masked.clone(), r.unordered),
            (Some("cells"), Some(vec![1]), true)
        );
        let swapped = rows(&[&[Some("b"), Some("whatever")], &[Some("a"), Some("else")]]);
        assert_eq!(r.compare(&swapped, false), Some(Kind::Same));
        let wrong = rows(&[&[Some("WRONG"), Some("x")], &[Some("a"), Some("y")]]);
        assert_eq!(r.compare(&wrong, false), Some(Kind::Values));
        // Same rows, other order.
        let c = rows(&[&[Some("2"), Some("y")], &[Some("1"), Some("x")]]);
        assert_eq!(merge_passes(1, "q", &[&a, &c]).nd.as_deref(), Some("order"));
        // Different row count / outcome kind: excluded, with a reason.
        let d = rows(&[&[Some("1"), Some("x")]]);
        assert!(matches!(
            merge_passes(1, "q", &[&a, &d]).body,
            Body::Excluded { .. }
        ));
        let e = Outcome::Err {
            code: Some(1),
            msg: "m".into(),
        };
        assert!(matches!(
            merge_passes(1, "q", &[&a, &e]).body,
            Body::Excluded { .. }
        ));
        // Same error number, other text: still deterministic, text not stored.
        let e2 = Outcome::Err {
            code: Some(1),
            msg: "other".into(),
        };
        let r = merge_passes(1, "q", &[&e, &e2]);
        assert_eq!(r.nd, None);
        assert_eq!(
            r.body,
            Body::Err {
                code: Some(1),
                msg: "<varies>".into()
            }
        );
        // A volatile statement keeps its shape and nothing else.
        let r = merge_passes(1, "SELECT NOW(), 1", &[&a, &a]);
        assert_eq!(
            (r.nd.as_deref(), r.class.as_deref()),
            (Some("volatile"), Some("time"))
        );
        assert_eq!(
            r.outcome().unwrap(),
            rows(&[
                &[Some("<masked>"), Some("<masked>")],
                &[Some("<masked>"), Some("<masked>")]
            ])
        );
    }

    #[test]
    fn masks_hide_only_the_masked_columns() {
        let o = rows(&[&[Some("1"), Some("x")], &[Some("2"), Some("y")]]);
        let mut rec = Rec::from_outcome(1, "q", &o);
        rec.nd = Some("cells".into());
        rec.masked = Some(vec![1]);
        let same_but_masked = rows(&[&[Some("1"), Some("OTHER")], &[Some("2"), Some("y")]]);
        assert_eq!(rec.compare(&same_but_masked, false), Some(Kind::Same));
        let differs_elsewhere = rows(&[&[Some("9"), Some("x")], &[Some("2"), Some("y")]]);
        assert_eq!(rec.compare(&differs_elsewhere, false), Some(Kind::Values));
        // A mask is by column, so it still lands on the right cells when zeta returns the rows in another order
        // and the test sorts the result.
        let reordered = rows(&[&[Some("2"), Some("q")], &[Some("1"), Some("w")]]);
        assert_eq!(rec.compare(&reordered, true), Some(Kind::Same));
        // A volatile statement compares shape only.
        let mut v = Rec::from_outcome(1, "q", &o);
        v.nd = Some("volatile".into());
        assert_eq!(v.compare(&differs_elsewhere, false), Some(Kind::Same));
        let fewer = rows(&[&[Some("1"), Some("x")]]);
        assert_eq!(v.compare(&fewer, false), Some(Kind::RowCount));
        // An excluded one compares to nothing; a timed-out one is the oracle's problem.
        assert_eq!(excluded(1, "q", "w").compare(&o, false), None);
        let t = Rec::from_outcome(1, "q", &Outcome::Timeout);
        assert_eq!(t.compare(&o, false), Some(Kind::OracleTimeout));
        // Order-nondeterministic: compared as sorted.
        let mut ord = Rec::from_outcome(1, "q", &o);
        ord.nd = Some("order".into());
        ord.unordered = true;
        let swapped = rows(&[&[Some("2"), Some("y")], &[Some("1"), Some("x")]]);
        assert_eq!(ord.compare(&swapped, false), Some(Kind::Same));
    }

    #[test]
    fn oversized_cells_are_stored_and_compared_as_a_hash() {
        let big = "x".repeat(10_000);
        let o = Outcome::Rows {
            cols: vec!["c".into()],
            rows: vec![vec![Some(big.clone())]],
        };
        let small = shrink(&o);
        let Outcome::Rows { rows, .. } = &small else {
            panic!()
        };
        let cell = rows[0][0].as_deref().unwrap();
        assert!(cell.starts_with("<big:len=10000:fnv="), "{cell}");
        // The record holds the shrunk form; zeta's full-size value compares equal, a different one does not.
        let rec = Rec::from_outcome(1, "q", &small);
        assert_eq!(rec.compare(&o, false), Some(Kind::Same));
        let other = Outcome::Rows {
            cols: vec!["c".into()],
            rows: vec![vec![Some("y".repeat(10_000))]],
        };
        assert_eq!(rec.compare(&other, false), Some(Kind::Values));
        // Small cells are untouched.
        assert_eq!(shrink(&rows_of("a")), rows_of("a"));
    }

    fn rows_of(v: &str) -> Outcome {
        Outcome::Rows {
            cols: vec!["c".into()],
            rows: vec![vec![Some(v.into())]],
        }
    }

    #[test]
    fn information_schema_row_order_is_not_part_of_the_answer() {
        let a = rows(&[&[Some("2"), Some("y")], &[Some("1"), Some("x")]]);
        let r = merge_passes(1, "SELECT a, b FROM information_schema.columns", &[&a, &a]);
        assert_eq!(r.nd.as_deref(), Some("order"));
        // Stored in one canonical order.
        assert_eq!(
            r.outcome().unwrap(),
            rows(&[&[Some("1"), Some("x")], &[Some("2"), Some("y")]])
        );
        // An explicit ORDER BY keeps the order meaningful.
        let r = merge_passes(
            1,
            "SELECT a, b FROM information_schema.columns ORDER BY a DESC",
            &[&a, &a],
        );
        assert_eq!(r.nd, None);
        assert_eq!(r.outcome().unwrap(), a);
    }

    #[test]
    fn engine_estimates_are_masked_even_when_the_passes_agree() {
        let o = Outcome::Rows {
            cols: vec!["Table".into(), "Key_name".into(), "Cardinality".into()],
            rows: vec![vec![
                Some("t".into()),
                Some("PRIMARY".into()),
                Some("3".into()),
            ]],
        };
        let r = merge_passes(1, "SHOW INDEX FROM t", &[&o, &o]);
        assert_eq!(
            (r.nd.as_deref(), r.masked.clone()),
            (Some("cells"), Some(vec![2]))
        );
        // Key_name is still compared.
        let z = Outcome::Rows {
            cols: vec!["Table".into(), "Key_name".into(), "Cardinality".into()],
            rows: vec![vec![
                Some("t".into()),
                Some("idx".into()),
                Some("99".into()),
            ]],
        };
        assert_eq!(r.compare(&z, false), Some(Kind::Values));
        let z = Outcome::Rows {
            cols: vec!["Table".into(), "Key_name".into(), "Cardinality".into()],
            rows: vec![vec![
                Some("t".into()),
                Some("PRIMARY".into()),
                Some("99".into()),
            ]],
        };
        assert_eq!(r.compare(&z, false), Some(Kind::Same));
    }

    #[test]
    fn a_limit_over_an_unordered_catalog_is_shape_only() {
        let a = rows(&[&[Some("1"), Some("x")]]);
        let r = merge_passes(
            1,
            "SELECT a, b FROM information_schema.parameters LIMIT 5",
            &[&a, &a],
        );
        assert_eq!(
            (r.nd.as_deref(), r.class.as_deref()),
            (Some("volatile"), Some("unordered-limit"))
        );
        let r = merge_passes(
            1,
            "SELECT a, b FROM information_schema.parameters ORDER BY a LIMIT 5",
            &[&a, &a],
        );
        assert_eq!(r.nd, None);
    }

    #[test]
    fn volatile_classes_look_at_code_not_strings() {
        assert_eq!(volatile_class("SELECT NOW()"), Some("time"));
        assert_eq!(volatile_class("select current_date"), Some("time"));
        assert_eq!(volatile_class("SELECT UUID()"), Some("random"));
        assert_eq!(volatile_class("SELECT CONNECTION_ID()"), Some("session"));
        assert_eq!(volatile_class("SELECT @@hostname"), Some("server"));
        assert_eq!(volatile_class("SELECT @@global.version"), Some("server"));
        assert_eq!(
            volatile_class("SHOW GLOBAL STATUS LIKE 'x'"),
            Some("server-state")
        );
        assert_eq!(
            volatile_class("SELECT * FROM performance_schema.threads"),
            Some("server-state")
        );
        assert_eq!(volatile_class("SELECT 'NOW()' AS c"), None);
        assert_eq!(volatile_class("SELECT UNIX_TIMESTAMP('2020-01-01')"), None);
        assert_eq!(volatile_class("SELECT @@sql_mode"), None);
        assert_eq!(
            volatile_class("SELECT table_name FROM information_schema.tables"),
            None
        );
    }

    #[test]
    fn staleness_is_detected() {
        let stmts = vec![
            ("SELECT 1".to_string(), false),
            ("SELECT 2".to_string(), false),
        ];
        let recs: Vec<Rec> = stmts
            .iter()
            .enumerate()
            .map(|(i, (s, _))| Rec::from_outcome(i + 1, s, &Outcome::Ok { affected: 0 }))
            .collect();
        assert_eq!(stale_reason(&recs, &stmts), None);
        let changed = vec![
            ("SELECT 1".to_string(), false),
            ("SELECT 3".to_string(), false),
        ];
        assert!(stale_reason(&recs, &changed)
            .unwrap()
            .contains("statement 2"));
        assert!(stale_reason(&recs, &stmts[..1])
            .unwrap()
            .contains("2 statements"));
        // Whitespace inside a literal is part of the statement; --sorted_result is part of the record.
        let lit = Rec::from_outcome(1, "SELECT 'a  b'", &Outcome::Ok { affected: 0 });
        assert!(stale_reason(
            std::slice::from_ref(&lit),
            &[("SELECT 'a b'".to_string(), false)]
        )
        .is_some());
        assert!(stale_reason(&[lit], &[("SELECT 'a  b'".to_string(), true)])
            .unwrap()
            .contains("sorted_result"));
        // Line endings do not matter.
        assert_eq!(
            stmt_hash("SELECT 1\r\nFROM t"),
            stmt_hash("SELECT 1\nFROM t")
        );
    }
}
