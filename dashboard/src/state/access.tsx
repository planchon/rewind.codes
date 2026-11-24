// What the caller may do, from `GET /v1/whoami`: the views follow it. The server enforces every
// rule; the dashboard only hides what would be refused.
import { createContext, useContext } from "react";
import { effectivePermissions, limitedToLabels } from "../model/access";
import type {
  Identity,
  Organization,
  OrganizationSettings,
  Permission,
  Person,
} from "../model/types";

export interface Section {
  path: string;
  title: string;
  /** What the caller needs to open it. */
  permission: Permission;
  /** Members, invitations and settings are a WorkOS organization's: a member's session. */
  needsOrganization: boolean;
}

/** The parts of the Organization view, in order. */
export const SECTIONS: readonly Section[] = [
  { path: "members", title: "Members", permission: "members:read", needsOrganization: true },
  {
    path: "invitations",
    title: "Invitations",
    permission: "members:read",
    needsOrganization: true,
  },
  { path: "roles", title: "Roles", permission: "roles:read", needsOrganization: false },
  { path: "api-keys", title: "API keys", permission: "api_keys:read", needsOrganization: false },
  // The keys Rewind signs its calls with, and the address of their public keys.
  {
    path: "signing-keys",
    title: "Signing keys",
    permission: "secrets:read",
    needsOrganization: false,
  },
  // People's connections, which open the organization: a WorkOS organization's members'.
  {
    path: "connections",
    title: "Connections",
    permission: "members:read",
    needsOrganization: true,
  },
  {
    path: "settings",
    title: "Settings",
    permission: "organization:write",
    needsOrganization: true,
  },
];

export interface Access {
  identity: Identity;
  /** What the caller may use: its role's permissions, less the administration ones with labels. */
  permissions: ReadonlySet<Permission>;
  can(permission: Permission): boolean;
  /** The person signed in as a member; `null` for a key or over the socket. */
  person: Person | null;
  /** The organization of a member's session, with its settings. */
  organization: OrganizationSettings | null;
  /** Every organization of the member. */
  organizations: Organization[];
  operator: boolean;
  /** The grant only reaches runs carrying its allowed labels. */
  limited: boolean;
  /** The role of the caller's grant; `null` without a grant. */
  role: string | null;
  /** The parts of the Organization view the caller may open. */
  sections: Section[];
  /**
   * The statistics (archi/05, "Statistics"): the Overview, Scripts, Tools, the failure groups and
   * the counts of the runs come from counters, which know no labels, and are refused to a grant
   * limited to labels.
   */
  statistics: boolean;
}

export function accessOf(identity: Identity): Access {
  const permissions = effectivePermissions(identity);
  const can = (p: Permission) => permissions.has(p);
  const organization = identity.organization ?? null;
  const limited = limitedToLabels(identity);
  return {
    identity,
    permissions,
    can,
    person: identity.principal?.kind === "member" ? identity.principal.person : null,
    organization,
    organizations: identity.organizations ?? [],
    operator: identity.operator,
    limited,
    role: identity.grant?.role ?? null,
    sections: SECTIONS.filter(
      (s) => can(s.permission) && (!s.needsOrganization || organization !== null),
    ),
    statistics: can("runs:read") && !limited,
  };
}

export const AccessContext = createContext<Access | null>(null);

export function useAccess(): Access {
  const access = useContext(AccessContext);
  if (!access) throw new Error("useAccess is used outside Gate");
  return access;
}
