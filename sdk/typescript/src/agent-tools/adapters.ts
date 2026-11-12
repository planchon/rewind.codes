/**
 * The agent tools in the tool formats of the Anthropic and OpenAI APIs. The types are the
 * shapes those APIs take and return, written here so the SDK depends on neither SDK.
 */
import type { AgentToolDefinition, JsonSchema } from "./definitions.js";

/** A tool for the Anthropic Messages API (`tools`). */
export interface AnthropicTool {
  name: string;
  description: string;
  input_schema: JsonSchema;
}

/** A `tool_use` content block of an Anthropic answer. */
export interface AnthropicToolUse {
  type: "tool_use";
  id: string;
  name: string;
  input: unknown;
}

/** A `tool_result` content block, for the next user message. */
export interface AnthropicToolResult {
  type: "tool_result";
  tool_use_id: string;
  content: string;
  is_error?: boolean;
}

/** A tool for the OpenAI Chat Completions API (`tools`). */
export interface OpenAIChatTool {
  type: "function";
  function: { name: string; description: string; parameters: JsonSchema };
}

/** A tool call of a Chat Completions answer (`message.tool_calls[]`). */
export interface OpenAIChatToolCall {
  id: string;
  type: "function";
  function: { name: string; arguments: string };
}

/** The `tool` message answering a tool call. */
export interface OpenAIChatToolMessage {
  role: "tool";
  tool_call_id: string;
  content: string;
}

/** A tool for the OpenAI Responses API (`tools`). */
export interface OpenAIResponsesTool {
  type: "function";
  name: string;
  description: string;
  parameters: JsonSchema;
  strict: false;
}

/** A `function_call` output item of a Responses API answer. */
export interface OpenAIFunctionCall {
  type: "function_call";
  call_id: string;
  name: string;
  arguments: string;
}

/** The `function_call_output` input item answering it. */
export interface OpenAIFunctionCallOutput {
  type: "function_call_output";
  call_id: string;
  output: string;
}

export function anthropicTools(definitions: readonly AgentToolDefinition[]): AnthropicTool[] {
  return definitions.map((d) => ({ name: d.name, description: d.description, input_schema: d.inputSchema }));
}

export function openaiChatTools(definitions: readonly AgentToolDefinition[]): OpenAIChatTool[] {
  return definitions.map((d) => ({
    type: "function",
    function: { name: d.name, description: d.description, parameters: d.inputSchema },
  }));
}

export function openaiResponsesTools(definitions: readonly AgentToolDefinition[]): OpenAIResponsesTool[] {
  return definitions.map((d) => ({
    type: "function",
    name: d.name,
    description: d.description,
    parameters: d.inputSchema,
    strict: false,
  }));
}
