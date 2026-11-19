// A person's connections (archi/06, "Connections"), the consent of an MCP client, and what
// `GET /v1/server` says of `/mcp`: their types, generated from api/openapi.json, their calls and
// data hooks.
import { useQuery } from "@tanstack/react-query";
import { useApi } from "../app/session";
import type { components } from "../generated/api";
import type { Api } from "./client";
import type { ServerInfo } from "./types";

type Schemas = components["schemas"];

/** What a connection may do in an organization: `read`, or `run` (start, signal, cancel too). */
export type ConnectionAccess = Schemas["Access"];

export const ACCESSES: readonly ConnectionAccess[] = ["read", "run"];

/** The person a connection acts for. */
export type ConnectionPerson = Schemas["ConnectionPerson"];
/** The MCP client: a metadata document's URL (CIMD), or `rc_…` when it registered itself. */
export type ConnectionClient = Schemas["ConnectionClient"];
/** An organization a connection opens, with its access and the labels of its grant there. */
export type ConnectionOrganization = Schemas["ConnectionOrganization"];
/** For `/v1/organization/connections`, `organizations` holds this organization only. */
export type Connection = Schemas["Connection"];
export type ConnectionList = Schemas["ConnectionList"];
/** `PATCH /v1/connections/{id}`: the organizations it opens, replaced whole. */
export type UpdateConnection = Schemas["UpdateConnection"];
/** An organization of the person, as the consent offers it. */
export type ConsentOrganization = Schemas["ConsentOrganization"];
/** `GET /v1/oauth/consent?request=…`: what an authorization request asks. */
export type ConsentRequest = Schemas["Consent"];
/** `POST /v1/oauth/consent`. */
export type ConsentDecision = Schemas["ConsentAnswer"];
/** Where to send the browser: the client's redirect URI with a code, or with an error. */
export type ConsentAnswer = Schemas["ConsentRedirect"];

/** What `GET /v1/server` says of `/mcp`. */
export interface McpInfo {
  /** `https://api.rewind.codes/mcp`; `null` when this server does not serve MCP. */
  mcpUrl: string | null;
  /** People connect through Rewind's OAuth (it has its public URL and WorkOS). */
  oauth: boolean;
}

/** What `GET /v1/server` says of `/mcp`. */
export function mcpOf(server: ServerInfo): McpInfo {
  return { mcpUrl: server.mcpUrl ?? null, oauth: server.oauth ?? false };
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
