// The collections that no single request fills: written by the answers that carry their rows and
// by the change streams. A TanStack DB collection with a sync of its own, whose writes `state/`
// makes: it skips the rows equal to what it holds, so an unchanged row keeps its object and
// renders nothing.
import { createCollection, deepEquals } from "@tanstack/db";

/** Two values equal by value: what makes a write worth making. */
export function sameValue(a: unknown, b: unknown): boolean {
  return a === b || deepEquals(a, b);
}

interface Writer<T extends object> {
  begin: () => void;
  write: (
    message: { type: "insert" | "update"; value: T } | { type: "delete"; key: string },
  ) => void;
  commit: () => unknown;
}

let serial = 0;

/** A collection rows are written to, by key, with what each row is now. */
export function writableCollection<T extends object>(name: string, getKey: (row: T) => string) {
  let writer: Writer<T> | null = null;
  /** The rows as written, without the virtual properties the collection adds. */
  const rows = new Map<string, T>();
  serial += 1;
  const collection = createCollection<T, string>({
    id: `${name}#${serial}`,
    getKey,
    // `state/` decides when a collection goes, not its subscribers.
    gcTime: 0,
    startSync: true,
    sync: {
      rowUpdateMode: "full",
      sync: ({ begin, write, commit, markReady }) => {
        writer = { begin, write: write as Writer<T>["write"], commit };
        markReady();
        return () => {
          writer = null;
        };
      },
    },
  });

  /** Writes the rows that differ from what is held; answers the keys written. */
  const upsert = (values: readonly T[]): string[] => {
    const w = writer;
    if (!w) return [];
    const changed: T[] = [];
    for (const value of values) {
      const key = getKey(value);
      const held = rows.get(key);
      if (held !== undefined && sameValue(held, value)) continue;
      changed.push(value);
    }
    if (changed.length === 0) return [];
    w.begin();
    for (const value of changed) {
      const key = getKey(value);
      w.write({ type: rows.has(key) ? "update" : "insert", value });
      rows.set(key, value);
    }
    w.commit();
    return changed.map(getKey);
  };

  const remove = (keys: Iterable<string>) => {
    const w = writer;
    if (!w) return;
    const gone = [...keys].filter((k) => rows.has(k));
    if (gone.length === 0) return;
    w.begin();
    for (const key of gone) {
      w.write({ type: "delete", key });
      rows.delete(key);
    }
    w.commit();
  };

  return {
    collection,
    upsert,
    remove,
    /** The row as written. */
    raw: (key: string): T | undefined => rows.get(key),
    keys: () => rows.keys(),
    /** The row as the collection holds it: the same object until the row changes. */
    get: (key: string): T | undefined => collection.get(key),
  };
}

export type WritableCollection<T extends object> = ReturnType<typeof writableCollection<T>>;
