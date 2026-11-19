// `pnpm dev:mock`: the Vite dev server answers /v1 and /auth with the mocked API, so the app runs
// unchanged, sign-in and event stream included, with no server and no service worker in the
// build. The codes and invitation links a deployment would email are printed here.
//
// - by default, members sign in as with WorkOS (a code by email, or GitHub simulated);
// - REWIND_MOCK_KEY=rewind_sk_… asks for that key instead, as a server without WorkOS;
// - REWIND_MOCK_WORKOS=0 needs neither, as the embedded server.
import { Readable } from "node:stream";
import type { Plugin } from "vite";

const ROUTES = ["/v1/", "/auth/"];

const ACCOUNTS = `
[mock] Sign in with one of these emails; the code is printed here.
[mock]   ada@example.net       owner of Acme, reader in Globex (chooses an organization)
[mock]   grace@example.net     harness in Acme
[mock]   linus@example.net     auditor in Acme: members, roles and keys, no runs
[mock]   alan@example.net      reader in Acme limited to agent:triage; verifies his email first
[mock]   margaret@example.net  no organization yet, invited to Globex
[mock]   any other email       sign-ups are closed: the waitlist
[mock] "Continue with GitHub" signs in as ada@example.net.
[mock] Namespaces: bob is above his CPU quota, chen pays for more, erin is blocked.
[mock] Connections: Ada connected Claude (Acme, Globex) and Claude Code (Acme), Grace Claude (Acme).`;

export function mockApi(): Plugin {
  return {
    name: "rewind-mock-api",
    apply: "serve",
    configureServer(server) {
      let handlers: Promise<unknown[]> | undefined;
      const log = (line: string) => server.config.logger.info(line, { timestamp: true });
      const load = async () => {
        const [{ createHandlers }, { createWorld }] = await Promise.all([
          server.ssrLoadModule("/src/mocks/handlers.ts"),
          server.ssrLoadModule("/src/mocks/fixtures.ts"),
        ]);
        const requireKey = process.env.REWIND_MOCK_KEY;
        const workos = !requireKey && process.env.REWIND_MOCK_WORKOS !== "0";
        const world = createWorld({ now: Date.now(), live: true, requireKey, workos, log });
        if (workos) {
          log(ACCOUNTS);
          for (const inv of world.access.invitations.filter(
            (i: { state: string }) => i.state === "pending",
          ))
            log(`[mock] invitation for ${inv.email}: /?invitation_token=${inv.token}`);
          for (const id of world.connections.requests.keys())
            log(`[mock] Claude asks for access: /#/connect/authorize?request=${id}`);
        }
        return createHandlers(world) as unknown[];
      };
      server.middlewares.use(async (req, res, next) => {
        if (!ROUTES.some((route) => req.url?.startsWith(route))) return next();
        try {
          handlers ??= load();
          const { getResponse } = await import("msw");
          const controller = new AbortController();
          res.on("close", () => controller.abort());
          const headers = new Headers();
          for (const [key, value] of Object.entries(req.headers)) {
            if (typeof value === "string") headers.set(key, value);
          }
          const method = req.method ?? "GET";
          let body: Buffer | undefined;
          if (method !== "GET" && method !== "HEAD") {
            const chunks: Buffer[] = [];
            for await (const chunk of req) chunks.push(chunk as Buffer);
            body = Buffer.concat(chunks);
          }
          const request = new Request(`http://${req.headers.host ?? "localhost"}${req.url}`, {
            method,
            headers,
            signal: controller.signal,
            ...(body && body.length > 0 ? { body: new Uint8Array(body) } : {}),
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
