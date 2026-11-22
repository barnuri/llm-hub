#!/bin/sh
# The checks the pr workflow runs. `fast`: format and lint. `coverage`: tests with the coverage
# gate. No argument: both.
set -eu
mode="${1:-all}"

if [ "$mode" != coverage ]; then
  cargo fmt --check
  cargo clippy --all-targets -- -D warnings
fi
if [ "$mode" = fast ]; then
  exit 0
fi

# nextest runs each test in its own process across all cores, so it is the fast path when installed.
if ! cargo llvm-cov --version >/dev/null 2>&1; then
  echo "cargo-llvm-cov is not installed: running the tests without the coverage gate" >&2
  if cargo nextest --version >/dev/null 2>&1; then
    cargo nextest run --all-targets --no-fail-fast
  else
    cargo test --all-targets
  fi
  exit 0
fi

min="${COVERAGE_MIN:-80}"
if cargo nextest --version >/dev/null 2>&1; then
  cargo llvm-cov nextest --no-fail-fast --json --summary-only --fail-under-lines "$min" --output-path target/coverage.json
else
  cargo llvm-cov --all-targets --json --summary-only --fail-under-lines "$min" --output-path target/coverage.json
fi
below=$(jq -r --argjson min "$min" --arg src "$PWD/src/" '
  .data[0].files[]
  | select((.filename | startswith($src)) and .summary.lines.percent < $min)
  | "\(.summary.lines.percent | floor)% \(.filename | ltrimstr($src))"' target/coverage.json)
if [ -n "$below" ]; then
  echo "Files under $min% line coverage (CI measures on Linux):"
  echo "$below"
  exit 1
fi
echo "All checks passed."
