# 6. Storage, scale and security

## Backends

Storage sits behind the `JournalStore`, `AccessStore`, `BlobStore`, `ReadModel` and `SecretStore`
ports of `rewind-engine`.

| Use | Journal, access, read model, secrets | Large payloads |
| --- | --- | --- |
| Production (`rewind-server`) | Postgres (`rewind-store-postgres`) | S3 or any S3-compatible service (`rewind-blob`) |
| Embedded (`rewind server --embedded`, local dev, tests) | Turso Database (`rewind-store-turso`, the `turso` crate) | Local files (`rewind-blob`) |
| Engine unit tests, mock mode | In memory | In memory |

Every adapter of `JournalStore`, `AccessStore`, `BlobStore`, `ReadModel` and `SecretStore` passes that port's
conformance suite, shipped by `rewind-engine` behind the `conformance` feature.

## Tables

Every key starts with the tenant, and what belongs to an end user names its namespace
([tenancy](#tenancy-namespaces-identity-and-access)).

```
tenants    (id PK, data_key, quotas, namespace_defaults, retention)   -- data_key encrypted by the master key
namespaces (tenant, name, quotas, tokens_revoked_before, created_at, last_active_at, deleting_since)
                                                           PK (tenant, name)
api_keys   (hash PK, tenant, name, created_by, created_at, revoked_at)
tokens     (hash PK, tenant, namespace, permissions, labels, expires_at, created_by, created_at,
            revoked_at)                                    index (tenant, namespace)
token_keys (kid PK, tenant, name, jwk, permissions, created_by, created_at)
roles      (tenant, name, description, permissions, created_at, updated_at)   PK (tenant, name)
grants     (tenant, principal, role, allowed_labels, granted_by, granted_at)   PK (tenant, principal)
                                                           -- principal: user:…, key:…, invitation:…
access_settings (tenant PK, default_role)
scripts    (tenant, digest, source, transpiled, source_map)   PK (tenant, digest)  -- only scripts that compile
namespace_scripts (tenant, namespace, digest, runs, first_run_at)   PK (tenant, namespace, digest)
toolsets   (tenant, digest, spec, declarations)               PK (tenant, digest)
workflows  (tenant, version, name, document, labels, created_at)   PK (tenant, version)
                                                           index on (tenant, name, created_at)
runs       (tenant, id, namespace, script, toolset, status, epoch, history_len, blocked_on,
            runnable_at, expires_at, quarantine, failures, attempts, lease_owner, lease_until,
            fence, sticky_worker,
            parent, workflow, workflow_version, started_by, started_seq, detached, scope,
            labels, trace_context, idempotency_key, created_at, updated_at, ended_at, archive)
            PK (tenant, id), UNIQUE (tenant, namespace, idempotency_key), index (tenant, namespace, status)
run_watchers (run_id, watcher, seq, attached)               PK (run_id, watcher)
history    (run_id, idx, recorded_at, event)              PK (run_id, idx)
inbox      (id, run_id, event, received_at)
signal_keys (run_id, key)                                  PK (run_id, key)
timers     (run_id, seq, fire_at)                          index on fire_at
tool_tasks (run_id, seq, tool, route, args, policy, server_concurrency, state, attempt, token,
            lease_owner, lease_until, next_attempt_at, parked_reason, created_at)
webhook_tokens (token PK, run_id, name)
payloads   (tenant, digest, bytes, preview, value, unreferenced_since, deletion_started_at)
                                                           PK (tenant, digest)  -- value up to 64 KiB
payload_refs (tenant, digest, run_id)                      PK (tenant, digest, run_id)
secrets    (tenant, owner, ciphertext)                     PK (tenant, owner)
                                                 -- owner: a toolset, a run, or a namespace's toolset
subscriptions (tenant, id, url, secret, events)
usage      (tenant, namespace, day, cpu_ms, runs_started)  PK (tenant, namespace, day)  -- namespace *: the tenant
quota_alerts (tenant, namespace, quota, period, threshold, used, quota_limit, raised_at,
              lease_owner, lease_until, sent_at)           PK (tenant, namespace, quota, period, threshold)
feed       (id, consumer, run_id, fence, from_idx, to_idx, lease_owner, lease_until)
meta       (key PK, value)                                 -- schema version; virtual time (embedded)

-- read model, written only by the projector
run_summaries (run_id PK, namespace, script, toolset, status, labels, started_at, ended_at,
               last_activity_at, tool_calls, errors, history_len)
tool_calls    (run_id, seq, namespace, tool, callsite_line, scheduled_at, completed_at, outcome,
               error_kind, args_digest, result_digest)      PK (run_id, seq)
```

`event` columns hold the `rewind-format` v1 JSON of the event. `namespace`, `labels`,
`trace_context`, `idempotency_key`, `parent`, `workflow`, `workflow_version`, `started_by`,
`started_seq`, `detached` and `scope` are run metadata: stored with the run, never in its events
([05](05-testing-and-observability.md#run-metadata)). A `workflows` row holds a
[workflow version](02-journal.md#stored-format-v1) document. `run_watchers` lists the runs that
receive a run's end ([workflows](03-execution.md#workflows-across-runs)): the run that started
it (`attached` unless it was started detached), and the forks that copied that start.
`namespaces`, `namespace_scripts`, `usage` and `quota_alerts` belong to the `JournalStore`, which
checks quotas when it creates a run; `tokens` and `token_keys` to the `AccessStore`.

`epoch` and `expires_at` are copies of the values in `run_started`, kept for queries:
workers pick runs by epoch, and the scheduler picks the runs past their deadline.
`quarantine` holds why a run is
[quarantined](03-execution.md#non-determinism-and-quarantine) (non-determinism or repeated
failures, the reason, and the last build that retried it), `failures` counts its
activations that failed in a row, and `attempts` the activations started since its last
commit: those that neither committed nor failed never finished, because their worker died. A tool task keeps the route and policy of its tool, copied from
the pinned toolset: the claim enforces the concurrency limits from them. `archive` points to the
journal document of an archived run, in the `BlobStore`.

## Leases, fencing, workers

- **Workers** hold nothing a run needs: their instance cache only saves a replay. They pick
  runnable runs with `FOR UPDATE SKIP LOCKED` (Postgres), among the epochs they support and
  outside quarantine, with the [fairness](#tenancy-namespaces-identity-and-access) between tenants and
  the priority of the [sticky worker](03-execution.md#sticky-execution). Acquiring a lease
  increments `fence`. The lease lasts 30 s, and the worker renews it about every 10 s while
  it works on the run, during the replay as during the execution: a long replay keeps its
  lease, and a dead worker frees the run at most 30 s after its last renewal.
- A commit only succeeds if the fence still matches, and `PK (run_id, idx)` is a second
  guard. A worker that lost its lease (GC pause, network partition) can never corrupt a
  history.
- The **scheduler** polls due timers the same way and pushes `timer_fired` into inboxes.
  It also polls the runs past their deadline and pushes
  `cancel_requested { reason: "ttl" }`.
- The **projector** claims feed items the same way, applies them to the read model and
  the OTel export, then deletes them.
- A terminal commit clears the run's pending timers and tool tasks.

Scaling path:

1. More workers.
2. Partition `history` and `inbox` by `run_id`; archive the histories of finished runs
   ([retention](#retention-and-deletion)).

Sharding over several Postgres clusters and a columnar read model are out of 0.1
([plan](09-roadmap.md#out-of-01)).

## Idempotency of the API

Tool calls carry a key on the way out ([tools](04-tools.md#routes)). The way in needs the
same protection: a request can succeed while its response is lost, and the caller then
retries without knowing.

| Request | Key | A second request with the same key | Without a key |
| --- | --- | --- | --- |
| Start a run | Unique per namespace | Returns the existing run | A second run starts and repeats every effect |
| Send a signal | Unique per run | Is acknowledged and ignored | A second `approval` stays in the mailbox and approves the script's next `signal("approval")` |
| A script starts a workflow run | `rewind:{run_id}/{seq}`, the call's key | Returns the run already started | |
| A script signals a run | `rewind:{run_id}/{seq}`, the call's key | Is acknowledged and ignored | |
| A run ends | `rewind:ended:{run id}`, for each run that watches it | Is ignored | |

- A signal's key is kept in `signal_keys` for the life of the run, not in the inbox. It
  must outlive the delivery of the signal, or a late retry would pass.
- Keys are run metadata. They never appear in events, so they are neither replayed nor
  exported in cassettes.
- Defaults in the SDK: the agent tools derive `run_script`'s key from the model's tool call
  id and the agent's labels, and a webhook uses the sender's delivery id when there is one.

## Payloads

Payloads (run input, tool args and results, run output, signal payloads) are stored in one
of three tiers, by size:

| Size | Where the value lives |
| --- | --- |
| Up to 1 KiB | In the event itself |
| Up to 64 KiB | In the `payloads` table (Postgres, Turso) |
| Above | In S3, or in files for the embedded server; its size and preview are in `payloads` |

The thresholds are configurable, and load tests confirm the defaults. Medium payloads stay
in the database so that a miss of the [instance cache](03-execution.md#sticky-execution) does
not cost an S3 round trip.

The responsibilities are split by layer:

| Layer | Knows |
| --- | --- |
| `rewind-core` | A payload's **identity**: the SHA-256 of its canonical JSON, as defined by RFC 8785 (JCS). An event holds either the value (`Payload::Value`) or only the digest (`Payload::Digest`). The determinism check compares digests. |
| `rewind-engine` | **When** to move values: at commit, a payload above the inline threshold leaves the event, which keeps only the digest. Its value goes into the `payloads` table with the commit, or to the `BlobStore` before it when it is above the second threshold. On delivery to the script, the value is loaded from where it lives. |
| `rewind-format` | **How** a payload is written in the journal: `{"value": …}` or `{"digest": "9f2c…"}`. A value is written in its canonical form, so its stored bytes are exactly the bytes its digest is computed on, whatever the JSON library's settings. |
| `rewind-blob` | **Where** large values live: `{tenant}/sha256/{digest}` on S3, `.rewind/blobs/{tenant}/sha256/ab/<digest>` on disk (atomic writes); workers keep a local cache in front of S3. |

Consequences:

- The canonical form is part of the stored format: a digest written today is compared
  during replays for as long as the run lives. RFC 8785 fixes what "sorted keys, no
  whitespace" leaves to the serializer: keys are ordered by UTF-16 code units, numbers
  are IEEE 754 doubles written as ECMAScript writes them, and string escaping is
  minimal. Golden vectors in `rewind-core`'s tests pin it.
- Payloads are immutable, deduplicated within a tenant, and cacheable forever.
- During replay, recorded command args are compared by digest, so they are never
  downloaded. Only completion results delivered to the script are fetched, and workers
  keep a local cache.
- Traces, OTel spans and the dashboard must render without downloading payloads. Size
  and preview are store metadata kept next to the digest in the `payloads` table, not part
  of the domain model ([decided](08-decisions.md#taken-after-the-layering-november-2026)).

## Tenancy, namespaces, identity and access

Four questions are kept apart:

| Question | Answered by |
| --- | --- |
| Who calls? | An API key, a member's session with WorkOS, or an end user's token |
| Which tenant? | The key's tenant, the member's organization, or the token's |
| Which namespace? | The token's; for the customer's keys and members, the `Rewind-Namespace` header |
| What may the caller do? | Rewind alone: the caller's grant, its role and the role's permissions; for a token, the permissions it was given |

WorkOS holds identity and nothing else: users, organizations, memberships, invitations and
the waitlist. It never says what someone may do. Rewind configures no role or permission there,
and never reads the role of a membership nor the `role`, `roles` and `permissions` claims of an
access token ([decided](08-decisions.md#taken-for-accounts-organizations-and-access-november-2026)).

### Tenants and organizations

A tenant is a WorkOS organization: the tenant's id is the organization's id (`org_01J…`, which
fits the tenant ids' alphabet). Rewind copies no user, organization or membership: WorkOS is
their only source. A tenant's record (quotas, retention) is created with the organization, or
at the first request of a member of an organization that has none.

The tenant is part of every storage key:

| Data | Key |
| --- | --- |
| Runs and their metadata | `(tenant, id)`, with their namespace |
| Idempotency keys of runs | `(tenant, namespace, key)` |
| Namespaces, their usage, alerts and script counts | `(tenant, namespace)` |
| Scripts, toolsets | `(tenant, digest)` |
| Workflow versions | `(tenant, version)` |
| Payloads | `(tenant, digest)`; blobs at `{tenant}/sha256/…` |
| Read model, secrets, quotas, keys, tokens, roles, grants | Per tenant |

- **Quotas**: per namespace and per tenant ([quotas](#quotas)).
- **Fairness**: workers draw a tenant at random among those with runnable work, then take
  that tenant's oldest runnable run. A large tenant cannot starve the others, and an end
  user's share of its tenant's queue is bounded by its namespace's `activeRuns`.

### Namespaces

A tenant is a customer of Rewind, and serves its own users, its **end users**. A namespace
isolates one end user: the runs started for them and everything that follows from those runs.
An agent acting for Alice holds a token of Alice's namespace and cannot see or reach anything of
Bob's, whatever script its model writes.

- **Name.** The customer's own id for the end user: 1 to 128 letters, digits, `-`, `_`, `.`, `:`
  and `@`, case-sensitive. `*` is not a name: it stands for the tenant in `usage` and
  `quota_alerts`. A namespace's name appears in webhooks, the dashboard and the service's logs,
  so an opaque id is better than an email.
- **Created at first use**: when a token is minted for it, when a signed token naming it is first
  used, or when a run is created in it, with the tenant's default limits. `PUT
  /v1/namespaces/{name}` creates it ahead, or sets its exceptions.
- **What belongs to a namespace**: its runs, with their journals, signals, idempotency keys,
  tool tasks, webhook tokens, read model rows and payload references; the end user's credentials
  per toolset; its usage and alerts; its tokens. A fork stays in its parent's namespace, and
  nothing a run does reaches another namespace.
- **What stays the tenant's**: toolsets; subscriptions, since lifecycle webhooks go to the
  customer, each event naming its namespace; members, roles, keys and token keys; the storage of
  scripts and payloads, deduplicated per tenant, which a token reads only through its namespace
  (the scripts its runs ran, the payloads its runs hold).
- **The customer names the namespace** with the header `Rewind-Namespace`. Creating in a
  namespace requires it: starting a run, `/v1/check` and `/v1/test`, which apply the namespace's
  limits (400 without it). There is no default namespace. A fork takes its parent's namespace.
  Reading without the header covers every namespace of the tenant; with it, that namespace
  only: a run of another namespace is 404, for a fork, a signal or a cancellation too.
- **An end user's token fixes the namespace.** A header naming another is 403; anything of
  another namespace is 404; lists hold its namespace only; `tasks:work` claims only the tasks of
  its namespace's runs.
- **Deleting a namespace** erases an end user ([retention](#retention-and-deletion)).

### Quotas

Quotas protect the platform and the customer against an end user who abuses the customer's
product. Six limits, each optional:

| Quota | Counts | Above it | Alert |
| --- | --- | --- | --- |
| `cpuPerMonth` | The CPU time of the isolates running the namespace's scripts in the calendar month (UTC), replays excluded | Nothing is refused, nothing is paused | At 80 % and 100 % |
| `activeRuns` | The runs active now | Starting or forking a run: 429 | At the first refusal of the month |
| `runsPerMonth` | The runs created in the month, forks included | 429 until the 1st of the next month | At 80 % and 100 % |
| `scripts` | The distinct scripts with at least one run kept in the namespace: active, ended or archived | 429 for a script new to the namespace, until a run is erased | At 80 % and 100 % |
| `maxTtl` | The TTL a run may ask for; the default TTL is lowered to it | 400 `invalid-request` | |
| `maxScriptLines` | The lines of the source sent | 422 `invalid-script` with a diagnostic, also from `/v1/check` and `/v1/test` | |

Three levels:

- **The tenant's defaults** (`namespace_defaults`), set by the customer (`namespaces:write`), apply
  to every namespace.
- **A namespace's exceptions**, set by the customer: a paying end user, or a blocked one
  (`activeRuns: 0`). Per quota, a namespace's limit is its exception if it has one, else the
  default.
- **The tenant's ceiling** (`quotas`), set by the operator: the customer's offer. For
  `cpuPerMonth`, `activeRuns`, `runsPerMonth` and `scripts` it bounds the tenant's totals, the sum
  over its namespaces; `maxTtl` and `maxScriptLines` bound every namespace.

A start passes its namespace's limit and the tenant's ceiling. `create_run` checks both under a
lock of the namespace and of the tenant, so two concurrent starts cannot take the same last
place. A refusal (`quota-exceeded`) says which quota, at which scope (`namespace` or
`organization`), the limit, the use and when it frees (`resetsAt`, with `Retry-After` for a
monthly quota), in words written for a model that never name another namespace. A retried
request with its idempotency key still returns its run.

- **Counting.** `runsPerMonth` is counted in `usage`, under the lock `create_run` takes.
  `scripts` is counted in `namespace_scripts`: a run adds one to its script's row, erasing a run
  removes one, and a row at zero is removed.
- **CPU time.** The runtime reports the CPU time of each call into the script, which its
  watchdog measures anyway. The engine counts the calls that execute and leaves out those of a
  replay, which are the cost of the instance cache, not the end user's. A worker adds them up per
  tenant, namespace and UTC day and writes them to `usage` about every 10 s, with its lease
  renewals, along with the tenant's own row (namespace `*`). A worker that dies loses at most
  10 s of counting.
- **CPU time is not in the journal.** It measures what the service spent, and differs from one
  replay to the next: it is metering, like service telemetry, and never changes what a run does.
- **Alerts.** A threshold crossed is recorded once per tenant, namespace (or `*`), quota, month
  and threshold, in `quota_alerts`, by the write that crosses it. The worker role sends it to the
  tenant's subscriptions as the event `quota.alert` (`namespace`, `quota`, `threshold`, `used`,
  `limit`, `period`; `namespace` is null for the tenant's ceiling), and the dashboard lists it.
  The customer decides what follows: telling the end user, raising their limits, or blocking
  them.

### Who calls

Every request to `/v1` carries a Bearer token, and nothing else authenticates it (no cookie):

- **An API key**, `rewind_sk_…`, for harnesses, the SDK, its worker and the CLI. Keys are stored
  hashed; each belongs to a tenant, has a name and records who created it.
- **A WorkOS access token**, for the dashboard: a JWT signed RS256, checked locally against
  WorkOS's public keys (JWKS, cached, fetched again for an unknown `kid`): the issuer
  `https://api.workos.com`, the deployment's `client_id`, the expiry with 30 s of leeway. `sub`
  is the user, `sid` the session, `org_id` the tenant. A token without `org_id` reaches only
  the onboarding endpoints (who am I, create an organization, the invitations sent to me).
- **An end user's token**, for an agent acting for one end user, in one of two forms the
  customer chooses:
  - **Opaque**, `rewind_nt_…`, minted by the customer's backend with
    `POST /v1/namespaces/{name}/tokens` (`tokens:write`): one namespace, its permissions, optional
    labels, an expiry (1 hour by default, 30 days at most). Stored hashed, as keys are; its
    SHA-256 is its id.
  - **Signed by the customer**: a JWT signed with a key whose public part the customer registered
    (`POST /v1/organization/token-keys`, `tokens:write`), EdDSA (Ed25519), ES256 or RS256. Rewind
    names the key `rk_…`, and the token's header gives that name as `kid`. Claims: `sub` the
    namespace, `aud` `rewind`, `iat` and `exp` (at most 30 days apart), and optionally
    `permissions`, `labels` and `jti`. The customer mints them without calling Rewind; a key
    records the permissions its tokens may carry.

  A Bearer starting with `rewind_nt_` is an opaque token; a JWT whose `kid` starts with `rk_` is
  a customer's token; any other JWT is a WorkOS access token. `DELETE
  /v1/namespaces/{name}/tokens` revokes the namespace's opaque tokens and refuses its signed
  tokens issued before that moment (`tokens_revoked_before`); `DELETE /v1/tokens/{id}` revokes
  one; deleting a token key invalidates every token it signed. Every request reads the token's
  state again: a revocation applies from the next request, and closes an event stream opened
  with the token. The workers delete an opaque token a day after it expired or was revoked.

The dashboard gets its access token from `/auth`, the only routes that read a cookie
([07](07-code-architecture.md#sign-in-auth)). People sign in with GitHub or with a 6-digit code
sent by email (WorkOS Magic Auth), on the dashboard's own pages. Sign-ups are closed: an
unknown person joins the WorkOS waitlist, and the operator's approval sends an invitation. A
member of an organization invites others by email.

The embedded server is the exception: its socket takes no token, and its one caller is the
operator of the tenant `local` ([embedded server](07-code-architecture.md#embedded-server-and-cli)).

### What the caller may do

```
principal (a member, a key, an invitation) ──► grant (role, labels) ──► role ──► permissions
                                                                               │
endpoint: require(permission), then the grant's labels on what it touches ◄────┘
```

- **Permissions** are Rewind's catalog, one per kind of action, defined in `rewind-api`. Every
  endpoint requires one. A new endpoint either uses one of them or adds one to the catalog.
- **Roles** are sets of permissions. Four are built in and never change; an organization
  creates its own from the catalog. A role's name is 1 to 64 lowercase letters, digits, `-`
  and `_`, and never a built-in name or `operator`.
- **A grant** gives one role, and optionally allowed labels, to one principal in one tenant: a
  member (`user_…`), an API key, or an invitation not yet accepted. A principal has at most one
  grant per tenant. A grant whose role no longer exists gives no permission.

| Permission | Lets the caller | owner | harness | worker | reader |
| --- | --- | --- | --- | --- | --- |
| `runs:read` | Read runs, their statistics and failure groups, a run's state, trace, entries, event stream and journal, the scripts, the tool statistics, payloads | yes | yes | | yes |
| `runs:start` | Start and fork runs; `/v1/check`, `/v1/test` | yes | yes | | |
| `runs:control` | Send signals, cancel | yes | yes | | |
| `runs:quarantine` | Release a quarantined run or give it up | yes | yes | | |
| `runs:delete` | Erase a run | yes | yes | | |
| `toolsets:read` | List toolsets and workflows, read their declarations, search their tools | yes | yes | | yes |
| `toolsets:write` | Register a toolset, save a workflow | yes | yes | | |
| `credentials:write` | Set the credentials of a toolset, a namespace or a run | yes | yes | | |
| `tasks:work` | Claim host-tool tasks, heartbeat, defer, complete, fail | yes | yes | yes | |
| `tasks:approve` | Approve or reject a parked task | yes | yes | | |
| `subscriptions:read` | List the lifecycle webhooks | yes | yes | | yes |
| `subscriptions:write` | Create and delete them | yes | yes | | |
| `members:read` | List the members, their grants and the invitations | yes | yes | | yes |
| `members:invite` | Invite, resend and revoke invitations | yes | | | |
| `members:manage` | Change a member's grant, remove a member | yes | | | |
| `roles:read` | List the roles | yes | yes | | yes |
| `roles:write` | Create, change and delete the organization's roles | yes | | | |
| `api_keys:read` | List the tenant's keys | yes | | | |
| `api_keys:write` | Create and revoke keys | yes | | | |
| `organization:write` | Rename the organization, set its default role | yes | | | |
| `namespaces:read` | List the namespaces, their limits, usage, alerts and tokens in use | yes | yes | | yes |
| `namespaces:write` | Set the default limits and a namespace's exceptions, delete a namespace | yes | yes | | |
| `tokens:write` | Mint and revoke end users' tokens; register and delete token keys | yes | yes | | |

The roles `harness`, `worker` and `reader` give exactly what the key roles of the same names
gave before roles became data. A grant applies in every namespace of its tenant: the customer
holds all rights on its namespaces, and there is no grant per namespace.

Rules, enforced by the server:

- **Labels.** A grant limited to labels sees only the runs carrying all of them: other runs are
  missing (404), the read model's lists filter them, and what the principal creates (runs,
  subscriptions) gets them; a contradicting label is refused (403). Such a grant cannot claim
  tasks, read tool statistics, nor use the permissions `members:invite`, `members:manage`,
  `roles:write`, `api_keys:*` and `organization:write` (403).
- **Workflows.** A workflow saved under a grant limited to labels carries those labels. Such a
  principal only lists, reads and pins the workflows carrying its labels, and a run it starts
  takes them as its **scope**: the scripts of that run and of the runs it starts only reach runs
  carrying them ([workflows](03-execution.md#workflows-across-runs)).
- **No one gives more than they hold.** Creating a key, inviting someone, setting a grant or
  writing a role requires holding every permission of the role given. Changing or removing a
  principal's grant also requires holding every permission of its current role.
- **An organization keeps an owner.** A write that would leave a tenant without a grant of the
  role `owner` is refused, atomically in the store.
- **A role in use stays.** A role of the tenant cannot be deleted while a grant points to it.
  Changing a role changes at once what every principal holding it may do.
- **No grant, no access.** A member without a grant gets 403 on the tenant's data. Members get
  their grant at their first session in the organization: the pending grant of the invitation
  they accepted, else the organization's default role (`reader` unless changed). The creator of
  an organization gets `owner`.
- **Every request reads the grant.** A grant removed or narrowed applies from the next request,
  without waiting for the access token to expire. An event stream opened with an access token
  is closed at the token's expiry, and the dashboard resumes it with `from`.
- **The operator.** The key given with `--admin-key` has the role `operator`: every permission
  in its own tenant, the keys of every tenant (`POST /v1/api-keys` with a `tenant`), the ceiling
  of every tenant (`PUT /v1/organizations/{tenant}/quotas`), and the stop of the embedded server.
  `operator` is not a role of any organization; only an operator creates an operator key.
- **Tokens.** A token holds the permissions it was given, among `runs:read`, `runs:start`,
  `runs:control`, `runs:quarantine`, `runs:delete`, `toolsets:read`, `credentials:write`,
  `tasks:work` and `tasks:approve`, by default `runs:read`, `runs:start`, `runs:control` and
  `toolsets:read`, and only within its namespace. It never holds another permission. Whoever mints
  a token or registers a token key must hold every permission it gives; a signed token that asks
  for a permission its key may not give is refused (401). A token's labels work as a grant's.
  `credentials:write` in a token covers its runs and its namespace's credentials.

## Retention and deletion

Active runs are never touched. Defaults, configurable per tenant:

| Data | Kept |
| --- | --- |
| A finished run's journal | 30 days in Postgres (or Turso). Then archived to S3 as a [journal document](02-journal.md#stored-format-v1) for 1 year, and loaded on demand |
| The read model | 90 days |
| Payloads | As long as a run's journal, live or archived, references them; deleted 24 h after the last reference is gone |
| An end user's opaque token | Until 24 h after it expired or was revoked; a namespace's erasure revokes its tokens |

- `DELETE /v1/runs/{id}` erases a run at any time: its journal, its archive, its read model
  rows, its credentials, its payload references and its count in `namespace_scripts`. Tool
  arguments can hold personal data.
- `DELETE /v1/namespaces/{name}` erases an end user and answers 202. The namespace stops taking
  runs and tokens (409 `namespace-deleting`); its tokens are revoked and its active runs
  cancelled through the normal path; then, in the background, its runs are erased as `DELETE
  /v1/runs/{id}` erases one, then its credentials, its usage and alerts, and the namespace. The
  tenant's own usage row keeps what the namespace consumed: deleting an end user gives nothing
  back to the tenant's ceiling.
- A `payload_refs` table, filled at commit, records which runs reference a payload. Cleanup
  deletes the payloads that no run has referenced for 24 h, per tenant.
- A blob delete cannot be undone or fenced, so no commit may count on a blob being deleted. A
  large payload's row is reserved before its blob is written, which restarts its grace; cleanup
  marks a row, deletes its blob, then the row. A payload whose deletion has started is
  committed with its value in its row instead ([decided](08-decisions.md#taken-while-fixing-the-review-of-the-engine-and-the-stores-november-2026)).
- A finished run is archived only once the projector has applied all its feed items: the read
  model, the traces and the webhooks read its history until then.

## Security

Scripts are untrusted (model-generated). They have no ambient authority: every
capability comes from the toolset, and secrets never enter the isolate
([credentials](04-tools.md#credentials)).

A run's seed keys its random generators, `crypto` included. It is protected like the
payloads.

A deployed server also sends requests where a tenant says: to its MCP servers (from the API
role at registration, from the workers at each call) and to its lifecycle webhooks. Neither goes
to the internal network: both go through the egress checks of the `http` tool, a name resolved
once, private, loopback, link-local and metadata addresses refused, the connection to the
checked address ([MCP servers](04-tools.md#where-mcp-servers-may-be)). Only the operator opens
the way: to internal MCP servers by host or network (`--allow-internal-mcp`), whose answers are
then never quoted to the tenant, and to webhook receivers on the private network
(`--allow-private-webhooks`). The embedded server, for one local developer, reaches any address.

### Sandboxed workers

A V8 isolate is not a hard boundary against V8 vulnerabilities. Only workers run code
written by a model, so only workers are sandboxed.

The same binary takes a role at start:

| Role | Runs | V8 |
| --- | --- | --- |
| `rewind server --role api` | HTTP API, authentication, SSE | No |
| `rewind server --role worker` | V8, dispatcher, scheduler, projector | Yes |

- Workers run under gVisor (`runsc`). A sandbox runs its own network stack: it takes over the
  network namespace it starts in, and reaches no DNS server on the host's loopback, such as
  Docker's: a worker's DNS server stands on its network.
- They run in pools per tenant when needed. Trusted tenants share a pool.
- They reach the outside only through the egress proxy, and have no route to the cloud
  metadata endpoint ([egress](04-tools.md#the-http-tool-and-egress)).
- Their container has a memory limit. The sandbox bounds each instance's heap and array
  buffers, but V8 runs some built-ins to their end once the limit is reached, and aborts the
  process on a few results too large for it
  ([limits](03-execution.md#resource-limits)): the container bounds what is left.
- After an upgrade of V8 or ICU, a worker pool of the previous
  [epoch](03-execution.md#determinism-epoch) stays up until its runs end, at their TTL.

Three walls stand between two tenants: V8, gVisor, and separate pools.
