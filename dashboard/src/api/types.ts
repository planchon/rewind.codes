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
export type ToolsetSummary = Schemas["ToolsetSummary"];
export type QuotaReached = Schemas["QuotaReached"];
/**
 * An answer that is not a success. The document flattens an optional `QuotaReached` into it
 * (`quota-exceeded` adds `quota`, `scope`, `limit`, `used`, `resetsAt`), which the generator turns
 * into required fields: they are optional here, as on the wire.
 */
export type Problem = Omit<Schemas["Problem"], keyof QuotaReached> & Partial<QuotaReached>;
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

// Namespaces: the end users of the organization, their quotas, use and alerts.
export type Namespace = Schemas["Namespace"];
export type NamespaceList = Schemas["NamespaceList"];
export type NamespaceDetail = Schemas["NamespaceDetail"];
export type NamespaceDefaults = Schemas["NamespaceDefaults"];
export type Quotas = Schemas["Quotas"];
export type QuotaUsage = Schemas["QuotaUsage"];
export type QuotaKind = Schemas["QuotaKind"];
export type QuotaAlert = Schemas["QuotaAlert"];
export type QuotaAlertList = Schemas["QuotaAlertList"];
export type DayUsage = Schemas["DayUsage"];
export type Duration = Schemas["Duration"];
export type PutNamespace = Schemas["PutNamespace"];
export type PutNamespaceDefaults = Schemas["PutNamespaceDefaults"];
export type RevokedTokens = Schemas["RevokedTokens"];

// Workflows: their versions, each with its suite, and the tests (archi/05, "Suites and the gate").
export type Workflow = Schemas["Workflow"];
export type WorkflowPage = Schemas["WorkflowPage"];
export type WorkflowVersions = Schemas["WorkflowVersions"];
export type SuiteTest = Schemas["SuiteTest"];
export type RetiredTest = Schemas["RetiredTest"];
export type UpdatedTest = Schemas["UpdatedTest"];
/** A test document: the world a run meets, and what must hold at its end. */
export type Test = Schemas["Test"];
export type TestWorld = Schemas["World"];
export type ToolRules = Schemas["ToolRules"];
export type ToolRule = Schemas["ToolRule"];
export type TestSignal = Schemas["TestSignal"];
export type Expectations = Schemas["Expectations"];
export type ExpectedCall = Schemas["ExpectedCall"];
export type TestOrigin = Schemas["TestOrigin"];

export type ListRunsQuery = NonNullable<paths["/v1/runs"]["get"]["parameters"]["query"]>;
export type ListScriptsQuery = NonNullable<paths["/v1/scripts"]["get"]["parameters"]["query"]>;
export type ToolStatsQuery = NonNullable<paths["/v1/tools/stats"]["get"]["parameters"]["query"]>;
export type RunStatsQuery = NonNullable<paths["/v1/runs/stats"]["get"]["parameters"]["query"]>;
export type FailuresQuery = NonNullable<paths["/v1/runs/failures"]["get"]["parameters"]["query"]>;
export type ListWorkflowsQuery = NonNullable<paths["/v1/workflows"]["get"]["parameters"]["query"]>;
export type ListNamespacesQuery = NonNullable<
  paths["/v1/namespaces"]["get"]["parameters"]["query"]
>;
export type NamespaceOrder = NonNullable<ListNamespacesQuery["order"]>;
export type QuotaAlertsQuery = NonNullable<
  paths["/v1/organization/quota-alerts"]["get"]["parameters"]["query"]
>;

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
