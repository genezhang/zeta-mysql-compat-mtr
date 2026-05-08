# zeta-mysql-compat-mtr

**GPL v2 licensed.** MySQL/MariaDB MTR-derived compatibility tests for [Zeta](https://github.com/genezhang/zeta).

This repo holds the **GPL-licensed** half of Zeta's MySQL-compat test material — tests adapted from MySQL's official `mysql-test/` suite and MariaDB's. It is intentionally **separate** from [`zeta-mysql-compat`](https://github.com/genezhang/zeta-mysql-compat) (Apache 2.0) so the licenses don't contaminate each other. There is no shared code or Cargo dependency between the two repos; the wall is deliberate.

## Status

Skeleton. Runner stub builds; `tests/` directories are empty placeholders. Will be populated as suites are adapted from upstream MTR.

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

```
cargo run --release -- --zeta-bin <path-to-zeta-binary> --suite main,binlog
```

The runner is **independently re-derived** from public MTR documentation. It does not copy from MySQL's `mysql-test-run.pl`. It connects to a running `zeta` server over the MySQL wire protocol and never links to Zeta as a library.

## License

GPL v2. See `LICENSE`. Adapted test files preserve their upstream GPL headers; see `UPSTREAM.md` for the version of MySQL/MariaDB whose tests this repo tracks.

## Companion

[`zeta-mysql-compat`](https://github.com/genezhang/zeta-mysql-compat) (Apache 2.0) holds the permissively-licensed half of the test material. The two repos are run as separate CI jobs against the same Zeta binary; failures from one never imply contamination of the other.
