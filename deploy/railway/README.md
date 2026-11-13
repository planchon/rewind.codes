# Rewind on Railway and Vercel

The hosted deployment of ai-ptc, under the name Rewind: the API and the workers on Railway, the
sites on Vercel, all under `rewind.codes`. It is the reference deployment of
[`deploy/`](../README.md) adapted to a platform with managed Postgres and S3, no network policy
and no choice of container runtime. It has one user, its owner.

| Name | Where | What |
| --- | --- | --- |
| `rewind.codes` | Vercel, project `rewind-landing` ([`landing/`](../../landing)) | The landing, static. `www.rewind.codes` redirects to it |
| `app.rewind.codes` | Vercel, project `rewind-dashboard` ([`dashboard/`](../../dashboard)) | The dashboard. `/v1/*` is rewritten to `api.rewind.codes`, so the app calls the API on its own origin |
| `api.rewind.codes` | Railway, service `edge` | Caddy ([`edge/Caddyfile`](edge/Caddyfile)): `POST /v1/test` to the workers, everything else to the api |
| (private) | Railway, service `api` | `ptc server --role api`: the HTTP API and keys. No V8 |
| (private) | Railway, service `worker` | `ptc server --role worker`: activations, tool dispatch, scheduler, projector, retention |
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
failed. `PTC_OTLP_ENDPOINT` is not set: the metric reader exports every 60 seconds, which would
keep the api awake. Logs are in Railway.

## The images

Railway builds [`Dockerfile`](Dockerfile) here for the api and the worker: the server stage of
the repository's `Dockerfile`, without its BuildKit cache mounts, whose ids Railway restricts to
one service. The start command picks the role, in exec form since the image has no shell:

| Service | Start command |
| --- | --- |
| `api` | `/usr/local/bin/ptc server --role api` |
| `worker` | `/usr/local/bin/ptc server --role worker` |

The image listens on `[::]:8080` (`PTC_LISTEN`): Railway's private network is IPv6, and the edge
reaches `api.railway.internal:8080` and `worker.railway.internal:8080` there. Every service sets
`PORT=8080`. The health check is `GET /`: the embedded dashboard, which answers without a key.
The edge is [`edge/Dockerfile`](edge/Dockerfile), Caddy with its Caddyfile.

A deploy uploads the working tree from the repository's root, one service at a time:
`railway up --service api`, `--service worker`, `--service edge` (`RAILWAY_DOCKERFILE_PATH` is
set on each). A release build of the server takes Railway about 10 minutes.

## Variables

| Variable | Services | Value |
| --- | --- | --- |
| `PTC_STORE` | api, worker | `${{Postgres.DATABASE_URL}}`, on the private network |
| `PTC_BLOBS` | api, worker | `s3://${{payloads.BUCKET}}` |
| `PTC_S3_ENDPOINT`, `PTC_S3_REGION` | api, worker | `${{payloads.ENDPOINT}}`, `${{payloads.REGION}}` |
| `PTC_S3_ACCESS_KEY_ID`, `PTC_S3_SECRET_ACCESS_KEY` | api, worker | References to the bucket's credentials: a reset of the credentials follows |
| `PTC_MASTER_KEY` | api, worker | 32 random bytes in base64. Back it up with the database: without it every stored credential is lost |
| `PTC_ADMIN_KEY`, `PTC_ADMIN_TENANT` | api | The first admin key (`ptc_sk_…`), tenant `default`. Never on the worker: it runs code written by a model |
| `PTC_PUBLIC_URL` | api, worker | `https://api.rewind.codes`: `webhook()` URLs start with it |
| `PTC_WORKER_NAME` | api | `api`. The worker keeps the default, its host name and process id, unique per replica |
| `PTC_ACTIVATIONS`, `PTC_DISPATCHES`, `PTC_CACHE_INSTANCES`, `PTC_CACHE_MEMORY` | worker | `4`, `64`, `500`, `384MiB`, for a 1 GB limit |
| `PTC_LOG` | api, worker | `info` |

Read the admin key with `railway variable list --service api --kv | grep PTC_ADMIN_KEY`. It opens
the dashboard, and creates the other keys as in
[`deploy/README.md`](../README.md#tenants-and-api-keys), against `https://api.rewind.codes`.

## The walls

Railway runs ordinary containers on a shared private network, and Rewind has one user: the owner
decided that it needs neither gVisor nor the egress gateway (13 November 2026). Of the walls of
[`archi/06-storage-and-scale.md`](../../archi/06-storage-and-scale.md) ("Sandboxed workers"),
V8 and the egress checks of the `http` tool and of MCP servers hold: private, unique-local
(Railway's `fd…` addresses), carrier-grade NAT and metadata addresses are refused, so a script
cannot reach `postgres.railway.internal`. The admin key stays off the worker.

## Vercel

Each site is a Vercel project deployed from its directory (`vercel deploy --prod` in `landing/`
or `dashboard/`, linked to the team `planchons-projects`). The dashboard's `vercel.json` builds
it with pnpm and rewrites `/v1/*` to the API; the landing is static files with a few headers.
