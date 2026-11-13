/**
 * A mocked Rewind server: a real HTTP server on a free local port, with handlers per route,
 * recording every request it gets.
 */
import { createServer, type IncomingHttpHeaders, type Server, type ServerResponse } from "node:http";
import type { AddressInfo } from "node:net";

import type { ProblemSlug } from "../src/errors.js";

export interface MockRequest {
  method: string;
  /** The path, without the query string. */
  path: string;
  query: URLSearchParams;
  headers: IncomingHttpHeaders;
  /** The path parameters of the route, decoded. */
  params: Record<string, string>;
  /** The body as JSON, or undefined when empty. */
  body: unknown;
  rawBody: string;
  /** Aborted when the client goes away. */
  signal: AbortSignal;
}

export interface JsonReply {
  status?: number;
  body?: unknown;
  headers?: Record<string, string>;
}

/** A reply: JSON, or a function writing the response itself (a stream). */
export type MockReply = JsonReply | ((response: ServerResponse) => void | Promise<void>);
export type MockHandler = (request: MockRequest) => MockReply | Promise<MockReply>;

interface Route {
  method: string;
  pattern: RegExp;
  names: string[];
  handler: MockHandler;
}

export class MockServer {
  readonly requests: MockRequest[] = [];
  private readonly routes: Route[] = [];

  private constructor(
    private readonly server: Server,
    readonly url: string,
  ) {}

  static async start(): Promise<MockServer> {
    let mock: MockServer | undefined;
    const server = createServer((request, response) => {
      void mock?.dispatch(request, response);
    });
    await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
    const { port } = server.address() as AddressInfo;
    mock = new MockServer(server, `http://127.0.0.1:${port}`);
    return mock;
  }

  /** Answers `method pattern` (such as `GET /v1/runs/{id}`). The last route declared wins. */
  on(method: string, path: string, handler: MockHandler | JsonReply): this {
    const names: string[] = [];
    const source = path.replace(/[.*+?^$()|[\]\\]/g, "\\$&").replace(/\{(\w+)\}/g, (_, name: string) => {
      names.push(name);
      return "([^/]+)";
    });
    this.routes.unshift({
      method,
      pattern: new RegExp(`^${source}$`),
      names,
      handler: typeof handler === "function" ? handler : () => handler,
    });
    return this;
  }

  /** The requests to `method path`, path parameters included. */
  calls(method: string, path: string): MockRequest[] {
    return this.requests.filter((request) => request.method === method && request.path === path);
  }

  async close(): Promise<void> {
    this.server.closeAllConnections();
    await new Promise<void>((resolve) => this.server.close(() => resolve()));
  }

  private async dispatch(incoming: import("node:http").IncomingMessage, response: ServerResponse): Promise<void> {
    const url = new URL(incoming.url ?? "/", this.url);
    const chunks: Buffer[] = [];
    for await (const chunk of incoming) {
      chunks.push(chunk as Buffer);
    }
    const rawBody = Buffer.concat(chunks).toString("utf8");
    const controller = new AbortController();
    response.on("close", () => controller.abort());
    const request: MockRequest = {
      method: incoming.method ?? "GET",
      path: url.pathname,
      query: url.searchParams,
      headers: incoming.headers,
      params: {},
      body: rawBody === "" ? undefined : JSON.parse(rawBody),
      rawBody,
      signal: controller.signal,
    };
    this.requests.push(request);
    const route = this.routes.find((r) => r.method === request.method && r.pattern.test(request.path));
    if (route === undefined) {
      writeJson(response, problem(404, "not-found", `no mock for ${request.method} ${request.path}`));
      return;
    }
    const match = route.pattern.exec(request.path);
    route.names.forEach((name, index) => {
      request.params[name] = decodeURIComponent(match?.[index + 1] ?? "");
    });
    try {
      const reply = await route.handler(request);
      if (typeof reply === "function") {
        await reply(response);
      } else {
        writeJson(response, reply);
      }
    } catch (error) {
      writeJson(response, problem(500, "internal", `the mock failed: ${String(error)}`));
    }
  }
}

/** A reply writing server-sent events. */
export function sse(
  events: Array<{ event?: string; id?: string; data: unknown }>,
  options: { end?: boolean } = {},
): MockHandler {
  return () => {
    const write = (response: ServerResponse) => {
      response.writeHead(200, { "Content-Type": "text/event-stream", "Cache-Control": "no-cache" });
      response.flushHeaders();
      for (const event of events) {
        if (event.event !== undefined) {
          response.write(`event: ${event.event}\n`);
        }
        if (event.id !== undefined) {
          response.write(`id: ${event.id}\n`);
        }
        response.write(`data: ${JSON.stringify(event.data)}\n\n`);
      }
      if (options.end ?? true) {
        response.end();
      } else {
        // The connection drops once what was written has left.
        setTimeout(() => response.destroy(), 20);
      }
    };
    return write;
  };
}

function writeJson(response: ServerResponse, reply: JsonReply): void {
  const status = reply.status ?? 200;
  const headers: Record<string, string> = { ...reply.headers };
  if (reply.body === undefined) {
    response.writeHead(status, headers);
    response.end();
    return;
  }
  const text = JSON.stringify(reply.body);
  headers["Content-Type"] ??= status >= 400 ? "application/problem+json" : "application/json";
  response.writeHead(status, headers);
  response.end(text);
}

/** An error reply, as the server writes them. */
export function problem(
  status: number,
  slug: ProblemSlug,
  detail: string,
  extra: Record<string, unknown> = {},
): JsonReply {
  return {
    status,
    body: { type: `urn:rewind:problem:${slug}`, title: slug, status, detail, ...extra },
  };
}

export function json(body: unknown, status = 200): JsonReply {
  return { status, body };
}

export const noContent: JsonReply = { status: 204 };
