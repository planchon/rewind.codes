/**
 * Helpers of the specs of tools, egress and observability (`release/areas/host-tools.spec.ts`,
 * `mcp-egress.spec.ts`, `observability.spec.ts`): starting runs and waiting on their states
 * through the raw API, claiming host-tool tasks by hand, and a broken MCP server.
 */
import { createServer, type Server as HttpServer } from "node:http";
import type { AddressInfo } from "node:net";
import { eventually } from "../../support/processes.js";
import { type Api, expect, expectOk, type Reply } from "./gate.js";

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

/** Claims host-tool tasks by hand: `GET /v1/tasks`, waiting up to `wait`. */
export async function claim(api: Api, toolset: string, tools: string[], wait = "10s"): Promise<RunState[]> {
  const reply = await api.get("/v1/tasks", { query: { toolset, tool: tools, wait, limit: 10 } });
  expectOk(reply);
  return reply.json.tasks as RunState[];
}

/** Claims until one task of `tool` (and of `runId`, when given) comes, within `timeoutMs`. */
export async function claimOne(api: Api, toolset: string, tool: string, runId?: string, timeoutMs = 30_000): Promise<RunState> {
  return eventually(
    `a task of ${tool} is claimed`,
    async () => {
      const tasks = await claim(api, toolset, [tool], "2s");
      return tasks.find((t) => runId === undefined || t.runId === runId);
    },
    timeoutMs,
    50,
  );
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
