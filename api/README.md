# The HTTP API `/v1`

`openapi.json` is the OpenAPI 3.1 document of the API that `rewind-server` serves
([07](../archi/07-code-architecture.md#http-api-v1)). It is generated from the DTOs of
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
| Authentication | `Authorization: Bearer rewind_sk_…` on every request; none over the embedded server's socket (`.rewind/server.sock`) or to `POST /hooks/{token}` |
| JSON | camelCase fields, snake_case enumerated values, RFC 3339 times, durations `"30s"`, `"1h30m"` or a number of milliseconds |
| Idempotency | `Idempotency-Key` on `POST /v1/runs`, `POST /v1/runs/{id}/fork` and `POST /v1/runs/{id}/signals/{name}` |
| Errors | `application/problem+json`: `type` is `urn:rewind:problem:<slug>`, `detail` is written for a model, `invalid-script` adds `diagnostics`, `invalid-query` adds `query` |
| Query strings | lists repeat their key: `?label=agent:triage&label=conversation:c-42`, `?tool=a&tool=b` |
| Event stream | `GET /v1/runs/{id}/events` is server-sent events: `event` is the `type` of the `StreamEvent` in `data`, `id` is the entry's index |
| Payloads | `{ digest, bytes, preview, value? }`; `value` is left out above 256 KiB, read it with `GET /v1/payloads/{digest}?run=<id>` (`run` is required with a key limited to labels) |
| Webhooks | Standard Webhooks: headers `webhook-id`, `webhook-timestamp`, `webhook-signature` (`v1,<base64 HMAC-SHA256>`), secret `whsec_…` |

The problem types:

| Slug | Status | When |
| --- | --- | --- |
| `invalid-request` | 400 | A missing or invalid field, header or parameter; a webhook URL the server does not send to |
| `invalid-query` | 400 | A query (`q`) that cannot be read; `query` gives the position of the term at fault |
| `unauthorized` | 401 | No API key, or an unknown or revoked one |
| `forbidden` | 403 | The key's role or allowed labels do not cover the request |
| `not-found` | 404 | No such run, toolset, script, task, payload, subscription or key |
| `run-ended` | 409 | A signal or a cancellation to a run that has ended |
| `not-quarantined` | 409 | Releasing or giving up a run that is not quarantined |
| `task-not-waiting` | 409 | Approving or rejecting a task that does not wait for the harness |
| `task-gone` | 410 | The task no longer exists: its run ended, it was answered, or its claim was lost |
| `payload-too-large` | 413 | A body or payload above the server's limits; a host-tool failure above 64 KiB |
| `unsupported-media-type` | 415 | A body that is not JSON where JSON is expected |
| `invalid-script` | 422 | The script was refused before any run exists |
| `invalid-toolset` | 422 | An invalid schema, a duplicate tool, a transport this server refuses |
| `invalid-workflow` | 422 | A workflow's name, a signal's name or a schema that cannot be used |
| `invalid-input` | 422 | The input of a run of a workflow does not match its input schema |
| `invalid-signal` | 422 | A signal name starting with `rewind:`; for a run of a workflow, a signal it does not declare or a payload that does not match |
| `quota-exceeded` | 429 | The tenant has its maximum of active runs |
| `internal` | 500 | A fault of the platform |
| `toolset-unavailable` | 502 | An MCP server of the toolset cannot be reached |
| `unavailable` | 503 | The server cannot serve the request now |
