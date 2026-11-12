# Dashboard

The operator's view of ai-ptc: every run, all the code executed, and the tools they called
([05](../archi/05-testing-and-observability.md#dashboard)). A React single-page application, served
as static files by `ptc-server`, and a client of the public API `/v1` only.

| View | Shows |
| --- | --- |
| Overview | The home page, over a window (1 h, 24 h, 7 d, 30 d): runs started (against the previous window), the share that completed, the failed ones, the runs waiting now, durations (p95, p50), quarantined runs; runs by outcome over time; what fails most; failing tools. Every figure, bar and row opens the runs behind it. Polled every 10 s. |
| Runs | Every run the key may read, searched with the query language in one bar: suggestions as you type, the term the server cannot read underlined, facets beside the results (a click asks for a value, a second excludes it, a third removes it), ordered by start, activity or next wake. Saved searches are kept in the browser and listed in the navigation. |
| Waiting | Every run paused on the outside world, by tab (signals, approvals, timers, tools) and grouped by what it waits for, with the line it is paused on and when it wakes, the soonest first. Polled every 10 s. |
| Failures | Failed, cancelled and aborted runs of a window grouped by fingerprint: signature, runs, their series, scripts, first and last seen, the line most of them ended at, top labels, the latest run. |
| Run | The source, with each tool call, timer, signal wait and log next to the line that caused it; the timeline; the journal, the logs and the LLM trace; payloads on demand. Live through the event stream while the run goes on. A time cursor steps through the history (`?step=`): the marks of each line, hovers that show a call's arguments and result or a variable's value at that step, watches, a playhead on the timeline. |
| Scripts | Every distinct script, with its runs by outcome, its tools and its labels; one script's source and runs. |
| Tools | Latency (p50, p95, p99, max), error rate by kind and retries per tool, grouped by namespace, over a window, optionally for one toolset. |

The query (`q`) follows the operator from Overview to Runs, Waiting and Failures in the URL.
Links of earlier versions (`#/runs?status=failed&label=agent:triage&tool=…`) are read as the
query they mean.

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
- Lists search with `q` (`GET /v1/runs`, `/v1/runs/stats`, `/v1/runs/failures`); the server
  reads the query and answers `invalid-query` with the position of the term at fault, in
  characters. Suggestions and facets come from `GET /v1/runs/stats?q=…&top=…`.
- The time cursor is computed in the browser from the journal: a step is a trigger
  (`run_started`, a tool's result or failure, `timer_fired`, `signal_received`,
  `cancel_requested`) and the entries committed with it. What a hover shows is found with
  Lezer's syntax tree: a callsite points at its callee's last name, which leads to the call and
  to the declaration that binds its awaited result. Only values that crossed the journal are
  known; the dashboard runs no code.
- Charts are small SVG components in the dashboard's tokens; no chart library.

## Layout

| Path | Holds |
| --- | --- |
| `src/api/` | The client, the event stream, the data hooks (TanStack Query), the DTO names |
| `src/model/` | Pure code: a run's activities and steps from its journal, callsites and bindings in the source, the query language, the timeline's axis, filters, formats, highlighting |
| `src/views/` | Overview, Runs (with its facets), Waiting, Failures, Run (`run/`, with the time cursor), Scripts, Script, Tools |
| `src/components/` | Shared pieces: statuses, payloads, the source view, the query bar, charts, tooltips |
| `src/app/` | Routes, the key gate, the session, settings (theme, time zone), saved searches |
| `src/mocks/` | The mocked API: a world of fixtures (with the e2e corpus's `triage_workflow` run), a small read model that reads the query language, MSW handlers, the Vite plugin of `dev:mock` |
| `src/styles/` | Tokens for light and dark, and the styles |
