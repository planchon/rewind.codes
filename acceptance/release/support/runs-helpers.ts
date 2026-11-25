/**
 * Helpers of the run-centred areas of the release gate (runs, endings, workflows, check and
 * test): start a run as raw HTTP, wait for its end, read its history's events.
 */
import { eventually } from "../../support/processes.js";
import { type Api, expect, expectOk, type Reply, recordingFetch } from "./gate.js";

/** The statuses a run ends with. */
export const ENDED = new Set(["completed", "failed", "cancelled", "aborted"]);

/** `POST /v1/runs` in a namespace; `key` is the `Idempotency-Key`. */
export function startRun(api: Api, body: Record<string, unknown>, options: { key?: string } = {}): Promise<Reply> {
  return api.post("/v1/runs", body, options.key ? { headers: { "idempotency-key": options.key } } : {});
}

/** Starts a run and asserts it was created (201); returns the run. */
export async function started(api: Api, body: Record<string, unknown>, options: { key?: string } = {}): Promise<any> {
  const reply = await startRun(api, body, options);
  expectOk(reply, 201);
  return reply.json;
}

/** The run's state now. */
export async function getRun(api: Api, id: string): Promise<any> {
  const reply = await api.get(`/v1/runs/${id}`);
  expectOk(reply, 200);
  return reply.json;
}

/** Waits until the run has ended, up to `timeoutMs`; returns its state. */
export async function ended(api: Api, id: string, timeoutMs = 60_000): Promise<any> {
  return eventually(
    `run ${id} ends`,
    async () => {
      const reply = await api.get(`/v1/runs/${id}`, { query: { wait: "20s" } });
      expectOk(reply, 200);
      return ENDED.has(reply.json.status) ? reply.json : undefined;
    },
    timeoutMs,
    100,
  );
}

/** Waits until the run waits for something of `kind` (`signal`, `timer`, `call`…). */
export async function waitingFor(api: Api, id: string, kind: string, name?: string, timeoutMs = 30_000): Promise<any> {
  return eventually(
    `run ${id} waits for a ${kind}${name ? ` ${name}` : ""}`,
    async () => {
      const run = await getRun(api, id);
      const found = (run.waitingFor ?? []).find((w: any) => w.kind === kind && (name === undefined || w.name === name));
      return found ? run : undefined;
    },
    timeoutMs,
    100,
  );
}

/** The run's export (`GET /v1/runs/{id}/export`). */
export async function runExport(api: Api, id: string): Promise<any> {
  const reply = await api.get(`/v1/runs/${id}/export`);
  expectOk(reply, 200);
  return reply.json;
}

/** The `type` of each event of the run's history. */
export async function events(api: Api, id: string): Promise<string[]> {
  return (await runExport(api, id)).events.map((event: any) => event.event.type);
}

/** Asserts a run was aborted for a limit, with `message` in its reason. */
export function expectLimit(run: any, message: string): void {
  const context = JSON.stringify({ status: run.status, abort: run.abort, error: run.error });
  expect(run.status, context).toBe("aborted");
  expect(run.abort?.kind, context).toBe("limit_exceeded");
  expect(run.abort?.message ?? "", context).toContain(message);
}

/** Asserts a run failed with the error `name`, and `message` in it when given. */
export function expectFailed(run: any, name: string, message?: string): void {
  const context = JSON.stringify({ status: run.status, error: run.error, abort: run.abort });
  expect(run.status, context).toBe("failed");
  expect(run.error?.name, context).toBe(name);
  if (message !== undefined) expect(run.error?.message ?? "", context).toContain(message);
}

/** The ids of the runs of a namespace, from the run records (`GET /v1/runs`). */
export async function runIds(api: Api, query: Record<string, string | number | (string | number)[] | undefined> = {}): Promise<string[]> {
  const reply = await api.get("/v1/runs", { query: { limit: 200, ...query } });
  expectOk(reply, 200);
  return (reply.json.runs as { id: string }[]).map((r) => r.id).sort();
}

/** Parses a server-sent events body into its events. */
export function parseSse(text: string): { event: string; id?: string; data: any }[] {
  const out: { event: string; id?: string; data: any }[] = [];
  for (const block of text.split(/\n\n+/)) {
    if (block.trim() === "") continue;
    let event = "message";
    let id: string | undefined;
    const data: string[] = [];
    for (const line of block.split("\n")) {
      if (line.startsWith("event:")) event = line.slice(6).trim();
      else if (line.startsWith("id:")) id = line.slice(3).trim();
      else if (line.startsWith("data:")) data.push(line.slice(5).replace(/^ /, ""));
    }
    if (data.length === 0 && event === "message") continue;
    let parsed: any = data.join("\n");
    try {
      parsed = JSON.parse(parsed);
    } catch {
      // Not JSON: kept as text.
    }
    out.push({ event, id, data: parsed });
  }
  return out;
}

/** One server-sent event. */
export interface SseEvent {
  event: string;
  id?: string;
  data: any;
}

/** An event stream read as it arrives: its events so far, until it is closed. */
export interface Following {
  status: number;
  contentType: string;
  events: SseEvent[];
  /** The first event `match` accepts, once it arrived; `undefined` after `timeoutMs`. */
  until(match: (event: SseEvent) => boolean, timeoutMs: number): Promise<SseEvent | undefined>;
  close(): void;
}

/** Opens an event stream that may never end (`GET /v1/changes`) and reads it as it arrives. */
export async function follow(api: Api, path: string): Promise<Following> {
  const controller = new AbortController();
  const headers = new Headers(api.defaultHeaders);
  if (api.bearer !== undefined) headers.set("authorization", `Bearer ${api.bearer}`);
  const response = await recordingFetch(`${api.url}${path}`, { headers, signal: controller.signal });
  const contentType = response.headers.get("content-type") ?? "";
  const events: SseEvent[] = [];
  if (contentType.includes("text/event-stream") && response.body) {
    const reader = response.body.pipeThrough(new TextDecoderStream()).getReader();
    void (async () => {
      let buffer = "";
      try {
        for (;;) {
          const { value, done } = await reader.read();
          if (done) return;
          buffer += value;
          const end = buffer.lastIndexOf("\n\n");
          if (end < 0) continue;
          events.push(...parseSse(buffer.slice(0, end + 2)));
          buffer = buffer.slice(end + 2);
        }
      } catch {
        // Closed.
      }
    })();
  } else {
    await response.body?.cancel();
  }
  return {
    status: response.status,
    contentType,
    events,
    async until(match, timeoutMs) {
      const deadline = Date.now() + timeoutMs;
      for (;;) {
        const found = events.find(match);
        if (found || Date.now() >= deadline) return found;
        await new Promise((resolve) => setTimeout(resolve, 50));
      }
    },
    close: () => controller.abort(),
  };
}

/** Milliseconds between two RFC 3339 times. */
export function between(from: string, to: string): number {
  return Date.parse(to) - Date.parse(from);
}
