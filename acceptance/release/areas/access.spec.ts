/**
 * Access (`archi/06-storage-and-scale.md`, "Who calls" and "What the caller may do";
 * `archi/07-code-architecture.md`, "HTTP API /v1"): every operation of `api/openapi.json` for
 * every kind of principal, roles, grants, no one giving more than they hold, the last owner,
 * revocation from the next request, keys limited to labels, the operator's endpoints, and forged
 * credentials. WorkOS is not configured: organizations and members are out of the gate, and keys
 * stand for the principals.
 */
import { randomBytes } from "node:crypto";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { exportJWK, generateKeyPair, SignJWT } from "jose";
import { root } from "../../support/paths.js";
import {
  ADMINISTRATION,
  BUILT_IN,
  createKey,
  mintToken,
  PERMISSIONS,
  startRun,
  TOKEN_DEFAULT,
  waitingRun,
} from "../support/access-helpers.js";
import { startWebhookReceiver } from "../support/fixtures.js";
import { TaskStream } from "../support/actions-helpers.js";
import {
  Api,
  evidence,
  expect,
  expectOk,
  expectProblem,
  expectRefused,
  newTenant,
  operator,
  type Reply,
  stacks,
  type Tenant,
  test,
  unique,
} from "../support/gate.js";

// ---------------------------------------------------------------------------------------------
// The matrix

/** A run id, a digest, a task token and ids no one was given. */
const NO_RUN = "01930000-0000-7000-8000-00000000abcd";
const NO_DIGEST = "ab".repeat(32);
const NO_TASK = "01930000-0000-7000-8000-00000000dcba";
const NO_TOKEN = "rewind_tt_unknown";

/**
 * Who may call an operation: a permission of the catalog, any caller, the operator only, a member
 * signed in on the dashboard only, no one (WorkOS's sign-in and the OAuth of MCP clients, absent
 * from a server without it),
 * nobody needs to (an inbound webhook, whose unknown token is not found), or anyone, answered
 * without credentials (the public keys of signed calls).
 */
type Need = { permission: string } | "any" | "operator" | "member" | "auth" | "public" | "open";

/** Why a principal holding the permission is still refused (06: a grant limited to labels, a token). */
type Scope = "labels" | "token";

interface Probe {
  /** `METHOD /path` as `api/openapi.json` writes it. */
  op: string;
  need: Need;
  /** A second permission the request needs, checked after `need`'s (attaching a secret). */
  also?: string;
  /** The request sent: a path with ids filled in, and a body valid for the operation. */
  path: string;
  body?: unknown;
  query?: Record<string, string>;
  /** Names the namespace with a key (`Rewind-Namespace`), required to create in one. */
  namespaced?: boolean;
  /** Answers an event stream that never ends when allowed: its head is read, its body closed. */
  stream?: true;
  /** Refused (403) to these principals even when they hold the permission, by a rule of 06. */
  refusedTo?: Scope[];
}

const p = (permission: string): Need => ({ permission });

/**
 * One minimal request per operation. Allowed, each answers without changing anything another
 * check relies on: ids no one has are not found, bodies are refused after the permission check
 * (an unknown role, a script that does not parse, a URL that is not one), and what is created is
 * named for the matrix alone.
 */
const PROBES: Probe[] = [
  // Sign-in: none without WorkOS.
  { op: "GET /auth/callback", need: "auth", path: "/auth/callback", query: { code: "x", state: "y" } },
  { op: "GET /auth/config", need: "auth", path: "/auth/config" },
  { op: "POST /auth/email-verification", need: "auth", path: "/auth/email-verification", body: { code: "000000" } },
  { op: "GET /auth/github", need: "auth", path: "/auth/github" },
  { op: "POST /auth/logout", need: "auth", path: "/auth/logout", body: {} },
  { op: "POST /auth/magic-auth", need: "auth", path: "/auth/magic-auth", body: { email: "gate@example.net" } },
  { op: "POST /auth/magic-auth/verify", need: "auth", path: "/auth/magic-auth/verify", body: { email: "gate@example.net", code: "000000" } },
  { op: "POST /auth/organization", need: "auth", path: "/auth/organization", body: { organizationId: "org_unknown" } },
  { op: "POST /auth/session", need: "auth", path: "/auth/session", body: {} },
  { op: "POST /auth/waitlist", need: "auth", path: "/auth/waitlist", body: { email: "gate@example.net" } },
  // OAuth for MCP clients: none without WorkOS (a person's sign-in), whoever asks.
  { op: "GET /.well-known/oauth-authorization-server", need: "auth", path: "/.well-known/oauth-authorization-server" },
  { op: "GET /.well-known/oauth-protected-resource", need: "auth", path: "/.well-known/oauth-protected-resource" },
  { op: "GET /.well-known/oauth-protected-resource/mcp", need: "auth", path: "/.well-known/oauth-protected-resource/mcp" },
  { op: "GET /oauth/authorize", need: "auth", path: "/oauth/authorize", query: { client_id: "rc_unknown", response_type: "code" } },
  { op: "POST /oauth/register", need: "auth", path: "/oauth/register", body: { redirect_uris: ["https://claude.ai/api/mcp/auth_callback"] } },
  { op: "POST /oauth/revoke", need: "auth", path: "/oauth/revoke", body: { token: "rewind_rt_unknown" } },
  { op: "POST /oauth/token", need: "auth", path: "/oauth/token", body: { grant_type: "refresh_token", refresh_token: "rewind_rt_unknown" } },
  // A person's connections and the consent: the person, signed in on the dashboard, only.
  { op: "GET /v1/connections", need: "member", path: "/v1/connections" },
  { op: "PATCH /v1/connections/{id}", need: "member", path: "/v1/connections/cn_unknown", body: { organizations: [] } },
  { op: "DELETE /v1/connections/{id}", need: "member", path: "/v1/connections/cn_unknown" },
  { op: "GET /v1/oauth/consent", need: "member", path: "/v1/oauth/consent", query: { request: "unknown" } },
  { op: "POST /v1/oauth/consent", need: "member", path: "/v1/oauth/consent", body: { request: "unknown", decision: "deny" } },
  // Inbound webhooks: no token, an unguessable URL.
  { op: "POST /hooks/{token}", need: "public", path: `/hooks/whk_${"0".repeat(40)}`, body: { hello: "world" } },
  // Keys.
  { op: "GET /v1/api-keys", need: p("api_keys:read"), path: "/v1/api-keys" },
  { op: "POST /v1/api-keys", need: p("api_keys:write"), path: "/v1/api-keys", body: { name: "matrix", role: "no-such-role" } },
  { op: "DELETE /v1/api-keys/{id}", need: p("api_keys:write"), path: `/v1/api-keys/${NO_DIGEST}` },
  // Scripts.
  { op: "POST /v1/check", need: p("runs:start"), path: "/v1/check", body: { script: "export default async function main( {" }, namespaced: true },
  { op: "POST /v1/test", need: p("runs:start"), path: "/v1/test", body: { script: "export default async function main( {" }, namespaced: true },
  // Scripts and statistics come from counters, which carry no labels: not for a grant limited to them.
  { op: "GET /v1/scripts", need: p("runs:read"), path: "/v1/scripts", refusedTo: ["labels"] },
  { op: "GET /v1/scripts/{digest}", need: p("runs:read"), path: `/v1/scripts/${NO_DIGEST}` },
  { op: "PATCH /v1/scripts/{digest}", need: p("scripts:write"), path: `/v1/scripts/${NO_DIGEST}`, body: { title: "matrix" } },
  // Onboarding: a member only.
  { op: "GET /v1/invitations", need: "member", path: "/v1/invitations" },
  { op: "POST /v1/invitations/{id}/accept", need: "member", path: "/v1/invitations/invitation_unknown/accept" },
  { op: "POST /v1/organizations", need: "member", path: "/v1/organizations", body: { name: "Matrix" } },
  // Namespaces.
  { op: "GET /v1/namespaces", need: p("namespaces:read"), path: "/v1/namespaces" },
  { op: "DELETE /v1/namespaces/{name}", need: p("namespaces:write"), path: "/v1/namespaces/matrix-never-used" },
  { op: "GET /v1/namespaces/{name}", need: p("namespaces:read"), path: "/v1/namespaces/matrix-other" },
  { op: "PUT /v1/namespaces/{name}", need: p("namespaces:write"), path: "/v1/namespaces/matrix-other", body: {} },
  { op: "DELETE /v1/namespaces/{name}/tokens", need: p("tokens:write"), path: "/v1/namespaces/matrix-other/tokens" },
  { op: "POST /v1/namespaces/{name}/tokens", need: p("tokens:write"), path: "/v1/namespaces/matrix-other/tokens", body: { ttl: "5m" } },
  { op: "PATCH /v1/namespaces/{name}", need: p("namespaces:write"), path: "/v1/namespaces/matrix-never-used", body: {} },
  // A namespace's secrets: a grant limited to labels reads them and never writes one (06).
  { op: "GET /v1/namespaces/{name}/secrets", need: p("secrets:read"), path: "/v1/namespaces/matrix-other/secrets" },
  {
    op: "PUT /v1/namespaces/{name}/secrets/{secret}",
    need: p("secrets:write"),
    path: "/v1/namespaces/matrix-other/secrets/matrix",
    body: { value: "the matrix's" },
    refusedTo: ["labels"],
  },
  {
    op: "DELETE /v1/namespaces/{name}/secrets/{secret}",
    need: p("secrets:write"),
    path: "/v1/namespaces/matrix-other/secrets/matrix-never-put",
    refusedTo: ["labels"],
  },
  { op: "GET /v1/namespaces/{name}/secrets/{secret}/usage", need: p("secrets:read"), path: "/v1/namespaces/matrix-other/secrets/matrix-never-put/usage" },
  // The organization: its endpoints exist, and answer 404 without WorkOS once the permission holds.
  { op: "PATCH /v1/organization", need: p("organization:write"), path: "/v1/organization", body: {} },
  { op: "GET /v1/organization/invitations", need: p("members:read"), path: "/v1/organization/invitations" },
  {
    op: "POST /v1/organization/invitations",
    need: p("members:invite"),
    path: "/v1/organization/invitations",
    body: { email: "matrix@example.net", role: "reader" },
  },
  { op: "DELETE /v1/organization/invitations/{id}", need: p("members:invite"), path: "/v1/organization/invitations/invitation_unknown" },
  { op: "POST /v1/organization/invitations/{id}/resend", need: p("members:invite"), path: "/v1/organization/invitations/invitation_unknown/resend" },
  { op: "GET /v1/organization/members", need: p("members:read"), path: "/v1/organization/members" },
  { op: "GET /v1/organization/connections", need: p("members:read"), path: "/v1/organization/connections" },
  { op: "DELETE /v1/organization/connections/{id}", need: p("members:manage"), path: "/v1/organization/connections/cn_unknown" },
  { op: "DELETE /v1/organization/members/{user}", need: p("members:manage"), path: "/v1/organization/members/user_unknown" },
  { op: "PUT /v1/organization/members/{user}/grant", need: p("members:manage"), path: "/v1/organization/members/user_unknown/grant", body: { role: "reader" } },
  { op: "GET /v1/organization/namespace-defaults", need: p("namespaces:read"), path: "/v1/organization/namespace-defaults" },
  { op: "GET /v1/organization/namespace-names", need: p("namespaces:read"), path: "/v1/organization/namespace-names" },
  { op: "PUT /v1/organization/namespace-defaults", need: p("namespaces:write"), path: "/v1/organization/namespace-defaults", body: { defaults: {} } },
  { op: "GET /v1/organization/quota-alerts", need: p("namespaces:read"), path: "/v1/organization/quota-alerts" },
  // The signing keys: read with secrets:read, replaced by organization:write (the matrix's tenant's own key).
  { op: "GET /v1/organization/signing-keys", need: p("secrets:read"), path: "/v1/organization/signing-keys", refusedTo: ["token"] },
  { op: "POST /v1/organization/signing-keys", need: p("organization:write"), path: "/v1/organization/signing-keys" },
  { op: "GET /v1/organization/token-keys", need: p("tokens:write"), path: "/v1/organization/token-keys" },
  { op: "POST /v1/organization/token-keys", need: p("tokens:write"), path: "/v1/organization/token-keys", body: { name: "matrix", jwk: {} } },
  { op: "DELETE /v1/organization/token-keys/{kid}", need: p("tokens:write"), path: "/v1/organization/token-keys/rk_unknown" },
  // The operator's.
  { op: "PUT /v1/organizations/{tenant}/quotas", need: "operator", path: "/v1/organizations/{tenant}/quotas", body: { ceiling: {} } },
  { op: "POST /v1/server/stop", need: "operator", path: "/v1/server/stop" },
  // Payloads: through a run (`run` is required with a token or a grant limited to labels).
  { op: "GET /v1/payloads/{digest}", need: p("runs:read"), path: `/v1/payloads/${NO_DIGEST}`, query: { run: NO_RUN } },
  // Access.
  { op: "GET /v1/permissions", need: "any", path: "/v1/permissions" },
  { op: "GET /v1/roles", need: p("roles:read"), path: "/v1/roles" },
  { op: "DELETE /v1/roles/{name}", need: p("roles:write"), path: "/v1/roles/matrix-never-made" },
  { op: "PUT /v1/roles/{name}", need: p("roles:write"), path: "/v1/roles/matrix-empty", body: { permissions: [] } },
  // Runs.
  { op: "GET /v1/runs", need: p("runs:read"), path: "/v1/runs" },
  { op: "GET /v1/changes", need: p("runs:read"), path: "/v1/changes", stream: true },
  {
    op: "POST /v1/runs",
    need: p("runs:start"),
    path: "/v1/runs",
    body: { script: "export default async function main( {" },
    namespaced: true,
  },
  { op: "GET /v1/runs/failures", need: p("runs:read"), path: "/v1/runs/failures", refusedTo: ["labels"] },
  { op: "GET /v1/runs/stats", need: p("runs:read"), path: "/v1/runs/stats", refusedTo: ["labels"] },
  { op: "DELETE /v1/runs/{id}", need: p("runs:delete"), path: `/v1/runs/${NO_RUN}` },
  { op: "GET /v1/runs/{id}", need: p("runs:read"), path: `/v1/runs/${NO_RUN}` },
  { op: "POST /v1/runs/{id}/cancel", need: p("runs:control"), path: `/v1/runs/${NO_RUN}/cancel`, body: {} },
  { op: "GET /v1/runs/{id}/entries/{idx}", need: p("runs:read"), path: `/v1/runs/${NO_RUN}/entries/0` },
  { op: "GET /v1/runs/{id}/events", need: p("runs:read"), path: `/v1/runs/${NO_RUN}/events` },
  { op: "POST /v1/runs/{id}/fork", need: p("runs:start"), path: `/v1/runs/${NO_RUN}/fork`, body: { script: "export default async function main() { return 1; }" } },
  { op: "POST /v1/runs/{id}/give-up", need: p("runs:quarantine"), path: `/v1/runs/${NO_RUN}/give-up` },
  { op: "GET /v1/runs/{id}/journal", need: p("runs:read"), path: `/v1/runs/${NO_RUN}/journal` },
  { op: "POST /v1/runs/{id}/release", need: p("runs:quarantine"), path: `/v1/runs/${NO_RUN}/release` },
  { op: "POST /v1/runs/{id}/signals/{name}", need: p("runs:control"), path: `/v1/runs/${NO_RUN}/signals/go`, body: { n: 1 } },
  { op: "GET /v1/runs/{id}/trace", need: p("runs:read"), path: `/v1/runs/${NO_RUN}/trace` },
  // The server.
  { op: "GET /v1/server", need: "any", path: "/v1/server" },
  // The organization's secrets: never an end user's token's (06: "Tokens"). A name that is no
  // secret's name is refused after the permission (400): nothing is written.
  { op: "GET /v1/secrets", need: p("secrets:read"), path: "/v1/secrets", refusedTo: ["token"] },
  { op: "PUT /v1/secrets/{name}", need: p("secrets:write"), path: "/v1/secrets/Not-A-Secret-Name", body: { value: "the matrix's" }, refusedTo: ["token"] },
  { op: "DELETE /v1/secrets/{name}", need: p("secrets:write"), path: "/v1/secrets/matrix-never-put", refusedTo: ["token"] },
  { op: "GET /v1/secrets/{name}/usage", need: p("secrets:read"), path: "/v1/secrets/matrix-never-put/usage", refusedTo: ["token"] },
  // The public keys of signed calls: no credentials, whoever asks.
  { op: "GET /organizations/{tenant}/jwks.json", need: "open", path: "/organizations/{tenant}/jwks.json" },
  // Lifecycle webhooks.
  { op: "GET /v1/subscriptions", need: p("subscriptions:read"), path: "/v1/subscriptions" },
  { op: "POST /v1/subscriptions", need: p("subscriptions:write"), path: "/v1/subscriptions", body: { url: "not a url" } },
  { op: "DELETE /v1/subscriptions/{id}", need: p("subscriptions:write"), path: "/v1/subscriptions/sub_unknown" },
  // Harness calls.
  { op: "GET /v1/calls/stream", need: p("calls:work"), path: "/v1/calls/stream", stream: true },
  { op: "POST /v1/calls/{id}/approve", need: p("calls:approve"), path: `/v1/calls/${NO_TASK}/approve` },
  { op: "POST /v1/calls/{id}/reject", need: p("calls:approve"), path: `/v1/calls/${NO_TASK}/reject`, body: {} },
  { op: "POST /v1/calls/{token}/complete", need: p("calls:work"), path: `/v1/calls/${NO_TOKEN}/complete`, body: { result: null } },
  { op: "POST /v1/calls/{token}/defer", need: p("calls:work"), path: `/v1/calls/${NO_TOKEN}/defer`, body: {} },
  { op: "POST /v1/calls/{token}/fail", need: p("calls:work"), path: `/v1/calls/${NO_TOKEN}/fail`, body: { message: "matrix" } },
  // Tokens.
  { op: "DELETE /v1/tokens/{id}", need: p("tokens:write"), path: `/v1/tokens/${NO_DIGEST}` },
  // Tool statistics span the tenant: not for a grant limited to labels.
  { op: "GET /v1/actions/stats", need: p("runs:read"), path: "/v1/actions/stats", refusedTo: ["labels"] },
  // Action sets.
  { op: "GET /v1/action-sets", need: p("actions:read"), path: "/v1/action-sets" },
  // Attaching an organization's secret to list a server needs secrets:use as well; no server of
  // the request takes it, so a holder is answered 422 secret-required and nothing is registered.
  {
    op: "POST /v1/action-sets",
    need: p("actions:write"),
    also: "secrets:use",
    path: "/v1/action-sets",
    body: { secrets: { matrix: "org/matrix-never-put" } },
  },
  { op: "GET /v1/action-sets/{id}/declarations", need: p("actions:read"), path: `/v1/action-sets/${NO_DIGEST}/declarations` },
  { op: "GET /v1/action-sets/{id}/search", need: p("actions:read"), path: `/v1/action-sets/${NO_DIGEST}/search`, query: { query: "x" } },
  // Who calls.
  { op: "GET /v1/whoami", need: "any", path: "/v1/whoami" },
  // Workflows.
  { op: "GET /v1/workflows", need: p("actions:read"), path: "/v1/workflows" },
  {
    op: "POST /v1/workflows",
    need: p("actions:write"),
    path: "/v1/workflows",
    body: { name: "matrix", actionSet: NO_DIGEST, script: "export default async function main() {}", input: {}, output: {} },
  },
  { op: "GET /v1/workflows/{name}", need: p("actions:read"), path: "/v1/workflows/matrix-never-saved" },
  { op: "POST /v1/workflows/{name}/record", need: p("runs:read"), path: "/v1/workflows/matrix-never-saved/record", body: {} },
  { op: "GET /v1/tests/{digest}", need: p("actions:read"), path: `/v1/tests/${NO_DIGEST}` },
];

/** Every operation of `api/openapi.json`, as `METHOD /path`. */
function operations(): string[] {
  const document = JSON.parse(readFileSync(join(root, "api", "openapi.json"), "utf8")) as { paths: Record<string, Record<string, unknown>> };
  const found: string[] = [];
  for (const [path, methods] of Object.entries(document.paths)) {
    for (const method of Object.keys(methods)) {
      if (["get", "post", "put", "patch", "delete"].includes(method)) found.push(`${method.toUpperCase()} ${path}`);
    }
  }
  return found.sort();
}

/** A principal of the matrix: who it is and what 06 says it holds. */
interface Principal {
  name: string;
  bearer: string;
  holds: readonly string[];
  scope?: Scope;
}

/** What the matrix expects of one cell, and what it got. */
interface Cell {
  op: string;
  expected: "answered" | "refused" | "not found";
  status: number;
  slug?: string;
  ok: boolean;
  why?: string;
}

async function send(api: Api, probe: Probe, principal: Principal, tenant: string): Promise<Reply> {
  const method = probe.op.split(" ")[0]!;
  let target = api;
  if (probe.namespaced && principal.scope !== "token") target = target.ns("matrix-ns");
  return target.request(method, probe.path.replace("{tenant}", tenant), {
    json: probe.body,
    query: probe.query,
    stream: probe.stream,
  });
}

/**
 * Runs every probe as `principal`: refused (403, naming the permission) what it lacks, answered
 * (neither 401 nor 403) what it holds, refused the operator's and members' endpoints, 404 for
 * `/auth` without WorkOS.
 */
async function matrix(tenant: Tenant, principal: Principal): Promise<Cell[]> {
  const api = new Api(stacks().a.url, principal.bearer);
  const cells: Cell[] = [];
  for (const probe of PROBES) {
    const reply = await send(api, probe, principal, tenant.id);
    const detail = String(reply.json?.detail ?? "");
    let expected: Cell["expected"];
    let ok: boolean;
    let why: string | undefined;
    if (probe.need === "auth") {
      expected = "not found";
      ok = reply.status === 404;
      why = "a server without WorkOS has no /auth, nor OAuth";
    } else if (probe.need === "public") {
      expected = "not found";
      ok = reply.status === 404 && reply.slug === "not-found";
      why = "no inbound webhook has this token";
    } else if (probe.need === "any") {
      expected = "answered";
      ok = reply.status === 200;
    } else if (probe.need === "open") {
      expected = "answered";
      ok = reply.status === 200 && reply.headers.get("cache-control") === "max-age=300";
      why = "public, cacheable 5 minutes";
    } else if (probe.need === "operator") {
      expected = "refused";
      ok = reply.status === 403 && reply.slug === "forbidden" && detail.includes("operator");
      why = "the operator's only";
    } else if (probe.need === "member") {
      expected = "refused";
      ok = reply.status === 403 && reply.slug === "forbidden";
      why = "a member signed in on the dashboard only";
    } else {
      const needed = [probe.need.permission, ...(probe.also ? [probe.also] : [])];
      const permission = needed.find((p) => !principal.holds.includes(p)) ?? probe.need.permission;
      const held = needed.every((p) => principal.holds.includes(p));
      const scoped = principal.scope !== undefined && (probe.refusedTo ?? []).includes(principal.scope);
      if (held && !scoped) {
        expected = "answered";
        ok = reply.status !== 401 && reply.status !== 403 && reply.status < 500;
      } else if (held && scoped) {
        expected = "refused";
        ok = reply.status === 403 && reply.slug === "forbidden";
        why = principal.scope === "labels" ? "a grant limited to labels" : "an end user's token";
      } else {
        expected = "refused";
        ok = reply.status === 403 && reply.slug === "forbidden" && detail.includes(`\`${permission}\``);
        why = `lacks ${permission}`;
      }
    }
    cells.push({ op: probe.op, expected, status: reply.status, slug: reply.slug, ok, why });
  }
  return cells;
}

function grid(cells: Cell[]): string {
  return cells.map((c) => `${c.ok ? "ok " : "BAD"} ${c.op} → ${c.status}${c.slug ? ` ${c.slug}` : ""} (expected ${c.expected}${c.why ? `: ${c.why}` : ""})`).join("\n");
}

function expectMatrix(principal: string, cells: Cell[]): void {
  const answered = cells.filter((c) => c.expected === "answered").length;
  const refused = cells.filter((c) => c.expected === "refused").length;
  evidence(`${principal}: ${cells.length} operations, ${answered} answered, ${refused} refused with 403, ${cells.length - answered - refused} not found`);
  evidence(grid(cells));
  expect(cells.filter((c) => !c.ok).map((c) => `${c.op} → ${c.status} ${c.slug ?? ""} (expected ${c.expected}${c.why ? `: ${c.why}` : ""})`)).toEqual([]);
}

test.describe("the permission matrix", () => {
  let tenant: Tenant;

  test.beforeAll(async () => {
    tenant = await newTenant("access-matrix");
  });

  test("the catalog and the built-in roles are 06's", { tag: "@success" }, async () => {
    const catalog = await tenant.api.get("/v1/permissions");
    expectOk(catalog, 200);
    expect((catalog.json.permissions as { permission: string }[]).map((p) => p.permission)).toEqual([...PERMISSIONS]);
    const roles = await tenant.api.get("/v1/roles");
    expectOk(roles, 200);
    const builtIn = Object.fromEntries(
      (roles.json.roles as { name: string; permissions: string[]; builtIn: boolean }[]).filter((r) => r.builtIn).map((r) => [r.name, r.permissions]),
    );
    expect(Object.keys(builtIn).sort()).toEqual(["harness", "owner", "reader", "worker"]);
    for (const [role, permissions] of Object.entries(BUILT_IN)) {
      expect([...builtIn[role]!].sort(), role).toEqual([...permissions].sort());
    }
    expect(builtIn.operator, "operator is no role of an organization").toBeUndefined();
    evidence("GET /v1/permissions lists the 25 permissions of 06 in order (secrets:read, secrets:write and secrets:use where credentials:write was); GET /v1/roles gives owner, harness, worker and reader exactly 06's table");
  });

  test("every operation of api/openapi.json has a probe in the matrix", { tag: "@success" }, () => {
    const probed = PROBES.map((probe) => probe.op).sort();
    expect(new Set(probed).size, "one probe per operation").toBe(probed.length);
    const all = operations();
    expect(all.filter((op) => !probed.includes(op)), "operations without a probe: add one to PROBES").toEqual([]);
    expect(probed.filter((op) => !all.includes(op)), "probes of operations the API no longer has").toEqual([]);
    evidence(`${all.length} operations in api/openapi.json, each probed`);
  });

  test("every permission is both held and refused by a principal of the matrix", { tag: "@success" }, () => {
    const holders: Record<string, readonly string[]> = {
      ...BUILT_IN,
      "custom role": ["calls:approve"],
      "owner limited to labels": PERMISSIONS.filter((p) => !ADMINISTRATION.includes(p)),
      "end user's token": TOKEN_DEFAULT,
    };
    const needed = new Set(PROBES.flatMap((probe) => (typeof probe.need === "object" ? [probe.need.permission, ...(probe.also ? [probe.also] : [])] : [])));
    expect([...needed].sort(), "every permission is needed by an operation").toEqual([...PERMISSIONS].sort());
    for (const permission of PERMISSIONS) {
      const holding = Object.entries(holders).filter(([, held]) => held.includes(permission)).map(([name]) => name);
      const lacking = Object.entries(holders).filter(([, held]) => !held.includes(permission)).map(([name]) => name);
      expect(holding.length, `${permission} is held by someone`).toBeGreaterThan(0);
      expect(lacking.length, `${permission} is refused to someone`).toBeGreaterThan(0);
    }
    evidence("each of the 25 permissions is held by at least one principal of the matrix and lacked by another");
  });

  test("an owner key reaches every operation of its tenant but the operator's and the members'", { tag: "@success" }, async () => {
    const whoami = await tenant.api.get("/v1/whoami");
    expect([...whoami.json.permissions].sort()).toEqual([...PERMISSIONS].sort());
    expectMatrix("owner", await matrix(tenant, { name: "owner", bearer: tenant.ownerKey, holds: PERMISSIONS }));
  });

  for (const role of ["harness", "worker", "reader"] as const) {
    test(`a ${role} key is refused, naming the permission, everything its role does not give`, { tag: "@attack" }, async () => {
      const { key } = await createKey(tenant.api, role, { name: `matrix ${role}` });
      const whoami = await new Api(stacks().a.url, key).get("/v1/whoami");
      expectOk(whoami, 200);
      expect([...whoami.json.permissions].sort()).toEqual([...BUILT_IN[role]!].sort());
      expect(whoami.json.grant?.role).toBe(role);
      expectMatrix(role, await matrix(tenant, { name: role, bearer: key, holds: BUILT_IN[role]! }));
    });
  }

  test("a key of a role of the organization holding one permission reaches that permission only", { tag: "@attack" }, async () => {
    const role = `approver-${unique()}`;
    expectOk(await tenant.api.put(`/v1/roles/${role}`, { description: "approves calls", permissions: ["calls:approve"] }));
    const { key } = await createKey(tenant.api, role);
    expectMatrix(role, await matrix(tenant, { name: role, bearer: key, holds: ["calls:approve"] }));
  });

  test("an owner key limited to labels cannot administer or read statistics", { tag: "@attack" }, async () => {
    const { key } = await createKey(tenant.api, "owner", { name: "matrix owner red", allowedLabels: { team: "red" } });
    const whoami = await new Api(stacks().a.url, key).get("/v1/whoami");
    const holds = PERMISSIONS.filter((p) => !ADMINISTRATION.includes(p));
    expect([...whoami.json.permissions].sort()).toEqual([...holds].sort());
    expect(whoami.json.grant?.allowedLabels).toEqual({ team: "red" });
    expectMatrix("owner limited to labels", await matrix(tenant, { name: "owner limited to labels", bearer: key, holds, scope: "labels" }));
  });

  test("an end user's token holds its default permissions, in its namespace, and nothing else", { tag: "@attack" }, async () => {
    const { token } = await mintToken(tenant.api, "matrix-alice");
    const whoami = await new Api(stacks().a.url, token).get("/v1/whoami");
    expectOk(whoami, 200);
    expect([...whoami.json.permissions].sort()).toEqual([...TOKEN_DEFAULT].sort());
    expectMatrix("end user's token", await matrix(tenant, { name: "token", bearer: token, holds: TOKEN_DEFAULT, scope: "token" }));
  });
});

// ---------------------------------------------------------------------------------------------
// Roles and grants

test.describe("roles and grants", () => {
  let tenant: Tenant;

  test.beforeAll(async () => {
    tenant = await newTenant("access-roles");
  });

  test("a role of the organization is created, listed, changed for its holders at once, and deleted", { tag: "@success" }, async () => {
    const role = `auditor-${unique()}`;
    const created = await tenant.api.put(`/v1/roles/${role}`, { description: "reads runs", permissions: ["runs:read"] });
    expectOk(created, 201);
    expect(created.json).toMatchObject({ name: role, permissions: ["runs:read"], builtIn: false });
    const listed = await tenant.api.get("/v1/roles");
    expect((listed.json.roles as { name: string }[]).map((r) => r.name)).toContain(role);

    const { key, id } = await createKey(tenant.api, role);
    const holder = new Api(stacks().a.url, key);
    expectOk(await holder.get("/v1/runs"), 200);
    expectProblem(await holder.get("/v1/action-sets"), 403, "forbidden");

    // Changed: the next request of its holder follows the role.
    expectOk(await tenant.api.put(`/v1/roles/${role}`, { description: "reads actionSets", permissions: ["actions:read"] }));
    expectOk(await holder.get("/v1/action-sets"), 200);
    const refused = await holder.get("/v1/runs");
    expectProblem(refused, 403, "forbidden");
    expect(refused.json.detail).toContain("`runs:read`");

    // Deleted once no grant points to it.
    expectOk(await tenant.api.delete(`/v1/api-keys/${id}`), 204);
    expectOk(await tenant.api.delete(`/v1/roles/${role}`), 204);
    const after = await tenant.api.get("/v1/roles");
    expect((after.json.roles as { name: string }[]).map((r) => r.name)).not.toContain(role);
    evidence("PUT /v1/roles/{name} creates and changes; a holder's next request follows the change; DELETE once unused → 204");
  });

  test("a role a grant points to cannot be deleted", { tag: "@failure" }, async () => {
    const role = `held-${unique()}`;
    expectOk(await tenant.api.put(`/v1/roles/${role}`, { permissions: ["runs:read"] }));
    const { key } = await createKey(tenant.api, role);
    await expectRefused({
      attempt: () => tenant.api.delete(`/v1/roles/${role}`),
      status: 409,
      slug: "role-in-use",
      probe: async () => (await tenant.api.get("/v1/roles")).json.roles.find((r: { name: string }) => r.name === role)?.permissions,
    });
    expectOk(await new Api(stacks().a.url, key).get("/v1/runs"), 200);
    evidence("DELETE /v1/roles/{name} of a role a key holds → 409 role-in-use; the key keeps its rights");
  });

  test("a role's name and permissions are checked", { tag: "@failure" }, async () => {
    for (const name of ["owner", "harness", "worker", "reader", "operator", "Upper", "a".repeat(65), "spa%20ce"]) {
      const reply = await tenant.api.put(`/v1/roles/${name}`, { permissions: ["runs:read"] });
      expect(reply.status, `PUT /v1/roles/${name}: ${reply.text}`).toBe(400);
      expect(reply.slug).toBe("invalid-request");
    }
    const unknown = await tenant.api.put(`/v1/roles/fine-${unique()}`, { permissions: ["runs:read", "runs:everything"] });
    expect(unknown.status, unknown.text).toBe(400);
    evidence("built-in names, operator, uppercase, 65 characters and an unknown permission → 400 invalid-request");
  });

  test("no one gives a key more than they hold", { tag: "@attack" }, async () => {
    const keys = async () => (await tenant.api.get("/v1/api-keys")).json.keys.map((k: { id: string }) => k.id).sort();
    const { key: harness } = await createKey(tenant.api, "harness");
    const asHarness = tenant.api.as(harness);
    // The harness role has no api_keys:write: refused by permission.
    await expectRefused({ attempt: () => asHarness.post("/v1/api-keys", { name: "escalate", role: "owner" }), status: 403, slug: "forbidden", probe: keys });

    // A role that writes keys but holds little: it may give only what it holds.
    const role = `keymaker-${unique()}`;
    expectOk(await tenant.api.put(`/v1/roles/${role}`, { permissions: ["api_keys:write", "api_keys:read", "runs:read"] }));
    const { key: keymaker } = await createKey(tenant.api, role);
    const asKeymaker = tenant.api.as(keymaker);
    for (const target of ["owner", "harness", "reader"]) {
      const reply = await expectRefused({ attempt: () => asKeymaker.post("/v1/api-keys", { name: "escalate", role: target }), status: 403, slug: "forbidden", probe: keys });
      expect(reply.json.detail).toContain("no one gives more than they hold");
    }
    // Its own role, which it holds entirely, it may give.
    expectOk(await asKeymaker.post("/v1/api-keys", { name: "same", role }), 201);
    // Only the operator gives `operator`, even to an owner.
    await expectRefused({ attempt: () => tenant.api.post("/v1/api-keys", { name: "op", role: "operator" }), status: 403, slug: "forbidden", probe: keys });
    // Taking a grant away needs its role's permissions too: the keymaker cannot revoke the owner's key.
    const ownerId = (await tenant.api.get("/v1/whoami")).json.principal.id as string;
    await expectRefused({ attempt: () => asKeymaker.delete(`/v1/api-keys/${ownerId}`), status: 403, slug: "forbidden", probe: keys });
    expectOk(await tenant.api.get("/v1/runs"), 200);
    evidence("harness → owner key: 403; a role with api_keys:write → owner, harness, reader keys: 403 \"no one gives more than they hold\"; owner → operator key: 403; revoking the owner key without its permissions: 403");
  });

  test("no one writes a role giving more than they hold", { tag: "@attack" }, async () => {
    const roles = async () => (await tenant.api.get("/v1/roles")).json.roles.map((r: { name: string; permissions: string[] }) => `${r.name}:${r.permissions.join(",")}`).sort();
    const writer = `role-writer-${unique()}`;
    expectOk(await tenant.api.put(`/v1/roles/${writer}`, { permissions: ["roles:write", "roles:read", "runs:read"] }));
    const { key } = await createKey(tenant.api, writer);
    const asWriter = tenant.api.as(key);
    // A new role with a permission it lacks.
    await expectRefused({ attempt: () => asWriter.put(`/v1/roles/escalated-${unique()}`, { permissions: ["runs:delete"] }), status: 403, slug: "forbidden", probe: roles });
    // Its own role, widened.
    await expectRefused({
      attempt: () => asWriter.put(`/v1/roles/${writer}`, { permissions: ["roles:write", "roles:read", "runs:read", "api_keys:write"] }),
      status: 403,
      slug: "forbidden",
      probe: roles,
    });
    // A role within what it holds is fine.
    expectOk(await asWriter.put(`/v1/roles/narrow-${unique()}`, { permissions: ["runs:read"] }));
    // A role someone else wrote that gives more than it holds cannot be changed by it: the
    // change would take away what its holders have.
    const wide = `wide-${unique()}`;
    expectOk(await tenant.api.put(`/v1/roles/${wide}`, { permissions: ["runs:delete", "runs:read"] }));
    await expectRefused({ attempt: () => asWriter.put(`/v1/roles/${wide}`, { permissions: ["runs:read"] }), status: 403, slug: "forbidden", probe: roles });
    evidence("a key with roles:write is refused (403) a role with a permission it lacks, widening its own role, and narrowing a role that gives more than it holds");
  });

  test("keys never count as the organization's owner: its only owner key can be revoked", { tag: "@failure" }, async () => {
    // archi/08 (accounts): `keep` refuses only a write that takes `owner` from the last member
    // (`user:`) holding it; keys and pending invitations never count as holders. A tenant the
    // operator made has no owner member, so its owner keys can all go; `last-owner` protects
    // members, which only WorkOS brings (criterion 8).
    const lone = await newTenant("access-last-owner");
    const ownerId = (await lone.api.get("/v1/whoami")).json.principal.id as string;
    const second = await createKey(lone.api, "owner", { name: "second owner" });
    expectOk(await lone.api.as(second.key).delete(`/v1/api-keys/${ownerId}`), 204);
    expectProblem(await lone.api.get("/v1/whoami"), 401, "unauthorized");
    const last = await lone.api.as(second.key).delete(`/v1/api-keys/${second.id}`);
    expectOk(last, 204);
    expectProblem(await lone.api.as(second.key).get("/v1/whoami"), 401, "unauthorized");
    // The operator still reaches the tenant: it makes a new owner key.
    const again = await operator().post("/v1/api-keys", { tenant: lone.id, role: "owner", name: "new owner" });
    expectOk(again, 201);
    expectOk(await lone.api.as(again.json.key).get("/v1/whoami"), 200);
    evidence("an owner key revokes another, then itself: 204 both times; keys never stand for the organization's owner (08), and the operator makes a new one");
  });

  test("a revoked key is refused from its next request", { tag: "@attack" }, async () => {
    const { key, id } = await createKey(tenant.api, "harness");
    const asKey = tenant.api.as(key);
    expectOk(await asKey.get("/v1/runs"), 200);
    expectOk(await tenant.api.delete(`/v1/api-keys/${id}`), 204);
    expectProblem(await asKey.get("/v1/runs"), 401, "unauthorized");
    expectProblem(await asKey.get("/v1/whoami"), 401, "unauthorized");
    const actionSets = async () => (await tenant.api.get("/v1/action-sets")).json;
    await expectRefused({ attempt: () => asKey.post("/v1/action-sets", {}), status: 401, slug: "unauthorized", probe: actionSets });
    // Revoked twice: not found.
    expectProblem(await tenant.api.delete(`/v1/api-keys/${id}`), 404, "not-found");
    evidence("after DELETE /v1/api-keys/{id}, the key's next request → 401 unauthorized");
  });

  test("a role narrowed takes the permission away from the next request", { tag: "@attack" }, async () => {
    const role = `shrinking-${unique()}`;
    expectOk(await tenant.api.put(`/v1/roles/${role}`, { permissions: ["runs:read", "runs:start"] }));
    const { key } = await createKey(tenant.api, role);
    const asKey = tenant.api.as(key);
    expectOk(await startRun(asKey, "shrinking", { wait: "30s" }));
    expectOk(await tenant.api.put(`/v1/roles/${role}`, { permissions: ["runs:read"] }));
    const runs = async () => (await tenant.api.ns("shrinking").get("/v1/runs")).json.runs.length;
    const refused = await expectRefused({ attempt: () => startRun(asKey, "shrinking", { wait: "30s" }), status: 403, slug: "forbidden", probe: runs });
    expect(refused.json.detail).toContain("`runs:start`");
    evidence("a role changed from [runs:read, runs:start] to [runs:read]: its key's next start → 403 naming runs:start, no run created");
  });
});

// ---------------------------------------------------------------------------------------------
// Keys limited to labels

/**
 * An API whose key is a secret the run attaches: its document inline, its base URL public and
 * never called (a start refused for its attachments sends nothing).
 */
const CRM_API = {
  apis: [
    {
      name: "crm",
      openapi: {
        openapi: "3.1.0",
        info: { title: "crm", version: "1" },
        servers: [{ url: "https://crm.example.com/v1" }],
        paths: {
          "/customers/{id}": {
            get: {
              operationId: "find",
              parameters: [{ name: "id", in: "path", required: true, schema: { type: "string" } }],
              responses: { "200": { description: "it", content: { "application/json": { schema: { type: "object" } } } } },
            },
          },
        },
      },
      auth: { type: "secret", header: "X-Api-Key" },
    },
  ],
};
const CRM_SCRIPT = `import { crm } from "rewind:actions";\nexport default async function main() { return await crm.find({ id: "1" }); }\n`;

test.describe("keys limited to labels", () => {
  let tenant: Tenant;
  let red: Api;
  let redRun: string;
  let blueRun: string;
  let blueWaiting: string;

  test.beforeAll(async () => {
    tenant = await newTenant("access-labels");
    const created = await createKey(tenant.api, "harness", { name: "red agent", allowedLabels: { team: "red" } });
    red = new Api(stacks().a.url, created.key);
    redRun = (await startRun(tenant.api, "labels", { labels: { team: "red" }, wait: "30s" })).json.id;
    blueRun = (await startRun(tenant.api, "labels", { labels: { team: "blue" }, wait: "30s" })).json.id;
    blueWaiting = (await waitingRun(tenant, "labels", "blue waits", { team: "blue" })).id;
  });

  test("a key limited to labels reads only the runs carrying them", { tag: "@attack" }, async () => {
    expectOk(await red.get(`/v1/runs/${redRun}`), 200);
    for (const path of [`/v1/runs/${blueRun}`, `/v1/runs/${blueRun}/journal`, `/v1/runs/${blueRun}/trace`, `/v1/runs/${blueRun}/entries/0`, `/v1/runs/${blueRun}/events`]) {
      expectProblem(await red.get(path), 404, "not-found");
    }
    const listed = await red.get("/v1/runs");
    expectOk(listed, 200);
    const ids = (listed.json.runs as { id: string; labels: Record<string, string> }[]).map((r) => r.id);
    expect(ids).toContain(redRun);
    expect(ids).not.toContain(blueRun);
    expect(ids).not.toContain(blueWaiting);
    evidence("the red key: the blue run, its journal, trace, entries and events → 404; lists hold red runs only");
  });

  test("a key limited to labels cannot signal, cancel, fork or erase a run it does not see", { tag: "@attack" }, async () => {
    const probe = async () => {
      const state = await tenant.api.get(`/v1/runs/${blueWaiting}`);
      const journal = await tenant.api.get(`/v1/runs/${blueWaiting}/journal`);
      return { status: state.json.status, entries: journal.json.entries.length };
    };
    await expectRefused({ attempt: () => red.post(`/v1/runs/${blueWaiting}/signals/go`, { n: 1 }), status: 404, slug: "not-found", probe });
    await expectRefused({ attempt: () => red.post(`/v1/runs/${blueWaiting}/cancel`, {}), status: 404, slug: "not-found", probe });
    await expectRefused({ attempt: () => red.post(`/v1/runs/${blueWaiting}/fork`, { script: "export default async function main() { return 0; }" }), status: 404, slug: "not-found", probe });
    await expectRefused({ attempt: () => red.delete(`/v1/runs/${blueWaiting}`), status: 404, slug: "not-found", probe });
    evidence("signal, cancel, fork and erase of a run without the key's labels → 404; the run still waits, its journal unchanged");
  });

  test("what a key limited to labels starts carries them, and a contradicting label is refused", { tag: "@attack" }, async () => {
    const started = await startRun(red, "labels", { wait: "30s" });
    expectOk(started);
    expect(started.json.labels).toMatchObject({ team: "red" });
    const count = async () => (await tenant.api.ns("labels").get("/v1/runs", { query: { label: "team:blue" } })).json.runs.length;
    const refused = await expectRefused({ attempt: () => startRun(red, "labels", { labels: { team: "blue" }, wait: "30s" }), status: 403, slug: "forbidden", probe: count });
    expect(refused.json.detail).toContain("team:red");
    // Its forks too: a fork of its run cannot be relabelled away.
    const fork = await red.post(`/v1/runs/${started.json.id}/fork`, { script: "export default async function main() { return 0; }", labels: { team: "blue" } });
    expectProblem(fork, 403, "forbidden");
    evidence("a run started by the red key gets team:red; asking team:blue for a run or a fork → 403 forbidden, no blue run created");
  });

  test("a key limited to labels sees, lists for and attaches only the organization's secrets carrying them, and never writes a namespace's", { tag: "@attack" }, async () => {
    expectOk(await tenant.api.put("/v1/secrets/blue-crm", { value: "blue's", labels: { team: "blue" } }), 201);
    const written = await red.put("/v1/secrets/red-crm", { value: "red's" });
    expectOk(written, 201);
    expect(written.json.labels, "the secrets it writes take its labels").toEqual({ team: "red" });
    const listed = await red.get("/v1/secrets");
    expect(listed.json.secrets.map((s: { reference: string }) => s.reference)).toEqual(["org/red-crm"]);
    const secrets = async () => (await tenant.api.get("/v1/secrets")).json.secrets.map((s: { reference: string; labels?: object }) => [s.reference, s.labels]);
    await expectRefused({ attempt: () => red.get("/v1/secrets/blue-crm/usage"), status: 404, slug: "not-found" });
    await expectRefused({ attempt: () => red.delete("/v1/secrets/blue-crm"), status: 404, slug: "not-found", probe: secrets });
    await expectRefused({ attempt: () => red.put("/v1/secrets/blue-crm", { value: "red's now" }), status: 403, slug: "forbidden", probe: secrets });
    await expectRefused({ attempt: () => red.put("/v1/secrets/other", { value: "x", labels: { team: "blue" } }), status: 403, slug: "forbidden", probe: secrets });
    // A namespace's secret serves the end user's runs whatever their labels: it reads them, never writes one.
    expectOk(await tenant.api.put("/v1/namespaces/labels/secrets/crm", { value: "the end user's" }), 201);
    expectOk(await red.get("/v1/namespaces/labels/secrets"), 200);
    await expectRefused({ attempt: () => red.put("/v1/namespaces/labels/secrets/crm", { value: "red's" }), status: 403, slug: "forbidden", probe: secrets });
    await expectRefused({ attempt: () => red.delete("/v1/namespaces/labels/secrets/crm"), status: 403, slug: "forbidden", probe: secrets });
    // Attaching: blue's is unknown to it, for a listing as for a run.
    const actionSet = { mcp: [{ name: "crm", url: "http://127.0.0.1:9/mcp", auth: { type: "secret", header: "authorization" } }] };
    const listing = await red.post("/v1/action-sets", { ...actionSet, secrets: { crm: "org/blue-crm" } });
    expectProblem(listing, 422, "secret-required");
    expect(listing.json.attachments[0]).toMatchObject({ server: "crm", problem: "unknown", secret: "org/blue-crm" });
    expect(listing.json.attachments[0].attachable).toEqual(["org/red-crm"]);
    const api = await tenant.api.post("/v1/action-sets", CRM_API);
    expectOk(api);
    const run = await red.ns("labels").post("/v1/runs", { script: CRM_SCRIPT, actionSet: api.json.id, secrets: { crm: "org/blue-crm" } });
    expectProblem(run, 422, "secret-required");
    expect(run.json.attachments[0]).toMatchObject({ server: "crm", problem: "unknown", secret: "org/blue-crm" });
    evidence("the red key lists org/red-crm only (written with team:red); blue's usage and deletion → 404, replacing it or labelling one blue → 403; a namespace's secret read, never written (403); org/blue-crm attached for a listing or a run → 422 secret-required (unknown), attachable org/red-crm");
  });

  test("attaching an organization's secret takes secrets:use", { tag: "@attack" }, async () => {
    const role = `starter-${unique()}`;
    expectOk(await tenant.api.put(`/v1/roles/${role}`, { description: "starts runs, registers actionSets", permissions: ["runs:start", "runs:read", "actions:write", "actions:read"] }));
    const starter = tenant.api.as((await createKey(tenant.api, role)).key);
    expectOk(await tenant.api.put("/v1/secrets/shared-crm", { value: "the organization's" }), 201);
    const listing = await starter.post("/v1/action-sets", {
      mcp: [{ name: "crm", url: "http://127.0.0.1:9/mcp", auth: { type: "secret", header: "authorization" } }],
      secrets: { crm: "org/shared-crm" },
    });
    expectProblem(listing, 403, "forbidden");
    expect(listing.json.detail).toContain("`secrets:use`");
    const actionSet = await tenant.api.post("/v1/action-sets", CRM_API);
    expectOk(actionSet);
    const started = async () => (await tenant.api.get("/v1/namespaces/use")).json?.namespace?.usage?.runsStarted ?? 0;
    const run = await expectRefused({
      attempt: () =>
        starter.ns("use").post("/v1/runs", { script: CRM_SCRIPT, actionSet: actionSet.json.id, secrets: { crm: "org/shared-crm" } }),
      status: 422,
      slug: "secret-required",
      probe: started,
    });
    expect(run.json.attachments[0]).toMatchObject({ server: "crm", problem: "forbidden", secret: "org/shared-crm" });
    evidence("a role without secrets:use: org/shared-crm for a listing → 403 naming secrets:use; for a run → 422 secret-required (forbidden), no run");
  });

  test("a key limited to labels reads no statistics, no scripts, and no payload without its run", { tag: "@attack" }, async () => {
    // Statistics come from hourly counters, which carry no labels: none would be the key's alone.
    for (const path of ["/v1/runs/stats", "/v1/runs/failures", "/v1/scripts", "/v1/actions/stats"]) {
      const reply = await red.get(path);
      expectProblem(reply, 403, "forbidden");
      expect(reply.text).not.toContain(blueRun);
    }
    // A payload belongs to its run: no caller reads one without naming it.
    expectProblem(await red.get(`/v1/payloads/${NO_DIGEST}`), 400, "invalid-request");
    evidence("GET /v1/runs/stats, /v1/runs/failures, /v1/scripts and /v1/actions/stats → 403 for a key limited to labels; a payload without its run → 400");
  });

  test("a worker key limited to labels is handed and answers the harness calls of its runs only", { tag: "@attack" }, async () => {
    // One action set for both teams, as two environments of a tenant register the same harness actions.
    const actionSet = await tenant.api.post("/v1/action-sets", {
      host: [{ name: "team.lookup", description: "Looks something up.", inputSchema: { type: "object" } }],
    });
    expectOk(actionSet);
    const script = `import { team } from "rewind:actions";
export default async function main() { return await team.lookup({}); }
`;
    const redWorker = new Api(stacks().a.url, (await createKey(tenant.api, "worker", { name: "red worker", allowedLabels: { team: "red" } })).key);
    const query = { actionSet: actionSet.json.id, actions: ["team.lookup"], slots: 10 };
    const reds = await TaskStream.open(redWorker, query);
    try {
      const ask = (labels: Record<string, string>) => startRun(tenant.api, "labels", { script, actionSet: actionSet.json.id, labels });
      const redAsks = await ask({ team: "red" });
      const blueAsks = await ask({ team: "blue" });
      expectOk(redAsks);
      expectOk(blueAsks);

      const ours = await reds.task("the red run's task", (task) => task.runId === redAsks.json.id);
      expect(ours.labels).toMatchObject({ team: "red" });

      // The blue task, handed to a worker of the tenant: the red worker is never handed it, and
      // cannot answer it.
      const all = await TaskStream.open(tenant.api, query);
      try {
        const blue = await all.task("the blue run's task", (task) => task.runId === blueAsks.json.id);
        expect(reds.handed.map((task) => task.runId), "the blue run's task is not the red worker's").toEqual([redAsks.json.id]);
        const probe = async () => (await tenant.api.get(`/v1/runs/${blueAsks.json.id}`)).json.status;
        await expectRefused({ attempt: () => redWorker.post(`/v1/calls/${blue.token}/complete`, { result: "from red" }), status: 410, slug: "call-gone", probe });
        await expectRefused({ attempt: () => redWorker.post(`/v1/calls/${blue.token}/defer`, {}), status: 410, slug: "call-gone", probe });

        expectOk(await redWorker.post(`/v1/calls/${ours.token}/complete`, { result: "red's" }), 204);
        expectOk(await tenant.api.post(`/v1/calls/${blue.token}/complete`, { result: "blue's" }), 204);
      } finally {
        await all.close();
      }
      for (const [run, answer] of [[redAsks.json.id, "red's"], [blueAsks.json.id, "blue's"]]) {
        const done = await tenant.api.get(`/v1/runs/${run}`, { query: { wait: "20s" } });
        expect(done.json.status).toBe("completed");
        expect(done.json.output.value).toBe(answer);
      }
    } finally {
      await reds.close();
    }
    evidence("the task stream of a worker key limited to team:red → the red run's task only; the blue task's token → 410 call-gone for it");
  });

  test("a subscription a key limited to labels creates carries them", { tag: "@success" }, async () => {
    const receiver = await startWebhookReceiver();
    try {
      const created = await red.post("/v1/subscriptions", { url: receiver.url });
      expectOk(created, 201);
      expect(created.json.labels).toMatchObject({ team: "red" });
      expectProblem(await red.post("/v1/subscriptions", { url: receiver.url, labels: { team: "blue" } }), 403, "forbidden");
      expectOk(await red.delete(`/v1/subscriptions/${created.json.id}`), 204);
    } finally {
      await receiver.close();
    }
    evidence("POST /v1/subscriptions by the red key → labels team:red; asking team:blue → 403");
  });
});

// ---------------------------------------------------------------------------------------------
// The operator

test.describe("the operator's endpoints", () => {
  let tenant: Tenant;

  test.beforeAll(async () => {
    tenant = await newTenant("access-operator");
  });

  test("an owner cannot set a ceiling, stop the server, nor make keys in another tenant", { tag: "@attack" }, async () => {
    const ceiling = async () => (await tenant.api.get("/v1/organization/namespace-defaults")).json;
    await expectRefused({
      attempt: () => tenant.api.put(`/v1/organizations/${tenant.id}/quotas`, { ceiling: { activeRuns: 1_000_000 } }),
      status: 403,
      slug: "forbidden",
      probe: ceiling,
    });
    expectProblem(await tenant.api.post("/v1/server/stop"), 403, "forbidden");
    const other = `access-elsewhere-${unique()}`;
    const reply = await tenant.api.post("/v1/api-keys", { name: "elsewhere", role: "owner", tenant: other });
    expectProblem(reply, 403, "forbidden");
    expect(reply.json.detail).toContain("operator");
    // The server still answers: it was not stopped.
    expectOk(await tenant.api.get("/v1/server"), 200);
    evidence("owner: PUT /v1/organizations/{own}/quotas, POST /v1/server/stop, a key in another tenant → 403 forbidden; the server still answers");
  });

  test("the operator sets a tenant's ceiling, and makes keys in any tenant", { tag: "@success" }, async () => {
    const set = await operator().put(`/v1/organizations/${tenant.id}/quotas`, { ceiling: { activeRuns: 7 } });
    expectOk(set);
    const seen = await tenant.api.get("/v1/organization/namespace-defaults");
    expectOk(seen, 200);
    expect(JSON.stringify(seen.json)).toContain('"activeRuns":7');
    const made = await operator().post("/v1/api-keys", { name: "reader by operator", role: "reader", tenant: tenant.id });
    expectOk(made, 201);
    const whoami = await tenant.api.as(made.json.key).get("/v1/whoami");
    expect(whoami.json.tenant).toBe(tenant.id);
    expect(whoami.json.operator).toBe(false);
    evidence("the operator's PUT /v1/organizations/{tenant}/quotas shows in the tenant's namespace defaults; its key of another tenant is that tenant's reader");
  });
});

// ---------------------------------------------------------------------------------------------
// Forged identity

test.describe("forged credentials", () => {
  let tenant: Tenant;

  test.beforeAll(async () => {
    tenant = await newTenant("access-forged");
  });

  test("no credential, or a malformed or unknown one, is refused with 401 and creates nothing", { tag: "@attack" }, async () => {
    const url = stacks().a.url;
    const actionSets = async () => (await tenant.api.get("/v1/action-sets")).json;
    const realKey = tenant.ownerKey;
    const attempts: [string, () => Promise<Reply>][] = [
      ["no Authorization", () => new Api(url).post("/v1/action-sets", {})],
      ["an empty Bearer", () => new Api(url).post("/v1/action-sets", {}, { headers: { authorization: "Bearer " } })],
      ["a random rewind_sk_", () => new Api(url, `rewind_sk_${randomBytes(24).toString("hex")}`).post("/v1/action-sets", {})],
      ["a real key with one character changed", () => new Api(url, `${realKey.slice(0, -1)}${realKey.endsWith("0") ? "1" : "0"}`).post("/v1/action-sets", {})],
      ["the key's id (its SHA-256) as a Bearer", async () => new Api(url, (await tenant.api.get("/v1/whoami")).json.principal.id).post("/v1/action-sets", {})],
      ["the key under Basic", () => new Api(url).post("/v1/action-sets", {}, { headers: { authorization: `Basic ${Buffer.from(`${realKey}:`).toString("base64")}` } })],
      ["the key in the query string", () => new Api(url).post("/v1/action-sets", {}, { query: { api_key: realKey, key: realKey } })],
      ["the key in a cookie", () => new Api(url).post("/v1/action-sets", {}, { headers: { cookie: `rewind_session=${realKey}; api_key=${realKey}` } })],
      ["two keys in one header", () => new Api(url).post("/v1/action-sets", {}, { headers: { authorization: `Bearer ${realKey}, Bearer ${realKey}` } })],
      ["a JWT that is not one", () => new Api(url, "a.b.c").post("/v1/action-sets", {})],
      ["an opaque token never minted", () => new Api(url, `rewind_nt_${randomBytes(24).toString("hex")}`).post("/v1/action-sets", {})],
      ["a key with a NUL byte", () => new Api(url, `${realKey}%00`).post("/v1/action-sets", {})],
    ];
    const seen: string[] = [];
    for (const [what, attempt] of attempts) {
      const reply = await expectRefused({ attempt, status: 401, slug: "unauthorized", probe: actionSets });
      seen.push(`${what} → ${reply.status} ${reply.slug}`);
    }
    evidence(seen.join("; "));
  });

  test("an access token shaped like WorkOS's is refused by a server without WorkOS", { tag: "@attack" }, async () => {
    const { privateKey } = await generateKeyPair("RS256");
    const now = Math.floor(Date.now() / 1000);
    const jwt = await new SignJWT({ sid: "session_forged", org_id: tenant.id, role: "owner", permissions: ["api_keys:write"] })
      .setProtectedHeader({ alg: "RS256", kid: "sso_oidc_key_pair_forged", typ: "JWT" })
      .setIssuer("https://api.workos.com")
      .setSubject("user_forged")
      .setIssuedAt(now)
      .setExpirationTime(now + 300)
      .sign(privateKey);
    const keys = async () => (await tenant.api.get("/v1/api-keys")).json.keys.length;
    for (const stack of [stacks().a, stacks().b]) {
      const api = new Api(stack.url, jwt);
      await expectRefused({ attempt: () => api.post("/v1/api-keys", { name: "forged", role: "owner" }), status: 401, slug: "unauthorized", probe: stack === stacks().a ? keys : undefined });
      expectProblem(await api.get("/v1/whoami"), 401, "unauthorized");
    }
    // Signed with an unregistered key under an rk_ kid: a customer's token Rewind cannot verify.
    const { privateKey: ed, publicKey } = await generateKeyPair("EdDSA", { crv: "Ed25519" });
    await exportJWK(publicKey);
    const customer = await new SignJWT({ sub: "alice", aud: "rewind", permissions: ["runs:read"] })
      .setProtectedHeader({ alg: "EdDSA", kid: `rk_${randomBytes(8).toString("hex")}` })
      .setIssuedAt(now)
      .setExpirationTime(now + 300)
      .sign(ed);
    expectProblem(await new Api(stacks().a.url, customer).get("/v1/runs"), 401, "unauthorized");
    evidence("an RS256 JWT with WorkOS's issuer and an org_id/role claim → 401 on setups A and B; an rk_ kid never registered → 401");
  });
});
