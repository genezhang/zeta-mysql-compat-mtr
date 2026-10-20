# zeta-mysql-compat-mtr

**GPL v2 licensed.** MySQL/MariaDB MTR-derived compatibility tests for [Zeta](https://github.com/genezhang/zeta).

This repo holds the **GPL-licensed** half of Zeta's MySQL-compat test material — tests adapted from MySQL's official `mysql-test/` suite and MariaDB's. It is intentionally **separate** from [`zeta-mysql-compat`](https://github.com/genezhang/zeta-mysql-compat) (Apache 2.0) so the licenses don't contaminate each other. There is no shared code or Cargo dependency between the two repos; the wall is deliberate.

## Status

**M0 — first .test green.** The runner parses MTR-format `.test` files (subset: comments, multi-line SQL, `--sorted_result`, `--error <code>`, `--echo <text>`), executes statements over zeta's MySQL wire, captures output as tab-separated `.result`-format text, and diffs against the matching `.result` file. `tests/main/select.test` passes against the current `zeta` binary. Other suites (`funcs_1/`, `funcs_2/`, `information_schema/`, `json/`, `binlog/`, `replication/`) are still empty placeholders.

## Layout

```
runner/                  # MTR-dialect runner (GPL v2)
  src/
    main.rs              # CLI: --zeta-bin <path>, --suite <name>
    mtr_parser.rs        # parser for the MTR DSL (--source / --let / --error / etc.)
    result_diff.rs       # .result comparison with --replace_regex etc.
    harness.rs           # spawn the `zeta` server binary on an ephemeral port
    oracle.rs            # differential mode: run both servers, compare, summarise
    outcome.rs           # one statement's outcome on one server, and how two are compared
    hazard.rs            # statements the differential mode never runs
    side.rs              # one server connection: reset to empty, run within a budget, kill on timeout
    record.rs            # oracle records: format, nondeterminism classes, merge of passes, staleness
    class.rs             # tests/zeta_only.list: PostgreSQL-grammar and sentinel tests, kept apart
    result_file.rs       # renders a record into the .result text

tests/
  main/                  # adapted from MySQL mysql-test/t + r/
  funcs_1/, funcs_2/
  information_schema/
  json/
  binlog/                # critical for replication design validation
  replication/           # subset that doesn't require master-replica topology
  skip_list.toml
```

## Running

Build a `zeta` binary in the main repo (`cargo build -p zeta-server-bin`), then:

```
cd runner
cargo run -- --zeta-bin /path/to/zeta --suite main
```

`--suite` accepts a comma-separated list (e.g. `main,binlog`) or `all`. `--filter <substring>` limits to matching `.test` paths. The runner picks a free port, spawns `zeta --no-pg --bind 127.0.0.1 --mysql-port <port> --storage-backend memory`, waits for the listener-ready banner, then runs each `.test` file with a fresh `mysql_async` connection.

The runner is **independently re-derived** from public MTR documentation. It does not copy from MySQL's `mysql-test-run.pl`. It connects to a running `zeta` server over the MySQL wire protocol and never links to Zeta as a library.

## Differential mode (compare with a real MySQL)

The `.result` files in this repository record what zeta printed when the test was written, so a pass means "unchanged", not "agrees with MySQL". To measure agreement with MySQL itself, point the runner at a running `mysqld` as well:

```
cargo run --release -- --zeta-bin /path/to/zeta --suite all \
    --oracle-url mysql://root@127.0.0.1:33306 \
    --report /tmp/diff-report.tsv > /tmp/diff-summary.tsv
```

Start the oracle as a throwaway server (MySQL 8.4, `--default-time-zone=+00:00`, since zeta evaluates in UTC; the default `sql_mode`). **The run DROPS every non-system database on it**, so never point it at a server you care about: a host other than this machine is refused unless `--allow-remote-oracle` is given.

Every statement of every `.test` file runs on both servers and the outcomes are compared: result set (column names and rows, `--sorted_result` honoured), affected rows, or error number (the message text is not compared). A SQL `NULL` and the string `'NULL'` are different values. The run continues past errors, so one divergence does not hide the rest of a file; only the first result set of a multi-result response is compared, but all of them are read, and an error in a later set is the statement's outcome. Before each file both servers are reset (all non-system databases dropped, `test` recreated) and checked to be empty, and the file gets a fresh session on each (no unix-socket upgrade, and no connection-time `SELECT`), so the file's first statement sees no statement of ours. Both sessions run `SET NAMES utf8mb4`, which gives MySQL's real default collation `utf8mb4_0900_ai_ci` (NO PAD), as JDBC, mysql2 and the `mysql` client get it; the library's own handshake would announce `utf8mb4_general_ci` (PAD SPACE) and hide differences. zeta keeps a default database it refuses to drop; it must at least come back empty. The server versions, `sql_mode`, `lower_case_table_names`, `time_zone` and connection collation of both sides are printed at the start (stderr).

`--exclude <substring>` (repeatable) skips files; `--filter <substring>` selects them.

### Statements that are not run

Some statements would stop the oracle or leak state into every later file, so they are skipped **on both servers**, counted, and listed by class in the summary (they are not in the statement totals, but keep their position in the statement numbering, and a file with a skipped statement is never counted as identical to MySQL): `SHUTDOWN`/`RESTART`; replication, binary-log and clone commands (`RESET MASTER`, `PURGE`, `CHANGE ...`, `BINLOG`, ...); assignments to `GLOBAL`/`PERSIST` variables; `INTO OUTFILE`/`DUMPFILE`; account management (`CREATE`/`ALTER`/`DROP`/`RENAME USER`/`ROLE`, `GRANT`, `REVOKE`, `SET PASSWORD`); `KILL`; plugin/component/instance administration and `CREATE FUNCTION ... SONAME`; `XA PREPARE`; server-level objects (`SERVER`, `TABLESPACE`, `LOGFILE GROUP`, `RESOURCE GROUP`, ...); writes that name or follow a `USE` of a system schema (`mysql`, `sys`, `performance_schema`, `information_schema`) and `CALL sys.*`. The match is textual and conservative: comments and string literals are blanked, a text holding several statements is checked piece by piece, `PREPARE ... FROM '...'` is checked as the statement it holds, and the body of a `CREATE PROCEDURE/FUNCTION/TRIGGER/EVENT` is scanned for the same hazards. A lookalike can be skipped (it costs one counted statement); what cannot be seen is a statement assembled at run time (`SET @s = '...'; PREPARE x FROM @s`). The list is in `runner/src/hazard.rs`. With these skipped, one sweep runs end to end without restoring the oracle between files.

### Robustness

Every network step (connect, reset, statement) has a time budget. A statement that outlives its 20 s budget is killed on the server (`KILL CONNECTION`), not just abandoned, and is reported as `timeout` (zeta) or `oracle-timeout` (MySQL: nothing to compare, never counted as a statement MySQL accepted). If zeta exits, or a timed-out statement could not be stopped, zeta is restarted before the next file; the statements it missed are `transport` divergences (always hard). A zeta that cannot be restarted, or cannot be reset three times in a row, an oracle that cannot be reset, and an oracle that cannot be told to stop a timed-out statement all end the run; so does Ctrl-C. The summary is printed anyway (marked `RUN ABORTED`). When one side loses its session (timeout, dropped connection), the other side starts a fresh session too, so lost `SET` variables, temporary tables and transactions are lost on both.

### Output

One row per file on stdout, tab separated: `file statements same first_divergence first_kind mysql_ok mysql_ok_same first_hard first_hard_kind diverging_kinds skipped mysql_ok_order`. A file the parser rejects gets a full-width row with `parse-error` and is counted on stderr. The first ten columns are the original ones; `statements` counts statements that ran, while `first_divergence`/`first_hard` are positions in the file counting skipped statements.

A *hard* divergence is one that points at a defect in zeta: a result that differs on a statement MySQL itself accepted, an error from zeta where MySQL succeeded, or a `transport` failure. A statement that MySQL rejects and zeta accepts (`zeta-accepts`) is leniency, and two different error numbers (`error-code`) are an error-code gap; neither is hard. `row-order` (same rows, other order) is not hard either when the statement did not ask for an order, because a SELECT without ORDER BY has no defined order: it is counted in `mysql_ok_order`, never in `mysql_ok_same`, and listed in the kind counts. A row-order difference on a statement with a top-level ORDER BY (and no `--sorted_result`) is a `values` divergence. `mysql_ok` counts statements MySQL ran without error; `mysql_ok_same` those zeta matched exactly.

Totals and histograms (files identical to MySQL, statement agreement, MySQL-accepted statements zeta matched, skipped statements by class, first divergence per file by kind, all divergences by kind) go to stderr. The `--report` file holds statement text and both outcomes, so keep it local if the test content is GPL.

## Oracle records: MySQL's answers, committed

A live `mysqld` is the right oracle for a developer, but a nightly job should not need one, and "what does MySQL do here" should be a reviewable artefact rather than a server's mood. So the oracle's answers are recorded, and zeta is compared with the recording.

**Record** (needs a throwaway MySQL, no zeta): `--record-oracle [DIR] --oracle-url URL` runs every test file against the oracle and writes `tests/<suite>/<name>.oracle.jsonl` next to the test. The file is JSON Lines: a header (format version, MySQL version, `sql_mode`, `time_zone`, number of passes) and one object per statement with its full outcome: result set (column names and rows, `null` for SQL NULL), or affected rows, or error number and message. Errors are compared by number; the message is informational. A cell over 4 KiB is stored as its length and a hash. Each statement carries a short hash of its text, so a record that no longer matches its `.test` is reported as stale instead of being compared out of step. The file is deterministic: recording the same corpus again on the same MySQL version gives the same bytes.

**Nondeterminism is found, not guessed, and counted, not hidden.** The corpus is run `--record-passes` times (default 2), the oracle restored before each pass (`--oracle-restore-cmd`) and the second pass in reverse file order, and the passes are compared statement by statement:

| class | how it is found | what replay compares |
|---|---|---|
| `volatile` (with a `class`: `time`, `random`, `session`, `server`, `server-state`) | the statement text calls `NOW()`, `RAND()`, `UUID()`, `CONNECTION_ID()`, reads `@@hostname`, `@@version`, `SHOW STATUS`, ... | the shape (column names, row count, error number, affected rows), not the values |
| `cells` | the passes differ in some cells only | everything except those cells, which are stored as `<masked>` |
| `order` | the passes return the same rows in another order, or the query reads `information_schema` without ORDER BY | the rows as a sorted set |
| `excluded` | the passes differ in any other way | nothing; zeta still runs the statement, so later statements see its effects |

Statements that are never run at all (SHUTDOWN, SET GLOBAL, accounts, ...; see "Statements that are not run") are stored as `skip`. Replay prints how many statements fall in each class, next to the numbers they are part of.

**Replay** (needs zeta, no MySQL): `--replay-oracle [DIR] --zeta-bin ...` compares zeta with the records and prints exactly what the live differential mode prints (the per-file rows and the summary), plus `masked`, `excluded` and `class` columns. A file without a record, or whose record is stale, is reported and not compared.

**Classes.** `tests/zeta_only.list` names the tests that pin zeta's handling of PostgreSQL grammar and unsupported-feature probes (the `pg_*` and `*sentinel*` tests): MySQL rejects most of that by design, so they are not judged against MySQL. Their `.result` files stay pinned to zeta's own output and are maintained by hand, and the summary reports them as a separate class (`class zeta-only: ...`) so they do not pollute the MySQL number. Every test not listed is MySQL-targeted. Nothing is moved, so paths stay stable. `--class mysql|zeta-only` restricts any mode to one class.

**The normal mode** (`--zeta-bin ... --suite ...` without `--oracle-url` or `--replay-oracle`, the one the nightly runs per test) works like this for a MySQL-targeted test once the repository has adopted records (`tests/zeta_only.list` exists): the verdict is zeta against the record, statement by statement: the test passes only if zeta agrees with MySQL on every compared statement, row order included (`  OK`), and fails with a list of the first divergences otherwise. The `.result` file is no longer an independent expectation: it is MySQL's output in the runner's format, generated from the record (`--write-results`), and the normal mode fails if it is not what the record renders to. A MySQL-targeted test without a record fails and says so. The zeta-only tests keep the old behaviour (compare with the hand-maintained `.result`).

**Refreshing** after an oracle upgrade, or after adding or editing a test, is one command: `scripts/refresh-oracle.sh` (see its header for `ORACLE_URL` and `ORACLE_RESTORE_CMD`), then commit the changed `.oracle.jsonl` and `.result` files. `--filter` re-records only matching tests. To start an oracle: unpack a MySQL 8.4 server, initialise a data directory, keep a pristine copy of it for the restore command, and start `mysqld --default-time-zone=+00:00` on a spare port with default settings; `root` without a password on localhost is what the runner expects.

The records contain statement text and MySQL's output for tests that live in this GPL-2.0 repository, which is where they belong: never copy them into a permissively licensed repository or into a pull-request description.

## License

GPL v2. See `LICENSE`. Adapted test files preserve their upstream GPL headers; see `UPSTREAM.md` for the version of MySQL/MariaDB whose tests this repo tracks.

## Companion

[`zeta-mysql-compat`](https://github.com/genezhang/zeta-mysql-compat) (Apache 2.0) holds the permissively-licensed half of the test material. The two repos are run as separate CI jobs against the same Zeta binary; failures from one never imply contamination of the other.
