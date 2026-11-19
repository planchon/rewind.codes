// Pure code of the Claude pages: what an access gives, the host tools of a toolset, the first
// message to send Claude, whether a connection arrived while Connect Claude was open, and the
// choices the consent starts from.
import type {
  Connection,
  ConnectionAccess,
  ConsentOrganization,
  ConsentRequest,
} from "../api/connections";
import type { ToolsetSummary } from "../api/types";
import { shortDigest } from "./format";

/** What each access lets the connection do (archi/06, "Connections"). */
export const ACCESS_INFO: Record<ConnectionAccess, { title: string; gives: string }> = {
  read: { title: "Read", gives: "runs, namespaces, toolsets and workflows" },
  run: {
    title: "Run",
    gives: "also check, test and start scripts, signal and cancel runs",
  },
};

/** The built-in tools a toolset holds without naming a server: they are not host tools. */
const BUILT_INS = new Set(["http", "webhook", "workflows"]);

/**
 * The host tools of a toolset: its tools that belong to none of its MCP servers nor to the
 * built-ins. A toolset's summary names its tools (`github.list_issues`, `ask_user`) and its
 * servers, not their routes.
 */
export function hostTools(toolset: ToolsetSummary): string[] {
  const servers = new Set(toolset.servers.map((s) => s.name));
  return toolset.tools.filter((tool) => {
    const group = tool.includes(".") ? tool.slice(0, tool.indexOf(".")) : tool;
    return !servers.has(group) && !BUILT_INS.has(group);
  });
}

/** How a toolset is told apart in a list: its servers and host-tool groups, or its id. */
export function toolsetTitle(toolset: ToolsetSummary): string {
  const groups = new Set(toolset.servers.map((s) => s.name));
  for (const tool of hostTools(toolset)) groups.add(tool.split(".")[0] ?? tool);
  if (toolset.tools.some((t) => t === "http" || t.startsWith("http."))) groups.add("http");
  const names = [...groups].sort();
  return names.length > 0 ? names.join(", ") : shortDigest(toolset.id);
}

export interface FirstMessage {
  organization: string;
  /** The toolset's id, as `list_toolsets` gives it; `null` before one exists. */
  toolset: string | null;
  namespace: string;
}

/**
 * The first message to send Claude: it names the organization, the toolset and the namespace, so
 * that Claude asks nothing before its first call.
 */
export function firstMessage({ organization, toolset, namespace }: FirstMessage): string {
  const parts = [`org ${organization}`];
  if (toolset) parts.push(`toolset ${toolset}`);
  parts.push(`namespace ${namespace.trim() || "demo"}`);
  return `With Rewind, ${parts.join(", ")}: show me what this toolset can do, then write a short script that uses it. Check it and test it, and run it once I agree.`;
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
 * may give there: Claude is connected to run scripts, and the person sees what is given. An
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
