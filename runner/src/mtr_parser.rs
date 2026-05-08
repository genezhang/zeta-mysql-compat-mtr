#![allow(dead_code)] // skeleton — fleshed out as suites land

//! Parser for the MTR (MySQL Test Run) DSL.
//!
//! Independently implemented from public MTR documentation. Does NOT copy
//! from MySQL's `mysql-test-run.pl`.
//!
//! Recognized directives (subset, expanded over time):
//!   --source <file>          inline another .test file
//!   --let $var = <value>     assign a runner variable
//!   --error <code>           expect the next statement to fail with code
//!   --sorted_result          sort result rows before comparison
//!   --replace_regex /a/b/    apply regex substitution to result text
//!   --disable_query_log      suppress logging of queries
//!   --enable_query_log       restore logging
//!   --disable_warnings       drop warning rows from result
//!   --enable_warnings        keep warning rows
//!
//! Bare lines are SQL statements (terminated by `;` or by start-of-line `;`).

use anyhow::Result;

#[derive(Debug, Clone)]
pub enum Directive {
    Sql(String),
    Source(String),
    Let {
        name: String,
        value: String,
    },
    Error(String),
    SortedResult,
    ReplaceRegex {
        pattern: String,
        replacement: String,
    },
    DisableQueryLog,
    EnableQueryLog,
    DisableWarnings,
    EnableWarnings,
}

pub fn parse(_input: &str) -> Result<Vec<Directive>> {
    // TODO: line-by-line tokenizer for MTR DSL.
    anyhow::bail!("mtr_parser::parse: not yet implemented")
}
