import { afterEach, beforeEach, describe, expect, it } from "vitest";

import {
  HostToolError,
  Ptc,
  PtcApiError,
  type DeferredTask,
  type HostToolContext,
  type HostToolHandler,
  type HostToolWorker,
  type Task,
  type WorkerErrorInfo,
} from "../src/index.js";
import { RUN_ID, TOOLSET, task, until } from "./fixtures.js";
import { MockServer, json, noContent, problem, type JsonReply, type MockRequest } from "./mock-server.js";

let server: MockServer;
let ptc: Ptc;
let queue: Task[];
let worker: HostToolWorker | undefined;
let errors: Array<{ error: unknown; info: WorkerErrorInfo }>;

beforeEach(async () => {
  server = await MockServer.start();
  ptc = new Ptc({ url: server.url, apiKey: "ptc_sk_worker" });
  queue = [];
  errors = [];
  server
    .on("GET", "/v1/tasks", async (request) => {
      const limit = Number(request.query.get("limit") ?? "1");
      const tasks = queue.splice(0, limit);
      if (tasks.length === 0) {
        await new Promise((resolve) => setTimeout(resolve, 10));
      }
      return json({ tasks });
    })
    .on("POST", "/v1/tasks/{token}/heartbeat", json({ heartbeatTimeoutMs: 60_000 }))
    .on("POST", "/v1/tasks/{token}/complete", noContent)
    .on("POST", "/v1/tasks/{token}/fail", noContent)
    .on("POST", "/v1/tasks/{token}/defer", noContent);
});

afterEach(async () => {
  await worker?.stop().catch(() => undefined);
  worker = undefined;
  await server.close();
});

function start(tools: Record<string, HostToolHandler>, concurrency?: number) {
  worker = ptc.worker({
    toolset: TOOLSET,
    tools,
    concurrency,
    pollWait: "1s",
    onError: (error, info) => errors.push({ error, info }),
  });
  return worker;
}

const answers = (kind: "complete" | "fail" | "defer" | "heartbeat", token = "tok-1"): MockRequest[] =>
  server.calls("POST", `/v1/tasks/${token}/${kind}`);

describe("the host-tool worker", () => {
  it("claims its tools, runs the handler and completes the task with its result", async () => {
    let context: HostToolContext | undefined;
    start({
      ask_user: (args: { question: string }, ctx) => {
        context = ctx;
        return { answer: `yes to ${args.question}` };
      },
      other_tool: () => null,
    });
    queue.push(task());
    await until(() => answers("complete").length === 1);
    expect(answers("complete")[0]?.body).toEqual({ result: { answer: "yes to Proceed?" } });

    const claim = server.calls("GET", "/v1/tasks")[0];
    expect(claim?.query.getAll("tool")).toEqual(["ask_user", "other_tool"]);
    expect(claim?.query.get("toolset")).toBe(TOOLSET);
    expect(claim?.query.get("limit")).toBe("10");
    expect(claim?.query.get("wait")).toBe("1s");
    expect(claim?.headers["authorization"]).toBe("Bearer ptc_sk_worker");

    expect(context).toMatchObject({
      runId: RUN_ID,
      seq: 3,
      attempt: 1,
      idempotencyKey: `${RUN_ID}/3`,
      labels: { conversation: "c-42" },
      traceparent: "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
    });
    expect(context?.signal.aborted).toBe(false);
  });

  it("completes with null when the handler returns nothing", async () => {
    start({ ask_user: () => undefined });
    queue.push(task());
    await until(() => answers("complete").length === 1);
    expect(answers("complete")[0]?.body).toEqual({ result: null });
  });

  it("keeps the claim alive with heartbeats while the handler runs, and only then", async () => {
    // A heartbeat every third of the timeout: every 100 ms.
    start({ ask_user: () => new Promise((resolve) => setTimeout(() => resolve("done"), 450)) });
    server.on("POST", "/v1/tasks/{token}/heartbeat", json({ heartbeatTimeoutMs: 300 }));
    queue.push(task({ heartbeatTimeoutMs: 300 }));
    await until(() => answers("complete").length === 1);
    const beats = answers("heartbeat").length;
    expect(beats).toBeGreaterThanOrEqual(3);
    await new Promise((resolve) => setTimeout(resolve, 150));
    expect(answers("heartbeat").length).toBe(beats);
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
    queue.push(task(), task({ token: "tok-2", tool: "crash", id: "task-2" }));
    await until(() => answers("fail").length === 1 && answers("fail", "tok-2").length === 1);
    expect(answers("fail")[0]?.body).toEqual({ message: "the user is away", data: { retryIn: "1h" }, retryable: true });
    expect(answers("fail", "tok-2")[0]?.body).toEqual({ message: "TypeError: x is undefined" });
    // A deliberate HostToolError is the tool's answer; an unexpected exception is reported too.
    expect(errors.map((e) => e.info.phase)).toEqual(["handler"]);
  });

  it("fails a task it has no handler for, and a result JSON cannot carry", async () => {
    start({ ask_user: () => 10n });
    queue.push(task(), task({ token: "tok-2", tool: "unknown", id: "task-2" }));
    await until(() => answers("fail").length === 1 && answers("fail", "tok-2").length === 1);
    expect(answers("fail")[0]?.body).toMatchObject({ message: expect.stringMatching(/is not JSON/) });
    expect(answers("fail", "tok-2")[0]?.body).toEqual({ message: "this worker has no handler for the tool unknown" });
  });

  it("aborts the handler's signal and drops the result when the claim is lost", async () => {
    let signal: AbortSignal | undefined;
    server.on("POST", "/v1/tasks/{token}/heartbeat", problem(410, "task-gone", "the run has ended"));
    start({
      ask_user: async (_args, ctx) => {
        signal = ctx.signal;
        await new Promise((resolve) => ctx.signal.addEventListener("abort", resolve));
        return "too late";
      },
    });
    queue.push(task({ heartbeatTimeoutMs: 30 }));
    await until(() => signal?.aborted === true);
    await until(() => worker?.running === 0);
    expect(answers("complete")).toHaveLength(0);
    expect(answers("fail")).toHaveLength(0);
    expect(signal?.reason).toBeInstanceOf(PtcApiError);
    expect(errors).toEqual([]);
  });

  it("defers the answer, which whoever holds the token sends later", async () => {
    let deferred: DeferredTask | undefined;
    start({
      ask_user: async (_args, ctx) => {
        deferred = await ctx.defer({ timeout: "3d" });
        return "ignored";
      },
    });
    queue.push(task({ heartbeatTimeoutMs: 60 }));
    await until(() => deferred !== undefined && worker?.running === 0);
    expect(answers("defer")[0]?.body).toEqual({ timeout: "3d" });
    expect(answers("complete")).toHaveLength(0);
    const beats = answers("heartbeat").length;
    await new Promise((resolve) => setTimeout(resolve, 80));
    expect(answers("heartbeat").length).toBe(beats);

    expect(deferred?.token).toBe("tok-1");
    await deferred?.complete({ answer: "yes" });
    expect(answers("complete")[0]?.body).toEqual({ result: { answer: "yes" } });
    await ptc.tasks.fail("tok-1", { message: "later failure" });
    expect(answers("fail")[0]?.body).toEqual({ message: "later failure" });
  });

  it("fails a deferred task when the handler throws after deferring", async () => {
    start({
      ask_user: async (_args, ctx) => {
        await ctx.defer();
        throw new HostToolError("could not hand the question over");
      },
    });
    queue.push(task());
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
    queue.push(task(), task({ token: "tok-gone", id: "task-2" }));
    await until(() => completes === 2 && answers("complete", "tok-gone").length === 1, 5_000);
    await until(() => worker?.running === 0);
    expect(errors).toEqual([]);
  });

  it("fails the task when its result is too large for the server", async () => {
    server.on("POST", "/v1/tasks/{token}/complete", problem(413, "payload-too-large", "the result is above 5 MiB"));
    start({ ask_user: () => "x".repeat(100) });
    queue.push(task());
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
    queue.push(task());
    await until(() => answers("fail").length === 2);
    expect(answers("fail")[1]?.body).toEqual({ message: `${"m".repeat(2_000)}…` });
  });

  it("runs at most `concurrency` handlers at once", async () => {
    let running = 0;
    let most = 0;
    const release: Array<() => void> = [];
    start(
      {
        ask_user: async () => {
          running += 1;
          most = Math.max(most, running);
          await new Promise<void>((resolve) => release.push(resolve));
          running -= 1;
          return "ok";
        },
      },
      2,
    );
    for (let i = 1; i <= 5; i++) {
      queue.push(task({ token: `tok-${i}`, id: `task-${i}` }));
    }
    await until(() => release.length === 2);
    await new Promise((resolve) => setTimeout(resolve, 50));
    expect(running).toBe(2);
    while (server.requests.filter((r) => r.path.endsWith("/complete")).length < 5) {
      release.shift()?.();
      await new Promise((resolve) => setTimeout(resolve, 10));
    }
    expect(most).toBe(2);
    const limits = server.calls("GET", "/v1/tasks").map((r) => Number(r.query.get("limit")));
    expect(Math.max(...limits)).toBeLessThanOrEqual(2);
  });

  it("stops gracefully: no new claim, and the handlers in flight answer", async () => {
    let finish: (() => void) | undefined;
    start({
      ask_user: () =>
        new Promise((resolve) => {
          finish = () => resolve("finished");
        }),
    });
    queue.push(task());
    await until(() => finish !== undefined);
    const stopped = worker!.stop();
    let done = false;
    void stopped.then(() => {
      done = true;
    });
    await new Promise((resolve) => setTimeout(resolve, 50));
    expect(done).toBe(false);
    const claims = server.calls("GET", "/v1/tasks").length;
    finish?.();
    await stopped;
    expect(answers("complete")[0]?.body).toEqual({ result: "finished" });
    await new Promise((resolve) => setTimeout(resolve, 30));
    expect(server.calls("GET", "/v1/tasks").length).toBeLessThanOrEqual(claims + 1);
  });

  it("stops when its signal aborts", async () => {
    const controller = new AbortController();
    worker = ptc.worker({ tools: { ask_user: () => "ok" }, signal: controller.signal, onError: () => undefined });
    controller.abort();
    await worker.done;
  });

  it("stops on a refused key, and retries claims after other failures", async () => {
    let claims = 0;
    server.on("GET", "/v1/tasks", () => {
      claims += 1;
      return claims === 1 ? problem(500, "internal", "the store is down") : problem(401, "unauthorized", "unknown key");
    });
    const w = start({ ask_user: () => "ok" });
    await expect(w.done).rejects.toMatchObject({ status: 401 });
    expect(errors.map((e) => [e.info.phase, (e.error as PtcApiError).status])).toEqual([
      ["claim", 500],
      ["claim", 401],
    ]);
  });

  it("refuses a worker without handlers or with a bad concurrency", () => {
    expect(() => ptc.worker({ tools: {} })).toThrow(TypeError);
    expect(() => ptc.worker({ tools: { a: () => 1 }, concurrency: 0 })).toThrow(RangeError);
  });

  it("sends a heartbeat on demand, and throws once the task is gone", async () => {
    let outcome: unknown;
    server.on("POST", "/v1/tasks/{token}/heartbeat", problem(410, "task-gone", "claim expired"));
    start({
      ask_user: async (_args, ctx) => {
        outcome = await ctx.heartbeat().catch((error: unknown) => error);
        return "x";
      },
    });
    queue.push(task());
    await until(() => outcome !== undefined && worker?.running === 0);
    expect(outcome).toBeInstanceOf(PtcApiError);
    expect(answers("complete")).toHaveLength(0);
  });
});
