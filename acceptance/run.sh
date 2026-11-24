#!/usr/bin/env bash
# The acceptance suite of Rewind 0.1 (archi/09-roadmap.md, "0.1 is done when"): each criterion
# proven end to end on the real `rewind` binary.
#
#   acceptance/run.sh            every criterion
#   acceptance/run.sh 1 3 7      these criteria only
#
# Needs Rust, Node 22.12 or later with pnpm 10, and Docker (Postgres, MinIO, an OpenTelemetry
# Collector). Criterion 8 signs in against WorkOS Staging: its credentials, WORKOS_API_KEY and
# WORKOS_CLIENT_ID, come from the environment; without them it is reported as not run. It builds
# what it runs, removes its containers and stops its processes at the end, and writes logs and
# reports to acceptance/results/ (summary.md, report.json, html/).
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$PWD"
RESULTS="$ROOT/acceptance/results"
CRITERIA=("$@")
[ ${#CRITERIA[@]} -eq 0 ] && CRITERIA=(1 2 3 4 5 6 7 8)

selected() {
  local c
  for c in "${CRITERIA[@]}"; do [ "$c" = "$1" ] && return 0; done
  return 1
}

cleanup() {
  # Containers of the suite, and servers left by an interrupted run.
  docker ps -aq --filter label=rewind-acceptance 2>/dev/null | xargs docker rm -f >/dev/null 2>&1 || true
  pkill -f "$ROOT/target/debug/rewind server" 2>/dev/null || true
}
trap cleanup EXIT
trap 'exit 130' INT TERM

note() { printf '\n== %s\n' "$*"; }

# WorkOS Staging's credentials reach criterion 8's spec only: a step run here gets none of the
# WORKOS_* (a server given some of the four refuses to start), and the specs' servers neither
# (acceptance/support/processes.ts).
WITHOUT_WORKOS=(env -u WORKOS_API_KEY -u WORKOS_CLIENT_ID -u WORKOS_COOKIE_PASSWORD -u WORKOS_REDIRECT_URI)
staging() { [ -n "${WORKOS_API_KEY:-}" ] && [ -n "${WORKOS_CLIENT_ID:-}" ]; }

# A step run by this script: its output in results/<name>.log, its status in steps.jsonl.
step() {
  local criterion="$1" name="$2"
  shift 2
  note "criterion $criterion: $name"
  local status=pass
  "${WITHOUT_WORKOS[@]}" "$@" >"$RESULTS/$name.log" 2>&1 || status=fail
  tail -n 5 "$RESULTS/$name.log"
  printf '{"criterion":"%s","name":"%s","status":"%s"}\n' "$criterion" "$name: $*" "$status" >>"$RESULTS/steps.jsonl"
}

# A criterion that cannot run here, and why: reported as not run, never as passed.
not_run() {
  note "criterion $1: not run: $2"
  printf '{"criterion":"%s","name":"not run","status":"not_run","reason":"%s"}\n' "$1" "$2" >>"$RESULTS/steps.jsonl"
}

rm -rf "$RESULTS"
mkdir -p "$RESULTS"
cleanup

note "building rewind, the evals, the SDK and the suite"
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-3}"
command -v sccache >/dev/null && export RUSTC_WRAPPER="${RUSTC_WRAPPER:-sccache}"
cargo build -p rewind -p evals
pnpm --dir sdk/typescript install --frozen-lockfile
pnpm --dir sdk/typescript build
pnpm --dir acceptance install --frozen-lockfile
pnpm --dir acceptance exec playwright install chromium
# The reference MCP server, fetched once so no test waits for npm.
npx -y @modelcontextprotocol/server-everything@2.2.1 stdio </dev/null >/dev/null 2>&1 || true

# Criterion 2, the part the CLI's own end-to-end tests prove through the binary: a run that
# sleeps, is interrupted and resumes in another process; signals across processes; kill -9.
if selected 2; then
  step 2 "cli-across-processes" cargo test -p rewind --test integration cli::
fi

# Criterion 6: the operations' load and limit tests.
if selected 6; then
  if [ -x deploy/acceptance.sh ]; then
    step 6 "load-and-limits" deploy/acceptance.sh
  else
    note "criterion 6: deploy/acceptance.sh is missing: the load and limit tests are not there"
  fi
fi

# Every other check: Playwright, one spec file per criterion.
specs=()
for c in "${CRITERIA[@]}"; do
  case "$c" in
    1) specs+=(tests/c1-sdk-workers-postgres.spec.ts) ;;
    2) specs+=(tests/c2-local-endings.spec.ts) ;;
    3 | 5) specs+=(tests/c3-c5-mcp-test-fork.spec.ts) ;;
    4) specs+=(tests/c4-evals.spec.ts) ;;
    7) specs+=(tests/c7-dashboard.spec.ts) ;;
    8)
      if staging; then
        specs+=(tests/c8-accounts.spec.ts)
      else
        not_run 8 "WORKOS_API_KEY and WORKOS_CLIENT_ID (WorkOS Staging) are not set"
      fi
      ;;
  esac
done
if [ ${#specs[@]} -gt 0 ]; then
  specs=($(printf '%s\n' "${specs[@]}" | sort -u))
  note "the Playwright suite: ${specs[*]}"
  (cd acceptance && pnpm exec playwright test "${specs[@]}") || true
fi

node acceptance/support/summary.mjs "$RESULTS" "${CRITERIA[@]}"
