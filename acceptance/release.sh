#!/usr/bin/env bash
# The release gate of Rewind (acceptance/release/README.md): run before any release. It builds the
# tree in release mode, runs every stage against the real binary, and writes a GO or NO-GO report
# to acceptance/results/release/ (report.html, report.md, report.json).
#
#   acceptance/release.sh                       every stage: GO or NO-GO
#   acceptance/release.sh --area sandbox        some stages only (an area, or criteria, lifecycle,
#                                               load); the verdict is then PARTIAL
#   acceptance/release.sh --skip-load           everything but the load test (PARTIAL)
#   acceptance/release.sh --skip-build          use the binary already built (REWIND_BIN, or
#                                               target/release/rewind)
#
# Exit code: 0 when every check that ran passed (GO, or PARTIAL without a failure), 1 otherwise.
# Needs Rust, Node 22.12 or later with pnpm 10, and Docker. It removes its containers and stops its
# servers at the end, even after Ctrl-C. WorkOS is never used: tenants are made with the
# operator's key.
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$PWD"
RESULTS="$ROOT/acceptance/results/release"
STAGES_ALL=(build criteria areas lifecycle sweeps load)
AREAS=()
SKIP_LOAD=0
SKIP_BUILD=0

while [ $# -gt 0 ]; do
  case "$1" in
    --area) AREAS+=("$2"); shift 2 ;;
    --area=*) AREAS+=("${1#--area=}"); shift ;;
    --skip-load) SKIP_LOAD=1; shift ;;
    --skip-build) SKIP_BUILD=1; shift ;;
    -h | --help) sed -n '2,18p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1 (see --help)" >&2; exit 64 ;;
  esac
done

# Which stages run: all, or those --area names (an area spec selects the stage `areas`).
SPECS=()
run_criteria=1 run_areas=1 run_lifecycle=1 run_load=1 partial=0
if [ ${#AREAS[@]} -gt 0 ]; then
  partial=1 run_criteria=0 run_areas=0 run_lifecycle=0 run_load=0
  for area in "${AREAS[@]}"; do
    case "$area" in
      criteria) run_criteria=1 ;;
      lifecycle) run_lifecycle=1 ;;
      load) run_load=1 ;;
      *)
        spec="release/areas/$area.spec.ts"
        if [ ! -f "acceptance/$spec" ]; then
          echo "no area $area: $(cd acceptance/release/areas && ls *.spec.ts | sed 's/\.spec\.ts//' | tr '\n' ' ')" >&2
          exit 64
        fi
        run_areas=1
        SPECS+=("$spec")
        ;;
    esac
  done
fi
[ "$SKIP_LOAD" = 1 ] && run_load=0 && partial=1

export REWIND_BIN="${REWIND_BIN:-$ROOT/target/release/rewind}"
export REWIND_GATE_RESULTS="$RESULTS"
# WorkOS is out of the gate: no step and no server gets its credentials.
unset WORKOS_API_KEY WORKOS_CLIENT_ID WORKOS_COOKIE_PASSWORD WORKOS_REDIRECT_URI

cleanup() {
  docker ps -aq --filter label=rewind-acceptance 2>/dev/null | xargs docker rm -f >/dev/null 2>&1 || true
  pkill -f "$REWIND_BIN server" 2>/dev/null || true
}
trap cleanup EXIT
trap 'exit 130' INT TERM

note() { printf '\n== %s\n' "$*"; }

# One stage: its output in <name>.log, its outcome in stages.jsonl.
stage() {
  local name="$1" title="$2"
  shift 2
  note "$title"
  local started status=pass
  started=$(date +%s)
  "$@" >"$RESULTS/$name.log" 2>&1 || status=fail
  tail -n 8 "$RESULTS/$name.log"
  printf '{"stage":"%s","title":"%s","status":"%s","seconds":%s,"log":"%s.log"}\n' \
    "$name" "$title" "$status" "$(($(date +%s) - started))" "$name" >>"$RESULTS/stages.jsonl"
  [ "$status" = pass ]
}

# A stage left out by the command line: shown in the report, never as passed.
excluded() {
  printf '{"stage":"%s","title":"%s","status":"excluded","reason":"%s"}\n' "$1" "$2" "$3" >>"$RESULTS/stages.jsonl"
}

rm -rf "$RESULTS"
mkdir -p "$RESULTS"
cleanup

# What is under test.
node -e '
const { execSync } = require("node:child_process");
const sh = (c) => { try { return execSync(c, { encoding: "utf8", stdio: ["ignore", "pipe", "ignore"] }).trim(); } catch { return ""; } };
require("node:fs").writeFileSync(process.argv[1], JSON.stringify({
  startedAt: new Date().toISOString(),
  commit: sh("git rev-parse HEAD"),
  branch: sh("git rev-parse --abbrev-ref HEAD"),
  dirty: sh("git status --porcelain") !== "",
  subject: sh("git log -1 --format=%s"),
  os: sh("uname -srm"),
  docker: sh("docker version --format {{.Server.Version}}"),
  node: process.version,
  rustc: sh("rustc --version"),
  partial: process.argv[2] === "1",
  args: process.argv.slice(3),
}, null, 2));
' "$RESULTS/meta.json" "$partial" "${AREAS[@]+"${AREAS[@]}"}"

build() {
  export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}"
  command -v sccache >/dev/null && export RUSTC_WRAPPER="${RUSTC_WRAPPER:-sccache}"
  if [ "$SKIP_BUILD" = 0 ]; then
    cargo build --release --locked -p rewind
  fi
  [ -x "$REWIND_BIN" ] || { echo "no rewind binary at $REWIND_BIN"; return 1; }
  "$REWIND_BIN" --version
  pnpm --dir sdk/typescript install --frozen-lockfile
  pnpm --dir sdk/typescript build
  pnpm --dir acceptance install --frozen-lockfile
  pnpm --dir acceptance exec playwright install chromium
  # The reference MCP server of criterion 3, fetched once so no check waits for npm.
  npx -y @modelcontextprotocol/server-everything@2.2.1 stdio </dev/null >/dev/null 2>&1 || true
}

# One Playwright project, to its end; its checks land in playwright-<project>.json.
playwright() {
  local project="$1"
  shift
  (cd acceptance && REWIND_GATE_PROJECT="$project" pnpm exec playwright test -c playwright.release.config.ts --project="$project" "$@")
}

if stage build "build: rewind in release mode, the SDK, the suite" build; then
  "$REWIND_BIN" --version >"$RESULTS/version.txt" 2>&1 || true

  if [ "$run_criteria" = 1 ]; then
    REWIND_GATE_NO_STACKS=1 stage criteria "criteria 1, 2, 3, 5 and 7 of 0.1" playwright criteria || true
  else
    excluded criteria "criteria 1, 2, 3, 5 and 7 of 0.1" "not selected"
  fi

  if [ "$run_areas" = 1 ]; then
    stage areas "areas on setups A (deployed) and B (strict)" playwright areas "${SPECS[@]+"${SPECS[@]}"}" || true
  else
    excluded areas "areas on setups A (deployed) and B (strict)" "not selected"
  fi

  if [ "$run_lifecycle" = 1 ]; then
    REWIND_GATE_NO_STACKS=1 stage lifecycle "lifecycle on setup D" playwright lifecycle || true
  else
    excluded lifecycle "lifecycle on setup D" "not selected"
  fi

  REWIND_GATE_NO_STACKS=1 stage sweeps "sweeps: 5xx, panics, the canary, problem slugs" playwright sweeps || true

  if [ "$run_load" = 1 ]; then
    stage load "load and limits (criterion 6, deploy/acceptance.sh)" deploy/acceptance.sh || true
  elif [ "$SKIP_LOAD" = 1 ]; then
    excluded load "load and limits (criterion 6, deploy/acceptance.sh)" "--skip-load"
  else
    excluded load "load and limits (criterion 6, deploy/acceptance.sh)" "not selected"
  fi
fi

node acceptance/release/report/build.mjs "$RESULTS"
