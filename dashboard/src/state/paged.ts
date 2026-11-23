// A list read page after page (the scripts, the namespaces, the workflows): one value of the store
// (`resource.ts`) holding every row shown, all read again together, so that a refresh swaps them in
// place and an equal answer renders nothing. A read after "Load more" asks for as many rows as are
// shown and a page more, a page at a time past the most one answer holds. A changed key (another
// filter, another order) keeps the previous rows, marked `stale`, until its first answer. When it is
// read again comes from the change stream (`live.ts`), as for any value of the store.
//
// What a view shows of the rows follows the operator as a table of runs does (D7): an answer that
// adds, drops or reorders rows applies at once while the list is scrolled to its top; otherwise
// the rows keep their place, each with its new values, and the answer waits, counted as "N new",
// until the operator shows it, comes back to the page, or an answer arrives at the top.
import { useCallback, useMemo, useRef, useState } from "react";
import { type Read, useResource } from "./read";
import type { ReadStatus, Resource, ResourceInit } from "./resource";
import type { PendingAnswer } from "./search";
import { useStore } from "./store";

/** The most rows one answer holds (`limit` is at most 500). */
const MAX_PAGE = 500;

/** One page as the server answers it. */
export interface Page<T, H> {
  rows: readonly T[];
  /** The cursor of the page after; `null` on the last. */
  next: string | null;
  /** What the page says besides its rows (a total, a month). */
  head: H;
}

export interface PagedInit<T, H> {
  kind: ResourceInit<unknown>["kind"];
  /** Whether a control-plane change touches the rows: they are then read again at once. */
  controls?: ResourceInit<unknown>["controls"];
  /** Rows per page: the first read, and each "Load more". */
  pageSize: number;
  /** One page of at most `limit` rows, after the cursor `after`. */
  readPage: (after: string | null, limit: number, signal: AbortSignal) => Promise<Page<T, H>>;
  /** A row's id: a row two pages repeat is shown once. */
  keyOf: (row: T) => string;
}

/** The value a list holds: what a write shows a change of (`Resource.change`). */
export interface Paged<T, H> {
  rows: readonly T[];
  /** More rows past these. */
  more: boolean;
  head: H;
}

export interface PagedRead<T, H> {
  status: ReadStatus;
  /** The rows shown, in the order shown; empty until the first answer. */
  rows: readonly T[];
  /** What the first page says besides its rows; `undefined` until the first answer. */
  head: H | undefined;
  /** The failure of the first read, when nothing can be shown. */
  error: unknown;
  /** The rows are the previous key's, until the new one answers. */
  stale: boolean;
  hasMore: boolean;
  loadingMore: boolean;
  /** Reads as many rows as shown, and a page more. */
  loadMore: () => void;
  /** An answer that adds, drops or reorders the rows, held back while the list is scrolled. */
  pending: PendingAnswer | null;
  /** Shows the answer held back. */
  showPending: () => void;
  /** Whether the list is scrolled to its top: answers apply at once while it is. */
  setAtTop: (atTop: boolean) => void;
}

/** How many rows each list asks for: its page size until "Load more". */
const counts = new WeakMap<Resource<unknown>, number>();

async function readRows<T, H>(
  init: PagedInit<T, H>,
  count: number,
  signal: AbortSignal,
): Promise<Paged<T, H>> {
  const rows: T[] = [];
  const seen = new Set<string>();
  let head: { value: H } | null = null;
  let after: string | null = null;
  do {
    const page: Page<T, H> = await init.readPage(
      after,
      Math.min(MAX_PAGE, count - rows.length),
      signal,
    );
    head ??= { value: page.head };
    for (const row of page.rows) {
      const key = init.keyOf(row);
      if (seen.has(key)) continue;
      seen.add(key);
      rows.push(row);
    }
    after = page.rows.length > 0 ? page.next : null;
  } while (after !== null && rows.length < count);
  return { rows, more: after !== null, head: (head as { value: H }).value };
}

const NO_ROWS: readonly never[] = [];

function sameKeys(a: readonly string[], b: readonly string[]): boolean {
  return a.length === b.length && a.every((k, i) => k === b[i]);
}

/** The keys shown, then those of the answer after the last of them ("Load more"). */
function appendTail(shown: readonly string[], answer: readonly string[]): string[] {
  const held = new Set(shown);
  let last = -1;
  for (let i = answer.length - 1; i >= 0; i -= 1) {
    if (held.has(answer[i] as string)) {
      last = i;
      break;
    }
  }
  if (last < 0) return [...answer];
  return [...shown, ...answer.slice(last + 1).filter((k) => !held.has(k))];
}

/** What a view shows of a list: its order, and the last value of each row it shows. */
export interface Shown<T> {
  /** The list it shows: another list shows its own rows. */
  resource: object | null;
  /** The answer it was built from. */
  answer: readonly T[] | null;
  keys: readonly string[];
  rows: readonly T[];
  pending: PendingAnswer | null;
}

/** What a view shows of a list's answer, given what it showed before (D7). */
export function showRows<T>(
  before: Shown<T>,
  resource: object | null,
  answer: readonly T[] | null,
  how: {
    keyOf: (row: T) => string;
    stale: boolean;
    atTop: boolean;
    /** A "Load more" is being read: its answer adds the new tail. */
    loadingMore: boolean;
    shownAnswer: readonly T[] | null;
  },
): Shown<T> {
  if (answer === null) return { resource, answer, keys: [], rows: NO_ROWS, pending: null };
  if (before.answer === answer && before.resource === resource && how.shownAnswer !== answer)
    return before;
  const keys = answer.map(how.keyOf);
  const show = (order: readonly string[]): Shown<T> => {
    if (sameKeys(order, keys)) return { resource, answer, keys, rows: answer, pending: null };
    const byKey = new Map(answer.map((row) => [how.keyOf(row), row] as const));
    const old = new Map(before.rows.map((row) => [how.keyOf(row), row] as const));
    const rows = order.flatMap((k) => {
      const row = byKey.get(k) ?? old.get(k);
      return row === undefined ? [] : [row];
    });
    const held = new Set(order);
    return {
      resource,
      answer,
      keys: order,
      rows,
      pending: { added: keys.filter((k) => !held.has(k)).length },
    };
  };
  // The first answer, another list or key, an answer at the top, or "Show": the answer as it is.
  if (
    before.resource !== resource ||
    before.answer === null ||
    how.stale ||
    how.atTop ||
    how.shownAnswer === answer
  )
    return show(keys);
  // "Load more": the rows shown, then the new tail.
  if (how.loadingMore && keys.length > before.keys.length)
    return show(appendTail(before.keys, keys));
  // Held back: the rows keep their place, each with its new values.
  return show(before.keys);
}

/** A list of the store, held while the view shows it; `key` null holds none. */
export function usePaged<T, H>(key: string | null, init: () => PagedInit<T, H>): PagedRead<T, H> {
  const store = useStore();
  const pageSize = init().pageSize;
  let resource: Resource<Paged<T, H>> | null = null;
  const make = (): ResourceInit<Paged<T, H>> => {
    const i = init();
    return {
      kind: i.kind,
      ...(i.controls ? { controls: i.controls } : {}),
      load: (signal) =>
        readRows(i, (resource && counts.get(resource as Resource<unknown>)) ?? i.pageSize, signal),
    };
  };
  resource = key === null ? null : store.resource(key, make);
  const read: Read<Paged<T, H>> = useResource(key, make);
  const [loading, setLoading] = useState<Resource<Paged<T, H>> | null>(null);
  const atTop = useRef(true);
  /** The answer the operator asked to see ("Show"). */
  const shownAnswer = useRef<readonly T[] | null>(null);
  const [showCount, setShowCount] = useState(0);
  const held = useRef<Shown<T>>({
    resource: null,
    answer: null,
    keys: [],
    rows: NO_ROWS,
    pending: null,
  });
  const keyOf = init().keyOf;
  // biome-ignore lint/correctness/useExhaustiveDependencies: `showCount` asks for the answer held back; `keyOf` never changes for a list.
  const shownView = useMemo(
    (): Shown<T> =>
      showRows(held.current, resource, read.data?.rows ?? null, {
        keyOf,
        stale: read.stale,
        atTop: atTop.current,
        loadingMore: loading !== null && loading === resource,
        shownAnswer: shownAnswer.current,
      }),
    [read.data, read.stale, resource, showCount, loading],
  );
  held.current = shownView;
  const answer = read.data?.rows ?? null;
  const showPending = useCallback(() => {
    shownAnswer.current = answer;
    setShowCount((n) => n + 1);
  }, [answer]);
  const setAtTop = useCallback((top: boolean) => {
    atTop.current = top;
  }, []);
  const shown = shownView.rows.length;
  const loadMore = useCallback(() => {
    if (!resource || loading === resource) return;
    const held = resource as Resource<unknown>;
    counts.set(held, Math.max(counts.get(held) ?? pageSize, shown) + pageSize);
    setLoading(resource);
    const done = resource;
    void done.refreshNow().finally(() => setLoading((now) => (now === done ? null : now)));
  }, [resource, loading, pageSize, shown]);
  return useMemo(
    () => ({
      status: read.status,
      rows: shownView.rows,
      head: read.data?.head,
      error: read.status === "error" ? read.error : null,
      stale: read.stale,
      hasMore: !read.stale && (read.data?.more ?? false),
      loadingMore: loading !== null && loading === resource,
      loadMore,
      pending: shownView.pending,
      showPending,
      setAtTop,
    }),
    [read, loading, resource, loadMore, shownView, showPending, setAtTop],
  );
}
