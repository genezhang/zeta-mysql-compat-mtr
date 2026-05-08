//! Parser for the MTR (MySQL Test Run) DSL.
//!
//! Independently implemented from public MTR documentation. Does NOT copy
//! from MySQL's `mysql-test-run.pl`.
//!
//! M0 subset: comments (#), blank lines, multi-line SQL terminated by `;`,
//! `--sorted_result`, `--error <code>`, `--echo <text>`. Other directives
//! parse as `Unknown` and are ignored at execution time so a partial
//! suite still moves forward.

use anyhow::{anyhow, Result};

#[derive(Debug, Clone)]
pub enum Directive {
    /// A SQL statement (semicolon stripped) to send over the wire.
    Sql(String),
    /// Echo the given literal text into the result stream.
    Echo(String),
    /// The next `Sql` directive is expected to fail with one of these
    /// MySQL error codes. Empty when no `--error` was active.
    ExpectError(Vec<u32>),
    /// Sort the rows of the next `Sql` directive's output before diffing.
    SortedResult,
    /// A directive whose name we didn't bother implementing yet. Carries
    /// the raw line for diagnostics.
    Unknown(String),
}

pub fn parse(input: &str) -> Result<Vec<Directive>> {
    let mut out = Vec::new();
    let mut sql_buf = String::new();

    for (lineno, raw) in input.lines().enumerate() {
        let lineno = lineno + 1;
        let line = raw.trim_end();

        // Inside a multi-line SQL statement, even comment-shaped lines
        // are part of the SQL — but we don't expect to encounter that
        // in practice. For M0 we treat `#` and blank lines as separators
        // only when the SQL buffer is empty.
        if sql_buf.is_empty() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(rest) = line.strip_prefix("--") {
                out.push(parse_directive(rest, lineno)?);
                continue;
            }
        }

        // Non-directive line — accumulate as SQL until we hit a terminating `;`.
        if !sql_buf.is_empty() {
            sql_buf.push('\n');
        }
        sql_buf.push_str(line);

        if line.trim_end().ends_with(';') {
            // Strip the trailing semicolon. SQL servers don't want it, and
            // result-format echoing re-attaches one to keep output stable.
            let stmt = sql_buf
                .trim_end_matches([';', ' ', '\t', '\r', '\n'])
                .to_string();
            sql_buf.clear();
            if !stmt.is_empty() {
                out.push(Directive::Sql(stmt));
            }
        }
    }

    if !sql_buf.trim().is_empty() {
        return Err(anyhow!(
            "unterminated SQL statement at end of file: {sql_buf:?}"
        ));
    }
    Ok(out)
}

fn parse_directive(rest: &str, lineno: usize) -> Result<Directive> {
    // Directive line shape after the leading `--`:
    //   `<name>` or `<name> <args...>`
    let trimmed = rest.trim_start();
    let (name, args) = match trimmed.find(char::is_whitespace) {
        Some(idx) => (&trimmed[..idx], trimmed[idx + 1..].trim()),
        None => (trimmed.trim(), ""),
    };

    match name {
        "sorted_result" => Ok(Directive::SortedResult),
        "echo" => Ok(Directive::Echo(args.to_string())),
        "error" => {
            let codes: Result<Vec<u32>, _> = args
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::parse::<u32>)
                .collect();
            let codes = codes
                .map_err(|e| anyhow!("line {lineno}: --error: bad code list `{args}`: {e}"))?;
            if codes.is_empty() {
                return Err(anyhow!("line {lineno}: --error needs at least one code"));
            }
            Ok(Directive::ExpectError(codes))
        }
        // Real MTR has dozens more directives. For M0 we record but ignore.
        _ => Ok(Directive::Unknown(format!("--{trimmed}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_sql_and_directives() {
        let src = "\
# leading comment
SELECT 1;

--sorted_result
SELECT 2;

--error 1062
INSERT INTO t VALUES (1);
";
        let dirs = parse(src).unwrap();
        assert_eq!(dirs.len(), 5);
        assert!(matches!(&dirs[0], Directive::Sql(s) if s == "SELECT 1"));
        assert!(matches!(dirs[1], Directive::SortedResult));
        assert!(matches!(&dirs[2], Directive::Sql(s) if s == "SELECT 2"));
        assert!(matches!(&dirs[3], Directive::ExpectError(c) if c == &[1062]));
        assert!(matches!(&dirs[4], Directive::Sql(s) if s == "INSERT INTO t VALUES (1)"));
    }

    #[test]
    fn handles_multi_line_sql() {
        let src = "\
SELECT
  1,
  2;
";
        let dirs = parse(src).unwrap();
        assert_eq!(dirs.len(), 1);
        let Directive::Sql(s) = &dirs[0] else {
            panic!("expected Sql, got {:?}", dirs[0])
        };
        assert!(s.contains("SELECT"));
        assert!(s.contains("1,"));
        assert!(s.contains("2"));
    }

    #[test]
    fn rejects_unterminated_sql() {
        let src = "SELECT 1\nSELECT 2";
        assert!(parse(src).is_err());
    }
}
