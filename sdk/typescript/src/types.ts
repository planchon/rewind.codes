/**
 * The types of the API `/v1`, named as in `api/openapi.json`. They are aliases of the types
 * generated in `generated/api.ts`, so a change of the API shows in the SDK's types.
 */
import type { components, operations } from "./generated/api.js";

type Schemas = components["schemas"];

export type AbortKind = Schemas["AbortKind"];
export type AbortReason = Schemas["AbortReason"];
/** An API of a toolset, described by its OpenAPI document: one tool per operation, called by Rewind. */
export type Api = Schemas["Api"];
export type ApiKey = Schemas["ApiKey"];
export type ApiSummary = Schemas["ApiSummary"];
/** With `secret-required`: one attachment that refused a start, a fork or a save, and what may be attached instead. */
export type AttachmentProblem = Schemas["AttachmentProblem"];
export type AttachmentProblemKind = Schemas["AttachmentProblemKind"];
/** How a server's or an API's calls prove who calls: none, `rewind` (signed by the organization's key), or `secret`. */
export type Auth = Schemas["Auth"];
export type Callsite = Schemas["Callsite"];
export type CallOrder = Schemas["CallOrder"];
export type CancelReason = Schemas["CancelReason"];
/**
 * The `data` of an event of the change stream (`GET /v1/changes`): a run's summary (`run`), what changed in the
 * control plane (`script`, `workflow`, `secret`, `namespace`, `connection`), or `{}` (`resync`).
 */
export type ChangeEvent = Schemas["ChangeEvent"];
export type CheckResult = Schemas["CheckResult"];
export type ConnectionChanged = Schemas["ConnectionChanged"];
export type CreateApiKey = Schemas["CreateApiKey"];
export type CreateSubscription = Schemas["CreateSubscription"];
export type CreateToken = Schemas["CreateToken"];
export type CreateTokenKey = Schemas["CreateTokenKey"];
export type CreateToolset = Schemas["CreateToolset"];
export type CreateWorkflow = Schemas["CreateWorkflow"];
export type CredentialsExpired = Schemas["CredentialsExpired"];
export type CredentialsExpiredReason = Schemas["CredentialsExpiredReason"];
export type DailyReads = Schemas["DailyReads"];
export type DayUsage = Schemas["DayUsage"];
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
export type ExpectedCall = Schemas["ExpectedCall"];
export type ExpectedError = Schemas["ExpectedError"];
export type Facets = Schemas["Facets"];
export type FacetValue = Schemas["FacetValue"];
export type FailedCall = Schemas["FailedCall"];
export type FailureGroup = Schemas["FailureGroup"];
export type FailureGroupPage = Schemas["FailureGroupPage"];
export type FailureSample = Schemas["FailureSample"];
export type ForkParent = Schemas["ForkParent"];
export type HostTool = Schemas["HostTool"];
export type HttpTool = Schemas["HttpTool"];
export type Jwk = Schemas["Jwk"];
export type Jwks = Schemas["Jwks"];
export type LabelFacet = Schemas["LabelFacet"];
export type Labels = Schemas["Labels"];
export type LastRead = Schemas["LastRead"];
export type Latency = Schemas["Latency"];
export type Location = Schemas["Location"];
export type LogLevel = Schemas["LogLevel"];
export type LogLine = Schemas["LogLine"];
export type McpServer = Schemas["McpServer"];
export type McpTransport = Schemas["McpTransport"];
export type Namespace = Schemas["Namespace"];
export type NamespaceChanged = Schemas["NamespaceChanged"];
export type NamespaceDefaults = Schemas["NamespaceDefaults"];
export type NamespaceDetail = Schemas["NamespaceDetail"];
export type NamespaceList = Schemas["NamespaceList"];
/** The customer's own id for an end user: 1 to 128 ASCII letters, digits, `-`, `_`, `.`, `:` and `@`. */
export type NamespaceName = Schemas["NamespaceName"];
/** How `namespaces.list` orders the namespaces: by a use of the month, the largest first, or by name. */
export type NamespaceOrder = NonNullable<NonNullable<operations["listNamespaces"]["parameters"]["query"]>["order"]>;
export type OnInterruption = Schemas["OnInterruption"];
export type ParkedReason = Schemas["ParkedReason"];
export type Payload = Schemas["Payload"];
export type PinnedWorkflow = Schemas["PinnedWorkflow"];
export type Permission = Schemas["Permission"];
/**
 * A problem, as the API answers errors. With `quota-exceeded`, it also says which quota was reached (`quota`,
 * `scope`, `limit`, `used`, `resetsAt`). The document flattens that part as a nullable `allOf` member, which
 * `openapi-typescript` reads as always present: it is optional here.
 */
export type Problem = Omit<Schemas["Problem"], keyof QuotaReached> & Partial<QuotaReached>;
export type PutNamespace = Schemas["PutNamespace"];
export type PutNamespaceDefaults = Schemas["PutNamespaceDefaults"];
export type PutSecret = Schemas["PutSecret"];
export type Quarantine = Schemas["Quarantine"];
export type QuarantineCause = Schemas["QuarantineCause"];
export type QuerySpan = Schemas["QuerySpan"];
export type QuotaAlert = Schemas["QuotaAlert"];
export type QuotaAlertList = Schemas["QuotaAlertList"];
export type QuotaKind = Schemas["QuotaKind"];
export type QuotaReached = Schemas["QuotaReached"];
export type QuotaScope = Schemas["QuotaScope"];
export type QuotaUsage = Schemas["QuotaUsage"];
/** Limits, each optional: the tenant's defaults, a namespace's exceptions, the limits that apply, the ceiling. */
export type Quotas = Schemas["Quotas"];
export type RecordedTest = Schemas["RecordedTest"];
export type RecordedTests = Schemas["RecordedTests"];
export type RecordTests = Schemas["RecordTests"];
export type RetiredTest = Schemas["RetiredTest"];
export type RevokedTokens = Schemas["RevokedTokens"];
export type Role = Schemas["Role"];
export type Route = Schemas["Route"];
export type RuleError = Schemas["RuleError"];
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
export type RunWorkflow = Schemas["RunWorkflow"];
export type Script = Schemas["Script"];
export type ScriptChanged = Schemas["ScriptChanged"];
export type ScriptError = Schemas["ScriptError"];
export type ScriptPage = Schemas["ScriptPage"];
export type ScriptSummary = Schemas["ScriptSummary"];
/** A secret as listed: its name, scope, labels, who wrote it and when, its reads per day and its last read; never its value. */
export type Secret = Schemas["Secret"];
export type SecretChanged = Schemas["SecretChanged"];
export type SecretEncoding = Schemas["SecretEncoding"];
export type SecretList = Schemas["SecretList"];
export type SecretScope = Schemas["SecretScope"];
/** What reads a secret, and what deleting it would stop. */
export type SecretUsage = Schemas["SecretUsage"];
export type SeriesBucket = Schemas["SeriesBucket"];
/** What the server is: its version and epochs, the node that answered, the partitions and nodes of the deployment. */
export type ServerInfo = Schemas["ServerInfo"];
export type ServerSummary = Schemas["ServerSummary"];
export type Severity = Schemas["Severity"];
export type SignalAccepted = Schemas["SignalAccepted"];
/** The claims of a token the customer signs itself, with a key it registered (`kid` `rk_…`). */
export type SignedTokenClaims = Schemas["SignedTokenClaims"];
export type SigningKey = Schemas["SigningKey"];
/** The organization's signing keys, the issuer of its JWTs and the address of its public keys. */
export type SigningKeyList = Schemas["SigningKeyList"];
export type SkippedOperation = Schemas["SkippedOperation"];
export type SkippedRun = Schemas["SkippedRun"];
export type StartedBy = Schemas["StartedBy"];
export type StatusCounts = Schemas["StatusCounts"];
export type StreamEvent = Schemas["StreamEvent"];
export type Subscription = Schemas["Subscription"];
export type SuiteTest = Schemas["SuiteTest"];
/** A host-tool call handed to the worker on its task stream, with its token. */
export type HandedTask = Schemas["HandedTask"];
/** A task handed on the task stream that no longer waits for its answer: its token. */
export type TaskGone = Schemas["TaskGone"];
/** An event of the task stream: a task handed (`task`) or gone (`gone`). */
export type TaskStreamEvent = { type: "task"; task: HandedTask } | { type: "gone"; token: string };
export type TaskState = Schemas["TaskState"];
/**
 * A test: the world a run meets (its input, the tools' answers, signals, its start and seed) and what must hold at the
 * end. It never names its script.
 */
export type Test = Schemas["Test"];
/** A stored test with a new expectation: `{ from: "<digest>", expect }`. */
export type TestFrom = Schemas["TestFrom"];
/** A test given to `scripts.test` or to a save: inline, by the digest of a stored test, or `{ from, expect }`. */
export type TestInput = Schemas["TestInput"];
export type TestOrigin = Schemas["TestOrigin"];
/** Why a test did not pass: `stale` and `incompatible` were not run, `failed` ran and an expectation does not hold. */
export type TestProblem = Schemas["TestProblem"];
export type TestRequest = Schemas["TestRequest"];
export type TestResult = Schemas["TestResult"];
export type TestRetirement = Schemas["TestRetirement"];
export type TestRun = Schemas["TestRun"];
export type TestSignal = Schemas["TestSignal"];
export type TestUpdate = Schemas["TestUpdate"];
export type Token = Schemas["Token"];
export type TokenAlgorithm = Schemas["TokenAlgorithm"];
export type TokenKey = Schemas["TokenKey"];
export type Tool = Schemas["Tool"];
export type ToolFailure = Schemas["ToolFailure"];
export type ToolFailureKind = Schemas["ToolFailureKind"];
export type ToolMatch = Schemas["ToolMatch"];
export type ToolPolicy = Schemas["ToolPolicy"];
export type ToolRule = Schemas["ToolRule"];
export type ToolRules = Schemas["ToolRules"];
export type ToolSchemaChanged = Schemas["ToolSchemaChanged"];
export type ToolSearchResults = Schemas["ToolSearchResults"];
export type ToolStats = Schemas["ToolStats"];
export type ToolStatsPage = Schemas["ToolStatsPage"];
export type Toolset = Schemas["Toolset"];
export type Trace = Schemas["Trace"];
export type UpdatedTest = Schemas["UpdatedTest"];
export type WaitCounts = Schemas["WaitCounts"];
export type Waiting = Schemas["Waiting"];
export type WaitKind = Schemas["WaitKind"];
export type WebhookEvent = Schemas["WebhookEvent"];
export type WebhookEventType = Schemas["WebhookEventType"];
export type Workflow = Schemas["Workflow"];
export type WorkflowAttaching = Schemas["WorkflowAttaching"];
export type WorkflowChanged = Schemas["WorkflowChanged"];
export type WorkflowPage = Schemas["WorkflowPage"];
export type WorkflowVersions = Schemas["WorkflowVersions"];
export type World = Schemas["World"];

/** A run that has ended: its status is final. */
export const TERMINAL_STATUSES: readonly RunStatus[] = ["completed", "failed", "cancelled", "aborted"];

/** Whether a run's status is final. */
export function isTerminal(status: RunStatus): boolean {
  return TERMINAL_STATUSES.includes(status);
}
