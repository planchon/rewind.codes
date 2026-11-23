// The mocked API's live runs: the change stream `GET /v1/changes` (a `run` event with the run's
// whole summary each time one changes, as `GET /v1/runs` lists it, among the runs the caller
// reads; `resync` when too many change at once; the control plane's changes the write handlers
// tell, `changes.ts`), and what changes them from the outside: a signal
// (`POST /v1/runs/{id}/signals/{name}`) and a cancellation (`POST /v1/runs/{id}/cancel`).
//
// A run of the world goes on by itself, its entries recorded ahead of time. A signal or a
// cancellation rewrites what is still to come: a signal the run awaits is received now and the run
// completes a moment later; a cancellation is requested now and the run ends cancelled a moment
// later.
import { HttpResponse, http } from "msw";
import { buildRunModel } from "../model/run";
import { isEnded, type RunSummary, type Workflow } from "../model/types";
import type { Caller } from "./access";
import { guard, runsWorld } from "./access-handlers";
import { changeEvent, onChange } from "./changes";
import type { RunFixture, World } from "./fixtures";
import { findRun } from "./handles";
import { problem } from "./problem";
import { deriveRun, publicSummary, summarize, visibleEntries } from "./readmodel";

/** More changes than this in one pass: `resync`, as the server's loop does at its limit. */
const RESYNC_AT = 500;

/** The keys already used, per run: the same key twice delivers once. */
const usedKeys = new WeakMap<RunFixture, Set<string>>();

/** The runs a cancellation was asked of. */
const cancelling = new WeakSet<RunFixture>();

function keyUsed(fixture: RunFixture, key: string | null): boolean {
  if (key === null) return false;
  let keys = usedKeys.get(fixture);
  if (!keys) {
    keys = new Set();
    usedKeys.set(fixture, keys);
  }
  if (keys.has(key)) return true;
  keys.add(key);
  return false;
}

/** Drops what the run would still have done after `now`: something new happens instead. */
function rewriteFrom(fixture: RunFixture, now: number): void {
  const journal = fixture.journal;
  const first = journal.entries.findIndex((e) => Date.parse(e.recordedAt) > now);
  if (first >= 0) journal.entries.splice(first);
  journal.clock = Math.max(now, Date.parse(journal.entries.at(-1)?.recordedAt ?? "") || now);
}

/** The latest version of the run's workflow, if it runs one. */
function workflowOf(world: World, fixture: RunFixture): Workflow | null {
  if (!fixture.workflow) return null;
  const versions = world.workflows.versions.filter((v) => v.name === fixture.workflow?.name);
  return versions.find((v) => v.version === fixture.workflow?.version) ?? versions[0] ?? null;
}

/** What a payload breaks of its schema, read loosely: types, required properties, enumerations. */
function schemaFault(schema: unknown, value: unknown, at = "the payload"): string | null {
  if (!schema || typeof schema !== "object") return null;
  const s = schema as Record<string, unknown>;
  if (Array.isArray(s.enum) && !s.enum.some((v) => JSON.stringify(v) === JSON.stringify(value)))
    return `${at} must be one of ${s.enum.map((v) => JSON.stringify(v)).join(", ")}`;
  const type = typeof s.type === "string" ? s.type : null;
  const kind =
    value === null
      ? "null"
      : Array.isArray(value)
        ? "array"
        : Number.isInteger(value)
          ? "integer"
          : typeof value;
  if (type !== null) {
    const fits =
      type === kind ||
      (type === "number" && kind === "integer") ||
      (type === "object" && kind === "object");
    if (!fits)
      return `${at} must be ${type === "integer" || type === "array" || type === "object" ? "an" : "a"} ${type}`;
  }
  if (type === "object" && value && typeof value === "object" && !Array.isArray(value)) {
    const record = value as Record<string, unknown>;
    for (const key of Array.isArray(s.required) ? s.required : []) {
      if (typeof key === "string" && !(key in record)) return `${at} misses "${key}"`;
    }
    const properties = (s.properties ?? {}) as Record<string, unknown>;
    for (const [key, sub] of Object.entries(properties)) {
      if (!(key in record)) continue;
      const fault = schemaFault(sub, record[key], `"${key}"`);
      if (fault) return fault;
    }
  }
  return null;
}

function readBody(text: string): { value: unknown } | Response {
  if (text.trim() === "") return { value: null };
  try {
    return { value: JSON.parse(text) };
  } catch {
    return problem(400, "invalid-request", "The body is not JSON.");
  }
}

/** The run, among those the caller may control, or the refusal the server answers. */
function controlled(world: World, request: Request, id: string): RunFixture | Response {
  const caller = guard(world, request, "runs:control");
  if (caller instanceof Response) return caller;
  const found = findRun(world, id);
  const fixture = found && runsWorld(world, caller).runs.includes(found) ? found : undefined;
  return fixture ?? problem(404, "not-found", `No run ${id} for this tenant.`);
}

export function runHandlers(world: World, now: () => number, changesMs: number) {
  return [
    http.post("*/v1/runs/:id/signals/:name", async ({ request, params }) => {
      const fixture = controlled(world, request, String(params.id));
      if (fixture instanceof Response) return fixture;
      const name = String(params.name);
      const at = now();
      const run = deriveRun(world, fixture, at);
      if (isEnded(run.status))
        return problem(409, "run-ended", `The run ${run.id} has ended (${run.status}).`);
      const body = readBody(await request.text());
      if (body instanceof Response) return body;
      const workflow = workflowOf(world, fixture);
      if (workflow) {
        const declared = Object.keys(workflow.signals);
        if (!declared.includes(name))
          return problem(
            422,
            "invalid-signal",
            `The workflow ${workflow.name} declares no signal "${name}": it declares ${declared.join(", ") || "none"}.`,
          );
        const fault = schemaFault(workflow.signals[name], body.value);
        if (fault)
          return problem(
            422,
            "invalid-signal",
            `The payload of "${name}" does not match the schema the workflow ${workflow.name} declares: ${fault}.`,
          );
      }
      const duplicate = keyUsed(fixture, request.headers.get("Idempotency-Key"));
      if (!duplicate) {
        const model = buildRunModel(visibleEntries(fixture, at));
        const wait = model.signals.find((s) => s.name === name && s.end === null);
        rewriteFrom(fixture, at);
        const journal = fixture.journal;
        journal.signal(name, body.value);
        if (wait) {
          if (wait.timeoutSeq !== null) journal.cancelTimer(wait.timeoutSeq);
          journal.after(1_500).complete({ received: name });
        }
        for (const [digest, value] of journal.payloads) world.payloads.set(digest, value);
      }
      return HttpResponse.json({ runId: fixture.id, name, duplicate }, { status: 202 });
    }),

    http.post("*/v1/runs/:id/cancel", async ({ request, params }) => {
      const fixture = controlled(world, request, String(params.id));
      if (fixture instanceof Response) return fixture;
      const at = now();
      const run = deriveRun(world, fixture, at);
      if (isEnded(run.status))
        return problem(409, "run-ended", `The run ${run.id} has ended (${run.status}).`);
      // A second cancellation of a run being cancelled is the first, as on the server.
      if (!cancelling.has(fixture)) {
        cancelling.add(fixture);
        rewriteFrom(fixture, at);
        fixture.journal
          .cancelRequested("requested", "Cancelled from the dashboard")
          .after(800)
          .failRun("CancelledError", "The run was cancelled: Cancelled from the dashboard");
      }
      return HttpResponse.json(deriveRun(world, fixture, at), { status: 202 });
    }),

    http.get("*/v1/changes", ({ request }) => {
      const caller = guard(world, request, "runs:read");
      if (caller instanceof Response) return caller;
      return changeStream(world, caller, request, now, changesMs);
    }),
  ];
}

/** The summaries of the runs the caller reads that changed since the last pass. */
function changeStream(
  world: World,
  caller: Caller,
  request: Request,
  now: () => number,
  everyMs: number,
): Response {
  const runs = runsWorld(world, caller).runs;
  const last = new Map<string, string>();
  /** How many entries each run showed at the last pass: a summary changes with its journal. */
  const seen = new Map<string, number>();
  /** The runs that ended: nothing changes them any more. */
  const done = new Set<string>();
  const ended = (fixture: RunFixture, at: number) => {
    const type = visibleEntries(fixture, at).at(-1)?.event.type;
    return type === "complete_run" || type === "fail_run" || type === "run_aborted";
  };
  const pass = (): RunSummary[] => {
    const changed: RunSummary[] = [];
    const at = now();
    for (const fixture of runs) {
      if (done.has(fixture.id)) continue;
      const length = visibleEntries(fixture, at).length;
      if (length === 0 || seen.get(fixture.id) === length) continue;
      seen.set(fixture.id, length);
      if (!opened && ended(fixture, at)) {
        done.add(fixture.id);
        continue;
      }
      const summary = publicSummary(summarize(world, fixture, at));
      const json = JSON.stringify(summary);
      if (last.get(fixture.id) !== json) {
        if (opened) changed.push(summary);
        last.set(fixture.id, json);
      }
      if (isEnded(summary.status)) done.add(fixture.id);
    }
    return changed;
  };
  let opened = false;
  pass();
  opened = true;
  // A stream opened with an access token is closed when the token expires.
  const closesAt = caller.kind === "user" ? caller.token.expiresAt : null;
  const encoder = new TextEncoder();
  let timer: ReturnType<typeof setInterval> | undefined;
  let expiry: ReturnType<typeof setTimeout> | undefined;
  let unfollow: (() => void) | undefined;
  const stop = () => {
    clearInterval(timer);
    clearTimeout(expiry);
    unfollow?.();
  };
  const stream = new ReadableStream<Uint8Array>({
    start(controller) {
      // What the control plane's writes changed, as they happen.
      unfollow = onChange(world, (change) => {
        try {
          controller.enqueue(encoder.encode(changeEvent(change)));
        } catch {
          stop();
        }
      });
      const close = () => {
        stop();
        try {
          controller.close();
        } catch {
          // Already closed.
        }
      };
      timer = setInterval(() => {
        try {
          const changed = pass();
          if (changed.length === 0) return;
          const chunk =
            changed.length > RESYNC_AT
              ? "event: resync\ndata: {}\n\n"
              : changed.map((run) => `event: run\ndata: ${JSON.stringify(run)}\n\n`).join("");
          controller.enqueue(encoder.encode(chunk));
        } catch {
          close();
        }
      }, everyMs);
      if (closesAt !== null) expiry = setTimeout(close, Math.max(0, closesAt - Date.now()));
      request.signal.addEventListener("abort", stop);
      // A comment, so that the stream's headers and first bytes leave at once.
      controller.enqueue(encoder.encode(": open\n\n"));
    },
    cancel: stop,
  });
  return eventStream(stream);
}

/**
 * A response of server-sent events. Chunked, as a server sends it: a stream the browser drops is
 * then not read as a body that ended.
 */
export function eventStream(stream: ReadableStream<Uint8Array>): Response {
  return new HttpResponse(stream, {
    headers: {
      "Content-Type": "text/event-stream",
      "Cache-Control": "no-cache",
      "Transfer-Encoding": "chunked",
    },
  });
}
