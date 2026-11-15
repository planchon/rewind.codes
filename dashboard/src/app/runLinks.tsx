// Where a run was opened from: the list of Observability its "back" returns to, with its query
// and filters (archi/08, "The back of a run returns to the list it was opened from"). The list
// names itself to the links of its runs; a run opened from anywhere else goes back to every run.
import { createContext, type ReactNode, useContext } from "react";
import { useLocation } from "react-router";

const RunsFromContext = createContext<string | null>(null);

/** The runs linked inside return to this page. */
export function RunsFrom({ children }: { children: ReactNode }) {
  const { pathname, search } = useLocation();
  return (
    <RunsFromContext.Provider value={`${pathname}${search}`}>{children}</RunsFromContext.Provider>
  );
}

interface FromState {
  from: string;
}

/** The state a link to a run carries: where its "back" returns. */
export function useRunLinkState(): FromState | undefined {
  const from = useContext(RunsFromContext);
  return from ? { from } : undefined;
}

/** Where a run's "back" returns, from the state its link carried. */
export function runBackPath(state: unknown): string {
  if (state && typeof state === "object" && "from" in state) {
    const from = (state as { from: unknown }).from;
    if (typeof from === "string" && from.startsWith("/observability")) return from;
  }
  return "/observability";
}
