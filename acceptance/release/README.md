# The release gate

Run before any release. It builds the tree in release mode, starts Rewind as production runs it,
drives every feature through the real binary, the HTTP API, the SDK and the CLI, tries to break
it, and ends with a verdict and a report.

```sh
acceptance/release.sh                   # every stage: GO or NO-GO
acceptance/release.sh --area sandbox    # some stages: an area, or criteria, lifecycle, load (PARTIAL)
acceptance/release.sh --skip-load       # everything but the load test (PARTIAL)
acceptance/release.sh --skip-build      # the binary already built (REWIND_BIN, else target/release/rewind)
```

It needs Rust, Node 22.12 or later with pnpm 10, and Docker. WorkOS is never used: the operator's
key makes the tenants, and organizations, members and invitations stay with criterion 8 of
`acceptance/run.sh`, against WorkOS Staging. It removes its containers and stops its servers at the
end, even after Ctrl-C.

## Verdict

| Verdict | When | Exit |
| --- | --- | --- |
| GO | Every stage ran and every check passed | 0 |
| NO-GO | A check failed or did not run, or a stage failed | 1 |
| PARTIAL | A stage was left out by the command line, and nothing failed. Not a release verdict | 0 |

## Stages

| Stage | Runs | Where |
| --- | --- | --- |
| build | `cargo build --release -p rewind`, the SDK, the suite, Chromium | |
| criteria | Criteria 1, 2, 3, 5 and 7 of 0.1 (`tests/`), unchanged | Their own servers |
| areas | One spec per area (`areas/`), three files at once, a tenant each | Setups A and B, C for the CLI |
| lifecycle | Kills, restarts, a Postgres outage, retention and erasure (`lifecycle/`) | Setup D |
| sweeps | What the stages left: no unexpected 5xx, no panic, no leaked canary, every problem type provoked (`sweeps/`) | The results directory |
| load | Criterion 6: `deploy/acceptance.sh`, the reference deployment under load, and the limits | Its own Compose project |

A failure does not stop the run: every later stage still runs, so one report shows everything
that is broken. The GitHub workflow (`.github/workflows/release-gate.yml`, started by hand) runs
`--skip-load`.

## Setups

| Setup | What | Used for |
| --- | --- | --- |
| A, deployed | Postgres 17, MinIO, an OpenTelemetry Collector, `--role api`, two `--role worker`, behind an edge that sends `POST /v1/test` to the workers (as `deploy/edge/Caddyfile`); `--allow-internal-tools 127.0.0.1`, `--allow-private-webhooks`, `--allow-stdio`, so the fixtures are reachable; `--public-url` its edge, so signed calls are checked against its JWKS | Every area |
| B, strict | One server with both roles on Turso, with a deployed server's defaults: no internal MCP server, no private webhook, no stdio, no WorkOS | Egress and SSRF, hardening |
| C, local | The CLI and the embedded server it starts in a project directory | CLI |
| D, lifecycle | A deployed setup of its own, retention in seconds | Lifecycle |

`support/stack.ts` starts them; the global setup (`global-setup.ts`) starts A and B once per
invocation and publishes them in `results/release/stacks.json`.

## Writing a check

Specs import `test`, `expect` and the helpers from `support/gate.ts`.

- **One kind per check.** Each test carries exactly one tag: `@success` (a feature works end to
  end), `@failure` (a legitimate caller's mistake ends exactly as specified) or `@attack` (a hostile
  caller or script is refused, and nothing happens). The report counts them by area and kind.
- **Attacks prove the absence of an effect.** `expectRefused({ attempt, status, slug, probe })`
  probes the state before and after the attempt and asserts both the exact refusal and that nothing
  moved; a limit attack ends with `expectHealthy(tenant)`, a run that must still complete.
  `evidence("…")` says which defense was observed: the report lists every attack with it.
- **Everything is recorded.** `Api` (raw HTTP) and the SDK clients of `newTenant` record every
  exchange: the test's evidence, and `results/release/exchanges/` for the sweeps. A 5xx the check
  expects is declared first with `allow5xx(slug)`; any other fails the gate.
- **The canary.** `canary()` is a value only this run knows. Put it as a secret, of the
  organization or of a namespace, that runs attach (`secrets: { fx: "ns/fx" }`): the MCP server or
  the API receives it, and it must never come back in an answer (a run, a journal, a cassette, an
  event, a problem), a trace, a log, the database in clear, or the dashboard.
- **A tenant per file**, made by the operator with `newTenant(prefix, stack)`; tests are
  independent given their file's `beforeAll`.
- **The spec decides.** Expected behavior comes from `archi/` and `api/README.md`, including the
  decisions of `archi/08-decisions.md`. When Rewind deviates, the check keeps asserting the spec.

Run one file against fresh setups while writing it:

```sh
cd acceptance
REWIND_BIN=$PWD/../target/release/rewind REWIND_GATE_RESULTS=$PWD/results/dev REWIND_GATE_PROJECT=dev \
  pnpm exec playwright test -c playwright.release.config.ts --project=areas release/areas/runs.spec.ts
```

`REWIND_GATE_KEEP=1` keeps the setups after the run and prints their `REWIND_GATE_STACKS`, which
later runs reuse.

## Areas

| Area | File | Covers |
| --- | --- | --- |
| Runs | `areas/runs.spec.ts` | Starts, idempotency, signals, sleeps, cancel, TTL, forks, erasure, exports, the event stream, the change stream, traces |
| Endings and limits | `areas/endings.spec.ts` | How a script ends; CPU, heap, array buffers, commands, payload sizes |
| Host tools and tasks | `areas/host-tools.spec.ts` | Claims, heartbeats, deferrals, failures, retries, timeouts, approvals, task tokens |
| MCP, APIs, http tool, egress | `areas/mcp-egress.spec.ts` | MCP over HTTP and stdio; secrets attached per run (`org/`, `ns/`, never another's), deleted mid-run (`secret_missing`) or refused (401), put again with the same key; signed calls checked by `verifyRewindCall`, a replaced key, labels refused then approved; an API read from its OpenAPI document; schema changes, `fetch`, every refused address; no value in clear in the database |
| Workflows | `areas/workflows.spec.ts` | Versions, a version refused by its own tests or by its suite, starts by name, from scripts, schemas, label scope |
| Check and test | `areas/check-test.spec.ts` | `/v1/check`, `/v1/test`, refusals at start |
| Observability | `areas/observability.spec.ts` | Queries, statistics, failure groups, OTLP traces, signed webhooks, quota alerts |
| Access | `areas/access.spec.ts` | The permission matrix over every operation of `api/openapi.json` (the 25 permissions of `GET /v1/permissions`, `secrets:*` included), roles, grants, revocation, labels on secrets, `secrets:use` |
| Namespaces and tokens | `areas/namespaces.spec.ts` | The header, opaque and signed tokens, Alice and Bob, quotas, each end user's secrets (no fallback, a token's `secrets:use`), the dashboard's Secrets, erasure |
| Tenant isolation | `areas/tenancy.spec.ts` | Everything of one tenant probed from another, its change stream among them |
| Sandbox | `areas/sandbox.spec.ts` | Determinism, removed globals, imports, state across runs, exhaustion |
| API hardening | `areas/hardening.spec.ts` | Malformed and oversized input and secrets, paths, injections, idempotency keys, `/hooks`, cookies, `/auth` |
| CLI and local | `areas/cli.spec.ts` | Exit codes, `--json`, the embedded server, virtual time, replay, an API called over http with the secret `rewind secrets put` wrote and `--secret` attaches |
| Lifecycle | `lifecycle/lifecycle.spec.ts` | Worker killed, full restart, Postgres paused, archive, erasure |
| Sweeps | `sweeps/sweeps.spec.ts` | 5xx, panics, the canary (answers, problems, logs, the Collector's spans), problem types |

## Report

`results/release/` (not committed):

| File | Holds |
| --- | --- |
| `report.html` | The verdict, the stages, the matrix of areas and kinds, every failed check with its error and exchanges, every attack with the defense observed, every check. Self-contained |
| `report.md` | The same in short: the GitHub job summary |
| `report.json` | Everything, for tools |
| `*.log`, `stack-*/` | Each stage's output and each server's log |
| `html-*/` | Playwright's reports, with the traces of failed checks |
