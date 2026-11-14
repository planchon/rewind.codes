// Names for the DTOs of the API /v1, generated from api/openapi.json (pnpm generate:api).
import type { components, paths } from "../generated/api";

type Schemas = components["schemas"];

export type { paths };
export type Run = Schemas["Run"];
export type RunSummary = Schemas["RunSummary"];
export type RunPage = Schemas["RunPage"];
export type RunStatus = Schemas["RunStatus"];
export type RunMode = Schemas["RunMode"];
export type RunCounts = Schemas["RunCounts"];
export type RunError = Schemas["RunError"];
export type Waiting = Schemas["Waiting"];
export type Entry = Schemas["Entry"];
export type JournalEvent = Schemas["Event"];
export type EventOf<T extends JournalEvent["type"]> = Extract<JournalEvent, { type: T }>;
export type StreamEvent = Schemas["StreamEvent"];
export type Payload = Schemas["Payload"];
export type Callsite = Schemas["Callsite"];
export type Labels = Schemas["Labels"];
export type LogLevel = Schemas["LogLevel"];
export type ToolFailure = Schemas["ToolFailure"];
export type ToolFailureKind = Schemas["ToolFailureKind"];
export type ScriptError = Schemas["ScriptError"];
export type AbortReason = Schemas["AbortReason"];
export type CancelReason = Schemas["CancelReason"];
export type Script = Schemas["Script"];
export type ScriptSummary = Schemas["ScriptSummary"];
export type ScriptPage = Schemas["ScriptPage"];
export type ToolStats = Schemas["ToolStats"];
export type ToolStatsPage = Schemas["ToolStatsPage"];
export type ServerInfo = Schemas["ServerInfo"];
export type Problem = Schemas["Problem"];
export type Trace = Schemas["Trace"];
export type Quarantine = Schemas["Quarantine"];
export type RunWait = Schemas["RunWait"];
export type WaitKind = Schemas["WaitKind"];
export type RunStats = Schemas["RunStats"];
export type StatusCounts = Schemas["StatusCounts"];
export type WaitCounts = Schemas["WaitCounts"];
export type SeriesBucket = Schemas["SeriesBucket"];
export type DurationStats = Schemas["DurationStats"];
export type Facets = Schemas["Facets"];
export type FacetValue = Schemas["FacetValue"];
export type LabelFacet = Schemas["LabelFacet"];
export type FailureGroup = Schemas["FailureGroup"];
export type FailureGroupPage = Schemas["FailureGroupPage"];
export type QuerySpan = Schemas["QuerySpan"];
export type RunSort = NonNullable<ListRunsQuery["sort"]>;

// Sign-in (`/auth`), the organization and access.
export type AuthConfig = Schemas["AuthConfig"];
export type AuthState = Schemas["AuthState"];
export type Authenticated = Extract<AuthState, { status: "authenticated" }>;
export type SignInMethod = Schemas["SignInMethod"];
export type User = Schemas["User"];
export type Organization = Schemas["Organization"];
export type OrganizationSettings = Schemas["OrganizationSettings"];
export type Identity = Schemas["Identity"];
export type Principal = Schemas["Principal"];
export type Grant = Schemas["Grant"];
export type Permission = Schemas["Permission"];
export type PermissionInfo = Schemas["PermissionInfo"];
export type Role = Schemas["Role"];
export type PutRole = Schemas["PutRole"];
export type Member = Schemas["Member"];
export type MembershipStatus = Schemas["MembershipStatus"];
export type Invitation = Schemas["Invitation"];
export type InvitationState = Schemas["InvitationState"];
export type CreateInvitation = Schemas["CreateInvitation"];
export type SetGrant = Schemas["SetGrant"];
export type ApiKey = Schemas["ApiKey"];
export type CreateApiKey = Schemas["CreateApiKey"];
export type UpdateOrganization = Schemas["UpdateOrganization"];

export type ListRunsQuery = NonNullable<paths["/v1/runs"]["get"]["parameters"]["query"]>;
export type ListScriptsQuery = NonNullable<paths["/v1/scripts"]["get"]["parameters"]["query"]>;
export type ToolStatsQuery = NonNullable<paths["/v1/tools/stats"]["get"]["parameters"]["query"]>;
export type RunStatsQuery = NonNullable<paths["/v1/runs/stats"]["get"]["parameters"]["query"]>;
export type FailuresQuery = NonNullable<paths["/v1/runs/failures"]["get"]["parameters"]["query"]>;

export const RUN_STATUSES: readonly RunStatus[] = [
  "pending",
  "active",
  "completed",
  "failed",
  "cancelled",
  "aborted",
];

export const WAIT_KINDS: readonly WaitKind[] = [
  "signal",
  "timer",
  "call",
  "host",
  "approval",
  "parked",
  "deferred",
];

export const TOOL_FAILURE_KINDS: readonly ToolFailureKind[] = [
  "error",
  "unavailable",
  "timeout",
  "outcome_unknown",
];

/** Whether a status is final: nothing will be appended to the journal. */
export function isEnded(status: RunStatus): boolean {
  return status !== "pending" && status !== "active";
}
