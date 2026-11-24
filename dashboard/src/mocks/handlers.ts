// The mocked API, as MSW handlers over a world of fixtures: `/v1` and the sign-in `/auth`. The
// tests run them with `msw/node`; `pnpm dev:mock` serves them from the Vite dev server.
import { HttpResponse, http } from "msw";
import { buildRunModel } from "../model/run";
import type {
  FailuresQuery,
  HistoryEvent,
  ListRunsQuery,
  RunSort,
  RunStatsQuery,
  RunStatus,
} from "../model/types";
import { isEnded } from "../model/types";
import { type Caller, principalOf } from "./access";
import { accessHandlers, authenticate, guard, runsWorld } from "./access-handlers";
import { connectionHandlers } from "./connection-handlers";
import type { RunFixture, World } from "./fixtures";
import { findRun } from "./handles";
import { headerNamespace, namespaceHandlers } from "./namespace-handlers";
import { invalidQuery, problem } from "./problem";
import { QueryError } from "./query";
import {
  actionStats,
  deriveRun,
  fingerprints,
  listRuns,
  runStats,
  visibleEvents,
} from "./readmodel";
import { eventStream, runHandlers } from "./run-handlers";
import { secretHandlers } from "./secret-handlers";
import { workflowHandlers } from "./workflow-handlers";

export { problem };

/** Answers with what `answer` computes, or with `invalid-query` when the query cannot be read. */
function reading(answer: () => unknown): Response {
  try {
    return HttpResponse.json(answer() as never);
  } catch (error) {
    if (error instanceof QueryError) return invalidQuery(error.message, error.start, error.end);
    throw error;
  }
}

/**
 * Runs `answer` over the runs the caller may read (`runs:read`): its tenant's, carrying its
 * grant's labels, and of the namespace the request names (`Rewind-Namespace`) if it names one.
 */
function withRuns(
  world: World,
  request: Request,
  answer: (world: World, caller: Caller) => Response,
): Response {
  const caller = guard(world, request, "runs:read");
  if (caller instanceof Response) return caller;
  const namespace = headerNamespace(request);
  if (namespace instanceof Response) return namespace;
  return answer(inNamespace(runsWorld(world, caller), namespace), caller);
}

/**
 * As `withRuns`, for the routes that answer from the hourly counters (archi/05, "Statistics"):
 * refused to a grant limited to labels, since counters know no labels.
 */
function withCounters(
  world: World,
  request: Request,
  answer: (world: World, caller: Caller) => Response,
): Response {
  return withRuns(world, request, (w, caller) => {
    const grant =
      caller.kind === "socket"
        ? null
        : world.access.grants.get(caller.tenant ?? "")?.get(principalOf(caller) ?? "");
    if (Object.keys(grant?.allowedLabels ?? {}).length > 0)
      return problem(
        403,
        "forbidden",
        "statistics come from counters, which know no labels: a grant or a token limited to labels cannot read them. Search the runs instead (GET /v1/runs)",
      );
    return answer(w, caller);
  });
}

export function inNamespace(world: World, namespace: string | null): World {
  if (namespace === null) return world;
  const runs = world.runs.filter((r) => r.namespace === namespace);
  const digests = new Set(runs.map((r) => r.code.digest));
  return { ...world, runs, codes: world.codes.filter((c) => digests.has(c.digest)) };
}

/** An event of a run's event stream: one of its history, or its end. */
export function historyMessage(event: HistoryEvent): string {
  return `event: event\nid: ${event.idx}\ndata: ${JSON.stringify({ type: "event", ...event })}\n\n`;
}

function endMessage(status: RunStatus): string {
  return `event: end\ndata: ${JSON.stringify({ type: "end", status })}\n\n`;
}

/**
 * The cassette of a run: its calls with their outcomes, and the signals it received, at their
 * time from its start.
 */
function cassetteOf(world: World, fixture: RunFixture, now: number) {
  const events = visibleEvents(fixture, now);
  const model = buildRunModel(events);
  const start = model.firstAt ?? now;
  return {
    v: 1,
    matching: "sequential",
    calls: model.calls.flatMap((c) =>
      c.end
        ? [
            {
              action: c.action,
              args: world.payloads.get(c.args.digest) ?? c.args.value,
              outcome:
                c.end.outcome === "ok"
                  ? { result: world.payloads.get(c.end.result.digest) ?? c.end.result.value }
                  : { error: c.end.error },
            },
          ]
        : [],
    ),
    signals: model.received.map((r) => ({
      name: r.name,
      payload: world.payloads.get(r.payload.digest) ?? r.payload.value,
      elapsed_ms: r.at - start,
    })),
  };
}

function bool(value: string | null): boolean | undefined {
  if (value === null) return undefined;
  return value === "true" || value === "1";
}

function int(value: string | null): number | undefined {
  return value === null ? undefined : Number(value);
}

export interface HandlerOptions {
  /** The current time; the real clock by default, so that a live run goes on. */
  now?: () => number;
  /** How often a followed stream looks for new events. */
  pollMs?: number;
  /** How often the change stream looks for runs that changed. */
  changesMs?: number;
}

export function createHandlers(world: World, options: HandlerOptions = {}) {
  const now = options.now ?? (() => Date.now());
  const pollMs = options.pollMs ?? 250;
  const changesMs = options.changesMs ?? 1_000;
  const noRun = (id: string) => problem(404, "not-found", `No run ${id} in this organization.`);
  const find = (w: World, id: string) => {
    const run = findRun(world, id);
    return run && w.runs.includes(run) ? run : undefined;
  };

  return [
    // Every request to /v1 carries a token, unless the server needs none.
    http.all("*/v1/*", ({ request }) => {
      const caller = authenticate(world, request);
      return caller instanceof Response ? caller : undefined;
    }),

    ...accessHandlers(world),

    ...namespaceHandlers(world, now),

    ...workflowHandlers(world, now),

    ...runHandlers(world, now, changesMs),

    ...connectionHandlers(world),

    ...secretHandlers(world, now),

    http.get("*/v1/server", () =>
      HttpResponse.json({
        version: "0.1.0",
        epochs: [1],
        node: "node-1",
        partitions: world.embedded ? 1 : 64,
        nodes: 1,
        embedded: world.embedded,
        virtualTime: false,
        // `mcpUrl` and `oauth`, as the world serves MCP.
        ...world.connections.mcp,
      } as never),
    ),

    http.get("*/v1/runs", ({ request }) =>
      withRuns(world, request, (w) => {
        const q = new URL(request.url).searchParams;
        const query: ListRunsQuery = { label: q.getAll("label") };
        const status = q.get("status");
        if (status) query.status = status as RunStatus;
        const sort = q.get("sort");
        if (sort) query.sort = sort as RunSort;
        for (const key of [
          "q",
          "action",
          "error",
          "source",
          "workflow",
          "startedBy",
          "startedAfter",
          "startedBefore",
          "cursor",
        ] as const) {
          const value = q.get(key);
          if (value !== null) query[key] = value;
        }
        const quarantined = bool(q.get("quarantined"));
        if (quarantined !== undefined) query.quarantined = quarantined;
        const limit = int(q.get("limit"));
        if (limit !== undefined) query.limit = limit;
        return reading(() => listRuns(w, query, now()));
      }),
    ),

    http.get("*/v1/runs/stats", ({ request }) =>
      withCounters(world, request, (w) => {
        const q = new URL(request.url).searchParams;
        const query: RunStatsQuery = {};
        for (const key of ["q", "since", "until", "interval"] as const) {
          const value = q.get(key);
          if (value !== null) query[key] = value;
        }
        const top = int(q.get("top"));
        if (top !== undefined) query.top = top;
        return reading(() => runStats(w, query, now()));
      }),
    ),

    http.get("*/v1/runs/failures", ({ request }) =>
      withCounters(world, request, (w) => {
        const q = new URL(request.url).searchParams;
        const query: FailuresQuery = {};
        for (const key of ["q", "since", "until", "interval"] as const) {
          const value = q.get(key);
          if (value !== null) query[key] = value;
        }
        const limit = int(q.get("limit"));
        if (limit !== undefined) query.limit = limit;
        return reading(() => fingerprints(w, query, now()));
      }),
    ),

    http.get("*/v1/runs/:id", ({ request, params }) =>
      withRuns(world, request, (w) => {
        const fixture = find(w, String(params.id));
        return fixture
          ? HttpResponse.json(deriveRun(world, fixture, now()))
          : noRun(String(params.id));
      }),
    ),

    http.get("*/v1/runs/:id/events/:idx", ({ request, params }) =>
      withRuns(world, request, (w) => {
        const fixture = find(w, String(params.id));
        if (!fixture) return noRun(String(params.id));
        const event = visibleEvents(fixture, now())[Number(params.idx)];
        if (!event)
          return problem(404, "not-found", `No event ${String(params.idx)} in run ${fixture.id}.`);
        // This endpoint gives every value, whatever its size.
        const full = JSON.parse(JSON.stringify(event), (key, value) => {
          if (
            key === "" ||
            !value ||
            typeof value !== "object" ||
            !("digest" in value) ||
            "value" in value
          )
            return value;
          return { ...value, value: w.payloads.get(value.digest) };
        });
        return HttpResponse.json(full);
      }),
    ),

    http.get("*/v1/runs/:id/export", ({ request, params }) =>
      withRuns(world, request, (w) => {
        const fixture = find(w, String(params.id));
        if (!fixture) return noRun(String(params.id));
        const as = new URL(request.url).searchParams.get("as") ?? "run";
        if (as !== "run" && as !== "cassette")
          return problem(400, "invalid-request", `as: "${as}" is not one of run, cassette.`);
        const events = visibleEvents(fixture, now());
        return HttpResponse.json(
          as === "cassette"
            ? cassetteOf(world, fixture, now())
            : {
                v: 1,
                run: fixture.id,
                code: fixture.code.digest,
                actionSet: fixture.actionSet,
                events,
              },
        );
      }),
    ),

    http.get("*/v1/runs/:id/events", ({ request, params }) =>
      withRuns(world, request, (w, caller) => {
        const fixture = find(w, String(params.id));
        if (!fixture) return noRun(String(params.id));
        const q = new URL(request.url).searchParams;
        const follow = bool(q.get("follow")) ?? false;
        const lastId = request.headers.get("Last-Event-ID");
        let next = int(q.get("from")) ?? (lastId === null ? 0 : Number(lastId) + 1);
        // A stream opened with an access token is closed when the token expires.
        const closesAt = caller.kind === "user" ? caller.token.expiresAt : null;
        const encoder = new TextEncoder();
        let timer: ReturnType<typeof setInterval> | undefined;
        let expiry: ReturnType<typeof setTimeout> | undefined;
        const stream = new ReadableStream<Uint8Array>({
          start(controller) {
            const stop = () => {
              clearInterval(timer);
              clearTimeout(expiry);
            };
            const push = (): boolean => {
              const events = visibleEvents(fixture, now());
              let chunk = "";
              for (; next < events.length; next += 1) {
                const event = events[next];
                if (event) chunk += historyMessage(event);
              }
              const status = deriveRun(world, fixture, now()).status;
              const ended = isEnded(status);
              if (ended || !follow) chunk += endMessage(status);
              if (chunk) controller.enqueue(encoder.encode(chunk));
              if (ended || !follow) {
                controller.close();
                return true;
              }
              return false;
            };
            if (push()) return;
            timer = setInterval(() => {
              try {
                if (push()) stop();
              } catch {
                stop();
              }
            }, pollMs);
            if (closesAt !== null) {
              expiry = setTimeout(
                () => {
                  stop();
                  try {
                    controller.close();
                  } catch {
                    // Already closed.
                  }
                },
                Math.max(0, closesAt - Date.now()),
              );
            }
            request.signal.addEventListener("abort", stop);
          },
          cancel() {
            clearInterval(timer);
            clearTimeout(expiry);
          },
        });
        return eventStream(stream);
      }),
    ),

    http.get("*/v1/actions/stats", ({ request }) =>
      withCounters(world, request, (w) => {
        const q = new URL(request.url).searchParams;
        const query: Record<string, string> = {};
        for (const key of ["q", "group", "actionSet", "action", "since", "until"]) {
          const value = q.get(key);
          if (value) query[key] = value;
        }
        return reading(() => actionStats(w, query, now()));
      }),
    ),

    // A payload belongs to its run, and is read through it.
    http.get("*/v1/payloads/:digest", ({ request, params }) =>
      withRuns(world, request, (w) => {
        const digest = String(params.digest);
        const run = new URL(request.url).searchParams.get("run");
        if (!run)
          return problem(
            400,
            "invalid-request",
            `run is required: a payload belongs to its run, and is read through it (GET /v1/payloads/${digest}?run=<id>)`,
          );
        if (!find(w, run)) return noRun(run);
        return w.payloads.has(digest)
          ? HttpResponse.json(w.payloads.get(digest) as never)
          : problem(404, "not-found", `no payload ${digest} in the history of run ${run}`);
      }),
    ),
  ];
}
