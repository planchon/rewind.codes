/**
 * Helpers of the specs of actions, egress and observability (`release/areas/harness-actions.spec.ts`,
 * `mcp-egress.spec.ts`, `observability.spec.ts`): starting runs and waiting on their states
 * through the raw API, call streams opened by hand, and a broken MCP server.
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

/** The run's wait on a call, once the call reaches `state`. */
export async function waitingCall(api: Api, id: string, state: string, timeoutMs = 60_000): Promise<RunState> {
  const run = await runUntil(
    api,
    id,
    `a call waits in state ${state}`,
    (r) => (r.waitingFor ?? []).some((w: RunState) => w.kind === "call" && w.state === state),
    timeoutMs,
  );
  return (run.waitingFor as RunState[]).find((w) => w.kind === "call" && w.state === state)!;
}

/** What a call stream asks for (`GET /v1/calls/stream`). */
export interface CallStreamQuery {
  actionSet?: string;
  actions?: string[];
  namespace?: string;
  /** The calls in flight on the stream at most: the server's 1 by default. */
  slots?: number;
  /** The worker's id, sent again by `reopen`; one is drawn when absent. */
  worker?: string;
}

/** Every stream `CallStream.open` opened and nobody closed: `closeCallStreams` closes them. */
const streams = new Set<CallStream>();

/**
 * A call stream opened by hand, as a harness's worker holds one (`archi/04-tools.md`, "Call
 * lifecycle"): the calls handed on it and the tokens it says are gone, kept as they come. The open
 * stream is the liveness of the calls handed on it; `close` drops it, as a worker that dies does,
 * and `reopen` opens it again with the same worker id.
 */
export class CallStream {
  /** Every call handed on the stream, in order: `HandedCall`s. */
  readonly handed: RunState[] = [];
  /** The tokens of the calls the stream said are gone. */
  readonly gone: string[] = [];
  /** Called with each call as it is handed, once set; the calls handed before are in `handed`. */
  onCall: ((call: RunState) => void) | undefined;
  /** Settles once the stream has ended: closed here, or dropped by the server. */
  readonly done: Promise<void>;
  private closed = false;
  /** The reader of the stream's body, cancelled by `close`. */
  private reader: ReadableStreamDefaultReader<Uint8Array> | undefined;

  private constructor(
    readonly api: Api,
    readonly query: CallStreamQuery & { worker: string },
    private readonly abort: AbortController,
    body: ReadableStream<Uint8Array>,
  ) {
    this.done = this.read(body);
    streams.add(this);
  }

  /** Opens a stream: the stream once the server answered with one, or the server's refusal. */
  static async tryOpen(api: Api, query: CallStreamQuery = {}): Promise<CallStream | Reply> {
    const full = { ...query, worker: query.worker ?? `gate-${randomUUID()}` };
    const params = new URLSearchParams();
    if (full.actionSet !== undefined) params.append("actionSet", full.actionSet);
    for (const action of full.actions ?? []) params.append("actions", action);
    if (full.namespace !== undefined) params.append("namespace", full.namespace);
    if (full.slots !== undefined) params.append("slots", String(full.slots));
    params.append("worker", full.worker);
    const headers = new Headers(api.defaultHeaders);
    if (api.bearer !== undefined) headers.set("authorization", `Bearer ${api.bearer}`);
    headers.set("accept", "text/event-stream");
    const abort = new AbortController();
    const response = await recordingFetch(`${api.url}/v1/calls/stream?${params}`, { headers, signal: abort.signal });
    const type = response.headers.get("content-type") ?? "";
    if (response.status === 200 && type.includes("text/event-stream") && response.body !== null) {
      return new CallStream(api, full, abort, response.body);
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
  static async open(api: Api, query: CallStreamQuery = {}): Promise<CallStream> {
    const opened = await CallStream.tryOpen(api, query);
    if (opened instanceof CallStream) return opened;
    throw new Error(`GET /v1/calls/stream → ${opened.status} ${opened.text.slice(0, 600)}`);
  }

  /** The same stream opened again, with the same worker id. */
  reopen(): Promise<CallStream> {
    return CallStream.open(this.api, this.query);
  }

  /** The first call handed on the stream that `check` accepts, within `timeoutMs`. */
  call(what: string, check: (call: RunState) => boolean = () => true, timeoutMs = 30_000): Promise<RunState> {
    return eventually(`${what} is handed on the stream`, () => this.handed.find(check), timeoutMs, 25);
  }

  /** The call of `action` (of `runId`, when given) handed on the stream, within `timeoutMs`. */
  callOf(action: string, runId?: string, timeoutMs = 30_000): Promise<RunState> {
    return this.call(`a call of ${action}${runId ? ` of run ${runId}` : ""}`, (c) => c.action === action && (runId === undefined || c.runId === runId), timeoutMs);
  }

  /** Resolves once the stream said the call of `token` is gone, within `timeoutMs`. */
  async goneOf(token: string, timeoutMs = 30_000): Promise<void> {
    await eventually(`the call ${token.slice(0, 16)}… is gone on the stream`, () => (this.gone.includes(token) ? true : undefined), timeoutMs, 25);
  }

  /** Drops the stream, as a worker that dies does. */
  async close(): Promise<void> {
    if (this.closed) return;
    this.closed = true;
    streams.delete(this);
    this.abort.abort();
    // The abort alone does not always end a read under way: cancelling the reader does.
    await this.reader?.cancel().catch(() => undefined);
    await this.done;
  }

  private async read(body: ReadableStream<Uint8Array>): Promise<void> {
    const decoder = new TextDecoder();
    let buffer = "";
    const reader = body.getReader();
    this.reader = reader;
    try {
      for (;;) {
        const { done, value: chunk } = await reader.read();
        if (done) break;
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
          if (event === "call") {
            this.handed.push(parsed);
            this.onCall?.(parsed);
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
export async function closeCallStreams(): Promise<void> {
  await Promise.all([...streams].map((stream) => stream.close()));
}

/**
 * Opens a stream for `action` of `actionSet` and waits for the call of `runId` (any, without it): the
 * call and the stream it was handed on, which stays open until it is closed.
 */
export async function handOne(api: Api, actionSet: string, action: string, runId?: string, timeoutMs = 30_000): Promise<{ call: RunState; stream: CallStream }> {
  const stream = await CallStream.open(api, { actionSet, actions: [action] });
  return { call: await stream.callOf(action, runId, timeoutMs), stream };
}

/** The calls handed within `ms` on a stream opened for it, then closed. */
export async function handedWithin(api: Api, query: CallStreamQuery, ms = 1_000): Promise<RunState[]> {
  const stream = await CallStream.open(api, query);
  await new Promise((resolve) => setTimeout(resolve, ms));
  await stream.close();
  return stream.handed;
}

/** The history's event types of a run, in order. */
export async function events(api: Api, id: string): Promise<string[]> {
  const reply = await api.get(`/v1/runs/${id}/export`);
  expectOk(reply);
  return (reply.json.events as { event: { type: string } }[]).map((e) => e.event.type);
}

/** The problem's detail, or the whole body. */
export function detail(reply: Reply): string {
  return String(reply.json?.detail ?? reply.text);
}

/** A harness action's declaration. */
export function harnessAction(name: string, policy: Record<string, unknown> = {}, inputSchema?: Record<string, unknown>) {
  return {
    name,
    description: `The gate's harness action ${name}.`,
    inputSchema: inputSchema ?? { type: "object", properties: { id: { type: "string" } }, required: ["id"] },
    ...(Object.keys(policy).length > 0 ? { policy } : {}),
  };
}

/**
 * A server on 127.0.0.1 that answers every request with `status` and `body`: an MCP server
 * that is broken, whose answer an internal server must never quote to the organization.
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

/** Asserts the run's history holds `type` (one of the four event families' events). */
export async function expectEvent(api: Api, id: string, type: string): Promise<void> {
  expect(await events(api, id)).toContain(type);
}
