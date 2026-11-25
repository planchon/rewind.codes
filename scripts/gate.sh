#!/usr/bin/env bash
# The gate of the Rust workspace, run on the developer's machine before a commit lands on main
# (CLAUDE.md) and before every deploy (.claude/skills/deploy): what GitHub Actions ran until
# November 2026 (archi/08-decisions.md, "Taken for the development loop"). It needs Docker: the
# tests that start Postgres, MinIO or a Collector fail instead of skipping.
#
#   scripts/gate.sh
#
# Every step runs, even after a failure; the exit status is 1 when one failed.
set -uo pipefail
cd "$(dirname "$0")/.."

export REWIND_REQUIRE_DOCKER=1

if ! docker info >/dev/null 2>&1; then
  echo "gate: Docker is not running (start OrbStack): the store, S3 and Collector tests need it" >&2
  exit 1
fi

failed=()
step() {
  local name=$1
  shift
  local started=$SECONDS
  echo "==> $name"
  if "$@"; then
    echo "    $name: ok in $((SECONDS - started)) s"
  else
    echo "    $name: FAILED in $((SECONDS - started)) s"
    failed+=("$name")
  fi
}

step format cargo fmt --all --check
step clippy cargo clippy --workspace --all-targets --locked -- -D warnings
step tests cargo nextest run --workspace --locked
step doctests cargo test --workspace --doc --locked --quiet
step rustdoc env RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked --quiet

if ((${#failed[@]})); then
  echo "gate: FAILED: ${failed[*]}"
  exit 1
fi
echo "gate: ok in $SECONDS s"
