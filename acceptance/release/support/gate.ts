/**
 * What every spec of the release gate imports: `test` and `expect`, the setups' addresses, an
 * HTTP client that records every exchange, tenants made with the operator key, and the helpers
 * of the three kinds of check (`acceptance/release/README.md`).
 *
 * - Each test carries exactly one tag: `@success`, `@failure` or `@attack`.
 * - Every exchange with Rewind, through `Api` or an SDK client made here, is recorded: in the
 *   test's evidence (an attachment the report shows) and in `exchanges/*.jsonl`, which the sweeps
 *   read to fail the gate on a 5xx nobody expected, or on the canary secret in an answer.
 */
import { randomBytes } from "node:crypto";
import { appendFileSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { Rewind } from "@rewind.codes/sdk";
import { test as base, expect } from "@playwright/test";
import { acceptance } from "../../support/paths.js";
import { eventually } from "../../support/processes.js";
import type { StackInfo } from "./stack.js";

export { expect };

/** Where the gate writes its results. */
export const gateResults = process.env.REWIND_GATE_RESULTS ?? join(acceptance, "results", "release");

/** The setups the global setup started, and the run's canary secret. */
export interface GateStacks {
  /** A: deployed, the area specs' shared setup. */
  a: StackInfo;
  /** B: strict, a deployed server's defaults. */
  b: StackInfo;
  /**
   * A value only this run of the gate knows. Specs put it as secrets (`PUT /v1/secrets/{name}`,
   * `PUT /v1/namespaces/{name}/secrets/{secret}`) that their runs attach, and an MCP server or an
   * API receives it; it must never come back in an answer of Rewind (a run, a journal, a problem),
   * a trace, a log, the database in clear, or the dashboard.
   */
  canary: string;
}

let loaded: GateStacks | undefined;

/** The setups of this run (`REWIND_GATE_STACKS`, written by the global setup). */
export function stacks(): GateStacks {
  if (loaded) return loaded;
  const file = process.env.REWIND_GATE_STACKS;
  if (!file) {
    throw new Error("REWIND_GATE_STACKS is not set: run the areas through acceptance/release.sh, or with the release config");
  }
  loaded = JSON.parse(readFileSync(file, "utf8")) as GateStacks;
  return loaded;
}

let ownCanary: string | undefined;

/**
 * The canary secret of this run: the global setup's, or, in a stage that runs without setups A
 * and B (the lifecycle), one made for this process and published as `REWIND_GATE_CANARY` so the
 * sweeps' scan of logs can find it in `stacks-<pid>.json`.
 */
export function canary(): string {
  if (process.env.REWIND_GATE_CANARY) return process.env.REWIND_GATE_CANARY;
  if (process.env.REWIND_GATE_STACKS) return stacks().canary;
  if (ownCanary === undefined) {
    ownCanary = `REWIND-CANARY-${randomBytes(12).toString("hex")}`;
    process.env.REWIND_GATE_CANARY = ownCanary;
    try {
      mkdirSync(gateResults, { recursive: true });
      writeFileSync(join(gateResults, `stacks-${process.pid}.json`), JSON.stringify({ canary: ownCanary }));
    } catch {
      // The sweeps then miss this canary in the logs; answers are still checked as they come.
    }
  }
  return ownCanary;
}

// ---------------------------------------------------------------------------------------------
// Recording

/** One request to Rewind and its answer, as the report shows it. */
export interface Exchange {
  at: string;
  file?: string;
  test?: string;
  method: string;
  url: string;
  /** The `Authorization` header's kind, never its value: `key`, `token`, `jwt`, `none`, `other`. */
  auth: string;
  requestBody?: string;
  status: number;
  /** The problem's slug, for `application/problem+json`. */
  problem?: string;
  responseBody?: string;
  ms: number;
  /** A 5xx the test said it expects. */
  expected5xx?: boolean;
  /** The canary secret was in the answer. */
  canaryLeak?: boolean;
}

const BODY_LIMIT = 2048;

interface TestState {
  exchanges: Exchange[];
  allowed5xx: Set<string>;
}

let current: TestState | undefined;

function exchangesFile(): string {
  const dir = join(gateResults, "exchanges");
  mkdirSync(dir, { recursive: true });
  return join(dir, `${process.pid}.jsonl`);
}

function authKind(headers: Headers): string {
  const value = headers.get("authorization");
  if (value === null) return "none";
  const token = value.replace(/^Bearer\s+/i, "");
  if (token.startsWith("rewind_sk_")) return "key";
  if (token.startsWith("rewind_nt_")) return "token";
  if (token.split(".").length === 3) return "jwt";
  return "other";
}

function truncate(text: string): string {
  return text.length > BODY_LIMIT ? `${text.slice(0, BODY_LIMIT)}… (${text.length} bytes)` : text;
}

function where(): { file?: string; test?: string } {
  try {
    const info = base.info();
    return { file: info.file.split("/").pop(), test: info.titlePath.slice(1).join(" › ") };
  } catch {
    return {};
  }
}

/** Records one exchange; never throws. */
async function record(request: Request, requestBody: string | undefined, response: Response, started: number): Promise<void> {
  const type = response.headers.get("content-type") ?? "";
  let text = "";
  // An event stream may never end: its body is not read.
  if (!type.includes("text/event-stream")) {
    try {
      text = await response.clone().text();
    } catch {
      text = "";
    }
  }
  let problem: string | undefined;
  if (type.includes("problem+json")) {
    try {
      problem = String((JSON.parse(text) as { type?: string }).type ?? "").replace("urn:rewind:problem:", "");
    } catch {
      problem = "(unreadable)";
    }
  }
  const secret = canary();
  const exchange: Exchange = {
    at: new Date(started).toISOString(),
    ...where(),
    method: request.method,
    url: request.url.replace(/^https?:\/\/[^/]+/, ""),
    auth: authKind(request.headers),
    requestBody: requestBody === undefined ? undefined : truncate(requestBody.replaceAll(secret, "<canary>")),
    status: response.status,
    problem,
    responseBody: truncate(text.replaceAll(secret, "<canary>")),
    ms: Date.now() - started,
  };
  if (response.status >= 500) {
    exchange.expected5xx = current !== undefined && (current.allowed5xx.has("*") || current.allowed5xx.has(problem ?? ""));
  }
  if (text.includes(secret)) exchange.canaryLeak = true;
  current?.exchanges.push(exchange);
  try {
    appendFileSync(exchangesFile(), `${JSON.stringify(exchange)}\n`);
  } catch {
    // The report loses one line; the test goes on.
  }
}

/** A `fetch` that records each exchange; give it to SDK clients (`new Rewind({ fetch })`). */
export const recordingFetch: typeof fetch = async (input, init) => {
  const request = new Request(input, init);
  let body: string | undefined;
  if (request.body !== null) {
    body = await request.clone().text();
  }
  const started = Date.now();
  const response = await fetch(request);
  await record(request, body, response, started);
  return response;
};

/**
 * Lets a 5xx with this problem slug through the sweep, for this test only (`*` for any): a
 * check that expects `toolset-unavailable` (502) or an outage's `unavailable` (503).
 */
export function allow5xx(slug: string): void {
  current?.allowed5xx.add(slug);
}

// ---------------------------------------------------------------------------------------------
// The raw HTTP client

/** An answer of Rewind, read whole. */
export interface Reply {
  status: number;
  headers: Headers;
  text: string;
  /** The parsed body, when it is JSON. */
  json: any;
  /** For `application/problem+json`: the problem's slug. */
  slug?: string;
}

export interface RequestOptions {
  /** A JSON body. */
  json?: unknown;
  /** A raw body, sent as is (with `contentType`). */
  body?: string | Uint8Array;
  contentType?: string;
  headers?: Record<string, string>;
  /** `?key=value`, keys repeated for arrays. */
  query?: Record<string, string | number | boolean | (string | number)[] | undefined>;
}

/**
 * Rewind's HTTP API as a hostile or careless caller sees it: any method, path, header and body,
 * every answer returned (never thrown) and recorded.
 */
export class Api {
  constructor(
    readonly url: string,
    readonly bearer?: string,
    readonly defaultHeaders: Record<string, string> = {},
  ) {}

  /** The same client with another Bearer (`undefined`: none). */
  as(bearer: string | undefined): Api {
    return new Api(this.url, bearer, this.defaultHeaders);
  }

  /** The same client naming a namespace (`Rewind-Namespace`). */
  ns(name: string): Api {
    return new Api(this.url, this.bearer, { ...this.defaultHeaders, "rewind-namespace": name });
  }

  /** The same client with more headers. */
  with(headers: Record<string, string>): Api {
    return new Api(this.url, this.bearer, { ...this.defaultHeaders, ...headers });
  }

  async request(method: string, path: string, options: RequestOptions = {}): Promise<Reply> {
    const headers = new Headers(this.defaultHeaders);
    if (this.bearer !== undefined) headers.set("authorization", `Bearer ${this.bearer}`);
    for (const [name, value] of Object.entries(options.headers ?? {})) headers.set(name, value);
    let body: string | Uint8Array | undefined;
    if (options.json !== undefined) {
      body = JSON.stringify(options.json);
      if (!headers.has("content-type")) headers.set("content-type", "application/json");
    } else if (options.body !== undefined) {
      body = options.body;
      if (options.contentType) headers.set("content-type", options.contentType);
    }
    let target = `${this.url}${path}`;
    if (options.query) {
      const params = new URLSearchParams();
      for (const [key, value] of Object.entries(options.query)) {
        if (value === undefined) continue;
        for (const item of Array.isArray(value) ? value : [value]) params.append(key, String(item));
      }
      const qs = params.toString();
      if (qs !== "") target += `${target.includes("?") ? "&" : "?"}${qs}`;
    }
    const response = await recordingFetch(target, { method, headers, body: body as BodyInit | undefined });
    const text = await response.text();
    let json: any;
    try {
      json = text === "" ? undefined : JSON.parse(text);
    } catch {
      json = undefined;
    }
    const type = response.headers.get("content-type") ?? "";
    const slug = type.includes("problem+json") && json?.type ? String(json.type).replace("urn:rewind:problem:", "") : undefined;
    return { status: response.status, headers: response.headers, text, json, slug };
  }

  get(path: string, options?: RequestOptions) {
    return this.request("GET", path, options);
  }
  post(path: string, json?: unknown, options: RequestOptions = {}) {
    return this.request("POST", path, json === undefined ? options : { ...options, json });
  }
  put(path: string, json?: unknown, options: RequestOptions = {}) {
    return this.request("PUT", path, json === undefined ? options : { ...options, json });
  }
  patch(path: string, json?: unknown, options: RequestOptions = {}) {
    return this.request("PATCH", path, json === undefined ? options : { ...options, json });
  }
  delete(path: string, options?: RequestOptions) {
    return this.request("DELETE", path, options);
  }
}

/** Asserts an answer is the problem `slug` with `status`, as `api/README.md` lists them. */
export function expectProblem(reply: Reply, status: number, slug: string): void {
  const context = `${reply.status} ${reply.text.slice(0, 600)}`;
  expect(reply.status, context).toBe(status);
  expect(reply.headers.get("content-type") ?? "", context).toContain("application/problem+json");
  expect(reply.slug, context).toBe(slug);
  expect(typeof reply.json?.detail === "string" && reply.json.detail.length > 0, `a problem has a detail: ${context}`).toBe(true);
}

/** Asserts a 2xx answer, with the body in the failure's message. */
export function expectOk(reply: Reply, status?: number): void {
  const context = `${reply.status} ${reply.text.slice(0, 600)}`;
  if (status === undefined) {
    expect(reply.status >= 200 && reply.status < 300, context).toBe(true);
  } else {
    expect(reply.status, context).toBe(status);
  }
}

/**
 * The attack pattern: probe the state, attempt, assert the exact refusal, probe again and assert
 * nothing moved. `probe` reads whatever the attempt would have changed (a run's journal, a
 * fixture's call log, a list of keys).
 */
export async function expectRefused<T>(options: {
  attempt: () => Promise<Reply>;
  status: number;
  slug: string;
  probe?: () => Promise<T> | T;
}): Promise<Reply> {
  const before = options.probe ? await options.probe() : undefined;
  const reply = await options.attempt();
  expectProblem(reply, options.status, options.slug);
  if (options.probe) {
    const after = await options.probe();
    expect(after as unknown, "the refused request changed nothing").toEqual(before);
  }
  return reply;
}

// ---------------------------------------------------------------------------------------------
// Tenants

/** A tenant made for a spec with the operator key: its owner key and clients. */
export interface Tenant {
  id: string;
  /** The `owner` key's secret. */
  ownerKey: string;
  /** Raw HTTP as the owner. */
  api: Api;
  /** The SDK as the owner, recorded. */
  rewind: Rewind;
  /** Another key of the tenant: its secret and id. */
  key(role: string, options?: { name?: string; labels?: Record<string, string> }): Promise<{ key: string; id: string }>;
  /** An SDK client with this bearer (a key or a token), recorded. */
  client(bearer: { apiKey?: string; token?: string }): Rewind;
}

/** The operator of a setup, as raw HTTP. */
export function operator(stack: StackInfo = stacks().a): Api {
  return new Api(stack.url, stack.operatorKey);
}

/**
 * A new tenant on a setup (A by default), named after the spec, with an `owner` key made by the
 * operator: organizations are not created through WorkOS in the gate.
 */
export async function newTenant(prefix: string, stack: StackInfo = stacks().a): Promise<Tenant> {
  const id = `${prefix.replace(/[^A-Za-z0-9_-]/g, "-").slice(0, 40)}-${randomBytes(4).toString("hex")}`;
  const created = await operator(stack).post("/v1/api-keys", { tenant: id, role: "owner", name: `${prefix} owner` });
  expectOk(created, 201);
  const ownerKey: string = created.json.key;
  const client = (bearer: { apiKey?: string; token?: string }) =>
    new Rewind({ url: stack.url, apiKey: bearer.apiKey, token: bearer.token, fetch: recordingFetch } as never);
  const api = new Api(stack.url, ownerKey);
  return {
    id,
    ownerKey,
    api,
    rewind: client({ apiKey: ownerKey }),
    async key(role, options = {}) {
      const reply = await api.post("/v1/api-keys", { role, name: options.name ?? `${role} key`, allowedLabels: options.labels });
      expectOk(reply, 201);
      return { key: reply.json.key as string, id: reply.json.id as string };
    },
    client,
  };
}

/**
 * The canary run: a trivial script that must complete on the setup, after an attack meant to
 * take a worker down. Uses the tenant's owner key and a namespace of its own.
 */
export async function expectHealthy(tenant: Tenant, namespace = "canary"): Promise<void> {
  const started = await tenant.api.ns(namespace).post("/v1/runs", {
    script: `export default async function main(input: { n: number }) { return input.n * 2; }\n`,
    input: { n: 21 },
    wait: "30s",
  });
  expectOk(started);
  let run = started.json;
  if (run.status !== "completed") {
    run = await eventually(
      "the canary run completes",
      async () => {
        const reply = await tenant.api.get(`/v1/runs/${run.id}`, { query: { wait: "10s" } });
        return reply.json?.status === "completed" ? reply.json : undefined;
      },
      60_000,
    );
  }
  expect(run.output?.value, JSON.stringify(run)).toBe(42);
}

// ---------------------------------------------------------------------------------------------
// The test

const KINDS = ["@success", "@failure", "@attack"] as const;
export type Kind = (typeof KINDS)[number];

/**
 * Playwright's `test`, with the gate's bookkeeping: the kind tag is checked, the exchanges of
 * the test are attached as its evidence, and its allowed 5xx are reset.
 */
export const test = base.extend<{ _gate: void }>({
  _gate: [
    async ({}, use, info) => {
      const kinds = info.tags.filter((tag) => (KINDS as readonly string[]).includes(tag));
      if (kinds.length !== 1) {
        throw new Error(`a check of the gate carries exactly one of ${KINDS.join(", ")}; it has ${info.tags.join(", ") || "none"}`);
      }
      current = { exchanges: [], allowed5xx: new Set() };
      try {
        await use();
      } finally {
        const state = current;
        current = undefined;
        if (state.exchanges.length > 0) {
          await info.attach("exchanges", {
            body: JSON.stringify(state.exchanges.slice(-200), null, 1),
            contentType: "application/json",
          });
        }
      }
    },
    { auto: true },
  ],
});

/**
 * A note the report shows under the check: what was tried and what the platform answered, in
 * words. For attacks, say the defense observed.
 */
export function evidence(text: string): void {
  base.info().annotations.push({ type: "evidence", description: text });
}

/** A short unique suffix, for names within a tenant. */
export function unique(prefix = ""): string {
  return `${prefix}${randomBytes(4).toString("hex")}`;
}
