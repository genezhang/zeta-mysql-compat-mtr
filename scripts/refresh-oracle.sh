#!/usr/bin/env bash
# Re-derive the committed MySQL expectations from a real MySQL server, in one command:
#   1. run the corpus against the oracle twice (restoring it before each pass, second pass in reverse file order),
#      store what MySQL did for every statement in tests/<suite>/<name>.oracle.jsonl, and mark nondeterministic
#      answers (see runner/src/record.rs);
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
#   RUNNER              path of the built runner (default: runner/target/release/zeta-mysql-compat-mtr-runner).
#   CARGO_TARGET_DIR    where `cargo build` puts it, used only if RUNNER is unset and the binary is missing.
#
# Run it from the repository root. Partial refresh (--filter) re-records only the matching tests; the records of
# the others are left alone, which is right after fixing one family but never replaces a full refresh when the
# oracle's version changes.
set -euo pipefail
cd "$(dirname "$0")/.."

filter=()
while [ "$#" -gt 0 ]; do
  case "$1" in
    --filter) filter=(--filter "${2:?--filter needs a value}"); shift 2 ;;
    -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 64 ;;
  esac
done

url="${ORACLE_URL:-mysql://root@127.0.0.1:33306}"
runner="${RUNNER:-${CARGO_TARGET_DIR:-runner/target}/release/zeta-mysql-compat-mtr-runner}"
if [ ! -x "$runner" ]; then
  echo "building the runner: $runner not found" >&2
  (cd runner && cargo build --release)
  runner="${CARGO_TARGET_DIR:-runner/target}/release/zeta-mysql-compat-mtr-runner"
fi
restore=()
[ -n "${ORACLE_RESTORE_CMD:-}" ] && restore=(--oracle-restore-cmd "$ORACLE_RESTORE_CMD")

before="$(git status --porcelain tests | wc -l)"
"$runner" --suite all --oracle-url "$url" --record-oracle tests "${restore[@]}" "${filter[@]}"
"$runner" --suite all --write-results "${filter[@]}"
echo
echo "tests changed by this refresh (records and results):"
git status --short tests | head -60 | sed 's/^/  /'
echo "($(git status --porcelain tests | wc -l) paths changed, $before before this run)"
