//! Statements the differential mode refuses to run on EITHER server.
//!
//! The oracle is a real `mysqld` that the run shares with nothing else, but it is still a long-lived
//! process: a statement that stops it (`SHUTDOWN`), changes a global (`SET GLOBAL`, `SET PERSIST`), writes a
//! file on its host (`INTO OUTFILE`), edits its accounts or system schemas, or kills sessions either ends the
//! run or leaks state into every later file, and does so on the next sweep too. Running such a statement on
//! zeta alone would make the two sides diverge for a reason that has nothing to do with compatibility, so it
//! is skipped on both, counted, and listed by class on stderr.
//!
//! The check is deliberately a conservative text match: comments and string literals are blanked first, a
//! statement text that holds several statements (`a; b`) is checked piece by piece, the text of a
//! `PREPARE ... FROM '...'` is checked as a statement, and the body of a `CREATE PROCEDURE/FUNCTION/TRIGGER/EVENT`
//! is scanned for the same hazards. A false positive costs one skipped statement that is still counted; a false
//! negative can cost the oracle. What it cannot see: a statement assembled at run time (`SET @s = '...'; PREPARE
//! x FROM @s`), and `NO_BACKSLASH_ESCAPES` changing what a backslash in a literal means.

use regex::Regex;

/// Why a statement was not run. The label is printed in the end-of-run summary.
pub type Hazard = &'static str;

pub struct HazardFilter {
    rules: Vec<(Hazard, Regex)>,
    set_item_global: Regex,
    program: Regex,
    program_body: Regex,
    prepare_from: Regex,
    system_use: Regex,
    any_use: Regex,
    write: Regex,
    /// True after `USE <system schema>`: writes are then hazardous even without a schema qualifier.
    in_system_db: bool,
}

/// Schemas whose contents a write must not touch. `information_schema` is read-only, so it is not one of them.
const SYSTEM_SCHEMAS: &str = "MYSQL|SYS|PERFORMANCE_SCHEMA";
const WRITE_VERBS: &str =
    "INSERT|UPDATE|DELETE|REPLACE|TRUNCATE|CREATE|ALTER|DROP|RENAME|LOAD|ANALYZE|OPTIMIZE|REPAIR";

impl HazardFilter {
    pub fn new() -> Self {
        let rx = |s: &str| Regex::new(s).expect("static hazard regex compiles");
        let rule = |label: Hazard, pat: &str| (label, rx(pat));
        let rules = vec![
            rule("SHUTDOWN / RESTART", r"^(SHUTDOWN|RESTART)\b"),
            rule(
                "replication, binary log, clone",
                r"^(RESET\s+(MASTER|REPLICA|SLAVE|BINARY|PERSIST)|PURGE\b|CHANGE\s+(MASTER|REPLICATION)|START\s+(REPLICA|SLAVE|GROUP_REPLICATION)|STOP\s+(REPLICA|SLAVE|GROUP_REPLICATION)|BINLOG\b|CLONE\b|IMPORT\s+TABLE\b)",
            ),
            rule("INTO OUTFILE / DUMPFILE", r"\bINTO\s+(OUTFILE|DUMPFILE)\b"),
            rule(
                "accounts and privileges",
                r"^((CREATE|ALTER|DROP|RENAME)\s+(USER|ROLE)\b|GRANT\b|REVOKE\b|SET\s+PASSWORD\b|SET\s+DEFAULT\s+ROLE\b)",
            ),
            rule("KILL", r"^KILL\b"),
            rule(
                "plugins, components, instance",
                r"^((INSTALL|UNINSTALL)\s+(PLUGIN|COMPONENT)\b|ALTER\s+INSTANCE\b|LOCK\s+INSTANCE\b|CREATE\s+(AGGREGATE\s+)?FUNCTION\b.*\bSONAME\b)",
            ),
            // A prepared XA transaction outlives its session and holds metadata locks: the next DROP DATABASE hangs.
            rule("XA PREPARE", r"^XA\s+PREPARE\b"),
            rule(
                "server-level objects",
                r"^(CREATE|ALTER|DROP)\s+(OR\s+REPLACE\s+)?(SERVER|(UNDO\s+)?TABLESPACE|LOGFILE\s+GROUP|RESOURCE\s+GROUP|SPATIAL\s+REFERENCE\s+SYSTEM)\b",
            ),
            rule(
                "writes to a system schema",
                &format!(
                    // DROP/ALTER/CREATE DATABASE <system schema>, CALL <system schema>.proc, and any write naming
                    // `<system schema>.<object>` (also behind a WITH clause).
                    r"^((CREATE|ALTER|DROP)\s+(DATABASE|SCHEMA)\s+(IF\s+(NOT\s+)?EXISTS\s+)?`?({s})`?\b|CALL\s+`?({s})`?\s*\.|(WITH\b.*\b)?({w})\b.*[^A-Z0-9_$`]`?({s})`?\s*\.\s*[`A-Z_])",
                    s = SYSTEM_SCHEMAS,
                    w = WRITE_VERBS
                ),
            ),
        ];
        HazardFilter {
            rules,
            // One comma-separated item of a SET list that assigns a global: `GLOBAL x = ..`, `PERSIST x = ..`,
            // `@@global.x = ..`. Reading one (`@v = @@global.x`) or a user variable called `@global` is not.
            set_item_global: rx(
                r"^\s*((GLOBAL|PERSIST|PERSIST_ONLY)\b|@@\s*(GLOBAL|PERSIST|PERSIST_ONLY)\s*\.\s*\w+\s*:?=)",
            ),
            program: rx(
                r"^(CREATE|ALTER)\s+(OR\s+REPLACE\s+)?(DEFINER\s*=\s*\S+(\s*@\s*\S+)?(\s*\(\s*\))?\s+)?(PROCEDURE|FUNCTION|TRIGGER|EVENT)\b",
            ),
            // The hazards that matter if a program or event runs: unanchored, since they sit inside the body.
            program_body: rx(&format!(
                r"\bSET\s+(GLOBAL|PERSIST|PERSIST_ONLY)\b|@@\s*(GLOBAL|PERSIST|PERSIST_ONLY)\s*\.\s*\w+\s*:?=|\bINTO\s+(OUTFILE|DUMPFILE)\b|\b(CREATE|ALTER|DROP)\s+(USER|ROLE)\b|\b(GRANT|REVOKE|SHUTDOWN|KILL)\b|\b({w})\b[^;]*[^A-Z0-9_$`]`?({s})`?\s*\.\s*[`A-Z_]",
                w = WRITE_VERBS,
                s = SYSTEM_SCHEMAS
            )),
            // On the normalized text, where a string literal is just `''`.
            prepare_from: rx(r"\bPREPARE\s+(\w+|`[^`]*`)\s+FROM\s+''"),
            system_use: rx(&format!(r"^USE\s+`?({SYSTEM_SCHEMAS})`?\s*$")),
            any_use: rx(r"^USE\b"),
            write: rx(&format!(r"^({WRITE_VERBS})\b")),
            in_system_db: false,
        }
    }

    /// `Some(label)` if the statement must not run. Call once per statement, in file order (it tracks
    /// `USE`). A new file starts a new session: call [`Self::new_file`] first.
    pub fn check(&mut self, sql: &str) -> Option<Hazard> {
        let saved = self.in_system_db;
        let hit = self.check_text(sql, true);
        if hit.is_some() {
            // A skipped statement does not run, so a `USE` inside it changes nothing.
            self.in_system_db = saved;
        }
        hit
    }

    fn check_text(&mut self, sql: &str, allow_prepare: bool) -> Option<Hazard> {
        let (norm, literals) = scan(sql);
        let mut hit = None;
        // A program body or event is judged as a whole: its hazards sit in later `;`-separated pieces.
        if self.program.is_match(&norm) && self.program_body.is_match(&norm) {
            hit = Some("hazard inside a stored program or event");
        }
        for piece in norm.split(';') {
            let piece = piece.trim();
            if !piece.is_empty() {
                // Every piece is looked at, so that a `USE` after a harmless statement is still tracked.
                let h = self.check_piece(piece);
                hit = hit.or(h);
            }
        }
        // `PREPARE s FROM '<text>'`, wherever it sits: the text is a statement of its own.
        if hit.is_none() && allow_prepare {
            let ends: Vec<usize> = self
                .prepare_from
                .find_iter(&norm)
                .map(|m| m.end())
                .collect();
            for end in ends {
                // The literal this match ends with is the (count of `''` up to its end)-th one.
                let n = norm[..end].matches("''").count();
                if let Some(inner) = n.checked_sub(1).and_then(|i| literals.get(i)) {
                    if let Some(h) = self.check_text(inner, false) {
                        return Some(h);
                    }
                }
            }
        }
        hit
    }

    fn check_piece(&mut self, s: &str) -> Option<Hazard> {
        if self.any_use.is_match(s) {
            self.in_system_db = self.system_use.is_match(s);
            return None;
        }
        if let Some((l, _)) = self.rules.iter().find(|(_, r)| r.is_match(s)) {
            return Some(l);
        }
        if let Some(rest) = s.strip_prefix("SET ") {
            if rest
                .split(',')
                .any(|item| self.set_item_global.is_match(item))
            {
                return Some("global or persisted server variable");
            }
        }
        if self.in_system_db && self.write.is_match(s) {
            return Some("writes to a system schema");
        }
        None
    }

    pub fn new_file(&mut self) {
        self.in_system_db = false;
    }
}

/// Upper-case, comments and string literals blanked, whitespace collapsed. `/*! ... */` conditional
/// comments are executed by MySQL, so their content is kept and only the delimiters dropped. Backtick-quoted
/// identifiers are kept (their content may name a system schema) but never interpreted, so a quote or `#` inside
/// one does not start a literal or a comment.
pub fn normalize(sql: &str) -> String {
    scan(sql).0
}

/// [`normalize`], plus the raw content of each string literal in order (each is `''` in the normalized text).
fn scan(sql: &str) -> (String, Vec<String>) {
    let b = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut lits: Vec<String> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let next = b.get(i + 1).copied();
        match c {
            b'`' => {
                out.push('`');
                i += 1;
                while i < b.len() {
                    if b[i] == b'`' {
                        if b.get(i + 1) == Some(&b'`') {
                            out.push_str("``");
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    out.push(b[i].to_ascii_uppercase() as char);
                    i += 1;
                }
                out.push('`');
                i += 1;
            }
            b'\'' | b'"' => {
                // Skip the literal, honouring doubled quotes and backslash escapes.
                let q = c;
                i += 1;
                let start = i;
                let mut end = b.len();
                while i < b.len() {
                    if b[i] == b'\\' {
                        i += 2;
                    } else if b[i] == q {
                        if b.get(i + 1) == Some(&q) {
                            i += 2;
                        } else {
                            end = i;
                            i += 1;
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
                let end = end.max(start).min(b.len());
                lits.push(String::from_utf8_lossy(&b[start..end]).into_owned());
                out.push_str("''");
            }
            b'/' if next == Some(b'*') => {
                if b.get(i + 2) == Some(&b'!') {
                    // Conditional comment: drop `/*!` and an optional version number.
                    i += 3;
                    while i < b.len() && b[i].is_ascii_digit() {
                        i += 1;
                    }
                    out.push(' ');
                } else {
                    i += 2;
                    while i < b.len() && !(b[i] == b'*' && b.get(i + 1) == Some(&b'/')) {
                        i += 1;
                    }
                    i += 2;
                    out.push(' ');
                }
            }
            b'*' if next == Some(b'/') => {
                // Closing delimiter of a conditional comment.
                i += 2;
                out.push(' ');
            }
            b'#' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'-' if next == Some(b'-') && b.get(i + 2).is_none_or(|x| x.is_ascii_whitespace()) => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            _ => {
                out.push(c.to_ascii_uppercase() as char);
                i += 1;
            }
        }
    }
    (out.split_whitespace().collect::<Vec<_>>().join(" "), lits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(sql: &str) -> Option<Hazard> {
        HazardFilter::new().check(sql)
    }

    #[test]
    fn the_listed_hazards_are_caught() {
        for (sql, label) in [
            ("SHUTDOWN", "SHUTDOWN / RESTART"),
            ("  shutdown ", "SHUTDOWN / RESTART"),
            ("RESET MASTER", "replication, binary log, clone"),
            ("PURGE BINARY LOGS TO 'x'", "replication, binary log, clone"),
            (
                "SET GLOBAL max_connections = 5",
                "global or persisted server variable",
            ),
            (
                "SET @@global.sql_mode = ''",
                "global or persisted server variable",
            ),
            ("set persist x = 1", "global or persisted server variable"),
            (
                "SET @a = 1, GLOBAL y = 2",
                "global or persisted server variable",
            ),
            (
                "SET GLOBAL  TRANSACTION ISOLATION LEVEL READ COMMITTED",
                "global or persisted server variable",
            ),
            ("SELECT 1 INTO OUTFILE '/tmp/x'", "INTO OUTFILE / DUMPFILE"),
            (
                "SELECT 1 /* c */ into dumpfile '/tmp/x'",
                "INTO OUTFILE / DUMPFILE",
            ),
            ("CREATE USER u", "accounts and privileges"),
            ("DROP ROLE r", "accounts and privileges"),
            ("GRANT ALL ON *.* TO u", "accounts and privileges"),
            ("SET PASSWORD = 'x'", "accounts and privileges"),
            ("KILL QUERY 3", "KILL"),
            (
                "INSTALL PLUGIN p SONAME 'x'",
                "plugins, components, instance",
            ),
            (
                "CREATE FUNCTION f RETURNS STRING SONAME 'x.so'",
                "plugins, components, instance",
            ),
            ("XA PREPARE 'x'", "XA PREPARE"),
            (
                "CREATE TABLESPACE ts ADD DATAFILE 'a'",
                "server-level objects",
            ),
            ("DROP DATABASE mysql", "writes to a system schema"),
            ("DROP SCHEMA IF EXISTS `sys`", "writes to a system schema"),
            ("DELETE FROM mysql.user", "writes to a system schema"),
            (
                "INSERT INTO `mysql`.`db` VALUES (1)",
                "writes to a system schema",
            ),
            (
                "WITH c AS (SELECT 1) DELETE FROM mysql.user",
                "writes to a system schema",
            ),
            (
                "CALL sys.ps_setup_reset_to_default(TRUE)",
                "writes to a system schema",
            ),
            (
                "/*!50000 SET GLOBAL x = 1 */",
                "global or persisted server variable",
            ),
        ] {
            assert_eq!(hit(sql), Some(label), "{sql}");
        }
    }

    #[test]
    fn hazards_behind_other_syntax_are_caught() {
        // Several statements in one text: only the first used to be looked at.
        assert_eq!(hit("SELECT 1; SHUTDOWN"), Some("SHUTDOWN / RESTART"));
        assert_eq!(
            hit("SELECT 1; SET GLOBAL x=1;"),
            Some("global or persisted server variable")
        );
        let mut f = HazardFilter::new();
        assert_eq!(
            f.check("USE mysql; DELETE FROM user"),
            Some("writes to a system schema")
        );
        // A skipped statement did not run, so its USE did not happen.
        assert_eq!(f.check("DELETE FROM user"), None);
        // Prepared text, program bodies, and quoting tricks that used to hide a statement.
        assert_eq!(
            hit("PREPARE s FROM 'SET GLOBAL x=1'"),
            Some("global or persisted server variable")
        );
        assert_eq!(
            hit("PREPARE s FROM \"SHUTDOWN\""),
            Some("SHUTDOWN / RESTART")
        );
        assert_eq!(
            hit("CREATE EVENT e ON SCHEDULE EVERY 1 SECOND DO SET GLOBAL x = 1"),
            Some("hazard inside a stored program or event")
        );
        assert_eq!(
            hit("CREATE PROCEDURE p() DELETE FROM mysql.user"),
            Some("hazard inside a stored program or event")
        );
        assert_eq!(
            hit("CREATE TRIGGER g BEFORE INSERT ON t FOR EACH ROW SET @@global.x = 1"),
            Some("hazard inside a stored program or event")
        );
        assert_eq!(
            hit("SELECT `a'b` FROM t INTO OUTFILE '/tmp/x'"),
            Some("INTO OUTFILE / DUMPFILE")
        );
        assert_eq!(
            hit("SELECT `a#b`, 1 INTO OUTFILE '/tmp/x'"),
            Some("INTO OUTFILE / DUMPFILE")
        );
        assert_eq!(
            hit("SELECT 'it''s' INTO OUTFILE '/tmp/x'"),
            Some("INTO OUTFILE / DUMPFILE")
        );
        assert_eq!(
            hit("SELECT 'a\\'b' INTO OUTFILE '/tmp/x'"),
            Some("INTO OUTFILE / DUMPFILE")
        );
    }

    #[test]
    fn ordinary_statements_and_lookalikes_run() {
        for sql in [
            "SELECT 1",
            "SET @x = 'global'",
            "SET @global = 1",
            "SET @save = @@global.sql_mode",
            "SET SESSION sql_mode = ''",
            "SET @@session.sql_mode = ''",
            "SELECT @@global.version",
            "SELECT * FROM mysql.user",
            "SELECT 'INTO OUTFILE'",
            "SELECT SUBSTRING_INDEX('www.mysql.com', '.', 1)",
            "INSERT INTO t VALUES ('SHUTDOWN')",
            "-- SHUTDOWN\nSELECT 1",
            "CREATE TABLE killer (id INT)",
            "DROP DATABASE test",
            "CREATE DATABASE db1",
            "FLUSH PRIVILEGES",
            "USE test",
            "PREPARE s FROM 'SELECT 1'",
            "CREATE PROCEDURE p() SELECT 1",
            "CREATE FUNCTION f(a INT) RETURNS INT RETURN a + 1",
            "XA START 'x'",
            "SELECT `a#b` FROM t",
        ] {
            assert_eq!(hit(sql), None, "{sql}");
        }
    }

    #[test]
    fn writes_after_use_of_a_system_schema_are_caught_until_the_next_use() {
        let mut f = HazardFilter::new();
        assert_eq!(f.check("USE mysql"), None);
        assert_eq!(
            f.check("DELETE FROM user"),
            Some("writes to a system schema")
        );
        assert_eq!(f.check("SELECT * FROM user"), None);
        assert_eq!(f.check("USE test"), None);
        assert_eq!(f.check("DELETE FROM user"), None);
        f.check("USE sys");
        f.new_file();
        assert_eq!(f.check("DELETE FROM user"), None);
    }

    #[test]
    fn prepare_and_program_bodies_are_found_wherever_they_sit() {
        let h = "global or persisted server variable";
        let p = "hazard inside a stored program or event";
        assert_eq!(hit("SELECT 1; PREPARE s FROM 'SET GLOBAL x=1'"), Some(h));
        assert_eq!(
            hit("/* c */ PREPARE s FROM 'SHUTDOWN'"),
            Some("SHUTDOWN / RESTART")
        );
        assert_eq!(
            hit("PREPARE /*c*/ s FROM 'SHUTDOWN'"),
            Some("SHUTDOWN / RESTART")
        );
        assert_eq!(
            hit("PREPARE `s` FROM 'SHUTDOWN'"),
            Some("SHUTDOWN / RESTART")
        );
        assert_eq!(
            hit("PREPARE s FROM 'it''s fine'; PREPARE t FROM 'SHUTDOWN'"),
            Some("SHUTDOWN / RESTART")
        );
        assert_eq!(
            hit("CREATE PROCEDURE p() BEGIN SELECT 1; IF 1 THEN SET GLOBAL x=1; END IF; END"),
            Some(p)
        );
        assert_eq!(hit("ALTER EVENT e DO SET GLOBAL x=1"), Some(p));
        assert_eq!(
            hit("CREATE DEFINER = CURRENT_USER () PROCEDURE p() SET GLOBAL x=1"),
            Some(p)
        );
        assert_eq!(
            hit("CREATE DEFINER = root @ localhost EVENT e ON SCHEDULE EVERY 1 SECOND DO SET GLOBAL x=1"),
            Some(p)
        );
        for ok in [
            "INSERT INTO t SELECT * FROM information_schema.tables",
            "CREATE TABLE ev (event INT)",
            "PREPARE s FROM 'SELECT 1'; EXECUTE s",
        ] {
            assert_eq!(hit(ok), None, "{ok}");
        }
    }
}
