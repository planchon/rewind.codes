# Examples

Harness code that uses [`@rewind.codes/sdk`](../sdk/typescript/README.md), one directory per
subject. Each `index.ts` is a program you run; each `workflows/*.ts` is code a model would write,
and runs in Rewind's sandbox (the workflows example's is in [`workflows/workflows/`](workflows/workflows)). The documentation at [docs.rewind.codes](https://docs.rewind.codes)
walks through them.

| Example | Shows |
| --- | --- |
| [`quickstart/`](quickstart/index.ts) | An action set with the `http` action and a harness action, a worker, a run and its output |
| [`agent/`](agent/anthropic.ts) | An agent loop on Claude ([`anthropic.ts`](agent/anthropic.ts)) and on the OpenAI Responses API ([`openai.ts`](agent/openai.ts)), with the agent tools |
| [`human-approval/`](human-approval/index.ts) | A workflow waiting days for a signal, and a call the harness approves |
| [`harness-actions/`](harness-actions/index.ts) | A worker: failures the workflow catches, idempotency keys, an answer deferred by its token |
| [`workflows/`](workflows/index.ts) | A named workflow with an interface, a workflow that fans out over its runs, a run started by name |
| [`end-users/`](end-users/index.ts) | Namespaces, an end user's token, quotas and their refusals, revocation |
| [`webhooks/`](webhooks/index.ts) | A subscription, verified webhooks, `run.waiting` and `run.completed` |
| [`testing/`](testing/index.ts) | Type-checking, tests in virtual time, a fork that fixes a failed run, a cassette |

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
The workflows reach `api.github.com` without credentials: GitHub allows 60 such requests an hour.

`webhooks/` needs a URL Rewind can post webhooks to: a public `https://` URL (a tunnel to your
machine, `WEBHOOK_URL=…`), or a server of your own started with `--allow-private-webhooks`.
