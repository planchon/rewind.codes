# 4. Actions

## Routes

What a workflow calls is an **action**. An action set has four kinds of actions:

| Route | Called by | Used for |
| --- | --- | --- |
| MCP actions (`mcp`) | The dispatcher of Rewind, an MCP client (`rmcp`, stdio or streamable HTTP) | Every tool of the action set's MCP servers |
| API actions (`api`) | The dispatcher of Rewind, an HTTP client | Every operation of the action set's APIs, each described by an OpenAPI document ([API actions](#api-actions)) |
| Built-ins: `http`, `signalUrl`; `workflows.<name>.start` and `workflows.<name>.signal` for each workflow the action set pins | The dispatcher of Rewind; the engine itself for `signalUrl` and the workflows' | `http` is the only way a workflow reaches the network ([egress](#the-http-action-and-egress)); `signalUrl(name)` returns a URL bound to the run and the signal name ([signals](01-product.md#signals)); the workflows' start and signal runs of [workflows](03-execution.md#workflows-across-runs), in the namespace of the run that calls them |
| Harness actions (`harness`) | The harness's worker, which holds a call stream open (`GET /v1/calls/stream`) and is handed the calls as they become ready; each call names the namespace of its run, the end user the handler acts for. A worker is handed only the calls of the runs its key or token sees: an end user's namespace, a grant's labels ([access](06-storage-and-scale.md#tenancy-namespaces-identity-and-access)) | Functions that live in the harness: ask the end user, call another LLM, start a sub-agent. A customer's backend that pulls its calls instead of receiving them, as an API's are: nothing reaches it from outside, and it needs no signature to check (Dax) |

An action's prefix is its **group**: `github.list_issues` is the action `list_issues` of the
group `github`, an MCP server, an API or a set of harness actions.

Rewind calls MCP actions, API actions and built-ins directly. Their results go back to the
workflow's code, never to the agent. The harness is on the path only for its harness actions, and
they are optional: an action set may have only MCP servers or APIs. There is no other kind of
action in 0.1.

Every call carries an **idempotency key** `"{run_id}/{seq}"`, passed in MCP `_meta`
(`codes.rewind/idempotency-key`), to an API in the `Idempotency-Key` header when its operation
declares one, and to harness action handlers, with the number of the delivery (`ctx.delivery`).
Calls also carry the W3C trace context (`traceparent`, `tracestate`), in `_meta` for MCP and in
headers for an API, with the Call span as parent, so the server's own spans join the run's trace.
How Rewind proves who calls, a secret or its own signature, is in
[authenticating calls](#authenticating-calls).

This key protects the way out. The way in (starting a run, sending a signal) has its
own keys: see [storage](06-storage-and-scale.md#idempotency-of-the-api). A fork's calls
carry other keys: see [forks](03-execution.md#forks).

A harness action can last days. The most complete case, a harness action that outlasts the run's
`wait`:

1. The agent starts the run with `POST /v1/runs` and `wait=30s`. The workflow calls `ask_user`.
2. The harness's worker holds `GET /v1/calls/stream?actions=ask_user` open, and is handed the
   call on it (a `call` message), with a token and the idempotency key.
3. While it works, its stream stays open: the open stream is the liveness of every call handed
   on it.
4. After 30 s, the agent gets the run `waiting`, for the call `ask_user` (`claimed`).
5. When it has the answer, even days later, the worker sends
   `POST /v1/calls/{token}/complete`. The completion goes to the run's inbox, the run resumes
   and ends, and the harness gets the signed webhook `run.completed`.

## Call lifecycle

Each `schedule_call` makes a **call**, executed outside the isolate. Under the hood, the
dispatcher carries it as a **task**: deliveries, backoff, approvals and tokens live in the task,
kept with its run in the run's partition, not in the history. The history only gets the final
outcome.

The task is created in the batch that commits its call, and nothing is sent before that batch
is durable. Then the run's owner sends it on its way. An MCP or API call waits for a slot: the
owner checks the per-run limit itself, and asks the tenant actor for a slot of the server's or
the API's limit, which is the organization's ([policies](#policies)); with the slot, the owner's
node sends the call. A harness action's call goes to the tenant actor, which hands it to a
worker's open stream with a free slot, each hand-out with a token of its own
([inside a node](07-code-architecture.md#inside-a-node)). The token names the run, so an answer
reaches the run's owner from whichever process receives it. A token whose call no longer waits
for it (its run ended, it was answered, or it was handed out again after its stream stayed closed
past its `reconnectTimeout`) is answered 410 `call-gone`.

One state machine covers every route. The states are the call's, and the API says so:

| State | The call | Leaves it |
| --- | --- | --- |
| `approval` | Waits for the harness. Only with the `approval` policy | approve → `ready`; reject → `done`, the call fails with an `ActionError` |
| `ready` | Waits for its next delivery (`next_delivery_at`, a timer after a backoff) and a concurrency slot | sent or handed out → `claimed` |
| `claimed` | Delivery n, in flight: sent by the owner's node, or handed to a worker's stream | See the next table |
| `deferred` | Holds a token until the handler answers | `/complete` or `/fail` → `done`; a timeout, bounded by the run's TTL and kept as a timer of the run's partition → `done` |
| `parked` | Waits for the harness after a schema change, a refused call or a missing secret | continue, or the secret put again → `ready`; cancel → `done` |
| `done` | Its completion is in the run's inbox: a result, a failure, or `outcomeUnknown` | |

From `claimed`:

| What happens | Next state |
| --- | --- |
| The action answered | `done`, with its result or its failure |
| Transient failure (for a harness action, a `/fail` with `retryable: true`) | `ready` with backoff (`idempotent: true`); `done` with the failure (`idempotent: false`) |
| The call was interrupted: the node that sent it died, or the worker's stream closed and did not come back within its `reconnectTimeout` (`previous_claim_expired` on the next hand-out) | `ready` with delivery + 1, up to 5 deliveries, then `done` with `outcomeUnknown` (`idempotent: true`); `done` with `outcomeUnknown` at once (`idempotent: false`) |
| The handler answers later | `deferred` |
| The schema changed at dispatch | `parked`, and the harness gets the webhook `action.schema_changed` ([action set change](#when-an-action-set-changes-during-a-run)) |
| The server answered 401 | `parked`, and the harness gets the webhook `secret.expired` with the reason `refused` ([authenticating calls](#authenticating-calls)) |
| The secret the run uses for the server no longer exists | `parked` before anything is sent, and the harness gets `secret.expired` with the reason `missing` |

The harness answers a call in `approval`, and a call `parked` by a schema change or a refused
call, with `POST /v1/calls/{id}/approve` (→ `ready`) or `POST /v1/calls/{id}/reject` (→ `done`,
the call fails with an `ActionError`); a call in neither state answers 409 `call-not-waiting`.
Putting a secret again makes `ready` the calls parked by a refusal or a missing secret in the
runs that use it: the write is a change of the control plane, which every owner receives and
applies to its runs.

A call decides its run's status ([run state](02-history.md#run-state)). An MCP, API or `http`
call in flight or backing off is Rewind's own work: its run is `running`. A harness action's call
(`ready` or `claimed`), and a call in `approval`, `deferred` or `parked`, waits for the outside
world: once nothing else moves the run, it is `waiting`, and its `waitingFor` names the call and
its state.

Sending a call or handing it out writes nothing: the task's records say `ready` while it is in
flight, and only the owner's memory knows it left. What is written is what happened to it: its
creation, a backoff, an interruption, its outcome. Nothing is written while a call is in flight
either: no heartbeat, no renewal.

A call is interrupted when the node that sent it dies. The node that takes the partition over
cannot know which of the run's `ready` calls had left, so it treats each as interrupted: under
`idempotent: true` it sends it again with delivery + 1, written in its recovery batch; under
`idempotent: false` it fails it with `outcomeUnknown`. A harness action's call is interrupted when
the stream it was handed on closes. The tenant actor waits for that worker to open a stream
again, for the policy's `reconnectTimeout`, and then the owner writes delivery + 1 and the actor
hands the call to another worker, or the owner fails it under `idempotent: false`.

When a run ends, its tasks are deleted: a call still queued is never sent. Results arriving
for a run that already ended are dropped.

The backoff curve between deliveries (`RedeliveryBackoff`) is an implementation choice, recorded
in 08.

## Policies

Each action has a policy, pinned in the action set. The harness sets it per action when it
registers the action set.

| Setting | Values | Default |
| --- | --- | --- |
| `idempotent` | `true`: an interrupted call is delivered again, with the same idempotency key; `false`: a call is delivered once at most | `true`; for an API operation `POST` or `PATCH`, `false` unless it declares an `Idempotency-Key` header ([API actions](#api-actions)) |
| `approval` | The call waits for the harness before it is sent | Off |
| Execution timeout | A duration | 5 min for MCP and API actions |
| `reconnectTimeout` | A duration: how long a harness action's call handed on a stream that closed waits for its worker to come back | 60 s for harness actions |
| Concurrency | Calls in flight | 16 per run; 8 per MCP server or API in the organization |

| Policy | On transient error | Interrupted (outcome unknown) |
| --- | --- | --- |
| `idempotent: true` | Redelivered after a backoff | Redelivered with the same idempotency key, up to 5 deliveries, then fails with `outcomeUnknown: true` |
| `idempotent: false` | Fails | Fails with `outcomeUnknown: true` |

An interrupted call is delivered again with the same idempotency key, so the action can
recognize it and not repeat its effect. An action that ignores the key may then run twice: such
an action is declared `idempotent: false` in the action set. Every count of deliveries says so: a
run's `deliveries`, an action's `redeliveries` in its statistics.

The `approval` policy lets the harness keep its permission gates: a workflow cannot call such an
action without the harness's consent.

Concurrency limits are invisible to the history: a call waiting for a slot stays `ready`. The
limit per run is kept by the run's owner. The limit of a server or an API holds across the
organization: its slots belong to the tenant actor, which grants them in the order they are
asked for.

A workflow's code sees failures as a thrown `ActionError` and decides what to do.

## Action sets and code generation

An action set is a list of MCP servers, APIs and harness actions, with a policy per action, and
of the [workflows](01-product.md#workflows) its runs may start. Registering it
(`POST /v1/action-sets`) connects to the MCP servers and lists their tools, reads the APIs'
OpenAPI documents, and **pins** the schemas, and the latest version of each organization workflow
it names (or the version it names: `triage@v12`, `triage@<digest>`). The pinned action set is a
document of the [stored format](02-history.md#stored-format-v1): its servers and APIs, with how
each one authenticates its calls ([authenticating calls](#authenticating-calls)), for each action
its name, route, schemas and policy, and for each workflow its name, the version it pins (its
number and digest), its description and schemas. The action set is content-addressed, and every
run records the digest it was started with. An action set can also be derived from a pinned one
(`base`): its servers and actions are kept as pinned, and the workflows it names are added or
pinned again, without listing the servers again.

The workflows an action set names are the organization's: they are generic, and any namespace's
run executes them ([workflows](01-product.md#workflows)). A run started through
`rewind:workflows` starts the version its action set pins. A namespace's fork of one is what that
namespace's runs and its harness get when they name the workflow; an import pinned by an action
set keeps its pinned version.

An action set holds no secret. A run reads a secret only when its start names it, and secrets
do not depend on the action set: an action set registered again with one more action keeps
calling with the same secrets.

Each pinned workflow gives two built-in actions, `workflows.<name>.start` and
`workflows.<name>.signal`, with the default policy of built-ins. They are not part of the
document: the workflow is.

From each pinned action, `rewind-codegen` generates three outputs. For `github.list_issues`:

| Output | Read by | Example |
| --- | --- | --- |
| The `rewind:actions` runtime module: thin wrappers over the call op | The runtime | `list_issues: (args) => call("github.list_issues", args)` |
| `rewind-actions.d.ts`: types for args (from `inputSchema`) and results (from `outputSchema` when present, otherwise the MCP content type with `.text()` and `.json()` helpers), with action descriptions as JSDoc | tsgo | `list_issues(args: { repo: string; state?: "open" \| "closed" }): Promise<Issue[]>` |
| A **compact prompt rendering** of the same declarations | The model | `github.list_issues({ repo, state? }) → Issue[] · List the issues` |

For each pinned workflow, `rewind-actions.d.ts` also declares the module `rewind:workflows`, and
the prompt rendering gives it a line:

| Output | Example |
| --- | --- |
| `rewind-actions.d.ts` | `export const triage: Workflow<{ repo: string }, { closed: number }, { approval: { close: boolean } }>` |
| Prompt rendering | `triage.start({ repo: string }) → run · run.signal("approval", { close: boolean }) · run.result() → { closed: number } · Triage…`, after the line `import { triage } from "rewind:workflows";` |

Codegen is deterministic. Without it, nothing could check a workflow's code before it runs: tsgo
would not know that `repo` is a string. With it, `github.list_issues({ repo: 42 })` is refused at
start, before any run exists: `Type 'number' is not assignable to type 'string'
(workflow.ts:3:28)`.

Action names are kept as the server names its tools (`github.list_issues`), and an API's as its
document names its operations (`dax.createMeasure`), so they match the descriptions the model
reads. The names `rewind:actions` declares, a group (a server, an API, a group of harness actions)
and a harness action without a group, are identifiers that are not reserved words of JavaScript
(`default`, `delete`, `class`…), nor the words strict mode reserves (`let`, `static`, `eval`…):
an action set naming one is refused when it is registered. So are the names of workflows, which
`rewind:workflows` declares. The group `workflows` is reserved for their built-ins: no server,
API or group of harness actions takes it.

Args are also validated at run time, against the same JSON Schema, inside the op, before any
command is recorded. Invalid args throw a `TypeError` at the call site. This is deterministic,
since the schema is pinned.

The JSON Schema → TypeScript converter is ours, written in Rust in `rewind-codegen`: no
maintained crate exists. It covers objects, `required`, enums, `anyOf`/`oneOf`/`allOf`,
`$ref`/`$defs`, arrays and `additionalProperties`.

### When an action set changes during a run

A run keeps the action set it started with, but the server behind it can change during a long
pause: a tool renamed, removed, or with a new schema. The dispatcher checks each MCP call against
the server's current schema at dispatch. An API has no listing to ask: its pinned document is
the contract, and a call that the API no longer accepts fails with its 4xx. A call that no longer
matches is not failed silently: it is `parked`, and the harness gets the webhook
`action.schema_changed`. The agent or the developer decides what happens to the run:

- continue against the new schema: `approve`, and the call is `ready` again; the dispatcher
  then sends it without checking the schema again;
- [fork](03-execution.md#forks) it with corrected code;
- cancel it, or `reject` the call, which fails with an `ActionError`.

### Observed result types

Most MCP servers declare no `outputSchema`, so the model would write code against a
result it cannot see. But every result ever returned is in a history. A projection
infers, per action, the **shape observed** in recorded results, and codegen uses it when
the server declares nothing.

- Shapes only: keys and types, never values. Observations stay within a tenant.
- A field missing from some results is optional. An MCP action never observed keeps the MCP
  content type and its helpers; an API action never observed returns `unknown`.
- The declaration says it is observed, not declared (`@observed` in its JSDoc), so the
  model knows it may be incomplete.
- Observed types serve type-checking and the prompt only. Results are never validated
  against them, and they are not part of the action set's digest, so replay does not depend
  on them.

## API actions

An action set names its APIs in `apis`, each with:

| Field | Meaning |
| --- | --- |
| `name` | Its group in `rewind:actions`, an identifier as a server's name is |
| `openapi` | Its OpenAPI 3.0 or 3.1 document, in JSON: given in the request, or its URL. A URL is read once, at registration, through the [egress checks](#where-mcp-servers-and-apis-may-be), at most 2 MiB; the document is then pinned and never read again |
| `baseUrl` | Where the calls go: the document's first `servers[]` unless given. `https`, or `http` on the embedded server |
| `auth` | How its calls prove who calls ([authenticating calls](#authenticating-calls)); the document's security requirement when left out |
| `operations` | Optional: the `operationId`s to keep, for an API too large for the prompt |
| `concurrency` | Its calls in flight across the organization, 8 by default |

From the document:

- **One action per operation**, named by its `operationId`, or from its method and path when it
  has none (`post_v1_measures`). A name that is not a valid action name, or two operations with
  the same name, refuse the registration (`invalid-action-set`), naming the operation's path.
- **Its input schema** is an object: the path, query and header parameters by their names, and
  the JSON request body under `body`. A name two parameters share, or a parameter named `body`,
  refuses the registration. The header `Idempotency-Key` and the headers of the authentication
  are not parameters: Rewind fills them.
- **Its output schema** is the schema of its first `2xx` response in `application/json`; without
  one, the action gets an [observed](#observed-result-types) type.
- `$ref` is resolved within the document (`#/components/…`); a reference to another document
  refuses the registration. `summary` and `description` become the action's description.
- An operation with a body other than `application/json`, or with a cookie parameter, is left
  out, and the registration's answer lists it (`skipped`).

A call:

1. The dispatcher builds the request from the args: the path parameters percent-encoded into
   the path, the query, the headers, the body as JSON. It adds `traceparent` and `tracestate`,
   `Idempotency-Key: {run_id}/{seq}` when the operation declares that header, and the
   [authentication](#authenticating-calls).
2. The request goes through the [egress checks](#where-mcp-servers-and-apis-may-be).
3. The answer decides the outcome:

| Answer | The call |
| --- | --- |
| `2xx` | Succeeds: the JSON body, `null` for an empty one, `{ contentType, text }` for another type; bounded to 5 MB |
| `401` | Is `parked` ([lifecycle](#call-lifecycle)) |
| `408`, `429`, `5xx`, a failed connection | Is transient: redelivered with backoff under `idempotent: true`, never before its `Retry-After` |
| Another status | Fails with an `ActionError` holding the status, and the body's `title` and `detail` when it is `application/problem+json`, else its first 2 KB |

The default policy follows the method: `idempotent: true` for `GET`, `HEAD`, `OPTIONS`, `PUT` and
`DELETE`, which HTTP defines as idempotent; `idempotent: false` for `POST` and `PATCH`, unless the
operation declares an `Idempotency-Key` header, by which the API recognizes a call sent again. The
action set may set another policy per action, as for any action.

An API suits calls answered within the execution timeout. A call that waits days, for a person's
answer, is a harness action, or the workflow waits for a signal
([`signalUrl()`](01-product.md#signals)).

## Authenticating calls

Each MCP server and API of an action set says, in its `auth`, how its calls prove who calls.
`auth` is part of the pinned action set; no value is.

| `auth` | The request carries | For |
| --- | --- | --- |
| none | Nothing | Public servers |
| `{ "type": "rewind" }` | A JWT signed with the tenant's key ([signed calls](#signed-calls)), in `Authorization: Bearer` | The customer's own APIs and MCP servers: they check the signature and learn the end user |
| `{ "type": "secret", "header": "<name>", "format"?: "Bearer {secret}" }` | The [secret](#secrets) the run uses for that server, in that header, `{secret}` replaced by its value; the value alone without `format` | Other servers: GitHub, Linear, an API with a key |
| `{ "type": "secret", "env": "<VARIABLE>" }` | The secret the run uses, in that variable of the process | A stdio MCP server |

A secret never goes in a URL. Without `auth`, an API follows its document's security
requirement: `http` `bearer`, `oauth2` and `openIdConnect` are `Authorization: Bearer {secret}`;
`http` `basic` is `Authorization: Basic` with the secret (`user:password`) in base64; `apiKey` in
a header is that header with the secret. An `apiKey` in the query or a cookie, or a requirement
of several schemes at once, refuses the registration unless `auth` is given.

The `http` built-in carries no secret and no signature: a call that needs one is an API action.

### Secrets

A **secret** is a named value, encrypted at rest, that only the dispatcher reads, for the runs
that use it.

| | An organization's | A namespace's |
| --- | --- | --- |
| Belongs to | The tenant, shared by every end user | One end user: Alice's GitHub token |
| Named by a run as | `org/<name>` | `ns/<name>`: the secret `<name>` of the run's own namespace |
| Labels | Its own: a grant limited to labels sees, writes and uses only the secrets carrying them, and the secrets it writes take them | None: the namespace is its scope |
| Written with | `PUT /v1/secrets/{name}` | `PUT /v1/namespaces/{namespace}/secrets/{name}` |

- A name is 1 to 64 lowercase letters, digits, `-` and `_`, starting with a letter. A value is a
  string of at most 16 KiB.
- **Write-only.** No endpoint answers a value. A secret is listed with its metadata: its name, its
  labels, who wrote it and when, its reads per day, its last read (run, namespace, date; an
  action set's registration, which reads an organization's secret to list the tools of the MCP
  server it is named for before any run exists, names no run)
  ([07](07-code-architecture.md#http-api-v1)).
- **Stored** in the `SecretStore`, encrypted with a data key per tenant, itself encrypted by a
  master key from the deployment's configuration.
- **The master key can be replaced.** Each data key records which master key encrypted it, by
  the key's id (the first 8 bytes of its SHA-256, in hex). The operator gives the new key as
  `--master-key` and the old one in `--previous-master-keys` (`REWIND_PREVIOUS_MASTER_KEYS`),
  which only decrypt, then runs `rewind server --rewrap-data-keys`: it encrypts each data key
  held under a previous key with the current one, one tenant at a time, prints how many it
  changed, and exits. The secrets and the private keys are not encrypted again. A server refuses
  to start (exit 70) while a data key is held under a master key it was not given, naming its id,
  so the old key cannot be removed too early.
- Secrets never enter the history, the isolate, a cassette, OpenTelemetry, nor an answer of the
  API.

**Using.** A run reads a secret only if its start names it, server by server, in the request
that gives the code, never in the code: `secrets: { "github": "ns/github",
"zendesk": "org/zendesk" }`.

| Where | The secrets used |
| --- | --- |
| `POST /v1/runs` with code | The request's |
| `POST /v1/runs` with a workflow, and `workflows.<name>.start` | The version's, given when the version was made (`POST /v1/workflows`, `POST /v1/workflows/{id}/versions`), in its document and so in its digest; for a version 1 that a run created, that run's. The request gives none |
| `POST /v1/runs/{id}/fork` | The source's, unless the request gives its own |
| `POST /v1/action-sets` | An organization's secrets only, to list the tools of an MCP server that needs one; no run keeps them |

The start is refused before any run exists (422 `secret-required`, with a message for a model
that lists the secrets the caller may use for that server, and `secrets`, one `SecretProblem`
per server at fault) when:

- the code imports a server or an API whose `auth` is `secret`, and the start names none for it;
- the start names a secret for a server that takes none, or a secret that does not exist (an
  `ns/` secret in the run's namespace);
- the caller may not use it.

Who may use:

- **A namespace's secret**: whoever starts a run in that namespace. The secret is the end
  user's own, and only that namespace's runs read it.
- **An organization's secret**: who holds `secrets:use`, for a secret whose labels are within
  the grant's. An end user's token only if it was given `secrets:use`; a connection with the
  access "Run", within its person's grant.
- **A workflow version is the permission.** Making a version checks the secrets it names against
  the rights of whoever makes it, a fork's and a pull's included: an organization's secret must
  exist and be theirs to use; an `ns/` secret has no namespace yet, and only its name is checked.
  A version 1 that a run created was checked with that run's start. Starting a run of a version,
  from the API or from a workflow's code, checks nothing more than that its `ns/` secrets exist in
  the run's namespace: its code is fixed and its suite passed. Code given to a run, and a fork of a
  run, whose code is new, are checked against their caller.

**Reading.** At each call to a server whose `auth` is `secret`, the dispatcher reads the secret
the run uses for it, in the run's namespace for an `ns/` one, injects it into the transport
and counts the read. A secret deleted since parks the call (`secret.expired`, reason
`missing`); a 401 parks it too (reason `refused`). Putting the secret again under the same name
makes those calls `ready`, delivered with the same idempotency key. Nothing falls back on another
secret: Alice's run that uses `ns/github` never calls with the organization's token, nor
with Bob's.

**Who reads what.** The secrets a run uses are run metadata, and those of a version part of its
document. Before a deletion, the dashboard and `GET …/secrets/{name}/usage` say which active runs
and which workflows read the secret, and how often it was read.

### Signed calls

With `auth: { "type": "rewind" }`, Rewind signs each request with the tenant's key, and the
server checks it with the public key, as any JWT library does with a JWKS. Rewind keeps no secret
of the customer, and the server learns the end user from a signature, not from a claim.

- **The key.** Ed25519, one per tenant, made with the first action set that needs it; its private
  part is encrypted in the `SecretStore` with the tenant's data key, as its secrets are. No key
  per namespace: every private key would sit in the same store, read by the same nodes, and
  `sub` comes from the run, never from the workflow's code. `POST
  /v1/organization/signing-keys` replaces it: the old key stops signing and stays published for
  24 hours.
- **The public keys** are at `{public-url}/organizations/{organization}/jwks.json`, without
  authentication, with `Cache-Control: max-age=300`.
- **The JWT**, in `Authorization: Bearer`, with `alg` `EdDSA` and the key's `kid`:

| Claim | Value |
| --- | --- |
| `iss` | `{public-url}/organizations/{organization}`: the organization is in the issuer, its keys at `{iss}/jwks.json` |
| `aud` | The API's `baseUrl`, or the MCP server's URL |
| `sub` | The run's namespace: the end user |
| `labels` | The run's labels |
| `run`, `action` | The run's id and the action's name |
| `tool` | The action's name again, for verifiers on SDK 0.3.0, which check `tool`; verifiers read `action` |
| `idempotency_key` | `{run_id}/{seq}` |
| `jti` | `{run_id}/{seq}/{delivery}`: one per request |
| `htm`, `htu`, `body` | The method, the URL, and the SHA-256 of the body in base64url (absent without a body) |
| `iat`, `exp` | Now, and 60 s later |

- **What the server checks**: the signature, with a key of `{iss}/jwks.json` for the issuer it
  expects; `aud` is its own; `exp` has not passed, give or take a few seconds; `htm`, `htu` and
  `body` match the request; and, for an organization that runs several environments, that
  `labels` hold its own (`env=prod`): one key signs every run of the organization, whatever its
  labels, and a key limited to `env=dev` may register an action set that names the production
  server. `sub` is then the end user it acts for.
- **The SDK checks it.** `verifyRewindCall(request, { issuer, audience, labels? })` in
  `@rewind.codes/sdk` does every check above, and `rewindIssuer(url, organizationId)` gives the
  issuer of an organization: the keys fetched from `{issuer}/jwks.json` and cached, a key with an
  unknown `kid` fetched again at most every 10 s, 5 s of leeway on `exp` and `iat`, the body
  hashed from the raw bytes. It answers `{ namespace, labels, run, action, idempotencyKey }`, the
  action read from `action`, or throws a `RewindSignatureError` naming the check that failed. It
  takes a Fetch `Request`, or `{ method, url, headers, body }` for a server that has already read
  it (Express, NestJS); `url` is the one the client called, which a server behind a proxy gives.
- **MCP.** Each HTTP request of the session is signed. Those of a call carry `sub`, `labels`,
  `run`, `action`, `tool` and `idempotency_key`; the others (`initialize`, the listing at
  registration) only `iss`, `aud`, `jti`, `htm`, `htu`, `body`, `iat` and `exp`.
- A refused signature (401) parks the call; the harness approves it once the server accepts the
  key again.

## The `http` action and egress

The `http` action's requests leave from the dispatcher, never from the isolate. An action set
enables the action with an allow-list of hosts. Without an allow-list there is no `http` action,
and no `fetch` in the workflow's code.

Each request goes through these steps, in order:

1. The host must be in the action set's allow-list.
2. The name is resolved once.
3. Private, loopback, link-local and cloud-metadata IPs are refused.
4. The connection goes to the IP that was checked. A DNS answer that changes between the
   check and the connection cannot reach the internal network.
5. Each redirect is checked again from step 1, up to 5 redirects.
6. The response is bounded to 5 MB and 30 s.

A refused request fails with an `ActionError`. In production, egress also goes through an
egress proxy ([sandboxed nodes](06-storage-and-scale.md#sandboxed-nodes)).

### Where MCP servers and APIs may be

An organization names the URLs of its MCP servers and APIs, and Rewind connects to them: the
edge at registration, to list an MCP server's tools or read an OpenAPI document, and the node of
the run's owner at each call. On a deployed server, these connections go through the egress
checks of the `http` action, at registration and at every connection:

1. The name is resolved once.
2. Private, loopback, link-local and cloud-metadata IPs are refused.
3. The connection goes to the IP that was checked.
4. A redirect is followed only within the server's origin, with a 307 or a 308, up to 5 times,
   each new connection checked again. A redirect elsewhere is refused: the secret or the
   signature would follow it.

An operator may let internal servers through, by configuration: host names
(`mcp.internal.example`, `*.svc.cluster.local`) and networks (`10.20.0.0/16`), never a switch
for the whole private network (`rewind server --allow-internal-addresses`). What such an internal
server answers is never quoted to the organization: an error gives the HTTP status, not the body.

A refused server fails the registration (`action-set-unavailable`), and a call with an
`ActionError` (`unavailable`), for good. The embedded server reaches any address, as it starts
stdio servers: one developer names the servers of their own machine.

## Workflow names

A workflow is shown by its name wherever it shows, never by its digest
([decisions](08-decisions.md#taken-while-reviewing-the-vocabulary-against-main-november-2026)).
A name is optional: a workflow is a workflow from its first run, identified by the id the API
assigned it (`wf_7k2m9q4x8d1c`), and a name brings what a name brings: versions numbered under it,
and starting it by its name ([workflows](01-product.md#workflows)).

- **What a name is.** An identifier that is not a reserved word of JavaScript, nor a word strict
  mode reserves, as the names `rewind:actions` declares are, since `rewind:workflows` exports it;
  it never starts with `wf_`, so a name never reads as an id.
- **Unique in its scope**: among its namespace's workflows, or among the organization's. A
  namespace's name shadows the organization's: a run or a harness in a namespace that names `gym`
  gets the namespace's `gym`, else the organization's. A fork keeps its source's name, so the
  namespace's runs use the fork from then on.
- **Given** when the workflow is created (`POST /v1/workflows`), or later (`PATCH
  /v1/workflows/{id}`, the agent tool `update_workflow`), with `workflows:write`; changed or
  removed the same way. The workflow keeps its id and its versions. A name its scope already has
  is 409 `name-taken`; an invalid one 422 `invalid-workflow`.

Where a workflow shows, it reads, in order:

1. its name;
2. the first line of the comment at the top of its latest version's code, a block
   (`/** … */`, `/* … */`) or a run of `//` lines before any code (tags such as `@ts-check` are
   left out);
3. without either, its short id: `wf-7k2m9q`, the first 6 characters of its id after `wf_`.

Its description is the one given, of at most 2,000 characters, else the lines of that comment
after its first. The comment and the description are display only: never part of a workflow's
identity, its name, its handle or its version. The comment is part of the code, so the same code
reads the same everywhere, and a comment that changes is new code. `run_workflow` and
`check_workflow` ask the model for the comment, and `check_workflow` says when the code has none.
A name or a description given, changed or removed sends the workflow's change: every client that
shows it reads it again (the change stream's `workflow` change).

A run's handle takes its workflow's name when the run is created, else its short id
(`user_2fQ81/wf-7k2m9q/06h0n8tcbk20`), and never changes after: a workflow named or renamed later
leaves the handles of its past runs as they are ([handles](06-storage-and-scale.md#run-handles)).
Handles stored before this release keep their origin, `script` among them.

Before this release, a person could give a script a title. During the migration
(`rewind server rename-data`), a title that is a valid name, free in its workflow's scope, became
that workflow's name; the others were dropped.

## Type-checking

`rewind check` (and `POST /v1/check`) type-checks a workflow's code against the sandbox lib and
the action set's declarations, in strict mode, before anything runs. Diagnostics are formatted
for a model: location, message, and a short code frame.

What tsgo sees:

| Declarations | Contents |
| --- | --- |
| ECMAScript lib | ES2025 and the ESNext pieces V8 ships (`Temporal`, explicit resource management…), no DOM, without the shared memory and weak references the sandbox removes |
| Sandbox globals | `globals.d.ts`, declaring exactly the globals the [sandbox](01-product.md#what-exists-in-the-sandbox) has; `fetch.d.ts` only when the action set enables the `http` action, whose `rewind-actions.d.ts` then starts with `/// <reference path="fetch.d.ts" />` |
| `rewind.d.ts` | The `rewind` module: `sleep`, `signal`, `signalUrl`, `run`, `ActionError`, `CancelledError`, `RunError`, the types of workflows and their runs, and `Signals`, the signals the run accepts (empty, so any name, unless the version has an interface) |
| `rewind-actions.d.ts` | Generated by `rewind-codegen` for the action set: `rewind:actions` and `rewind:workflows` |
| The workflow's own declarations | Only for a version with an interface, generated from its schemas: its signals, which `Signals` takes, so `signal()` and `signalUrl()` take only their names and `signal()` returns their types; and `entry.ts`, which assigns the code's default export to a function from the input type to the output type |

Only the names of this release are declared. The sandbox still resolves the names of code written
before it, `rewind:tools`, `webhook()`, `ToolError` and `WorkflowError`, so that stored versions
and the runs waiting on them keep running and replaying
([determinism epoch](03-execution.md#determinism-epoch)); no declaration, prompt or document
shows them, and new code that uses them does not type-check.

What type-checks exists at run time, and the reverse. The sandbox lib lives in
`crates/rewind-runtime/lib/`, with `tsconfig.json` (tsgo's options) and `globals.json` (the
declared names); a test in CI checks that the isolate's globals and the sandbox lib declare the
same names ([decided](08-decisions.md#taken-while-freezing-the-http-api-and-the-sandbox-lib-november-2026)).

`rewind-check` drives tsgo, the native TypeScript 7 compiler, as a CLI, behind the `TypeChecker`
port: TypeScript 7.0 has no programmatic API. The tsgo binary is embedded in the `rewind` binary
and extracted to a cache directory at its first launch, so Rewind stays a single binary.

The checker also runs rules of its own. They run on the swc syntax tree that the checks at
start already parse (`deno_ast`), without type information. They are syntactic, and so
slightly less precise than rules based on types.

| Rule | Flags | Fix |
| --- | --- | --- |
| Floating promise | A call to an action, to a `rewind` function or to an async function of the code, used as a statement: neither awaited, returned nor assigned | `await`; `void expr` says it is intended |
| Ignored async callback | An `async` function passed to `forEach` and the like, which ignore its result | `for … of` with `await`, or `Promise.all` over `map` |
| Absent global | `fetch`, `require`, `process`, `Buffer` | A hint saying what to use instead |

Their diagnostics join tsgo's. These rules are the first line. At run time, a forgotten
`await` is still harmless ([how a run ends](03-execution.md#how-a-run-ends)).

`runs.start` checks by default; the harness can skip it. A version made through
`/v1/workflows` always has its code checked, and a run of a workflow, given by its id or its
name, is not checked again when it starts.

Transpilation is `deno_ast` (swc: type stripping and source maps).
