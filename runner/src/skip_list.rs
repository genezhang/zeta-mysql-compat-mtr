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

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};

/// Parsed skip-list entries (internal).
#[derive(Debug, serde::Deserialize)]
struct SkipListRaw {
    entries: Vec<SkipEntry>,
}

/// A single skip-list entry: why a test is skipped and where it's tracked.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct SkipEntry {
    pub path: String,
    pub reason: String,
    pub ticket: String,
    pub note: Option<String>,
}

/// A set of test paths that should be skipped, keyed by path for lookup.
#[derive(Debug, Clone, Default)]
pub struct SkipList {
    entries: HashMap<String, SkipEntry>,
}

impl SkipList {
    /// Load the skip list from `tests/skip_list.toml`.
    ///
    /// Returns an empty skip list if the file doesn't exist (e.g. first
    /// run before `skip_list.toml` is populated). A malformed file is a
    /// hard error — a silently-empty skip list would just report every
    /// previously-skipped test as a fresh failure.
    pub fn load(root: &Path) -> Result<Self> {
        let skip_path = root.join("tests").join("skip_list.toml");
        if !skip_path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(&skip_path)
            .with_context(|| format!("reading {}", skip_path.display()))?;
        let parsed: SkipListRaw =
            toml::from_str(&content).with_context(|| format!("parsing {}", skip_path.display()))?;
        let entries = parsed
            .entries
            .into_iter()
            .map(|e| (e.path.clone(), e))
            .collect();
        Ok(Self { entries })
    }

    /// Returns the skip entry for the given test path, if any.
    ///
    /// The `test_path` is the path to the `.test` file (e.g.
    /// `tests/main/foo.test`). It is matched against the `path` field in
    /// skip_list.toml, which uses the same format.
    pub fn entry(&self, test_path: &Path) -> Option<&SkipEntry> {
        self.entries.get(&test_path.to_string_lossy().to_string())
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
        assert!(skip.entries.is_empty());
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
# Known-failing tests.
entries = [
    # A real bug.
    {{ path = "tests/main/foo.test", reason = "bug", ticket = "https://github.com/genezhang/zeta/issues/1", note = "test note" }},

    {{ path = "tests/main/bar.test", reason = "not-yet-implemented", ticket = "https://github.com/genezhang/zeta/issues/2", note = "another test" }},
]
"#
        )
        .unwrap();
        drop(f);

        let skip = SkipList::load(dir.path()).unwrap();
        assert_eq!(skip.entries.len(), 2);
        // entry() takes a Path and converts it to a string for comparison.
        // The skip_list has relative paths, so we need to pass relative paths too.
        assert!(skip.entry(&Path::new("tests/main/foo.test")).is_some());
        assert!(skip.entry(&Path::new("tests/main/bar.test")).is_some());
        assert!(skip.entry(&Path::new("tests/main/baz.test")).is_none());
        assert_eq!(
            skip.entry(&Path::new("tests/main/foo.test"))
                .unwrap()
                .reason,
            "bug"
        );
    }

    #[test]
    fn load_malformed_skip_list_is_an_error() {
        let dir = TempDir::new().unwrap();
        let tests_dir = dir.path().join("tests");
        std::fs::create_dir_all(&tests_dir).unwrap();
        std::fs::write(tests_dir.join("skip_list.toml"), "not valid toml [[[").unwrap();
        assert!(SkipList::load(dir.path()).is_err());
    }
}
