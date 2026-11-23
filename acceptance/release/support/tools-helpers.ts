/**
 * Helpers of the specs of tools, egress and observability (`release/areas/host-tools.spec.ts`,
 * `mcp-egress.spec.ts`, `observability.spec.ts`): starting runs and waiting on their states
 * through the raw API, task streams opened by hand, and a broken MCP server.
 */
import { randomUUID } from "node:crypto";
import { createServer, type Server as HttpServer } from "node:http";
import type { AddressInfo } from "node:net";
import { eventually } from "../../support/processes.js";
import { type Api, expect, expectOk, type Reply, recordingFetch } from "./gate.js";

/** A run as `GET /v1/runs/{id}` answers it. */
export type RunState = Record<string, any>;

/** Starts a run (raw API), and returns its state as the start answered it. */
export async function startRun(api: Api, body: Record<string, unknown>, headers: Record<string, string> = {}): Promise<RunState> {
  const reply = await api.post("/v1/runs", body, { headers });
  expectOk(reply);
  return reply.json;
}

/** The run once it has ended (`wait` on `GET /v1/runs/{id}`), within `timeoutMs`. */
export async function ended(api: Api, id: string, timeoutMs = 60_000): Promise<RunState> {
  return eventually(
    `run ${id} ends`,
    async () => {
      const reply = await api.get(`/v1/runs/${id}`, { query: { wait: "10s" } });
      expectOk(reply);
      return ["completed", "failed", "cancelled", "aborted"].includes(reply.json.status) ? reply.json : undefined;
    },
    timeoutMs,
    100,
  );
}

/** The run once `check` holds of its state. */
export async function runUntil(api: Api, id: string, what: string, check: (run: RunState) => boolean, timeoutMs = 60_000): Promise<RunState> {
  return eventually(
    `run ${id}: ${what}`,
    async () => {
      const reply = await api.get(`/v1/runs/${id}`);
      expectOk(reply);
      return check(reply.json) ? reply.json : undefined;
    },
    timeoutMs,
    150,
  );
}

/** The run's wait on a tool call, once the call's task reaches `state`. */
export async function waitingCall(api: Api, id: string, state: string, timeoutMs = 60_000): Promise<RunState> {
  const run = await runUntil(
    api,
    id,
    `a tool call waits in state ${state}`,
    (r) => (r.waitingFor ?? []).some((w: RunState) => w.kind === "tool_call" && w.state === state),
    timeoutMs,
  );
  return (run.waitingFor as RunState[]).find((w) => w.kind === "tool_call" && w.state === state)!;
}

/** What a task stream asks for (`GET /v1/tasks/stream`). */
export interface TaskStreamQuery {
  toolset?: string;
  tools?: string[];
  namespace?: string;
  /** The tasks in flight on the stream at most: the server's 1 by default. */
  slots?: number;
  /** The worker's id, sent again by `reopen`; one is drawn when absent. */
  worker?: string;
}

/** Every stream `TaskStream.open` opened and nobody closed: `closeTaskStreams` closes them. */
const streams = new Set<TaskStream>();

/**
 * A task stream opened by hand, as a harness's worker holds one (`archi/04-tools.md`, "Tool task
 * lifecycle"): the tasks handed on it and the tokens it says are gone, kept as they come. The open
 * stream is the liveness of the tasks handed on it; `close` drops it, as a worker that dies does,
 * and `reopen` opens it again with the same worker id.
 */
export class TaskStream {
  /** Every task handed on the stream, in order: `HandedTask`s. */
  readonly handed: RunState[] = [];
  /** The tokens of the tasks the stream said are gone. */
  readonly gone: string[] = [];
  /** Called with each task as it is handed, once set; the tasks handed before are in `handed`. */
  onTask: ((task: RunState) => void) | undefined;
  /** Settles once the stream has ended: closed here, or dropped by the server. */
  readonly done: Promise<void>;
  private closed = false;

  private constructor(
    readonly api: Api,
    readonly query: TaskStreamQuery & { worker: string },
    private readonly abort: AbortController,
    body: ReadableStream<Uint8Array>,
  ) {
    this.done = this.read(body);
    streams.add(this);
  }

  /** Opens a stream: the stream once the server answered with one, or the server's refusal. */
  static async attempt(api: Api, query: TaskStreamQuery = {}): Promise<TaskStream | Reply> {
    const full = { ...query, worker: query.worker ?? `gate-${randomUUID()}` };
    const params = new URLSearchParams();
    if (full.toolset !== undefined) params.append("toolset", full.toolset);
    for (const tool of full.tools ?? []) params.append("tools", tool);
    if (full.namespace !== undefined) params.append("namespace", full.namespace);
    if (full.slots !== undefined) params.append("slots", String(full.slots));
    params.append("worker", full.worker);
    const headers = new Headers(api.defaultHeaders);
    if (api.bearer !== undefined) headers.set("authorization", `Bearer ${api.bearer}`);
    headers.set("accept", "text/event-stream");
    const abort = new AbortController();
    const response = await recordingFetch(`${api.url}/v1/tasks/stream?${params}`, { headers, signal: abort.signal });
    const type = response.headers.get("content-type") ?? "";
    if (response.status === 200 && type.includes("text/event-stream") && response.body !== null) {
      return new TaskStream(api, full, abort, response.body);
    }
    const text = await response.text();
    let json: any;
    try {
      json = text === "" ? undefined : JSON.parse(text);
    } catch {
      json = undefined;
    }
    const slug = type.includes("problem+json") && json?.type ? String(json.type).replace("urn:rewind:problem:", "") : undefined;
    return { status: response.status, headers: response.headers, text, json, slug };
  }

  /** Opens a stream, which the server must answer with one. */
  static async open(api: Api, query: TaskStreamQuery = {}): Promise<TaskStream> {
    const opened = await TaskStream.attempt(api, query);
    if (opened instanceof TaskStream) return opened;
    throw new Error(`GET /v1/tasks/stream → ${opened.status} ${opened.text.slice(0, 600)}`);
  }

  /** The same stream opened again, with the same worker id. */
  reopen(): Promise<TaskStream> {
    return TaskStream.open(this.api, this.query);
  }

  /** The first task handed on the stream that `check` accepts, within `timeoutMs`. */
  task(what: string, check: (task: RunState) => boolean = () => true, timeoutMs = 30_000): Promise<RunState> {
    return eventually(`${what} is handed on the stream`, () => this.handed.find(check), timeoutMs, 25);
  }

  /** The task of `tool` (of `runId`, when given) handed on the stream, within `timeoutMs`. */
  taskOf(tool: string, runId?: string, timeoutMs = 30_000): Promise<RunState> {
    return this.task(`a task of ${tool}${runId ? ` of run ${runId}` : ""}`, (t) => t.tool === tool && (runId === undefined || t.runId === runId), timeoutMs);
  }

  /** Resolves once the stream said the task of `token` is gone, within `timeoutMs`. */
  async goneOf(token: string, timeoutMs = 30_000): Promise<void> {
    await eventually(`the task ${token.slice(0, 16)}… is gone on the stream`, () => (this.gone.includes(token) ? true : undefined), timeoutMs, 25);
  }

  /** Drops the stream, as a worker that dies does. */
  async close(): Promise<void> {
    if (this.closed) return;
    this.closed = true;
    streams.delete(this);
    this.abort.abort();
    await this.done;
  }

  private async read(body: ReadableStream<Uint8Array>): Promise<void> {
    const decoder = new TextDecoder();
    let buffer = "";
    try {
      for await (const chunk of body as unknown as AsyncIterable<Uint8Array>) {
        buffer += decoder.decode(chunk, { stream: true }).replace(/\r\n?/g, "\n");
        let end: number;
        while ((end = buffer.indexOf("\n\n")) >= 0) {
          const block = buffer.slice(0, end);
          buffer = buffer.slice(end + 2);
          let event = "message";
          const data: string[] = [];
          for (const line of block.split("\n")) {
            // Comments (`:`, the keep-alive) are skipped.
            if (line.startsWith(":") || line === "") continue;
            const colon = line.indexOf(":");
            const field = colon < 0 ? line : line.slice(0, colon);
            const value = colon < 0 ? "" : line.slice(colon + 1).replace(/^ /, "");
            if (field === "event") event = value;
            else if (field === "data") data.push(value);
          }
          if (data.length === 0) continue;
          const parsed = JSON.parse(data.join("\n"));
          if (event === "task") {
            this.handed.push(parsed);
            this.onTask?.(parsed);
          } else if (event === "gone") {
            this.gone.push(String(parsed.token));
          }
        }
      }
    } catch {
      // Closed by `close`, or dropped by the server: what came is kept.
    }
    streams.delete(this);
  }
}

/** Closes every stream still open: a spec's `afterEach`. */
export async function closeTaskStreams(): Promise<void> {
  await Promise.all([...streams].map((stream) => stream.close()));
}

/**
 * Opens a stream for `tool` of `toolset` and waits for the task of `runId` (any, without it): the
 * task and the stream it was handed on, which stays open until it is closed.
 */
export async function handOne(api: Api, toolset: string, tool: string, runId?: string, timeoutMs = 30_000): Promise<{ task: RunState; stream: TaskStream }> {
  const stream = await TaskStream.open(api, { toolset, tools: [tool] });
  return { task: await stream.taskOf(tool, runId, timeoutMs), stream };
}

/** The tasks handed within `ms` on a stream opened for it, then closed. */
export async function handedWithin(api: Api, query: TaskStreamQuery, ms = 1_000): Promise<RunState[]> {
  const stream = await TaskStream.open(api, query);
  await new Promise((resolve) => setTimeout(resolve, ms));
  await stream.close();
  return stream.handed;
}

/** The journal's event types of a run, in order. */
export async function events(api: Api, id: string): Promise<string[]> {
  const reply = await api.get(`/v1/runs/${id}/journal`);
  expectOk(reply);
  return (reply.json.entries as { event: { type: string } }[]).map((e) => e.event.type);
}

/** The problem's detail, or the whole body. */
export function detail(reply: Reply): string {
  return String(reply.json?.detail ?? reply.text);
}

/** A host tool's declaration. */
export function hostTool(name: string, policy: Record<string, unknown> = {}, inputSchema?: Record<string, unknown>) {
  return {
    name,
    description: `The gate's host tool ${name}.`,
    inputSchema: inputSchema ?? { type: "object", properties: { id: { type: "string" } }, required: ["id"] },
    ...(Object.keys(policy).length > 0 ? { policy } : {}),
  };
}

/**
 * A server on 127.0.0.1 that answers every request with `status` and `body`: an MCP server
 * that is broken, whose answer an internal host must never quote to the tenant.
 */
export async function startBroken(status: number, body: string): Promise<{ url: string; hits: number; close(): Promise<void> }> {
  const state = { hits: 0 };
  const server: HttpServer = createServer((req, res) => {
    state.hits += 1;
    req.resume();
    req.on("end", () => res.writeHead(status, { "content-type": "text/plain" }).end(body));
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const port = (server.address() as AddressInfo).port;
  return {
    url: `http://127.0.0.1:${port}/mcp`,
    get hits() {
      return state.hits;
    },
    close: () => {
      server.closeAllConnections();
      return new Promise((resolve) => server.close(() => resolve()));
    },
  };
}

/** Asserts the run's journal holds `type` (one of the four event families' entries). */
export async function expectEvent(api: Api, id: string, type: string): Promise<void> {
  expect(await events(api, id)).toContain(type);
}
