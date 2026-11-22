# 4. Tools

## Routes

A toolset has four kinds of tools:

| Route | Called by | Used for |
| --- | --- | --- |
| MCP tools | The dispatcher of Rewind, an MCP client (`rmcp`, stdio or streamable HTTP) | Every tool of the toolset's MCP servers |
| API tools | The dispatcher of Rewind, an HTTP client | Every operation of the toolset's APIs, each described by an OpenAPI document ([API tools](#api-tools)) |
| Built-ins: `http`, `webhook`; `workflows.<name>.start` and `workflows.<name>.signal` for each workflow the toolset pins | The dispatcher of Rewind; the engine itself for `webhook` and the workflows' | `http` is the only way a script reaches the network ([egress](#the-http-tool-and-egress)); `webhook(name)` returns a URL bound to the run and the signal name ([signals](01-product.md#signals)); the workflows' start and signal runs of [workflows](03-execution.md#workflows-across-runs), in the namespace of the run that calls them |
| Host tools | The harness's SDK worker, which holds a task stream open (`GET /v1/tasks/stream`) and is handed the tasks as they become ready; each task names the namespace of its run, the end user the handler acts for. A worker is handed only the tasks of the runs its key or token sees: an end user's namespace, a grant's labels ([access](06-storage-and-scale.md#tenancy-namespaces-identity-and-access)) | Functions that live in the harness: ask the end user, call another LLM, start a sub-agent. A customer's backend that pulls its calls instead of receiving them, as an API's are: nothing reaches it from outside, and it needs no signature to check (Dax) |

Rewind calls MCP tools, API tools and built-ins directly. Their results go back to the script,
never to the agent. The harness is on the path only for its host tools, and they are optional: a
toolset may have only MCP servers or APIs. There is no other kind of tool in 0.1.

Every call carries an **idempotency key** `"{run_id}/{seq}"`, passed in MCP `_meta`
(`codes.rewind/idempotency-key`), to an API in the `Idempotency-Key` header when its operation
declares one, and to host tool handlers. Calls also carry the W3C trace context (`traceparent`,
`tracestate`), in `_meta` for MCP and in headers for an API, with the tool call span as parent,
so the server's own spans join the run's trace. How Rewind proves who calls, a secret or its own
signature, is in [authenticating calls](#authenticating-calls).

This key protects the way out. The way in (starting a run, sending a signal) has its
own keys: see [storage](06-storage-and-scale.md#idempotency-of-the-api). A fork's calls
carry other keys: see [forks](03-execution.md#forks).

A host tool can last days. The most complete case, a host tool that outlasts the run's
`wait`:

1. The agent starts the run with `POST /v1/runs` and `wait=30s`. The script calls `ask_user`.
2. The harness's worker holds `GET /v1/tasks/stream?tools=ask_user` open, and is handed the
   task on it, with a token and the idempotency key.
3. While it works, its stream stays open: the open stream is the liveness of every task handed
   on it.
4. After 30 s, the agent gets `suspended`, waiting for `ask_user`.
5. When it has the answer, even days later, the worker sends
   `POST /v1/tasks/{token}/complete`. The completion goes to the run's inbox, the run resumes
   and ends, and the harness gets the signed webhook `run.completed`.

## Tool task lifecycle

Each `schedule_tool_call` creates a **tool task**, executed outside the isolate. Attempts,
backoff, approvals and tokens live in the tool task, kept with its run in the run's partition,
not in the journal. The journal only gets the final outcome.

The task is created in the batch that commits its call, and nothing is sent before that batch
is durable. Then the run's owner sends it on its way. An MCP or API call waits for a slot: the
owner checks the per-run limit itself, and asks the tenant's organization actor for a slot of
the server's or the API's limit, which is the organization's ([policies](#policies)); with the
slot, the owner's node sends the call. A host tool's task goes to the organization actor,
which hands it to an SDK worker's open stream with a free slot, each hand-out with a token of
its own ([inside a node](07-code-architecture.md#inside-a-node)). The token names the run, so
an answer reaches the run's owner from whichever process receives it.

One state machine covers every route:

| State | The task | Leaves it |
| --- | --- | --- |
| `approval` | Waits for the harness. Only with the `approval` policy | approve → `ready`; reject → `done`, the call fails with a `ToolError` |
| `ready` | Waits for its next attempt (`next_attempt_at`, a timer after a backoff) and a concurrency slot | sent or handed out → `claimed` |
| `claimed` | Attempt n, in flight: sent by the owner's node, or handed to an SDK worker's stream | See the next table |
| `deferred` | Holds a token until the handler answers | `/complete` or `/fail` → `done`; a timeout, bounded by the run's TTL and kept as a timer of the run's partition → `done` |
| `parked` | Waits for the harness after a schema change, a refused call or a missing secret | continue, or the secret put again → `ready`; cancel → `done` |
| `done` | Its completion is in the run's inbox: a result, a failure, or `outcomeUnknown` | |

From `claimed`:

| Event | Next state |
| --- | --- |
| The tool answered | `done`, with its result or its failure |
| Transient failure | `ready` with backoff (policy `retry`); `done` with the failure (`at_most_once`) |
| The call was interrupted: the node that sent it died, or the SDK worker's stream closed and did not come back within its grace (`previous_claim_expired` on the next hand-out) | `ready` with attempt + 1, up to 5 attempts, then `done` with `outcomeUnknown` (policy `retry`); `done` with `outcomeUnknown` at once (`at_most_once`) |
| The handler answers later | `deferred` |
| The schema changed at dispatch | `parked`, and the harness gets the webhook `tool.schema_changed` ([toolset change](#when-a-toolset-changes-during-a-run)) |
| The server answered 401 | `parked`, and the harness gets the webhook `credentials.expired` with the reason `refused` ([authenticating calls](#authenticating-calls)) |
| The secret the run attached for the server no longer exists | `parked` before anything is sent, and the harness gets `credentials.expired` with the reason `missing` |

The harness answers a task in `approval`, and a task `parked` by a schema change or a refused
call, with `POST /v1/tasks/{id}/approve` (→ `ready`) or `POST /v1/tasks/{id}/reject` (→ `done`,
the call fails with a `ToolError`). Putting a secret again makes `ready` the tasks parked by a
refusal or a missing secret in the runs that attach it: the write is a change of the control
plane, which every owner receives and applies to its runs. A run with a task in `approval` or
`parked` is suspended, and its state says which call waits.

Sending a call or handing out a task writes nothing: the task's records say `ready` while it
is in flight, and only the owner's memory knows it left. What is written is what happened to
it: its creation, a backoff, an interruption, its outcome. Nothing is written while a task is in
flight either: no heartbeat, no renewal.

A call is interrupted when the node that sent it dies. The node that takes the partition over
cannot know which of the run's `ready` tasks had left, so it treats each as interrupted: under
`retry` it sends it again with attempt + 1, written in its recovery batch; under
`at_most_once` it fails it with `outcomeUnknown`. A host task is interrupted when the stream it
was handed on closes. The organization actor waits for that worker to open a stream again, for
the policy's grace, and then the owner writes attempt + 1 and the actor hands the task to
another worker, or the owner fails it under `at_most_once`.

When a run ends, its tasks are deleted: a call still queued is never sent. Results arriving
for a run that already ended are dropped.

The backoff curve is an implementation choice, recorded in 08.

## Policies

Each tool has a policy, pinned in the toolset. The harness sets it per tool when it
registers the toolset.

| Setting | Values | Default |
| --- | --- | --- |
| After an interruption | `retry` or `at_most_once` | `retry`; for an API operation `POST` or `PATCH`, `at_most_once` unless it declares an `Idempotency-Key` header ([API tools](#api-tools)) |
| `approval` | The task waits for the harness before it is sent | Off |
| Execution timeout | A duration | 5 min for MCP and API tools |
| Grace after a stream closes | A duration: how long a host task handed on a closed stream waits for its worker to come back | 60 s for host tools |
| Concurrency | Calls in flight | 16 per run; 8 per MCP server or API in the organization |

| Policy | On transient error | Interrupted (outcome unknown) |
| --- | --- | --- |
| `retry` | Retry with backoff | Retry with the same idempotency key, up to 5 attempts, then fail with `outcomeUnknown: true` |
| `at_most_once` | Fail | Fail with `outcomeUnknown: true` |

An interrupted call is called again with the same idempotency key, so the tool can recognize
it and not repeat its effect. A tool that ignores the key may then run twice: such a tool is
declared `at_most_once` in the toolset.

The `approval` policy lets the harness keep its permission gates: a script cannot call such a
tool without the harness's consent.

Concurrency limits are invisible to the journal: a task waiting for a slot stays `ready`. The
limit per run is kept by the run's owner. The limit of a server or an API holds across the
organization: its slots belong to the tenant's organization actor, which grants them in the
order they are asked for.

The script sees failures as a thrown `ToolError` and decides what to do.

## Toolsets and code generation

A toolset is a list of MCP servers, APIs and host tools, with a policy per tool, and of the
[workflows](01-product.md#workflows) its scripts may start. Registering it
(`POST /v1/toolsets`) connects to the MCP servers and lists their tools, reads the APIs'
OpenAPI documents, and **pins** the schemas, and the latest version of each workflow it names (or
the version it names: `triage@9f2c4e1a07b3`). The pinned toolset is a document of the
[stored format](02-journal.md#stored-format-v1): its servers and APIs, with how each one
authenticates its calls ([authenticating calls](#authenticating-calls)), for each tool its name,
route, schemas and policy, and for each workflow its name, version, description and schemas. The
toolset is content-addressed, and every run records the digest it was started with. A toolset
can also be derived from a pinned one (`base`): its servers and tools are kept as pinned, and the
workflows it names are added or pinned again, without listing the servers again.

A toolset holds no secret. A run reads a secret only when it attached it at start, by name, and
secrets do not depend on the toolset: a toolset registered again with one more tool keeps
calling with the same secrets.

Each pinned workflow gives two built-in tools, `workflows.<name>.start` and
`workflows.<name>.signal`, with the default policy of built-ins. They are not part of the
document: the workflow is.

From each pinned tool, `rewind-codegen` generates three outputs. For `github.list_issues`:

| Output | Read by | Example |
| --- | --- | --- |
| The `rewind:tools` runtime module: thin wrappers over the tool op | The runtime | `list_issues: (args) => call("github.list_issues", args)` |
| `rewind-tools.d.ts`: types for args (from `inputSchema`) and results (from `outputSchema` when present, otherwise the MCP content type with `.text()` and `.json()` helpers), with tool descriptions as JSDoc | tsgo | `list_issues(args: { repo: string; state?: "open" \| "closed" }): Promise<Issue[]>` |
| A **compact prompt rendering** of the same declarations | The model | `github.list_issues({ repo, state? }) → Issue[] · List the issues` |

For each pinned workflow, `rewind-tools.d.ts` also declares the module `rewind:workflows`, and
the prompt rendering gives it a line:

| Output | Example |
| --- | --- |
| `rewind-tools.d.ts` | `export const triage: Workflow<{ repo: string }, { closed: number }, { approval: { close: boolean } }>` |
| Prompt rendering | `triage.start({ repo: string }) → run · run.signal("approval", { close: boolean }) · run.result() → { closed: number } · Triage…`, after the line `import { triage } from "rewind:workflows";` |

Codegen is deterministic. Without it, nothing could check a script before it runs: tsgo would
not know that `repo` is a string. With it, `github.list_issues({ repo: 42 })` is refused at
start, before any run exists: `Type 'number' is not assignable to type 'string'
(script.ts:3:28)`.

Tool names are kept as the server defines them (`github.list_issues`), and an API's as its
document names its operations (`dax.createMeasure`), so they match the descriptions the model
reads. The names `rewind:tools` declares, a server, an API, a host-tool group and a host tool
without a group, are identifiers that are not reserved words of JavaScript (`default`,
`delete`, `class`…), nor the words strict mode reserves (`let`, `static`, `eval`…): a toolset
naming one is refused when it is registered. So are the names of workflows, which
`rewind:workflows` declares. The namespace `workflows` is reserved for their built-ins: no server,
API or host-tool group takes it.

Args are also validated at run time, against the same JSON Schema, inside the op, before any
command is recorded. Invalid args throw a `TypeError` at the call site. This is deterministic,
since the schema is pinned.

The JSON Schema → TypeScript converter is ours, written in Rust in `rewind-codegen`: no
maintained crate exists. It covers objects, `required`, enums, `anyOf`/`oneOf`/`allOf`,
`$ref`/`$defs`, arrays and `additionalProperties`.

### When a toolset changes during a run

A run keeps the toolset it started with, but the server behind it can change during a long
pause: a tool renamed, removed, or with a new schema. The dispatcher checks each MCP call against
the server's current schema at dispatch. An API has no listing to ask: its pinned document is
the contract, and a call that the API no longer accepts fails with its 4xx. A call that no longer matches is not failed
silently: its task is `parked`, and the harness gets the webhook `tool.schema_changed`. The
agent or the developer decides what happens to the run:

- continue against the new schema: `approve`, and the task is `ready` again; the dispatcher
  then sends it without checking the schema again;
- [fork](03-execution.md#forks) it with a corrected script;
- cancel it, or `reject` the call, which fails with a `ToolError`.

### Observed result types

Most MCP servers declare no `outputSchema`, so the model would write code against a
result it cannot see. But every result ever returned is in a journal. A projection
infers, per tool, the **shape observed** in recorded results, and codegen uses it when
the server declares nothing.

- Shapes only: keys and types, never values. Observations stay within a tenant.
- A field missing from some results is optional. An MCP tool never observed keeps the MCP
  content type and its helpers; an API tool never observed returns `unknown`.
- The declaration says it is observed, not declared (`@observed` in its JSDoc), so the
  model knows it may be incomplete.
- Observed types serve type-checking and the prompt only. Results are never validated
  against them, and they are not part of the toolset digest, so replay does not depend
  on them.

## API tools

A toolset names its APIs in `apis`, each with:

| Field | Meaning |
| --- | --- |
| `name` | Its namespace in `rewind:tools`, an identifier as a server's name is |
| `openapi` | Its OpenAPI 3.0 or 3.1 document, in JSON: given in the request, or its URL. A URL is read once, at registration, through the [egress checks](#where-mcp-servers-and-apis-may-be), at most 2 MiB; the document is then pinned and never read again |
| `baseUrl` | Where the calls go: the document's first `servers[]` unless given. `https`, or `http` on the embedded server |
| `auth` | How its calls prove who calls ([authenticating calls](#authenticating-calls)); the document's security requirement when left out |
| `operations` | Optional: the `operationId`s to keep, for an API too large for the prompt |
| `concurrency` | Its calls in flight across the organization, 8 by default |

From the document:

- **One tool per operation**, named by its `operationId`, or from its method and path when it has
  none (`post_v1_measures`). A name that is not a valid tool name, or two operations with the same
  name, refuse the registration (`invalid-toolset`), naming the operation's path.
- **Its input schema** is an object: the path, query and header parameters by their names, and
  the JSON request body under `body`. A name two parameters share, or a parameter named `body`,
  refuses the registration. The header `Idempotency-Key` and the headers of the authentication
  are not parameters: Rewind fills them.
- **Its output schema** is the schema of its first `2xx` response in `application/json`; without
  one, the tool gets an [observed](#observed-result-types) type.
- `$ref` is resolved within the document (`#/components/…`); a reference to another document
  refuses the registration. `summary` and `description` become the tool's description.
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
| `401` | Is `parked` ([lifecycle](#tool-task-lifecycle)) |
| `408`, `429`, `5xx`, a failed connection | Is transient: retried with backoff under `retry`, never before its `Retry-After` |
| Another status | Fails with a `ToolError` holding the status, and the body's `title` and `detail` when it is `application/problem+json`, else its first 2 KB |

The default policy follows the method: `retry` for `GET`, `HEAD`, `OPTIONS`, `PUT` and `DELETE`,
which HTTP defines as idempotent; `at_most_once` for `POST` and `PATCH`, unless the operation
declares an `Idempotency-Key` header, by which the API recognizes a call sent again. The toolset
may set another policy per tool, as for any tool.

An API suits calls answered within the execution timeout. A call that waits days, for a person's
answer, is a host tool, or the script waits for a signal ([`webhook()`](01-product.md#signals)).

## Authenticating calls

Each MCP server and API of a toolset says, in its `auth`, how its calls prove who calls. `auth` is
part of the pinned toolset; no value is.

| `auth` | The request carries | For |
| --- | --- | --- |
| none | Nothing | Public servers |
| `{ "type": "rewind" }` | A JWT signed with the tenant's key ([signed calls](#signed-calls)), in `Authorization: Bearer` | The customer's own APIs and MCP servers: they check the signature and learn the end user |
| `{ "type": "secret", "header": "<name>", "format"?: "Bearer {secret}" }` | The [secret](#secrets) the run attached, in that header, `{secret}` replaced by its value; the value alone without `format` | Other servers: GitHub, Linear, an API with a key |
| `{ "type": "secret", "env": "<VARIABLE>" }` | The attached secret in that variable of the process | A stdio MCP server |

A secret never goes in a URL. Without `auth`, an API follows its document's security
requirement: `http` `bearer`, `oauth2` and `openIdConnect` are `Authorization: Bearer {secret}`;
`http` `basic` is `Authorization: Basic` with the secret (`user:password`) in base64; `apiKey` in
a header is that header with the secret. An `apiKey` in the query or a cookie, or a requirement
of several schemes at once, refuses the registration unless `auth` is given.

The `http` built-in carries no secret and no signature: a call that needs one is an API tool.

### Secrets

A **secret** is a named value, encrypted at rest, that only the dispatcher reads, for the runs
that attached it.

| | An organization's | A namespace's |
| --- | --- | --- |
| Belongs to | The tenant, shared by every end user | One end user: Alice's GitHub token |
| Named in an attachment | `org/<name>` | `ns/<name>`: the secret `<name>` of the run's own namespace |
| Labels | Its own: a grant limited to labels sees, writes and attaches only the secrets carrying them, and the secrets it writes take them | None: the namespace is its scope |
| Written with | `PUT /v1/secrets/{name}` | `PUT /v1/namespaces/{namespace}/secrets/{name}` |

- A name is 1 to 64 lowercase letters, digits, `-` and `_`, starting with a letter. A value is a
  string of at most 16 KiB.
- **Write-only.** No endpoint answers a value. A secret is listed with its metadata: its name, its
  labels, who wrote it and when, its reads per day, its last read (run, namespace, date; a
  toolset's registration, which reads an organization's secret to list the tools of the server it
  is attached for before any run exists, names no run) ([07](07-code-architecture.md#http-api-v1)).
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
- Secrets never enter the journal, the isolate, a cassette, OpenTelemetry, nor an answer of the
  API.

**Attaching.** A run reads a secret only if it attached it at start, server by server, in the
request that gives the script, never in the script: `secrets: { "github": "ns/github",
"zendesk": "org/zendesk" }`.

| Where | The attachments |
| --- | --- |
| `POST /v1/runs` with a script | The request's |
| `POST /v1/runs` with a workflow, and `workflows.<name>.start` | The version's, saved with it (`POST /v1/workflows`), in its document and so in its digest. The request gives none |
| `POST /v1/runs/{id}/fork` | The parent's, unless the request gives its own |
| `POST /v1/toolsets` | An organization's secrets only, to list the tools of an MCP server that needs one; no run keeps them |

The start is refused before any run exists (422 `secret-required`, with a message for a model
that lists the secrets the caller may attach for that server) when:

- the script imports a server or an API whose `auth` is `secret`, and nothing is attached for it;
- an attachment names a server that takes no secret, or a secret that does not exist (an `ns/`
  secret in the run's namespace);
- the caller may not attach it.

Who may attach:

- **A namespace's secret**: whoever starts a run in that namespace. The secret is the end
  user's own, and only that namespace's runs read it.
- **An organization's secret**: who holds `secrets:use`, for a secret whose labels are within
  the grant's. An end user's token only if it was given `secrets:use`; a connection with the
  access "Run", within its person's grant.
- **A workflow version is the permission.** Saving a version checks its attachments against the
  saver's rights: an organization's secret must exist and be theirs to attach; an `ns/` secret
  has no namespace yet, and only its name is checked. Starting a run of it, from the API or from
  a script, checks nothing more than that its `ns/` secrets exist in the run's namespace: its
  code is fixed and its suite passed. A script written for one run, and a fork, whose script is
  new code, are checked against their caller.

**Reading.** At each call to a server whose `auth` is `secret`, the dispatcher reads the secret
the run attached for it, in the run's namespace for an `ns/` one, injects it into the transport
and counts the read. A secret deleted since parks the call (`credentials.expired`, reason
`missing`); a 401 parks it too (reason `refused`). Putting the secret again under the same name
makes those calls `ready`, sent with the same idempotency key. Nothing falls back on another
secret: Alice's run that attached `ns/github` never calls with the organization's token, nor
with Bob's.

**Who reads what.** Attachments are run metadata and part of a version's document. Before a
deletion, the dashboard and `GET …/secrets/{name}/usage` say which active runs and which
workflows read the secret, and how often it was read.

### Signed calls

With `auth: { "type": "rewind" }`, Rewind signs each request with the tenant's key, and the
server checks it with the public key, as any JWT library does with a JWKS. Rewind keeps no secret
of the customer, and the server learns the end user from a signature, not from a claim.

- **The key.** Ed25519, one per tenant, made with the first toolset that needs it; its private
  part is encrypted in the `SecretStore` with the tenant's data key, as its secrets are. No key
  per namespace: every private key would sit in the same store, read by the same nodes, and
  `sub` comes from the run, never from the script. `POST
  /v1/organization/signing-keys` replaces it: the old key stops signing and stays published for
  24 hours.
- **The public keys** are at `{public-url}/organizations/{tenant}/jwks.json`, without
  authentication, with `Cache-Control: max-age=300`.
- **The JWT**, in `Authorization: Bearer`, with `alg` `EdDSA` and the key's `kid`:

| Claim | Value |
| --- | --- |
| `iss` | `{public-url}/organizations/{tenant}`: the tenant is in the issuer, its keys at `{iss}/jwks.json` |
| `aud` | The API's `baseUrl`, or the MCP server's URL |
| `sub` | The run's namespace: the end user |
| `labels` | The run's labels |
| `run`, `tool` | The run's id and the tool's name |
| `idempotency_key` | `{run_id}/{seq}` |
| `jti` | `{run_id}/{seq}/{attempt}`: one per request |
| `htm`, `htu`, `body` | The method, the URL, and the SHA-256 of the body in base64url (absent without a body) |
| `iat`, `exp` | Now, and 60 s later |

- **What the server checks**: the signature, with a key of `{iss}/jwks.json` for the issuer it
  expects; `aud` is its own; `exp` has not passed, give or take a few seconds; `htm`, `htu` and
  `body` match the request; and, for an organization that runs several environments, that
  `labels` hold its own (`env=prod`): one key signs every run of the organization, whatever its
  labels, and a key limited to `env=dev` may register a toolset that names the production server.
  `sub` is then the end user it acts for.
- **The SDK checks it.** `verifyRewindCall(request, { issuer, audience, labels? })` in
  `@rewind.codes/sdk` does every check above: the keys fetched from `{issuer}/jwks.json` and
  cached, a key with an unknown `kid` fetched again at most every 10 s, 5 s of leeway on `exp`
  and `iat`, the body hashed from the raw bytes. It answers `{ namespace, labels, run, tool,
  idempotencyKey }`, or throws a `RewindSignatureError` naming the check that failed. It takes a
  Fetch `Request`, or `{ method, url, headers, body }` for a server that has already read it
  (Express, NestJS); `url` is the one the client called, which a server behind a proxy gives.
- **MCP.** Each HTTP request of the session is signed. Those of a call carry `sub`, `labels`,
  `run`, `tool` and `idempotency_key`; the others (`initialize`, the listing at registration)
  only `iss`, `aud`, `jti`, `htm`, `htu`, `body`, `iat` and `exp`.
- A refused signature (401) parks the call; the harness approves it once the server accepts the
  key again.

## The `http` tool and egress

The `http` tool's requests leave from the dispatcher, never from the isolate. A toolset
enables the tool with an allow-list of hosts. Without an allow-list there is no `http` tool,
and no `fetch` in the script.

Each request goes through these steps, in order:

1. The host must be in the toolset's allow-list.
2. The name is resolved once.
3. Private, loopback, link-local and cloud-metadata IPs are refused.
4. The connection goes to the IP that was checked. A DNS answer that changes between the
   check and the connection cannot reach the internal network.
5. Each redirect is checked again from step 1, up to 5 redirects.
6. The response is bounded to 5 MB and 30 s.

A refused request fails with a `ToolError`. In production, egress also goes through an
egress proxy ([sandboxed workers](06-storage-and-scale.md#sandboxed-nodes)).

### Where MCP servers and APIs may be

A tenant names the URLs of its MCP servers and APIs, and Rewind connects to them: the edge at
registration, to list an MCP server's tools or read an OpenAPI document, and the node of the run's
owner at each call. On a deployed server, these connections go through the egress checks of the `http` tool,
at registration and at every connection:

1. The name is resolved once.
2. Private, loopback, link-local and cloud-metadata IPs are refused.
3. The connection goes to the IP that was checked.
4. A redirect is followed only within the server's origin, with a 307 or a 308, up to 5 times,
   each new connection checked again. A redirect elsewhere is refused: the secret or the
   signature would follow it.

An operator may let internal servers through, by configuration: host names
(`mcp.internal.example`, `*.svc.cluster.local`) and networks (`10.20.0.0/16`), never a switch
for the whole private network (`rewind server --allow-internal-tools`). What such an internal
server answers is never quoted to the tenant: an error gives the HTTP status, not the body.

A refused server fails the registration (`toolset-unavailable`), and a call with a `ToolError`
(`unavailable`), for good. The embedded server reaches any address, as it starts stdio servers:
one developer names the servers of their own machine.

## Script titles

A script is named by its title wherever it shows, never by its digest
([decisions](08-decisions.md#taken-for-script-titles-and-run-handles-november-2026)). Its
title is, in order:

1. the one a person gave it for the organization (`PATCH /v1/scripts/{digest}`,
   `scripts:write`): 1 to 120 characters, with a description of at most 2,000; `null` removes
   them;
2. the first line of the comment at its top, a block (`/** … */`, `/* … */`) or a run of `//`
   lines before any code: the lines after it are its description (tags such as `@ts-check`
   are left out);
3. without either, the workflow's name where a run of a workflow shows, else "Untitled script".

The comment is part of the source, so the same source has the same title everywhere; a title a
person gives renames the script for every run of it, past and future. `run_script` and
`check_script` ask the model for the comment, and `check_script` says when a script has none.
A run's handle takes the title when the run is created ([handles](06-storage-and-scale.md#run-handles)).

## Type-checking

`rewind check` (and `POST /v1/check`) type-checks the script against the sandbox lib and the
toolset's declarations, in strict mode, before anything runs. Diagnostics are formatted
for a model: location, message, and a short code frame.

What tsgo sees:

| Declarations | Contents |
| --- | --- |
| ECMAScript lib | ES2025 and the ESNext pieces V8 ships (`Temporal`, explicit resource management…), no DOM, without the shared memory and weak references the sandbox removes |
| Sandbox globals | `globals.d.ts`, declaring exactly the globals the [sandbox](01-product.md#what-exists-in-the-sandbox) has; `fetch.d.ts` only when the toolset enables the `http` tool, whose `rewind-tools.d.ts` then starts with `/// <reference path="fetch.d.ts" />` |
| `rewind.d.ts` | The `rewind` module: `sleep`, `signal`, `webhook`, `run`, `ToolError`, `CancelledError`, `WorkflowError`, the types of workflows and their runs, and `Signals`, the signals the run accepts (empty, so any name, unless the script is a workflow's) |
| `rewind-tools.d.ts` | Generated by `rewind-codegen` for the toolset: `rewind:tools` and `rewind:workflows` |
| The workflow's own declarations | Only when a workflow is registered, generated from its schemas: its signals, which `Signals` takes, so `signal()` and `webhook()` take only their names and `signal()` returns their types; and `entry.ts`, which assigns the script's default export to a function from the input type to the output type |

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
| Floating promise | A call to a tool, to a `rewind` function or to an async function of the script, used as a statement: neither awaited, returned nor assigned | `await`; `void expr` says it is intended |
| Ignored async callback | An `async` function passed to `forEach` and the like, which ignore its result | `for … of` with `await`, or `Promise.all` over `map` |
| Absent global | `fetch`, `require`, `process`, `Buffer` | A hint saying what to use instead |

Their diagnostics join tsgo's. These rules are the first line. At run time, a forgotten
`await` is still harmless ([how a script ends](03-execution.md#how-a-script-ends)).

`runs.start` checks by default; the harness can skip it. Registering a workflow always checks
its script, and a run of a workflow is not checked again when it starts.

Transpilation is `deno_ast` (swc: type stripping and source maps).
