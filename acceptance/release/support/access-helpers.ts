/**
 * Helpers of the access and tenancy areas of the release gate: keys with allowed labels, end
 * users' opaque tokens, runs that end at once or wait for a signal, and the permission catalog of
 * `archi/06-storage-and-scale.md` ("What the caller may do") as the specification writes it.
 */
import { expect } from "@playwright/test";
import { type Api, expectOk, type Reply, type Tenant } from "./gate.js";

/** The catalog, in 06's order. */
export const PERMISSIONS = [
  "runs:read",
  "runs:start",
  "runs:control",
  "runs:quarantine",
  "runs:delete",
  "toolsets:read",
  "toolsets:write",
  "scripts:write",
  "secrets:read",
  "secrets:write",
  "secrets:use",
  "tasks:work",
  "tasks:approve",
  "subscriptions:read",
  "subscriptions:write",
  "members:read",
  "members:invite",
  "members:manage",
  "roles:read",
  "roles:write",
  "api_keys:read",
  "api_keys:write",
  "organization:write",
  "namespaces:read",
  "namespaces:write",
  "tokens:write",
] as const;

/** 06's table of the built-in roles. */
export const BUILT_IN: Record<string, readonly string[]> = {
  owner: PERMISSIONS,
  harness: [
    "runs:read",
    "runs:start",
    "runs:control",
    "runs:quarantine",
    "runs:delete",
    "toolsets:read",
    "toolsets:write",
    "scripts:write",
    "secrets:read",
    "secrets:write",
    "secrets:use",
    "tasks:work",
    "tasks:approve",
    "subscriptions:read",
    "subscriptions:write",
    "members:read",
    "roles:read",
    "namespaces:read",
    "namespaces:write",
    "tokens:write",
  ],
  worker: ["tasks:work"],
  reader: ["runs:read", "toolsets:read", "secrets:read", "subscriptions:read", "members:read", "roles:read", "namespaces:read"],
};

/** What a grant limited to labels may not use (06: "Labels"). */
export const ADMINISTRATION = ["members:invite", "members:manage", "roles:write", "api_keys:read", "api_keys:write", "organization:write"];

/** The permissions of a token minted without `permissions` (06: "Tokens"). */
export const TOKEN_DEFAULT = ["runs:read", "runs:start", "runs:control", "toolsets:read"];

/** A run that returns its input's `n` doubled at once. */
export const QUICK_SCRIPT = `export default async function main(input: { n: number }) {
  return { doubled: input.n * 2 };
}
`;

/** A run that waits for the signal `go` and returns its payload's `n`. */
export const WAITING_SCRIPT = `import { signal } from "rewind";

export default async function main(input: { note: string }) {
  const go = await signal<{ n: number }>("go");
  return { note: input.note, n: go.n };
}
`;

/** A key of the tenant with `role`, limited to `allowedLabels` when given. */
export async function createKey(
  api: Api,
  role: string,
  options: { name?: string; allowedLabels?: Record<string, string> } = {},
): Promise<{ key: string; id: string }> {
  const reply = await api.post("/v1/api-keys", { role, name: options.name ?? `${role} key`, allowedLabels: options.allowedLabels });
  expectOk(reply, 201);
  return { key: reply.json.key as string, id: reply.json.id as string };
}

/** An end user's opaque token for `namespace`, minted by `api`. */
export async function mintToken(
  api: Api,
  namespace: string,
  body: { permissions?: string[]; labels?: Record<string, string>; ttl?: string } = {},
): Promise<{ token: string; id: string }> {
  const reply = await api.post(`/v1/namespaces/${encodeURIComponent(namespace)}/tokens`, body);
  expectOk(reply, 201);
  return { token: reply.json.token as string, id: reply.json.id as string };
}

/** Starts a run in `namespace` and returns its view (waits up to `wait` for it to end). */
export async function startRun(
  api: Api,
  namespace: string,
  body: { script?: string; input?: unknown; labels?: Record<string, string>; wait?: string; toolset?: string },
  headers: Record<string, string> = {},
): Promise<Reply> {
  return api.ns(namespace).post(
    "/v1/runs",
    { script: body.script ?? QUICK_SCRIPT, input: body.input ?? { n: 1 }, labels: body.labels, wait: body.wait, toolset: body.toolset },
    { headers },
  );
}

/** A run that completed, for reads. */
export async function completedRun(tenant: Tenant, namespace: string, labels?: Record<string, string>, n = 1): Promise<any> {
  const reply = await startRun(tenant.api, namespace, { input: { n }, labels, wait: "30s" });
  expectOk(reply);
  expect(reply.json.status, reply.text).toBe("completed");
  return reply.json;
}

/** A run waiting for the signal `go`, once it waits. */
export async function waitingRun(tenant: Tenant, namespace: string, note = "waiting", labels?: Record<string, string>): Promise<any> {
  const reply = await startRun(tenant.api, namespace, { script: WAITING_SCRIPT, input: { note }, labels });
  expectOk(reply);
  const id = reply.json.id as string;
  for (let attempt = 0; attempt < 120; attempt++) {
    const state = await tenant.api.get(`/v1/runs/${id}`);
    if (state.json?.waitingFor?.some((w: { kind: string; name?: string }) => w.kind === "signal" && w.name === "go")) return state.json;
    await new Promise((resolve) => setTimeout(resolve, 250));
  }
  throw new Error(`run ${id} never waited for its signal`);
}

/** What a victim's run looks like, for probes: its status and the length of its journal. */
export async function runProbe(api: Api, id: string): Promise<{ status: string; entries: number; signals: number }> {
  const state = await api.get(`/v1/runs/${id}`);
  expectOk(state, 200);
  const journal = await api.get(`/v1/runs/${id}/journal`);
  expectOk(journal, 200);
  const entries = (journal.json.entries ?? []) as { event: { type: string } }[];
  return {
    status: state.json.status as string,
    entries: entries.length,
    signals: entries.filter((e) => e.event.type.includes("signal")).length,
  };
}
