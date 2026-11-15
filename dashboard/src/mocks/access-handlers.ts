// The mocked `/auth` and the organization and access endpoints of `/v1`, over the access world.
import { HttpResponse, http } from "msw";
import type { Grant, Labels, Permission, PutRole } from "../api/types";
import { PERMISSIONS, roleNameProblem } from "../model/access";
import {
  acceptInvitation,
  afterVerified,
  apiKeyDto,
  authenticated,
  BUILT_IN,
  type Caller,
  catalog,
  grantsOf,
  HEX,
  invitationDto,
  isMember,
  isOperator,
  iso,
  keepsOwner,
  type MockKey,
  memberDtos,
  newId,
  permissionsOf,
  principalOf,
  random,
  refusal,
  rolePermissions,
  rolesOf,
  sendCode,
  sessionState,
  signIn,
  userByEmail,
  userOf,
  whoami,
} from "./access";
import type { World } from "./fixtures";
import { problem } from "./problem";

const DAY = 24 * 60 * 60_000;

/** Who calls `/v1`: the Bearer token, or the socket of a server that needs none. */
export function authenticate(world: World, request: Request): Caller | Response {
  const access = world.access;
  const open = !world.requireKey && !access.workos;
  const header = request.headers.get("Authorization");
  const token = header?.replace(/^Bearer\s+/i, "") ?? null;
  if (token) {
    const at = access.tokens.get(token);
    if (at) {
      if (at.expiresAt <= Date.now())
        return problem(
          401,
          "unauthorized",
          "The access token has expired: ask POST /auth/session for a fresh one.",
        );
      const user = userOf(access, at.userId)?.user;
      if (user) return { kind: "user", tenant: at.organizationId, user, token: at };
    }
    const key = access.keys.find((k) => k.secret === token && k.revokedAt === null);
    if (key) return { kind: "key", tenant: key.tenant, key };
  }
  if (open) return { kind: "socket", tenant: access.tenant };
  return problem(
    401,
    "unauthorized",
    token
      ? "This API key is unknown or revoked. Create a key with POST /v1/api-keys."
      : access.workos
        ? "Send a token: Authorization: Bearer <an access token from POST /auth/session, or an API key>."
        : "Send an API key: Authorization: Bearer rewind_sk_….",
  );
}

/** The caller, if it may use the permission. */
export function guard(
  world: World,
  request: Request,
  permission: Permission | null,
): Caller | Response {
  const caller = authenticate(world, request);
  if (caller instanceof Response || permission === null) return caller;
  const why = refusal(world.access, caller, permission);
  return why ? problem(403, "forbidden", why) : caller;
}

/** The runs the caller reaches: its tenant's, carrying its grant's allowed labels. */
export function runsWorld(world: World, caller: Caller): World {
  if (caller.tenant !== world.access.tenant) {
    return { ...world, runs: [], scripts: [], payloads: new Map() };
  }
  const grant =
    caller.kind === "socket"
      ? null
      : grantsOf(world.access, caller.tenant).get(principalOf(caller) ?? "");
  const pairs = Object.entries(grant?.allowedLabels ?? {});
  if (pairs.length === 0) return world;
  return { ...world, runs: world.runs.filter((r) => pairs.every(([k, v]) => r.labels[k] === v)) };
}

async function json<T>(request: Request): Promise<T | Response> {
  if (!request.headers.get("Content-Type")?.includes("application/json")) {
    return problem(
      415,
      "unsupported-media-type",
      "Send a JSON body: Content-Type: application/json.",
    );
  }
  try {
    return (await request.json()) as T;
  } catch {
    return problem(400, "invalid-request", "The body is not JSON.");
  }
}

function noContent(status = 204): HttpResponse<null> {
  return new HttpResponse(null, { status });
}

function redirect(location: string): HttpResponse<null> {
  return new HttpResponse(null, { status: 302, headers: { Location: location } });
}

export function accessHandlers(world: World) {
  const access = world.access;
  const noSignIn = () => problem(404, "not-found", "This server has no sign-in: use an API key.");
  const noSession = () => problem(401, "unauthorized", "No session, or one that expired: sign in.");

  /** No one gives more than they hold. */
  const exceeds = (caller: Caller, tenant: string, role: string): string | null => {
    if (isOperator(access, caller)) return null;
    const held = new Set(permissionsOf(access, caller));
    const missing = rolePermissions(access, tenant, role).filter((p) => !held.has(p));
    return missing.length === 0
      ? null
      : `No one gives more than they hold: the role ${role} gives ${missing.join(", ")}, which your role does not.`;
  };
  const roleExists = (tenant: string, role: string) =>
    rolesOf(access, tenant).some((r) => r.name === role);
  const labelsOf = (value: unknown): Labels =>
    value && typeof value === "object" ? (value as Labels) : {};
  const inviteLink = (token: string) => `/?invitation_token=${token}`;

  return [
    // Sign-in: `/auth`, only with WorkOS.
    http.all("*/auth/*", () => (access.workos ? undefined : noSignIn())),

    http.get("*/auth/config", () => HttpResponse.json(access.workos ?? undefined)),

    http.post("*/auth/magic-auth", async ({ request }) => {
      const body = await json<{ email?: string }>(request);
      if (body instanceof Response) return body;
      if (!body.email?.includes("@")) return problem(400, "invalid-request", "Give an email.");
      sendCode(access, body.email.trim());
      return noContent(202);
    }),

    http.post("*/auth/magic-auth/verify", async ({ request }) => {
      const body = await json<{ email?: string; code?: string; invitationToken?: string }>(request);
      if (body instanceof Response) return body;
      const email = body.email?.trim().toLowerCase() ?? "";
      if (!body.code || access.codes.get(email) !== body.code) {
        return problem(
          400,
          "invalid-request",
          "The code is wrong or has expired: ask for another with POST /auth/magic-auth.",
        );
      }
      access.codes.delete(email);
      const state = signIn(access, email, body.invitationToken ?? null);
      if (typeof state === "string") return problem(400, "invalid-request", state);
      return HttpResponse.json(state);
    }),

    http.post("*/auth/email-verification", async ({ request }) => {
      const body = await json<{ code?: string }>(request);
      if (body instanceof Response) return body;
      const session = access.session;
      if (session?.step !== "email_verification_required") return noSession();
      const account = userOf(access, session.userId);
      if (!account) return noSession();
      const email = account.user.email.toLowerCase();
      if (!body.code || access.codes.get(email) !== body.code)
        return problem(400, "invalid-request", "The code is wrong or has expired.");
      access.codes.delete(email);
      account.verified = true;
      return HttpResponse.json(afterVerified(access, account.user, session.organizationId));
    }),

    http.post("*/auth/organization", async ({ request }) => {
      const body = await json<{ organizationId?: string }>(request);
      if (body instanceof Response) return body;
      const session = access.session;
      if (session?.step !== "organization_selection_required") return noSession();
      const user = userOf(access, session.userId)?.user;
      if (!user) return noSession();
      if (!body.organizationId || !isMember(access, user.id, body.organizationId))
        return problem(403, "forbidden", "The user is not a member of this organization.");
      return HttpResponse.json(authenticated(access, user, body.organizationId));
    }),

    http.post("*/auth/session", async ({ request }) => {
      const body = await json<{ organizationId?: string }>(request);
      if (body instanceof Response) return body;
      const session = access.session;
      if (body.organizationId) {
        if (session?.step !== "authenticated") return noSession();
        const user = userOf(access, session.userId)?.user;
        if (!user) return noSession();
        if (!isMember(access, user.id, body.organizationId))
          return problem(403, "forbidden", "The user is not a member of this organization.");
        return HttpResponse.json(authenticated(access, user, body.organizationId));
      }
      const state = sessionState(access);
      return state ? HttpResponse.json(state) : noSession();
    }),

    http.post("*/auth/waitlist", async ({ request }) => {
      const body = await json<{ email?: string }>(request);
      if (body instanceof Response) return body;
      if (!body.email?.includes("@")) return problem(400, "invalid-request", "Give an email.");
      if (!access.waitlist.includes(body.email)) access.waitlist.push(body.email);
      access.log(`[mock] ${body.email} joined the waitlist`);
      return noContent(202);
    }),

    http.post("*/auth/logout", async ({ request }) => {
      const body = await json<unknown>(request);
      if (body instanceof Response) return body;
      if (!access.session) return noSession();
      // The access tokens stay valid to their expiry, as WorkOS's JWTs.
      access.session = null;
      return noContent();
    }),

    http.get("*/auth/github", ({ request }) => {
      const state = random(HEX, 16);
      const invitationToken = new URL(request.url).searchParams.get("invitationToken");
      access.oauth = { state, invitationToken };
      // GitHub would ask the person; the mock goes straight back with a code.
      return redirect(`/auth/callback?code=mock&state=${state}`);
    }),

    http.get("*/auth/callback", ({ request }) => {
      const state = new URL(request.url).searchParams.get("state");
      const oauth = access.oauth;
      access.oauth = null;
      access.session = null;
      if (oauth && state === oauth.state) signIn(access, access.githubEmail, oauth.invitationToken);
      return redirect("/#/");
    }),

    // Who calls, the catalog, the onboarding.
    http.get("*/v1/whoami", ({ request }) => {
      const caller = authenticate(world, request);
      return caller instanceof Response ? caller : HttpResponse.json(whoami(access, caller));
    }),

    http.get("*/v1/permissions", ({ request }) => {
      const caller = authenticate(world, request);
      return caller instanceof Response ? caller : HttpResponse.json({ permissions: catalog() });
    }),

    http.post("*/v1/organizations", async ({ request }) => {
      const caller = authenticate(world, request);
      if (caller instanceof Response) return caller;
      if (caller.kind !== "user")
        return problem(403, "forbidden", "Only a member signed in creates an organization.");
      const body = await json<{ name?: string }>(request);
      if (body instanceof Response) return body;
      const name = body.name?.trim();
      if (!name) return problem(400, "invalid-request", "Give the organization a name.");
      const org = { id: newId("org"), name, defaultRole: "reader" };
      access.organizations.push(org);
      access.memberships.push({ userId: caller.user.id, organizationId: org.id, status: "active" });
      grantsOf(access, org.id).set(`user:${caller.user.id}`, {
        role: "owner",
        allowedLabels: {},
        grantedAt: iso(Date.now()),
      });
      return HttpResponse.json({ id: org.id, name }, { status: 201 });
    }),

    http.get("*/v1/invitations", ({ request }) => {
      const caller = authenticate(world, request);
      if (caller instanceof Response) return caller;
      if (caller.kind !== "user")
        return problem(403, "forbidden", "Only a member has invitations.");
      const email = caller.user.email.toLowerCase();
      const invitations = access.invitations
        .filter((i) => i.email.toLowerCase() === email)
        .map((i) => invitationDto(access, i))
        .filter((i) => i.state === "pending");
      return HttpResponse.json({ invitations });
    }),

    http.post("*/v1/invitations/:id/accept", ({ request, params }) => {
      const caller = authenticate(world, request);
      if (caller instanceof Response) return caller;
      if (caller.kind !== "user")
        return problem(403, "forbidden", "Only a member accepts an invitation.");
      const inv = access.invitations.find(
        (i) =>
          i.id === params.id &&
          i.email.toLowerCase() === caller.user.email.toLowerCase() &&
          invitationDto(access, i).state === "pending",
      );
      if (!inv)
        return problem(404, "not-found", `No pending invitation ${String(params.id)} for you.`);
      acceptInvitation(access, inv, caller.user.id);
      return HttpResponse.json(invitationDto(access, inv));
    }),

    // The organization.
    http.patch("*/v1/organization", async ({ request }) => {
      const caller = guard(world, request, "organization:write");
      if (caller instanceof Response) return caller;
      const org = access.organizations.find((o) => o.id === caller.tenant);
      if (!org) return problem(404, "not-found", "This tenant is no organization.");
      const body = await json<{ name?: string | null; defaultRole?: string | null }>(request);
      if (body instanceof Response) return body;
      if (body.name !== undefined && body.name !== null && !body.name.trim())
        return problem(400, "invalid-request", "An organization's name cannot be empty.");
      if (body.defaultRole && !roleExists(org.id, body.defaultRole))
        return problem(404, "not-found", `No role ${body.defaultRole} in this organization.`);
      if (body.name) org.name = body.name.trim();
      if (body.defaultRole) org.defaultRole = body.defaultRole;
      return HttpResponse.json({ id: org.id, name: org.name, defaultRole: org.defaultRole });
    }),

    http.get("*/v1/organization/members", ({ request }) => {
      const caller = guard(world, request, "members:read");
      if (caller instanceof Response) return caller;
      return HttpResponse.json({ members: caller.tenant ? memberDtos(access, caller.tenant) : [] });
    }),

    http.put("*/v1/organization/members/:user/grant", async ({ request, params }) => {
      const caller = guard(world, request, "members:manage");
      if (caller instanceof Response) return caller;
      const tenant = caller.tenant ?? "";
      const userId = String(params.user);
      if (!access.memberships.some((m) => m.userId === userId && m.organizationId === tenant))
        return problem(404, "not-found", `No member ${userId} in this organization.`);
      const body = await json<{ role?: string; allowedLabels?: Labels }>(request);
      if (body instanceof Response) return body;
      const role = body.role ?? "";
      if (!roleExists(tenant, role))
        return problem(404, "not-found", `No role ${role} in this organization.`);
      const principal = `user:${userId}`;
      const current = grantsOf(access, tenant).get(principal);
      const why =
        exceeds(caller, tenant, role) ?? (current ? exceeds(caller, tenant, current.role) : null);
      if (why) return problem(403, "forbidden", why);
      const next: Grant = {
        role,
        allowedLabels: labelsOf(body.allowedLabels),
        grantedAt: iso(Date.now()),
        ...(principalOf(caller) ? { grantedBy: principalOf(caller) } : {}),
      };
      if (!keepsOwner(access, tenant, principal, next))
        return problem(
          409,
          "last-owner",
          "This would leave the organization without an owner: another member must hold the role owner first.",
        );
      grantsOf(access, tenant).set(principal, next);
      return HttpResponse.json(next);
    }),

    http.delete("*/v1/organization/members/:user", ({ request, params }) => {
      const caller = guard(world, request, "members:manage");
      if (caller instanceof Response) return caller;
      const tenant = caller.tenant ?? "";
      const userId = String(params.user);
      const at = access.memberships.findIndex(
        (m) => m.userId === userId && m.organizationId === tenant,
      );
      if (at < 0) return problem(404, "not-found", `No member ${userId} in this organization.`);
      const principal = `user:${userId}`;
      const current = grantsOf(access, tenant).get(principal);
      const why = current ? exceeds(caller, tenant, current.role) : null;
      if (why) return problem(403, "forbidden", why);
      if (!keepsOwner(access, tenant, principal, null))
        return problem(
          409,
          "last-owner",
          "This would leave the organization without an owner: give the role owner to another member first.",
        );
      grantsOf(access, tenant).delete(principal);
      access.memberships.splice(at, 1);
      return noContent();
    }),

    http.get("*/v1/organization/invitations", ({ request }) => {
      const caller = guard(world, request, "members:read");
      if (caller instanceof Response) return caller;
      const invitations = access.invitations
        .filter((i) => i.organizationId === caller.tenant)
        .map((i) => invitationDto(access, i));
      return HttpResponse.json({ invitations });
    }),

    http.post("*/v1/organization/invitations", async ({ request }) => {
      const caller = guard(world, request, "members:invite");
      if (caller instanceof Response) return caller;
      const tenant = caller.tenant ?? "";
      const body = await json<{ email?: string; role?: string; allowedLabels?: Labels }>(request);
      if (body instanceof Response) return body;
      const email = body.email?.trim() ?? "";
      if (!email.includes("@")) return problem(400, "invalid-request", "Give an email to invite.");
      const role = body.role ?? "";
      if (!roleExists(tenant, role))
        return problem(404, "not-found", `No role ${role} in this organization.`);
      const why = exceeds(caller, tenant, role);
      if (why) return problem(403, "forbidden", why);
      // WorkOS creates the invited user at once.
      let account = userByEmail(access, email);
      if (!account) {
        account = { user: { id: newId("user"), email: email.toLowerCase() }, verified: true };
        access.users.push(account);
      }
      const userId = account.user.id;
      if (!access.memberships.some((m) => m.userId === userId && m.organizationId === tenant))
        access.memberships.push({ userId, organizationId: tenant, status: "pending" });
      const now = Date.now();
      const inv = {
        id: newId("invitation"),
        token: random(HEX, 24),
        email,
        organizationId: tenant,
        inviter: caller.kind === "user" ? caller.user.id : null,
        state: "pending" as const,
        createdAt: now,
        expiresAt: now + 7 * DAY,
      };
      access.invitations.push(inv);
      grantsOf(access, tenant).set(`invitation:${inv.id}`, {
        role,
        allowedLabels: labelsOf(body.allowedLabels),
        grantedAt: iso(now),
        ...(principalOf(caller) ? { grantedBy: principalOf(caller) } : {}),
      });
      access.log(`[mock] invitation for ${email}: ${inviteLink(inv.token)}`);
      return HttpResponse.json(invitationDto(access, inv), { status: 201 });
    }),

    http.post("*/v1/organization/invitations/:id/resend", ({ request, params }) => {
      const caller = guard(world, request, "members:invite");
      if (caller instanceof Response) return caller;
      const inv = access.invitations.find(
        (i) => i.id === params.id && i.organizationId === caller.tenant,
      );
      if (!inv || (inv.state !== "pending" && inv.state !== "expired"))
        return problem(404, "not-found", "No such invitation, or one already accepted or revoked.");
      inv.state = "pending";
      inv.expiresAt = Date.now() + 7 * DAY;
      access.log(`[mock] invitation for ${inv.email}, again: ${inviteLink(inv.token)}`);
      return HttpResponse.json(invitationDto(access, inv));
    }),

    http.delete("*/v1/organization/invitations/:id", ({ request, params }) => {
      const caller = guard(world, request, "members:invite");
      if (caller instanceof Response) return caller;
      const inv = access.invitations.find(
        (i) => i.id === params.id && i.organizationId === caller.tenant,
      );
      if (!inv || invitationDto(access, inv).state !== "pending")
        return problem(404, "not-found", "No such pending invitation.");
      inv.state = "revoked";
      grantsOf(access, inv.organizationId).delete(`invitation:${inv.id}`);
      const pending = access.memberships.findIndex(
        (m) =>
          m.organizationId === inv.organizationId &&
          m.status === "pending" &&
          userOf(access, m.userId)?.user.email.toLowerCase() === inv.email.toLowerCase(),
      );
      if (pending >= 0) access.memberships.splice(pending, 1);
      return noContent();
    }),

    // Roles.
    http.get("*/v1/roles", ({ request }) => {
      const caller = guard(world, request, "roles:read");
      if (caller instanceof Response) return caller;
      return HttpResponse.json({ roles: rolesOf(access, caller.tenant ?? "") });
    }),

    http.put("*/v1/roles/:name", async ({ request, params }) => {
      const caller = guard(world, request, "roles:write");
      if (caller instanceof Response) return caller;
      const tenant = caller.tenant ?? "";
      const name = String(params.name);
      const invalid = roleNameProblem(name);
      if (invalid) return problem(400, "invalid-request", `Invalid role name ${name}: ${invalid}`);
      const body = await json<PutRole>(request);
      if (body instanceof Response) return body;
      const permissions = body.permissions ?? [];
      const unknown = permissions.find((p) => !PERMISSIONS.includes(p));
      if (unknown)
        return problem(
          400,
          "invalid-request",
          `Unknown permission ${unknown}: GET /v1/permissions lists the catalog.`,
        );
      const held = new Set(permissionsOf(access, caller));
      const missing = isOperator(access, caller) ? [] : permissions.filter((p) => !held.has(p));
      if (missing.length > 0)
        return problem(
          403,
          "forbidden",
          `Writing a role needs every permission it gives: you do not hold ${missing.join(", ")}.`,
        );
      const own = access.roles.get(tenant) ?? [];
      access.roles.set(tenant, own);
      const now = iso(Date.now());
      const before = own.find((r) => r.name === name);
      const role = {
        name,
        builtIn: false,
        description: body.description ?? "",
        permissions: PERMISSIONS.filter((p) => permissions.includes(p)),
        createdAt: before?.createdAt ?? now,
        updatedAt: now,
      };
      if (before) own.splice(own.indexOf(before), 1, role);
      else own.push(role);
      return HttpResponse.json(role, { status: before ? 200 : 201 });
    }),

    http.delete("*/v1/roles/:name", ({ request, params }) => {
      const caller = guard(world, request, "roles:write");
      if (caller instanceof Response) return caller;
      const tenant = caller.tenant ?? "";
      const name = String(params.name);
      const own = access.roles.get(tenant) ?? [];
      const at = own.findIndex((r) => r.name === name);
      if (at < 0 || BUILT_IN.some((r) => r.name === name))
        return problem(404, "not-found", `No role ${name} of this organization.`);
      const holders = [...grantsOf(access, tenant).values()].filter((g) => g.role === name).length;
      if (holders > 0)
        return problem(
          409,
          "role-in-use",
          `${holders} grant${holders === 1 ? "" : "s"} still point${holders === 1 ? "s" : ""} to the role ${name}: give their members, keys or invitations another role first.`,
        );
      own.splice(at, 1);
      return noContent();
    }),

    // API keys.
    http.get("*/v1/api-keys", ({ request }) => {
      const caller = guard(world, request, "api_keys:read");
      if (caller instanceof Response) return caller;
      const keys = access.keys
        .filter((k) => k.tenant === caller.tenant)
        .sort((a, b) => a.createdAt - b.createdAt)
        .map((k) => apiKeyDto(access, k));
      return HttpResponse.json({ keys });
    }),

    http.post("*/v1/api-keys", async ({ request }) => {
      const caller = guard(world, request, "api_keys:write");
      if (caller instanceof Response) return caller;
      const body = await json<{
        name?: string;
        role?: string;
        allowedLabels?: Labels;
        tenant?: string | null;
      }>(request);
      if (body instanceof Response) return body;
      const operator = isOperator(access, caller);
      if (body.tenant && !operator)
        return problem(403, "forbidden", "Only the operator creates a key for another tenant.");
      const tenant = body.tenant || caller.tenant || "";
      const name = body.name?.trim();
      if (!name) return problem(400, "invalid-request", "Give the key a name.");
      const role = body.role ?? "";
      if (role === "operator" && !operator)
        return problem(403, "forbidden", "Only the operator creates an operator key.");
      if (role !== "operator" && !roleExists(tenant, role))
        return problem(404, "not-found", `No role ${role} in ${tenant}.`);
      const why = exceeds(caller, tenant, role);
      if (why) return problem(403, "forbidden", why);
      const now = Date.now();
      const key: MockKey = {
        id: random(HEX, 64),
        secret: `rewind_sk_${random(HEX, 32)}`,
        tenant,
        name,
        createdAt: now,
        createdBy: principalOf(caller),
        revokedAt: null,
      };
      access.keys.push(key);
      grantsOf(access, tenant).set(`key:${key.id}`, {
        role,
        allowedLabels: labelsOf(body.allowedLabels),
        grantedAt: iso(now),
        ...(principalOf(caller) ? { grantedBy: principalOf(caller) } : {}),
      });
      return HttpResponse.json(apiKeyDto(access, key, true), { status: 201 });
    }),

    http.delete("*/v1/api-keys/:id", ({ request, params }) => {
      const caller = guard(world, request, "api_keys:write");
      if (caller instanceof Response) return caller;
      const key = access.keys.find((k) => k.id === params.id && k.tenant === caller.tenant);
      if (!key) return problem(404, "not-found", "No such key.");
      key.revokedAt ??= Date.now();
      return noContent();
    }),
  ];
}
