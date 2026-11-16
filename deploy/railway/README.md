# Rewind on Railway and Vercel

The hosted deployment of Rewind: the API and the workers on Railway, the sites on Vercel, all
under `rewind.codes`. It is the reference deployment of
[`deploy/`](../README.md) adapted to a platform with managed Postgres and S3, no network policy
and no choice of container runtime. Its users are the people the operator approves through
WorkOS's waitlist: they sign in to the dashboard ([sign-in](#sign-in)).

| Name | Where | What |
| --- | --- | --- |
| `rewind.codes` | Vercel, project `rewind-landing` ([`landing/`](../../landing)) | The landing, static. `www.rewind.codes` redirects to it |
| `app.rewind.codes` | Vercel, project `rewind-dashboard` ([`dashboard/`](../../dashboard)) | The dashboard. `/v1/*` and `/auth/*` are rewritten to `api.rewind.codes`, so the app calls the API and signs in on its own origin: the sign-in's cookies are the app's |
| `api.rewind.codes` | Railway, service `edge` | Caddy ([`edge/Caddyfile`](edge/Caddyfile)): `POST /v1/test` to the workers, everything else to the api |
| (private) | Railway, service `api` | `rewind server --role api`: the HTTP API and keys. No V8 |
| (private) | Railway, service `worker` | `rewind server --role worker`: activations, tool dispatch, scheduler, projector, retention |
| (private) | Railway, `Postgres` | Journal, read model, secrets. Railway's Postgres template, with a 5 GB volume |
| (private) | Railway, bucket `payloads` | Payloads above 64 KiB and archived journals (S3) |

The Railway project is `rewind`, environment `production`, region `europe-west4` (Amsterdam);
the bucket is in `ams`. DNS for `rewind.codes` is on Vercel: `api` is a CNAME to the target
Railway gives the edge's custom domain, with its `_railway-verify.api` TXT record.

## Sleeping

| Service | Railway's serverless setting | Why |
| --- | --- | --- |
| `edge`, `api` | On | No background work: each sleeps after 5 to 10 minutes without a request, and starts again on the next one |
| `worker` | Off | Its loops poll the store every 250 ms; it also holds the timers, retries and deadlines that must fire while no request comes |
| `Postgres` | Off | The worker uses it continuously |

While the api starts again, its private address refuses connections: the edge retries them for up
to 30 seconds (`lb_try_duration`), so the first request after a quiet period is slow, not
failed. `REWIND_OTLP_ENDPOINT` is not set: the metric reader exports every 60 seconds, which would
keep the api awake. Logs are in Railway.

## The images

Railway builds [`Dockerfile`](Dockerfile) here for the api and the worker: the server stage of
the repository's `Dockerfile`, without its BuildKit cache mounts, whose ids Railway restricts to
one service. The start command picks the role, in exec form since the image has no shell:

| Service | Start command |
| --- | --- |
| `api` | `/usr/local/bin/rewind server --role api` |
| `worker` | `/usr/local/bin/rewind server --role worker` |

The image listens on `[::]:8080` (`REWIND_LISTEN`): Railway's private network is IPv6, and the edge
reaches `api.railway.internal:8080` and `worker.railway.internal:8080` there. Every service sets
`PORT=8080`. The health check is `GET /`: the embedded dashboard, which answers without a key.
The edge is [`edge/Dockerfile`](edge/Dockerfile), Caddy with its Caddyfile.

A deploy uploads the working tree from the repository's root, one service at a time:
`railway up --service api`, `--service worker`, `--service edge` (`RAILWAY_DOCKERFILE_PATH` is
set on each). A release build of the server takes Railway about 10 minutes.

## Variables

| Variable | Services | Value |
| --- | --- | --- |
| `REWIND_STORE` | api, worker | `${{Postgres.DATABASE_URL}}`, on the private network |
| `REWIND_BLOBS` | api, worker | `s3://${{payloads.BUCKET}}` |
| `REWIND_S3_ENDPOINT`, `REWIND_S3_REGION` | api, worker | `${{payloads.ENDPOINT}}`, `${{payloads.REGION}}` |
| `REWIND_S3_ACCESS_KEY_ID`, `REWIND_S3_SECRET_ACCESS_KEY` | api, worker | References to the bucket's credentials: a reset of the credentials follows |
| `REWIND_MASTER_KEY` | api, worker | 32 random bytes in base64. Back it up with the database: without it every stored credential is lost |
| `REWIND_ADMIN_KEY`, `REWIND_ADMIN_TENANT` | api | The operator's key (`rewind_sk_…`), tenant `default`. Never on the worker: it runs code written by a model |
| `WORKOS_CLIENT_ID`, `WORKOS_API_KEY` | api | The WorkOS environment's client (`client_…`) and secret key (`sk_…`). Never on the worker |
| `WORKOS_COOKIE_PASSWORD` | api | 32 random bytes in hexadecimal (`openssl rand -hex 32`): the key of the sign-in's sealed cookies. Changing it signs everyone out |
| `WORKOS_REDIRECT_URI` | api | `https://app.rewind.codes/auth/callback`, registered in WorkOS |
| `REWIND_PUBLIC_URL` | api, worker | `https://api.rewind.codes`: `webhook()` URLs start with it |
| `REWIND_QUOTA_ACTIVE_RUNS` | api | `10000`: the ceiling of active runs given to each organization created from then on (formerly `REWIND_RUN_QUOTA`). An organization keeps its own ceiling, which `PUT /v1/organizations/{tenant}/quotas` changes |
| `REWIND_WORKER_NAME` | api | `api`. The worker keeps the default, its host name and process id, unique per replica |
| `REWIND_ACTIVATIONS`, `REWIND_DISPATCHES`, `REWIND_CACHE_INSTANCES`, `REWIND_CACHE_MEMORY` | worker | `4`, `64`, `500`, `384MiB`, for a 1 GB limit |
| `REWIND_LOG` | api, worker | `info` |

Read the admin key with `railway variable list --service api --kv | grep REWIND_ADMIN_KEY`. It
creates the keys of any tenant as in
[`deploy/README.md`](../README.md#tenants-keys-and-roles), against `https://api.rewind.codes`.

## Sign-in

People sign in on `app.rewind.codes`, with GitHub or a code by email, through WorkOS: the four
`WORKOS_*` are on the api only (all four or none; a worker accepts only API keys). Sign-ups go
through WorkOS's waitlist: the operator approves people in WorkOS's dashboard, which sends them
an invitation; a member then creates an organization, or joins one by an invitation of its
members, and gets their rights from their grant in Rewind
([`archi/06-storage-and-scale.md`](../../archi/06-storage-and-scale.md#tenancy-namespaces-identity-and-access)).
The WorkOS environment is set as [`deploy/`](../README.md#sign-in) says, with
`https://app.rewind.codes/` as its *User invitation URL*.

Vercel rewrites `/auth/*` to the API like `/v1/*`, and the edge sends both to the api. The
browser's address does not reach the api: Vercel's rewrite and Railway's proxy stand between,
and the edge's `X-Forwarded-For` holds a proxy's private address, which the api does not pass
to WorkOS. WorkOS records the browser's user agent.

## The walls

Railway runs ordinary containers on a shared private network. The owner decided that it needs
neither gVisor nor the egress gateway (13 November 2026); the people the operator approves run
code behind the walls that hold
([08](../../archi/08-decisions.md#taken-for-accounts-organizations-and-access-november-2026)). Of the walls of
[`archi/06-storage-and-scale.md`](../../archi/06-storage-and-scale.md) ("Sandboxed workers"),
V8 and the egress checks of the `http` tool and of MCP servers hold: private, unique-local
(Railway's `fd…` addresses), carrier-grade NAT and metadata addresses are refused, so a script
cannot reach `postgres.railway.internal`. The admin key and WorkOS's key stay off the worker.

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

The API and the workers are not deployed by CI ([The images](#the-images)): a commit that
changes the API and the dashboard together reaches `app.rewind.codes` before the API, until
`railway up` deploys it.
