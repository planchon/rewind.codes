// The mocked API, as MSW handlers over a world of fixtures: `/v1` and the sign-in `/auth`. The
// tests run them with `msw/node`; `pnpm dev:mock` serves them from the Vite dev server.
import { HttpResponse, http } from "msw";
import type {
  FailuresQuery,
  ListRunsQuery,
  RunSort,
  RunStatsQuery,
  RunStatus,
  StreamEvent,
} from "../api/types";
import type { Caller } from "./access";
import { accessHandlers, authenticate, guard, runsWorld } from "./access-handlers";
import type { World } from "./fixtures";
import { headerNamespace, namespaceHandlers } from "./namespace-handlers";
import { invalidQuery, problem } from "./problem";
import { QueryError } from "./query";
import {
  deriveRun,
  failureGroups,
  listRuns,
  listScripts,
  runStats,
  toolStats,
  visibleEntries,
} from "./readmodel";

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

/** The runs of one namespace, and the scripts they ran; every namespace without one. */
function inNamespace(world: World, namespace: string | null): World {
  if (namespace === null) return world;
  const runs = world.runs.filter((r) => r.namespace === namespace);
  const digests = new Set(runs.map((r) => r.script.digest));
  return { ...world, runs, scripts: world.scripts.filter((s) => digests.has(s.digest)) };
}

function sse(event: StreamEvent): string {
  const id = event.type === "entry" ? `id: ${event.idx}\n` : "";
  return `event: ${event.type}\n${id}data: ${JSON.stringify(event)}\n\n`;
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
  /** How often a followed stream looks for new entries. */
  pollMs?: number;
}

export function createHandlers(world: World, options: HandlerOptions = {}) {
  const now = options.now ?? (() => Date.now());
  const pollMs = options.pollMs ?? 250;
  const noRun = (id: string) => problem(404, "not-found", `No run ${id} for this tenant.`);
  const find = (w: World, id: string) => w.runs.find((r) => r.id === id);

  return [
    // Every request to /v1 carries a token, unless the server needs none.
    http.all("*/v1/*", ({ request }) => {
      const caller = authenticate(world, request);
      return caller instanceof Response ? caller : undefined;
    }),

    ...accessHandlers(world),

    ...namespaceHandlers(world, now),

    http.get("*/v1/server", () =>
      HttpResponse.json({
        version: "0.1.0",
        epochs: [1],
        roles: ["api", "worker"],
        embedded: world.embedded,
        virtualTime: false,
      }),
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
          "tool",
          "script",
          "error",
          "parent",
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
      withRuns(world, request, (w) => {
        const q = new URL(request.url).searchParams;
        const query: RunStatsQuery = {};
        for (const key of ["q", "since", "until", "bucket"] as const) {
          const value = q.get(key);
          if (value !== null) query[key] = value;
        }
        const top = int(q.get("top"));
        if (top !== undefined) query.top = top;
        return reading(() => runStats(w, query, now()));
      }),
    ),

    http.get("*/v1/runs/failures", ({ request }) =>
      withRuns(world, request, (w) => {
        const q = new URL(request.url).searchParams;
        const query: FailuresQuery = {};
        for (const key of ["q", "since", "until", "bucket"] as const) {
          const value = q.get(key);
          if (value !== null) query[key] = value;
        }
        const limit = int(q.get("limit"));
        if (limit !== undefined) query.limit = limit;
        return reading(() => failureGroups(w, query, now()));
      }),
    ),

    http.get("*/v1/runs/:id", ({ request, params }) =>
      withRuns(world, request, (w) => {
        const fixture = find(w, String(params.id));
        return fixture ? HttpResponse.json(deriveRun(fixture, now())) : noRun(String(params.id));
      }),
    ),

    http.get("*/v1/runs/:id/entries/:idx", ({ request, params }) =>
      withRuns(world, request, (w) => {
        const fixture = find(w, String(params.id));
        if (!fixture) return noRun(String(params.id));
        const entry = visibleEntries(fixture, now())[Number(params.idx)];
        if (!entry)
          return problem(404, "not-found", `No entry ${String(params.idx)} in run ${fixture.id}.`);
        // This endpoint gives every value, whatever its size.
        const full = JSON.parse(JSON.stringify(entry), (key, value) => {
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

    http.get("*/v1/runs/:id/trace", ({ request, params }) =>
      withRuns(world, request, (w) => {
        const fixture = find(w, String(params.id));
        if (!fixture) return noRun(String(params.id));
        const run = deriveRun(fixture, now());
        const lines = visibleEntries(fixture, now()).map(
          (e) => `  ${String(e.idx).padStart(3)}  ${e.event.type}`,
        );
        return HttpResponse.json({
          runId: run.id,
          format: "llm",
          text: `run ${run.id}  ${run.status}\nscript ${fixture.script.name} · toolset ${run.toolset.slice(0, 12)}\n\n${lines.join("\n")}\n`,
        });
      }),
    ),

    http.get("*/v1/runs/:id/journal", ({ request, params }) =>
      withRuns(world, request, (w) => {
        const fixture = find(w, String(params.id));
        if (!fixture) return noRun(String(params.id));
        const as = new URL(request.url).searchParams.get("as") ?? "journal";
        const entries = visibleEntries(fixture, now());
        return HttpResponse.json(
          as === "cassette"
            ? { v: 1, matching: "sequential", calls: [], signals: [] }
            : {
                v: 1,
                run: fixture.id,
                script: fixture.script.digest,
                toolset: fixture.toolset,
                entries,
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
              const entries = visibleEntries(fixture, now());
              let chunk = "";
              for (; next < entries.length; next += 1) {
                const entry = entries[next];
                if (entry) chunk += sse({ type: "entry", ...entry });
              }
              const status = deriveRun(fixture, now()).status;
              const ended = status !== "active" && status !== "pending";
              if (ended || !follow) chunk += sse({ type: "end", status });
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
        return new HttpResponse(stream, {
          headers: { "Content-Type": "text/event-stream", "Cache-Control": "no-cache" },
        });
      }),
    ),

    http.get("*/v1/scripts", ({ request }) =>
      withRuns(world, request, (w) => {
        const q = new URL(request.url).searchParams;
        const tool = q.get("tool");
        const cursor = q.get("cursor");
        const limit = int(q.get("limit"));
        return HttpResponse.json(
          listScripts(
            w,
            {
              label: q.getAll("label"),
              ...(tool ? { tool } : {}),
              ...(cursor ? { cursor } : {}),
              ...(limit !== undefined ? { limit } : {}),
            },
            now(),
          ),
        );
      }),
    ),

    http.get("*/v1/scripts/:digest", ({ request, params }) =>
      withRuns(world, request, (w) => {
        const script = w.scripts.find((s) => s.digest === params.digest);
        return script
          ? HttpResponse.json({ digest: script.digest, source: script.source })
          : problem(404, "not-found", `No script ${String(params.digest)} for this tenant.`);
      }),
    ),

    http.get("*/v1/tools/stats", ({ request }) =>
      withRuns(world, request, (w, caller) => {
        const grant =
          caller.kind === "socket"
            ? null
            : world.access.grants
                .get(caller.tenant ?? "")
                ?.get(caller.kind === "key" ? `key:${caller.key.id}` : `user:${caller.user.id}`);
        if (Object.keys(grant?.allowedLabels ?? {}).length > 0)
          return problem(
            403,
            "forbidden",
            "A grant limited to labels cannot read the tool statistics, which span every run.",
          );
        const q = new URL(request.url).searchParams;
        const query: Record<string, string> = {};
        for (const key of ["toolset", "tool", "since", "until"]) {
          const value = q.get(key);
          if (value) query[key] = value;
        }
        return HttpResponse.json(toolStats(w, query, now()));
      }),
    ),

    http.get("*/v1/payloads/:digest", ({ request, params }) =>
      withRuns(world, request, (w) => {
        const digest = String(params.digest);
        return w.payloads.has(digest)
          ? HttpResponse.json(w.payloads.get(digest) as never)
          : problem(404, "not-found", `No payload ${digest} for this tenant.`);
      }),
    ),
  ];
}
