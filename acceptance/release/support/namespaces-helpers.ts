/**
 * Helpers of the areas "Namespaces and tokens" and "API hardening": scripts, probes of a
 * namespace and of a run, and raw requests with repeated headers (which `fetch` merges).
 */
import { request as httpRequest } from "node:http";
import { eventually } from "../../support/processes.js";
import { type Api, expect, expectOk, type Reply } from "./gate.js";

/** Every permission a token may hold (`archi/06`, "What the caller may do", Tokens). */
export const TOKEN_ALL = [
  "runs:read",
  "runs:start",
  "runs:control",
  "runs:quarantine",
  "runs:delete",
  "actions:read",
  "secrets:read",
  "secrets:write",
  "secrets:use",
  "calls:work",
  "calls:approve",
];

/** What a token holds without `permissions`. */
export const TOKEN_DEFAULT = ["runs:read", "runs:start", "runs:control", "actions:read"];

/** A two-line script that returns at once; the marker makes it a script of its own. */
export function short(marker: string): string {
  return `// ${marker}\nexport default async function main() { return 1; }\n`;
}

/** A script that waits for the signal `approval` and returns its payload. */
export function waiting(marker: string): string {
  return `// ${marker}
import { signal } from "rewind";

export default async function main(input: unknown) {
  const decision = await signal<{ ok: boolean }>("approval");
  return { decision, input };
}
`;
}

/** Starts a run, asserting it was created; returns the run. */
export async function start(api: Api, body: Record<string, unknown>, headers: Record<string, string> = {}): Promise<any> {
  const reply = await api.post("/v1/runs", body, { headers });
  expectOk(reply);
  return reply.json;
}

/** Waits until a run has committed its first activation and waits for something. */
export async function untilWaiting(api: Api, id: string): Promise<any> {
  return eventually(
    `run ${id} waits`,
    async () => {
      const reply = await api.get(`/v1/runs/${id}`);
      return reply.json?.status === "active" && (reply.json.waitingFor?.length ?? 0) > 0 ? reply.json : undefined;
    },
    60_000,
    200,
  );
}

/** What a request could have changed in a namespace: whether it exists, its runs and tokens. */
export async function namespaceState(owner: Api, name: string): Promise<unknown> {
  const reply = await owner.get(`/v1/namespaces/${encodeURIComponent(name)}`);
  if (reply.status === 404) return { exists: false };
  expectOk(reply, 200);
  return {
    exists: true,
    runsStarted: reply.json.namespace.usage.runsStarted,
    activeRuns: reply.json.namespace.usage.activeRuns,
    tokens: reply.json.tokens,
    deleting: reply.json.namespace.deletingSince ?? null,
  };
}

/** What a request could have changed in a run: its status, its journal's length, its waits. */
export async function runState(owner: Api, id: string): Promise<unknown> {
  const reply = await owner.get(`/v1/runs/${id}`);
  expectOk(reply, 200);
  return {
    status: reply.json.status,
    historyLen: reply.json.historyLen,
    waiting: (reply.json.waitingFor ?? []).map((w: any) => `${w.kind}:${w.name ?? w.action ?? ""}:${w.state ?? ""}`),
    labels: reply.json.labels,
  };
}

/** Asserts no answer is a 5xx, with the request in the message. */
export function expectNo5xx(reply: Reply, what: string): void {
  expect(reply.status, `${what} → ${reply.status} ${reply.text.slice(0, 300)}`).toBeLessThan(500);
}

export interface RawReply {
  status: number;
  headers: Record<string, string | string[] | undefined>;
  text: string;
  json: any;
}

/**
 * A request with headers given as a list, so that one can be sent twice; `fetch` would join
 * them into one value. With `expectContinue`, the body is sent only once the server asks for it
 * (`Expect: 100-continue`): a server that refuses the request from its headers answers without
 * reading it, and the client never writes into a closed connection.
 */
export function rawRequest(
  url: string,
  method: string,
  path: string,
  headers: [string, string][],
  body?: string,
  options: { expectContinue?: boolean } = {},
): Promise<RawReply> {
  const target = new URL(url);
  return new Promise((resolve, reject) => {
    const flat: string[] = [];
    // A raw header list gets no Host from Node: send it, as every HTTP/1.1 client does (an edge
    // in front of the server refuses a request without one).
    if (!headers.some(([name]) => name.toLowerCase() === "host")) flat.push("host", target.host);
    for (const [name, value] of headers) flat.push(name, value);
    if (body !== undefined) flat.push("content-length", String(Buffer.byteLength(body)));
    if (options.expectContinue) flat.push("expect", "100-continue");
    // A connection of its own: a pooled one the server or the edge closed after its last answer
    // would fail the write (EPIPE) before this request is even sent.
    const req = httpRequest({ host: target.hostname, port: target.port, method, path, headers: flat as never, agent: false }, (res) => {
      const chunks: Buffer[] = [];
      res.on("data", (chunk: Buffer) => chunks.push(chunk));
      res.on("end", () => {
        const text = Buffer.concat(chunks).toString("utf8");
        let json: any;
        try {
          json = text === "" ? undefined : JSON.parse(text);
        } catch {
          json = undefined;
        }
        resolve({ status: res.statusCode ?? 0, headers: res.headers, text, json });
      });
    });
    req.on("error", reject);
    if (body !== undefined && options.expectContinue) {
      req.on("continue", () => req.end(body));
    } else {
      if (body !== undefined) req.write(body);
      req.end();
    }
  });
}
