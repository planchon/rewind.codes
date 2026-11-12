# Reference deployment

A production-shaped deployment of ai-ptc on one Docker host, with Docker Compose: the shape
`archi/06-storage-and-scale.md` describes, small enough to read in one sitting. Copy it and
change what your platform does differently (managed Postgres and S3, your own collector, a
Kubernetes egress policy instead of the gateway container).

| Service | Image | Networks | Role |
| --- | --- | --- | --- |
| `edge` | `caddy:2-alpine` | public, core | The only published port. Sends `POST /v1/test` to the workers, everything else to the api |
| `api` | `ai-ptc` | public, core | `ptc server --role api`: the HTTP API, keys, the dashboard. No V8 |
| `worker-1`, `worker-2` | `ai-ptc` | a holder's namespace | `ptc server --role worker`: activations (V8), tool dispatch, scheduler, projector, retention |
| `worker-N-net` | `ai-ptc-egress` | core | Holds a worker's network namespace, with its default route through `egress`; healthy once the route is set |
| `egress` | `ai-ptc-egress` | core, public | The workers' only way out: forwards to the addresses of allowed names only |
| `worker-dns` | `ai-ptc-egress` | core | Profile `gvisor`: the DNS server of the workers under gVisor, a relay to Docker's ([gVisor](#workers-the-walls)) |
| `walls`, `walls-net` | `ai-ptc-egress` | a holder's namespace | Profile `walls`: the gVisor sandbox `check-walls.sh` checks the gVisor workers' walls from, and its holder; started for the check only |
| `postgres` | `postgres:17` | core | Journal, read model, secrets (`ptc-store-postgres`) |
| `minio`, `minio-init` | Chainguard's MinIO | core | Large payloads and archived journals (S3), and the bucket's creation |
| `otel-collector` | `otel/opentelemetry-collector` | core | Traces, metrics and logs of the api and the workers, over OTLP |
| `worker-previous` | `ai-ptc`, a previous build | a holder's namespace | Profile `previous-epoch` (`previous-epoch-gvisor` under gVisor): the pool of a previous determinism epoch ([upgrades](#upgrades)) |
| `loadgen` | `ai-ptc-load` | public | Profile `load`: the load generator and its fake MCP server ([load tests](#load-and-limit-tests)) |

`core` is an internal network: nothing on it has a route out. `public` is an ordinary bridge.

## Quick start

```sh
cd deploy
./init.sh                         # .env with fresh secrets, once (./init.sh <file> writes another file)
docker compose up -d --build      # builds the image from the repository (a release build: minutes)
./check-walls.sh                  # the workers' network walls hold (worker-1's; ./check-walls.sh worker-2-net)
```

The API and the dashboard are then at <http://localhost:8080> (`PTC_PORT`, `PTC_BIND`). The
image is the repository's `Dockerfile`: `docker build -t ai-ptc:local .` builds it alone, and
`PTC_IMAGE` names another one (a registry tag) for `docker compose up -d --no-build`.

## The image

One image for every role, chosen by the arguments: `ptc server --role api` or
`--role worker` (both roles without `--role`; `ptc server --help` lists every option). It holds
the `ptc` binary only, with the dashboard and tsgo embedded, on a distroless base (glibc, CA
certificates, no shell), and runs as the user `nonroot` (65532). It sets `TZ=UTC`, which the
sandbox requires, and `PTC_LISTEN=0.0.0.0:8080`.

Every service of the image runs with a read-only root file system, no capability and
`no-new-privileges`. The api type-checks scripts with tsgo, which it extracts at its first check
into `/var/cache/ptc` (`PTC_TSGO_CACHE`): give it a writable directory that allows execution,
as the tmpfs of `compose.yaml` does.

## Configuration

Every option of `ptc server` also reads an environment variable (`PTC_*`). `compose.yaml` sets
the ones the deployment needs; `.env` (written by `init.sh`) or your shell sets these:

| Variable | Default | What |
| --- | --- | --- |
| `PTC_IMAGE` | `ai-ptc:local` | The image of the api and the workers |
| `PTC_BIND`, `PTC_PORT` | `127.0.0.1`, `8080` | Where the edge listens on the host |
| `PTC_PUBLIC_URL` | `http://localhost:8080` | The URL clients reach the edge at: `webhook()` URLs start with it |
| `PTC_EGRESS_ALLOW` | `loadgen` | The names the workers may reach: MCP servers, hosts of `http` tools, webhook receivers (spaces or commas). A name allows its subdomains |
| `PTC_ALLOW_INTERNAL_MCP` | `loadgen` | MCP servers at private addresses the api and the workers may use (names, `*.domain`, addresses, CIDR networks; commas). Without it a deployed server reaches MCP servers at public addresses only |
| `PTC_RUN_QUOTA` | `10000` | Active runs of a tenant created by this deployment; one more is refused with 429 |
| `PTC_DEFAULT_TTL`, `PTC_MAX_TTL` | `30d`, `365d` | A run's TTL when the harness gives none, and the most it may ask for |
| `PTC_JOURNAL_RETENTION`, `PTC_ARCHIVE_RETENTION`, `PTC_READ_MODEL_RETENTION`, `PTC_PAYLOAD_GRACE` | `30d`, `365d`, `90d`, `24h` | The retention of the tenants this deployment creates ([retention](#retention)) |
| `PTC_WORKER_ACTIVATIONS`, `PTC_WORKER_DISPATCHES` | `4`, `64` | Activations run at once, and tool calls in flight, per worker |
| `PTC_WORKER_MEMORY`, `PTC_WORKER_CPUS` | `1g`, `2` | A worker container's limits |
| `PTC_CACHE_INSTANCES`, `PTC_CACHE_MEMORY` | `500`, `384MiB` | The sticky instance cache of a worker: keep its memory well below the container's limit, under each isolate's 128 MiB heap limit |
| `PTC_LOG` | `info` | What the servers log on standard error, as `RUST_LOG` writes it |
| `COMPOSE_PROFILES` | `runc` | `runc` or `gvisor`, plus `previous-epoch` (under gVisor `previous-epoch-gvisor`) during an upgrade |
| `PTC_CORE_SUBNET`, `PTC_CORE_RANGE`, `PTC_EGRESS_ADDRESS`, `PTC_WORKER_DNS_ADDRESS` | `172.29.213.0/24`, `172.29.213.128/25`, `172.29.213.2`, `172.29.213.3` | The core network, the part of it Docker hands out, and the fixed addresses below that part of the gateway and of `worker-dns`. `egress/resolv.conf` names `PTC_WORKER_DNS_ADDRESS`: change it there too |

`ptc server` has more options (`--allow-stdio`, `--otlp-protocol`, `--tenant` pools…): see
`docker run --rm ai-ptc:local server --help`.

## Secrets

`init.sh` writes `.env` with mode 0600; it is ignored by git. Keep it secret.

| Secret | What it protects | If lost |
| --- | --- | --- |
| `PTC_MASTER_KEY` | 32 random bytes in base64. It encrypts each tenant's data key, which encrypts the credentials of toolsets and runs | Every stored credential is unreadable: back it up with the database. The servers refuse to start without it |
| `PTC_ADMIN_KEY` | The first API key: role `admin` of the tenant `PTC_ADMIN_TENANT`, registered when the api starts. It creates the other keys | Revoke it (`DELETE /v1/api-keys/{id}`, its id is `GET /v1/whoami`'s `keyId`) and set a new one: a revoked key is never registered again |
| `POSTGRES_PASSWORD`, `MINIO_ROOT_*` | The stores | Change them in the store first, then in `.env` |

The servers read them from the environment. On a platform with a secret manager, inject them as
environment variables of the containers instead of a file. Never give `PTC_ADMIN_KEY` to the
workers: they do not need it, and they run code written by a model.

## Tenants and API keys

A tenant is created with its first key, with the deployment's quota and retention. Every key is
`ptc_sk_…`, shown once, stored hashed.

```sh
. ./.env
curl -s http://localhost:8080/v1/api-keys -H "Authorization: Bearer $PTC_ADMIN_KEY" \
  -H 'content-type: application/json' \
  -d '{"tenant": "acme", "role": "harness", "name": "acme harness"}'
```

| Role | May |
| --- | --- |
| `admin` | Everything, and keys of every tenant: the operator |
| `harness` | Everything of its tenant but keys: toolsets, runs, tasks, subscriptions, reading. The harness and its agent tools |
| `worker` | The host-tool task endpoints: the harness's SDK worker |
| `reader` | The `GET` endpoints of its tenant: a dashboard |

A key may carry `allowedLabels` (`{"conversation": "c-42"}`): it then sees and creates only the
runs carrying them, which is how one conversation's agent tools are scoped. Revoke a key with
`DELETE /v1/api-keys/{id}`.

A tenant's quota and retention are rows of the `tenants` table: change an existing tenant there.

```sh
docker compose exec postgres psql -U ptc -d ptc -c \
  "UPDATE tenants SET active_run_quota = 50000, journal_retention_ms = 7 * 86400000 WHERE id = 'acme'"
```

**Worker pools per tenant.** `ptc server --role worker --tenant acme` serves only `acme`;
`--except-tenant acme` serves every other one. Give an untrusted tenant its own pool, on its own
hosts.

## Workers: the walls

Workers run code written by a model. Three walls stand between two tenants: V8, gVisor, and
separate pools (`archi/06-storage-and-scale.md`, "Sandboxed workers").

- **Network.** A worker joins the network namespace of its holder (`worker-N-net`), whose only
  routes are the core network and a default route through `egress`. Its DNS server answers
  the names of the core network and asks `egress` for every other one. `egress` answers only
  the names of `PTC_EGRESS_ALLOW`, adds each address it answers to the set of allowed
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
and the receivers of your webhook subscriptions to `PTC_EGRESS_ALLOW`, then
`docker compose up -d egress`. An MCP server at a private address must also be in
`PTC_ALLOW_INTERNAL_MCP`: the servers check where MCP servers are, as the `http` tool does. S3, Postgres and the collector are on the core network here; a
managed S3 or collector outside it must be allowed too. The api is not behind the gateway: it
lists MCP servers' tools when a toolset is registered, and runs no model-written code.

## Upgrades

A build supports a set of determinism epochs (`GET /v1/server`'s `epochs`), and new runs start
under its latest. Most releases keep the epochs: replace the image and
`docker compose up -d`. A stopping server lets the work in flight commit (30 s at most), and
leases give the other runs to the new workers.

After a release that changes V8 or ICU, the new build no longer runs the previous epoch, whose
runs need a pool of the previous build until they end (`archi/03-execution.md`, "Determinism
epoch"). Workers are only offered runs of the epochs they support, so the two pools share the
database without meeting:

1. Keep the current image under its own tag: `docker tag ai-ptc:local ai-ptc:previous` (or pin
   its registry digest), and set `PTC_PREVIOUS_IMAGE=ai-ptc:previous` and
   `COMPOSE_PROFILES=runc,previous-epoch` in `.env` (`gvisor,previous-epoch-gvisor` under
   gVisor).
2. Build or pull the new image as `PTC_IMAGE`, then `docker compose up -d`: the api and the
   workers run the new build, and `worker-previous` the previous one.
3. Watch the previous epoch drain (here epoch 1):

   ```sh
   docker compose exec postgres psql -U ptc -d ptc -c \
     "SELECT count(*) FROM runs WHERE epoch = 1 AND status IN ('pending', 'active')"
   ```

   Every run has a deadline, so this reaches 0 at most `PTC_MAX_TTL` after the upgrade; a run
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
| A finished run's journal | 30 days in Postgres, then archived to the bucket as a journal document | `PTC_JOURNAL_RETENTION` |
| An archived journal | 1 year after its archiving, then the run is deleted | `PTC_ARCHIVE_RETENTION` |
| The read model (lists, dashboard, tool statistics) | 90 days after the run ended | `PTC_READ_MODEL_RETENTION` |
| A payload no journal references | 24 hours | `PTC_PAYLOAD_GRACE` |

The settings apply to the tenants created after they change; update the `tenants` row of an
existing one ([above](#tenants-and-api-keys)). `DELETE /v1/runs/{id}` erases one run at any
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
port, `PTC_ACCEPTANCE_PORT`, 18580 by default; its own network and secrets), the walls, two
loads with the thresholds recorded in `archi/09-roadmap.md` ("Load tests"), and the agent tools'
bound against it. It removes its containers, volumes and images at the end
(`PTC_ACCEPTANCE_KEEP=1` keeps them), and exits with 0 only when every step passed.

`ptc-load` drives a deployment through its API: it registers a toolset with its own fake MCP
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
`cargo test -p ptc-server --test limits` (CPU per call, heap, commands per activation, history
cap, payload sizes, TTL), the quota's 429 by `--test api`, and the 256 KiB bound of the agent
tools' `run_script` against a live deployment by the SDK:

```sh
. ./.env
KEY=$(curl -s http://localhost:8080/v1/api-keys -H "Authorization: Bearer $PTC_ADMIN_KEY" \
  -H 'content-type: application/json' -d '{"tenant": "sdk", "role": "harness"}' | jq -r .key)
(cd ../sdk/typescript && PTC_LIVE_URL=http://localhost:8080 PTC_LIVE_KEY=$KEY pnpm vitest run test/live.test.ts)
```

## Stopping

`docker compose down` stops everything and keeps the data; `docker compose down -v` deletes the
volumes too.
