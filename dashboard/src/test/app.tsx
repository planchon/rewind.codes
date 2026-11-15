// Renders the whole app on a route, against the mocked API, and records the requests it makes.
import { render } from "@testing-library/react";
import { createMemoryRouter, RouterProvider } from "react-router";
import { Providers, routes } from "../app/App";
import { authenticated, signIn, userByEmail } from "../mocks/access";
import { createWorld, type World } from "../mocks/fixtures";
import { createHandlers } from "../mocks/handlers";
import { server } from "./server";

export interface AppOptions {
  world?: World;
  /** The mocked API's clock; fixed by default. */
  clock?: () => number;
  /** Where the app sends the browser away (GitHub's sign-in). */
  assign?: (url: string) => void;
}

export interface Call {
  url: URL;
  method: string;
  headers: Headers;
  /** The body, as text. */
  body: Promise<string>;
}

export const NOW = Date.now();

export function renderApp(path: string, options: AppOptions = {}) {
  const world = options.world ?? createWorld({ now: NOW });
  server.use(...createHandlers(world, { now: options.clock ?? (() => NOW), pollMs: 10 }));
  const calls: Call[] = [];
  const onRequest = ({ request }: { request: Request }) => {
    calls.push({
      url: new URL(request.url),
      method: request.method,
      headers: new Headers(request.headers),
      body: request
        .clone()
        .text()
        .catch(() => ""),
    });
  };
  server.events.on("request:start", onRequest);
  const router = createMemoryRouter(routes, { initialEntries: [path] });
  const view = render(
    <Providers {...(options.assign ? { assign: options.assign } : {})}>
      <RouterProvider router={router} />
    </Providers>,
  );
  const unmount = () => {
    server.events.removeListener("request:start", onRequest);
    view.unmount();
  };
  /** The requests made to a path, such as `/v1/runs`. */
  const requested = (path: string) =>
    calls.filter((c) => c.url.pathname === path).map((c) => c.url);
  /** The calls made to a path, with their method, headers and body. */
  const callsTo = (path: string, method?: string) =>
    calls.filter((c) => c.url.pathname === path && (!method || c.method === method));
  return { ...view, unmount, world, router, requested, callsTo };
}

/** The search of the route the app shows. */
export function search(router: ReturnType<typeof createMemoryRouter>): URLSearchParams {
  return new URLSearchParams(router.state.location.search);
}

/** A world with WorkOS, its browser already signed in as `email`, in `organizationId` if given. */
export function signedIn(
  email: string,
  organizationId?: string,
  options: Parameters<typeof createWorld>[0] = { now: NOW },
): World {
  const world = createWorld({ workos: true, ...options });
  const state = signIn(world.access, email, null);
  if (typeof state === "string") throw new Error(state);
  const user = userByEmail(world.access, email)?.user;
  if (!user) throw new Error(`no user ${email}`);
  if (organizationId || state.status === "organization_selection_required") {
    authenticated(world.access, user, organizationId ?? null);
  }
  return world;
}
