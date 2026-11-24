// Names for the DTOs of the API /v1, generated from api/openapi.json (pnpm generate:api).
import type { components, paths } from "../generated/api";

type Schemas = components["schemas"];

export type { paths };

// Statuses (archi/03, archi/05 "Read model"; archi/08, "Waiting is a status"). A run is `queued`
// until a step is committed, `running` while a step is in progress or a call Rewind makes itself
// is in flight or backing off, `waiting` on the outside world otherwise, until it ends.

export type RunStatus = Schemas["RunStatus"];

/**
 * What a waiting run waits for, as the query `waiting:` and the counts name it: a signal, a timer,
 * a harness action's call (`ready` or `claimed`), a call waiting for its approval, deferred or
 * parked, or a quarantine.
 */
export type WaitKind = Schemas["WaitKind"];

/** The runs of a scope by status: the active ones now, the ended ones of the window. */
export type StatusCounts = Schemas["StatusCounts"];
/**
 * A workflow's runs of a window, from the hourly counters: the active ones (queued, running or
 * waiting) together, the others by how they ended.
 */
export type RunCounts = Schemas["RunCounts"];
/** The waiting runs now, by what they wait for (a run may count under several). */
export type WaitCounts = Schemas["WaitCounts"];

/** What a run in a list waits for, each with the event that opened it and its callsite. */
export type RunWait = Schemas["RunWait"];

/** What a run waits for, in its details: a signal, a timer, a call with its state, a quarantine. */
export type Waiting = Schemas["Waiting"];

export type Run = Schemas["Run"];
export type RunSummary = Schemas["RunSummary"];
export type RunPage = Schemas["RunPage"];
export type RunStats = Schemas["RunStats"];
export type FailureSample = Schemas["FailureSample"];
export type Fingerprint = Schemas["Fingerprint"];
export type FingerprintPage = Schemas["FingerprintPage"];

export type RunMode = Schemas["RunMode"];
/** A run's error, a `fail_run`'s and a test's: its name, message and stack. */
export type ErrorInfo = Schemas["ErrorInfo"];
/** One event of a run's history, at its position `idx`. */
export type HistoryEvent = Schemas["HistoryEvent"];
/** What an event of the history says: `run_started`, `schedule_call`, `call_completed`… */
export type HistoryEventBody = Schemas["Event"];
export type EventOf<T extends HistoryEventBody["type"]> = Extract<HistoryEventBody, { type: T }>;
export type StreamMessage = Schemas["StreamMessage"];
export type Payload = Schemas["Payload"];
export type Callsite = Schemas["Callsite"];
export type Labels = Schemas["Labels"];
export type LogLevel = Schemas["LogLevel"];
export type ActionFailure = Schemas["ActionFailure"];
export type ActionFailureKind = Schemas["ActionFailureKind"];
export type AbortReason = Schemas["AbortReason"];
export type CancelReason = Schemas["CancelReason"];
export type ActionStats = Schemas["ActionStats"];
export type ActionStatsPage = Schemas["ActionStatsPage"];
export type ServerInfo = Schemas["ServerInfo"];
export type ActionSetSummary = Schemas["ActionSetSummary"];
export type QuotaReached = Schemas["QuotaReached"];
/**
 * An answer that is not a success. The document flattens an optional `QuotaReached` into it
 * (`quota-exceeded` adds `quota`, `scope`, `limit`, `used`, `resetsAt`), which the generator turns
 * into required fields: they are optional here, as on the wire.
 */
export type Problem = Omit<Schemas["Problem"], keyof QuotaReached> & Partial<QuotaReached>;
export type Quarantine = Schemas["Quarantine"];
export type WorkflowChanged = Schemas["WorkflowChanged"];
export type SecretChanged = Schemas["SecretChanged"];
export type NamespaceChanged = Schemas["NamespaceChanged"];
export type ConnectionChanged = Schemas["ConnectionChanged"];
/** What changed in the control plane, from a change of `GET /v1/changes` (its type, its data). */
export type ControlChange =
  | ({ kind: "workflow" } & WorkflowChanged)
  | ({ kind: "secret" } & SecretChanged)
  | ({ kind: "namespace" } & NamespaceChanged)
  | ({ kind: "connection" } & ConnectionChanged);
/** One interval of a series of runs: its start, and the runs started in it by status. */
export type SeriesInterval = Schemas["SeriesInterval"];
export type DurationStats = Schemas["DurationStats"];
export type Facets = Schemas["Facets"];
export type FacetValue = Schemas["FacetValue"];
export type LabelFacet = Schemas["LabelFacet"];
export type QuerySpan = Schemas["QuerySpan"];
export type SignalAccepted = Schemas["SignalAccepted"];
export type CancelRun = Schemas["CancelRun"];
export type RunSort = NonNullable<ListRunsQuery["sort"]>;

// Sign-in (`/auth`), the organization and access.
export type AuthConfig = Schemas["AuthConfig"];
export type AuthState = Schemas["AuthState"];
export type Authenticated = Extract<AuthState, { status: "authenticated" }>;
export type SignInMethod = Schemas["SignInMethod"];
/** A person: a member of an organization, as WorkOS knows them. */
export type Person = Schemas["Person"];
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

// A person's connections (archi/06, "Connections") and the consent of an MCP client.
/** What a connection may do in an organization: `read`, or `run` (start, signal, cancel too). */
export type ConnectionAccess = Schemas["Access"];
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

// Namespaces: the end users of the organization, their quotas, use and alerts.
export type Namespace = Schemas["Namespace"];
export type NamespaceList = Schemas["NamespaceList"];
export type NamespaceDetail = Schemas["NamespaceDetail"];
export type NamespaceDefaults = Schemas["NamespaceDefaults"];
export type NamespacePatch = Schemas["NamespacePatch"];
export type NamespaceNames = Schemas["NamespaceNames"];
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

// Secrets: the organization's and each namespace's, never a value; the keys Rewind signs with.
export type Secret = Schemas["Secret"];
export type SecretUsage = Schemas["SecretUsage"];
export type PutSecret = Schemas["PutSecret"];
/** A workflow whose latest version uses a secret, and for which servers. */
export type WorkflowSecrets = Schemas["WorkflowSecrets"];
export type DailyReads = Schemas["DailyReads"];
export type SigningKey = Schemas["SigningKey"];
export type SigningKeyList = Schemas["SigningKeyList"];

// Workflows (archi/01, "Workflows"): one entity, an id the API assigns, an optional name unique in
// its scope (a namespace's, or the organization's), versions numbered from 1, each with its code,
// interface and action set, and a suite; a fork of an organization's workflow names its source.
export type WorkflowSummary = Schemas["WorkflowSummary"];
export type WorkflowPage = Schemas["WorkflowPage"];
export type Workflow = Schemas["Workflow"];
export type WorkflowVersion = Schemas["WorkflowVersion"];
export type WorkflowSource = Schemas["WorkflowSource"];
export type RunWorkflow = Schemas["RunWorkflow"];
export type CodeComment = Schemas["CodeComment"];
export type PatchWorkflow = Schemas["PatchWorkflow"];
/** A test of a suite, now or before: when it joined, and when and why it left. */
export type SuiteEntry = Schemas["SuiteEntry"];
export type SuiteTest = Schemas["SuiteTest"];
export type RetiredTest = Schemas["RetiredTest"];
export type ReplacedTest = Schemas["ReplacedTest"];
/** A test document: the world a run meets, and what must hold at its end. */
export type Test = Schemas["Test"];
export type TestWorld = Schemas["World"];
export type ActionRules = Schemas["ActionRules"];
export type ActionRule = Schemas["ActionRule"];
export type TestSignal = Schemas["TestSignal"];
export type Expectations = Schemas["Expectations"];
export type ExpectedCall = Schemas["ExpectedCall"];
/** The run a recorded test comes from: its id, namespace and the version it executed. */
export type TestFromRun = Schemas["TestFromRun"];

export type ListRunsQuery = NonNullable<paths["/v1/runs"]["get"]["parameters"]["query"]>;
export type ActionStatsQuery = NonNullable<
  paths["/v1/actions/stats"]["get"]["parameters"]["query"]
>;
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
  "queued",
  "running",
  "waiting",
  "completed",
  "failed",
  "cancelled",
  "aborted",
];

/** The keys of a workflow's counts, in the order its outcomes show: the active runs first. */
export const RUN_COUNT_KEYS: readonly (keyof RunCounts)[] = [
  "active",
  "completed",
  "failed",
  "cancelled",
  "aborted",
];

export const WAIT_KINDS: readonly WaitKind[] = [
  "signal",
  "timer",
  "harness",
  "approval",
  "deferred",
  "parked",
  "quarantined",
];

export const ACTION_FAILURE_KINDS: readonly ActionFailureKind[] = [
  "error",
  "unavailable",
  "timeout",
  "outcome_unknown",
];

/** Whether a run is active: queued, running or waiting, not ended. */
export function isActive(status: RunStatus): boolean {
  return status === "queued" || status === "running" || status === "waiting";
}

/** Whether a status is final: nothing will be appended to the history. */
export function isEnded(status: RunStatus): boolean {
  return !isActive(status);
}
