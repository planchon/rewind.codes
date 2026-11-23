export { Rewind, type RewindOptions, type Webhooks } from "./client.js";

export {
  NamespaceRequiredError,
  RewindApiError,
  RewindConnectionError,
  RewindError,
  isProblem,
  type ProblemSlug,
} from "./errors.js";
export { formatDuration, toMillis } from "./duration.js";
export { isNamespaceName, type TransportOptions } from "./http.js";

export type { RequestOptions } from "./resources/toolsets.js";
export { Toolsets } from "./resources/toolsets.js";
export { Workflows, type ListWorkflowsParams, type RecordTestsParams } from "./resources/workflows.js";
export { Tests } from "./resources/tests.js";
export { Scripts, type CheckParams, type ListScriptsParams, type ScriptTitleParams } from "./resources/scripts.js";
export {
  Runs,
  type EventsParams,
  type FailuresParams,
  type ForkRunParams,
  type ListRunsParams,
  type RunStatsParams,
  type ScriptTarget,
  type StartRunOptions,
  type StartRunParams,
  type WorkflowTarget,
} from "./resources/runs.js";
export { Tasks, type TaskFailure, type TaskStreamParams } from "./resources/tasks.js";
export {
  ApiKeys,
  type PayloadOptions,
  Payloads,
  Server,
  Subscriptions,
  Tools,
  type ToolStatsParams,
} from "./resources/admin.js";
export {
  NamespaceSecrets,
  Secrets,
  SigningKeys,
  isSecretName,
  rewindIssuer,
  rewindJwksUrl,
} from "./resources/secrets.js";
export {
  DefaultLimits,
  Namespaces,
  TokenKeys,
  Tokens,
  type CreatedToken,
  type ListNamespacesParams,
  type NewTokenKey,
  type QuotaAlertsParams,
} from "./resources/namespaces.js";

export {
  HostToolError,
  HostToolWorker,
  type DeferredTask,
  type HostToolContext,
  type HostToolHandler,
  type WorkerErrorInfo,
  type WorkerOptions,
  type WorkerTimings,
} from "./worker.js";
export { Approvals, pendingApprovals, type PendingApproval } from "./approvals.js";
export { WebhookVerificationFailed, verifyWebhook, type WebhookHeaders } from "./webhooks.js";
export {
  RewindSignatureError,
  verifyRewindCall,
  type ReceivedRequest,
  type RewindCall,
  type SignatureCheck,
  type VerifyRewindCallOptions,
} from "./signatures.js";

export {
  AgentTools,
  idempotencyKeyOf,
  runIdempotencyKey,
  type AgentToolResult,
  type AgentSecrets,
  type AgentToolsClient,
  type AgentToolsOptions,
  type ListedSecret,
  type HandleOptions,
} from "./agent-tools/agent-tools.js";
export {
  AGENT_TOOL_NAMES,
  type AgentToolDefinition,
  type AgentToolName,
  type JsonSchema,
} from "./agent-tools/definitions.js";
export {
  anthropicTools,
  openaiChatTools,
  openaiResponsesTools,
  type AnthropicTool,
  type AnthropicToolResult,
  type AnthropicToolUse,
  type OpenAIChatTool,
  type OpenAIChatToolCall,
  type OpenAIChatToolMessage,
  type OpenAIFunctionCall,
  type OpenAIFunctionCallOutput,
  type OpenAIResponsesTool,
} from "./agent-tools/adapters.js";
export { OUTPUT_LIMIT } from "./agent-tools/bound.js";

export * from "./types.js";
export type { components, operations, paths, webhooks } from "./generated/api.js";
