// The address a sign-in started from, kept through GitHub's round trip. A code by email signs in
// on the same page, so the route in the fragment stays; GitHub's callback lands on `/#/`, so the
// route is kept in the tab's storage before leaving and put back when the app loads again.
// Claude's consent (`#/connect/authorize?request=…`) is the address this is for, but any deep
// link comes back the same way.
import { read, write } from "./storage";

const KEY = "rewind.route";
/** A route kept longer than this is stale: the sign-in was given up. */
const KEPT_MS = 15 * 60_000;

function isHome(hash: string): boolean {
  return hash === "" || hash === "#" || hash === "#/";
}

/** Keeps the route of the address, before leaving for GitHub. */
export function keepRoute(): void {
  try {
    const hash = window.location.hash;
    if (isHome(hash)) return;
    write("session", KEY, JSON.stringify({ hash, at: Date.now() }));
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
    if (!isHome(window.location.hash)) return;
    const { hash, at } = JSON.parse(kept) as { hash?: unknown; at?: unknown };
    if (typeof hash !== "string" || !hash.startsWith("#/") || typeof at !== "number") return;
    if (now - at > KEPT_MS) return;
    const { pathname, search } = window.location;
    window.history.replaceState(window.history.state, "", `${pathname}${search}${hash}`);
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
    return window.location.hash.startsWith("#/connect/authorize");
  } catch {
    return false;
  }
}
