#!/usr/bin/env bash
# Run every check and record the output in test/results/latest.
# Any number quoted on kvsql.com comes from a run of this script.
#
# Heavier settings than plain `cargo test`; override with the same variables:
#   KVSQL_POWER_CUTS   simulated power cuts            (default here 20000)
#   KVSQL_MODEL_ROUNDS model-check transaction rounds  (default here 2000)
#   KVSQL_KILL_ROUNDS  real-process SIGKILL rounds     (default here 200)
#   KVSQL_CHURN_SECS   readers-beside-writer stress    (default here 10)
#   KVSQL_SQL_STMTS    random SQL checked vs SQLite    (default here 30000)
#   KVSQL_SERVER_SECS  concurrent HTTP clients run     (default here 10)
#   KVSQL_SERVER_KILLS kvsqld SIGKILLs under load      (default here 100)
#   KVSQL_SEED         replay one seed
set -euo pipefail
cd "$(dirname "$0")/.."

export KVSQL_POWER_CUTS="${KVSQL_POWER_CUTS:-20000}"
export KVSQL_MODEL_ROUNDS="${KVSQL_MODEL_ROUNDS:-2000}"
export KVSQL_KILL_ROUNDS="${KVSQL_KILL_ROUNDS:-200}"
export KVSQL_CHURN_SECS="${KVSQL_CHURN_SECS:-10}"
export KVSQL_SQL_STMTS="${KVSQL_SQL_STMTS:-30000}"
export KVSQL_SERVER_SECS="${KVSQL_SERVER_SECS:-10}"
export KVSQL_SERVER_KILLS="${KVSQL_SERVER_KILLS:-100}"

result_dir="test/results"
mkdir -p "$result_dir"
# One current run lives in test/results/latest; git history keeps earlier runs.
result_path="$result_dir/latest"
rm -rf "$result_path"
mkdir "$result_path"
exec > >(tee "$result_path/checks.log") 2>&1

printf 'UTC: %s\n' "$(date -u +%FT%TZ)"
printf 'Host: %s %s\n' "$(uname -s)" "$(uname -m)"
rustc --version
cargo --version
printf 'Parent commit: %s\n' "$(git rev-parse HEAD 2>/dev/null || printf unknown)"
printf 'Settings: KVSQL_POWER_CUTS=%s KVSQL_MODEL_ROUNDS=%s KVSQL_KILL_ROUNDS=%s KVSQL_CHURN_SECS=%s KVSQL_SQL_STMTS=%s KVSQL_SERVER_SECS=%s KVSQL_SERVER_KILLS=%s\n' \
  "$KVSQL_POWER_CUTS" "$KVSQL_MODEL_ROUNDS" "$KVSQL_KILL_ROUNDS" "$KVSQL_CHURN_SECS" "$KVSQL_SQL_STMTS" \
  "$KVSQL_SERVER_SECS" "$KVSQL_SERVER_KILLS"

cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --release -- --nocapture --test-threads=1

printf 'PASS: formatting, clippy, unit, model, snapshot, SQL, SQLite differential, power-cut, SIGKILL, server API, concurrent client and server-kill tests\n'
