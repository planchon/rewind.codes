// The mocked identity and access: a WorkOS of users, organizations, memberships, invitations and
// a waitlist, with one browser's session; and Rewind's catalog, roles, grants and API keys. The
// rules are the server's (archi/06, "What the caller may do"): no one gives more than they hold,
// an organization keeps an owner, a role in use stays, a grant limited to labels cannot
// administer. What a deployment sends by email (codes, invitation links) goes to `log`.
import type {
  ApiKey,
  AuthConfig,
  AuthState,
  Grant,
  Identity,
  Invitation,
  Labels,
  Member,
  MembershipStatus,
  Organization,
  Permission,
  PermissionInfo,
  Role,
  User,
} from "../api/types";
import { ADMIN_PERMISSIONS, PERMISSIONS } from "../model/access";

const MINUTE = 60_000;
const DAY = 24 * 60 * MINUTE;

export const DESCRIPTIONS: Record<Permission, string> = {
  "runs:read":
    "Read runs, their statistics and failure groups, a run's state, trace, entries, event stream and journal, the scripts, the tool statistics and payloads.",
  "runs:start": "Start and fork runs, and check and test scripts (`/v1/check`, `/v1/test`).",
  "runs:control": "Send signals to runs and cancel them.",
  "runs:quarantine": "Release a quarantined run, or give it up.",
  "runs:delete": "Erase a run, its payloads included.",
  "toolsets:read": "List toolsets and workflows, read their declarations and search their tools.",
  "toolsets:write": "Register a toolset, and save a workflow.",
  "credentials:write": "Set the credentials of a toolset or a run.",
  "tasks:work": "Claim host-tool tasks, and heartbeat, defer, complete or fail them.",
  "tasks:approve": "Approve or reject a parked task.",
  "subscriptions:read": "List the lifecycle webhooks.",
  "subscriptions:write": "Create and delete lifecycle webhooks.",
  "members:read": "List the members, their grants and the invitations.",
  "members:invite": "Invite people, and resend and revoke invitations.",
  "members:manage": "Change a member's grant, and remove a member.",
  "roles:read": "List the roles and their permissions.",
  "roles:write": "Create, change and delete the organization's own roles.",
  "api_keys:read": "List the tenant's API keys.",
  "api_keys:write": "Create and revoke API keys.",
  "organization:write": "Rename the organization and set its default role.",
};

const HARNESS: Permission[] = [
  "runs:read",
  "runs:start",
  "runs:control",
  "runs:quarantine",
  "runs:delete",
  "toolsets:read",
  "toolsets:write",
  "credentials:write",
  "tasks:work",
  "tasks:approve",
  "subscriptions:read",
  "subscriptions:write",
  "members:read",
  "roles:read",
];

export const BUILT_IN: Role[] = [
  {
    name: "owner",
    builtIn: true,
    description: "Every permission: administers the organization.",
    permissions: [...PERMISSIONS],
  },
  {
    name: "harness",
    builtIn: true,
    description:
      "Runs, toolsets, credentials, tasks and subscriptions, and reading the members and roles: a harness and its agent tools.",
    permissions: HARNESS,
  },
  {
    name: "worker",
    builtIn: true,
    description: "Only the host-tool tasks: a harness's SDK worker.",
    permissions: ["tasks:work"],
  },
  {
    name: "reader",
    builtIn: true,
    description: "Reading runs, toolsets, subscriptions, members and roles: a dashboard.",
    permissions: ["runs:read", "toolsets:read", "subscriptions:read", "members:read", "roles:read"],
  },
];

export interface MockUser {
  user: User;
  /** WorkOS asks to verify the email at the next sign-in. */
  verified: boolean;
}

export interface MockOrganization {
  id: string;
  name: string;
  defaultRole: string;
}

export interface MockMembership {
  userId: string;
  organizationId: string;
  status: MembershipStatus;
}

export interface MockInvitation {
  id: string;
  /** The token of the invitation link, `/?invitation_token=…`. */
  token: string;
  email: string;
  organizationId: string;
  inviter: string | null;
  state: Invitation["state"];
  createdAt: number;
  expiresAt: number;
}

export interface MockKey {
  id: string;
  secret: string;
  tenant: string;
  name: string;
  createdAt: number;
  createdBy: string | null;
  revokedAt: number | null;
}

export interface MockToken {
  token: string;
  userId: string;
  organizationId: string | null;
  expiresAt: number;
}

/** What the session cookie holds: signed in, or the step of a sign-in. */
export type MockSession =
  | { step: "authenticated"; userId: string; organizationId: string | null; token: string }
  | { step: "organization_selection_required"; userId: string }
  | { step: "email_verification_required"; userId: string; organizationId: string | null }
  | { step: "sign_up_closed"; email: string | null };

export interface AccessWorld {
  /** `GET /auth/config`'s answer; `null` without WorkOS (404). */
  workos: AuthConfig | null;
  /** The tenant the world's runs belong to; other tenants have none. */
  tenant: string;
  users: MockUser[];
  organizations: MockOrganization[];
  memberships: MockMembership[];
  invitations: MockInvitation[];
  /** The organizations' own roles, by tenant. */
  roles: Map<string, Role[]>;
  /** Grants by tenant, then by principal (`user:<id>`, `key:<id>`, `invitation:<id>`). */
  grants: Map<string, Map<string, Grant>>;
  keys: MockKey[];
  waitlist: string[];
  /** The last code sent to each email. */
  codes: Map<string, string>;
  /** The one browser's session cookie. */
  session: MockSession | null;
  tokens: Map<string, MockToken>;
  oauth: { state: string; invitationToken: string | null } | null;
  tokenTtlMs: number;
  /** The email of the account GitHub signs in. */
  githubEmail: string;
  log: (line: string) => void;
}

export interface AccessOptions {
  /** WorkOS is configured: `/auth` answers. `true` for both methods and the waitlist. */
  workos?: boolean | Partial<AuthConfig>;
  /** The operator's key (`--admin-key`): with it, `/v1` needs a key. */
  requireKey?: string | undefined;
  /** Access tokens live 5 minutes, as WorkOS's. */
  tokenTtlMs?: number;
  githubEmail?: string;
  log?: (line: string) => void;
}

function random(alphabet: string, length: number): string {
  const bytes = new Uint8Array(length);
  crypto.getRandomValues(bytes);
  return Array.from(bytes, (b) => alphabet[b % alphabet.length]).join("");
}

const HEX = "0123456789abcdef";
const BASE32 = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";

export function newId(prefix: string): string {
  return `${prefix}_01${random(BASE32, 24)}`;
}

function iso(ms: number): string {
  return new Date(ms).toISOString();
}

export function createAccessWorld(options: AccessOptions = {}): AccessWorld {
  const now = Date.now();
  const workos =
    options.workos === undefined || options.workos === false
      ? null
      : {
          methods: ["github", "magic_auth"] as AuthConfig["methods"],
          waitlist: true,
          ...(options.workos === true ? {} : options.workos),
        };
  const world: AccessWorld = {
    workos,
    tenant: workos ? "org_01ACME" : "local",
    users: [],
    organizations: [],
    memberships: [],
    invitations: [],
    roles: new Map(),
    grants: new Map(),
    keys: [],
    waitlist: [],
    codes: new Map(),
    session: null,
    tokens: new Map(),
    oauth: null,
    tokenTtlMs: options.tokenTtlMs ?? 5 * MINUTE,
    githubEmail: options.githubEmail ?? "ada@example.net",
    log: options.log ?? (() => {}),
  };
  const grant = (tenant: string, principal: string, g: Grant) => {
    let map = world.grants.get(tenant);
    if (!map) {
      map = new Map();
      world.grants.set(tenant, map);
    }
    map.set(principal, g);
  };
  const key = (
    tenant: string,
    name: string,
    role: string,
    createdBy: string | null,
    ageMs: number,
    allowedLabels: Labels = {},
    revokedAgoMs: number | null = null,
  ) => {
    const k: MockKey = {
      id: random(HEX, 64),
      secret: `rewind_sk_${random(HEX, 32)}`,
      tenant,
      name,
      createdAt: now - ageMs,
      createdBy,
      revokedAt: revokedAgoMs === null ? null : now - revokedAgoMs,
    };
    world.keys.push(k);
    grant(tenant, `key:${k.id}`, {
      role,
      allowedLabels,
      grantedAt: iso(k.createdAt),
      grantedBy: createdBy,
    });
    return k;
  };

  if (!workos) {
    if (options.requireKey) {
      // The operator's key given at start: listed, with no creator.
      const k = key("local", "operator", "operator", null, 30 * DAY);
      k.secret = options.requireKey;
    }
    key("local", "ci", "harness", null, 12 * DAY);
    return world;
  }

  const person = (id: string, first: string, last: string, verified = true) => {
    const user: User = {
      id,
      email: `${first.toLowerCase()}@example.net`,
      firstName: first,
      lastName: last,
    };
    world.users.push({ user, verified });
    return user;
  };
  const ada = person("user_01ADA", "Ada", "Lovelace");
  const grace = person("user_01GRACE", "Grace", "Hopper");
  const linus = person("user_01LINUS", "Linus", "Torvalds");
  const alan = person("user_01ALAN", "Alan", "Turing", false);
  const barbara = person("user_01BARBARA", "Barbara", "Liskov");
  const margaret = person("user_01MARGARET", "Margaret", "Hamilton");
  const katherine = person("user_01KATHERINE", "Katherine", "Johnson");

  world.organizations.push(
    { id: "org_01ACME", name: "Acme", defaultRole: "reader" },
    { id: "org_01GLOBEX", name: "Globex", defaultRole: "reader" },
  );
  const member = (user: User, org: string, status: MembershipStatus = "active") =>
    world.memberships.push({ userId: user.id, organizationId: org, status });
  member(ada, "org_01ACME");
  member(grace, "org_01ACME");
  member(linus, "org_01ACME");
  member(alan, "org_01ACME");
  member(katherine, "org_01ACME", "pending");
  member(ada, "org_01GLOBEX");
  member(barbara, "org_01GLOBEX");
  member(margaret, "org_01GLOBEX", "pending");

  world.roles.set("org_01ACME", [
    {
      name: "auditor",
      builtIn: false,
      description: "Reviews who has access: members, roles and keys; no runs.",
      permissions: ["members:read", "roles:read", "api_keys:read"],
      createdAt: iso(now - 9 * DAY),
      updatedAt: iso(now - 2 * DAY),
    },
    {
      name: "on-call",
      builtIn: false,
      description: "Reads runs, sends signals, cancels, approves parked tasks.",
      permissions: ["runs:read", "runs:control", "tasks:approve", "toolsets:read"],
      createdAt: iso(now - 4 * DAY),
      updatedAt: iso(now - 4 * DAY),
    },
  ]);

  const by = (user: User) => `user:${user.id}`;
  grant("org_01ACME", by(ada), {
    role: "owner",
    allowedLabels: {},
    grantedAt: iso(now - 40 * DAY),
  });
  grant("org_01ACME", by(grace), {
    role: "harness",
    allowedLabels: {},
    grantedAt: iso(now - 30 * DAY),
    grantedBy: by(ada),
  });
  grant("org_01ACME", by(linus), {
    role: "auditor",
    allowedLabels: {},
    grantedAt: iso(now - 9 * DAY),
    grantedBy: by(ada),
  });
  grant("org_01ACME", by(alan), {
    role: "reader",
    allowedLabels: { agent: "triage" },
    grantedAt: iso(now - 6 * DAY),
    grantedBy: by(grace),
  });
  grant("org_01GLOBEX", by(barbara), {
    role: "owner",
    allowedLabels: {},
    grantedAt: iso(now - 60 * DAY),
  });
  grant("org_01GLOBEX", by(ada), {
    role: "reader",
    allowedLabels: {},
    grantedAt: iso(now - 20 * DAY),
  });

  const invite = (
    email: string,
    org: string,
    inviter: User,
    role: string,
    ageMs: number,
    state: Invitation["state"] = "pending",
  ) => {
    const inv: MockInvitation = {
      id: newId("invitation"),
      token: random(HEX, 24),
      email,
      organizationId: org,
      inviter: inviter.id,
      state,
      createdAt: now - ageMs,
      expiresAt: now - ageMs + 7 * DAY,
    };
    world.invitations.push(inv);
    if (state === "pending" || state === "expired")
      grant(org, `invitation:${inv.id}`, {
        role,
        allowedLabels: {},
        grantedAt: iso(inv.createdAt),
        grantedBy: `user:${inviter.id}`,
      });
    return inv;
  };
  invite(katherine.email, "org_01ACME", ada, "reader", 2 * DAY);
  invite("dorothy@example.net", "org_01ACME", grace, "harness", 9 * DAY, "expired");
  invite(margaret.email, "org_01GLOBEX", barbara, "harness", DAY);

  const ci = key("org_01ACME", "ci", "harness", by(ada), 20 * DAY);
  key("org_01ACME", "host-worker", "worker", `key:${ci.id}`, 15 * DAY);
  key("org_01ACME", "triage-agent", "reader", by(grace), 8 * DAY, { agent: "triage" });
  key("org_01ACME", "old-ci", "harness", by(ada), 60 * DAY, {}, 3 * DAY);
  key("org_01GLOBEX", "globex-harness", "harness", by(barbara), 5 * DAY);
  return world;
}

// --------------------------------------------------------------------------------------------
// Roles and grants

export function rolesOf(world: AccessWorld, tenant: string): Role[] {
  const own = [...(world.roles.get(tenant) ?? [])].sort((a, b) => a.name.localeCompare(b.name));
  return [...BUILT_IN, ...own];
}

export function rolePermissions(world: AccessWorld, tenant: string, role: string): Permission[] {
  if (role === "operator") return [...PERMISSIONS];
  return rolesOf(world, tenant).find((r) => r.name === role)?.permissions ?? [];
}

export function grantsOf(world: AccessWorld, tenant: string): Map<string, Grant> {
  let map = world.grants.get(tenant);
  if (!map) {
    map = new Map();
    world.grants.set(tenant, map);
  }
  return map;
}

/** Whether the write leaves the tenant with a member (`user:`) holding `owner`. */
export function keepsOwner(
  world: AccessWorld,
  tenant: string,
  principal: string,
  next: Grant | null,
): boolean {
  const grants = grantsOf(world, tenant);
  const before = grants.get(principal);
  if (before?.role !== "owner" || next?.role === "owner") return true;
  if (!principal.startsWith("user:")) return true;
  return [...grants].some(
    ([p, g]) => p !== principal && p.startsWith("user:") && g.role === "owner",
  );
}

// --------------------------------------------------------------------------------------------
// Who calls

export type Caller =
  /** The embedded server's socket: no token, the operator of the tenant `local`. */
  | { kind: "socket"; tenant: string }
  | { kind: "key"; tenant: string; key: MockKey }
  | { kind: "user"; tenant: string | null; user: User; token: MockToken };

export function principalOf(caller: Caller): string | null {
  if (caller.kind === "key") return `key:${caller.key.id}`;
  if (caller.kind === "user") return `user:${caller.user.id}`;
  return null;
}

export function grantOf(world: AccessWorld, caller: Caller): Grant | null {
  if (caller.kind === "socket") return { role: "operator", allowedLabels: {} };
  if (!caller.tenant) return null;
  const principal = principalOf(caller);
  return principal ? (grantsOf(world, caller.tenant).get(principal) ?? null) : null;
}

export function isOperator(world: AccessWorld, caller: Caller): boolean {
  return grantOf(world, caller)?.role === "operator";
}

/** What the caller's role allows. */
export function permissionsOf(world: AccessWorld, caller: Caller): Permission[] {
  const grant = grantOf(world, caller);
  if (!grant || !caller.tenant) return [];
  return rolePermissions(world, caller.tenant, grant.role);
}

/** Why the caller may not use the permission, or `null` when it may. */
export function refusal(world: AccessWorld, caller: Caller, permission: Permission): string | null {
  const grant = grantOf(world, caller);
  if (!caller.tenant)
    return "Sign in to an organization first: create one, or accept an invitation.";
  if (!grant) return `You have no grant in ${caller.tenant}: no permission.`;
  if (!permissionsOf(world, caller).includes(permission))
    return `This operation needs the permission ${permission}, which the role ${grant.role} does not give.`;
  if (ADMIN_PERMISSIONS.has(permission) && Object.keys(grant.allowedLabels ?? {}).length > 0)
    return `A grant limited to labels cannot administer: ${permission} is refused.`;
  return null;
}

export function userOf(world: AccessWorld, id: string): MockUser | undefined {
  return world.users.find((u) => u.user.id === id);
}

export function userByEmail(world: AccessWorld, email: string): MockUser | undefined {
  const wanted = email.trim().toLowerCase();
  return world.users.find((u) => u.user.email.toLowerCase() === wanted);
}

export function organizationsOf(world: AccessWorld, userId: string): Organization[] {
  return world.memberships
    .filter((m) => m.userId === userId && m.status === "active")
    .map((m) => world.organizations.find((o) => o.id === m.organizationId))
    .filter((o): o is MockOrganization => o !== undefined)
    .map((o) => ({ id: o.id, name: o.name }));
}

export function isMember(world: AccessWorld, userId: string, organizationId: string): boolean {
  return world.memberships.some(
    (m) => m.userId === userId && m.organizationId === organizationId && m.status === "active",
  );
}

export function whoami(world: AccessWorld, caller: Caller): Identity {
  const grant = grantOf(world, caller);
  const identity: Identity = {
    operator: grant?.role === "operator",
    permissions: permissionsOf(world, caller),
    ...(grant ? { grant } : {}),
    ...(caller.tenant ? { tenant: caller.tenant } : {}),
  };
  if (caller.kind === "key") identity.principal = { kind: "key", id: caller.key.id };
  if (caller.kind === "user") {
    identity.principal = { kind: "user", user: caller.user };
    identity.organizations = organizationsOf(world, caller.user.id);
    const org = world.organizations.find((o) => o.id === caller.tenant);
    if (org) identity.organization = { id: org.id, name: org.name, defaultRole: org.defaultRole };
  }
  return identity;
}

// --------------------------------------------------------------------------------------------
// Sign-in

export function issueToken(world: AccessWorld, userId: string, organizationId: string | null) {
  const token: MockToken = {
    token: `mock_at_${random(HEX, 24)}`,
    userId,
    organizationId,
    expiresAt: Date.now() + world.tokenTtlMs,
  };
  world.tokens.set(token.token, token);
  return token;
}

export function sendCode(world: AccessWorld, email: string): void {
  const code = random("0123456789", 6);
  world.codes.set(email.toLowerCase(), code);
  world.log(`[mock] the code for ${email} is ${code}`);
}

/** Accepts an invitation for the user: an active membership, and the invitation's grant. */
export function acceptInvitation(world: AccessWorld, inv: MockInvitation, userId: string): void {
  inv.state = "accepted";
  const membership = world.memberships.find(
    (m) => m.userId === userId && m.organizationId === inv.organizationId,
  );
  if (membership) membership.status = "active";
  else world.memberships.push({ userId, organizationId: inv.organizationId, status: "active" });
  const grants = grantsOf(world, inv.organizationId);
  const pending = grants.get(`invitation:${inv.id}`);
  grants.delete(`invitation:${inv.id}`);
  // A member who already has a grant keeps it.
  if (pending && !grants.has(`user:${userId}`)) grants.set(`user:${userId}`, pending);
}

/** A member's first grant in an organization: the default role, when nothing gave one. */
function firstGrant(world: AccessWorld, organizationId: string, userId: string): void {
  const grants = grantsOf(world, organizationId);
  if (grants.has(`user:${userId}`)) return;
  const org = world.organizations.find((o) => o.id === organizationId);
  grants.set(`user:${userId}`, {
    role: org?.defaultRole ?? "reader",
    allowedLabels: {},
    grantedAt: iso(Date.now()),
  });
}

export function authenticated(
  world: AccessWorld,
  user: User,
  organizationId: string | null,
): AuthState {
  if (organizationId) firstGrant(world, organizationId, user.id);
  const token = issueToken(world, user.id, organizationId);
  world.session = { step: "authenticated", userId: user.id, organizationId, token: token.token };
  return {
    status: "authenticated",
    accessToken: token.token,
    expiresAt: iso(token.expiresAt),
    user,
    ...(organizationId ? { organizationId } : {}),
  };
}

/** Once the email is verified: the organization named, the only one, or a choice. */
export function afterVerified(
  world: AccessWorld,
  user: User,
  organizationId: string | null,
): AuthState {
  if (organizationId) return authenticated(world, user, organizationId);
  const organizations = organizationsOf(world, user.id);
  if (organizations.length <= 1) return authenticated(world, user, organizations[0]?.id ?? null);
  world.session = { step: "organization_selection_required", userId: user.id };
  return { status: "organization_selection_required", user, organizations };
}

/**
 * Where a sign-in of `email` ends, the identity proven (a code, GitHub). With an invitation's
 * token, the invitation is accepted, even for an email that differs from the invited one.
 * Answers a string when the invitation is unknown.
 */
export function signIn(
  world: AccessWorld,
  email: string,
  invitationToken: string | null,
): AuthState | string {
  let invitation: MockInvitation | undefined;
  if (invitationToken) {
    invitation = world.invitations.find(
      (i) => i.token === invitationToken && i.state === "pending",
    );
    if (!invitation) return "The invitation is unknown, expired, revoked or already accepted.";
  }
  let account = userByEmail(world, email);
  if (!account) {
    if (!invitation && world.workos?.waitlist) {
      world.session = { step: "sign_up_closed", email };
      return { status: "sign_up_closed", email };
    }
    account = {
      user: { id: newId("user"), email: email.trim().toLowerCase() },
      verified: true,
    };
    world.users.push(account);
  }
  const user = account.user;
  if (invitation) acceptInvitation(world, invitation, user.id);
  const organizationId = invitation?.organizationId ?? null;
  if (!account.verified) {
    world.session = { step: "email_verification_required", userId: user.id, organizationId };
    sendCode(world, user.email);
    return { status: "email_verification_required", email: user.email };
  }
  return afterVerified(world, user, organizationId);
}

/** The state the session cookie stands at. */
export function sessionState(world: AccessWorld): AuthState | null {
  const session = world.session;
  if (!session) return null;
  switch (session.step) {
    case "authenticated": {
      const user = userOf(world, session.userId)?.user;
      if (!user) return null;
      let token = world.tokens.get(session.token);
      if (!token || token.expiresAt - Date.now() < MINUTE) {
        token = issueToken(world, user.id, session.organizationId);
        session.token = token.token;
      }
      return {
        status: "authenticated",
        accessToken: token.token,
        expiresAt: iso(token.expiresAt),
        user,
        ...(session.organizationId ? { organizationId: session.organizationId } : {}),
      };
    }
    case "organization_selection_required": {
      const user = userOf(world, session.userId)?.user;
      if (!user) return null;
      return {
        status: "organization_selection_required",
        user,
        organizations: organizationsOf(world, user.id),
      };
    }
    case "email_verification_required": {
      const user = userOf(world, session.userId)?.user;
      return user ? { status: "email_verification_required", email: user.email } : null;
    }
    case "sign_up_closed":
      return { status: "sign_up_closed", ...(session.email ? { email: session.email } : {}) };
  }
}

// --------------------------------------------------------------------------------------------
// DTOs

export function apiKeyDto(world: AccessWorld, k: MockKey, withSecret = false): ApiKey {
  const grant = grantsOf(world, k.tenant).get(`key:${k.id}`);
  return {
    id: k.id,
    name: k.name,
    tenant: k.tenant,
    role: grant?.role ?? "",
    allowedLabels: grant?.allowedLabels ?? {},
    createdAt: iso(k.createdAt),
    ...(k.createdBy ? { createdBy: k.createdBy } : {}),
    ...(k.revokedAt !== null ? { revokedAt: iso(k.revokedAt) } : {}),
    ...(withSecret ? { key: k.secret } : {}),
  };
}

export function invitationDto(world: AccessWorld, inv: MockInvitation): Invitation {
  const org = world.organizations.find((o) => o.id === inv.organizationId);
  const grant = grantsOf(world, inv.organizationId).get(`invitation:${inv.id}`);
  const expired = inv.state === "pending" && inv.expiresAt < Date.now();
  return {
    id: inv.id,
    email: inv.email,
    state: expired ? "expired" : inv.state,
    createdAt: iso(inv.createdAt),
    expiresAt: iso(inv.expiresAt),
    ...(inv.inviter ? { inviter: inv.inviter } : {}),
    ...(org ? { organization: { id: org.id, name: org.name } } : {}),
    ...(grant ? { grant } : {}),
  };
}

export function memberDtos(world: AccessWorld, organizationId: string): Member[] {
  const grants = grantsOf(world, organizationId);
  return world.memberships
    .filter((m) => m.organizationId === organizationId)
    .flatMap((m): Member[] => {
      const user = userOf(world, m.userId)?.user;
      if (!user) return [];
      const grant = grants.get(`user:${m.userId}`);
      return [{ user, status: m.status, ...(grant ? { grant } : {}) }];
    });
}

export function catalog(): PermissionInfo[] {
  return PERMISSIONS.map((permission) => ({ permission, description: DESCRIPTIONS[permission] }));
}

export { HEX, iso, random };
