#![allow(dead_code)] // skeleton — fleshed out as suites land

//! Compare captured MTR test output against a `.result` file, honoring
//! the runner-state directives (`--sorted_result`, `--replace_regex`).
//!
//! Independently implemented; not derived from `mysql-test-run.pl`.

use anyhow::Result;

pub struct DiffOptions {
    pub sorted: bool,
    pub regex_replacements: Vec<(regex::Regex, String)>,
}

pub fn compare(_actual: &str, _expected: &str, _opts: &DiffOptions) -> Result<()> {
    // TODO: apply regex replacements, optionally sort lines within the
    // current result block, then unified-diff and bail on mismatch.
    anyhow::bail!("result_diff::compare: not yet implemented")
}
