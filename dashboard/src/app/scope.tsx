// The scope in the frame (archi/05, "Namespaces"): the route `/ns/:namespace` that scopes its
// views to one namespace, the links that stay in the scope, the namespace the frame shows (the
// route's, else that of the object a page shows, a run), and where the picker leads.
import {
  createContext,
  type ReactNode,
  useCallback,
  useContext,
  useLayoutEffect,
  useMemo,
  useState,
} from "react";
import { Outlet, useLocation, useParams } from "react-router";
import { ProblemView } from "../components/bits";
import { removeTerm, splitTerms } from "../model/query";
import {
  isNamespaceName,
  ScopeContext,
  scopedPath,
  splitScope,
  useScopeNamespace,
} from "../state/scope";

/** The route `/ns/:namespace`: the views under it read that namespace. */
export function NamespaceScope() {
  const { namespace = "" } = useParams();
  if (!isNamespaceName(namespace))
    return (
      <div className="page">
        <ProblemView error={new Error("No namespace can have this name.")} title="Not found" />
      </div>
    );
  return (
    <ScopeContext.Provider value={namespace}>
      <Outlet />
    </ScopeContext.Provider>
  );
}

/** The paths of the views, in the scope of the page: `at("/observability")`. */
export function useAt(): (path: string) => string {
  const namespace = useScopeNamespace();
  return useCallback((path: string) => scopedPath(path, namespace), [namespace]);
}

interface ObjectScope {
  namespace: string | null;
  set: (namespace: string | null) => void;
}

const ObjectScopeContext = createContext<ObjectScope | null>(null);

/** Holds the namespace of the object the page shows, for the frame. */
export function ObjectScopeProvider({ children }: { children: ReactNode }) {
  const [namespace, setNamespace] = useState<string | null>(null);
  const set = useCallback((next: string | null) => setNamespace(next), []);
  const value = useMemo(() => ({ namespace, set }), [namespace, set]);
  return <ObjectScopeContext.Provider value={value}>{children}</ObjectScopeContext.Provider>;
}

/**
 * The page shows an object of one namespace (a run, at its handle's path): the picker and the
 * sidebar show that namespace while the page is open.
 */
export function useObjectNamespace(namespace: string | null | undefined): void {
  const set = useContext(ObjectScopeContext)?.set;
  useLayoutEffect(() => {
    if (!set || !namespace) return;
    set(namespace);
    return () => set(null);
  }, [set, namespace]);
}

/** The namespace the frame shows: the route's, else the object's on the page; `null` for all. */
export function useShownNamespace(): string | null {
  const { pathname } = useLocation();
  const route = splitScope(pathname).namespace;
  const object = useContext(ObjectScopeContext)?.namespace ?? null;
  return route ?? object;
}

/** The query without its `namespace:` terms, which named namespaces of the scope left. */
export function withoutNamespaceTerms(q: string): string {
  let next = q;
  const terms = splitTerms(q).filter((t) => t.kind === "field" && t.field === "namespace");
  for (const t of terms.reverse()) next = removeTerm(next, t);
  return next.replace(/\s+/g, " ").trim();
}

/** The parameters a view of runs keeps when its scope changes. */
const KEPT = ["q", "window", "sort"] as const;
/** Those a list of everything since the first run keeps: it has no window (Tools, Scripts). */
const KEPT_QUERY = ["q"] as const;

/**
 * Where picking a scope leads from the page at `pathname` (and `search`): the same view in that
 * scope, with its query and window; from the page of one object (a run, a script, a workflow),
 * the list of its kind. Every namespace's Namespaces and one namespace's Usage stand for each
 * other. `null` picks every namespace.
 */
export function switchedPath(pathname: string, search: string, namespace: string | null): string {
  const { path } = splitScope(pathname);
  const parts = path.split("/").filter(Boolean);
  const first = parts[0] ?? "";
  let view: string;
  let keep: readonly string[] = [];
  switch (first) {
    case "overview":
      view = "/overview";
      keep = KEPT;
      break;
    case "tools":
      view = "/tools";
      keep = KEPT_QUERY;
      break;
    case "observability":
      view = parts.length > 1 ? `/observability/${parts[1]}` : "/observability";
      keep = KEPT;
      break;
    case "runs":
      view = "/observability";
      break;
    case "scripts":
      view = "/scripts";
      keep = parts.length === 1 ? KEPT_QUERY : [];
      break;
    case "testing":
      view = "/testing";
      break;
    case "secrets":
      view = "/secrets";
      break;
    case "usage":
    case "namespaces":
      view = namespace === null ? "/namespaces" : "/usage";
      break;
    default:
      view = "/";
  }
  const params = new URLSearchParams();
  if (keep.length > 0) {
    const from = new URLSearchParams(search);
    for (const name of keep) {
      const value = from.get(name);
      if (value === null) continue;
      const kept = name === "q" ? withoutNamespaceTerms(value) : value;
      if (kept) params.set(name, kept);
    }
  }
  const query = params.toString();
  return `${scopedPath(view, namespace)}${query ? `?${query}` : ""}`;
}

/**
 * Where the namespace's row in the sidebar leads: the same view over every namespace (from a run,
 * the runs).
 */
export function unscopedPath(pathname: string, search: string): string {
  return switchedPath(pathname, search, null);
}
