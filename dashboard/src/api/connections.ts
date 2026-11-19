// A person's connections (archi/06, "Connections"), the consent of an MCP client, and what
// `GET /v1/server` says of `/mcp`: their types, calls and data hooks.
//
// HAND-WRITTEN TYPES. `api/openapi.json` does not describe `/v1/connections`,
// `/v1/organization/connections`, `/v1/oauth/consent`, nor the fields `mcpUrl` and `oauth` of
// `GET /v1/server` yet. Once it does, run `pnpm generate:api`, replace every type below with its
// generated schema (`components["schemas"][…]` through `./types`), and move the calls onto the
// typed client of `./client` (`api.client.GET("/v1/connections")`…). Until then the calls go
// through `Api.raw`, which sends the same Bearer token, renews it once on a 401, and reads a
// problem from any answer that is not a success.
import { useQuery } from "@tanstack/react-query";
import { useApi } from "../app/session";
import type { Api } from "./client";
import type { ServerInfo } from "./types";

/** What a connection may do in an organization: `read`, or `run` (start, signal, cancel too). */
export type ConnectionAccess = "read" | "run";

export const ACCESSES: readonly ConnectionAccess[] = ["read", "run"];

/** The person a connection acts for. */
export interface ConnectionPerson {
  id: string;
  email: string | null;
  name: string | null;
}

/** The MCP client: a metadata document's URL (CIMD), or `rc_…` when it registered itself. */
export interface ConnectionClient {
  id: string;
  name: string;
  uri: string | null;
  kind: "metadata_document" | "registered";
}

/** An organization a connection opens, with its access and the labels of its grant there. */
export interface ConnectionOrganization {
  id: string;
  name: string | null;
  access: ConnectionAccess;
  allowedLabels: Record<string, string>;
}

export interface Connection {
  /** `cn_…` */
  id: string;
  person: ConnectionPerson;
  client: ConnectionClient;
  /** For `/v1/organization/connections`, this organization only. */
  organizations: ConnectionOrganization[];
  createdAt: string;
  lastUsedAt: string | null;
}

export interface ConnectionList {
  connections: Connection[];
}

/** `PATCH /v1/connections/{id}`: the organizations it opens, replaced whole. */
export interface UpdateConnection {
  organizations: { id: string; access: ConnectionAccess; allowedLabels: Record<string, string> }[];
}

/** An organization of the person, as the consent offers it. */
export interface ConsentOrganization {
  id: string;
  name: string | null;
  /** The person's role there. */
  role: string | null;
  /** The accesses the person may give there; none means it cannot be opened. */
  accesses: ConnectionAccess[];
  /** The organization open in the dashboard: ticked in advance. */
  current: boolean;
}

/** `GET /v1/oauth/consent?request=…`: what an authorization request asks. */
export interface ConsentRequest {
  client: ConnectionClient & { logoUri: string | null };
  /** The host the browser returns to. */
  redirectHost: string;
  /** It returns to a program on this computer (`http://127.0.0.1:…`, Claude Code's). */
  loopback: boolean;
  organizations: ConsentOrganization[];
  /** The labels of what the client starts (`via: claude`), which the person may untick. */
  allowedLabels: Record<string, string>;
  expiresAt: string;
}

/** `POST /v1/oauth/consent`. */
export interface ConsentDecision {
  request: string;
  decision: "allow" | "deny";
  organizations: { id: string; access: ConnectionAccess }[];
  allowedLabels: Record<string, string>;
}

export interface ConsentAnswer {
  /** Where to send the browser: the client's redirect URI with a code, or with an error. */
  redirectTo: string;
}

/** What `GET /v1/server` says of `/mcp`. */
export interface McpInfo {
  /** `https://api.rewind.codes/mcp`; `null` when this server does not serve MCP. */
  mcpUrl: string | null;
  /** People connect through Rewind's OAuth (it has its public URL and WorkOS). */
  oauth: boolean;
}

/** The fields of `GET /v1/server` the generated `ServerInfo` does not have yet. */
export function mcpOf(server: ServerInfo): McpInfo {
  const more = server as ServerInfo & Partial<McpInfo>;
  return { mcpUrl: more.mcpUrl ?? null, oauth: more.oauth ?? false };
}

// Calls

async function json<T>(response: Response): Promise<T> {
  return (await response.json()) as T;
}

function jsonInit(method: string, body: unknown): RequestInit {
  return {
    method,
    headers: { "Content-Type": "application/json", Accept: "application/json" },
    body: JSON.stringify(body),
  };
}

/** The caller's own connections, in every organization. */
export async function listConnections(api: Api, signal?: AbortSignal): Promise<Connection[]> {
  const response = await api.raw("/v1/connections", signal ? { signal } : {});
  return (await json<ConnectionList>(response)).connections;
}

export async function updateConnection(
  api: Api,
  id: string,
  body: UpdateConnection,
): Promise<Connection> {
  const response = await api.raw(
    `/v1/connections/${encodeURIComponent(id)}`,
    jsonInit("PATCH", body),
  );
  return json<Connection>(response);
}

export async function revokeConnection(api: Api, id: string): Promise<void> {
  await api.raw(`/v1/connections/${encodeURIComponent(id)}`, { method: "DELETE" });
}

/** The connections that open the caller's organization. */
export async function listOrganizationConnections(
  api: Api,
  signal?: AbortSignal,
): Promise<Connection[]> {
  const response = await api.raw("/v1/organization/connections", signal ? { signal } : {});
  return (await json<ConnectionList>(response)).connections;
}

/** Removes the caller's organization from a connection. */
export async function removeOrganizationConnection(api: Api, id: string): Promise<void> {
  await api.raw(`/v1/organization/connections/${encodeURIComponent(id)}`, { method: "DELETE" });
}

export async function readConsent(
  api: Api,
  request: string,
  signal?: AbortSignal,
): Promise<ConsentRequest> {
  const response = await api.raw(
    `/v1/oauth/consent?request=${encodeURIComponent(request)}`,
    signal ? { signal } : {},
  );
  return json<ConsentRequest>(response);
}

export async function answerConsent(api: Api, body: ConsentDecision): Promise<ConsentAnswer> {
  const response = await api.raw("/v1/oauth/consent", jsonInit("POST", body));
  return json<ConsentAnswer>(response);
}

// Data hooks

export const CONNECTIONS = ["connections"] as const;
export const ORG_CONNECTIONS = ["org-connections"] as const;

/** How often Connect Claude asks whether Claude has connected, while it is open. */
export const CONNECTION_POLL_MS = 2_000;

/** The caller's connections; polled while `pollMs` is given. */
export function useConnections(enabled = true, pollMs: number | false = false) {
  const api = useApi();
  return useQuery({
    queryKey: CONNECTIONS,
    queryFn: ({ signal }) => listConnections(api, signal),
    refetchInterval: pollMs,
    enabled,
  });
}

export function useOrganizationConnections(enabled = true) {
  const api = useApi();
  return useQuery({
    queryKey: ORG_CONNECTIONS,
    queryFn: ({ signal }) => listOrganizationConnections(api, signal),
    enabled,
  });
}

/** The toolsets of the organization (`toolsets:read`). */
export function useToolsets(enabled = true) {
  const api = useApi();
  return useQuery({
    queryKey: ["toolsets"],
    queryFn: ({ signal }) => api.toolsets(signal),
    enabled,
  });
}
