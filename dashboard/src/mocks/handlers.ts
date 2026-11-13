// The mocked API /v1, as MSW handlers over a world of fixtures. The tests run them with
// `msw/node`; `pnpm dev:mock` serves them from the Vite dev server.
import { HttpResponse, http } from "msw";
import type {
  FailuresQuery,
  ListRunsQuery,
  Problem,
  RunSort,
  RunStatsQuery,
  RunStatus,
  StreamEvent,
} from "../api/types";
import type { World } from "./fixtures";
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

const PROBLEM_TITLES: Record<string, string> = {
  unauthorized: "Unauthorized",
  "not-found": "Not found",
  "invalid-request": "Invalid request",
  "invalid-query": "The query cannot be read",
};

export function problem(status: number, slug: string, detail: string): HttpResponse<Problem> {
  return HttpResponse.json(
    { type: `urn:rewind:problem:${slug}`, title: PROBLEM_TITLES[slug] ?? slug, status, detail },
    { status, headers: { "Content-Type": "application/problem+json" } },
  ) as HttpResponse<Problem>;
}

/** A query the server cannot read: `invalid-query`, with the position of the term at fault. */
function invalidQuery(error: QueryError): HttpResponse<Problem> {
  return HttpResponse.json(
    {
      type: "urn:rewind:problem:invalid-query",
      title: PROBLEM_TITLES["invalid-query"] ?? "invalid-query",
      status: 400,
      detail: error.message,
      query: { start: error.start, end: error.end },
    },
    { status: 400, headers: { "Content-Type": "application/problem+json" } },
  ) as HttpResponse<Problem>;
}

/** Answers with what `answer` computes, or with `invalid-query` when the query cannot be read. */
function reading(answer: () => unknown): Response {
  try {
    return HttpResponse.json(answer() as never);
  } catch (error) {
    if (error instanceof QueryError) return invalidQuery(error);
    throw error;
  }
}

function refused(world: World, request: Request): HttpResponse<Problem> | null {
  if (!world.requireKey) return null;
  const header = request.headers.get("Authorization");
  if (header === `Bearer ${world.requireKey}`) return null;
  return problem(
    401,
    "unauthorized",
    header
      ? "This API key is unknown or revoked. Create a key with POST /v1/api-keys."
      : "Send an API key: Authorization: Bearer rewind_sk_….",
  );
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
  const find = (id: string) => world.runs.find((r) => r.id === id);
  const noRun = (id: string) => problem(404, "not-found", `No run ${id} for this tenant.`);

  return [
    http.all("*/v1/*", ({ request }) => refused(world, request) ?? undefined),

    http.get("*/v1/server", () =>
      HttpResponse.json({
        version: "0.1.0",
        epochs: [1],
        roles: ["api", "worker"],
        embedded: world.embedded,
        virtualTime: false,
      }),
    ),

    http.get("*/v1/runs", ({ request }) => {
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
      return reading(() => listRuns(world, query, now()));
    }),

    http.get("*/v1/runs/stats", ({ request }) => {
      const q = new URL(request.url).searchParams;
      const query: RunStatsQuery = {};
      for (const key of ["q", "since", "until", "bucket"] as const) {
        const value = q.get(key);
        if (value !== null) query[key] = value;
      }
      const top = int(q.get("top"));
      if (top !== undefined) query.top = top;
      return reading(() => runStats(world, query, now()));
    }),

    http.get("*/v1/runs/failures", ({ request }) => {
      const q = new URL(request.url).searchParams;
      const query: FailuresQuery = {};
      for (const key of ["q", "since", "until", "bucket"] as const) {
        const value = q.get(key);
        if (value !== null) query[key] = value;
      }
      const limit = int(q.get("limit"));
      if (limit !== undefined) query.limit = limit;
      return reading(() => failureGroups(world, query, now()));
    }),

    http.get("*/v1/runs/:id", ({ params }) => {
      const fixture = find(String(params.id));
      return fixture ? HttpResponse.json(deriveRun(fixture, now())) : noRun(String(params.id));
    }),

    http.get("*/v1/runs/:id/entries/:idx", ({ params }) => {
      const fixture = find(String(params.id));
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
        return { ...value, value: world.payloads.get(value.digest) };
      });
      return HttpResponse.json(full);
    }),

    http.get("*/v1/runs/:id/trace", ({ params }) => {
      const fixture = find(String(params.id));
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

    http.get("*/v1/runs/:id/journal", ({ params, request }) => {
      const fixture = find(String(params.id));
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

    http.get("*/v1/runs/:id/events", ({ params, request }) => {
      const fixture = find(String(params.id));
      if (!fixture) return noRun(String(params.id));
      const q = new URL(request.url).searchParams;
      const follow = bool(q.get("follow")) ?? false;
      const lastId = request.headers.get("Last-Event-ID");
      let next = int(q.get("from")) ?? (lastId === null ? 0 : Number(lastId) + 1);
      const encoder = new TextEncoder();
      let timer: ReturnType<typeof setInterval> | undefined;
      const stream = new ReadableStream<Uint8Array>({
        start(controller) {
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
              if (push()) clearInterval(timer);
            } catch {
              clearInterval(timer);
            }
          }, pollMs);
          request.signal.addEventListener("abort", () => clearInterval(timer));
        },
        cancel() {
          clearInterval(timer);
        },
      });
      return new HttpResponse(stream, {
        headers: { "Content-Type": "text/event-stream", "Cache-Control": "no-cache" },
      });
    }),

    http.get("*/v1/scripts", ({ request }) => {
      const q = new URL(request.url).searchParams;
      const tool = q.get("tool");
      const cursor = q.get("cursor");
      const limit = int(q.get("limit"));
      return HttpResponse.json(
        listScripts(
          world,
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

    http.get("*/v1/scripts/:digest", ({ params }) => {
      const script = world.scripts.find((s) => s.digest === params.digest);
      return script
        ? HttpResponse.json({ digest: script.digest, source: script.source })
        : problem(404, "not-found", `No script ${String(params.digest)} for this tenant.`);
    }),

    http.get("*/v1/tools/stats", ({ request }) => {
      const q = new URL(request.url).searchParams;
      const query: Record<string, string> = {};
      for (const key of ["toolset", "tool", "since", "until"]) {
        const value = q.get(key);
        if (value) query[key] = value;
      }
      return HttpResponse.json(toolStats(world, query, now()));
    }),

    http.get("*/v1/payloads/:digest", ({ params }) => {
      const digest = String(params.digest);
      return world.payloads.has(digest)
        ? HttpResponse.json(world.payloads.get(digest) as never)
        : problem(404, "not-found", `No payload ${digest} for this tenant.`);
    }),
  ];
}
