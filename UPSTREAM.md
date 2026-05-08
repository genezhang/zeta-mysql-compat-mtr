# Upstream tracking

This repo's MTR-derived test files are adapted from public MySQL and MariaDB
sources. Specific upstream commits/versions tracked per directory are
recorded here; individual test files retain their original GPL v2 headers.

## tests/main/

- Source: https://github.com/mysql/mysql-server (`mysql-test/t/`, `mysql-test/r/`)
- Tracking version: TBD (initial seed pending)
- Last sync: TBD

## tests/funcs_1/ and tests/funcs_2/

- Source: https://github.com/mysql/mysql-server (`mysql-test/suite/funcs_1/`, `funcs_2/`)
- Tracking version: TBD
- Last sync: TBD

## tests/information_schema/

- Source: https://github.com/mysql/mysql-server (`mysql-test/suite/funcs_1/t/is_*.test`)
- Tracking version: TBD
- Last sync: TBD

## tests/json/

- Source: https://github.com/mysql/mysql-server (`mysql-test/t/json*.test`)
- Tracking version: TBD
- Last sync: TBD

## tests/binlog/

- Source: https://github.com/mysql/mysql-server (`mysql-test/suite/binlog/`,
  `mysql-test/suite/binlog_gtid/`, `mysql-test/suite/rpl/`)
- Tracking version: TBD
- Last sync: TBD
- Notes: this suite is the primary validation surface for Zeta's binlog/CDC
  compatibility (see `mysql_replication_design.md` in the main zeta repo).
  Will be populated once binlog phase B3 lands.

## tests/replication/

- Source: https://github.com/mysql/mysql-server (`mysql-test/suite/rpl/`)
- Tracking version: TBD
- Last sync: TBD
- Notes: subset that does not require an actual master-replica topology;
  GTID-set arithmetic, position handling, etc.

## Adaptation rules

1. Preserve upstream GPL v2 headers in every adapted test file.
2. Note adaptations (zeta-specific replacements, skipped directives) inline
   with `# zeta-adapt:` comments.
3. Tests that cannot be made to work without significant rewriting belong in
   `skip_list.toml` with a reason, not in deleted form.
