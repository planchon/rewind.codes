// The mocked connections of people (archi/06, "Connections"), the authorization requests waiting
// for their consent, and the tenant's toolsets. A connection is a principal with a grant in each
// organization it opens (`connection:cn_…`, the built-in role `reader` for `read`, `harness` for
// `run`), kept in the access world's grants as the server keeps them. The mock creates the
// connection at the consent, as if Claude exchanged its code at once.
import { ADMIN_PERMISSIONS } from "../model/access";
import type {
  Connection,
  ConnectionAccess,
  ConnectionClient,
  ConsentOrganization,
  Permission,
  ToolsetSummary,
} from "../model/types";
import {
  type AccessWorld,
  grantsOf,
  HEX,
  iso,
  newId,
  organizationsOf,
  random,
  rolePermissions,
  userOf,
} from "./access";
import { fakeDigest } from "./journal";

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;

/** The permissions each access needs, and gives (archi/06, "Connections"). */
export const ACCESS_PERMISSIONS: Record<ConnectionAccess, Permission[]> = {
  read: ["runs:read", "toolsets:read", "namespaces:read"],
  run: ["runs:read", "toolsets:read", "namespaces:read", "runs:start", "runs:control"],
};

const ROLE_OF: Record<ConnectionAccess, string> = { read: "reader", run: "harness" };

export interface MockConnection {
  id: string;
  userId: string;
  client: ConnectionClient;
  createdAt: number;
  lastUsedAt: number | null;
  revokedAt: number | null;
}

/** A request `/oauth/authorize` sealed and sent to the consent page. */
export interface MockConsentRequest {
  id: string;
  client: ConnectionClient & { logoUri: string | null };
  redirectUri: string;
  state: string;
  allowedLabels: Record<string, string>;
  expiresAt: number;
}

export interface ConnectionWorld {
  connections: MockConnection[];
  requests: Map<string, MockConsentRequest>;
  /** The tenant's toolsets; other tenants have none. */
  toolsets: ToolsetSummary[];
  /** What `GET /v1/server` says of `/mcp`. */
  mcp: { mcpUrl: string | null; oauth: boolean };
  /**
   * `pnpm dev:mock`: a connection made at the consent is used by "Claude" a few seconds later,
   * as Claude lists the tools right after connecting.
   */
  simulateUse: boolean;
}

export const CLAUDE_AI: ConnectionClient & { logoUri: string | null } = {
  id: "https://claude.ai/oauth/mcp-oauth-client-metadata",
  name: "Claude",
  uri: "https://claude.ai",
  logoUri: null,
  kind: "metadata_document",
};

export const CLAUDE_CODE: ConnectionClient & { logoUri: string | null } = {
  id: "https://claude.ai/oauth/claude-code-client-metadata",
  name: "Claude Code",
  uri: "https://claude.com/product/claude-code",
  logoUri: null,
  kind: "metadata_document",
};

/** The redirect URI of claude.ai, Desktop and mobile. */
export const CLAUDE_CALLBACK = "https://claude.ai/api/mcp/auth_callback";

function toolsets(): ToolsetSummary[] {
  const http = (name: string, tools: number) => ({ name, tools, transport: "http" as const });
  return [
    {
      id: fakeDigest("toolset:github,slack"),
      servers: [http("github", 2), http("slack", 1)],
      tools: ["github.close_issue", "github.list_issues", "slack.post_message"],
    },
    {
      id: fakeDigest("toolset:github,llm,slack"),
      servers: [http("github", 2), http("llm", 1), http("slack", 1)],
      tools: ["github.list_issues", "github.list_pulls", "llm.complete", "slack.post_message"],
    },
    {
      id: fakeDigest("toolset:ask_user,crm"),
      servers: [http("crm", 2)],
      tools: ["ask_user", "crm.find_account", "crm.provision"],
    },
    {
      id: fakeDigest("toolset:weather"),
      servers: [http("weather", 1)],
      tools: ["weather.forecast"],
    },
    {
      id: fakeDigest("toolset:github,slack+workflows:digest,triage"),
      servers: [http("github", 2), http("slack", 1)],
      tools: ["github.close_issue", "github.list_issues", "slack.post_message"],
      workflows: ["digest", "triage"],
    },
  ];
}

export interface ConnectionOptions {
  mcp?: { mcpUrl: string | null; oauth: boolean };
  /** `pnpm dev:mock`: a request waiting for the consent, and connections used once made. */
  live?: boolean;
}

export function createConnectionWorld(
  access: AccessWorld,
  options: ConnectionOptions = {},
): ConnectionWorld {
  const now = Date.now();
  const world: ConnectionWorld = {
    connections: [],
    requests: new Map(),
    toolsets: toolsets(),
    mcp:
      options.mcp ??
      (access.workos
        ? { mcpUrl: "https://api.rewind.codes/mcp", oauth: true }
        : { mcpUrl: null, oauth: false }),
    simulateUse: options.live ?? false,
  };
  if (!access.workos) return world;

  // Ada connected claude.ai to Acme and Globex, and Claude Code to Acme; Grace claude.ai to Acme.
  connect(access, world, {
    userId: "user_01ADA",
    client: CLAUDE_AI,
    organizations: [
      { id: "org_01ACME", access: "run", allowedLabels: { via: "claude" } },
      { id: "org_01GLOBEX", access: "read", allowedLabels: { via: "claude" } },
    ],
    createdAt: now - 3 * DAY,
    lastUsedAt: now - 2 * HOUR,
  });
  connect(access, world, {
    userId: "user_01ADA",
    client: CLAUDE_CODE,
    organizations: [{ id: "org_01ACME", access: "run", allowedLabels: {} }],
    createdAt: now - 10 * DAY,
    lastUsedAt: now - DAY,
  });
  connect(access, world, {
    userId: "user_01GRACE",
    client: CLAUDE_AI,
    organizations: [{ id: "org_01ACME", access: "run", allowedLabels: { via: "claude" } }],
    createdAt: now - 6 * DAY,
    lastUsedAt: now - 5 * HOUR,
  });
  if (options.live) consentRequest(world, { id: "claude-demo", expiresInMs: 30 * DAY });
  return world;
}

/** Creates a connection, its grants with it. */
export function connect(
  access: AccessWorld,
  world: ConnectionWorld,
  c: {
    userId: string;
    client: ConnectionClient;
    organizations: {
      id: string;
      access: ConnectionAccess;
      allowedLabels: Record<string, string>;
    }[];
    createdAt?: number;
    lastUsedAt?: number | null;
  },
): MockConnection {
  const connection: MockConnection = {
    id: newId("cn"),
    userId: c.userId,
    client: { id: c.client.id, name: c.client.name, uri: c.client.uri, kind: c.client.kind },
    createdAt: c.createdAt ?? Date.now(),
    lastUsedAt: c.lastUsedAt ?? null,
    revokedAt: null,
  };
  world.connections.push(connection);
  setOrganizations(access, connection, c.organizations);
  return connection;
}

/** A request waiting for the consent, as `/oauth/authorize` would send it. */
export function consentRequest(
  world: ConnectionWorld,
  options: {
    /** A fixed id: `pnpm dev:mock`'s request is `claude-demo`, for links and screenshots. */
    id?: string;
    client?: ConnectionClient & { logoUri: string | null };
    redirectUri?: string;
    expiresInMs?: number;
    allowedLabels?: Record<string, string>;
  } = {},
): MockConsentRequest {
  const request: MockConsentRequest = {
    id: options.id ?? random(HEX, 32),
    client: options.client ?? CLAUDE_AI,
    redirectUri: options.redirectUri ?? CLAUDE_CALLBACK,
    state: random(HEX, 16),
    allowedLabels: options.allowedLabels ?? { via: "claude" },
    expiresAt: Date.now() + (options.expiresInMs ?? 10 * MINUTE),
  };
  world.requests.set(request.id, request);
  return request;
}

export function principalOfConnection(id: string): string {
  return `connection:${id}`;
}

/** Replaces the organizations a connection opens: its grants. */
export function setOrganizations(
  access: AccessWorld,
  connection: MockConnection,
  organizations: { id: string; access: ConnectionAccess; allowedLabels: Record<string, string> }[],
): void {
  const principal = principalOfConnection(connection.id);
  for (const grants of access.grants.values()) grants.delete(principal);
  for (const o of organizations) {
    grantsOf(access, o.id).set(principal, {
      role: ROLE_OF[o.access],
      allowedLabels: o.allowedLabels,
      grantedAt: iso(Date.now()),
      grantedBy: `user:${connection.userId}`,
    });
  }
}

/** The organizations a connection opens, from its grants. */
export function organizationsOfConnection(
  access: AccessWorld,
  connection: MockConnection,
): Connection["organizations"] {
  const principal = principalOfConnection(connection.id);
  const out: Connection["organizations"] = [];
  for (const [tenant, grants] of access.grants) {
    const grant = grants.get(principal);
    if (!grant) continue;
    const org = access.organizations.find((o) => o.id === tenant);
    out.push({
      id: tenant,
      name: org?.name ?? null,
      access: grant.role === "harness" ? "run" : "read",
      allowedLabels: grant.allowedLabels ?? {},
    });
  }
  return out.sort((a, b) => (a.name ?? a.id).localeCompare(b.name ?? b.id));
}

export function connectionDto(
  access: AccessWorld,
  connection: MockConnection,
  onlyTenant: string | null = null,
): Connection {
  const user = userOf(access, connection.userId)?.user;
  const name = user ? [user.firstName, user.lastName].filter(Boolean).join(" ") : "";
  return {
    id: connection.id,
    person: { id: connection.userId, email: user?.email ?? null, name: name || null },
    client: connection.client,
    organizations: organizationsOfConnection(access, connection).filter(
      (o) => onlyTenant === null || o.id === onlyTenant,
    ),
    createdAt: iso(connection.createdAt),
    lastUsedAt: connection.lastUsedAt === null ? null : iso(connection.lastUsedAt),
  };
}

/** The accesses a person may give in an organization: those whose permissions they hold. */
export function accessesOf(
  access: AccessWorld,
  userId: string,
  organizationId: string,
): ConnectionAccess[] {
  const grant = grantsOf(access, organizationId).get(`user:${userId}`);
  if (!grant) return [];
  const limited = Object.keys(grant.allowedLabels ?? {}).length > 0;
  const held = new Set(
    rolePermissions(access, organizationId, grant.role).filter(
      (p) => !(limited && ADMIN_PERMISSIONS.has(p)),
    ),
  );
  return (["read", "run"] as const).filter((a) => ACCESS_PERMISSIONS[a].every((p) => held.has(p)));
}

/** The person's organizations, as the consent offers them. */
export function consentOrganizations(
  access: AccessWorld,
  userId: string,
  current: string | null,
): ConsentOrganization[] {
  return organizationsOf(access, userId).map((o) => ({
    id: o.id,
    name: o.name,
    role: grantsOf(access, o.id).get(`user:${userId}`)?.role ?? null,
    accesses: accessesOf(access, userId, o.id),
    current: o.id === current,
  }));
}
