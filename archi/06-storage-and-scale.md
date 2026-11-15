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

Every key starts with the tenant ([tenancy](#tenancy-identity-and-access)).

```
tenants    (id PK, data_key, active_run_quota, retention)   -- data_key encrypted by the master key
api_keys   (hash PK, tenant, name, created_by, created_at, revoked_at)
roles      (tenant, name, description, permissions, created_at, updated_at)   PK (tenant, name)
grants     (tenant, principal, role, allowed_labels, granted_by, granted_at)   PK (tenant, principal)
                                                           -- principal: user:…, key:…, invitation:…
access_settings (tenant PK, default_role)
scripts    (tenant, digest, source, transpiled, source_map)   PK (tenant, digest)  -- only scripts that compile
toolsets   (tenant, digest, spec, declarations)               PK (tenant, digest)
workflows  (tenant, version, name, document, labels, created_at)   PK (tenant, version)
                                                           index on (tenant, name, created_at)
runs       (tenant, id, script, toolset, status, epoch, history_len, blocked_on, runnable_at,
            expires_at, quarantine, failures, attempts, lease_owner, lease_until, fence,
            sticky_worker,
            parent, workflow, workflow_version, started_by, started_seq, detached, scope,
            labels, trace_context, idempotency_key, created_at, updated_at, ended_at, archive)
            PK (tenant, id), UNIQUE (tenant, idempotency_key)
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
secrets    (tenant, owner, ciphertext)                     PK (tenant, owner)   -- owner: a toolset or a run
subscriptions (tenant, id, url, secret, events)
feed       (id, consumer, run_id, fence, from_idx, to_idx, lease_owner, lease_until)
meta       (key PK, value)                                 -- schema version; virtual time (embedded)

-- read model, written only by the projector
run_summaries (run_id PK, script, toolset, status, labels, started_at, ended_at,
               last_activity_at, tool_calls, errors, history_len)
tool_calls    (run_id, seq, tool, callsite_line, scheduled_at, completed_at, outcome,
               error_kind, args_digest, result_digest)      PK (run_id, seq)
```

`event` columns hold the `rewind-format` v1 JSON of the event. `labels`, `trace_context`,
`idempotency_key`, `parent`, `workflow`, `workflow_version`, `started_by`, `started_seq`,
`detached` and `scope` are run metadata: stored with the run, never in its events
([05](05-testing-and-observability.md#run-metadata)). A `workflows` row holds a
[workflow version](02-journal.md#stored-format-v1) document. `run_watchers` lists the runs that
receive a run's end ([workflows](03-execution.md#workflows-across-runs)): the run that started
it (`attached` unless it was started detached), and the forks that copied that start.

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
  outside quarantine, with the [fairness](#tenancy-identity-and-access) between tenants and
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
| Start a run | Unique per tenant | Returns the existing run | A second run starts and repeats every effect |
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

## Tenancy, identity and access

Three questions are kept apart:

| Question | Answered by |
| --- | --- |
| Who calls? | An API key, or a member's session with WorkOS |
| Which tenant? | The key's tenant, or the member's organization |
| What may the caller do? | Rewind alone: the caller's grant, its role and the role's permissions |

WorkOS holds identity and nothing else: users, organizations, memberships, invitations and
the waitlist. It never says what someone may do. Rewind configures no role or permission there,
and never reads the role of a membership nor the `role`, `roles` and `permissions` claims of an
access token ([decided](08-decisions.md#taken-for-accounts-organizations-and-access-november-2026)).

### Tenants and organizations

A tenant is a WorkOS organization: the tenant's id is the organization's id (`org_01J…`, which
fits the tenant ids' alphabet). Rewind copies no user, organization or membership: WorkOS is
their only source. A tenant's record (quota, retention) is created with the organization, or
at the first request of a member of an organization that has none.

The tenant is part of every storage key:

| Data | Key |
| --- | --- |
| Runs and their metadata | `(tenant, id)` |
| Idempotency keys of runs | `(tenant, key)` |
| Scripts, toolsets | `(tenant, digest)` |
| Workflow versions | `(tenant, version)` |
| Payloads | `(tenant, digest)`; blobs at `{tenant}/sha256/…` |
| Read model, secrets, quotas, keys, roles, grants | Per tenant |

- **Quota**: each tenant has a maximum number of active runs. Above it, starting a run
  returns 429.
- **Fairness**: workers draw a tenant at random among those with runnable work, then take
  that tenant's oldest runnable run. A large tenant cannot starve the others.

### Who calls

Every request to `/v1` carries a Bearer token, and nothing else authenticates it (no cookie):

- **An API key**, `rewind_sk_…`, for harnesses, the SDK, its worker and the CLI. Keys are stored
  hashed; each belongs to a tenant, has a name and records who created it.
- **A WorkOS access token**, for the dashboard: a JWT signed RS256, checked locally against
  WorkOS's public keys (JWKS, cached, fetched again for an unknown `kid`): the issuer
  `https://api.workos.com`, the deployment's `client_id`, the expiry with 30 s of leeway. `sub`
  is the user, `sid` the session, `org_id` the tenant. A token without `org_id` reaches only
  the onboarding endpoints (who am I, create an organization, the invitations sent to me).

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
| `credentials:write` | Set the credentials of a toolset or a run | yes | yes | | |
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

The roles `harness`, `worker` and `reader` give exactly what the key roles of the same names
gave before roles became data.

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
  in its own tenant, the keys of every tenant (`POST /v1/api-keys` with a `tenant`), and the
  stop of the embedded server. `operator` is not a role of any organization; only an operator
  creates an operator key.

## Retention and deletion

Active runs are never touched. Defaults, configurable per tenant:

| Data | Kept |
| --- | --- |
| A finished run's journal | 30 days in Postgres (or Turso). Then archived to S3 as a [journal document](02-journal.md#stored-format-v1) for 1 year, and loaded on demand |
| The read model | 90 days |
| Payloads | As long as a run's journal, live or archived, references them; deleted 24 h after the last reference is gone |

- `DELETE /v1/runs/{id}` erases a run at any time: its journal, its archive, its read model
  rows, its credentials and its payload references. Tool arguments can hold personal data.
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
