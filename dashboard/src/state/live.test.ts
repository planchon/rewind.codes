// When what is on screen is read again (archi/05, "Live"): from the change stream only. A run's
// change reads the searches a second after the last change (at most 5 s after the first of a
// burst), the counts at most every 10 s, the run's details at once; a control-plane change reads
// the views that show it; a reopened stream or a `resync` reads everything once; nothing is read on
// a timer, and nothing while the stream is down.
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ControlChange, RunSummary } from "../model/types";
import type { ChangeHandlers, ChangeStream } from "../sources/changes";
import { Api } from "../sources/client";
import { Live, type LiveTargets, type Watched } from "./live";

/** A change stream the test drives, as `followChanges` would. */
function fakeStream() {
  let handlers: ChangeHandlers | null = null;
  let opened = 0;
  const follow = (_api: Api, h: ChangeHandlers): ChangeStream => {
    handlers = h;
    opened += 1;
    return { reopen: () => undefined, close: () => undefined };
  };
  const on = () => {
    if (!handlers) throw new Error("the stream is not open");
    return handlers;
  };
  return {
    follow,
    get opened() {
      return opened;
    },
    open: (reopened = false) => {
      on().onState("live");
      on().onOpen(reopened);
    },
    drop: () => on().onState("reconnecting"),
    run: (id: string) => on().onRun({ id } as RunSummary),
    control: (change: ControlChange) => on().onControl(change),
    resync: () => on().onResync(),
  };
}

/** Something on screen, counting its reads. */
function item(
  kind: Watched["kind"],
  options: { runId?: string; controls?: (change: ControlChange) => boolean } = {},
) {
  const reads: number[] = [];
  const watched: Watched & { reads: number[] } = {
    kind,
    reads,
    refresh: () => {
      reads.push(Date.now());
    },
    touches: (runId) => runId === options.runId,
    ...(options.controls ? { controls: options.controls } : {}),
  };
  return watched;
}

function live(targets: Partial<LiveTargets> = {}) {
  const stream = fakeStream();
  const counted = { applied: 0, aggregates: 0, rereads: 0, controls: [] as ControlChange[] };
  const api = new Api({ baseUrl: "http://rewind.test", token: () => null });
  const l = new Live(
    api,
    {
      apply: () => {
        counted.applied += 1;
      },
      aggregates: () => {
        counted.aggregates += 1;
      },
      reread: () => {
        counted.rereads += 1;
      },
      control: (change) => {
        counted.controls.push(change);
      },
      ...targets,
    },
    { follow: stream.follow },
  );
  return { live: l, stream, counted };
}

const T0 = Date.parse("2026-11-23T10:00:00Z");

beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(T0);
});

afterEach(() => {
  vi.useRealTimers();
});

describe("the live views", () => {
  it("read nothing on a timer, open or down", async () => {
    const { live: l, stream, counted } = live();
    const all = [item("search"), item("aggregate"), item("detail", { runId: "r1" })];
    for (const i of all) l.watch(i);
    stream.open();
    await vi.advanceTimersByTimeAsync(30 * 60_000);
    stream.drop();
    expect(l.getState()).toBe("reconnecting");
    await vi.advanceTimersByTimeAsync(30 * 60_000);
    expect(all.map((i) => i.reads.length)).toEqual([0, 0, 0]);
    expect(counted.aggregates + counted.rereads).toBe(0);
    l.dispose();
  });

  it("read once what is on screen when the stream opens again, or resyncs", async () => {
    const { live: l, stream, counted } = live();
    const all = [item("search"), item("aggregate"), item("detail"), item("control")];
    for (const i of all) l.watch(i);
    // The first opening follows the first reads: nothing more.
    stream.open(false);
    expect(all.map((i) => i.reads.length)).toEqual([0, 0, 0, 0]);
    stream.drop();
    stream.open(true);
    expect(all.map((i) => i.reads.length)).toEqual([1, 1, 1, 1]);
    expect(counted.rereads).toBe(1);
    stream.resync();
    expect(all.map((i) => i.reads.length)).toEqual([2, 2, 2, 2]);
    expect(counted.rereads).toBe(2);
    await vi.advanceTimersByTimeAsync(60 * 60_000);
    expect(all.map((i) => i.reads.length)).toEqual([2, 2, 2, 2]);
    l.dispose();
  });

  it("read a search a second after the last change of a run, at most 5 s after the first", async () => {
    const { live: l, stream, counted } = live();
    const search = item("search");
    l.watch(search);
    stream.open();
    stream.run("r1");
    expect(counted.applied).toBe(1);
    await vi.advanceTimersByTimeAsync(999);
    expect(search.reads).toEqual([]);
    await vi.advanceTimersByTimeAsync(1);
    expect(search.reads).toEqual([T0 + 1_000]);

    // A steady burst, a change every 500 ms: read 5 s after its first change, then a second after
    // its last one.
    const start = Date.now();
    for (let i = 0; i < 16; i += 1) {
      stream.run(`r${i}`);
      await vi.advanceTimersByTimeAsync(500);
    }
    expect(search.reads.slice(1)).toEqual([start + 5_000]);
    await vi.advanceTimersByTimeAsync(1_000);
    expect(search.reads.slice(1)).toEqual([start + 5_000, start + 7_500 + 1_000]);
    l.dispose();
  });

  it("read the counts at most every 10 s while runs change, and a run's details at once", async () => {
    const { live: l, stream, counted } = live();
    const counts = item("aggregate");
    const details = item("detail", { runId: "r1" });
    const other = item("detail", { runId: "r2" });
    for (const i of [counts, details, other]) l.watch(i);
    stream.open();
    stream.run("r1");
    expect(details.reads).toEqual([T0]);
    expect(other.reads).toEqual([]);
    await vi.advanceTimersByTimeAsync(0);
    expect(counts.reads).toEqual([T0]);
    expect(counted.aggregates).toBe(1);
    for (let s = 1; s <= 25; s += 1) {
      await vi.advanceTimersByTimeAsync(1_000);
      stream.run("r3");
    }
    await vi.advanceTimersByTimeAsync(10_000);
    expect(counts.reads).toEqual([T0, T0 + 10_000, T0 + 20_000, T0 + 30_000]);
    expect(counted.aggregates).toBe(4);
    expect(details.reads).toEqual([T0]);
    l.dispose();
  });

  it("read what a control-plane change touches, and nothing else", async () => {
    const { live: l, stream, counted } = live();
    const secrets = item("aggregate", { controls: (c) => c.kind === "secret" });
    const workflows = item("control", { controls: (c) => c.kind === "workflow" });
    const alice = item("aggregate", {
      controls: (c) => c.kind === "namespace" && (c.name == null || c.name === "alice"),
    });
    const search = item("search");
    for (const i of [secrets, workflows, alice, search]) l.watch(i);
    stream.open();
    stream.control({ kind: "secret", name: "github" });
    expect([secrets, workflows, alice, search].map((i) => i.reads.length)).toEqual([1, 0, 0, 0]);
    stream.control({ kind: "workflow", name: "triage", version: "ab" });
    stream.control({ kind: "namespace", name: "bob" });
    expect([secrets, workflows, alice, search].map((i) => i.reads.length)).toEqual([1, 1, 0, 0]);
    stream.control({ kind: "namespace" });
    stream.control({ kind: "namespace", name: "alice" });
    stream.control({ kind: "script", digest: "cd" });
    stream.control({ kind: "connection", id: "conn_1" });
    expect([secrets, workflows, alice, search].map((i) => i.reads.length)).toEqual([1, 1, 2, 0]);
    expect(counted.controls.map((c) => c.kind)).toEqual([
      "secret",
      "workflow",
      "namespace",
      "namespace",
      "namespace",
      "script",
      "connection",
    ]);
    // A control-plane change moves no count of runs.
    await vi.advanceTimersByTimeAsync(60_000);
    expect(counted.aggregates).toBe(0);
    l.dispose();
  });

  it("opens the stream for what follows the organization, not for what only rereads", () => {
    const { live: l, stream } = live();
    l.watch(item("control"), false);
    expect(stream.opened).toBe(0);
    expect(l.getState()).toBe("idle");
    l.watch(item("search"));
    expect(stream.opened).toBe(1);
    l.dispose();
  });
});
