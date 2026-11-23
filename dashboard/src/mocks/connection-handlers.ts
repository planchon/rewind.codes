// The mocked `/v1/connections`, `/v1/organization/connections`, `/v1/oauth/consent` and
// `/v1/toolsets`, with the server's rules (archi/06, "Connections"; archi/07, "MCP and OAuth"): a
// connection is its person's, opens only organizations where the person holds the access given,
// and an administrator (`members:manage`) removes their organization from it.
import { HttpResponse, http } from "msw";
import type { ConnectionAccess, ConsentDecision, UpdateConnection } from "../model/types";
import { type Caller, isMember } from "./access";
import { authenticate, guard, json } from "./access-handlers";
import { emitChange, told } from "./changes";
import {
  accessesOf,
  connect,
  connectionDto,
  consentOrganizations,
  organizationsOfConnection,
  principalOfConnection,
  setOrganizations,
} from "./connections";
import type { World } from "./fixtures";
import { problem } from "./problem";

const ACCESS_NAMES = new Set<string>(["read", "run"]);

function labelsOf(value: unknown): Record<string, string> | null {
  if (value === undefined || value === null) return {};
  if (typeof value !== "object" || Array.isArray(value)) return null;
  const out: Record<string, string> = {};
  for (const [k, v] of Object.entries(value)) {
    if (typeof v !== "string") return null;
    out[k] = v;
  }
  return out;
}

export function connectionHandlers(world: World) {
  const access = world.access;
  const cw = world.connections;

  /** A member signed in, with or without organization. */
  const person = (request: Request): Extract<Caller, { kind: "user" }> | Response => {
    const caller = authenticate(world, request);
    if (caller instanceof Response) return caller;
    if (caller.kind !== "user")
      return problem(403, "forbidden", "Connections are a person's: sign in as a member.");
    return caller;
  };

  /** Each organization given: one the person holds the access in. */
  const checkChoices = (
    userId: string,
    organizations: { id?: unknown; access?: unknown }[],
  ): string | null => {
    const seen = new Set<string>();
    for (const o of organizations) {
      if (typeof o.id !== "string" || typeof o.access !== "string" || !ACCESS_NAMES.has(o.access))
        return "Each organization needs an id and an access, read or run.";
      if (seen.has(o.id)) return `${o.id} is given twice.`;
      seen.add(o.id);
      if (!isMember(access, userId, o.id)) return `You are not a member of ${o.id}.`;
      if (!accessesOf(access, userId, o.id).includes(o.access as ConnectionAccess))
        return `Your role in ${o.id} does not give the access ${o.access}: no one gives more than they hold.`;
    }
    return null;
  };

  const live = () => cw.connections.filter((c) => c.revokedAt === null);

  return [
    http.get("*/v1/toolsets", ({ request }) => {
      const caller = guard(world, request, "toolsets:read");
      if (caller instanceof Response) return caller;
      return HttpResponse.json({
        toolsets: caller.tenant === access.tenant ? cw.toolsets : [],
      });
    }),

    http.get("*/v1/connections", ({ request }) => {
      const caller = person(request);
      if (caller instanceof Response) return caller;
      const now = Date.now();
      const connections = live()
        .filter((c) => c.userId === caller.user.id)
        .map((c) => {
          // Claude lists the tools right after it connects.
          if (cw.simulateUse && c.lastUsedAt === null && now - c.createdAt > 3_000)
            c.lastUsedAt = c.createdAt + 3_000;
          return connectionDto(access, c);
        })
        .sort((a, b) => b.createdAt.localeCompare(a.createdAt));
      return HttpResponse.json({ connections });
    }),

    http.patch("*/v1/connections/:id", async ({ request, params }) => {
      const caller = person(request);
      if (caller instanceof Response) return caller;
      const connection = live().find((c) => c.id === params.id && c.userId === caller.user.id);
      if (!connection)
        return problem(404, "not-found", `You have no connection ${String(params.id)}.`);
      const body = await json<Partial<UpdateConnection>>(request);
      if (body instanceof Response) return body;
      if (!Array.isArray(body.organizations))
        return problem(400, "invalid-request", "Give the organizations it opens.");
      const why = checkChoices(caller.user.id, body.organizations);
      if (why) return problem(403, "forbidden", why);
      const organizations = [];
      for (const o of body.organizations) {
        const labels = labelsOf(o.allowedLabels);
        if (!labels) return problem(400, "invalid-request", "allowedLabels maps names to strings.");
        organizations.push({ id: o.id, access: o.access, allowedLabels: labels });
      }
      setOrganizations(access, connection, organizations);
      return told(
        world,
        { kind: "connection", id: connection.id },
        HttpResponse.json(connectionDto(access, connection)),
      );
    }),

    http.delete("*/v1/connections/:id", ({ request, params }) => {
      const caller = person(request);
      if (caller instanceof Response) return caller;
      const connection = live().find((c) => c.id === params.id && c.userId === caller.user.id);
      if (!connection)
        return problem(404, "not-found", `You have no connection ${String(params.id)}.`);
      connection.revokedAt = Date.now();
      setOrganizations(access, connection, []);
      return told(
        world,
        { kind: "connection", id: connection.id },
        new HttpResponse(null, { status: 204 }),
      );
    }),

    http.get("*/v1/organization/connections", ({ request }) => {
      const caller = guard(world, request, "members:read");
      if (caller instanceof Response) return caller;
      const tenant = caller.tenant;
      const connections = live()
        .filter((c) => organizationsOfConnection(access, c).some((o) => o.id === tenant))
        .map((c) => connectionDto(access, c, tenant))
        .sort((a, b) => (b.lastUsedAt ?? "").localeCompare(a.lastUsedAt ?? ""));
      return HttpResponse.json({ connections });
    }),

    http.delete("*/v1/organization/connections/:id", ({ request, params }) => {
      const caller = guard(world, request, "members:manage");
      if (caller instanceof Response) return caller;
      const tenant = caller.tenant ?? "";
      const connection = live().find((c) => c.id === params.id);
      const grants = access.grants.get(tenant);
      const principal = connection ? principalOfConnection(connection.id) : "";
      if (!connection || !grants?.has(principal))
        return problem(
          404,
          "not-found",
          `No connection ${String(params.id)} opens this organization.`,
        );
      grants.delete(principal);
      return told(
        world,
        { kind: "connection", id: connection.id },
        new HttpResponse(null, { status: 204 }),
      );
    }),

    http.get("*/v1/oauth/consent", ({ request }) => {
      const caller = person(request);
      if (caller instanceof Response) return caller;
      const id = new URL(request.url).searchParams.get("request") ?? "";
      const pending = cw.requests.get(id);
      if (!pending || pending.expiresAt <= Date.now())
        return problem(
          400,
          "invalid-request",
          "This authorization request is unknown or has expired: start again from the client.",
        );
      const redirect = new URL(pending.redirectUri);
      return HttpResponse.json({
        client: pending.client,
        redirectHost: redirect.host,
        loopback: ["localhost", "127.0.0.1", "[::1]"].includes(redirect.hostname),
        organizations: consentOrganizations(access, caller.user.id, caller.tenant),
        allowedLabels: pending.allowedLabels,
        expiresAt: new Date(pending.expiresAt).toISOString(),
      });
    }),

    http.post("*/v1/oauth/consent", async ({ request }) => {
      const caller = person(request);
      if (caller instanceof Response) return caller;
      const body = await json<Partial<ConsentDecision>>(request);
      if (body instanceof Response) return body;
      const pending = cw.requests.get(body.request ?? "");
      if (!pending || pending.expiresAt <= Date.now())
        return problem(
          400,
          "invalid-request",
          "This authorization request is unknown or has expired: start again from the client.",
        );
      const back = new URL(pending.redirectUri);
      back.searchParams.set("state", pending.state);
      if (body.decision === "deny") {
        cw.requests.delete(pending.id);
        back.searchParams.set("error", "access_denied");
        return HttpResponse.json({ redirectTo: back.toString() });
      }
      if (body.decision !== "allow")
        return problem(400, "invalid-request", "The decision is allow or deny.");
      const organizations = Array.isArray(body.organizations) ? body.organizations : [];
      if (organizations.length === 0)
        return problem(400, "invalid-request", "Allow at least one organization, or deny.");
      const why = checkChoices(caller.user.id, organizations);
      if (why) return problem(403, "forbidden", why);
      const labels = labelsOf(body.allowedLabels);
      if (!labels) return problem(400, "invalid-request", "allowedLabels maps names to strings.");
      cw.requests.delete(pending.id);
      // The server creates the connection when the client exchanges its code; the mock's client
      // exchanges it at once.
      const made = connect(access, cw, {
        userId: caller.user.id,
        client: pending.client,
        organizations: organizations.map((o) => ({
          id: o.id,
          access: o.access,
          allowedLabels: labels,
        })),
      });
      emitChange(world, { kind: "connection", id: made.id });
      back.searchParams.set("code", `mock_code_${pending.id.slice(0, 8)}`);
      return HttpResponse.json({ redirectTo: back.toString() });
    }),
  ];
}
