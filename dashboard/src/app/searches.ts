// Saved searches: a name and a query, kept in the browser like the key (archi/08-decisions.md,
// "Saved searches are kept in the browser"), so the dashboard keeps sending only GET. Storage
// can be missing or throw: the searches then last until the page is closed.
import { useCallback, useSyncExternalStore } from "react";
import { read, write } from "./storage";

const KEY = "rewind.searches";

/** The views a search opens. */
export type SearchView = "overview" | "runs" | "waiting" | "failures";

export interface SavedSearch {
  name: string;
  q: string;
  view: SearchView;
}

const VIEWS: readonly SearchView[] = ["overview", "runs", "waiting", "failures"];

const listeners = new Set<() => void>();
/** What was written when storage refused it. */
let memory: string | null = null;
let cache: { raw: string | null; searches: SavedSearch[] } = { raw: null, searches: [] };

function parse(raw: string | null): SavedSearch[] {
  if (!raw) return [];
  try {
    const value: unknown = JSON.parse(raw);
    if (!Array.isArray(value)) return [];
    return value.flatMap((s) =>
      s &&
      typeof s === "object" &&
      typeof s.name === "string" &&
      typeof s.q === "string" &&
      VIEWS.includes(s.view)
        ? [{ name: s.name, q: s.q, view: s.view as SearchView }]
        : [],
    );
  } catch {
    return [];
  }
}

function snapshot(): SavedSearch[] {
  const raw = read("local", KEY) ?? memory;
  if (raw !== cache.raw) cache = { raw, searches: parse(raw) };
  return cache.searches;
}

function store(searches: SavedSearch[]) {
  const raw = searches.length > 0 ? JSON.stringify(searches) : null;
  write("local", KEY, raw);
  memory = read("local", KEY) === raw ? null : raw;
  for (const listener of listeners) listener();
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  const onStorage = (e: StorageEvent) => {
    if (e.key === KEY || e.key === null) listener();
  };
  window.addEventListener("storage", onStorage);
  return () => {
    listeners.delete(listener);
    window.removeEventListener("storage", onStorage);
  };
}

export function useSavedSearches() {
  const searches = useSyncExternalStore(subscribe, snapshot, snapshot);
  /** Saves a search; one of the same name is replaced. */
  const save = useCallback((search: SavedSearch) => {
    const name = search.name.trim();
    if (!name) return;
    store([...snapshot().filter((s) => s.name !== name), { ...search, name }]);
  }, []);
  const remove = useCallback((name: string) => {
    store(snapshot().filter((s) => s.name !== name));
  }, []);
  return { searches, save, remove };
}
