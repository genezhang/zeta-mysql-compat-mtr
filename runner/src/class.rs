//! Which tests target MySQL and which are "zeta-only dialect leakage".
//!
//! About a quarter of the corpus (names starting `pg_`, and the `*sentinel*` tests) was written to pin what
//! zeta does with PostgreSQL grammar and unsupported-feature probes sent over the MySQL wire. MySQL rejects most of
//! that by design, so "zeta agrees with MySQL" is not the question for those files: they stay pinned to zeta's own
//! behaviour, and their agreement numbers are reported on a separate line so they do not pollute the MySQL one.
//!
//! The list is `tests/zeta_only.list`: one `path<TAB>reason` per line, `#` comments. A path not listed is
//! MySQL-targeted. Nothing is moved: paths stay stable for everything that refers to them.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};

pub const MYSQL: &str = "mysql";
pub const ZETA_ONLY: &str = "zeta-only";

#[derive(Debug, Default)]
pub struct Classes {
    zeta_only: HashMap<String, String>,
    /// `tests/zeta_only.list` exists: the repository has adopted oracle records, so a MySQL-targeted test must have
    /// one.
    adopted: bool,
}

impl Classes {
    /// Load `<root>/tests/zeta_only.list`; a missing file means every test targets MySQL.
    pub fn load(root: &Path) -> Result<Self> {
        let path = root.join("tests").join("zeta_only.list");
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        Ok(Classes {
            adopted: true,
            ..Self::parse(&text)
        })
    }

    fn parse(text: &str) -> Self {
        let mut zeta_only = HashMap::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (p, why) = line.split_once('\t').unwrap_or((line, ""));
            zeta_only.insert(p.trim().to_string(), why.trim().to_string());
        }
        Classes {
            zeta_only,
            adopted: false,
        }
    }

    /// `"zeta-only"` or `"mysql"`.
    pub fn of(&self, test: &Path) -> &'static str {
        if self.zeta_only.contains_key(test.to_string_lossy().as_ref()) {
            ZETA_ONLY
        } else {
            MYSQL
        }
    }
}

fn classes() -> &'static Classes {
    use std::sync::OnceLock;
    static CLASSES: OnceLock<Classes> = OnceLock::new();
    CLASSES.get_or_init(|| Classes::load(Path::new(".")).unwrap_or_default())
}

/// The class of `test`, from `./tests/zeta_only.list` (read once). A malformed line cannot occur: the format has
/// no syntax to get wrong, and a path that matches no test is simply never looked up.
pub fn of(test: &Path) -> &'static str {
    classes().of(test)
}

/// Whether this repository keeps oracle records (see [`Classes::adopted`]).
pub fn records_adopted() -> bool {
    classes().adopted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listed_paths_are_zeta_only_everything_else_targets_mysql() {
        let c = Classes::parse(
            "# comment\n\ntests/main/pg_x.test\tpg-grammar\ntests/main/y_sentinel.test\tsentinel\n",
        );
        assert_eq!(c.of(Path::new("tests/main/pg_x.test")), ZETA_ONLY);
        assert_eq!(c.of(Path::new("tests/main/y_sentinel.test")), ZETA_ONLY);
        assert_eq!(c.of(Path::new("tests/main/select.test")), MYSQL);
    }
}
