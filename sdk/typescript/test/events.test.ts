import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { Rewind, RewindConnectionError, type StreamMessage } from "../src/index.js";
import { NAMESPACE, RUN_ID, collectGarbage, payload, run, until } from "./fixtures.js";
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

function event(idx: number) {
  return {
    event: "event",
    id: String(idx),
    data: { type: "event", idx, recordedAt: "2026-11-12T10:00:00Z", event: { type: "timer_fired", seq: idx } },
  };
}

const end = { event: "end", data: { type: "end", status: "completed" } };

async function collect(messages: AsyncIterable<StreamMessage>): Promise<StreamMessage[]> {
  const all: StreamMessage[] = [];
  for await (const message of messages) {
    all.push(message);
  }
  return all;
}

describe("the event stream", () => {
  it("reads the committed events, then the end", async () => {
    server.on("GET", "/v1/runs/{id}/events", sse([event(0), event(1), end]));
    const events = await collect(rewind.runs.events(RUN_ID));
    expect(events.map((e) => e.type)).toEqual(["event", "event", "end"]);
    const request = server.requests[0];
    expect(request?.query.get("from")).toBe("0");
    expect(request?.query.has("follow")).toBe(false);
    expect(request?.headers["accept"]).toBe("text/event-stream");
  });

  it("resumes a followed stream after the last event received", async () => {
    let connections = 0;
    server.on("GET", "/v1/runs/{id}/events", (request) => {
      connections += 1;
      return connections === 1 ? sse([event(0), event(1)], { end: false })(request) : sse([event(2), end])(request);
    });
    const events = await collect(rewind.runs.events(RUN_ID, { follow: true }));
    expect(events.map((e) => (e.type === "event" ? e.idx : e.type))).toEqual([0, 1, 2, "end"]);
    expect(server.requests.map((r) => r.query.get("from"))).toEqual(["0", "2"]);
    expect(server.requests.every((r) => r.query.get("follow") === "true")).toBe(true);
  });

  it("ends a followed stream the server holds open when its signal aborts, after a garbage collection", async () => {
    let closed = false;
    server.on(
      "GET",
      "/v1/runs/{id}/events",
      () => (response) =>
        new Promise<void>((resolve) => {
          response.writeHead(200, { "Content-Type": "text/event-stream", "Cache-Control": "no-cache" });
          response.write(`event: event\nid: 0\ndata: ${JSON.stringify(event(0).data)}\n\n`);
          response.on("close", () => {
            closed = true;
            resolve();
          });
        }),
    );
    const controller = new AbortController();
    const events = rewind.runs.events(RUN_ID, { follow: true, signal: controller.signal });
    expect((await events.next()).value).toMatchObject({ type: "event", idx: 0 });
    const next = events.next();
    collectGarbage();
    await new Promise((resolve) => setTimeout(resolve, 20));
    controller.abort(new Error("enough"));
    const outcome = await Promise.race([
      next.then(
        () => "resolved",
        (error: unknown) => (error instanceof Error ? error.message : String(error)),
      ),
      new Promise((resolve) => setTimeout(() => resolve("still reading"), 2_000)),
    ]);
    expect(outcome).toBe("enough");
    await until(() => closed);
  });

  it("reconnects to a server it cannot reach for a while, as to a dropped stream", async () => {
    // A server restarting: the stream drops, then connections are refused for a moment.
    let connections = 0;
    server.on("GET", "/v1/runs/{id}/events", (request) => {
      connections += 1;
      if (connections === 1) {
        return sse([event(0), event(1)], { end: false })(request);
      }
      if (connections <= 5) {
        return (response) => {
          response.destroy();
        };
      }
      return sse([event(2), end])(request);
    });
    const events = await collect(rewind.runs.events(RUN_ID, { follow: true }));
    expect(events.map((e) => (e.type === "event" ? e.idx : e.type))).toEqual([0, 1, 2, "end"]);
    expect(server.requests.at(-1)?.query.get("from")).toBe("2");
  });

  it("does not reconnect to a run that is gone", async () => {
    let connections = 0;
    server.on("GET", "/v1/runs/{id}/events", (request) => {
      connections += 1;
      return connections === 1 ? sse([event(0)], { end: false })(request) : problem(404, "not-found", "no such run");
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
    server.on("GET", "/v1/runs/{id}/events", sse([event(0)]));
    await expect(collect(rewind.runs.events(RUN_ID))).rejects.toThrow(/before its end event/);
  });

  it("reports an unknown run", async () => {
    server.on("GET", "/v1/runs/{id}/events", problem(404, "not-found", "no such run"));
    await expect(collect(rewind.runs.events(RUN_ID))).rejects.toMatchObject({ status: 404 });
  });

  it("closes the connection when the reader stops early", async () => {
    server.on("GET", "/v1/runs/{id}/events", sse([event(0), event(1), end]));
    for await (const message of rewind.runs.events(RUN_ID, { from: 4 })) {
      expect(message.type).toBe("event");
      break;
    }
    expect(server.requests[0]?.query.get("from")).toBe("4");
  });
});

describe("approvals", () => {
  it("lists the calls that wait for the harness, and reads their arguments", async () => {
    const pending = rewind.approvals.pending(runWaiting());
    expect(pending.map((p) => [p.callId, p.action, p.delivery, p.reason])).toEqual([
      ["c-approval", "github.delete_repo", 1, "approval"],
      ["c-schema", "big", 2, "schema_changed"],
    ]);

    server.on("GET", "/v1/payloads/{digest}", { status: 200, body: { big: 1 } });
    server.on("POST", "/v1/calls/{id}/approve", { status: 204 });
    server.on("POST", "/v1/calls/{id}/reject", { status: 204 });

    const [first, second] = pending;
    expect(await rewind.approvals.args(first!)).toEqual({ repo: "acme/api" });
    expect(await rewind.approvals.args(second!)).toEqual({ big: 1 });
    // Read through the run, as a key limited to labels must.
    expect(
      server.calls("GET", "/v1/payloads/{digest}".replace("{digest}", second!.payload.digest))[0]?.query.get("run"),
    ).toBe(second!.runId);
    await rewind.approvals.approve(first!);
    await rewind.approvals.reject(second!, "not now");
    expect(server.calls("POST", "/v1/calls/c-approval/approve")).toHaveLength(1);
    expect(server.calls("POST", "/v1/calls/c-schema/reject")[0]?.body).toEqual({ message: "not now" });
  });
});

function runWaiting() {
  return run({
    namespace: NAMESPACE,
    status: "active",
    historyLen: 5,
    waitingFor: [
      {
        kind: "call",
        seq: 4,
        action: "github.delete_repo",
        callId: "c-approval",
        state: "approval",
        delivery: 1,
        args: payload({ repo: "acme/api" }),
      },
      {
        kind: "call",
        seq: 5,
        action: "big",
        callId: "c-schema",
        state: "parked",
        parked: "schema_changed",
        delivery: 2,
        args: payload({ big: 1 }, false),
      },
      {
        kind: "call",
        seq: 6,
        action: "x",
        callId: "c-creds",
        state: "parked",
        parked: "credentials_refused",
        delivery: 1,
        args: payload({}),
      },
      {
        kind: "call",
        seq: 7,
        action: "y",
        callId: "c-ready",
        state: "ready",
        delivery: 1,
        args: payload({}),
      },
      { kind: "signal", name: "approval" },
    ],
  });
}
