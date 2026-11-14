import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { Rewind, RewindConnectionError, type StreamEvent } from "../src/index.js";
import { NAMESPACE, RUN_ID, payload } from "./fixtures.js";
import { MockServer, problem, sse } from "./mock-server.js";

let server: MockServer;
let rewind: Rewind;

beforeEach(async () => {
  server = await MockServer.start();
  rewind = new Rewind({ url: server.url, apiKey: "rewind_sk_test" });
});

afterEach(async () => {
  await server.close();
});

function entry(idx: number) {
  return {
    event: "entry",
    id: String(idx),
    data: { type: "entry", idx, recordedAt: "2026-11-12T10:00:00Z", event: { type: "timer_fired", seq: idx } },
  };
}

const end = { event: "end", data: { type: "end", status: "completed" } };

async function collect(events: AsyncIterable<StreamEvent>): Promise<StreamEvent[]> {
  const all: StreamEvent[] = [];
  for await (const event of events) {
    all.push(event);
  }
  return all;
}

describe("the event stream", () => {
  it("reads the committed entries, then the end", async () => {
    server.on("GET", "/v1/runs/{id}/events", sse([entry(0), entry(1), end]));
    const events = await collect(rewind.runs.events(RUN_ID));
    expect(events.map((e) => e.type)).toEqual(["entry", "entry", "end"]);
    const request = server.requests[0];
    expect(request?.query.get("from")).toBe("0");
    expect(request?.query.has("follow")).toBe(false);
    expect(request?.headers["accept"]).toBe("text/event-stream");
  });

  it("resumes a followed stream after the last entry received", async () => {
    let connections = 0;
    server.on("GET", "/v1/runs/{id}/events", (request) => {
      connections += 1;
      return connections === 1 ? sse([entry(0), entry(1)], { end: false })(request) : sse([entry(2), end])(request);
    });
    const events = await collect(rewind.runs.events(RUN_ID, { follow: true }));
    expect(events.map((e) => (e.type === "entry" ? e.idx : e.type))).toEqual([0, 1, 2, "end"]);
    expect(server.requests.map((r) => r.query.get("from"))).toEqual(["0", "2"]);
    expect(server.requests.every((r) => r.query.get("follow") === "true")).toBe(true);
  });

  it("reconnects to a server it cannot reach for a while, as to a dropped stream", async () => {
    // A server restarting: the stream drops, then connections are refused for a moment.
    let connections = 0;
    server.on("GET", "/v1/runs/{id}/events", (request) => {
      connections += 1;
      if (connections === 1) {
        return sse([entry(0), entry(1)], { end: false })(request);
      }
      if (connections <= 5) {
        return (response) => {
          response.destroy();
        };
      }
      return sse([entry(2), end])(request);
    });
    const events = await collect(rewind.runs.events(RUN_ID, { follow: true }));
    expect(events.map((e) => (e.type === "entry" ? e.idx : e.type))).toEqual([0, 1, 2, "end"]);
    expect(server.requests.at(-1)?.query.get("from")).toBe("2");
  });

  it("does not reconnect to a run that is gone", async () => {
    let connections = 0;
    server.on("GET", "/v1/runs/{id}/events", (request) => {
      connections += 1;
      return connections === 1 ? sse([entry(0)], { end: false })(request) : problem(404, "not-found", "no such run");
    });
    await expect(collect(rewind.runs.events(RUN_ID, { follow: true }))).rejects.toMatchObject({ status: 404 });
    expect(connections).toBe(2);
  });

  it("gives up after the reconnections allowed", async () => {
    server.on("GET", "/v1/runs/{id}/events", sse([], { end: false }));
    await expect(collect(rewind.runs.events(RUN_ID, { follow: true, maxReconnects: 1 }))).rejects.toBeInstanceOf(
      RewindConnectionError,
    );
    expect(server.requests).toHaveLength(2);
  });

  it("fails a stream that is not followed and ends early", async () => {
    server.on("GET", "/v1/runs/{id}/events", sse([entry(0)]));
    await expect(collect(rewind.runs.events(RUN_ID))).rejects.toThrow(/before its end event/);
  });

  it("reports an unknown run", async () => {
    server.on("GET", "/v1/runs/{id}/events", problem(404, "not-found", "no such run"));
    await expect(collect(rewind.runs.events(RUN_ID))).rejects.toMatchObject({ status: 404 });
  });

  it("closes the connection when the reader stops early", async () => {
    server.on("GET", "/v1/runs/{id}/events", sse([entry(0), entry(1), end]));
    for await (const event of rewind.runs.events(RUN_ID, { from: 4 })) {
      expect(event.type).toBe("entry");
      break;
    }
    expect(server.requests[0]?.query.get("from")).toBe("4");
  });
});

describe("approvals", () => {
  it("lists the calls that wait for the harness, and reads their arguments", async () => {
    const waiting = {
      ...runWaiting(),
    };
    const pending = rewind.approvals.pending(waiting);
    expect(pending.map((p) => [p.taskId, p.reason])).toEqual([
      ["t-approval", "approval"],
      ["t-schema", "schema_changed"],
    ]);

    server.on("GET", "/v1/payloads/{digest}", { status: 200, body: { big: 1 } });
    server.on("POST", "/v1/tasks/{id}/approve", { status: 204 });
    server.on("POST", "/v1/tasks/{id}/reject", { status: 204 });

    const [first, second] = pending;
    expect(await rewind.approvals.args(first!)).toEqual({ repo: "acme/api" });
    expect(await rewind.approvals.args(second!)).toEqual({ big: 1 });
    // Read through the run, as a key limited to labels must.
    expect(
      server.calls("GET", "/v1/payloads/{digest}".replace("{digest}", second!.payload.digest))[0]?.query.get("run"),
    ).toBe(second!.runId);
    await rewind.approvals.approve(first!);
    await rewind.approvals.reject(second!, "not now");
    expect(server.calls("POST", "/v1/tasks/t-approval/approve")).toHaveLength(1);
    expect(server.calls("POST", "/v1/tasks/t-schema/reject")[0]?.body).toEqual({ message: "not now" });
  });
});

function runWaiting() {
  return {
    id: RUN_ID,
    namespace: NAMESPACE,
    status: "active" as const,
    createdAt: "t",
    updatedAt: "t",
    expiresAt: "t",
    epoch: 1,
    historyLen: 5,
    input: payload({}),
    labels: {},
    mode: "live" as const,
    script: "s",
    toolset: "ts",
    traceId: "x",
    waitingFor: [
      {
        kind: "tool_call" as const,
        seq: 4,
        tool: "github.delete_repo",
        taskId: "t-approval",
        state: "approval" as const,
        attempt: 1,
        args: payload({ repo: "acme/api" }),
      },
      {
        kind: "tool_call" as const,
        seq: 5,
        tool: "big",
        taskId: "t-schema",
        state: "parked" as const,
        parked: "schema_changed" as const,
        attempt: 1,
        args: payload({ big: 1 }, false),
      },
      {
        kind: "tool_call" as const,
        seq: 6,
        tool: "x",
        taskId: "t-creds",
        state: "parked" as const,
        parked: "credentials_expired" as const,
        attempt: 1,
        args: payload({}),
      },
      {
        kind: "tool_call" as const,
        seq: 7,
        tool: "y",
        taskId: "t-ready",
        state: "ready" as const,
        attempt: 1,
        args: payload({}),
      },
      { kind: "signal" as const, name: "approval" },
    ],
  };
}
