import "@testing-library/jest-dom/vitest";
import { cleanup, configure } from "@testing-library/react";
import { afterAll, afterEach, beforeAll } from "vitest";
import { server } from "./server";

// Before any view, the app asks how to sign in, then who calls: on a busy machine the first
// render of a test can take more than the default second.
configure({ asyncUtilTimeout: 3_000 });

beforeAll(() => server.listen({ onUnhandledFrame: "error" }));
afterEach(() => {
  cleanup();
  server.resetHandlers();
  try {
    window.localStorage.clear();
    window.sessionStorage.clear();
  } catch {
    // Storage may be unavailable.
  }
  // An invitation link's query, from a test that opened one.
  window.history.replaceState(null, "", "/");
});
afterAll(() => server.close());

// jsdom lays nothing out: give elements a size, so virtualized lists render their first rows.
Object.defineProperties(HTMLElement.prototype, {
  offsetWidth: { configurable: true, get: () => 1000 },
  offsetHeight: { configurable: true, get: () => 400 },
  clientWidth: { configurable: true, get: () => 1000 },
  clientHeight: { configurable: true, get: () => 400 },
});
