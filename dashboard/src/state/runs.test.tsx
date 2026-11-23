// The state of the runs (archi/05, "Live" and "Nothing moves on its own"): a change from the stream
// updates a run everywhere at once, equal data renders nothing, a refresh keeps the rows' nodes,
// an answer that moves rows waits behind "N new · Show" while the table is scrolled, and a read
// after "Load more" keeps as many rows.
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { memo, type ReactNode, useEffect, useRef } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createWorld, type RunFixture, runId, type World } from "../mocks/fixtures";
import { createHandlers } from "../mocks/handlers";
import { JournalBuilder } from "../mocks/journal";
import { publicSummary, summarize } from "../mocks/readmodel";
import type { RunSummary } from "../model/types";
import { Api } from "../sources/client";
import { NOW, renderApp } from "../test/app";
import { server } from "../test/server";
import { changeStream } from "../test/streams";
import { useRunSearch } from "./runs";
import { StateProvider } from "./store";

function summaryOf(world: World, fixture: RunFixture): RunSummary {
  return publicSummary(summarize(world, fixture, NOW));
}

/** The requests to `/v1/runs` the mocked API answered. */
function answered() {
  const urls: URL[] = [];
  const listener = ({ request }: { request: Request }) => {
    const url = new URL(request.url);
    if (url.pathname === "/v1/runs") urls.push(url);
  };
  server.events.on("response:mocked", listener);
  return {
    urls,
    stop: () => server.events.removeListener("response:mocked", listener),
  };
}

/** The state alone, over the mocked API, without the app around it. */
function renderState(ui: ReactNode, world: World) {
  server.use(...createHandlers(world, { now: () => NOW, pollMs: 10 }));
  const changes = changeStream();
  const api = new Api({ baseUrl: window.location.origin, token: () => null });
  const wrap = (inner: ReactNode) => (
    <StateProvider api={api} token={null}>
      {inner}
    </StateProvider>
  );
  const view = render(wrap(ui));
  return { ...view, rerender: (inner: ReactNode) => view.rerender(wrap(inner)), changes };
}

const renders = new Map<string, number>();

const Row = memo(function Row({ run }: { run: RunSummary }) {
  renders.set(run.id, (renders.get(run.id) ?? 0) + 1);
  return (
    <li data-run={run.id}>
      {run.id} {run.status}
    </li>
  );
});

function Runs({ q, label }: { q: string; label: string }) {
  const search = useRunSearch({ q });
  renders.set(label, (renders.get(label) ?? 0) + 1);
  return (
    <ul aria-label={label}>
      {search.runs.map((run) => (
        <Row key={run.id} run={run} />
      ))}
    </ul>
  );
}

function rowOf(list: HTMLElement, id: string): HTMLElement {
  const row = list.querySelector<HTMLElement>(`[data-run="${id}"]`);
  if (!row) throw new Error(`no row ${id}`);
  return row;
}

/** Waits for the mocked API to have answered `n` more reads of runs. */
async function readAgain(log: { urls: URL[] }, before: number, n = 1) {
  await waitFor(() => expect(log.urls.length).toBeGreaterThanOrEqual(before + n), {
    timeout: 4_000,
  });
  // The answer is applied after the response: give it a turn.
  await new Promise((resolve) => setTimeout(resolve, 50));
}

beforeEach(() => {
  renders.clear();
});

describe("the state of the runs", () => {
  it("applies a run's change from the stream in every table that shows it, without a request", async () => {
    const world = createWorld({ now: NOW });
    const active = world.runs.find((r) => r.labels.conversation === "c-42") as RunFixture;
    const log = answered();
    const { changes } = renderState(
      <>
        <Runs q="" label="every run" />
        <Runs q="status:active" label="active runs" />
      </>,
      world,
    );
    const every = screen.getByRole("list", { name: "every run" });
    const activeList = screen.getByRole("list", { name: "active runs" });
    await waitFor(() => expect(rowOf(every, active.id)).toHaveTextContent("active"));
    await waitFor(() => expect(rowOf(activeList, active.id)).toHaveTextContent("active"));
    await waitFor(() => expect(changes.live).toBe(true));
    const reads = log.urls.length;

    const summary = summaryOf(world, active);
    changes.run({
      ...summary,
      status: "completed",
      historyLen: summary.historyLen + 2,
      endedAt: new Date(NOW).toISOString(),
      waitingFor: [],
    });
    await waitFor(() => expect(rowOf(every, active.id)).toHaveTextContent("completed"));
    expect(rowOf(activeList, active.id)).toHaveTextContent("completed");
    // No request: the change carried the run's summary.
    expect(log.urls.length).toBe(reads);
    log.stop();
  });

  it("renders nothing for an answer equal to what it shows", async () => {
    const world = createWorld({ now: NOW });
    const log = answered();
    const { changes } = renderState(<Runs q="" label="every run" />, world);
    const list = screen.getByRole("list", { name: "every run" });
    await waitFor(() => expect(within(list).getAllByRole("listitem")).toHaveLength(50));
    await waitFor(() => expect(changes.live).toBe(true));
    const nodes = within(list).getAllByRole("listitem");
    const before = new Map(renders);
    const reads = log.urls.length;

    // Too much changed: everything shown is read again, and the server answers the same.
    changes.resync();
    await readAgain(log, reads);
    expect(new Map(renders)).toEqual(before);
    expect(within(list).getAllByRole("listitem")).toEqual(nodes);
    log.stop();
  });
});

function ScrolledRuns({ q }: { q: string }) {
  const search = useRunSearch({ q });
  // Told once, as the table's observer tells it: when the table crosses its top.
  const told = useRef(false);
  useEffect(() => {
    if (told.current) return;
    told.current = true;
    search.setAtTop(false);
  });
  return (
    <ul aria-label="scrolled">
      {search.runs.map((run) => (
        <Row key={run.id} run={run} />
      ))}
      {search.pending && <li>{search.pending.added} new</li>}
    </ul>
  );
}

describe("a table scrolled down", () => {
  it("keeps holding answers back after its search changes: the scroll is the view's", async () => {
    const world = createWorld({ now: NOW });
    const log = answered();
    const { changes, rerender } = renderState(<ScrolledRuns q="" />, world);
    const list = screen.getByRole("list", { name: "scrolled" });
    await waitFor(() => expect(within(list).getAllByRole("listitem")).toHaveLength(50));
    await waitFor(() => expect(changes.live).toBe(true));

    // Another search, while the table is still scrolled: its first answer shows.
    rerender(<ScrolledRuns q="@agent:triage" />);
    await waitFor(() => expect(within(list).queryByText(/new$/)).toBeNull());
    const shown = within(list)
      .getAllByRole("listitem")
      .map((li) => li.getAttribute("data-run"));
    const reads = log.urls.length;

    // A new run of the search: held back, since the view is still scrolled.
    const started = startRun(world, 3);
    changes.run(summaryOf(world, started));
    await readAgain(log, reads);
    await waitFor(() => expect(within(list).getByText("1 new")).toBeVisible());
    expect(
      within(list)
        .getAllByRole("listitem")
        .filter((li) => li.hasAttribute("data-run"))
        .map((li) => li.getAttribute("data-run")),
    ).toEqual(shown);
    log.stop();
  });
});

/** An IntersectionObserver the test drives: where the table's top is. */
class FakeObserver {
  static all: FakeObserver[] = [];
  readonly elements: Element[] = [];
  constructor(readonly callback: IntersectionObserverCallback) {
    FakeObserver.all.push(this);
  }
  observe(element: Element) {
    this.elements.push(element);
  }
  unobserve() {}
  disconnect() {}
  takeRecords() {
    return [];
  }
  /** The table's top is visible (`top`), or scrolled above the view. */
  static place(atTop: boolean) {
    for (const o of FakeObserver.all) {
      for (const target of o.elements) {
        o.callback(
          [
            {
              target,
              isIntersecting: atTop,
              intersectionRatio: atTop ? 1 : 0,
              boundingClientRect: { top: atTop ? 120 : -400 } as DOMRectReadOnly,
              rootBounds: { top: 56 } as DOMRectReadOnly,
              intersectionRect: {} as DOMRectReadOnly,
              time: 0,
            },
          ],
          o as unknown as IntersectionObserver,
        );
      }
    }
  }
}

function tableRows(): HTMLElement[] {
  return within(screen.getByRole("table")).getAllByRole("row").slice(1);
}

function idOf(row: HTMLElement | undefined): string {
  return row?.getAttribute("data-run") ?? "";
}

/** A run started a second ago: the first of every run, the most recently started first. */
function startRun(world: World, n: number): RunFixture {
  const base = world.runs.find((r) => r.labels.conversation === "c-42") as RunFixture;
  const start = NOW - 1_000 + n;
  const fixture: RunFixture = {
    id: runId(start, 9_000 + n),
    namespace: base.namespace,
    script: base.script,
    toolset: base.toolset,
    labels: { agent: "triage", conversation: `new-${n}` },
    journal: new JournalBuilder(start).started({
      script: base.script.digest,
      toolset: base.toolset,
      input: { repo: "acme/new" },
    }),
  };
  world.runs.push(fixture);
  return fixture;
}

describe("the table of runs", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
    FakeObserver.all = [];
  });

  it("keeps the nodes of the rows that did not change, and changes the one that did", async () => {
    const world = createWorld({ now: NOW });
    const log = answered();
    renderApp("/observability", { world });
    const changes = changeStream();
    await waitFor(() => expect(tableRows()).toHaveLength(50));
    await waitFor(() => expect(changes.live).toBe(true));
    const rows = tableRows();
    const target = world.runs.find((r) => r.id === idOf(rows[3])) as RunFixture;
    const summary = summaryOf(world, target);
    const reads = log.urls.length;

    changes.run({ ...summary, status: "failed", historyLen: summary.historyLen + 1 });
    await waitFor(() => expect(within(rows[3] as HTMLElement).getByText("failed")).toBeVisible());
    // The search is read again a second after the change: the same rows, the same nodes.
    await readAgain(log, reads);
    const after = tableRows();
    expect(after).toHaveLength(50);
    for (const [i, row] of after.entries()) expect(row).toBe(rows[i]);
    log.stop();
  });

  it("applies an answer that adds rows at once while the table is at its top, and holds it back while it is scrolled", async () => {
    vi.stubGlobal("IntersectionObserver", FakeObserver);
    const user = userEvent.setup();
    const world = createWorld({ now: NOW });
    const log = answered();
    renderApp("/observability", { world });
    const changes = changeStream();
    await waitFor(() => expect(tableRows()).toHaveLength(50));
    await waitFor(() => expect(changes.live).toBe(true));
    FakeObserver.place(true);

    // At its top: the new run shows at once.
    const first = startRun(world, 1);
    let reads = log.urls.length;
    changes.run(summaryOf(world, first));
    await readAgain(log, reads);
    await waitFor(() => expect(idOf(tableRows()[0])).toBe(first.id));

    // Scrolled: the rows stay, and the toolbar says what waits.
    FakeObserver.place(false);
    const shown = tableRows();
    const second = startRun(world, 2);
    reads = log.urls.length;
    changes.run(summaryOf(world, second));
    await readAgain(log, reads);
    const show = await screen.findByRole("button", { name: "1 new · Show" });
    expect(tableRows()).toEqual(shown);
    expect(idOf(tableRows()[0])).toBe(first.id);

    await user.click(show);
    await waitFor(() => expect(idOf(tableRows()[0])).toBe(second.id));
    expect(screen.queryByRole("button", { name: /new · Show/ })).toBeNull();
    // The rows that stayed kept their nodes.
    expect(tableRows()[1]).toBe(shown[0]);
    log.stop();
  });

  it('reads again as many runs as "Load more" showed', async () => {
    const user = userEvent.setup();
    const world = createWorld({ now: NOW });
    const log = answered();
    renderApp("/observability", { world });
    const changes = changeStream();
    await waitFor(() => expect(tableRows()).toHaveLength(50));
    await user.click(screen.getByRole("button", { name: "Load more" }));
    await waitFor(() => expect(tableRows().length).toBeGreaterThan(50));
    const count = tableRows().length;
    await waitFor(() => expect(changes.live).toBe(true));
    const reads = log.urls.length;

    changes.resync();
    await readAgain(log, reads);
    const reread = log.urls.slice(reads).find((u) => !u.searchParams.has("cursor"));
    expect(reread?.searchParams.get("limit")).toBe("100");
    expect(tableRows()).toHaveLength(count);
    log.stop();
  });
});
