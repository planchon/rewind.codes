#!/usr/bin/env bash
# Criterion 6 of 0.1, "Load tests and limit tests pass" (archi/09-roadmap.md): the limits of
# archi/03-execution.md through the API, then the reference deployment brought up from this
# directory and loaded by ptc-load, with the thresholds recorded in 09 ("Load tests").
#
#   deploy/acceptance.sh
#
# Needs Rust, Docker with Compose, and Node with pnpm (the SDK's live test). It builds the
# images, runs the deployment as its own Compose project beside any other (its own port,
# network and secrets; deploy/.env is not read), and removes its containers, volumes, networks
# and images at the end. Exits with 0 only when every step passed.
#
#   PTC_ACCEPTANCE_PORT   the edge's port on 127.0.0.1 (18580)
#   PTC_ACCEPTANCE_KEEP=1 keep the deployment and the images, for a look afterwards
set -euo pipefail

cd "$(dirname "$0")"
DEPLOY="$PWD"
ROOT="$(cd .. && pwd)"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/ptc-acceptance.XXXXXX")"
PORT="${PTC_ACCEPTANCE_PORT:-18580}"
URL="http://127.0.0.1:$PORT"

export COMPOSE_PROJECT_NAME="ptc-acceptance-$$"
export COMPOSE_ENV_FILES="$WORK/env"
export COMPOSE_PROFILES=runc
export PTC_IMAGE="ai-ptc:acceptance-$$"
export PTC_LOAD_IMAGE="ai-ptc-load:acceptance-$$"
export PTC_BIND=127.0.0.1 PTC_PORT="$PORT" PTC_PUBLIC_URL="$URL"
export PTC_CORE_SUBNET=172.29.223.0/24 PTC_CORE_RANGE=172.29.223.128/25
export PTC_EGRESS_ADDRESS=172.29.223.2
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-3}"
command -v sccache >/dev/null && export RUSTC_WRAPPER="${RUSTC_WRAPPER:-sccache}"

failed=()
note() { printf '\n== %s\n' "$*"; }
# Runs a step; a failure is recorded and the next steps still run.
step() {
  local name="$1"
  shift
  note "$name"
  if "$@"; then
    echo "-- $name: passed"
  else
    echo "-- $name: FAILED"
    failed+=("$name")
  fi
}

cleanup() {
  local status=$?
  if [ "${PTC_ACCEPTANCE_KEEP:-}" = 1 ]; then
    note "kept: COMPOSE_PROJECT_NAME=$COMPOSE_PROJECT_NAME COMPOSE_ENV_FILES=$WORK/env, on $URL"
    return
  fi
  note "cleaning up"
  docker compose --profile runc --profile load down -v --remove-orphans >/dev/null 2>&1 || true
  docker image rm -f "$PTC_IMAGE" "$PTC_LOAD_IMAGE" >/dev/null 2>&1 || true
  rm -rf "$WORK"
  return "$status"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

# 1. The limits, through the HTTP API of a server on Turso: CPU per call, heap, commands per
#    activation, the history cap, payload sizes, the TTL; the quota's 429.
limits() {
  (cd "$ROOT" && cargo test -p ptc-server --test limits &&
    cargo test -p ptc-server --test api a_tenant_above_its_quota_is_refused_with_429)
}
step "limits through the API" limits

# 2. The reference deployment, from its images.
images() {
  local jobs=(--build-arg "JOBS=$CARGO_BUILD_JOBS")
  docker build --quiet "${jobs[@]}" --target ptc -t "$PTC_IMAGE" "$ROOT" &&
    docker build --quiet "${jobs[@]}" --target load -t "$PTC_LOAD_IMAGE" "$ROOT"
}
up() {
  ./init.sh "$WORK/env" >/dev/null &&
    docker compose up -d --quiet-pull &&
    wait_for_api
}
wait_for_api() {
  local key
  key="$(sed -n 's/^PTC_ADMIN_KEY=//p' "$WORK/env")"
  for _ in $(seq 90); do
    curl -fsS -H "Authorization: Bearer $key" "$URL/v1/server" >/dev/null 2>&1 && return 0
    sleep 2
  done
  echo "the API did not answer on $URL"
  docker compose logs --tail 50
  return 1
}
step "images" images || true
if [[ " ${failed[*]-} " == *" images "* ]]; then
  note "the images could not be built: the deployment's steps are not run"
else
  step "deployment up" up
  step "the workers' walls" bash -c './check-walls.sh worker-1-net && ./check-walls.sh worker-2-net'

  # 3. The load: small payloads, then payloads above 64 KiB, which go to S3.
  load() {
    docker compose --profile load run --rm --no-deps --use-aliases loadgen run \
      --timeout 15m --max-read-model-lag 3m "$@"
  }
  step "load: 1000 runs, 100 at once" load --runs 1000 --concurrency 100 \
    --min-runs-per-s 10 --max-p99-end-to-end 15s --max-p99-activation 2s
  step "load: 200 runs with 100 KiB payloads" load --runs 200 --concurrency 50 \
    --payload-bytes 102400 --min-runs-per-s 4 --max-p99-end-to-end 20s --max-p99-activation 3s

  # 4. The 256 KiB bound of the agent tools' run_script, against the deployment.
  sdk() {
    local admin key
    admin="$(sed -n 's/^PTC_ADMIN_KEY=//p' "$WORK/env")"
    key="$(curl -fsS "$URL/v1/api-keys" -H "Authorization: Bearer $admin" \
      -H 'content-type: application/json' -d '{"tenant": "sdk", "role": "harness"}' |
      sed -n 's/.*"key":"\(ptc_sk_[A-Za-z0-9]*\)".*/\1/p')"
    [ -n "$key" ] || return 1
    pnpm --dir "$ROOT/sdk/typescript" install --frozen-lockfile >/dev/null &&
      PTC_LIVE_URL="$URL" PTC_LIVE_KEY="$key" \
        pnpm --dir "$ROOT/sdk/typescript" exec vitest run test/live.test.ts
  }
  step "run_script's bound against the deployment" sdk

  if [ ${#failed[@]} -gt 0 ]; then
    note "logs of the deployment"
    docker compose logs --tail 100 api worker-1 worker-2 egress
  fi
fi

note "result"
if [ ${#failed[@]} -eq 0 ]; then
  echo "load and limit tests: passed"
  exit 0
fi
printf 'FAILED: %s\n' "${failed[@]}"
exit 1
