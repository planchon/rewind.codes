# Reference deployment

A production-shaped deployment of Rewind on one Docker host, with Docker Compose: the shape
`archi/06-storage-and-scale.md` describes, small enough to read in one sitting. Copy it and
change what your platform does differently (managed Postgres and S3, your own collector, a
Kubernetes egress policy instead of the gateway container).

[`railway/`](railway/README.md) is such a copy: the hosted deployment of Rewind, on Railway and
Vercel.

| Service | Image | Networks | Role |
| --- | --- | --- | --- |
| `edge` | `caddy:2-alpine` | public, core | The only published port. Sends each request to the next node |
| `node-1`, `node-2` | `rewind` | a holder's namespace | `rewind server`: each an HTTP edge (the API, keys, the dashboard) and a node of the run plane (the owners of its partitions, the organization actors homed there, executors with V8, the calls of actions, timers, retention), linked to the other by the node link |
| `node-N-net` | `rewind-egress` | core | Holds a node's network namespace, with its default route through `egress`; healthy once the route is set |
| `egress` | `rewind-egress` | core, public | The nodes' only way out: forwards to the addresses of allowed names only |
| `node-dns` | `rewind-egress` | core | Profile `gvisor`: the DNS server of the nodes under gVisor, a relay to Docker's ([gVisor](#nodes-the-walls)) |
| `walls`, `walls-net` | `rewind-egress` | a holder's namespace | Profile `walls`: the gVisor sandbox `check-walls.sh` checks the gVisor nodes' walls from, and its holder; started for the check only |
| `postgres` | `postgres:17` | core | The control plane (`rewind-store-postgres`): organizations (its `tenants`), namespaces, keys, grants, code, action sets, workflows, secrets, subscriptions, the nodes and the partition map. The nodes' liveness locks, in its database `postgres`. It never holds a run |
| `minio`, `minio-init` | Chainguard's MinIO | core | The bucket (S3): the partitions (SlateDB, `partitions/`), the archive of ended runs (`runs/`), the payloads above 64 KiB and the test documents (`blobs/`, `tests/`); and the bucket's creation |
| `otel-collector` | `otel/opentelemetry-collector` | core | Traces, metrics and logs of the nodes, over OTLP |
| `node-previous` | `rewind`, a previous build | a holder's namespace | Profile `previous-epoch` (`previous-epoch-gvisor` under gVisor): `rewind server --executors-only`, the executors of a previous determinism epoch ([upgrades](#upgrades)) |
| `loadgen` | `rewind-load` | public | Profile `load`: the load generator and its fake MCP server ([load tests](#load-and-limit-tests)) |

`core` is an internal network: nothing on it has a route out. `public` is an ordinary bridge.

## Quick start

```sh
cd deploy
./init.sh                         # .env with fresh secrets, once (./init.sh <file> writes another file)
docker compose up -d --build      # builds the image from the repository (a release build: minutes)
./check-walls.sh                  # the nodes' network walls hold (node-1's; ./check-walls.sh node-2-net)
```

The API and the dashboard are then at <http://localhost:8080> (`REWIND_PORT`, `REWIND_BIND`). The
image is the repository's `Dockerfile`: `docker build -t rewind:local .` builds it alone, and
`REWIND_IMAGE` names another one (a registry tag) for `docker compose up -d --no-build`.

## The image

One image for every node: `rewind server`, an HTTP edge and a node in one process
(`rewind server --help` lists every option). It holds the `rewind` binary only, with the dashboard
and tsgo embedded, on a distroless base (glibc, CA certificates, no shell), and runs as the user
`nonroot` (65532). It sets `TZ=UTC`, which the sandbox requires, and `REWIND_LISTEN=0.0.0.0:8080`.

Every service of the image runs with a read-only root file system, no capability and
`no-new-privileges`. A node type-checks code with tsgo, which it extracts at its first check
into `/var/cache/rewind` (`REWIND_TSGO_CACHE`): give it a writable directory that allows execution,
as the tmpfs of `compose.yaml` does.

## Configuration

Every option of `rewind server` also reads an environment variable (`REWIND_*`). `compose.yaml`
gives each node what it needs:

| Variable | In `compose.yaml` | What |
| --- | --- | --- |
| `REWIND_STORE` | `postgres://rewind:…@postgres:5432/rewind` | The control plane |
| `REWIND_CLUSTER_DATABASE` | (`postgres`, its default) | The database of the same Postgres server where the nodes take their liveness locks, where nothing else lives |
| `REWIND_BUCKET` | `s3://rewind-payloads` | The bucket of the partitions, the archive and the blobs, with `REWIND_S3_ENDPOINT`, `REWIND_S3_REGION`, `REWIND_S3_ALLOW_HTTP` and the two keys `REWIND_S3_ACCESS_KEY_ID`, `REWIND_S3_SECRET_ACCESS_KEY` (`auto` is R2's region) |
| `REWIND_CLUSTER_SECRET` | from `.env` | The secret every node shares, at least 32 characters: it authenticates the node link. Required with Postgres |
| `REWIND_NODE_ID` | `node-1`, `node-2` | The node's id, and `service.instance.id` of its telemetry; the host name and the process id by default |
| `REWIND_LINK_LISTEN`, `REWIND_LINK_ADDRESS` | (`0.0.0.0:7070`), `node-N-net:7070` | Where the node link listens, and where the other nodes reach it on the private network (the host name and the port of `REWIND_LINK_LISTEN` by default) |
| `REWIND_PARTITIONS` | (64, its default) | The partitions of the deployment. Fixed once the deployment holds runs: more only serve the namespaces created after them |
| `REWIND_OTLP_ENDPOINT` | `http://otel-collector:4318` | The collector |

`.env` (written by `init.sh`) or your shell sets these:

| Variable | Default | What |
| --- | --- | --- |
| `REWIND_IMAGE` | `rewind:local` | The image of the nodes |
| `REWIND_BIND`, `REWIND_PORT` | `127.0.0.1`, `8080` | Where the edge listens on the host |
| `REWIND_PUBLIC_URL` | `http://localhost:8080` | The URL clients reach the edge at: `webhook()` URLs start with it |
| `REWIND_EGRESS_ALLOW` | `loadgen` | The names the nodes may reach: MCP servers, APIs and their OpenAPI documents, hosts of the `http` action, webhook receivers (spaces or commas). A name allows its subdomains |
| `REWIND_ALLOW_INTERNAL_ADDRESSES` | `loadgen` | MCP servers and APIs at private addresses the nodes may use (names, `*.domain`, addresses, CIDR networks; commas). Without it a deployed server reaches MCP servers, APIs and OpenAPI documents at public addresses only |
| `REWIND_QUOTA_ACTIVE_RUNS` | `10000` | The ceiling of the organizations this deployment creates: their active runs across their namespaces; one more is refused with 429. `REWIND_QUOTA_RUNS_PER_MONTH`, `REWIND_QUOTA_CPU_PER_MONTH`, `REWIND_QUOTA_WORKFLOWS`, `REWIND_QUOTA_MAX_TTL` and `REWIND_QUOTA_MAX_CODE_LINES` set the others, none by default |
| `REWIND_DEFAULT_TTL`, `REWIND_MAX_TTL` | `30d`, `365d` | A run's TTL when the harness gives none, and the most it may ask for |
| `REWIND_RETENTION` | `365d` | How long an ended run is kept, for the organizations this deployment creates ([retention](#retention)) |
| `REWIND_EXECUTORS`, `REWIND_CALLS` | `4`, `256` | Activations run at once (executor threads), and calls of actions sent at once, per node |
| `REWIND_NODE_MEMORY`, `REWIND_NODE_CPUS` | `1g`, `2` | A node container's limits |
| `REWIND_CACHE_INSTANCES`, `REWIND_CACHE_MEMORY` | `500`, `384MiB` | The sticky instance cache of a node: keep its memory well below the container's limit, under each isolate's 128 MiB heap limit |
| `REWIND_LOG` | `info` | What the nodes log on standard error, as `RUST_LOG` writes it |
| `COMPOSE_PROFILES` | `runc` | `runc` or `gvisor`, plus `previous-epoch` (under gVisor `previous-epoch-gvisor`) during an upgrade |
| `REWIND_CORE_SUBNET`, `REWIND_CORE_RANGE`, `REWIND_EGRESS_ADDRESS`, `REWIND_NODE_DNS_ADDRESS` | `172.29.213.0/24`, `172.29.213.128/25`, `172.29.213.2`, `172.29.213.3` | The core network, the part of it Docker hands out, and the fixed addresses below that part of the gateway and of `node-dns`. `egress/resolv.conf` names `REWIND_NODE_DNS_ADDRESS`: change it there too |

`rewind server` has more options (`--allow-stdio`, `--otlp-protocol`, `--pool`…): see
`docker run --rm rewind:local server --help`.

**More nodes.** Copy `node-2` and its holder as `node-3` and `node-3-net`, with its own
`REWIND_NODE_ID` and `REWIND_LINK_ADDRESS`, and add `node-3-net:8080` to `edge/Caddyfile`. A node
that joins takes some partitions from the others; one that stops hands its partitions over before
it exits.

## Secrets

`init.sh` writes `.env` with mode 0600; it is ignored by git. Keep it secret.

| Secret | What it protects | If lost |
| --- | --- | --- |
| `REWIND_MASTER_KEY` | 32 random bytes in base64. It encrypts each organization's data key, which encrypts its secrets | Every stored credential is unreadable: back it up with the database. The nodes refuse to start without it |
| `REWIND_CLUSTER_SECRET` | The node link between the nodes, on the private network | A new one, given to every node at once, is enough: nothing is stored under it |
| `REWIND_ADMIN_KEY` | The first API key: role `operator` in the organization `REWIND_ADMIN_ORGANIZATION`, registered when a node starts. It creates the other keys | Revoke it (`DELETE /v1/api-keys/{id}`, its id is `GET /v1/whoami`'s `principal.id`) and set a new one: a revoked key is never registered again |
| `WORKOS_API_KEY`, `WORKOS_COOKIE_PASSWORD` | WorkOS's secret key, and the key of the sign-in's sealed cookies ([sign-in](#sign-in)) | A new cookie password signs everyone out; a new WorkOS key is set in WorkOS first |
| `POSTGRES_PASSWORD`, `MINIO_ROOT_*` | The stores | Change them in the store first, then in `.env` |

The nodes read them from the environment. On a platform with a secret manager, inject them as
environment variables of the containers instead of a file. Never give `REWIND_ADMIN_KEY` nor
`WORKOS_*` to a node started with `--executors-only`: it serves no HTTP route.

## Organizations, keys and roles

An organization (a tenant, as the engine and the store call it) is created with its first key, or
when people sign in ([sign-in](#sign-in)), with the deployment's quota and retention. Every key is `rewind_sk_…`,
shown once, stored hashed.

```sh
. ./.env
curl -s http://localhost:8080/v1/api-keys -H "Authorization: Bearer $REWIND_ADMIN_KEY" \
  -H 'content-type: application/json' \
  -d '{"organizationId": "acme", "role": "harness", "name": "acme harness"}'
```

A key gets what it may do from its grant, given with it: a role, and optionally allowed labels.
Whoever creates a key holds every permission of its role. Four roles are built in; an organization
composes its own from the catalog of permissions (`GET /v1/permissions`, `PUT /v1/roles/{name}`).

| Role | May |
| --- | --- |
| `owner` | Every permission of its organization: its runs, and its members, roles, keys and settings |
| `harness` | Runs, action sets, workflows, secrets, calls, subscriptions, namespaces and tokens, and reading the members and roles. The harness and its agent tools |
| `worker` | The calls of harness actions (`calls:work`): the harness's SDK worker, which holds the call stream open |
| `reader` | Reading runs, workflows, action sets, the secrets' metadata, subscriptions, members and roles: a dashboard |

**The operator.** The key of `REWIND_ADMIN_KEY` has the role `operator`: every permission in its
organization, and the keys of every organization (`POST /v1/api-keys` with an `organizationId`, as above, and
their revocation). `operator` is the deployment's role, never an organization's; only the operator
creates an operator key.

A key may carry `allowedLabels` (`{"conversation": "c-42"}`): it then sees and creates only the
runs carrying them, which is how one conversation's agent tools are scoped; such a key
administers nothing, and reads no statistics (they come from counters, which carry no labels).
Revoke a key with `DELETE /v1/api-keys/{id}`: its grant goes with it.

## Sign-in

With WorkOS, people sign in to the dashboard (GitHub, or a code by email), each in their
organizations: a WorkOS organization is a Rewind organization, with the same id. WorkOS says who someone is;
what they may do is their grant in Rewind, as for a key. Sign-ups go through WorkOS's waitlist:
the operator approves people there, which invites them.

Give every node that serves the API the four variables together, or none (`rewind server` refuses
to start with some of them; without them the server accepts only API keys). Add them to
`x-edge-env` in `compose.yaml`, never to `node-previous`:

| Variable | Value |
| --- | --- |
| `WORKOS_CLIENT_ID` | The WorkOS environment's client, `client_…` |
| `WORKOS_API_KEY` | Its secret key, `sk_…` |
| `WORKOS_COOKIE_PASSWORD` | 32 random bytes in hexadecimal (`openssl rand -hex 32`) |
| `WORKOS_REDIRECT_URI` | The dashboard's origin and `/auth/callback` (`http://localhost:8080/auth/callback`), registered in WorkOS |

In the WorkOS environment, set:

- **Authentication methods**: GitHub OAuth and Magic Auth, the two the sign-in page offers.
  Without Magic Auth, a code by email is refused (`authentication_method_not_allowed`).
- **Sign-ups**: the Waitlist. Without it, anyone who signs in gets an account.
- **Redirects**: the redirect URI above, and the *User invitation URL* set to the dashboard's
  origin (`http://localhost:8080/`). WorkOS adds `?invitation_token=…`, which the sign-in page
  reads. Left unset, members' invitations link to WorkOS's hosted AuthKit.

The invitation WorkOS sends when the operator approves someone on the waitlist links to hosted
AuthKit whatever the *User invitation URL*. The approval creates the person's account: tell them
to sign in on the dashboard with the approved email. A sign-in completed on AuthKit comes back to
`/auth/callback` without the dashboard's `state`; the page then asks to sign in again.

The sign-in's cookies are `Secure`: browsers keep them on `https://` origins and on
`localhost`. A node passes WorkOS the address of each browser that signs in: the last of
`X-Forwarded-For`, which the edge writes with the address that reached it (Caddy ignores the one
a client sends; the edge is the only published port), when it is a public address.

An organization's ceiling is the operator's offer: the admin key changes it with `PUT
/v1/organizations/{organization}/quotas`. Its retention is kept in its `tenants` row of the control
plane: change an existing organization there.

```sh
curl -X PUT "$REWIND_PUBLIC_URL/v1/organizations/acme/quotas" \
  -H "Authorization: Bearer $REWIND_ADMIN_KEY" -H 'Content-Type: application/json' \
  -d '{"ceiling": {"activeRuns": 50000, "runsPerMonth": 2000000}}'
```

**A pool per organization.** `rewind server --pool <name>` starts a node of a pool of executors,
and `--organization-pool <organization>=<pool>` (`REWIND_ORGANIZATION_POOLS`, the same list on every
node) sends that organization's activations there. Give an untrusted organization its own pool, on
its own hosts.

## Nodes: the walls

Every node runs code written by a model. Three walls stand between two organizations: V8, gVisor, and
separate pools (`archi/06-storage-and-scale.md`, "Sandboxed nodes").

- **Network.** A node joins the network namespace of its holder (`node-N-net`), whose only
  routes are the core network and a default route through `egress`. Its DNS server answers
  the names of the core network and asks `egress` for every other one. `egress` answers only
  the names of `REWIND_EGRESS_ALLOW`, adds each address it answers to the set of allowed
  addresses, and forwards new TCP connections from the core network to that set only, never to
  `169.254.0.0/16` (the cloud metadata endpoint). The nodes need no proxy setting: the
  gateway is a rule of their network. The `http` action still checks its own allow-list and
  refuses private addresses (`archi/04-tools.md`). `./check-walls.sh` checks it all from inside
  a node's namespace (under gVisor, from a sandbox like the nodes'). The gateway needs a
  host that does not pass bridged traffic through iptables: with `br_netfilter` and
  `net.bridge.bridge-nf-call-iptables=1`, Docker's rules for the internal network `core` drop
  the nodes' packets to the gateway, and allowed names are unreachable too.
- **Process.** Read-only root, no capability, `no-new-privileges`, a memory and CPU limit. The
  holder alone has `NET_ADMIN`, to set its route at start.
- **gVisor.** `docker compose --profile gvisor up -d` runs the same nodes under `runsc`, on a
  host whose Docker has the runtime ([install](https://gvisor.dev/docs/user_guide/install/)),
  behind the same walls. A gVisor sandbox runs its own network stack, so three things differ:
  - The sandbox copies the addresses and routes of its holder's namespace when it starts, and
    removes them from the namespace. So a gVisor node starts once its holder is healthy,
    with its route set, and the holder is left with an empty namespace.
  - The sandbox cannot reach Docker's DNS server, at `127.0.0.11`: that address is the
    sandbox's own loopback ([gVisor's FAQ](https://gvisor.dev/docs/user_guide/faq/#docker-bridge)).
    The nodes' DNS server is `node-dns` instead (`egress/resolv.conf`). It stands where a
    holder stands, on the core network only with `egress` as its DNS server, and relays every
    question to Docker's DNS server. So it answers what Docker's answers a node under runc:
    the core network's names, and every other name through `egress`.
  - Nothing else can run in a node's namespace. `./check-walls.sh` checks the walls from
    `walls`, a gVisor sandbox with the nodes' network settings in a holder of its own. It
    starts both for the check and removes them after.

  The profile is not tested on Docker Desktop or OrbStack, which have no `runsc`: test it on a
  Linux host (`COMPOSE_PROFILES=gvisor`, `./check-walls.sh`, then the loads of
  [Load and limit tests](#load-and-limit-tests)).

**The nodes' allow-list.** Add your MCP servers, the APIs and OpenAPI documents your action sets
name, the hosts your action sets' `http` action allows, and the receivers of your webhook
subscriptions to `REWIND_EGRESS_ALLOW`, then `docker compose up -d egress`: a node lists an MCP
server's tools and reads an OpenAPI document when an action set is registered, and calls them for
runs, through the gateway. An MCP server or an API at a private address must also be in
`REWIND_ALLOW_INTERNAL_ADDRESSES`: the nodes check where MCP servers and APIs are, as the `http`
action does. S3, Postgres, the collector and the other nodes are on the core network here; a managed S3
or collector outside it must be allowed too.

## Upgrades

A build supports a set of determinism epochs (`GET /v1/server`'s `epochs`), and new runs start
under its latest. Most releases keep the epochs: replace the image and `docker compose up -d`. A
stopping node hands its partitions over to the other nodes and lets the requests in flight end
(30 s at most); the node that takes a partition over recovers its runs from their records.

After a release that changes V8 or ICU, the new build does not run the previous epoch, whose
runs need executors of the previous build until they end (`archi/03-execution.md`, "Determinism
epoch"). `node-previous` is such a node: `rewind server --executors-only`, it owns no partition
and serves no HTTP route, and the owners send it, over the node link, the activations of the runs
its build supports:

1. Keep the current image under its own tag: `docker tag rewind:local rewind:previous` (or pin
   its registry digest), and set `REWIND_PREVIOUS_IMAGE=rewind:previous` and
   `COMPOSE_PROFILES=runc,previous-epoch` in `.env` (`gvisor,previous-epoch-gvisor` under
   gVisor).
2. Build or pull the new image as `REWIND_IMAGE`, then `docker compose up -d`: the nodes run the
   new build, and `node-previous` the previous one.
3. Keep it `REWIND_MAX_TTL` after the upgrade: every run has a deadline, so the runs of the
   previous epoch have ended by then; a run still quarantined at its deadline is aborted.
4. Remove `previous-epoch` (or `previous-epoch-gvisor`) from `COMPOSE_PROFILES` and
   `docker compose up -d --remove-orphans`.

The previous executors share the control plane: a build refuses a schema newer than its own, so a
release that migrates the schema cannot keep executors of a build older than the migration. Such
a release and a change of V8 or ICU go in separate releases.

**The deploy that moves runs to partitions.** The build that runs on partitions moves the run
tables of a deployment's Postgres into a schema `legacy` when it first opens it, and
`rewind server migrate-runs` moves those runs once: the ended ones into the archive, the live ones
into their partitions, each with its payloads, then drops `legacy`. Run it once, with the nodes'
options, before the nodes start; run again, it skips each run it already moved:

```sh
. ./.env
echo "REWIND_CLUSTER_SECRET=$(openssl rand -hex 32)" >> .env   # the deployment's nodes share it
docker compose down                                            # the data stays
docker compose run --rm node-1 server migrate-runs             # with node-1's options
docker compose up -d --remove-orphans
```

## Rename release

The rename release gives Rewind the vocabulary of `archi/` (a workflow and its code, an action
set and its actions, a history and its events, a harness action, a call's delivery, the statuses
`queued`, `running` and `waiting`…), in the API, the SDK and the stored data alike
(`archi/09-roadmap.md`, "The rename release"). What a deployment stored under the old names is
rewritten once, offline, by `rewind server rename-data`, with the new build's image, between the
old nodes stopping and the new ones starting (`archi/06-storage-and-scale.md`, "Rewriting the
stored names"):

- opening the control plane migrates its schema (Postgres 17, Turso 15): the tables `code` and
  `action_sets` (were `scripts` and `toolsets`), a partition's `generation`, the quotas
  `workflows` and `max_code_lines` (were `scripts` and `max_script_lines`), a subscription's
  `types` with the webhook types `run.waiting`, `secret.expired` and `action.schema_changed`,
  the principals `member:` (were `user:`), the permissions `actions:*` and `calls:*`;
- the command refuses to start while a node serves the deployment, or while runs wait in the
  tables `migrate-runs` moves; it then rewrites the catalog's tests, workflow versions and pinned
  action sets, each under the digest of its new form (an old digest stops resolving; names and
  version numbers keep working), gives every run a workflow and moves each saved workflow by its
  use, rewrites every record of every partition (a run that was `active` is `running` or
  `waiting` from the waits its head holds) and every sealed attempt of the archive, and prints
  what it changed, and the titles it could not turn into names;
- run again, it finds everything in the new names and says `nothing to rename`; with
  `--dry-run` it counts what it would change, by kind, and writes nothing.

**Runs not yet moved to partitions come first.** A deployment whose Postgres still holds runs
in its schema `legacy` (one that skipped the deploy that moves runs to partitions, or whose
`migrate-runs` did not finish) moves them with `rewind server migrate-runs` **of the build
before the rename release**, nodes stopped, before it runs `rename-data`: `rename-data` refuses
while such runs wait, and says so. The same goes for an embedded server whose `.rewind/` still
holds `journal.db`: `rewind server migrate-runs --embedded` with the previous `rewind`, then
`rewind server rename-data --embedded` with the new one. A deployment that already moved its
runs has nothing to do.

```sh
docker compose down
REWIND_IMAGE=rewind:previous docker compose run --rm node-1 server migrate-runs   # the build before the release
REWIND_IMAGE=rewind:new docker compose run --rm node-1 server rename-data
```

Code is never rewritten: the runs started before the release replay under the determinism epoch
1, whose glue still names `ToolError`, `WorkflowError` and `webhook`; new runs start under
epoch 2. Both epochs are in the new build: no `node-previous` is needed.

**Rehearse it on a copy of production first.** Copy the bucket's prefix and restore the
database into a scratch deployment, run the command against them, and start a node on the copy:

```sh
# The bucket: the partitions, the archive and the blobs, into a prefix of their own.
aws s3 sync s3://$REWIND_S3_BUCKET/ s3://$REWIND_S3_BUCKET-rehearsal/
# The control plane: a dump of production, restored into a scratch database.
pg_dump --format=custom "$PRODUCTION_DATABASE_URL" > rewind.dump
createdb rewind_rehearsal && pg_restore --no-owner --dbname=rewind_rehearsal rewind.dump
# With the new image, against the copies: count, rename, then run again to see nothing left.
REHEARSAL="--store postgres://…/rewind_rehearsal --bucket s3://$REWIND_S3_BUCKET-rehearsal"
docker run --rm --env-file .env rewind:new server rename-data --dry-run $REHEARSAL
docker run --rm --env-file .env rewind:new server rename-data $REHEARSAL
docker run --rm --env-file .env rewind:new server rename-data $REHEARSAL   # nothing to rename
```

Then start one node of the new build on the copy (its own `--node-id`, `--cluster-database`
and port), and check the runs: `GET /v1/runs?q=status:active` lists the waiting ones with their
`waitingFor`, a run waiting on a signal ends when it is signalled, and `GET /v1/workflows` lists
the saved workflows under their names and versions.

**At the release:**

```sh
docker compose down                                            # every node stops; the data stays
docker compose run --rm node-1 server rename-data --dry-run   # with node-1's options
docker compose run --rm node-1 server rename-data
REWIND_IMAGE=rewind:new docker compose up -d --remove-orphans
```

On Railway, stop the `node` service (its volume makes a deploy stop the old node first), rename
the variables the release renamed (`REWIND_ADMIN_ORGANIZATION`, `REWIND_ORGANIZATION_POOLS`,
`REWIND_QUOTA_WORKFLOWS`, `REWIND_QUOTA_MAX_CODE_LINES`, `REWIND_ALLOW_INTERNAL_ADDRESSES`), run
`rewind server rename-data` once from a one-off service on the new image with the node's
variables, then deploy the node on the new image. An embedded server's `.rewind/` is renamed with
`rewind server rename-data --embedded` while no `rewind` command serves it.

## Retention

Active runs are never touched. Each organization's retention applies to its ended runs
(`archi/06-storage-and-scale.md`, "Retention and deletion"):

| Data | Kept | Setting |
| --- | --- | --- |
| An ended run: its summary in its partition's index, the history of each attempt sealed in the bucket (`runs/`), its payloads (`blobs/`) | 1 year after it ended, then the run is erased by its owner, at a timer written when it was sealed | `REWIND_RETENTION` |
| An end user's opaque token | 24 hours after it expired or was revoked | |
| A test no workflow version references | 24 hours after it was stored | |

The setting applies to the organizations created after it changes; update the `tenants` row of an
existing one ([above](#sign-in)). `DELETE /v1/runs/{id}` erases one run at any time, its sealed
histories and payloads included. Back up Postgres and the bucket together, with the master key.

## Observability

The nodes export service telemetry (activation, replay, call delivery spans and their metrics) and
run telemetry (one trace per run, projected from its history) over OTLP to `otel-collector`, whose
`debug` exporter prints them: replace it in `otel-collector.yaml` by your backend's. A run's
trace joins the harness's when the harness passes `traceparent`.

## Load and limit tests

```sh
deploy/acceptance.sh
```

runs criterion 6 of 0.1 ("Load tests and limit tests pass", which `acceptance/run.sh` runs too):
the limit tests through the API, then this deployment as a Compose project of its own (its own
port, `REWIND_ACCEPTANCE_PORT`, 18580 by default; its own network and secrets), the walls, two
loads with the thresholds recorded in `archi/09-roadmap.md` ("Load tests"), and the agent tools'
bound against it. It removes its containers, volumes and images at the end
(`REWIND_ACCEPTANCE_KEEP=1` keeps them), and exits with 0 only when every step passed.

`rewind-load` drives a deployment through its API: it registers an action set with its own fake MCP
server (`fake.echo`, and `fake.flaky`, which fails transiently once per call) and a harness action it
answers itself on the call stream (`host.square`), starts many runs that each call the three actions,
sleep, and read a signal sent twice with one idempotency key, waits for every run, then checks
that each ended as expected and that no call reached its action more times than its failures
call for (`flaky` twice, the others once). It reports throughput, latencies (start, first
activation, activation, end to end) and errors, and exits with 1 when an invariant broke or a
threshold given (`--min-runs-per-s`, `--max-p99-end-to-end`, `--max-p99-activation`) was missed. `--payload-bytes` pads each run's payloads: above 1 KiB they
are stored apart from their event, above 64 KiB in S3.

```sh
docker compose --profile load build loadgen
docker compose --profile load run --rm --no-deps --use-aliases loadgen run --runs 2000 --concurrency 200
```

`--use-aliases` gives the container its service name, `loadgen`, which the nodes reach through
the egress gateway. `--no-deps` leaves the running deployment as it is: `--profile load` replaces
`COMPOSE_PROFILES`, so Compose would bring the edge's dependencies up again without the nodes,
and recreate a holder whose image was rebuilt under its node. The numbers measured on the reference deployment are in
`archi/09-roadmap.md`.

The limits of `archi/03-execution.md` are tested through the API by
`cargo test -p rewind-server --test integration limits::` (CPU per call, heap, commands per
activation, history cap, payload sizes, TTL), the quota's 429 by `api::`, and the 256 KiB bound of
the agent tools' `run_workflow` against a live deployment by the SDK:

```sh
. ./.env
KEY=$(curl -s http://localhost:8080/v1/api-keys -H "Authorization: Bearer $REWIND_ADMIN_KEY" \
  -H 'content-type: application/json' -d '{"organizationId": "sdk", "role": "harness", "name": "sdk live test"}' | jq -r .key)
(cd ../sdk/typescript && REWIND_LIVE_URL=http://localhost:8080 REWIND_LIVE_KEY=$KEY pnpm vitest run test/live.test.ts)
```

## Stopping

`docker compose down` stops everything and keeps the data; `docker compose down -v` deletes the
volumes too.
