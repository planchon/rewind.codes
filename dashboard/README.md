# Dashboard

The operator's view of ai-ptc: every run, all the code executed, and the tools they called
([05](../archi/05-testing-and-observability.md#dashboard)). A React single-page application, served
as static files by `ptc-server`, and a client of the public API `/v1` only.

| View | Shows |
| --- | --- |
| Runs | Every run the key may read, filtered by status, label, script, tool, error kind or name, start time, quarantine and parent. Filters live in the URL. |
| Run | The source, with each tool call, timer, signal wait and log next to the line that caused it; the timeline; the journal, the logs and the LLM trace; payloads on demand. Live through the event stream while the run goes on. |
| Scripts | Every distinct script, with its runs by outcome, its tools and its labels; one script's source and runs. |
| Tools | Latency (p50, p95, p99, max), error rate by kind and retries per tool, grouped by namespace, over a window, optionally for one toolset. |

## Commands

Node 22.12 or later and pnpm 10.

```sh
pnpm install
pnpm dev:mock                          # the app against the mocked API of src/mocks (a run goes on live)
PTC_MOCK_KEY=ptc_sk_x pnpm dev:mock    # the same, with an API key required
PTC_URL=http://localhost:8080 pnpm dev # the app against a ptc server; /v1 is proxied to it
pnpm test                              # vitest, against the mocked API
pnpm typecheck                         # TypeScript 7, strict
pnpm check                             # Biome: lint and format
pnpm build                             # type-check, then build to dist/
pnpm generate:api                      # regenerate src/generated/api.ts from ../api/openapi.json
```

## The build

`pnpm build` writes `dist/`: `index.html`, `favicon.svg` and `assets/` (scripts, styles and fonts,
named by their content hash). `dist/` is not committed; `ptc-server` embeds it
([07](../archi/07-code-architecture.md#dashboard)).

- Every path in the build is relative, and the routes live in the URL fragment (`#/runs/…`): the
  server can serve the directory under any prefix, with no fallback route.
- The app calls the API at `/v1` on the origin that serves it.
- Nothing is loaded from elsewhere: the fonts (IBM Plex Sans and Mono) are in the build.
- `index.html` should not be cached; the files of `assets/` never change.

## How it uses the API

- Types come from `api/openapi.json` (`openapi-typescript`, committed in `src/generated/api.ts`);
  calls go through `openapi-fetch` (`src/api/client.ts`).
- It only sends `GET` requests: a key with the `reader` role is enough. The operator types the key;
  it stays in `sessionStorage`, or in `localStorage` when asked. A server that needs no key lets the
  operator in at once.
- The event stream is read with `fetch` and `eventsource-parser`, since `EventSource` cannot send the
  `Authorization` header; a dropped stream resumes from the next entry (`from`).
- A payload shows its size, digest and preview; its value is shown on demand, and read with
  `GET /v1/payloads/{digest}` when the entry left it out (above 256 KiB).

## Layout

| Path | Holds |
| --- | --- |
| `src/api/` | The client, the event stream, the data hooks (TanStack Query), the DTO names |
| `src/model/` | Pure code: a run's activities from its journal, the timeline's axis, filters, formats, highlighting |
| `src/views/` | Runs, Run (`run/`), Scripts, Script, Tools |
| `src/components/` | Shared pieces: statuses, payloads, the source view, tooltips |
| `src/app/` | Routes, the key gate, the session, settings (theme, time zone) |
| `src/mocks/` | The mocked API: a world of fixtures, a small read model, MSW handlers, the Vite plugin of `dev:mock` |
| `src/styles/` | Tokens for light and dark, and the styles |
