// Renders the whole app on a route, against the mocked API, and records the requests it makes.
import { render } from "@testing-library/react";
import { createMemoryRouter, RouterProvider } from "react-router";
import { Providers, routes } from "../app/App";
import { createWorld, type World } from "../mocks/fixtures";
import { createHandlers } from "../mocks/handlers";
import { server } from "./server";

export interface AppOptions {
  world?: World;
  /** The mocked API's clock; fixed by default. */
  clock?: () => number;
}

export const NOW = Date.now();

export function renderApp(path: string, options: AppOptions = {}) {
  const world = options.world ?? createWorld({ now: NOW });
  server.use(...createHandlers(world, { now: options.clock ?? (() => NOW), pollMs: 10 }));
  const requests: URL[] = [];
  const onRequest = ({ request }: { request: Request }) => {
    requests.push(new URL(request.url));
  };
  server.events.on("request:start", onRequest);
  const router = createMemoryRouter(routes, { initialEntries: [path] });
  const view = render(
    <Providers>
      <RouterProvider router={router} />
    </Providers>,
  );
  const unmount = () => {
    server.events.removeListener("request:start", onRequest);
    view.unmount();
  };
  /** The requests made to a path, such as `/v1/runs`. */
  const requested = (path: string) => requests.filter((u) => u.pathname === path);
  return { ...view, unmount, world, router, requested };
}

/** The search of the route the app shows. */
export function search(router: ReturnType<typeof createMemoryRouter>): URLSearchParams {
  return new URLSearchParams(router.state.location.search);
}
