// The address a sign-in started from, kept through GitHub's round trip. A code by email signs in
// on the same page, so the route stays; GitHub's callback lands on `/`, so the route is kept in
// the tab's storage before leaving and put back when the app loads again. Claude's consent
// (`/connect/authorize?request=…`) is the address this is for, but any deep link comes back the
// same way. Links of earlier versions carried the route in the fragment (`/#/runs/abc`): they are
// read as the path they meant, before anything else reads the address.
import { read, write } from "./storage";

const KEY = "rewind.route";
/** A route kept longer than this is stale: the sign-in was given up. */
const KEPT_MS = 15 * 60_000;

function isHome(pathname: string): boolean {
  return pathname === "" || pathname === "/";
}

/**
 * An address of an earlier version, its route in the fragment (`/#/runs/abc?step=3`,
 * `/?auth_error=x#/`), rewritten as the path it meant (`/runs/abc?step=3`, `/?auth_error=x`):
 * the query of the address and that of the route both kept.
 */
export function adoptHashRoute(): void {
  try {
    const { hash, search } = window.location;
    if (!hash.startsWith("#/")) return;
    const route = new URL(hash.slice(1), window.location.origin);
    const params = new URLSearchParams(search);
    for (const [key, value] of route.searchParams) params.append(key, value);
    const query = params.toString();
    window.history.replaceState(
      window.history.state,
      "",
      `${route.pathname}${query ? `?${query}` : ""}`,
    );
  } catch {
    // The address stays as it is.
  }
}

/** Keeps the route of the address, before leaving for GitHub. */
export function keepRoute(): void {
  try {
    const { pathname, search } = window.location;
    if (isHome(pathname)) return;
    write("session", KEY, JSON.stringify({ route: `${pathname}${search}`, at: Date.now() }));
  } catch {
    // Not kept: the sign-in lands on the home page.
  }
}

/**
 * Back from GitHub on the home page: the route kept before leaving, put back in the address,
 * once. Called before the router reads the address.
 */
export function restoreRoute(now = Date.now()): void {
  const kept = read("session", KEY);
  if (kept === null) return;
  write("session", KEY, null);
  try {
    if (!isHome(window.location.pathname)) return;
    const { route, at } = JSON.parse(kept) as { route?: unknown; at?: unknown };
    if (typeof route !== "string" || !route.startsWith("/") || typeof at !== "number") return;
    if (route.startsWith("//") || now - at > KEPT_MS) return;
    // What the callback put in the address (`?auth_error=…`) is read after this: it stays.
    const url = new URL(route, window.location.origin);
    for (const [key, value] of new URLSearchParams(window.location.search))
      url.searchParams.append(key, value);
    window.history.replaceState(window.history.state, "", `${url.pathname}${url.search}`);
  } catch {
    // The address stays as it is.
  }
}

/**
 * The address is Claude's consent: a member without organization meets the onboarding first, and
 * the consent once in one (the address stays).
 */
export function atConsent(): boolean {
  try {
    return window.location.pathname.startsWith("/connect/authorize");
  } catch {
    return false;
  }
}
