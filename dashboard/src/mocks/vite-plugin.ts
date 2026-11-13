// `pnpm dev:mock`: the Vite dev server answers /v1 with the mocked API, so the app runs
// unchanged, event stream included, with no server and no service worker in the build.
import { Readable } from "node:stream";
import type { Plugin } from "vite";

export function mockApi(): Plugin {
  return {
    name: "rewind-mock-api",
    apply: "serve",
    configureServer(server) {
      let handlers: Promise<unknown[]> | undefined;
      const load = async () => {
        const [{ createHandlers }, { createWorld }] = await Promise.all([
          server.ssrLoadModule("/src/mocks/handlers.ts"),
          server.ssrLoadModule("/src/mocks/fixtures.ts"),
        ]);
        const requireKey = process.env.REWIND_MOCK_KEY;
        return createHandlers(
          createWorld({ now: Date.now(), live: true, requireKey }),
        ) as unknown[];
      };
      server.middlewares.use(async (req, res, next) => {
        if (!req.url?.startsWith("/v1/")) return next();
        try {
          handlers ??= load();
          const { getResponse } = await import("msw");
          const controller = new AbortController();
          req.on("close", () => controller.abort());
          const headers = new Headers();
          for (const [key, value] of Object.entries(req.headers)) {
            if (typeof value === "string") headers.set(key, value);
          }
          const request = new Request(`http://${req.headers.host ?? "localhost"}${req.url}`, {
            method: req.method ?? "GET",
            headers,
            signal: controller.signal,
          });
          const response = await getResponse((await handlers) as never, request);
          if (!response) return next();
          res.statusCode = response.status;
          response.headers.forEach((value, key) => {
            res.setHeader(key, value);
          });
          if (!response.body) return res.end();
          Readable.fromWeb(response.body as never).pipe(res);
        } catch (error) {
          next(error);
        }
      });
    },
  };
}
