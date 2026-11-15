# Examples

Harness code that uses [`@rewind.codes/sdk`](../sdk/typescript/README.md), one directory per
subject. Each `index.ts` is a program you run; each `scripts/*.ts` is what a model would write,
and runs in Rewind's sandbox. The documentation at [docs.rewind.codes](https://docs.rewind.codes)
walks through them.

| Example | Shows |
| --- | --- |
| [`quickstart/`](quickstart/index.ts) | A toolset with the `http` tool and a host tool, a worker, a run and its output |
| [`agent/`](agent/anthropic.ts) | An agent loop on Claude ([`anthropic.ts`](agent/anthropic.ts)) and on the OpenAI Responses API ([`openai.ts`](agent/openai.ts)), with the agent tools |
| [`human-approval/`](human-approval/index.ts) | A script waiting days for a signal, and a tool call the harness approves |
| [`host-tools/`](host-tools/index.ts) | A worker: failures the script catches, idempotency keys, an answer deferred by its token |
| [`workflows/`](workflows/index.ts) | A typed workflow, a script that fans out over its runs, a run started by name |
| [`end-users/`](end-users/index.ts) | Namespaces, an end user's token, quotas and their refusals, revocation |
| [`webhooks/`](webhooks/index.ts) | A subscription, verified deliveries, `run.suspended` and `run.completed` |
| [`testing/`](testing/index.ts) | Type-checking, scenarios in virtual time, a fork that fixes a failed run, a cassette |

## Running them

Node 22.18 or later (it runs TypeScript as is) and pnpm.

```sh
cd examples
pnpm install
cp .env.example .env      # REWIND_URL and REWIND_API_KEY, and a model's key for agent/
pnpm quickstart           # or: pnpm human-approval, pnpm workflows, pnpm testing…
pnpm typecheck            # the harness code, against the SDK's types
```

`REWIND_URL` is `https://api.rewind.codes` with a key made in the dashboard
([app.rewind.codes](https://app.rewind.codes), Organization → API keys), or a server of your own.
The scripts reach `api.github.com` without credentials: GitHub allows 60 such requests an hour.

`webhooks/` needs a URL Rewind can deliver to: a public `https://` URL (a tunnel to your machine,
`WEBHOOK_URL=…`), or a server of your own started with `--allow-private-webhooks`.
