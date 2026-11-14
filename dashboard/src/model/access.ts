// What the caller may do, as `GET /v1/whoami` tells it, and the rules the organization views
// follow before the server enforces them (archi/06, "What the caller may do").
import type { Identity, Labels, Permission, Role, User } from "../api/types";
import { labelPair, splitLabel } from "./format";

/** The catalog, in its order (`GET /v1/permissions` answers the same, with descriptions). */
export const PERMISSIONS: readonly Permission[] = [
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
  "members:invite",
  "members:manage",
  "roles:read",
  "roles:write",
  "api_keys:read",
  "api_keys:write",
  "organization:write",
];

/** The permissions a grant limited to labels cannot use: it cannot administer. */
export const ADMIN_PERMISSIONS: ReadonlySet<Permission> = new Set<Permission>([
  "members:invite",
  "members:manage",
  "roles:write",
  "api_keys:read",
  "api_keys:write",
  "organization:write",
]);

export const BUILT_IN_ROLES: readonly string[] = ["owner", "harness", "worker", "reader"];
export const OPERATOR_ROLE = "operator";

/** Whether the grant only reaches runs carrying some labels. */
export function limitedToLabels(identity: Identity): boolean {
  return Object.keys(identity.grant?.allowedLabels ?? {}).length > 0;
}

/** The permissions the caller may use: its role's, without the administration ones when its grant is limited to labels. */
export function effectivePermissions(identity: Identity): ReadonlySet<Permission> {
  const limited = limitedToLabels(identity);
  return new Set(identity.permissions.filter((p) => !(limited && ADMIN_PERMISSIONS.has(p))));
}

/** The permissions of `wanted` that `held` lacks: no one gives more than they hold. */
export function missingPermissions(
  wanted: readonly Permission[],
  held: ReadonlySet<Permission>,
): Permission[] {
  return wanted.filter((p) => !held.has(p));
}

/** Whether the caller may give the role: it holds every permission of it. */
export function canGive(role: Role, held: ReadonlySet<Permission>): boolean {
  return missingPermissions(role.permissions, held).length === 0;
}

/** The permissions of a role by its name; `undefined` for a role that no longer exists. */
export function permissionsOf(name: string, roles: readonly Role[]): Permission[] | undefined {
  if (name === OPERATOR_ROLE) return [...PERMISSIONS];
  return roles.find((r) => r.name === name)?.permissions;
}

/** Why a name cannot be a role of the organization, or `null` when it can. */
export function roleNameProblem(name: string): string | null {
  if (name.length === 0) return "Give the role a name.";
  if (name.length > 64) return "A role's name has at most 64 characters.";
  if (!/^[a-z0-9_-]+$/.test(name)) return "Use lowercase letters, digits, - and _ only.";
  if (BUILT_IN_ROLES.includes(name) || name === OPERATOR_ROLE)
    return `${name} is a built-in role: choose another name.`;
  return null;
}

/** The permissions grouped by what they act on (`runs`, `members`…), in the catalog's order. */
export function groupPermissions<T extends { permission: Permission }>(
  items: readonly T[],
): { resource: string; items: T[] }[] {
  const groups = new Map<string, T[]>();
  for (const item of items) {
    const resource = item.permission.slice(0, item.permission.indexOf(":"));
    let group = groups.get(resource);
    if (!group) {
      group = [];
      groups.set(resource, group);
    }
    group.push(item);
  }
  return [...groups].map(([resource, list]) => ({ resource, items: list }));
}

/** Allowed labels as they are typed: `agent:triage conversation:c-42`. */
export function formatLabels(labels: Labels | undefined): string {
  return Object.entries(labels ?? {})
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([k, v]) => labelPair(k, v))
    .join(" ");
}

/** Reads typed labels, `key:value` separated by spaces or commas. */
export function parseLabels(text: string): { labels: Labels } | { error: string } {
  const labels: Labels = {};
  for (const pair of text.split(/[\s,]+/).filter(Boolean)) {
    const split = splitLabel(pair);
    if (!split || split[1] === "") return { error: `${pair} is not a label: write key:value.` };
    const [key, value] = split;
    if (key in labels && labels[key] !== value)
      return { error: `The label ${key} is given twice, with two values.` };
    labels[key] = value;
  }
  return { labels };
}

/** A person's name, or their email when the identity provider has none. */
export function displayName(user: User): string {
  const name = [user.firstName, user.lastName].filter(Boolean).join(" ").trim();
  return name || user.email;
}

/** One or two letters for an avatar. */
export function initials(user: User): string {
  const first = user.firstName?.trim()[0];
  const last = user.lastName?.trim()[0];
  if (first) return `${first}${last ?? ""}`.toUpperCase();
  return (user.email[0] ?? "?").toUpperCase();
}

/** Splits a principal's text (`user:<id>`, `key:<id>`, `invitation:<id>`). */
export function splitPrincipal(text: string): { kind: string; id: string } {
  const at = text.indexOf(":");
  return at < 0 ? { kind: "", id: text } : { kind: text.slice(0, at), id: text.slice(at + 1) };
}
