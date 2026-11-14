// The namespace filter: the end user every view is narrowed to, kept in the URL (`?ns=`) and sent
// as `Rewind-Namespace` with every request of the run views (archi/05, "Dashboard"). Without it,
// the views read every namespace of the organization.
//
// The links of the app follow the filter: `Link`, `NavLink`, `Navigate` and `useNavigate` from
// here carry `ns` to wherever they lead, unless the target names a namespace itself. Biome
// refuses the router's own elsewhere (biome.json), so that no link drops the filter.
import { useCallback, useMemo } from "react";
import {
  type NavigateOptions,
  type NavigateProps,
  Link as RouterLink,
  type LinkProps as RouterLinkProps,
  Navigate as RouterNavigate,
  NavLink as RouterNavLink,
  type NavLinkProps as RouterNavLinkProps,
  type To,
  useNavigate as useRouterNavigate,
  useSearchParams,
} from "react-router";
import type { Api } from "../api/client";
import { useApi } from "./session";

/** The query parameter of the filter: `#/runs?ns=bob`. */
export const NS_PARAM = "ns";

/** The namespace the views are narrowed to, from the URL; `null` for every namespace. */
export function useNamespace(): string | null {
  const [params] = useSearchParams();
  const ns = params.get(NS_PARAM);
  return ns ? ns : null;
}

/** The client of the run views: it names the filter's namespace with every request. */
export function useScopedApi(): Api {
  const api = useApi();
  const ns = useNamespace();
  return useMemo(() => api.inNamespace(ns), [api, ns]);
}

/** A view's new search, with the namespace filter of the current one kept. */
export function keepNamespace(current: URLSearchParams, next: URLSearchParams): URLSearchParams {
  const ns = current.get(NS_PARAM);
  if (ns === null || next.has(NS_PARAM)) return next;
  const kept = new URLSearchParams(next);
  kept.set(NS_PARAM, ns);
  return kept;
}

/** A link's target with the namespace added, unless it names one (`ns=`) itself. */
export function withNamespace(to: To, ns: string | null): To {
  if (ns === null) return to;
  if (typeof to !== "string") {
    const params = new URLSearchParams(to.search ?? "");
    if (params.has(NS_PARAM)) return to;
    params.set(NS_PARAM, ns);
    return { ...to, search: `?${params.toString()}` };
  }
  const hashAt = to.indexOf("#");
  const head = hashAt < 0 ? to : to.slice(0, hashAt);
  const hash = hashAt < 0 ? "" : to.slice(hashAt);
  const queryAt = head.indexOf("?");
  const path = queryAt < 0 ? head : head.slice(0, queryAt);
  const params = new URLSearchParams(queryAt < 0 ? "" : head.slice(queryAt + 1));
  if (params.has(NS_PARAM)) return to;
  params.set(NS_PARAM, ns);
  return `${path}?${params.toString()}${hash}`;
}

/** A link of the app, which keeps the namespace filter. */
export function Link({ to, ...props }: RouterLinkProps) {
  const ns = useNamespace();
  return <RouterLink to={withNamespace(to, ns)} {...props} />;
}

/** A navigation link of the app, which keeps the namespace filter. */
export function NavLink({ to, ...props }: RouterNavLinkProps) {
  const ns = useNamespace();
  return <RouterNavLink to={withNamespace(to, ns)} {...props} />;
}

/** A redirection of the app, which keeps the namespace filter. */
export function Navigate({ to, ...props }: NavigateProps) {
  const ns = useNamespace();
  return <RouterNavigate to={withNamespace(to, ns)} {...props} />;
}

/** `navigate(to)`, keeping the namespace filter. */
export function useNavigate(): (to: To, options?: NavigateOptions) => void {
  const navigate = useRouterNavigate();
  const ns = useNamespace();
  return useCallback(
    (to: To, options?: NavigateOptions) => {
      void navigate(withNamespace(to, ns), options);
    },
    [navigate, ns],
  );
}
