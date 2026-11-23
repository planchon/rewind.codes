# The HTTP API `/v1`

`openapi.json` is the OpenAPI 3.1 document of the API that `rewind-server` serves
([07](../archi/07-code-architecture.md#http-api-v1)), of the dashboard's sign-in, `/auth`
([07](../archi/07-code-architecture.md#sign-in-auth)), and of the OAuth authorization server of
people's connections, `/oauth` and `/.well-known` ([07](../archi/07-code-architecture.md#mcp-and-oauth)). It is generated from the DTOs of
`crates/rewind-api` with `utoipa`. Do not edit it by hand.

## Regenerate it

After a change of a DTO, a path or an operation in `crates/rewind-api`:

```sh
REWIND_OPENAPI_WRITE=1 cargo test -p rewind-api --test openapi
```

Review the diff and commit it. Without the variable, the same test fails when the committed
file is stale, so CI catches a DTO changed without its document.

## Generate TypeScript types from it

The TypeScript SDK and the dashboard generate their types from this file with
[`openapi-typescript`](https://openapi-ts.dev) 7. From the root of the repository:

```sh
npx openapi-typescript@7 --empty-objects-unknown api/openapi.json -o sdk/typescript/src/generated/api.ts
npx openapi-typescript@7 --empty-objects-unknown api/openapi.json -o dashboard/src/generated/api.ts
```

`openapi-typescript` 7 prints its output through the TypeScript 5 compiler API, so it needs
`typescript@5` installed next to it, even in a package compiled with TypeScript 7. The output
has `paths` (one entry per operation, with its parameters, body and answers), `components`
(every DTO, under its Rust name) and `webhooks` (the outbound lifecycle events). It pairs with
`openapi-fetch` for a typed client. `--empty-objects-unknown` is required: without it every
`{"type": "object"}` schema (input schemas, cassettes, journals) becomes `Record<string, never>`.

Each package runs that command in a `generate:api` script and commits the output, so that a
change of the API shows in its diff.

## Conventions, in short

| Topic | Rule |
| --- | --- |
| Authentication | `Authorization: Bearer <token>` on every request to `/v1`: an API key `rewind_sk_…`, a member's access token from `/auth`, an end user's token: opaque `rewind_nt_…`, or a JWT the customer signed with a key it registered (`kid` `rk_…`), or a person's connection `rewind_ct_…`; none over the embedded server's socket (`.rewind/server.sock`) or to `POST /hooks/{token}` |
| Organization | `Rewind-Organization: org_…` names the organization a connection's request acts in: required on the tenant's data (`invalid-request` without it; only `GET /v1/whoami` answers), `forbidden` for an organization the connection does not open. Any other caller may repeat its own tenant; another is `forbidden` |
| OAuth | `/oauth` and `/.well-known` speak OAuth's formats, not the API's: snake_case fields, forms (`application/x-www-form-urlencoded`) at `/oauth/token` and `/oauth/revoke`, errors `{ "error", "error_description" }` in `application/json`, never cached; 404 (a problem) on a server without a public URL and WorkOS |
| Access | Every operation requires one permission of the catalog (`GET /v1/permissions`), held through the caller's grant and role, or given to a token; a missing one is `forbidden`, with a message that names it |
| Namespace | `Rewind-Namespace: <name>` names the end user a key's or a member's request acts for: required by `POST /v1/runs`, `/v1/check` and `/v1/test`, it narrows the reads to that namespace. A token's namespace is its own; a header naming another is `forbidden` |
| Sign-in | `/auth` is the dashboard's, for a browser: it reads and writes the cookie `rewind_session`, which `/v1` never reads; a `POST` takes a JSON body only (`Content-Type: application/json`, else `unsupported-media-type`); the steps answer an `AuthState` tagged by `status`; `GET /auth/callback` redirects to `/#/`, or to `/?auth_error=<code>#/` (`state_mismatch`, `provider_error`, `sign_in_failed`). A member's access token without organization reaches only `GET /v1/whoami`, `POST /v1/organizations`, `GET /v1/invitations` and `POST /v1/invitations/{id}/accept` (`forbidden` elsewhere) |
| JSON | camelCase fields, snake_case enumerated values, RFC 3339 times, durations `"30s"`, `"1h30m"` or a number of milliseconds |
| Runs | A run's `id` is its UUID; its `handle`, `<namespace>/<origin>/<suffix>` (`dana/triage/06bqx3k9f2mt`), names it too wherever a path or a query takes a run (`/v1/runs/{id}` and its sub-resources, `?parent=`, `?startedBy=`, `?run=`), percent-encoded in a path: `/v1/runs/dana%2Ftriage%2F06bqx3k9f2mt` |
| Workflow versions | `name` names the latest version the caller sees, `name@<version>` one by its digest (12 hex digits at least) and `name@v<number>` one by its number (`triage@v12`) |
| Idempotency | `Idempotency-Key` on `POST /v1/runs`, `POST /v1/runs/{id}/fork` and `POST /v1/runs/{id}/signals/{name}` |
| Errors | `application/problem+json`: `type` is `urn:rewind:problem:<slug>`, `detail` is written for a model, `invalid-script` adds `diagnostics`, `invalid-query` adds `query` |
| Query strings | lists repeat their key: `?label=agent:triage&label=conversation:c-42`, `?tool=a&tool=b`, `?tools=ask_user&tools=approve` |
| Event streams | Server-sent events, read with `fetch` by a browser (`EventSource` cannot send `Authorization`); while it sends nothing else, the server writes a comment line (`:`) at least every 15 s, which a client skips. `GET /v1/runs/{id}/events`: `event` is the `type` of the `StreamEvent` in `data`, `id` is the entry's index, resumed with `from` or `Last-Event-ID`. `GET /v1/changes`: `event` is `run` (`data`: the run's summary, as `GET /v1/runs` lists it), `script` (`{ digest }`), `workflow` (`{ name, version }`), `secret` (`{ name, namespace? }`), `namespace` (`{ name? }`, no name for the default limits) or `connection` (`{ id }`), or `resync` (`{}`: read again what is shown); no `id`, no resume. `GET /v1/tasks/stream`: `event` is `task` (`data`: the `HandedTask`, with its token) or `gone` (`{ token }`); no `id`; reopened after a drop with the same `worker` |
| Waits | `wait` on `POST /v1/runs`, `GET /v1/runs/{id}`, `POST /v1/runs/{id}/cancel` and `POST /v1/runs/{id}/fork` holds the one request until the run ends or is quarantined, or until `wait` passes, however long it is |
| Pages of runs | `GET /v1/runs` may answer fewer runs than `limit`, or none, with a `nextCursor`: a partition stopped at its scan budget. Only a page without `nextCursor` is the last |
| Statistics | `GET /v1/runs/stats`, `GET /v1/runs/failures`, `GET /v1/scripts` and `GET /v1/tools/stats` answer from hourly counters: a scope (the tenant, or one namespace: a token's, `Rewind-Namespace`, or a single `namespace:<name>` term of `q`) and a window of whole UTC hours (bounds rounded out, `bucket` rounded up to hours); `GET /v1/tools/stats` also takes `prefix`, the start of a tool's name. Any other term of `q` is `invalid-query`; a grant or a token limited to labels is `forbidden` |
| Payloads | `{ digest, bytes, preview, value? }`; `value` is left out above 256 KiB, read it with `GET /v1/payloads/{digest}?run=<id>` (`run` is required from every caller: a payload belongs to its run; 400 without it, 404 when the run's history does not hold it) |
| Webhooks | Standard Webhooks: headers `webhook-id`, `webhook-timestamp`, `webhook-signature` (`v1,<base64 HMAC-SHA256>`), secret `whsec_…`; every event about a run names its `namespace`; `quota.alert` reports a limit crossed |
| Secrets | `secret-required` adds `attachments` |
| Quotas | `quota-exceeded` adds `quota`, `scope` (`namespace` or `organization`), `limit`, `used` and `resetsAt`, with `Retry-After` for a monthly quota |

The problem types:

| Slug | Status | When |
| --- | --- | --- |
| `invalid-request` | 400 | A missing or invalid field, header or parameter; a webhook URL the server does not send to |
| `invalid-query` | 400 | A query (`q`) that cannot be read, or, on a statistics route, a term other than one `namespace:` of one value; `query` gives the position of the term at fault |
| `unauthorized` | 401 | No token, or an unknown, revoked or expired one; a signed token whose key, signature, audience, lifetime or permissions do not hold; on `/auth`, no session |
| `forbidden` | 403 | The caller's grant does not hold the permission the request needs (the message names it), or its allowed labels do not cover the request; a grant or a token limited to labels on a statistics route, since counters do not know labels; a member without a grant in the organization |
| `not-found` | 404 | No such run, toolset, script, task, payload, subscription, key, role, member or invitation |
| `run-ended` | 409 | A signal or a cancellation to a run that has ended |
| `not-quarantined` | 409 | Releasing or giving up a run that is not quarantined |
| `task-not-waiting` | 409 | Approving or rejecting a task that does not wait for the harness |
| `role-in-use` | 409 | Deleting a role that a grant still points to |
| `namespace-deleting` | 409 | Starting a run in, or minting a token for, a namespace being erased |
| `last-owner` | 409 | Changing or removing a grant that would leave the organization without an `owner` |
| `task-gone` | 410 | The task no longer exists: its run ended, it was answered, or it was handed again after the stream it was handed on stayed closed past its grace |
| `payload-too-large` | 413 | A body or payload above the server's limits; a host-tool failure above 64 KiB |
| `unsupported-media-type` | 415 | A body that is not JSON where JSON is expected |
| `invalid-script` | 422 | The script was refused before any run exists |
| `invalid-toolset` | 422 | An invalid schema, a duplicate tool, a transport this server refuses |
| `invalid-workflow` | 422 | A workflow's name, a signal's name or a schema that cannot be used |
| `invalid-test` | 422 | A test or a suite refused: an invalid test, a stored test that does not exist, a retired or updated test that is not inherited or has no reason, two tests of the same name |
| `tests-failed` | 422 | Tests did not pass: the version was not saved, or the fork not created; `tests` holds each test's outcome |
| `secret-required` | 422 | A run, a fork or a workflow version refused before it exists for its secrets: a server or an API the script imports takes one and none is attached, an attachment names a server that takes none or a secret that does not exist, or the caller may not attach it; also a toolset's listing secret. `attachments` gives each one at fault (`server`, `problem`, `secret`) and what the caller may attach (`attachable`) |
| `invalid-input` | 422 | The input of a run of a workflow does not match its input schema |
| `invalid-signal` | 422 | A signal name starting with `rewind:`; for a run of a workflow, a signal it does not declare or a payload that does not match |
| `invalid-title` | 422 | A script's title that is empty or longer than 120 characters, or a description longer than 2,000 (`PATCH /v1/scripts/{digest}`) |
| `quota-exceeded` | 429 | The namespace or the tenant reached its `activeRuns`, `runsPerMonth` or `scripts` |
| `internal` | 500 | A fault of the platform |
| `toolset-unavailable` | 502 | An MCP server of the toolset cannot be reached |
| `identity-unavailable` | 502 | The identity provider (WorkOS) cannot be reached, or answered with an error |
| `unavailable` | 503 | The server cannot serve the request now |
