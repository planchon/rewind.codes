// The reads made through TanStack Query that the change stream makes stale (archi/05, "Live"):
// the secrets' lists and usages, the signing keys, the namespaces' names and suggestions. The store
// reads again those on screen (TanStack Query's active queries) when a change touches them; none is
// read on a timer.
import type { ControlChange } from "../model/types";

/** The start of the keys of every read of the secrets. */
export const SECRETS = ["secrets"] as const;
export const SIGNING_KEYS = ["signing-keys"] as const;
/** The start of the keys of the namespaces' names and suggestions. */
export const NAMESPACES = ["namespaces"] as const;

/** The scope of a secret in the keys: `*` for the organization's. */
export const secretScope = (namespace: string | null | undefined) => namespace ?? "*";

/** The reads a control-plane change makes stale, by the start of their keys. */
export function staleOn(change: ControlChange): (readonly unknown[])[] {
  switch (change.kind) {
    case "secret":
      return [[...SECRETS, secretScope(change.namespace)]];
    case "namespace":
      return [NAMESPACES];
    default:
      return [];
  }
}

/** The reads that move with the runs: a secret's reads, the runs that attach it. */
export const MOVED_BY_RUNS: readonly (readonly unknown[])[] = [SECRETS];

/** The reads read once again when the change stream reopens or resyncs. */
export const LIVE_QUERIES: readonly (readonly unknown[])[] = [SECRETS, SIGNING_KEYS, NAMESPACES];
