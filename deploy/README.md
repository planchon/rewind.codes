# Reference deployment

A production-shaped deployment of Rewind on one Docker host, with Docker Compose: the shape
`archi/06-storage-and-scale.md` describes, small enough to read in one sitting. Copy it and
change what your platform does differently (managed Postgres and S3, your own collector, a
Kubernetes egress policy instead of the gateway container).

[`railway/`](railway/README.md) is such a copy: the hosted deployment of Rewind, on Railway and
Vercel.

| Service | Image | Networks | Role |
| --- | --- | --- | --- |
| `edge` | `caddy:2-alpine` | public, core | The only published port. Sends what runs tests (`POST /v1/test`, `POST /v1/workflows`, `POST /v1/runs/{id}/fork`) to the workers, everything else to the api |
| `api` | `rewind` | public, core | `rewind server --role api`: the HTTP API, keys, the dashboard. No V8 |
| `worker-1`, `worker-2` | `rewind` | a holder's namespace | `rewind server --role worker`: activations (V8), tool dispatch, scheduler, projector, retention |
| `worker-N-net` | `rewind-egress` | core | Holds a worker's network namespace, with its default route through `egress`; healthy once the route is set |
| `egress` | `rewind-egress` | core, public | The workers' only way out: forwards to the addresses of allowed names only |
| `worker-dns` | `rewind-egress` | core | Profile `gvisor`: the DNS server of the workers under gVisor, a relay to Docker's ([gVisor](#workers-the-walls)) |
| `walls`, `walls-net` | `rewind-egress` | a holder's namespace | Profile `walls`: the gVisor sandbox `check-walls.sh` checks the gVisor workers' walls from, and its holder; started for the check only |
| `postgres` | `postgres:17` | core | Journal, read model, secrets (`rewind-store-postgres`) |
| `minio`, `minio-init` | Chainguard's MinIO | core | Large payloads and archived journals (S3), and the bucket's creation |
| `otel-collector` | `otel/opentelemetry-collector` | core | Traces, metrics and logs of the api and the workers, over OTLP |
| `worker-previous` | `rewind`, a previous build | a holder's namespace | Profile `previous-epoch` (`previous-epoch-gvisor` under gVisor): the pool of a previous determinism epoch ([upgrades](#upgrades)) |
| `loadgen` | `rewind-load` | public | Profile `load`: the load generator and its fake MCP server ([load tests](#load-and-limit-tests)) |

`core` is an internal network: nothing on it has a route out. `public` is an ordinary bridge.

## Quick start

```sh
cd deploy
./init.sh                         # .env with fresh secrets, once (./init.sh <file> writes another file)
docker compose up -d --build      # builds the image from the repository (a release build: minutes)
./check-walls.sh                  # the workers' network walls hold (worker-1's; ./check-walls.sh worker-2-net)
```

The API and the dashboard are then at <http://localhost:8080> (`REWIND_PORT`, `REWIND_BIND`). The
image is the repository's `Dockerfile`: `docker build -t rewind:local .` builds it alone, and
`REWIND_IMAGE` names another one (a registry tag) for `docker compose up -d --no-build`.

## The image

One image for every role, chosen by the arguments: `rewind server --role api` or
`--role worker` (both roles without `--role`; `rewind server --help` lists every option). It holds
the `rewind` binary only, with the dashboard and tsgo embedded, on a distroless base (glibc, CA
certificates, no shell), and runs as the user `nonroot` (65532). It sets `TZ=UTC`, which the
sandbox requires, and `REWIND_LISTEN=0.0.0.0:8080`.

Every service of the image runs with a read-only root file system, no capability and
`no-new-privileges`. The api type-checks scripts with tsgo, which it extracts at its first check
into `/var/cache/rewind` (`REWIND_TSGO_CACHE`): give it a writable directory that allows execution,
as the tmpfs of `compose.yaml` does.

## Configuration

Every option of `rewind server` also reads an environment variable (`REWIND_*`). `compose.yaml` sets
the ones the deployment needs; `.env` (written by `init.sh`) or your shell sets these:

| Variable | Default | What |
| --- | --- | --- |
| `REWIND_IMAGE` | `rewind:local` | The image of the api and the workers |
| `REWIND_BIND`, `REWIND_PORT` | `127.0.0.1`, `8080` | Where the edge listens on the host |
| `REWIND_PUBLIC_URL` | `http://localhost:8080` | The URL clients reach the edge at: `webhook()` URLs start with it |
| `REWIND_EGRESS_ALLOW` | `loadgen` | The names the workers may reach: MCP servers, hosts of `http` tools, webhook receivers (spaces or commas). A name allows its subdomains |
| `REWIND_ALLOW_INTERNAL_TOOLS` | `loadgen` | MCP servers and APIs at private addresses the api and the workers may use (names, `*.domain`, addresses, CIDR networks; commas). Without it a deployed server reaches MCP servers, APIs and OpenAPI documents at public addresses only |
| `REWIND_QUOTA_ACTIVE_RUNS` | `10000` | The ceiling of the tenants this deployment creates: their active runs across their namespaces; one more is refused with 429. `REWIND_QUOTA_RUNS_PER_MONTH`, `REWIND_QUOTA_CPU_PER_MONTH`, `REWIND_QUOTA_SCRIPTS`, `REWIND_QUOTA_MAX_TTL` and `REWIND_QUOTA_MAX_SCRIPT_LINES` set the others, none by default |
| `REWIND_DEFAULT_TTL`, `REWIND_MAX_TTL` | `30d`, `365d` | A run's TTL when the harness gives none, and the most it may ask for |
| `REWIND_JOURNAL_RETENTION`, `REWIND_ARCHIVE_RETENTION`, `REWIND_READ_MODEL_RETENTION`, `REWIND_PAYLOAD_GRACE` | `30d`, `365d`, `90d`, `24h` | The retention of the tenants this deployment creates ([retention](#retention)) |
| `REWIND_WORKER_ACTIVATIONS`, `REWIND_WORKER_DISPATCHES` | `4`, `64` | Activations run at once, and tool calls in flight, per worker |
| `REWIND_WORKER_MEMORY`, `REWIND_WORKER_CPUS` | `1g`, `2` | A worker container's limits |
| `REWIND_CACHE_INSTANCES`, `REWIND_CACHE_MEMORY` | `500`, `384MiB` | The sticky instance cache of a worker: keep its memory well below the container's limit, under each isolate's 128 MiB heap limit |
| `REWIND_LOG` | `info` | What the servers log on standard error, as `RUST_LOG` writes it |
| `COMPOSE_PROFILES` | `runc` | `runc` or `gvisor`, plus `previous-epoch` (under gVisor `previous-epoch-gvisor`) during an upgrade |
| `REWIND_CORE_SUBNET`, `REWIND_CORE_RANGE`, `REWIND_EGRESS_ADDRESS`, `REWIND_WORKER_DNS_ADDRESS` | `172.29.213.0/24`, `172.29.213.128/25`, `172.29.213.2`, `172.29.213.3` | The core network, the part of it Docker hands out, and the fixed addresses below that part of the gateway and of `worker-dns`. `egress/resolv.conf` names `REWIND_WORKER_DNS_ADDRESS`: change it there too |

`rewind server` has more options (`--allow-stdio`, `--otlp-protocol`, `--tenant` pools…): see
`docker run --rm rewind:local server --help`.

## Secrets

`init.sh` writes `.env` with mode 0600; it is ignored by git. Keep it secret.

| Secret | What it protects | If lost |
| --- | --- | --- |
| `REWIND_MASTER_KEY` | 32 random bytes in base64. It encrypts each tenant's data key, which encrypts the credentials of toolsets and runs | Every stored credential is unreadable: back it up with the database. The servers refuse to start without it |
| `REWIND_ADMIN_KEY` | The first API key: role `operator` in the tenant `REWIND_ADMIN_TENANT`, registered when the api starts. It creates the other keys | Revoke it (`DELETE /v1/api-keys/{id}`, its id is `GET /v1/whoami`'s `principal.id`) and set a new one: a revoked key is never registered again |
| `WORKOS_API_KEY`, `WORKOS_COOKIE_PASSWORD` | WorkOS's secret key, and the key of the sign-in's sealed cookies ([sign-in](#sign-in)) | A new cookie password signs everyone out; a new WorkOS key is set in WorkOS first |
| `POSTGRES_PASSWORD`, `MINIO_ROOT_*` | The stores | Change them in the store first, then in `.env` |

The servers read them from the environment. On a platform with a secret manager, inject them as
environment variables of the containers instead of a file. Never give `REWIND_ADMIN_KEY` nor
`WORKOS_API_KEY` to the workers: they do not need them, and they run code written by a model.

## Tenants, keys and roles

A tenant is created with its first key, or with its organization when people sign in
([sign-in](#sign-in)), with the deployment's quota and retention. Every key is `rewind_sk_…`,
shown once, stored hashed.

```sh
. ./.env
curl -s http://localhost:8080/v1/api-keys -H "Authorization: Bearer $REWIND_ADMIN_KEY" \
  -H 'content-type: application/json' \
  -d '{"tenant": "acme", "role": "harness", "name": "acme harness"}'
```

A key gets what it may do from its grant, given with it: a role, and optionally allowed labels.
Whoever creates a key holds every permission of its role. Four roles are built in; a tenant
composes its own from the catalog of permissions (`GET /v1/permissions`, `PUT /v1/roles/{name}`).

| Role | May |
| --- | --- |
| `owner` | Every permission of its tenant: its runs, and its members, roles, keys and organization |
| `harness` | Runs, toolsets, credentials, tasks and subscriptions, and reading the members and roles. The harness and its agent tools |
| `worker` | The host-tool task endpoints: the harness's SDK worker |
| `reader` | Reading runs, toolsets, subscriptions, members and roles: a dashboard |

**The operator.** The key of `REWIND_ADMIN_KEY` has the role `operator`: every permission in its
tenant, and the keys of every tenant (`POST /v1/api-keys` with a `tenant`, as above, and their
revocation). `operator` is the deployment's role, never an organization's; only the operator
creates an operator key.

A key may carry `allowedLabels` (`{"conversation": "c-42"}`): it then sees and creates only the
runs carrying them, which is how one conversation's agent tools are scoped; such a key
administers nothing. Revoke a key with `DELETE /v1/api-keys/{id}`: its grant goes with it.

## Sign-in

With WorkOS, people sign in to the dashboard (GitHub, or a code by email), each in their
organizations: a WorkOS organization is a tenant, with the same id. WorkOS says who someone is;
what they may do is their grant in Rewind, as for a key. Sign-ups go through WorkOS's waitlist:
the operator approves people there, which invites them.

Give the api role the four variables together, or none (`rewind server` refuses to start with
some of them; without them the server accepts only API keys). Add them to the `api` service's
`environment` in `compose.yaml`, never to a worker's:

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
`localhost`. The api passes WorkOS the address of each browser that signs in: the last of
`X-Forwarded-For`, which the edge writes with the address that reached it (Caddy ignores the one
a client sends; the edge is the only published port), when it is a public address.

A tenant's ceiling is the operator's offer: the admin key changes it with `PUT
/v1/organizations/{tenant}/quotas`. Its retention is a row of the `tenants` table: change an
existing tenant there.

```sh
curl -X PUT "$REWIND_PUBLIC_URL/v1/organizations/acme/quotas" \
  -H "Authorization: Bearer $REWIND_ADMIN_KEY" -H 'Content-Type: application/json' \
  -d '{"ceiling": {"activeRuns": 50000, "runsPerMonth": 2000000}}'
docker compose exec postgres psql -U rewind -d rewind -c \
  "UPDATE tenants SET journal_retention_ms = 7 * 86400000 WHERE id = 'acme'"
```

**Worker pools per tenant.** `rewind server --role worker --tenant acme` serves only `acme`;
`--except-tenant acme` serves every other one. Give an untrusted tenant its own pool, on its own
hosts.

## Workers: the walls

Workers run code written by a model. Three walls stand between two tenants: V8, gVisor, and
separate pools (`archi/06-storage-and-scale.md`, "Sandboxed workers").

- **Network.** A worker joins the network namespace of its holder (`worker-N-net`), whose only
  routes are the core network and a default route through `egress`. Its DNS server answers
  the names of the core network and asks `egress` for every other one. `egress` answers only
  the names of `REWIND_EGRESS_ALLOW`, adds each address it answers to the set of allowed
  addresses, and forwards new TCP connections from the core network to that set only, never to
  `169.254.0.0/16` (the cloud metadata endpoint). The workers need no proxy setting: the
  gateway is a rule of their network. The `http` tool still checks its own allow-list and
  refuses private addresses (`archi/04-tools.md`). `./check-walls.sh` checks it all from inside
  a worker's namespace (under gVisor, from a sandbox like the workers'). The gateway needs a
  host that does not pass bridged traffic through iptables: with `br_netfilter` and
  `net.bridge.bridge-nf-call-iptables=1`, Docker's rules for the internal network `core` drop
  the workers' packets to the gateway, and allowed names are unreachable too.
- **Process.** Read-only root, no capability, `no-new-privileges`, a memory and CPU limit, no
  admin key. The holder alone has `NET_ADMIN`, to set its route at start.
- **gVisor.** `docker compose --profile gvisor up -d` runs the same workers under `runsc`, on a
  host whose Docker has the runtime ([install](https://gvisor.dev/docs/user_guide/install/)),
  behind the same walls. A gVisor sandbox runs its own network stack, so three things differ:
  - The sandbox copies the addresses and routes of its holder's namespace when it starts, and
    removes them from the namespace. So a gVisor worker starts once its holder is healthy,
    with its route set, and the holder is left with an empty namespace.
  - The sandbox cannot reach Docker's DNS server, at `127.0.0.11`: that address is the
    sandbox's own loopback ([gVisor's FAQ](https://gvisor.dev/docs/user_guide/faq/#docker-bridge)).
    The workers' DNS server is `worker-dns` instead (`egress/resolv.conf`). It stands where a
    holder stands, on the core network only with `egress` as its DNS server, and relays every
    question to Docker's DNS server. So it answers what Docker's answers a worker under runc:
    the core network's names, and every other name through `egress`.
  - Nothing else can run in a worker's namespace. `./check-walls.sh` checks the walls from
    `walls`, a gVisor sandbox with the workers' network settings in a holder of its own. It
    starts both for the check and removes them after.

  The profile is not tested on Docker Desktop, which has no `runsc`; CI runs it on Linux.

**The workers' allow-list.** Add your MCP servers, the hosts your toolsets' `http` tools allow,
and the receivers of your webhook subscriptions to `REWIND_EGRESS_ALLOW`, then
`docker compose up -d egress`. An MCP server or an API at a private address must also be in
`REWIND_ALLOW_INTERNAL_TOOLS`: the servers check where MCP servers and APIs are, as the `http`
tool does. S3, Postgres and the collector are on the core network here; a managed S3 or
collector outside it must be allowed too. The api is not behind the gateway: it lists MCP
servers' tools and reads OpenAPI documents when a toolset is registered, and runs no
model-written code.

## Upgrades

A build supports a set of determinism epochs (`GET /v1/server`'s `epochs`), and new runs start
under its latest. Most releases keep the epochs: replace the image and
`docker compose up -d`. A stopping server lets the work in flight commit (30 s at most), and
leases give the other runs to the new workers.

After a release that changes V8 or ICU, the new build no longer runs the previous epoch, whose
runs need a pool of the previous build until they end (`archi/03-execution.md`, "Determinism
epoch"). Workers are only offered runs of the epochs they support, so the two pools share the
database without meeting:

1. Keep the current image under its own tag: `docker tag rewind:local rewind:previous` (or pin
   its registry digest), and set `REWIND_PREVIOUS_IMAGE=rewind:previous` and
   `COMPOSE_PROFILES=runc,previous-epoch` in `.env` (`gvisor,previous-epoch-gvisor` under
   gVisor).
2. Build or pull the new image as `REWIND_IMAGE`, then `docker compose up -d`: the api and the
   workers run the new build, and `worker-previous` the previous one.
3. Watch the previous epoch drain (here epoch 1):

   ```sh
   docker compose exec postgres psql -U rewind -d rewind -c \
     "SELECT count(*) FROM runs WHERE epoch = 1 AND status IN ('pending', 'active')"
   ```

   Every run has a deadline, so this reaches 0 at most `REWIND_MAX_TTL` after the upgrade; a run
   still quarantined at its deadline is aborted.
4. Remove `previous-epoch` (or `previous-epoch-gvisor`) from `COMPOSE_PROFILES` and
   `docker compose up -d --remove-orphans`.

The previous pool shares the database: a build refuses a schema newer than its own, so a
release that migrates the schema cannot keep a pool of a build older than the migration. Such a
release and a change of V8 or ICU go in separate releases.

## Retention

Active runs are never touched. The workers' retention jobs, every 5 minutes, apply each tenant's
settings (`archi/06-storage-and-scale.md`, "Retention and deletion"):

| Data | Kept | Setting |
| --- | --- | --- |
| A finished run's journal | 30 days in Postgres, then archived to the bucket as a journal document | `REWIND_JOURNAL_RETENTION` |
| An archived journal | 1 year after its archiving, then the run is deleted | `REWIND_ARCHIVE_RETENTION` |
| The read model (lists, dashboard, tool statistics) | 90 days after the run ended | `REWIND_READ_MODEL_RETENTION` |
| A payload no journal references | 24 hours | `REWIND_PAYLOAD_GRACE` |

The settings apply to the tenants created after they change; update the `tenants` row of an
existing one ([above](#tenants-keys-and-roles)). `DELETE /v1/runs/{id}` erases one run at any
time, archive and payloads included. Back up Postgres and the bucket together, with the master
key.

## Observability

The api and the workers export service telemetry (activation, replay, tool attempt spans and
their metrics) and run telemetry (one trace per run, projected from the journal) over OTLP to
`otel-collector`, whose `debug` exporter prints them: replace it in `otel-collector.yaml` by your
backend's. A run's trace joins the harness's when the harness passes `traceparent`.

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

`rewind-load` drives a deployment through its API: it registers a toolset with its own fake MCP
server (`fake.echo`, and `fake.flaky`, which fails transiently once per call) and a host tool it
answers itself (`host.square`), starts many runs that each call the three tools, sleep, and read
a signal sent twice with one idempotency key, waits for every run, then checks that each ended
as expected and that no tool call was sent more times than the run says it was attempted. It
reports throughput, latencies (start, first activation, activation, end to end), errors, and
how long the read model took to hold every run after the last ended. It reads the runs' attempts
once the read model holds them, and exits with 1 when an invariant broke or a threshold given
(`--min-runs-per-s`, `--max-p99-end-to-end`, `--max-p99-activation`, `--max-read-model-lag`) was
missed. `--payload-bytes` pads each run's payloads: above 1 KiB they are stored apart from their
event, above 64 KiB in S3.

```sh
docker compose --profile load build loadgen
docker compose --profile load run --rm --use-aliases loadgen run --runs 2000 --concurrency 200
```

`--use-aliases` gives the container its service name, `loadgen`, which the workers reach through
the egress gateway. The numbers measured on the reference deployment are in
`archi/09-roadmap.md`.

The limits of `archi/03-execution.md` are tested through the API by
`cargo test -p rewind-server --test limits` (CPU per call, heap, commands per activation, history
cap, payload sizes, TTL), the quota's 429 by `--test api`, and the 256 KiB bound of the agent
tools' `run_script` against a live deployment by the SDK:

```sh
. ./.env
KEY=$(curl -s http://localhost:8080/v1/api-keys -H "Authorization: Bearer $REWIND_ADMIN_KEY" \
  -H 'content-type: application/json' -d '{"tenant": "sdk", "role": "harness", "name": "sdk live test"}' | jq -r .key)
(cd ../sdk/typescript && REWIND_LIVE_URL=http://localhost:8080 REWIND_LIVE_KEY=$KEY pnpm vitest run test/live.test.ts)
```

## Stopping

`docker compose down` stops everything and keeps the data; `docker compose down -v` deletes the
volumes too.
