// Pure code of the Claude pages: what an access gives, the harness actions of an action set, the first
// message to send Claude, whether a connection arrived while Connect Claude was open, and the
// choices the consent starts from.
import { shortDigest } from "./format";
import type {
  ActionSetSummary,
  Connection,
  ConnectionAccess,
  ConsentOrganization,
  ConsentRequest,
  ServerInfo,
} from "./types";

/** The accesses a connection may have in an organization, the least first. */
export const ACCESSES: readonly ConnectionAccess[] = ["read", "run"];

/** What `GET /v1/server` says of `/mcp`. */
export interface McpInfo {
  /** `https://api.rewind.codes/mcp`; `null` when this server does not serve MCP. */
  mcpUrl: string | null;
  /** People connect through Rewind's OAuth (it has its public URL and WorkOS). */
  oauth: boolean;
}

export function mcpOf(server: ServerInfo): McpInfo {
  return { mcpUrl: server.mcpUrl ?? null, oauth: server.oauth ?? false };
}

/** What each access lets the connection do (archi/06, "Connections"). */
export const ACCESS_INFO: Record<ConnectionAccess, { title: string; gives: string }> = {
  read: { title: "Read", gives: "runs, namespaces, action sets and workflows" },
  run: {
    title: "Run",
    gives: "also check, test and run workflows, signal and cancel runs",
  },
};

/** The built-ins an action set holds without naming a server: they are not harness actions. */
const BUILT_INS = new Set(["http", "signalUrl", "webhook", "workflows"]);

/**
 * The harness actions of an action set: its actions that belong to none of its MCP servers or
 * APIs nor to the built-ins. An action set's summary names its actions (`github.list_issues`,
 * `ask_user`), its servers and its APIs, not their routes.
 */
export function harnessActions(actionSet: ActionSetSummary): string[] {
  const groups = new Set([
    ...actionSet.servers.map((s) => s.name),
    ...(actionSet.apis ?? []).map((a) => a.name),
  ]);
  return actionSet.actions.filter((action) => {
    const group = action.includes(".") ? action.slice(0, action.indexOf(".")) : action;
    return !groups.has(group) && !BUILT_INS.has(group);
  });
}

/** How an action set is told apart in a list: its groups, or its id. */
export function actionSetTitle(actionSet: ActionSetSummary): string {
  const groups = new Set([
    ...actionSet.servers.map((s) => s.name),
    ...(actionSet.apis ?? []).map((a) => a.name),
  ]);
  for (const action of harnessActions(actionSet)) groups.add(action.split(".")[0] ?? action);
  if (actionSet.actions.some((t) => t === "http" || t.startsWith("http."))) groups.add("http");
  const names = [...groups].sort();
  return names.length > 0 ? names.join(", ") : shortDigest(actionSet.id);
}

export interface FirstMessage {
  organization: string;
  /** The action set's id, as `list_action_sets` gives it; `null` before one exists. */
  actionSet: string | null;
  namespace: string;
}

/**
 * The first message to send Claude: it names the organization, the action set and the namespace,
 * so that Claude asks nothing before its first call.
 */
export function firstMessage({ organization, actionSet, namespace }: FirstMessage): string {
  const parts = [`organization ${organization}`];
  if (actionSet) parts.push(`action set ${actionSet}`);
  parts.push(`namespace ${namespace.trim() || "demo"}`);
  return `With Rewind, ${parts.join(", ")}: show me what this action set can do, then write a short workflow that uses it. Check it and test it, and run it once I agree.`;
}

/** A namespace's name (archi/06): 1 to 128 letters, digits, `-`, `_`, `.`, `:` and `@`. */
export function namespaceProblem(name: string): string | null {
  const trimmed = name.trim();
  if (trimmed === "") return null;
  if (!/^[A-Za-z0-9._:@-]{1,128}$/.test(trimmed))
    return "Letters, digits, '-', '_', '.', ':' and '@' only, up to 128.";
  return null;
}

/** What each connection was when Connect Claude opened: its id and its last use. */
export type Snapshot = ReadonlyMap<string, string | null>;

export function snapshotOf(connections: readonly Connection[]): Snapshot {
  return new Map(connections.map((c) => [c.id, c.lastUsedAt ?? null]));
}

/**
 * The connection that arrived since the snapshot: a new one, or one used since. The most recent
 * when several did; `null` when none. Compared with the server's own times only, so the
 * browser's clock does not matter.
 */
export function arrivedSince(
  snapshot: Snapshot,
  connections: readonly Connection[],
): Connection | null {
  const moved = connections.filter((c) => {
    if (!snapshot.has(c.id)) return true;
    const before = snapshot.get(c.id) ?? null;
    const used = c.lastUsedAt ?? null;
    return used !== null && used !== before;
  });
  if (moved.length === 0) return null;
  const at = (c: Connection) => Date.parse(c.lastUsedAt ?? c.createdAt);
  return moved.reduce((a, b) => (at(b) > at(a) ? b : a));
}

/** What the consent starts from, per organization: ticked or not, and its access. */
export interface ConsentChoice {
  checked: boolean;
  access: ConnectionAccess;
}

/**
 * The organization open in the dashboard is ticked in advance, with the widest access the person
 * may give there: Claude is connected to run workflows, and the person sees what is given. An
 * organization the person may give nothing in cannot be ticked.
 */
export function initialChoices(request: ConsentRequest): Map<string, ConsentChoice> {
  const choices = new Map<string, ConsentChoice>();
  for (const org of request.organizations) {
    choices.set(org.id, { checked: org.current && org.accesses.length > 0, access: widest(org) });
  }
  return choices;
}

function widest(org: ConsentOrganization): ConnectionAccess {
  return org.accesses.includes("run") ? "run" : "read";
}

/** `via=claude`, `env=dev`: labels as the consent and the tables write them. */
export function labelsText(labels: Record<string, string>): string {
  return Object.entries(labels)
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([k, v]) => `${k}=${v}`)
    .join(", ");
}

/** The host of a URL, or the text as it is. */
export function hostOf(uri: string | null): string | null {
  if (!uri) return null;
  try {
    return new URL(uri).host || uri;
  } catch {
    return uri;
  }
}
