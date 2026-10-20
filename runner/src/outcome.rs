//! What one statement did on one server, and how two such outcomes are compared.
//!
//! Shared by the live differential mode (`oracle.rs`) and, through the same `compare`, by anything that
//! replays a stored outcome. Cells are `Option<String>` so that SQL NULL and the four-letter string
//! `'NULL'` are different values.

use std::fmt::Write as _;
use std::time::Duration;

use mysql_async::prelude::Queryable;

use crate::runner::format_value;

/// One cell of a result row. `None` is SQL NULL.
pub type Cell = Option<String>;

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Rows {
        cols: Vec<String>,
        rows: Vec<Vec<Cell>>,
    },
    Ok {
        affected: u64,
    },
    /// `code` is the server's error number; `None` means no server answered with one (the connection
    /// failed, dropped or broke the protocol).
    Err {
        code: Option<u32>,
        msg: String,
    },
    Timeout,
}

impl Outcome {
    /// The server executed the statement without error (it returned a result set or an OK).
    pub fn accepted(&self) -> bool {
        matches!(self, Outcome::Rows { .. } | Outcome::Ok { .. })
    }

    /// Short human-readable form for the local report. NULL prints bare, the string 'NULL' quoted.
    pub fn brief(&self) -> String {
        let cell = |c: &Cell| match c {
            None => "NULL".to_string(),
            Some(s) if s == "NULL" => "'NULL'".to_string(),
            Some(s) => s.clone(),
        };
        let s = match self {
            Outcome::Rows { cols, rows } => {
                let mut s = cols.join("|");
                for r in rows.iter().take(6) {
                    let _ = write!(s, "\n{}", r.iter().map(cell).collect::<Vec<_>>().join("|"));
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

/// Why two outcomes differ. `Same` is the only exact-agreement label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Same,
    /// zeta returns an error, MySQL succeeds: missing feature or defect.
    ZetaError,
    /// zeta succeeds, MySQL returns an error: missing validation.
    ZetaAccepts,
    /// Both error, different error numbers.
    ErrorCode,
    /// Same rows in a different order. Not a defect by itself (a SELECT without ORDER BY has no defined
    /// order), so it never counts as a hard divergence, but it is not an exact match either.
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
    /// The oracle did not answer within the budget: nothing to compare with. Never counted as a statement
    /// MySQL accepted, never a hard divergence.
    OracleTimeout,
    /// At least one side produced no server answer at all (connection failed or dropped). Always hard: a
    /// crashed or unreachable server must not look like agreement.
    Transport,
}

impl Kind {
    pub fn label(self) -> &'static str {
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
            Kind::OracleTimeout => "oracle-timeout",
            Kind::Transport => "transport",
        }
    }

    /// A divergence that points at a defect in zeta: not an exact match, not a row-order-only difference, and
    /// MySQL itself ran the statement. (A statement MySQL rejects and zeta accepts is leniency; two different
    /// error numbers are an error-code gap, tracked separately.) A transport failure is always hard.
    pub fn is_hard(self, oracle_accepted: bool) -> bool {
        match self {
            Kind::Same | Kind::RowOrder | Kind::OracleTimeout => false,
            Kind::Transport => true,
            _ => oracle_accepted,
        }
    }
}

pub fn compare(zeta: &Outcome, oracle: &Outcome, sorted: bool) -> Kind {
    use Outcome::*;
    match (zeta, oracle) {
        // zeta produced no server answer at all: a crash or a dead connection, even if the oracle timed out too.
        (Err { code: None, .. }, _) => Kind::Transport,
        // The oracle gave no answer: nothing to compare against, whatever zeta did.
        (_, Timeout) => Kind::OracleTimeout,
        (_, Err { code: None, .. }) => Kind::Transport,
        (Timeout, _) => Kind::Timeout,
        (Err { code: zc, .. }, Err { code: oc, .. }) => {
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

fn cell(v: &mysql_async::Value) -> Cell {
    match v {
        mysql_async::Value::NULL => None,
        other => Some(format_value(other)),
    }
}

/// Run one statement and capture its outcome. Every result set of the response is drained here: a
/// multi-result response (CALL, multi-statement text) whose LATER set fails would otherwise leave that error
/// queued on the connection, to be raised by the next statement. The first result set (columns, rows,
/// affected-rows) is what is compared; a later error is this statement's outcome.
pub async fn exec(conn: &mut mysql_async::Conn, sql: &str, budget: Duration) -> Outcome {
    let fut = async {
        let mut res = conn.query_iter(sql).await?;
        let cols: Vec<String> = res
            .columns_ref()
            .iter()
            .map(|c| c.name_str().into_owned())
            .collect();
        let affected = res.affected_rows();
        let rows: Vec<mysql_async::Row> = res.collect().await?;
        res.drop_result().await?;
        Ok::<_, mysql_async::Error>((cols, affected, rows))
    };
    match tokio::time::timeout(budget, fut).await {
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
                            .map(|i| cell(r.as_ref(i).expect("column in range")))
                            .collect()
                    })
                    .collect();
                Outcome::Rows { cols, rows }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(cols: &[&str], data: &[&[Option<&str>]]) -> Outcome {
        Outcome::Rows {
            cols: cols.iter().map(|c| c.to_string()).collect(),
            rows: data
                .iter()
                .map(|r| r.iter().map(|c| c.map(str::to_string)).collect())
                .collect(),
        }
    }
    fn err(code: Option<u32>) -> Outcome {
        Outcome::Err {
            code,
            msg: String::new(),
        }
    }

    #[test]
    fn null_and_the_string_null_differ() {
        let a = rows(&["x"], &[&[None]]);
        let b = rows(&["x"], &[&[Some("NULL")]]);
        assert_eq!(compare(&a, &b, false), Kind::Values);
        assert_eq!(compare(&a, &a, false), Kind::Same);
        assert_eq!(a.brief(), "x\nNULL");
        assert_eq!(b.brief(), "x\n'NULL'");
    }

    #[test]
    fn oracle_timeout_is_not_a_zeta_verdict() {
        let ok = Outcome::Ok { affected: 0 };
        assert_eq!(compare(&ok, &Outcome::Timeout, false), Kind::OracleTimeout);
        assert_eq!(
            compare(&err(Some(1)), &Outcome::Timeout, false),
            Kind::OracleTimeout
        );
        assert_eq!(
            compare(&Outcome::Timeout, &Outcome::Timeout, false),
            Kind::OracleTimeout
        );
        assert_eq!(compare(&Outcome::Timeout, &ok, false), Kind::Timeout);
        assert!(!Kind::OracleTimeout.is_hard(false));
        assert!(!Outcome::Timeout.accepted());
    }

    #[test]
    fn missing_server_answers_never_agree() {
        let ok = Outcome::Ok { affected: 0 };
        assert_eq!(compare(&err(None), &err(None), false), Kind::Transport);
        assert_eq!(compare(&err(None), &ok, false), Kind::Transport);
        assert_eq!(compare(&ok, &err(None), false), Kind::Transport);
        assert!(Kind::Transport.is_hard(false));
    }

    #[test]
    fn a_zeta_timeout_next_to_a_dead_oracle_is_still_transport() {
        assert_eq!(
            compare(&Outcome::Timeout, &err(None), false),
            Kind::Transport
        );
    }

    #[test]
    fn error_numbers_decide_error_agreement() {
        assert_eq!(
            compare(&err(Some(1062)), &err(Some(1062)), false),
            Kind::Same
        );
        assert_eq!(
            compare(&err(Some(1105)), &err(Some(1062)), false),
            Kind::ErrorCode
        );
        let ok = Outcome::Ok { affected: 1 };
        assert_eq!(compare(&err(Some(1105)), &ok, false), Kind::ZetaError);
        assert_eq!(compare(&ok, &err(Some(1062)), false), Kind::ZetaAccepts);
    }

    #[test]
    fn row_order_is_visible_but_not_hard() {
        let a = rows(&["x"], &[&[Some("1")], &[Some("2")]]);
        let b = rows(&["x"], &[&[Some("2")], &[Some("1")]]);
        assert_eq!(compare(&a, &b, false), Kind::RowOrder);
        assert_eq!(compare(&a, &b, true), Kind::Same);
        assert!(!Kind::RowOrder.is_hard(true));
        assert!(Kind::Values.is_hard(true));
        assert!(!Kind::Values.is_hard(false));
    }
}
