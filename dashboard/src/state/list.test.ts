// A list the server answers whole (archi/05, "Nothing moves on its own"): an equal answer keeps
// the rows' objects, a changed row alone gets a new one, a write known beforehand shows at once,
// and a refusal takes it back.
import { QueryClient } from "@tanstack/react-query";
import { afterEach, describe, expect, it, vi } from "vitest";
import { type Api, ApiError } from "../sources/client";
import { type KeyedList, keyedList } from "./list";
import { Store } from "./store";

interface Item {
  id: string;
  n: number;
}

let stores: Store[] = [];
afterEach(() => {
  for (const store of stores) store.dispose();
  stores = [];
});

/** A list over an answer the test changes. */
function listOf(first: Item[]): { list: KeyedList<Item>; answer: { rows: Item[] } } {
  // The list reads through its own `load`: the client is never called.
  const store = new Store({} as Api, new QueryClient());
  stores.push(store);
  const answer = { rows: first };
  const list = keyedList<Item>(store, "items", () => ({
    load: async () => answer.rows.map((r) => ({ ...r })),
    key: (r) => r.id,
  }));
  return { list, answer };
}

const ids = (list: KeyedList<Item>) => list.getSnapshot().data?.map((r) => r.id);
const ns = (list: KeyedList<Item>) => list.getSnapshot().data?.map((r) => r.n);

describe("a keyed list", () => {
  it("is ready with an empty first answer", async () => {
    const { list } = listOf([]);
    await vi.waitFor(() => expect(list.getSnapshot().status).toBe("ready"));
    expect(list.getSnapshot().data).toEqual([]);
  });

  it("keeps the rows' objects through an equal answer, and renews only a changed row", async () => {
    const { list, answer } = listOf([
      { id: "a", n: 1 },
      { id: "b", n: 2 },
    ]);
    await vi.waitFor(() => expect(ids(list)).toEqual(["a", "b"]));
    const first = list.getSnapshot();

    await list.refreshNow();
    expect(list.getSnapshot()).toBe(first);

    answer.rows = [
      { id: "a", n: 1 },
      { id: "b", n: 3 },
    ];
    await list.refreshNow();
    const second = list.getSnapshot().data ?? [];
    expect(ns(list)).toEqual([1, 3]);
    expect(second[0]).toBe(first.data?.[0]);
    expect(second[1]).not.toBe(first.data?.[1]);
  });

  it("shows a write known beforehand at once, and keeps it once the server agrees", async () => {
    const { list, answer } = listOf([
      { id: "a", n: 1 },
      { id: "b", n: 2 },
    ]);
    await vi.waitFor(() => expect(ids(list)).toEqual(["a", "b"]));
    let release = () => {};
    const sent = new Promise<void>((resolve) => {
      release = resolve;
    });
    const done = list.change(
      async () => {
        await sent;
        answer.rows = [{ id: "a", n: 1 }];
      },
      (rows) => rows.delete("b"),
    );
    // Before the server answers.
    await vi.waitFor(() => expect(ids(list)).toEqual(["a"]));
    release();
    await done;
    expect(ids(list)).toEqual(["a"]);
  });

  it("takes a refused write back, and says why", async () => {
    const { list } = listOf([{ id: "a", n: 1 }]);
    await vi.waitFor(() => expect(ns(list)).toEqual([1]));
    let refuse = (_: Error) => {};
    const refused = new Promise<void>((_, reject) => {
      refuse = reject;
    });
    const done = list.change(
      () => refused,
      (rows) =>
        rows.update("a", (row) => {
          row.n = 9;
        }),
    );
    await vi.waitFor(() => expect(ns(list)).toEqual([9]));
    refuse(new Error("role-in-use"));
    await expect(done).rejects.toThrow("role-in-use");
    await vi.waitFor(() => expect(ns(list)).toEqual([1]));
  });

  it("keeps the rows read before when a read fails", async () => {
    const { list, answer } = listOf([{ id: "a", n: 1 }]);
    await vi.waitFor(() => expect(ns(list)).toEqual([1]));
    const shown = list.getSnapshot().data;
    // A refusal is not asked again: the read fails at once.
    const refusal = new ApiError(403, {
      type: "urn:rewind:problem:forbidden",
      title: "Forbidden",
      status: 403,
      detail: "",
    });
    Object.defineProperty(answer, "rows", {
      get: () => {
        throw refusal;
      },
    });
    await list.refreshNow();
    expect(list.getSnapshot().status).toBe("ready");
    expect(list.getSnapshot().data).toBe(shown);
    expect(list.getSnapshot().error).toBe(refusal);
  });
});
