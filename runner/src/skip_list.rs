//! Skip-list support for the MTR runner.
//!
//! Parses `tests/skip_list.toml` and provides a fast lookup to decide
//! whether a test should be skipped.
//!
//! Schema (see `tests/skip_list.toml`):
//! ```toml
//! entries = [
//!     { path = "tests/main/foo.test", reason = "...", ticket = "...", note = "..." },
//!     ...
//! ]
//! ```

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};

/// Parsed skip-list entries (internal).
#[derive(Debug, serde::Deserialize)]
struct SkipListRaw {
    entries: Vec<SkipEntryRaw>,
}

#[derive(Debug, serde::Deserialize)]
struct SkipEntryRaw {
    path: String,
    #[allow(dead_code)]
    reason: String,
    #[allow(dead_code)]
    ticket: String,
    #[allow(dead_code)]
    note: Option<String>,
}

/// A set of test paths that should be skipped.
#[derive(Debug, Clone, Default)]
pub struct SkipList {
    paths: HashSet<String>,
}

/// Remove TOML comments and blank lines from a string.
///
/// Only removes comments that are on their own line (after trimming
/// whitespace). Comments in the middle of a line (e.g. inside strings)
/// are preserved.
fn preprocess_toml(input: &str) -> String {
    let mut result = Vec::with_capacity(input.len());
    for line in input.lines() {
        // Only remove lines that are comments (start with # after trimming).
        if line.trim_start().starts_with('#') {
            continue;
        }
        // Skip blank lines.
        if line.trim().is_empty() {
            continue;
        }
        result.push(line.to_string());
    }
    result.join("\n")
}

impl SkipList {
    /// Load the skip list from `tests/skip_list.toml`.
    ///
    /// Returns an empty skip list if the file doesn't exist (e.g. first
    /// run before `skip_list.toml` is populated).
    pub fn load(root: &Path) -> Result<Self> {
        let skip_path = root.join("tests").join("skip_list.toml");
        if !skip_path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(&skip_path)
            .with_context(|| format!("reading {}", skip_path.display()))?;
        // Remove comments and blank lines before parsing (TOML array
        // comments and blank lines aren't supported by the `toml` crate).
        let cleaned = preprocess_toml(&content);
        let parsed: SkipListRaw = toml::from_str(&cleaned)
            .with_context(|| format!("parsing {}", skip_path.display()))?;
        let paths: HashSet<String> = parsed
            .entries
            .into_iter()
            .map(|e| e.path)
            .collect();
        Ok(Self { paths })
    }

    /// Returns true if the given test path should be skipped.
    ///
    /// The `test_path` is the path to the `.test` file (e.g.
    /// `tests/main/foo.test`). It is matched against the `path` field in
    /// skip_list.toml, which uses the same format.
    pub fn should_skip(&self, test_path: &Path) -> bool {
        let path_str = test_path.to_string_lossy().to_string();
        self.paths.contains(&path_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn load_empty_skip_list() {
        let dir = TempDir::new().unwrap();
        let skip = SkipList::load(dir.path()).unwrap();
        assert!(skip.paths.is_empty());
    }

    #[test]
    fn load_skip_list_with_entries() {
        let dir = TempDir::new().unwrap();
        let tests_dir = dir.path().join("tests");
        std::fs::create_dir_all(&tests_dir).unwrap();
        let mut f = std::fs::File::create(tests_dir.join("skip_list.toml")).unwrap();
        writeln!(
            f,
            r#"
entries = [
    {{ path = "tests/main/foo.test", reason = "bug", ticket = "https://github.com/genezhang/zeta/issues/1", note = "test note" }},
    {{ path = "tests/main/bar.test", reason = "not-yet-implemented", ticket = "https://github.com/genezhang/zeta/issues/2", note = "another test" }},
]
"#
        )
        .unwrap();
        drop(f);

        let skip = SkipList::load(dir.path()).unwrap();
        assert_eq!(skip.paths.len(), 2);
        // should_skip takes a Path and converts it to a string for comparison.
        // The skip_list has relative paths, so we need to pass relative paths too.
        assert!(skip.should_skip(&Path::new("tests/main/foo.test")));
        assert!(skip.should_skip(&Path::new("tests/main/bar.test")));
        assert!(!skip.should_skip(&Path::new("tests/main/baz.test")));
    }

    #[test]
    fn preprocess_toml_removes_comments_and_blanks() {
        let input = r#"
# Comment
entries = [
    # Another comment
    { path = "foo.test", reason = "bug", ticket = "t1", note = "n" },

    { path = "bar.test", reason = "bug", ticket = "t2", note = "n2" },
]
"#;
        let output = preprocess_toml(input);
        assert!(!output.contains('#'));
        assert!(!output.contains("\n\n"));
        assert!(output.contains("entries = ["));
        assert!(output.contains("foo.test"));
        assert!(output.contains("bar.test"));
    }
}
