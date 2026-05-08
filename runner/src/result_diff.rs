//! Compare captured MTR test output against a `.result` file.
//!
//! Independently implemented; not derived from `mysql-test-run.pl`.
//!
//! M0 strategy: line-level equality. The runner is responsible for
//! applying any `--sorted_result` etc. transforms inside the per-block
//! capture before this compares whole-file actual vs expected.

use anyhow::{bail, Result};

#[allow(dead_code)] // sort/regex application happens in runner.rs for now
pub struct DiffOptions {
    pub sorted: bool,
    pub regex_replacements: Vec<(regex::Regex, String)>,
}

pub fn compare(actual: &str, expected: &str) -> Result<()> {
    if actual == expected {
        return Ok(());
    }

    let actual_lines: Vec<&str> = actual.lines().collect();
    let expected_lines: Vec<&str> = expected.lines().collect();

    // Smallest diff that locates the user's eye on the first divergence.
    let max = actual_lines.len().max(expected_lines.len());
    let mut report = String::from("result mismatch\n");
    for i in 0..max {
        let a = actual_lines.get(i).copied();
        let e = expected_lines.get(i).copied();
        if a == e {
            continue;
        }
        match (a, e) {
            (Some(a), Some(e)) => {
                report.push_str(&format!("line {}: expected {e:?}, got {a:?}\n", i + 1));
            }
            (None, Some(e)) => {
                report.push_str(&format!(
                    "line {}: expected {e:?}, but actual ended\n",
                    i + 1
                ));
            }
            (Some(a), None) => {
                report.push_str(&format!(
                    "line {}: extra actual line {a:?} (expected ended)\n",
                    i + 1
                ));
            }
            (None, None) => unreachable!(),
        }
        if report.lines().count() > 12 {
            report.push_str("(more lines truncated)\n");
            break;
        }
    }
    bail!(report);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_strings_compare_ok() {
        assert!(compare("a\nb\n", "a\nb\n").is_ok());
    }

    #[test]
    fn divergent_strings_report_first_line() {
        let err = compare("a\nx\nc\n", "a\nb\nc\n").unwrap_err().to_string();
        assert!(
            err.contains("line 2"),
            "report should point at line 2, got: {err}"
        );
    }

    #[test]
    fn shorter_actual_is_caught() {
        let err = compare("a\n", "a\nb\n").unwrap_err().to_string();
        assert!(err.contains("actual ended"), "got: {err}");
    }
}
