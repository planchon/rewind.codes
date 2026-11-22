// What a view gets of a value the state reads: pending until its first answer, then its data, kept
// through every later read (a refresh, a reconnection, a resync swaps it in place, and an equal
// answer keeps its object). A view whose key changes (another filter, another window) keeps the
// previous key's data, marked `stale`, until the new key's first answer.
import type { UseQueryResult } from "@tanstack/react-query";
import { useEffect, useMemo, useRef, useSyncExternalStore } from "react";
import type { Resource, ResourceInit, ResourceSnapshot } from "./resource";
import { useStore } from "./store";

export type Read<T> = (
  | { status: "pending"; data: undefined; error: null; isPending: true; isError: false }
  | { status: "error"; data: undefined; error: unknown; isPending: false; isError: true }
  | { status: "ready"; data: T; error: unknown; isPending: false; isError: false }
) & {
  /** The data shown is the previous key's, until the first answer of the new one. */
  stale: boolean;
  /** Reads it again now, and resolves once it is read. */
  refresh: () => Promise<void>;
};

const PENDING: ResourceSnapshot<never> = { status: "pending", data: undefined, error: null };
const noSubscribe = () => () => {};
const pendingSnapshot = () => PENDING;
const noRefresh = () => Promise.resolve();

/** A snapshot as a view reads it. */
export function toRead<T>(
  snap: ResourceSnapshot<T>,
  stale: boolean,
  refresh: () => Promise<void>,
): Read<T> {
  switch (snap.status) {
    case "pending":
      return {
        status: "pending",
        data: undefined,
        error: null,
        isPending: true,
        isError: false,
        stale,
        refresh,
      };
    case "error":
      return {
        status: "error",
        data: undefined,
        error: snap.error,
        isPending: false,
        isError: true,
        stale,
        refresh,
      };
    case "ready":
      return {
        status: "ready",
        data: snap.data as T,
        error: snap.error,
        isPending: false,
        isError: false,
        stale,
        refresh,
      };
  }
}

/** A resource of the store, held while the view shows it; `key` null holds none. */
export function useResource<T>(key: string | null, init: () => ResourceInit<T>): Read<T> {
  const store = useStore();
  const resource: Resource<T> | null = key === null ? null : store.resource(key, init);
  // The query of a read names the time it is read at: the latest way to read is kept.
  if (resource) resource.setLoad(init().load);
  useEffect(() => resource?.retain(), [resource]);
  const snap = useSyncExternalStore<ResourceSnapshot<T>>(
    resource?.subscribe ?? noSubscribe,
    resource?.getSnapshot ?? pendingSnapshot,
    resource?.getSnapshot ?? pendingSnapshot,
  );
  const last = useRef<ResourceSnapshot<T> | null>(null);
  if (snap.status === "ready") last.current = snap;
  const previous = snap.status === "pending" && key !== null ? last.current : null;
  return useMemo(
    () =>
      toRead(
        previous ?? snap,
        previous !== null,
        resource ? () => resource.refreshNow() : noRefresh,
      ),
    [snap, previous, resource],
  );
}

/** A value read through TanStack Query: a script, a payload, an entry, which never change. */
export function useQueryRead<T>(query: UseQueryResult<T>): Read<T> {
  const { status, data, error, isPlaceholderData, refetch } = query;
  return useMemo(
    () =>
      fromQuery<T>(status, data, error, isPlaceholderData, async () => {
        await refetch();
      }),
    [status, data, error, isPlaceholderData, refetch],
  );
}

function fromQuery<T>(
  status: UseQueryResult<T>["status"],
  data: T | undefined,
  error: unknown,
  isPlaceholderData: boolean,
  refresh: () => Promise<void>,
): Read<T> {
  const query = { status, data, error, isPlaceholderData };
  // A read that fails after an answer keeps showing that answer, with the failure beside it.
  if (query.status === "success" || (query.status === "error" && query.data !== undefined)) {
    return {
      status: "ready",
      data: query.data as T,
      error: query.error,
      isPending: false,
      isError: false,
      stale: query.isPlaceholderData,
      refresh,
    };
  }
  if (query.status === "error") {
    return {
      status: "error",
      data: undefined,
      error: query.error,
      isPending: false,
      isError: true,
      stale: false,
      refresh,
    };
  }
  return {
    status: "pending",
    data: undefined,
    error: null,
    isPending: true,
    isError: false,
    stale: false,
    refresh,
  };
}
