// The scope of the views (archi/05, "Namespaces"): every namespace of the organization, or one,
// picked at the top of the page. One namespace's views live under `/ns/<name>/`, the same views
// as over every namespace; their reads of runs and statistics send `Rewind-Namespace`, and every
// read made for one namespace is kept apart from the others (its key holds the namespace).
import { createContext, useContext } from "react";

/** The namespace of the route (`/ns/<name>/…`); `null` over every namespace. */
export const ScopeContext = createContext<string | null>(null);

/** The namespace the views read, `null` for every namespace: from the route. */
export function useScopeNamespace(): string | null {
  return useContext(ScopeContext);
}

const PREFIX = "/ns/";

/** A path of the views, in a namespace's scope: `/observability` → `/ns/alice/observability`. */
export function scopedPath(path: string, namespace: string | null): string {
  if (namespace === null) return path;
  const rest = path === "/" ? "" : path;
  return `${PREFIX}${encodeURIComponent(namespace)}${rest}`;
}

/** A path of the app, its scope taken off: `/ns/alice/scripts` → `alice` and `/scripts`. */
export function splitScope(pathname: string): { namespace: string | null; path: string } {
  if (!pathname.startsWith(PREFIX)) return { namespace: null, path: pathname };
  const rest = pathname.slice(PREFIX.length);
  const slash = rest.indexOf("/");
  const raw = slash === -1 ? rest : rest.slice(0, slash);
  let namespace = raw;
  try {
    namespace = decodeURIComponent(raw);
  } catch {
    // Kept as written: the route answers that no such namespace can exist.
  }
  return { namespace: namespace || null, path: slash === -1 ? "/" : rest.slice(slash) };
}

/**
 * A namespace's name as the API takes it (archi/06, "Namespaces"): 1 to 128 letters, digits,
 * `-`, `_`, `.`, `:` and `@`, not starting with `rewind:`.
 */
export function isNamespaceName(name: string): boolean {
  return /^[A-Za-z0-9\-_.:@]{1,128}$/.test(name) && !name.startsWith("rewind:");
}
