// Where a run was opened from: the list of Runs its "back" returns to, with its query
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

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/**
 * The path of a run's page, by its handle (`/runs/dana/triage/06bqx3k9f2mt`) or its UUID, and of
 * one of its sections.
 */
export function runPath(run: string, section?: string): string {
  return `/runs/${run}${section ? `/${section}` : ""}`;
}

/** What the path after `/runs/` names: a UUID or a handle, then a section. */
export function parseRunPath(rest: string): { run: string; section: string | null } {
  const parts = rest.split("/").filter(Boolean);
  if (UUID.test(parts[0] ?? "")) return { run: parts[0] ?? "", section: parts[1] ?? null };
  return { run: parts.slice(0, 3).join("/"), section: parts[3] ?? null };
}

/** Whether `text` reads as a run's UUID or handle. */
export function isRunName(text: string): boolean {
  return UUID.test(text) || /^[\w.:@-]+\/[\w$-]+\/[0-9a-z]{12,25}$/i.test(text);
}

/** The Runs view's paths: `/runs`, `/runs/running`, `/runs/waiting`, `/runs/failures`. */
export function isRunsView(pathname: string): boolean {
  return /^\/runs(\/(running|waiting|failures))?\/?$/.test(pathname);
}

/** Where a run's "back" returns, from the state its link carried. */
export function runBackPath(state: unknown): string {
  if (state && typeof state === "object" && "from" in state) {
    const from = (state as { from: unknown }).from;
    if (typeof from === "string" && isRunsView(from.split("?")[0] ?? "")) return from;
  }
  return "/runs";
}
