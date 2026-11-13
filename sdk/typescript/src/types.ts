/**
 * The types of the API `/v1`, named as in `api/openapi.json`. They are aliases of the types
 * generated in `generated/api.ts`, so a change of the API shows in the SDK's types.
 */
import type { components } from "./generated/api.js";

type Schemas = components["schemas"];

export type AbortKind = Schemas["AbortKind"];
export type AbortReason = Schemas["AbortReason"];
export type ApiKey = Schemas["ApiKey"];
export type Callsite = Schemas["Callsite"];
export type CancelReason = Schemas["CancelReason"];
export type CheckResult = Schemas["CheckResult"];
export type CreateApiKey = Schemas["CreateApiKey"];
export type CreateSubscription = Schemas["CreateSubscription"];
export type CreateToolset = Schemas["CreateToolset"];
export type Credentials = Schemas["Credentials"];
export type CredentialsExpired = Schemas["CredentialsExpired"];
export type DeclarationFormat = Schemas["DeclarationFormat"];
export type Declarations = Schemas["Declarations"];
export type DeclarationsText = Schemas["DeclarationsText"];
export type Diagnostic = Schemas["Diagnostic"];
/** A duration: `"500ms"`, `"30s"`, `"1h30m"`, `"2d"`, `"1w"`, or a number of milliseconds. */
export type Duration = Schemas["Duration"];
export type DurationStats = Schemas["DurationStats"];
export type Entry = Schemas["Entry"];
export type Event = Schemas["Event"];
export type Expectations = Schemas["Expectations"];
export type ExpectedError = Schemas["ExpectedError"];
export type Facets = Schemas["Facets"];
export type FacetValue = Schemas["FacetValue"];
export type FailedCall = Schemas["FailedCall"];
export type FailureGroup = Schemas["FailureGroup"];
export type FailureGroupPage = Schemas["FailureGroupPage"];
export type FailureSample = Schemas["FailureSample"];
export type ForkParent = Schemas["ForkParent"];
export type HeartbeatAck = Schemas["HeartbeatAck"];
export type HostTool = Schemas["HostTool"];
export type HttpTool = Schemas["HttpTool"];
export type LabelFacet = Schemas["LabelFacet"];
export type Labels = Schemas["Labels"];
export type Latency = Schemas["Latency"];
export type Location = Schemas["Location"];
export type LogLevel = Schemas["LogLevel"];
export type LogLine = Schemas["LogLine"];
export type McpServer = Schemas["McpServer"];
export type McpTransport = Schemas["McpTransport"];
export type OnInterruption = Schemas["OnInterruption"];
export type ParkedReason = Schemas["ParkedReason"];
export type Payload = Schemas["Payload"];
export type Problem = Schemas["Problem"];
export type Quarantine = Schemas["Quarantine"];
export type QuarantineCause = Schemas["QuarantineCause"];
export type QuerySpan = Schemas["QuerySpan"];
export type Role = Schemas["Role"];
export type Route = Schemas["Route"];
export type Run = Schemas["Run"];
export type RunCounts = Schemas["RunCounts"];
export type RunError = Schemas["RunError"];
export type RunEvent = Schemas["RunEvent"];
export type RunMode = Schemas["RunMode"];
export type RunPage = Schemas["RunPage"];
export type RunStats = Schemas["RunStats"];
export type RunStatus = Schemas["RunStatus"];
export type RunSummary = Schemas["RunSummary"];
export type RunWait = Schemas["RunWait"];
export type ScenarioSignal = Schemas["ScenarioSignal"];
export type Script = Schemas["Script"];
export type ScriptError = Schemas["ScriptError"];
export type ScriptPage = Schemas["ScriptPage"];
export type ScriptSummary = Schemas["ScriptSummary"];
export type SeriesBucket = Schemas["SeriesBucket"];
export type ServerCredentials = Schemas["ServerCredentials"];
export type ServerInfo = Schemas["ServerInfo"];
export type ServerRole = Schemas["ServerRole"];
export type ServerSummary = Schemas["ServerSummary"];
export type Severity = Schemas["Severity"];
export type SignalAccepted = Schemas["SignalAccepted"];
export type StatusCounts = Schemas["StatusCounts"];
export type StreamEvent = Schemas["StreamEvent"];
export type Subscription = Schemas["Subscription"];
export type Task = Schemas["Task"];
export type TaskBatch = Schemas["TaskBatch"];
export type TaskState = Schemas["TaskState"];
export type TestRequest = Schemas["TestRequest"];
export type TestResult = Schemas["TestResult"];
export type Tool = Schemas["Tool"];
export type ToolFailure = Schemas["ToolFailure"];
export type ToolFailureKind = Schemas["ToolFailureKind"];
export type ToolMatch = Schemas["ToolMatch"];
export type ToolPolicy = Schemas["ToolPolicy"];
export type ToolSchemaChanged = Schemas["ToolSchemaChanged"];
export type ToolSearchResults = Schemas["ToolSearchResults"];
export type ToolStats = Schemas["ToolStats"];
export type ToolStatsPage = Schemas["ToolStatsPage"];
export type Toolset = Schemas["Toolset"];
export type Trace = Schemas["Trace"];
export type WaitCounts = Schemas["WaitCounts"];
export type Waiting = Schemas["Waiting"];
export type WaitKind = Schemas["WaitKind"];
export type WebhookEvent = Schemas["WebhookEvent"];
export type WebhookEventType = Schemas["WebhookEventType"];

/** A run that has ended: its status is final. */
export const TERMINAL_STATUSES: readonly RunStatus[] = ["completed", "failed", "cancelled", "aborted"];

/** Whether a run's status is final. */
export function isTerminal(status: RunStatus): boolean {
  return TERMINAL_STATUSES.includes(status);
}
