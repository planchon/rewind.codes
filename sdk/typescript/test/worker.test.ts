import type { ServerResponse } from "node:http";

import { afterEach, beforeEach, describe, expect, it } from "vitest";

import {
  HostToolError,
  Rewind,
  RewindApiError,
  isProblem,
  type DeferredTask,
  type HandedTask,
  type HostToolContext,
  type HostToolHandler,
  type HostToolWorker,
  type WorkerErrorInfo,
  type WorkerTimings,
} from "../src/index.js";
import { NAMESPACE, RUN_ID, TOOLSET, collectGarbage, task, until } from "./fixtures.js";
import { MockServer, noContent, problem, type JsonReply, type MockReply, type MockRequest } from "./mock-server.js";

/** A task stream the server holds open: the test writes its events. */
interface Stream {
  request: MockRequest;
  response: ServerResponse;
  closed: boolean;
}

let server: MockServer;
let rewind: Rewind;
let streams: Stream[];
let worker: HostToolWorker | undefined;
let errors: Array<{ error: unknown; info: WorkerErrorInfo }>;

/** Holds each stream open until the client or the test closes it. */
function holdStream(request: MockRequest): MockReply {
  return (response) =>
    new Promise<void>((resolve) => {
      response.writeHead(200, { "Content-Type": "text/event-stream", "Cache-Control": "no-cache" });
      response.flushHeaders();
      const stream: Stream = { request, response, closed: false };
      streams.push(stream);
      const done = () => {
        stream.closed = true;
        resolve();
      };
      response.on("close", done);
      request.signal.addEventListener("abort", done);
    });
}

beforeEach(async () => {
  server = await MockServer.start();
  rewind = new Rewind({ url: server.url, apiKey: "rewind_sk_worker" });
  streams = [];
  errors = [];
  server
    .on("GET", "/v1/tasks/stream", holdStream)
    .on("POST", "/v1/tasks/{token}/complete", noContent)
    .on("POST", "/v1/tasks/{token}/fail", noContent)
    .on("POST", "/v1/tasks/{token}/defer", noContent);
});

afterEach(async () => {
  await worker?.stop().catch(() => undefined);
  worker = undefined;
  await server.close();
});

const FAST: WorkerTimings = { backoffMs: 20, maxBackoffMs: 100, minLifeMs: 50 };

function start(tools: Record<string, HostToolHandler>, concurrency?: number, timings: WorkerTimings = FAST) {
  worker = rewind.worker({
    toolset: TOOLSET,
    tools,
    concurrency,
    timings,
    onError: (error, info) => errors.push({ error, info }),
  });
  return worker;
}

/** The stream open now, once the worker has opened one. */
async function open(count = 1): Promise<Stream> {
  await until(() => streams.filter((s) => !s.closed).length >= 1 && streams.length >= count);
  const stream = streams.at(-1);
  if (stream === undefined) {
    throw new Error("no stream");
  }
  return stream;
}

function send(stream: Stream, event: string, data: unknown): void {
  stream.response.write(`event: ${event}\ndata: ${JSON.stringify(data)}\n\n`);
}

/** Hands tasks on the stream open now. */
async function hand(...tasks: HandedTask[]): Promise<Stream> {
  const stream = await open();
  for (const t of tasks) {
    send(stream, "task", t);
  }
  return stream;
}

const answers = (kind: "complete" | "fail" | "defer", token = "tok-1"): MockRequest[] =>
  server.calls("POST", `/v1/tasks/${token}/${kind}`);

const sleepMs = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

describe("the host-tool worker", () => {
  it("follows its tools' task stream, runs the handler and completes the task with its result", async () => {
    let context: HostToolContext | undefined;
    const w = start({
      ask_user: (args: { question: string }, ctx) => {
        context = ctx;
        return { answer: `yes to ${args.question}` };
      },
      other_tool: () => null,
    });
    const stream = await hand(task());
    await until(() => answers("complete").length === 1);
    expect(answers("complete")[0]?.body).toEqual({ result: { answer: "yes to Proceed?" } });

    const query = stream.request.query;
    expect(query.getAll("tools")).toEqual(["ask_user", "other_tool"]);
    expect(query.get("toolset")).toBe(TOOLSET);
    expect(query.get("slots")).toBe("10");
    expect(query.get("worker")).toBe(w.id);
    expect(w.id).toMatch(/^[0-9a-f-]{36}$/);
    expect(stream.request.headers["accept"]).toBe("text/event-stream");
    expect(stream.request.headers["authorization"]).toBe("Bearer rewind_sk_worker");

    expect(context).toMatchObject({
      runId: RUN_ID,
      namespace: NAMESPACE,
      seq: 3,
      attempt: 1,
      idempotencyKey: `${RUN_ID}/3`,
      labels: { conversation: "c-42" },
      traceparent: "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
    });
    expect(context?.signal.aborted).toBe(false);
    expect(streams).toHaveLength(1);
  });

  it("tells each handler the end user it acts for, whatever the namespace of the task", async () => {
    const seen: string[] = [];
    start({
      ask_user: (_args, ctx) => {
        seen.push(ctx.namespace);
        return ctx.namespace;
      },
    });
    await hand(task({ token: "tok-a", namespace: "alice" }), task({ id: "task-2", token: "tok-b", namespace: "bob" }));
    await until(() => answers("complete", "tok-a").length === 1 && answers("complete", "tok-b").length === 1);
    expect(seen.sort()).toEqual(["alice", "bob"]);
    expect(answers("complete", "tok-a")[0]?.body).toEqual({ result: "alice" });
    expect(answers("complete", "tok-b")[0]?.body).toEqual({ result: "bob" });
  });

  it("follows the stream with an end user's token, or for one namespace, and names tools without a toolset", async () => {
    const asAlice = new Rewind({ url: server.url, token: "rewind_nt_alice" });
    worker = asAlice.worker({ tools: { ask_user: () => null }, timings: FAST });
    const first = await open(1);
    await worker.stop();
    worker = rewind.namespace("bob").worker({ toolset: TOOLSET, tools: { ask_user: () => null }, timings: FAST });
    const second = await open(2);
    expect(first.request.headers["authorization"]).toBe("Bearer rewind_nt_alice");
    expect(first.request.headers["rewind-namespace"]).toBeUndefined();
    expect(first.request.query.get("toolset")).toBeNull();
    expect(first.request.query.getAll("tools")).toEqual(["ask_user"]);
    expect(second.request.headers["authorization"]).toBe("Bearer rewind_sk_worker");
    expect(second.request.headers["rewind-namespace"]).toBe("bob");
  });

  it("completes with null when the handler returns nothing", async () => {
    start({ ask_user: () => undefined });
    await hand(task());
    await until(() => answers("complete").length === 1);
    expect(answers("complete")[0]?.body).toEqual({ result: null });
  });

  it("sends nothing while a handler runs: the open stream is its liveness", async () => {
    start({ ask_user: () => new Promise((resolve) => setTimeout(() => resolve("done"), 300)) });
    await hand(task());
    await until(() => answers("complete").length === 1);
    expect(server.requests.map((r) => `${r.method} ${r.path}`)).toEqual([
      "GET /v1/tasks/stream",
      "POST /v1/tasks/tok-1/complete",
    ]);
  });

  it("fails the task with what the handler threw", async () => {
    start({
      ask_user: () => {
        throw new HostToolError("the user is away", { data: { retryIn: "1h" }, retryable: true });
      },
      crash: () => {
        throw new TypeError("x is undefined");
      },
    });
    await hand(task(), task({ token: "tok-2", tool: "crash", id: "task-2" }));
    await until(() => answers("fail").length === 1 && answers("fail", "tok-2").length === 1);
    expect(answers("fail")[0]?.body).toEqual({ message: "the user is away", data: { retryIn: "1h" }, retryable: true });
    expect(answers("fail", "tok-2")[0]?.body).toEqual({ message: "TypeError: x is undefined" });
    // A deliberate HostToolError is the tool's answer; an unexpected exception is reported too.
    expect(errors.map((e) => e.info.phase)).toEqual(["handler"]);
  });

  it("fails a task it has no handler for, and a result JSON cannot carry", async () => {
    start({ ask_user: () => 10n });
    await hand(task(), task({ token: "tok-2", tool: "unknown", id: "task-2" }));
    await until(() => answers("fail").length === 1 && answers("fail", "tok-2").length === 1);
    expect(answers("fail")[0]?.body).toMatchObject({ message: expect.stringMatching(/is not JSON/) });
    expect(answers("fail", "tok-2")[0]?.body).toEqual({ message: "this worker has no handler for the tool unknown" });
  });

  it("aborts the handler's signal and drops the result when the stream says the task is gone", async () => {
    const signals = new Map<string, AbortSignal>();
    start({
      ask_user: async (_args, ctx) => {
        signals.set(ctx.task.token, ctx.signal);
        await new Promise((resolve) => ctx.signal.addEventListener("abort", resolve));
        return "too late";
      },
    });
    const stream = await hand(task(), task({ token: "tok-2", id: "task-2" }));
    await until(() => signals.size === 2);
    send(stream, "gone", { token: "tok-1" });
    send(stream, "gone", { token: "tok-unknown" });
    const signal = signals.get("tok-1");
    await until(() => signal?.aborted === true);
    await until(() => worker?.running === 1);
    expect(signals.get("tok-2")?.aborted).toBe(false);
    expect(answers("complete")).toHaveLength(0);
    expect(answers("fail")).toHaveLength(0);
    expect(signal?.reason).toBeInstanceOf(RewindApiError);
    expect(isProblem(signal?.reason, "task-gone")).toBe(true);
    expect(errors).toEqual([]);
    // The worker stops once the last handler gives up too.
    send(stream, "gone", { token: "tok-2" });
    await until(() => worker?.running === 0);
  });

  it("defers the answer, which whoever holds the token sends later", async () => {
    let deferred: DeferredTask | undefined;
    start({
      ask_user: async (_args, ctx) => {
        deferred = await ctx.defer({ timeout: "3d" });
        return "ignored";
      },
    });
    await hand(task());
    await until(() => deferred !== undefined && worker?.running === 0);
    expect(answers("defer")[0]?.body).toEqual({ timeout: "3d" });
    expect(answers("complete")).toHaveLength(0);

    expect(deferred?.token).toBe("tok-1");
    await deferred?.complete({ answer: "yes" });
    expect(answers("complete")[0]?.body).toEqual({ result: { answer: "yes" } });
    await rewind.tasks.fail("tok-1", { message: "later failure" });
    expect(answers("fail")[0]?.body).toEqual({ message: "later failure" });
  });

  it("fails a deferred task when the handler throws after deferring", async () => {
    start({
      ask_user: async (_args, ctx) => {
        await ctx.defer();
        throw new HostToolError("could not hand the question over");
      },
    });
    await hand(task());
    await until(() => answers("fail").length === 1);
    expect(answers("defer")[0]?.rawBody).toBe("{}");
    expect(answers("fail")[0]?.body).toEqual({ message: "could not hand the question over" });
  });

  it("answers again after a server error, and drops an answer to a task that is gone", async () => {
    let completes = 0;
    server.on("POST", "/v1/tasks/{token}/complete", (request): JsonReply => {
      if (request.params["token"] === "tok-gone") {
        return problem(410, "task-gone", "the run has ended");
      }
      completes += 1;
      return completes === 1 ? problem(503, "unavailable", "stopping") : noContent;
    });
    start({ ask_user: () => "ok" });
    await hand(task(), task({ token: "tok-gone", id: "task-2" }));
    await until(() => completes === 2 && answers("complete", "tok-gone").length === 1, 5_000);
    await until(() => worker?.running === 0);
    expect(errors).toEqual([]);
  });

  it("fails the task when its result is too large for the server", async () => {
    server.on("POST", "/v1/tasks/{token}/complete", problem(413, "payload-too-large", "the result is above 5 MiB"));
    start({ ask_user: () => "x".repeat(100) });
    await hand(task());
    await until(() => answers("fail").length === 1);
    expect(answers("fail")[0]?.body).toEqual({
      message: "the result of ask_user is too large: the result is above 5 MiB",
    });
  });

  it("cuts a failure too large for the server", async () => {
    let fails = 0;
    server.on("POST", "/v1/tasks/{token}/fail", (): JsonReply => {
      fails += 1;
      return fails === 1 ? problem(413, "payload-too-large", "the failure is above 64 KiB") : noContent;
    });
    start({
      ask_user: () => {
        throw new HostToolError("m".repeat(70_000), { data: { trace: "d".repeat(70_000) } });
      },
    });
    await hand(task());
    await until(() => answers("fail").length === 2);
    expect(answers("fail")[1]?.body).toEqual({ message: `${"m".repeat(2_000)}…` });
  });

  it("asks for as many slots as its concurrency, and runs the tasks it is handed", async () => {
    const release: Array<() => void> = [];
    start(
      {
        ask_user: () => new Promise<string>((resolve) => release.push(() => resolve("ok"))),
      },
      2,
    );
    const stream = await hand(task({ token: "tok-1" }), task({ token: "tok-2", id: "task-2" }));
    expect(stream.request.query.get("slots")).toBe("2");
    await until(() => release.length === 2);
    expect(worker?.running).toBe(2);
    release.splice(0).forEach((done) => done());
    await until(() => answers("complete", "tok-1").length === 1 && answers("complete", "tok-2").length === 1);
    // The same task handed twice runs once.
    send(stream, "task", task({ token: "tok-3", id: "task-3" }));
    send(stream, "task", task({ token: "tok-3", id: "task-3" }));
    await until(() => release.length >= 1);
    await sleepMs(30);
    expect(release).toHaveLength(1);
    release.splice(0).forEach((done) => done());
    await until(() => answers("complete", "tok-3").length === 1);
  });

  it("opens the stream again after a drop, with the same id, while its handlers go on", async () => {
    let finish: (() => void) | undefined;
    const w = start({ ask_user: () => new Promise((resolve) => (finish = () => resolve("finished"))) });
    const first = await hand(task());
    await until(() => finish !== undefined);
    first.response.destroy();
    const second = await open(2);
    expect(second.request.query.get("worker")).toBe(w.id);
    expect(second.request.query.get("slots")).toBe("10");
    expect(w.running).toBe(1);
    finish?.();
    await until(() => answers("complete").length === 1);
    expect(answers("complete")[0]?.body).toEqual({ result: "finished" });
    expect(errors.map((e) => e.info.phase)).toEqual(["stream"]);
    // The new stream hands tasks as the first did.
    send(second, "task", task({ token: "tok-2", id: "task-2" }));
    await until(() => finish !== undefined && w.running === 1);
    finish?.();
    await until(() => answers("complete", "tok-2").length === 1);
  });

  it("waits longer after each drop that comes at once, and starts over once a stream lived", async () => {
    let refusals = 3;
    const opened: number[] = [];
    server.on("GET", "/v1/tasks/stream", (request): MockReply | JsonReply => {
      opened.push(Date.now());
      if (refusals > 0) {
        refusals -= 1;
        return problem(500, "internal", "the store is down");
      }
      return holdStream(request);
    });
    start({ ask_user: () => "ok" }, undefined, { backoffMs: 40, maxBackoffMs: 1_000, minLifeMs: 50 });
    await until(() => opened.length === 4 && streams.length === 1);
    const waits = opened.slice(1).map((at, i) => at - (opened[i] ?? at));
    // 40 ms, 80 ms, 160 ms, each spread down to half.
    expect(waits[0]).toBeGreaterThanOrEqual(18);
    expect(waits[2]).toBeGreaterThanOrEqual(78);
    expect(errors.map((e) => (e.error as RewindApiError).status)).toEqual([500, 500, 500]);

    // A stream that lived, closed by the server: opened again at once.
    await sleepMs(80);
    streams[0]?.response.end();
    await open(2);
    expect((opened.at(-1) ?? 0) - (opened.at(-2) ?? 0)).toBeLessThan(200);
    expect(errors).toHaveLength(3);
  });

  it("takes a silent stream as broken, and keeps one that writes comments", async () => {
    start({ ask_user: () => "ok" }, undefined, { ...FAST, silenceMs: 150 });
    const first = await open(1);
    for (let i = 0; i < 4; i++) {
      await sleepMs(60);
      first.response.write(":\n\n");
    }
    expect(streams).toHaveLength(1);
    await open(2);
    expect(first.closed).toBe(true);
    expect(errors.map((e) => (e.error as Error).message)).toEqual(["the task stream went silent"]);
  });

  it("stops: no task is handed any more, and the handlers in flight answer on a stream with no slot", async () => {
    let finish: (() => void) | undefined;
    const w = start({
      ask_user: () =>
        new Promise((resolve) => {
          finish = () => resolve("finished");
        }),
    });
    const first = await hand(task());
    await until(() => finish !== undefined);
    const stopped = w.stop();
    let done = false;
    void stopped.then(() => {
      done = true;
    });
    const draining = await open(2);
    expect(first.closed).toBe(true);
    expect(draining.request.query.get("slots")).toBe("0");
    expect(draining.request.query.get("worker")).toBe(w.id);
    await sleepMs(50);
    expect(done).toBe(false);
    finish?.();
    await stopped;
    expect(answers("complete")[0]?.body).toEqual({ result: "finished" });
    await until(() => draining.closed);
    expect(streams).toHaveLength(2);
  });

  it("stops at once without handlers in flight, and when its signal aborts", async () => {
    const w = start({ ask_user: () => "ok" });
    const stream = await open(1);
    await w.stop();
    await until(() => stream.closed);
    expect(streams).toHaveLength(1);

    const controller = new AbortController();
    worker = rewind.worker({ tools: { ask_user: () => "ok" }, signal: controller.signal, onError: () => undefined });
    controller.abort();
    await worker.done;
  });

  it("stops while the server holds its stream open, after a garbage collection", async () => {
    const w = start({ ask_user: () => "ok" });
    const stream = await open(1);
    // Node's fetch follows the caller's signal through its own request, held weakly: once it is collected, only the
    // worker's own hold on the stream can end it.
    collectGarbage();
    await sleepMs(20);
    const stopped = await Promise.race([w.stop().then(() => true), sleepMs(2_000).then(() => false)]);
    expect(stopped).toBe(true);
    await until(() => stream.closed);
  });

  it("stops on a refused key, and opens the stream again after other failures", async () => {
    let opens = 0;
    server.on("GET", "/v1/tasks/stream", () => {
      opens += 1;
      return opens === 1 ? problem(500, "internal", "the store is down") : problem(401, "unauthorized", "unknown key");
    });
    const w = start({ ask_user: () => "ok" });
    await expect(w.done).rejects.toMatchObject({ status: 401 });
    expect(errors.map((e) => [e.info.phase, (e.error as RewindApiError).status])).toEqual([
      ["stream", 500],
      ["stream", 401],
    ]);
  });

  it("refuses a worker without handlers or with a bad concurrency", () => {
    expect(() => rewind.worker({ tools: {} })).toThrow(TypeError);
    expect(() => rewind.worker({ tools: { a: () => 1 }, concurrency: 0 })).toThrow(RangeError);
    expect(() => rewind.worker({ tools: { a: () => 1 }, concurrency: 101 })).toThrow(RangeError);
  });

  it("reads the task stream itself: tasks and gone, without comments or unknown events", async () => {
    server.on("GET", "/v1/tasks/stream", () => (response: ServerResponse) => {
      response.writeHead(200, { "Content-Type": "text/event-stream" });
      response.write(":\n\n");
      response.write(`event: task\ndata: ${JSON.stringify(task())}\n\n`);
      response.write("event: later\ndata: {}\n\n");
      response.write(`event: gone\ndata: ${JSON.stringify({ token: "tok-1" })}\n\n`);
      response.end();
    });
    const events = [];
    for await (const event of rewind.tasks.stream({ tools: ["ask_user"], slots: 3 })) {
      events.push(event);
    }
    expect(events).toEqual([
      { type: "task", task: task() },
      { type: "gone", token: "tok-1" },
    ]);
    expect(server.calls("GET", "/v1/tasks/stream")[0]?.query.get("slots")).toBe("3");
  });
});
