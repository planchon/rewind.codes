// The action sets the organization registered (`actions:read`), without their schemas: what a
// workflow imports from "rewind:actions".
import type { ActionSetSummary } from "../model/types";
import { type ListInit, useList } from "./list";
import type { Read } from "./read";
import type { Store } from "./store";

const ACTION_SETS = (store: Store): ListInit<ActionSetSummary> => ({
  load: (signal) => store.api.actionSets(signal),
  key: (t) => t.id,
});

export function useActionSets(enabled = true): Read<readonly ActionSetSummary[]> {
  return useList(enabled ? "action-sets" : null, ACTION_SETS);
}
