/**
 * The outside world of the release gate, run in the spec's process on 127.0.0.1: an MCP server,
 * a receiver of lifecycle webhooks, a sink that counts whatever reaches it, a redirector, and the
 * keys that sign (or forge) end users' tokens. Each records what it received, so a check can prove
 * that an attack had no effect.
 */
import { createServer, type IncomingMessage, type Server as HttpServer, type ServerResponse } from "node:http";
import type { AddressInfo } from "node:net";
import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StreamableHTTPServerTransport } from "@modelcontextprotocol/sdk/server/streamableHttp.js";
import { CallToolRequestSchema, type CallToolResult, ListToolsRequestSchema } from "@modelcontextprotocol/sdk/types.js";
import { exportJWK, generateKeyPair, type JWK, SignJWT } from "jose";

async function listen(handler: (req: IncomingMessage, res: ServerResponse) => void): Promise<{ server: HttpServer; port: number }> {
  const server = createServer(handler);
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  return { server, port: (server.address() as AddressInfo).port };
}

function close(server: HttpServer): Promise<void> {
  server.closeAllConnections();
  return new Promise((resolve) => server.close(() => resolve()));
}

async function readBody(req: IncomingMessage): Promise<string> {
  const chunks: Buffer[] = [];
  for await (const chunk of req) chunks.push(chunk as Buffer);
  return Buffer.concat(chunks).toString("utf8");
}

function headersOf(req: IncomingMessage): Record<string, string> {
  return Object.fromEntries(Object.entries(req.headers).map(([k, v]) => [k, Array.isArray(v) ? v.join(", ") : String(v ?? "")]));
}

// ---------------------------------------------------------------------------------------------
// MCP

/** A tool of the fixture MCP server. */
export interface FixtureTool {
  name: string;
  description?: string;
  inputSchema: Record<string, unknown>;
  outputSchema?: Record<string, unknown>;
  handler: (args: any, call: McpCall) => Promise<CallToolResult> | CallToolResult;
}

/** A `tools/call` the fixture received. */
export interface McpCall {
  tool: string;
  args: any;
  /** `params._meta`: Rewind's idempotency key and trace context. */
  meta: Record<string, unknown> | undefined;
  /** The HTTP headers of the request: where credentials arrive. */
  headers: Record<string, string>;
  at: number;
}

export interface McpFixture {
  /** The streamable HTTP endpoint, `http://127.0.0.1:<port>/mcp`. */
  url: string;
  /** Every `tools/call`, in order. */
  calls: McpCall[];
  /** Every JSON-RPC method received (`initialize`, `tools/list`, `tools/call`…). */
  methods: string[];
  /** Replaces the tools: what a changed server would list. */
  setTools(tools: FixtureTool[]): void;
  close(): Promise<void>;
}

/** Tools most specs need: `echo`, `lookup` (structured), `fail` (a tool error), `slow`. */
export function defaultTools(): FixtureTool[] {
  return [
    {
      name: "echo",
      description: "Returns its text.",
      inputSchema: { type: "object", properties: { text: { type: "string" } }, required: ["text"] },
      handler: ({ text }) => ({ content: [{ type: "text", text: String(text) }] }),
    },
    {
      name: "lookup",
      description: "Finds a customer by id.",
      inputSchema: { type: "object", properties: { id: { type: "string" } }, required: ["id"] },
      outputSchema: {
        type: "object",
        properties: { id: { type: "string" }, name: { type: "string" }, tier: { type: "string" } },
        required: ["id", "name", "tier"],
      },
      handler: ({ id }) => {
        const value = { id: String(id), name: `Customer ${id}`, tier: "gold" };
        return { content: [{ type: "text", text: JSON.stringify(value) }], structuredContent: value };
      },
    },
    {
      name: "fail",
      description: "Always fails with a tool error.",
      inputSchema: { type: "object", properties: { reason: { type: "string" } } },
      handler: ({ reason }) => ({ isError: true, content: [{ type: "text", text: `failed: ${reason ?? "no reason"}` }] }),
    },
    {
      name: "slow",
      description: "Answers after `ms` milliseconds.",
      inputSchema: { type: "object", properties: { ms: { type: "number" } }, required: ["ms"] },
      handler: async ({ ms }) => {
        await new Promise((resolve) => setTimeout(resolve, Number(ms)));
        return { content: [{ type: "text", text: `waited ${ms}` }] };
      },
    },
  ];
}

/** An MCP server over streamable HTTP (stateless, JSON answers), on 127.0.0.1. */
export async function startMcpFixture(tools: FixtureTool[] = defaultTools()): Promise<McpFixture> {
  let current = tools;
  const calls: McpCall[] = [];
  const methods: string[] = [];
  const { server, port } = await listen(async (req, res) => {
    if (!req.url?.startsWith("/mcp")) {
      res.writeHead(404).end("not here");
      return;
    }
    if (req.method !== "POST") {
      res.writeHead(405, { allow: "POST" }).end();
      return;
    }
    const text = await readBody(req);
    let body: any;
    try {
      body = JSON.parse(text);
    } catch {
      res.writeHead(400).end("not JSON");
      return;
    }
    for (const message of Array.isArray(body) ? body : [body]) {
      if (typeof message?.method === "string") methods.push(message.method);
    }
    const headers = headersOf(req);
    const mcp = new Server({ name: "rewind-gate-fixture", version: "1.0.0" }, { capabilities: { tools: {} } });
    mcp.setRequestHandler(ListToolsRequestSchema, async () => ({
      tools: current.map((tool) => ({
        name: tool.name,
        description: tool.description,
        inputSchema: tool.inputSchema as never,
        ...(tool.outputSchema ? { outputSchema: tool.outputSchema as never } : {}),
      })),
    }));
    mcp.setRequestHandler(CallToolRequestSchema, async (request) => {
      const call: McpCall = {
        tool: request.params.name,
        args: request.params.arguments ?? {},
        meta: request.params._meta as Record<string, unknown> | undefined,
        headers,
        at: Date.now(),
      };
      calls.push(call);
      const tool = current.find((t) => t.name === request.params.name);
      if (!tool) return { isError: true, content: [{ type: "text", text: `no tool ${request.params.name}` }] };
      return tool.handler(call.args, call);
    });
    const transport = new StreamableHTTPServerTransport({ sessionIdGenerator: undefined, enableJsonResponse: true });
    res.on("close", () => {
      void transport.close();
      void mcp.close();
    });
    await mcp.connect(transport);
    await transport.handleRequest(req, res, body);
  });
  return {
    url: `http://127.0.0.1:${port}/mcp`,
    calls,
    methods,
    setTools(tools) {
      current = tools;
    },
    close: () => close(server),
  };
}

// ---------------------------------------------------------------------------------------------
// Webhooks

export interface Delivery {
  path: string;
  headers: Record<string, string>;
  body: string;
  at: number;
}

export interface WebhookReceiver {
  /** `http://127.0.0.1:<port>/hooks`. */
  url: string;
  deliveries: Delivery[];
  close(): Promise<void>;
}

/** Receives lifecycle webhooks and answers 204. */
export async function startWebhookReceiver(): Promise<WebhookReceiver> {
  const deliveries: Delivery[] = [];
  const { server, port } = await listen(async (req, res) => {
    deliveries.push({ path: req.url ?? "", headers: headersOf(req), body: await readBody(req), at: Date.now() });
    res.writeHead(204).end();
  });
  return { url: `http://127.0.0.1:${port}/hooks`, deliveries, close: () => close(server) };
}

// ---------------------------------------------------------------------------------------------
// The sink and the redirector

export interface Sink {
  /** `http://127.0.0.1:<port>`. */
  origin: string;
  port: number;
  /** Every request that reached it. */
  hits: { method: string; path: string; headers: Record<string, string> }[];
  close(): Promise<void>;
}

/** A server that only counts: the internal service an SSRF would reach. */
export async function startSink(): Promise<Sink> {
  const hits: Sink["hits"] = [];
  const { server, port } = await listen(async (req, res) => {
    hits.push({ method: req.method ?? "", path: req.url ?? "", headers: headersOf(req) });
    await readBody(req);
    res.writeHead(200, { "content-type": "text/plain" }).end("internal secret page");
  });
  return { origin: `http://127.0.0.1:${port}`, port, hits, close: () => close(server) };
}

export interface Redirector {
  origin: string;
  /** A URL that answers `status` with `Location: to`. */
  to(target: string, status?: 301 | 302 | 303 | 307 | 308): string;
  hits: string[];
  close(): Promise<void>;
}

/** Answers every request with a redirect to the `to` of its query. */
export async function startRedirector(): Promise<Redirector> {
  const hits: string[] = [];
  const { server, port } = await listen(async (req, res) => {
    hits.push(req.url ?? "");
    await readBody(req);
    const url = new URL(req.url ?? "/", "http://x");
    const to = url.searchParams.get("to") ?? "http://127.0.0.1:1/";
    const status = Number(url.searchParams.get("status") ?? "302");
    res.writeHead(status, { location: to }).end();
  });
  const origin = `http://127.0.0.1:${port}`;
  return {
    origin,
    to: (target, status = 302) => `${origin}/r?to=${encodeURIComponent(target)}&status=${status}`,
    hits,
    close: () => close(server),
  };
}

// ---------------------------------------------------------------------------------------------
// End users' tokens signed by the customer

export type TokenAlg = "EdDSA" | "ES256" | "RS256";

export interface SigningKey {
  alg: TokenAlg;
  /** The public key as `POST /v1/organization/token-keys` takes it. */
  publicJwk: JWK;
  /** The `kid` Rewind answered at registration (`rk_…`), once set. */
  kid?: string;
  sign(claims: TokenClaims, header?: Record<string, unknown>): Promise<string>;
}

export interface TokenClaims {
  sub?: string;
  aud?: string | string[];
  iat?: number;
  exp?: number;
  permissions?: string[];
  labels?: Record<string, string>;
  jti?: string;
  [claim: string]: unknown;
}

/** A key pair the customer signs tokens with. */
export async function signingKey(alg: TokenAlg): Promise<SigningKey> {
  const { privateKey, publicKey } = await generateKeyPair(alg, alg === "EdDSA" ? { crv: "Ed25519", extractable: true } : { extractable: true });
  const publicJwk = await exportJWK(publicKey);
  const key: SigningKey = {
    alg,
    publicJwk,
    async sign(claims, header = {}) {
      const now = Math.floor(Date.now() / 1000);
      const body: TokenClaims = { aud: "rewind", iat: now, exp: now + 600, ...claims };
      return new SignJWT(body as never).setProtectedHeader({ alg, typ: "JWT", kid: key.kid, ...header } as never).sign(privateKey);
    },
  };
  return key;
}

function b64url(value: string | Buffer): string {
  return Buffer.from(value).toString("base64url");
}

/** A JWT with `alg: none` and no signature. */
export function unsignedToken(claims: TokenClaims, kid: string): string {
  const now = Math.floor(Date.now() / 1000);
  const header = b64url(JSON.stringify({ alg: "none", typ: "JWT", kid }));
  const body = b64url(JSON.stringify({ aud: "rewind", iat: now, exp: now + 600, ...claims }));
  return `${header}.${body}.`;
}

/**
 * A JWT signed with HS256 whose secret is the registered public key's JSON: the confusion of a
 * verifier that takes the key's bytes as an HMAC secret.
 */
export async function hmacConfusedToken(claims: TokenClaims, kid: string, publicJwk: JWK): Promise<string> {
  const { createHmac } = await import("node:crypto");
  const now = Math.floor(Date.now() / 1000);
  const header = b64url(JSON.stringify({ alg: "HS256", typ: "JWT", kid }));
  const body = b64url(JSON.stringify({ aud: "rewind", iat: now, exp: now + 600, ...claims }));
  const secret = JSON.stringify(publicJwk);
  const signature = createHmac("sha256", secret).update(`${header}.${body}`).digest("base64url");
  return `${header}.${body}.${signature}`;
}
