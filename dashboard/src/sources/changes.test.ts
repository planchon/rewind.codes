import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ControlChange, RunSummary } from "../model/types";
import { backoffDelay, type ChangeStreamState, controlChange, followChanges } from "./changes";
import { Api } from "./client";

/** A stream the test writes to, as the server would. */
function sse() {
  let controller!: ReadableStreamDefaultController<Uint8Array>;
  const body = new ReadableStream<Uint8Array>({
    start(c) {
      controller = c;
    },
  });
  const encoder = new TextEncoder();
  return {
    response: new Response(body, { headers: { "Content-Type": "text/event-stream" } }),
    send: (text: string) => controller.enqueue(encoder.encode(text)),
    close: () => controller.close(),
  };
}

function recorder() {
  const states: ChangeStreamState[] = [];
  const opens: boolean[] = [];
  const runs: RunSummary[] = [];
  const controls: ControlChange[] = [];
  let resyncs = 0;
  return {
    states,
    opens,
    runs,
    controls,
    get resyncs() {
      return resyncs;
    },
    handlers: {
      onOpen: (reopened: boolean) => opens.push(reopened),
      onRun: (run: RunSummary) => runs.push(run),
      onControl: (change: ControlChange) => controls.push(change),
      onResync: () => {
        resyncs += 1;
      },
      onState: (state: ChangeStreamState) => states.push(state),
    },
  };
}

beforeEach(() => {
  // The middle of the spread: the waits as the backoff says them.
  vi.spyOn(Math, "random").mockReturnValue(0.5);
});

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
});

describe("the change stream", () => {
  it("waits 1 s after a drop, doubling up to 30 s", () => {
    expect([0, 1, 2, 3, 4, 5, 6, 12].map((n) => backoffDelay(n))).toEqual([
      1_000, 2_000, 4_000, 8_000, 16_000, 30_000, 30_000, 30_000,
    ]);
  });

  it("opens again after each failure, 1 s, 2 s, 4 s… later, up to 30 s", async () => {
    vi.useFakeTimers();
    const tries: number[] = [];
    const api = new Api({
      baseUrl: "http://rewind.test",
      token: () => "rewind_sk_test",
      fetch: async () => {
        tries.push(Date.now());
        throw new TypeError("network down");
      },
    });
    const r = recorder();
    const stream = followChanges(api, r.handlers);
    await vi.advanceTimersByTimeAsync(0);
    expect(tries).toHaveLength(1);
    for (const wait of [1_000, 2_000, 4_000, 8_000, 16_000, 30_000, 30_000]) {
      await vi.advanceTimersByTimeAsync(wait - 1);
      const before = tries.length;
      await vi.advanceTimersByTimeAsync(1);
      expect(tries).toHaveLength(before + 1);
    }
    const gaps = tries.slice(1).map((t, i) => t - (tries[i] as number));
    expect(gaps).toEqual([1_000, 2_000, 4_000, 8_000, 16_000, 30_000, 30_000]);
    expect(r.states[0]).toBe("connecting");
    expect(r.states.slice(1).every((s) => s === "reconnecting")).toBe(true);
    stream.close();
  });

  it("reads the runs' summaries and resyncs; reopens at once when the server closes it", async () => {
    vi.useFakeTimers();
    const streams: ReturnType<typeof sse>[] = [];
    const api = new Api({
      baseUrl: "http://rewind.test",
      token: () => "rewind_sk_test",
      fetch: async () => {
        const s = sse();
        streams.push(s);
        return s.response;
      },
    });
    const r = recorder();
    const stream = followChanges(api, r.handlers);
    await vi.advanceTimersByTimeAsync(0);
    expect(r.opens).toEqual([false]);
    expect(r.states).toEqual(["connecting", "live"]);

    streams[0]?.send('event: run\ndata: {"id":"r1","status":"active"}\n\n');
    streams[0]?.send("event: resync\ndata: {}\n\n");
    await vi.advanceTimersByTimeAsync(0);
    expect(r.runs.map((run) => run.id)).toEqual(["r1"]);
    expect(r.resyncs).toBe(1);

    // The token it was opened with expired: the server closes it, and it opens again at once.
    await vi.advanceTimersByTimeAsync(10 * 60_000);
    streams[0]?.close();
    await vi.advanceTimersByTimeAsync(0);
    expect(streams).toHaveLength(2);
    expect(r.opens).toEqual([false, true]);
    expect(r.states.at(-1)).toBe("live");
    expect(r.states).not.toContain("reconnecting");

    // Closed again as soon as it opened: a drop, followed after 1 s.
    streams[1]?.close();
    await vi.advanceTimersByTimeAsync(999);
    expect(streams).toHaveLength(2);
    expect(r.states.at(-1)).toBe("reconnecting");
    await vi.advanceTimersByTimeAsync(1);
    expect(streams).toHaveLength(3);

    // Closed at once again: the waits go on growing, 2 s, then 4 s.
    streams[2]?.close();
    await vi.advanceTimersByTimeAsync(1_999);
    expect(streams).toHaveLength(3);
    await vi.advanceTimersByTimeAsync(1);
    expect(streams).toHaveLength(4);
    streams[3]?.close();
    await vi.advanceTimersByTimeAsync(3_999);
    expect(streams).toHaveLength(4);
    await vi.advanceTimersByTimeAsync(1);
    expect(streams).toHaveLength(5);

    // One that lives starts the waits over: dropped later, it is followed after 1 s.
    await vi.advanceTimersByTimeAsync(60_000);
    streams[4]?.close();
    await vi.advanceTimersByTimeAsync(0);
    expect(streams).toHaveLength(6);
    streams[5]?.close();
    await vi.advanceTimersByTimeAsync(1_000);
    expect(streams).toHaveLength(7);
    stream.close();
  });

  it("names what changed in the control plane, and skips what it cannot read", async () => {
    vi.useFakeTimers();
    let stream!: ReturnType<typeof sse>;
    const api = new Api({
      baseUrl: "http://rewind.test",
      token: () => "rewind_sk_test",
      fetch: async () => {
        stream = sse();
        return stream.response;
      },
    });
    const r = recorder();
    const changes = followChanges(api, r.handlers);
    await vi.advanceTimersByTimeAsync(0);
    stream.send(
      [
        'event: script\ndata: {"digest":"ab"}',
        'event: workflow\ndata: {"workflow":"wf_7k2m9q4x8d1c"}',
        'event: secret\ndata: {"name":"github","namespace":"alice"}',
        'event: secret\ndata: {"name":"zendesk"}',
        'event: namespace\ndata: {"name":"bob"}',
        "event: namespace\ndata: {}",
        'event: connection\ndata: {"id":"conn_1"}',
        'event: later\ndata: {"id":"x"}',
        "event: script\ndata: {}",
        ": keep-alive",
      ]
        .map((block) => `${block}\n\n`)
        .join(""),
    );
    await vi.advanceTimersByTimeAsync(0);
    expect(r.controls).toEqual([
      { kind: "workflow", workflow: "wf_7k2m9q4x8d1c" },
      { kind: "secret", name: "github", namespace: "alice" },
      { kind: "secret", name: "zendesk" },
      { kind: "namespace", name: "bob" },
      { kind: "namespace" },
      { kind: "connection", id: "conn_1" },
    ]);
    expect(r.runs).toEqual([]);
    // A change of earlier versions, or a workflow by its name: not known.
    expect(controlChange("workflow", '{"name":"triage"}')).toBeNull();
    expect(controlChange("script", '{"digest":"ab"}')).toBeNull();
    expect(controlChange("connection", "not json")).toBeNull();
    changes.close();
  });

  it("reopens at once when asked, with the token held then", async () => {
    vi.useFakeTimers();
    let token = "first";
    const sent: string[] = [];
    const api = new Api({
      baseUrl: "http://rewind.test",
      token: () => token,
      fetch: async (request) => {
        sent.push(request.headers.get("Authorization") ?? "");
        const s = sse();
        request.signal.addEventListener("abort", () => s.close());
        return s.response;
      },
    });
    const r = recorder();
    const stream = followChanges(api, r.handlers);
    await vi.advanceTimersByTimeAsync(0);
    token = "second";
    stream.reopen();
    await vi.advanceTimersByTimeAsync(0);
    expect(sent).toEqual(["Bearer first", "Bearer second"]);
    expect(r.opens).toEqual([false, true]);
    stream.close();
  });
});
