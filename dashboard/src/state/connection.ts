// The state of the change stream, for the dot beside the organization picker: `live` while it is
// open, `reconnecting` after a drop, `idle` until a view that follows the organization opens it.
import { useSyncExternalStore } from "react";
import type { ConnectionState } from "./live";
import { useStore } from "./store";

export type { ConnectionState } from "./live";

export function useConnection(): ConnectionState {
  const { live } = useStore();
  return useSyncExternalStore(live.subscribe, live.getState, live.getState);
}
