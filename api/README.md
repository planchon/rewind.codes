# The HTTP API `/v1`

`openapi.json` is the OpenAPI 3.1 document of the API that `rewind-server` serves
([07](../archi/07-code-architecture.md#http-api-v1)), and of the dashboard's sign-in, `/auth`
([07](../archi/07-code-architecture.md#sign-in-auth)). It is generated from the DTOs of
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
| Authentication | `Authorization: Bearer <token>` on every request to `/v1`: an API key `rewind_sk_…`, a member's access token from `/auth`, or an end user's token: opaque `rewind_nt_…`, or a JWT the customer signed with a key it registered (`kid` `rk_…`); none over the embedded server's socket (`.rewind/server.sock`) or to `POST /hooks/{token}` |
| Access | Every operation requires one permission of the catalog (`GET /v1/permissions`), held through the caller's grant and role, or given to a token; a missing one is `forbidden`, with a message that names it |
| Namespace | `Rewind-Namespace: <name>` names the end user a key's or a member's request acts for: required by `POST /v1/runs`, `/v1/check` and `/v1/test`, it narrows the reads to that namespace. A token's namespace is its own; a header naming another is `forbidden` |
| Sign-in | `/auth` is the dashboard's, for a browser: it reads and writes the cookie `rewind_session`, which `/v1` never reads; a `POST` takes a JSON body only (`Content-Type: application/json`, else `unsupported-media-type`); the steps answer an `AuthState` tagged by `status`; `GET /auth/callback` redirects to `/#/`, or to `/?auth_error=<code>#/` (`state_mismatch`, `provider_error`, `sign_in_failed`). A member's access token without organization reaches only `GET /v1/whoami`, `POST /v1/organizations`, `GET /v1/invitations` and `POST /v1/invitations/{id}/accept` (`forbidden` elsewhere) |
| JSON | camelCase fields, snake_case enumerated values, RFC 3339 times, durations `"30s"`, `"1h30m"` or a number of milliseconds |
| Idempotency | `Idempotency-Key` on `POST /v1/runs`, `POST /v1/runs/{id}/fork` and `POST /v1/runs/{id}/signals/{name}` |
| Errors | `application/problem+json`: `type` is `urn:rewind:problem:<slug>`, `detail` is written for a model, `invalid-script` adds `diagnostics`, `invalid-query` adds `query` |
| Query strings | lists repeat their key: `?label=agent:triage&label=conversation:c-42`, `?tool=a&tool=b` |
| Event stream | `GET /v1/runs/{id}/events` is server-sent events: `event` is the `type` of the `StreamEvent` in `data`, `id` is the entry's index |
| Payloads | `{ digest, bytes, preview, value? }`; `value` is left out above 256 KiB, read it with `GET /v1/payloads/{digest}?run=<id>` (`run` is required with a token, a `Rewind-Namespace` header, or a grant limited to labels) |
| Webhooks | Standard Webhooks: headers `webhook-id`, `webhook-timestamp`, `webhook-signature` (`v1,<base64 HMAC-SHA256>`), secret `whsec_…`; every event about a run names its `namespace`; `quota.alert` reports a limit crossed |
| Quotas | `quota-exceeded` adds `quota`, `scope` (`namespace` or `organization`), `limit`, `used` and `resetsAt`, with `Retry-After` for a monthly quota |

The problem types:

| Slug | Status | When |
| --- | --- | --- |
| `invalid-request` | 400 | A missing or invalid field, header or parameter; a webhook URL the server does not send to |
| `invalid-query` | 400 | A query (`q`) that cannot be read; `query` gives the position of the term at fault |
| `unauthorized` | 401 | No token, or an unknown, revoked or expired one; a signed token whose key, signature, audience, lifetime or permissions do not hold; on `/auth`, no session |
| `forbidden` | 403 | The caller's grant does not hold the permission the request needs (the message names it), or its allowed labels do not cover the request; a member without a grant in the organization |
| `not-found` | 404 | No such run, toolset, script, task, payload, subscription, key, role, member or invitation |
| `run-ended` | 409 | A signal or a cancellation to a run that has ended |
| `not-quarantined` | 409 | Releasing or giving up a run that is not quarantined |
| `task-not-waiting` | 409 | Approving or rejecting a task that does not wait for the harness |
| `role-in-use` | 409 | Deleting a role that a grant still points to |
| `namespace-deleting` | 409 | Starting a run in, or minting a token for, a namespace being erased |
| `last-owner` | 409 | Changing or removing a grant that would leave the organization without an `owner` |
| `task-gone` | 410 | The task no longer exists: its run ended, it was answered, or its claim was lost |
| `payload-too-large` | 413 | A body or payload above the server's limits; a host-tool failure above 64 KiB |
| `unsupported-media-type` | 415 | A body that is not JSON where JSON is expected |
| `invalid-script` | 422 | The script was refused before any run exists |
| `invalid-toolset` | 422 | An invalid schema, a duplicate tool, a transport this server refuses |
| `quota-exceeded` | 429 | The namespace or the tenant reached its `activeRuns`, `runsPerMonth` or `scripts` |
| `internal` | 500 | A fault of the platform |
| `toolset-unavailable` | 502 | An MCP server of the toolset cannot be reached |
| `identity-unavailable` | 502 | The identity provider (WorkOS) cannot be reached, or answered with an error |
| `unavailable` | 503 | The server cannot serve the request now |
