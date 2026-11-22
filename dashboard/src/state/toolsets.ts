// The toolsets the organization registered (`toolsets:read`), without their schemas: what a
// script imports from "rewind:tools".
import type { ToolsetSummary } from "../model/types";
import { type ListInit, useList } from "./list";
import type { Read } from "./read";
import type { Store } from "./store";

const TOOLSETS = (store: Store): ListInit<ToolsetSummary> => ({
  load: (signal) => store.api.toolsets(signal),
  key: (t) => t.id,
});

export function useToolsets(enabled = true): Read<readonly ToolsetSummary[]> {
  return useList(enabled ? "toolsets" : null, TOOLSETS);
}
