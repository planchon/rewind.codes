/**
 * The types of the API `/v1`, named as in `api/openapi.json`. They are aliases of the types
 * generated in `generated/api.ts`, so a change of the API shows in the SDK's types.
 */
import type { components, operations } from "./generated/api.js";

type Schemas = components["schemas"];

export type AbortKind = Schemas["AbortKind"];
export type AbortReason = Schemas["AbortReason"];
export type Action = Schemas["Action"];
export type ActionFailure = Schemas["ActionFailure"];
export type ActionFailureKind = Schemas["ActionFailureKind"];
export type ActionMatch = Schemas["ActionMatch"];
export type ActionPolicy = Schemas["ActionPolicy"];
export type ActionRule = Schemas["ActionRule"];
export type ActionRules = Schemas["ActionRules"];
export type ActionSchemaChanged = Schemas["ActionSchemaChanged"];
export type ActionSearchResults = Schemas["ActionSearchResults"];
export type ActionSet = Schemas["ActionSet"];
export type ActionSetSummary = Schemas["ActionSetSummary"];
export type ActionStats = Schemas["ActionStats"];
export type ActionStatsPage = Schemas["ActionStatsPage"];
/** An API of an action set, described by its OpenAPI document: one action per operation, called by Rewind. */
export type Api = Schemas["Api"];
export type ApiKey = Schemas["ApiKey"];
export type ApiSummary = Schemas["ApiSummary"];
/** How a server's or an API's calls prove who calls: none, `rewind` (signed by the organization's key), or `secret`. */
export type Auth = Schemas["Auth"];
/** A call handed on the call stream that no longer waits for its answer: its token. */
export type CallGone = Schemas["CallGone"];
/** A message of the call stream: a call handed (`call`) or gone (`gone`). */
export type CallStreamMessage = { type: "call"; call: HandedCall } | { type: "gone"; token: string };
export type CallOrder = Schemas["CallOrder"];
export type CallState = Schemas["CallState"];
export type Callsite = Schemas["Callsite"];
export type CancelReason = Schemas["CancelReason"];
/** The `data` of a change of the change stream (`GET /v1/changes`): a run's summary (`run`), what changed in the
 * control plane (`workflow`, `secret`, `namespace`, `connection`), or `{}` (`resync`). */
export type Change = Schemas["Change"];
export type ChangeTests = Schemas["ChangeTests"];
export type CheckRequest = Schemas["CheckRequest"];
export type CheckResult = Schemas["CheckResult"];
export type CodeComment = Schemas["CodeComment"];
export type ConnectionChanged = Schemas["ConnectionChanged"];
export type CreateActionSet = Schemas["CreateActionSet"];
export type CreateApiKey = Schemas["CreateApiKey"];
export type CreateSubscription = Schemas["CreateSubscription"];
export type CreateToken = Schemas["CreateToken"];
export type CreateTokenKey = Schemas["CreateTokenKey"];
export type CreateVersion = Schemas["CreateVersion"];
export type CreateWorkflow = Schemas["CreateWorkflow"];
export type DailyReads = Schemas["DailyReads"];
export type DayUsage = Schemas["DayUsage"];
export type DeclarationFormat = Schemas["DeclarationFormat"];
export type Declarations = Schemas["Declarations"];
export type DeclarationsText = Schemas["DeclarationsText"];
export type Deliveries = Schemas["Deliveries"];
export type Diagnostic = Schemas["Diagnostic"];
/** A duration: `"500ms"`, `"30s"`, `"1h30m"`, `"2d"`, `"1w"`, or a number of milliseconds. */
export type Duration = Schemas["Duration"];
export type DurationStats = Schemas["DurationStats"];
/** How a run, or a test's run, failed: the error's name, message and stack; for a run's error, where it failed. */
export type ErrorInfo = Schemas["ErrorInfo"];
export type Event = Schemas["Event"];
export type Expectations = Schemas["Expectations"];
export type ExpectedCall = Schemas["ExpectedCall"];
export type ExpectedError = Schemas["ExpectedError"];
export type FacetValue = Schemas["FacetValue"];
export type Facets = Schemas["Facets"];
export type FailedCall = Schemas["FailedCall"];
export type FailureSample = Schemas["FailureSample"];
export type Fingerprint = Schemas["Fingerprint"];
export type FingerprintPage = Schemas["FingerprintPage"];
export type ForkRun = Schemas["ForkRun"];
export type ForkSource = Schemas["ForkSource"];
/** A call of a harness action handed to a worker on its call stream, with its token. */
export type HandedCall = Schemas["HandedCall"];
export type HarnessAction = Schemas["HarnessAction"];
/** One event of a run's history, by its position `idx`. */
export type HistoryEvent = Schemas["HistoryEvent"];
export type HttpAction = Schemas["HttpAction"];
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
export type ParkedReason = Schemas["ParkedReason"];
export type PatchWorkflow = Schemas["PatchWorkflow"];
export type Payload = Schemas["Payload"];
export type Permission = Schemas["Permission"];
/**
 * A problem, as the API answers errors. With `quota-exceeded`, it also says which quota was reached (`quota`,
 * `scope`, `limit`, `used`, `resetsAt`). The document flattens that part as a nullable `allOf` member, which
 * `openapi-typescript` reads as always present: it is optional here.
 */
export type Problem = Omit<Schemas["Problem"], keyof QuotaReached> & Partial<QuotaReached>;
export type PinnedWorkflow = Schemas["PinnedWorkflow"];
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
/** Limits, each optional: the organization's defaults, a namespace's exceptions, the limits that apply, the ceiling. */
export type Quotas = Schemas["Quotas"];
export type RecordTests = Schemas["RecordTests"];
export type RecordedTest = Schemas["RecordedTest"];
export type RecordedTests = Schemas["RecordedTests"];
export type ReplacedTest = Schemas["ReplacedTest"];
export type RetiredTest = Schemas["RetiredTest"];
export type RevokedTokens = Schemas["RevokedTokens"];
export type Role = Schemas["Role"];
export type Route = Schemas["Route"];
export type RuleError = Schemas["RuleError"];
export type Run = Schemas["Run"];
export type RunCounts = Schemas["RunCounts"];
export type RunMode = Schemas["RunMode"];
export type RunPage = Schemas["RunPage"];
export type RunStats = Schemas["RunStats"];
export type RunStatus = Schemas["RunStatus"];
export type RunSummary = Schemas["RunSummary"];
export type RunWait = Schemas["RunWait"];
export type RunWebhook = Schemas["RunWebhook"];
export type RunWorkflow = Schemas["RunWorkflow"];
/** A secret as listed: its name, scope, labels, who wrote it and when, its reads per day and its last read; never its value. */
export type Secret = Schemas["Secret"];
export type SecretChanged = Schemas["SecretChanged"];
export type SecretEncoding = Schemas["SecretEncoding"];
export type SecretExpired = Schemas["SecretExpired"];
export type SecretExpiredReason = Schemas["SecretExpiredReason"];
export type SecretList = Schemas["SecretList"];
/** With `secret-required`: one secret that refused a start, a fork or a version, and the secrets the caller may use instead (`usable`). */
export type SecretProblem = Schemas["SecretProblem"];
export type SecretProblemKind = Schemas["SecretProblemKind"];
export type SecretScope = Schemas["SecretScope"];
/** What reads a secret, and what deleting it would stop. */
export type SecretUsage = Schemas["SecretUsage"];
export type SeriesInterval = Schemas["SeriesInterval"];
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
export type StartRun = Schemas["StartRun"];
export type StartedBy = Schemas["StartedBy"];
export type StatusCounts = Schemas["StatusCounts"];
export type StreamMessage = Schemas["StreamMessage"];
export type Subscription = Schemas["Subscription"];
export type SuiteEntry = Schemas["SuiteEntry"];
export type SuiteTest = Schemas["SuiteTest"];
/** A test: the world a run meets (its input, the actions' answers, signals, its start and seed) and what must hold at
 * the end. It never names its code. */
export type Test = Schemas["Test"];
/** A stored test with a new expectation: `{ test: "<digest>", expect }`. */
export type TestFrom = Schemas["TestFrom"];
/** The run a recorded test comes from: the run, the version it executed and its namespace. */
export type TestFromRun = Schemas["TestFromRun"];
/** A test given to `workflows.test`, to a version or to a suite: inline, by the digest of a stored test, or
 * `{ test, expect }`. */
export type TestInput = Schemas["TestInput"];
/** Why a test did not pass: `stale` and `incompatible` were not run, `failed` ran and an expectation does not hold. */
export type TestProblem = Schemas["TestProblem"];
/** A test of the suite replaced, with a reason: by a whole new test (`new`), or by itself with a new `expect`. */
export type TestReplacement = Schemas["TestReplacement"];
/** What `workflows.test` ran: each test's outcome, and whether all passed. */
export type TestReport = Schemas["TestReport"];
export type TestRequest = Schemas["TestRequest"];
export type TestResult = Schemas["TestResult"];
export type TestRetirement = Schemas["TestRetirement"];
export type TestSignal = Schemas["TestSignal"];
export type Token = Schemas["Token"];
export type TokenAlgorithm = Schemas["TokenAlgorithm"];
export type TokenKey = Schemas["TokenKey"];
/** A run as text for a model: a header, then one line per event. */
export type Transcript = Schemas["Transcript"];
export type WaitCounts = Schemas["WaitCounts"];
export type WaitKind = Schemas["WaitKind"];
export type Waiting = Schemas["Waiting"];
/** The body of a webhook Rewind sends: `{ type, timestamp, data }`. */
export type Webhook = Schemas["Webhook"];
export type WebhookType = Schemas["WebhookType"];
/** A workflow: its id, name, description, scope, source, versions and suite, and the version shown with its code. */
export type Workflow = Schemas["Workflow"];
export type WorkflowChanged = Schemas["WorkflowChanged"];
export type WorkflowPage = Schemas["WorkflowPage"];
export type WorkflowSecrets = Schemas["WorkflowSecrets"];
export type WorkflowSource = Schemas["WorkflowSource"];
/** A workflow in a list: its latest version, and what its runs did in the window. */
export type WorkflowSummary = Schemas["WorkflowSummary"];
/** A version of a workflow: its number (`version`), its `digest`, its code's digest, interface, action set and suite. */
export type WorkflowVersion = Schemas["WorkflowVersion"];
export type World = Schemas["World"];

/** The statuses of a run that has ended: they are final. */
export const ENDED_STATUSES: readonly RunStatus[] = ["completed", "failed", "cancelled", "aborted"];

/** Whether a run has ended: completed, failed, cancelled or aborted. An active run is queued, running or waiting. */
export function isEnded(status: RunStatus): boolean {
  return ENDED_STATUSES.includes(status);
}
