# Rewind on Railway and Vercel

The hosted deployment of Rewind: the API and its node on Railway, the sites on Vercel, all
under `rewind.codes`. It is the reference deployment of
[`deploy/`](../README.md) adapted to a platform with managed Postgres and S3, no network policy
and no choice of container runtime. Its users are the people the operator approves through
WorkOS's waitlist: they sign in to the dashboard ([sign-in](#sign-in)).

| Name | Where | What |
| --- | --- | --- |
| `rewind.codes` | Vercel, project `rewind-landing` ([`landing/`](../../landing)) | The landing, static. `www.rewind.codes` redirects to it. `/auth/waitlist` is rewritten to `api.rewind.codes`, for its waitlist form |
| `app.rewind.codes` | Vercel, project `rewind-dashboard` ([`dashboard/`](../../dashboard)) | The dashboard. `/v1/*` and `/auth/*` are rewritten to `api.rewind.codes`, so the app calls the API and signs in on its own origin: the sign-in's cookies are the app's |
| `api.rewind.codes` | Railway, service `edge` | Caddy ([`edge/Caddyfile`](edge/Caddyfile)): every request to the node |
| (private) | Railway, service `node` | `rewind server`: the HTTP API, keys and the dashboard's sign-in, and the node of the run plane (the owners of the partitions, the organization actors, the executors with V8, tool calls, timers, retention) |
| (private) | Railway, `Postgres` | The control plane: tenants, namespaces, keys, grants, scripts, toolsets, workflows, secrets, subscriptions, the nodes and the partition map; the nodes' liveness locks in its database `postgres`. Railway's Postgres template, with a 5 GB volume |
| (private) | AWS S3, bucket `rewind.codes-paris-prod-23112026-13b80` | The bucket: the partitions, the archive of ended runs, the payloads above 64 KiB and the test documents |

The Railway project is `rewind`, environment `production`, region `europe-west4` (Amsterdam);
the bucket is on AWS S3 in `eu-west-3` (Paris), the closest S3 region. DNS for `rewind.codes` is on Vercel: `api` is a CNAME to the target
Railway gives the edge's custom domain, with its `_railway-verify.api` TXT record.

## Sleeping

| Service | Railway's serverless setting | Why |
| --- | --- | --- |
| `edge` | On | No background work: it sleeps after 5 to 10 minutes without a request, and starts again on the next one |
| `node` | Off | It holds the timers of its partitions (sleeps, retries, deadlines, retention) that must fire while no request comes, and its liveness lock in Postgres |
| `Postgres` | Off | The node holds a session on it for as long as it lives |

`REWIND_OTLP_ENDPOINT` is not set: logs are in Railway.

## The images

Railway builds [`Dockerfile`](Dockerfile) here for the node: the server stage of the repository's
`Dockerfile`, without its BuildKit cache mounts, whose ids Railway restricts to one service. Its
start command, in exec form since the image has no shell, is `/usr/local/bin/rewind server`.

The image listens on `[::]:8080` (`REWIND_LISTEN`) and its node link on `[::]:7070`
(`REWIND_LINK_LISTEN`): Railway's private network is IPv6, and the edge reaches
`node.railway.internal:8080` there. Every service sets `PORT=8080`. The health check is `GET /`:
the embedded dashboard, which answers without a key. The edge is
[`edge/Dockerfile`](edge/Dockerfile), Caddy with its Caddyfile.

The node service runs one replica: each node of a deployment is reached by the others at its own
address (`REWIND_LINK_ADDRESS`), and the replicas of a Railway service share one private name.

The node service carries a volume, `node-handover` (500 MB at `/var/lib/rewind`), that the node
never writes: with a volume attached, Railway stops the running deployment before it starts the
next one, as `docker compose up` does. Without it Railway starts the next deployment first and
stops the old one once the new one answers its health check; but a node holds its id's lock in
Postgres for as long as it lives, so a second `node` cannot join while the first runs, never
answers, and the deploy fails. A deploy leaves the API down about 10 s: the old node stops, the
new one starts and opens its partitions.

A deploy uploads the working tree from the repository's root, one service at a time:
`railway up --service node`, `--service edge` (`RAILWAY_DOCKERFILE_PATH` is set on each). A
release build of the server takes Railway about 10 minutes. The deploy that moves runs to
partitions runs `rewind server migrate-runs` once, with the node's variables, before the node
starts ([`deploy/`](../README.md#upgrades)): as the node's pre-deploy command for that one
deploy, removed after it. A redeploy reuses its deployment's start command; a changed start
command takes a new deploy (`railway up`).

## Variables

| Variable | Services | Value |
| --- | --- | --- |
| `REWIND_STORE` | node | `${{Postgres.DATABASE_URL}}`, on the private network |
| `REWIND_BUCKET` | node | `s3://rewind.codes-paris-prod-23112026-13b80` |
| `REWIND_S3_REGION` | node | `eu-west-3`; no endpoint, AWS S3's own |
| `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` | node | The key of the IAM user `rewind-railway-prod`, allowed only that bucket's objects and listing, over TLS ([08](../../archi/08-decisions.md#taken-for-the-object-storage-rewrite-november-2026)). Railway gives its services no identity token for a role, so the key lives only in these variables; rotate it in IAM and here together |
| `REWIND_MASTER_KEY` | node | 32 random bytes in base64. Back it up with the database: without it every stored credential is lost |
| `REWIND_CLUSTER_SECRET` | node | 32 random bytes in hexadecimal (`openssl rand -hex 32`): it authenticates the node link, and every node of the deployment shares it |
| `REWIND_NODE_ID` | node | `node`: the node's id, and the `service.instance.id` of its telemetry |
| `REWIND_LINK_ADDRESS` | node | `${{RAILWAY_PRIVATE_DOMAIN}}:7070`: where the other nodes reach its link |
| `REWIND_ADMIN_KEY`, `REWIND_ADMIN_TENANT` | node | The operator's key (`rewind_sk_…`), tenant `default` |
| `WORKOS_CLIENT_ID`, `WORKOS_API_KEY` | node | The WorkOS environment's client (`client_…`) and secret key (`sk_…`) |
| `WORKOS_COOKIE_PASSWORD` | node | 32 random bytes in hexadecimal (`openssl rand -hex 32`): the key of the sign-in's sealed cookies. Changing it signs everyone out |
| `WORKOS_REDIRECT_URI` | node | `https://app.rewind.codes/auth/callback`, registered in WorkOS |
| `REWIND_PUBLIC_URL` | node | `https://api.rewind.codes`: `webhook()` URLs start with it |
| `REWIND_QUOTA_ACTIVE_RUNS` | node | `10000`: the ceiling of active runs given to each organization created from then on. An organization keeps its own ceiling, which `PUT /v1/organizations/{tenant}/quotas` changes |
| `REWIND_EXECUTORS`, `REWIND_CALLS`, `REWIND_CACHE_INSTANCES`, `REWIND_CACHE_MEMORY` | node | `4`, `256`, `500`, `384MiB`, for a 1 GB limit |
| `REWIND_LOG` | node | `info` |

Read the admin key with `railway variable list --service node --kv | grep REWIND_ADMIN_KEY`. It
creates the keys of any tenant as in
[`deploy/README.md`](../README.md#tenants-keys-and-roles), against `https://api.rewind.codes`.

## Sign-in

People sign in on `app.rewind.codes`, with GitHub or a code by email, through WorkOS: the four
`WORKOS_*` are on the node (all four or none; without them it accepts only API keys). Sign-ups go
through WorkOS's waitlist: the operator approves people in WorkOS's dashboard, which sends them
an invitation; a member then creates an organization, or joins one by an invitation of its
members, and gets their rights from their grant in Rewind
([`archi/06-storage-and-scale.md`](../../archi/06-storage-and-scale.md#tenancy-namespaces-identity-and-access)).
The WorkOS environment is set as [`deploy/`](../README.md#sign-in) says, with
`https://app.rewind.codes/` as its *User invitation URL*.

Vercel rewrites `/auth/*` to the API like `/v1/*`, and the edge sends both to the node. The
browser's address does not reach the node: Vercel's rewrite and Railway's proxy stand between,
and the edge's `X-Forwarded-For` holds a proxy's private address, which the node does not pass
to WorkOS. WorkOS records the browser's user agent.

## The walls

Railway runs ordinary containers on a shared private network. The owner decided that it needs
neither gVisor nor the egress gateway (13 November 2026); the people the operator approves run
code behind the walls that hold
([08](../../archi/08-decisions.md#taken-for-accounts-organizations-and-access-november-2026)). Of the walls of
[`archi/06-storage-and-scale.md`](../../archi/06-storage-and-scale.md) ("Sandboxed nodes"),
V8 and the egress checks of the `http` tool and of MCP servers hold: private, unique-local
(Railway's `fd…` addresses), carrier-grade NAT and metadata addresses are refused, so a script
cannot reach `postgres.railway.internal`. The node that runs the scripts also holds the admin key
and WorkOS's key, as every node that serves the API does.

## Vercel

Each site is a Vercel project of the team `planchons-projects`, deployed from its directory:
`rewind-dashboard` from `dashboard/`, `rewind-landing` from `landing/`. The dashboard's
`vercel.json` builds it with pnpm and rewrites `/v1/*` and `/auth/*` to the API; the landing is
static files with a few headers.

CI deploys both to production on every push to `main`
([`.github/workflows/ci.yml`](../../.github/workflows/ci.yml), jobs `deploy-dashboard` and
`deploy-landing`): the dashboard once its job (lint, types, tests, the generated API types,
the build) passes, the landing at once. Each runs `vercel deploy --prod` in its directory, with
the project's id in `VERCEL_PROJECT_ID` and the repository's secret `VERCEL_TOKEN`, a token of
the team (vercel.com, Account settings, Tokens). The projects are not connected to the
repository on Vercel: a pull request deploys nothing. By hand, `vercel deploy --prod` in the
site's directory, linked to its project, still works.

The node is not deployed by CI ([The images](#the-images)), but from the developer's machine by
the `deploy` skill ([`.claude/skills/deploy`](../../.claude/skills/deploy/SKILL.md)): a commit
that changes the API and the dashboard together reaches `app.rewind.codes` before the API, until
`railway up` deploys it.
