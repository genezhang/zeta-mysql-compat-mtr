//! The `.result` text of a test with an oracle record: what MySQL printed, in the runner's own output format.
//!
//! For such a test the file is generated (`--write-results`), never edited by hand, and the normal mode checks that
//! it still equals what the record renders to. The format is the one the normal mode has always used for zeta's
//! output: the statement and its terminating `;`, then, for a statement that returns rows, the column names and
//! one tab-separated line per row (nothing at all for an empty result), then `ERROR <code>: <message>` for an
//! error; `--echo` lines verbatim. What has no MySQL output to show gets a bracketed note: `<skipped: ...>` for a
//! statement that is never run, `<excluded: ...>` for one whose answer is unstable, `<oracle timeout>`.

use std::fmt::Write as _;

use anyhow::{bail, Result};

use crate::mtr_parser::Directive;
use crate::record::{Body, Rec};
use crate::runner::{returns_rows, sort_block};

pub fn render(directives: &[Directive], recs: &[Rec]) -> Result<String> {
    let mut out = String::new();
    let mut k = 0usize;
    let mut sorted = false;
    for d in directives {
        match d {
            Directive::Echo(text) => {
                writeln!(out, "{text}")?;
            }
            Directive::SortedResult => sorted = true,
            Directive::Sql(sql) => {
                let Some(rec) = recs.get(k) else {
                    bail!("the record has fewer statements than the test");
                };
                k += 1;
                writeln!(out, "{sql};")?;
                match &rec.body {
                    Body::Rows { cols, rows } if returns_rows(sql) && !rows.is_empty() => {
                        let mut block = String::new();
                        writeln!(block, "{}", cols.join("\t"))?;
                        for r in rows {
                            let cells: Vec<&str> =
                                r.iter().map(|c| c.as_deref().unwrap_or("NULL")).collect();
                            writeln!(block, "{}", cells.join("\t"))?;
                        }
                        out.push_str(&if sorted { sort_block(&block) } else { block });
                    }
                    Body::Rows { .. } | Body::Ok { .. } => {}
                    Body::Err { code, msg } => match code {
                        Some(c) => {
                            writeln!(out, "ERROR {c}: {msg}")?;
                        }
                        None => {
                            writeln!(out, "ERROR (no server answer): {msg}")?;
                        }
                    },
                    Body::Timeout => {
                        writeln!(out, "<oracle timeout>")?;
                    }
                    Body::Skip { why } => {
                        writeln!(out, "<skipped: {why}>")?;
                    }
                    Body::Excluded { why } => {
                        writeln!(out, "<excluded: {why}>")?;
                    }
                }
                sorted = false;
            }
            _ => {}
        }
    }
    if k != recs.len() {
        bail!("the record has more statements than the test");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outcome::Outcome;

    fn rows(cols: &[&str], data: &[&[Option<&str>]]) -> Outcome {
        Outcome::Rows {
            cols: cols.iter().map(|c| c.to_string()).collect(),
            rows: data
                .iter()
                .map(|r| r.iter().map(|c| c.map(str::to_string)).collect())
                .collect(),
        }
    }

    #[test]
    fn renders_the_runners_format() {
        let d = vec![
            Directive::Echo("=== a ===".into()),
            Directive::Sql("SELECT 1, NULL".into()),
            Directive::SortedResult,
            Directive::Sql("SELECT x FROM t".into()),
            Directive::Sql("INSERT INTO t VALUES (1)".into()),
            Directive::Sql("SELECT * FROM e".into()),
            Directive::Sql("SELECT nope".into()),
            Directive::Sql("SHUTDOWN".into()),
        ];
        let recs = vec![
            Rec::from_outcome(
                1,
                "SELECT 1, NULL",
                &rows(&["1", "NULL"], &[&[Some("1"), None]]),
            ),
            Rec::from_outcome(
                2,
                "SELECT x FROM t",
                &rows(&["x"], &[&[Some("b")], &[Some("a")]]),
            ),
            Rec::from_outcome(3, "i", &Outcome::Ok { affected: 1 }),
            Rec::from_outcome(4, "e", &rows(&["x"], &[])),
            Rec::from_outcome(
                5,
                "n",
                &Outcome::Err {
                    code: Some(1054),
                    msg: "Unknown column".into(),
                },
            ),
            Rec::skipped(6, "SHUTDOWN", "SHUTDOWN / RESTART"),
        ];
        let text = render(&d, &recs).unwrap();
        assert_eq!(
            text,
            "=== a ===\nSELECT 1, NULL;\n1\tNULL\n1\tNULL\nSELECT x FROM t;\nx\na\nb\nINSERT INTO t VALUES (1);\nSELECT * FROM e;\nSELECT nope;\nERROR 1054: Unknown column\nSHUTDOWN;\n<skipped: SHUTDOWN / RESTART>\n"
        );
        assert!(render(&d, &recs[..5]).is_err());
    }
}
