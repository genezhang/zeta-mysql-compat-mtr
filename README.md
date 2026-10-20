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

Every statement of every `.test` file runs on both servers (the oracle's non-system databases are dropped and `test` is recreated before each file) and the outcomes are compared: result set (column names and rows, `--sorted_result` honoured), affected rows, or error code. The run continues past errors, so one divergence does not hide the rest of a file.

`--exclude <substring>` (repeatable) skips files. Files that change accounts or global variables (`CREATE USER`, `SET GLOBAL`, `SHUTDOWN`, ...) damage a shared oracle, so run them one at a time against a restored data directory. Start the oracle with `--default-time-zone=+00:00`, since zeta evaluates in UTC.

Output, one row per file on stdout: `file statements same first_divergence first_kind mysql_ok mysql_ok_same first_hard first_hard_kind diverging_kinds`. A *hard* divergence is one on a statement MySQL itself accepted (a result that differs, or an error from zeta); a statement that MySQL rejects and zeta accepts is only leniency. Totals and histograms go to stderr. The `--report` file holds statement text and both outcomes, so keep it local if the test content is GPL.

## License

GPL v2. See `LICENSE`. Adapted test files preserve their upstream GPL headers; see `UPSTREAM.md` for the version of MySQL/MariaDB whose tests this repo tracks.

## Companion

[`zeta-mysql-compat`](https://github.com/genezhang/zeta-mysql-compat) (Apache 2.0) holds the permissively-licensed half of the test material. The two repos are run as separate CI jobs against the same Zeta binary; failures from one never imply contamination of the other.
