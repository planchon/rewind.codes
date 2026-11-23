// The mocked secrets endpoints of `/v1` and the signing keys (archi/07, "HTTP API /v1"), with the
// server's rules (archi/04, "Secrets"; archi/06, "What the caller may do"): a grant limited to
// labels sees, writes and deletes only the organization's secrets carrying them, and the secrets
// it writes take them; it never writes a namespace's secret. No answer holds a value.
import { HttpResponse, http } from "msw";
import type { Labels, PutSecret, Secret, SecretUsage, WorkflowAttaching } from "../model/types";
import { type Caller, grantOf } from "./access";
import { guard, json, runsWorld } from "./access-handlers";
import type { World } from "./fixtures";
import { handleOf } from "./handles";
import { problem } from "./problem";
import { deriveRun } from "./readmodel";
import {
  attaches,
  emptySecretWorld,
  type MockSecret,
  newKid,
  refersTo,
  type SecretWorld,
  secretDto,
  signingKeyDto,
} from "./secrets";

const NAME_RULE = "1 to 64 lowercase letters, digits, - and _, starting with a letter";
const MAX_VALUE_BYTES = 16 * 1024;
const PUBLISHED_MS = 24 * 3_600_000;
const USAGE_LIMIT = 100;

/** The secrets of the caller's tenant; a tenant without any has an empty world. */
export function secretsOf(world: World, caller: Caller): SecretWorld {
  const tenant = caller.tenant ?? "";
  let found = world.secrets.get(tenant);
  if (!found) {
    found = emptySecretWorld();
    world.secrets.set(tenant, found);
  }
  return found;
}

function allowedLabels(world: World, caller: Caller): Labels {
  return grantOf(world.access, caller)?.allowedLabels ?? {};
}

/** Whether the caller's grant sees the secret: an organization's secret carries all its labels. */
function sees(world: World, caller: Caller, s: MockSecret): boolean {
  if (s.namespace !== null) return true;
  return Object.entries(allowedLabels(world, caller)).every(([k, v]) => s.labels[k] === v);
}

function find(
  world: World,
  caller: Caller,
  namespace: string | null,
  name: string,
): MockSecret | undefined {
  return secretsOf(world, caller).secrets.find(
    (s) => s.namespace === namespace && s.name === name && sees(world, caller, s),
  );
}

const noSecret = (namespace: string | null, name: string) =>
  problem(
    404,
    "not-found",
    namespace === null
      ? `No secret ${name} in the organization.`
      : `No secret ${name} in the namespace ${namespace}.`,
  );

function nameProblem(name: string): Response | null {
  return /^[a-z][a-z0-9_-]{0,63}$/.test(name)
    ? null
    : problem(
        400,
        "invalid-request",
        `${JSON.stringify(name)} is not a secret's name: ${NAME_RULE}.`,
      );
}

/** What reads the secret: the active runs that attach it, those waiting on a call it serves. */
function usageOf(world: World, caller: Caller, s: MockSecret, now: number): SecretUsage {
  const active: string[] = [];
  const waiting: string[] = [];
  for (const fixture of runsWorld(world, caller).runs) {
    if (!attaches(fixture, s)) continue;
    const run = deriveRun(world, fixture, now);
    if (run.status !== "active") continue;
    active.push(run.id);
    const servers = Object.entries(fixture.secrets ?? {})
      .filter(([, ref]) => refersTo(ref, s, fixture.namespace))
      .map(([server]) => server);
    const waits = (run.waitingFor ?? []).some(
      (w) => w.kind === "tool_call" && servers.includes(w.tool.split(".")[0] ?? ""),
    );
    if (waits) waiting.push(run.id);
  }
  // The latest version of each workflow.
  const latest = new Map<string, (typeof world.workflows.versions)[number]>();
  for (const v of world.workflows.versions) {
    const seen = latest.get(v.name);
    if (!seen || seen.createdAt < v.createdAt) latest.set(v.name, v);
  }
  const workflows: WorkflowAttaching[] = [];
  for (const v of latest.values()) {
    const servers = Object.entries(v.secrets ?? {})
      .filter(([, ref]) => refersTo(ref, s, null))
      .map(([server]) => server);
    if (servers.length > 0) workflows.push({ name: v.name, version: v.version, servers });
  }
  return {
    secret: withRunHandle(world, secretDto(s, now)),
    activeRuns: active.slice(0, USAGE_LIMIT),
    activeTotal: active.length,
    waitingRuns: waiting.slice(0, USAGE_LIMIT),
    waitingTotal: waiting.length,
    workflows: workflows.sort((a, b) => a.name.localeCompare(b.name)),
  };
}

function valueProblem(body: unknown): Response | null {
  if (!body || typeof body !== "object" || Array.isArray(body))
    return problem(400, "invalid-request", 'Send { "value": "…", "labels"?: {…} }.');
  const { value, labels, ...rest } = body as Record<string, unknown>;
  const unknown = Object.keys(rest)[0];
  if (unknown !== undefined)
    return problem(400, "invalid-request", `Unknown field ${JSON.stringify(unknown)}.`);
  if (typeof value !== "string")
    return problem(400, "invalid-request", "value: expected a string.");
  if (new TextEncoder().encode(value).length > MAX_VALUE_BYTES)
    return problem(400, "invalid-request", "value: at most 16 KiB.");
  if (
    labels !== undefined &&
    (labels === null ||
      typeof labels !== "object" ||
      Array.isArray(labels) ||
      Object.values(labels).some((v) => typeof v !== "string"))
  )
    return problem(400, "invalid-request", "labels: expected an object of strings.");
  return null;
}

export function secretHandlers(world: World, now: () => number) {
  /** Creates or replaces a secret, as `PUT` does: 201 or 200. */
  const put = (
    caller: Caller,
    namespace: string | null,
    name: string,
    body: PutSecret,
  ): Response => {
    const store = secretsOf(world, caller);
    const by =
      caller.kind === "user"
        ? `user:${caller.user.id}`
        : caller.kind === "key"
          ? `key:${caller.key.id}`
          : "operator";
    const grantLabels = allowedLabels(world, caller);
    let labels: Labels = {};
    if (namespace === null) {
      labels = { ...(body.labels ?? {}) };
      for (const [k, v] of Object.entries(grantLabels)) {
        if (labels[k] !== undefined && labels[k] !== v)
          return problem(
            403,
            "forbidden",
            `Your grant is limited to ${k}=${v}: a secret it writes cannot carry ${k}=${labels[k]}.`,
          );
        labels[k] = v;
      }
    }
    const at = now();
    const existing = store.secrets.find((s) => s.namespace === namespace && s.name === name);
    if (existing && !sees(world, caller, existing))
      return problem(403, "forbidden", `The secret ${name} carries labels outside your grant's.`);
    if (existing) {
      existing.value = body.value;
      existing.labels = labels;
      existing.updatedBy = by;
      existing.updatedAt = at;
      return HttpResponse.json(withRunHandle(world, secretDto(existing, at)));
    }
    const created: MockSecret = {
      namespace,
      name,
      value: body.value,
      labels,
      createdBy: by,
      createdAt: at,
      updatedBy: by,
      updatedAt: at,
      reads: new Map(),
      lastRead: null,
    };
    store.secrets.push(created);
    store.secrets.sort((a, b) => a.name.localeCompare(b.name));
    return HttpResponse.json(withRunHandle(world, secretDto(created, at)), { status: 201 });
  };

  const list = (caller: Caller, namespace: string | null) =>
    HttpResponse.json({
      secrets: secretsOf(world, caller)
        .secrets.filter((s) => s.namespace === namespace && sees(world, caller, s))
        .sort((a, b) => a.name.localeCompare(b.name))
        .map((s) => withRunHandle(world, secretDto(s, now()))),
    });

  const remove = (caller: Caller, namespace: string | null, name: string): Response => {
    const s = find(world, caller, namespace, name);
    if (!s) return noSecret(namespace, name);
    const store = secretsOf(world, caller);
    store.secrets = store.secrets.filter((x) => x !== s);
    return new HttpResponse(null, { status: 204 });
  };

  return [
    http.get("*/v1/secrets", ({ request }) => {
      const caller = guard(world, request, "secrets:read");
      return caller instanceof Response ? caller : list(caller, null);
    }),

    http.put("*/v1/secrets/:name", async ({ request, params }) => {
      const caller = guard(world, request, "secrets:write");
      if (caller instanceof Response) return caller;
      const name = String(params.name);
      const bad = nameProblem(name);
      if (bad) return bad;
      const body = await json<PutSecret>(request);
      if (body instanceof Response) return body;
      return valueProblem(body) ?? put(caller, null, name, body);
    }),

    http.delete("*/v1/secrets/:name", ({ request, params }) => {
      const caller = guard(world, request, "secrets:write");
      return caller instanceof Response ? caller : remove(caller, null, String(params.name));
    }),

    http.get("*/v1/secrets/:name/usage", ({ request, params }) => {
      const caller = guard(world, request, "secrets:read");
      if (caller instanceof Response) return caller;
      const s = find(world, caller, null, String(params.name));
      return s
        ? HttpResponse.json(usageOf(world, caller, s, now()))
        : noSecret(null, String(params.name));
    }),

    http.get("*/v1/namespaces/:ns/secrets", ({ request, params }) => {
      const caller = guard(world, request, "secrets:read");
      return caller instanceof Response ? caller : list(caller, String(params.ns));
    }),

    http.put("*/v1/namespaces/:ns/secrets/:name", async ({ request, params }) => {
      const caller = guard(world, request, "secrets:write");
      if (caller instanceof Response) return caller;
      if (Object.keys(allowedLabels(world, caller)).length > 0)
        return problem(
          403,
          "forbidden",
          "A grant limited to labels never writes a namespace's secret: it serves the end user's runs whatever their labels.",
        );
      const name = String(params.name);
      const bad = nameProblem(name);
      if (bad) return bad;
      const body = await json<PutSecret>(request);
      if (body instanceof Response) return body;
      const invalid = valueProblem(body);
      if (invalid) return invalid;
      if (body.labels !== undefined)
        return problem(400, "invalid-request", "A namespace's secret takes no labels.");
      return put(caller, String(params.ns), name, body);
    }),

    http.delete("*/v1/namespaces/:ns/secrets/:name", ({ request, params }) => {
      const caller = guard(world, request, "secrets:write");
      return caller instanceof Response
        ? caller
        : remove(caller, String(params.ns), String(params.name));
    }),

    http.get("*/v1/namespaces/:ns/secrets/:name/usage", ({ request, params }) => {
      const caller = guard(world, request, "secrets:read");
      if (caller instanceof Response) return caller;
      const s = find(world, caller, String(params.ns), String(params.name));
      return s
        ? HttpResponse.json(usageOf(world, caller, s, now()))
        : noSecret(String(params.ns), String(params.name));
    }),

    http.get("*/v1/organization/signing-keys", ({ request }) => {
      const caller = guard(world, request, "secrets:read");
      if (caller instanceof Response) return caller;
      const at = now();
      const issuer = `${new URL(request.url).origin}/organizations/${caller.tenant ?? ""}`;
      return HttpResponse.json({
        issuer,
        jwksUrl: `${issuer}/jwks.json`,
        keys: secretsOf(world, caller)
          .signingKeys.filter((k) => k.retiredAt === null || at - k.retiredAt < PUBLISHED_MS)
          .map(signingKeyDto),
      });
    }),

    http.post("*/v1/organization/signing-keys", ({ request }) => {
      const caller = guard(world, request, "organization:write");
      if (caller instanceof Response) return caller;
      const store = secretsOf(world, caller);
      const at = now();
      for (const k of store.signingKeys) if (k.retiredAt === null) k.retiredAt = at;
      const key = {
        kid: newKid(`${at}:${store.signingKeys.length}`),
        createdAt: at,
        retiredAt: null,
      };
      store.signingKeys.unshift(key);
      return HttpResponse.json(signingKeyDto(key), { status: 201 });
    }),
  ];
}

/** A secret as served, its last read naming its run by handle while the run is kept. */
function withRunHandle(world: World, dto: Secret): Secret {
  const run = dto.lastRead?.run ? world.runs.find((r) => r.id === dto.lastRead?.run) : undefined;
  if (!run || !dto.lastRead) return dto;
  return { ...dto, lastRead: { ...dto.lastRead, runHandle: handleOf(world, run) } };
}
