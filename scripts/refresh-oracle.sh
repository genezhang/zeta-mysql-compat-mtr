#!/usr/bin/env bash
# Re-derive the committed MySQL expectations from a real MySQL server, in one command:
#   1. run the corpus against the oracle twice (restoring it before each pass, second pass in reverse file order and on
#      a clock shifted by weeks), store what MySQL did for every statement in tests/<suite>/<name>.oracle.jsonl, and
#      mark nondeterministic answers (see runner/src/record.rs);
#   2. regenerate the human-readable tests/<suite>/<name>.result of every MySQL-targeted test from those records.
# Then `git status` / `git diff --stat` show exactly which tests the new MySQL behaviour moved.
#
#   scripts/refresh-oracle.sh [--filter SUBSTR]
#
# Environment:
#   ORACLE_URL          base URL of the oracle, default mysql://root@127.0.0.1:33306. The run DROPS every non-system
#                       database on it, so it must be a throwaway mysqld (MySQL 8.4, --default-time-zone=+00:00,
#                       default sql_mode). A host other than this machine is refused by the runner.
#   ORACLE_RESTORE_CMD  shell command that puts the oracle's data directory back to pristine and starts it. Without
#                       it the second pass starts from the state the first one left, which weakens the check for
#                       answers that depend on server state (counters, global settings).
#   RUNNER              path of the built runner. Default: build it (`cargo build --release` in runner/, a no-op when
#                       nothing changed) and use CARGO_TARGET_DIR (default runner/target)/release/.
#   CORPUS_DIR          the directory that holds tests/<suite>/*.test (default: the repository root). Any other
#                       corpus works the same way, e.g. an application-workload corpus kept in another repository;
#                       it adopts oracle records by having a tests/zeta_only.list (which may list nothing).
#
# Every test is recorded, the PostgreSQL-grammar ones too (their answers feed the separate "zeta-only" figures), but
# only the MySQL-targeted tests get a generated .result file; the others keep their hand-maintained one.
# The records of tests that no longer exist are not removed: delete their .oracle.jsonl by hand.
#
# Partial refresh (--filter) re-records only the matching tests; the records of the others are left alone, which is
# right after fixing one family but never replaces a full refresh when the oracle's version changes.
set -euo pipefail
here="$(cd "$(dirname "$0")/.." && pwd)"
cd "${CORPUS_DIR:-$here}"

filter=()
while [ "$#" -gt 0 ]; do
  case "$1" in
    --filter) filter=(--filter "${2:?--filter needs a value}"); shift 2 ;;
    -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 64 ;;
  esac
done

url="${ORACLE_URL:-mysql://root@127.0.0.1:33306}"
if [ -n "${RUNNER:-}" ]; then
  runner="$RUNNER"
else
  (cd "$here/runner" && cargo build --release) >&2
  runner="${CARGO_TARGET_DIR:-$here/runner/target}/release/zeta-mysql-compat-mtr-runner"
fi
[ -x "$runner" ] || { echo "runner not found or not executable: $runner" >&2; exit 65; }
restore=()
[ -n "${ORACLE_RESTORE_CMD:-}" ] && restore=(--oracle-restore-cmd "$ORACLE_RESTORE_CMD")

before="$(git status --porcelain tests | wc -l)"
# ${a[@]+"${a[@]}"}: an empty array expands to nothing even under `set -u` on bash < 4.4.
"$runner" --suite all --oracle-url "$url" --record-oracle tests ${restore[@]+"${restore[@]}"} ${filter[@]+"${filter[@]}"}
"$runner" --suite all --write-results ${filter[@]+"${filter[@]}"}
echo
changed="$(git status --short tests)"
echo "tests changed by this refresh (records and results):"
printf '%s\n' "$changed" | sed -n '1,60p' | sed 's/^/  /'
echo "($(printf '%s\n' "$changed" | sed '/^$/d' | wc -l) paths changed, $before before this run)"
