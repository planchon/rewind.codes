---
name: deploy
description: Deploy Rewind's server, the `rewind` binary in its Docker image, to production on Railway, from the developer's machine. Covers the local gate that replaces Rust CI, the release gate for releases, `railway up` of the node and the edge, and the checks after. Use whenever the owner asks to deploy, release or ship the server, the binary or the image, or asks whether main is ready to go out. The SDK on npm and the sites on Vercel are not deployed here: GitHub Actions does that.
---

# Deploying Rewind

GitHub Actions no longer builds Rust or the image (archi/08-decisions.md, "Taken for the
development loop"). It still checks the pnpm packages, deploys `app.rewind.codes` and
`rewind.codes` on every push to main, and publishes the SDK on an `sdk-v*` tag. The server goes
out from this machine, by the steps below. The platform's facts (services, variables, the
stop-first volume, the 10 s outage) are in [deploy/railway/README.md](../../../deploy/railway/README.md).

Production is shared: **ask the owner before step 4**, and say what goes out, which checks ran and
which did not.

## 1. What goes out

Deploy a commit of `main` that is pushed: run from the main checkout, or a worktree at
`origin/main`, with a clean tree.

```sh
git fetch origin && git status --short          # clean, and HEAD == origin/main
railway deployment list --service node --limit 3 --json   # the last deploy's message names its commit
git log --oneline <deployed>..HEAD             # what this deploy brings
```

Read the range for what needs care: a Postgres migration (`crates/rewind-store-postgres/migrations/`),
a change of V8 or ICU (a new determinism epoch, deploy/README.md "Upgrades"), a new variable, a
change of `deploy/railway/`. A range that holds the rename release (its migration 17 and
`rewind server rename-data`) goes out by [its own steps](#the-rename-release).

## 2. The gate

```sh
scripts/gate.sh      # fmt, clippy -D warnings, nextest (Docker tests required), doctests, rustdoc
```

It must pass. It is the only check the Rust workspace has.

## 3. The release gate, for a release

`acceptance/release.sh` builds the release binary, runs Rewind as production runs it, drives every
feature, and ends GO, NO-GO or PARTIAL (acceptance/release/README.md). Its load stage builds the
Docker images and runs the reference deployment under load (`deploy/acceptance.sh`), so it also
proves the `Dockerfile`.

Run it, and require GO, when the range has a migration, a change of the engine, a store, the
runtime or the run plane, a new epoch, or a change of `Dockerfile` or `deploy/`. For a fix of a
few lines elsewhere, skip it and tell the owner it was skipped. It takes a while: start it in the
background and wait for its verdict, and read `acceptance/results/release/report.md` on NO-GO.

## 4. Deploy (after the owner's yes)

Railway builds `deploy/railway/Dockerfile` from the uploaded tree (about 10 minutes); the node's
volume makes it stop the old node before it starts the new one, so the API is down about 10 s.

```sh
sha=$(git rev-parse --short HEAD)
railway up --service node --ci --message "$sha $(git log -1 --format=%s)"
railway up --service edge --ci --message "$sha"   # only when deploy/railway/edge/ changed
```

`--ci` streams the build and returns when it ends. A build that fails leaves the running node as
it was: read the build log, fix on main, start again at step 1. A deploy that needs a one-time
pre-deploy command (as `rewind server migrate-runs` once did) sets it on the service for that
deploy and removes it after.

### The rename release

The rename release renames what the deployment stored, offline, once (deploy/README.md, "Rename
release"; `archi/06-storage-and-scale.md`, "Rewriting the stored names"). It needs the release
gate's GO, and a rehearsal on a copy of production first (the bucket's prefix and a dump of the
database, as deploy/README.md shows), with `rename-data --dry-run`, `rename-data`, then
`rename-data` again saying `nothing to rename`. Tell the owner the API is down for the whole
rewrite, not 10 s. Then, after the owner's yes:

1. Stop the `node` service: `rename-data` refuses a deployment a node serves.
2. Rename the node's variables the release renamed, those it has
   (`railway variable list --service node --kv`): `REWIND_ADMIN_TENANT` to
   `REWIND_ADMIN_ORGANIZATION`, `REWIND_TENANT_POOLS` to `REWIND_ORGANIZATION_POOLS`,
   `REWIND_QUOTA_SCRIPTS` to `REWIND_QUOTA_WORKFLOWS`, `REWIND_QUOTA_MAX_SCRIPT_LINES` to
   `REWIND_QUOTA_MAX_CODE_LINES`, `REWIND_ALLOW_INTERNAL_TOOLS` to
   `REWIND_ALLOW_INTERNAL_ADDRESSES`. The new build does not read the old names.
3. Run `rewind server rename-data --dry-run`, then `rewind server rename-data`, once, from a
   one-off service on the new image with the node's variables; read what it prints (the counts,
   the titles it could not turn into names), and remove the service.
4. Deploy the node on the new image (`railway up --service node`, above), then check (step 5),
   with `GET /v1/workflows` listing the saved workflows under their names and versions.

## 5. Check

```sh
curl -fsS -o /dev/null -w '%{http_code}\n' https://api.rewind.codes/          # 200: the dashboard
key=$(railway variable list --service node --kv | sed -n 's/^REWIND_ADMIN_KEY=//p')
curl -fsS -H "Authorization: Bearer $key" https://api.rewind.codes/v1/server    # version, epochs
railway logs --service node | tail -100                                         # no panic, no ERROR loop
```

Then a smoke run through the CLI (`cargo build --release -p rewind`, or the gate's build):

```sh
printf 'import { sleep } from "rewind";\nexport default async function main() {\n  await sleep("2s");\n  return "ok";\n}\n' > "$TMPDIR/smoke.ts"
REWIND_URL=https://api.rewind.codes REWIND_API_KEY="$key" \
  target/release/rewind --namespace smoke-deploy run "$TMPDIR/smoke.ts"
```

It must complete with `"ok"`. Tell the owner the commit deployed, the checks and their results.
If the smoke run fails, say so at once with the node's logs: the previous image is one
`railway redeploy` of the previous deployment away (`railway deployment list`).
