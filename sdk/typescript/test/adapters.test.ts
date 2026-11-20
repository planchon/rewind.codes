/**
 * The adapters' shapes against the types of the official Anthropic and OpenAI SDKs: this file
 * does not compile if a tool or a tool result stops fitting their APIs.
 */
import type Anthropic from "@anthropic-ai/sdk";
import type OpenAI from "openai";
import { describe, expect, it } from "vitest";

import { Rewind } from "../src/index.js";

const tools = new Rewind({ url: "http://127.0.0.1:1" }).agentTools({ toolset: "ts", labels: { conversation: "c" } });

describe("the adapters", () => {
  it("fit the Anthropic Messages API", async () => {
    const definitions: Anthropic.Messages.Tool[] = tools.anthropic.tools;
    const toolUse: Anthropic.Messages.ToolUseBlock = {
      type: "tool_use",
      id: "toolu_1",
      name: "run_script",
      input: {},
      caller: { type: "direct" },
    };
    const result: Anthropic.Messages.ToolResultBlockParam = await tools.anthropic.handle(toolUse);
    expect(definitions).toHaveLength(13);
    expect(result).toEqual({
      type: "tool_result",
      tool_use_id: "toolu_1",
      content: "run_script: `code` must be a non-empty string.",
      is_error: true,
    });
  });

  it("fit the OpenAI Chat Completions API", async () => {
    const definitions: OpenAI.Chat.Completions.ChatCompletionTool[] = tools.openai.tools;
    const call: OpenAI.Chat.Completions.ChatCompletionMessageFunctionToolCall = {
      id: "call_1",
      type: "function",
      function: { name: "list_runs", arguments: '{"status":"nope"}' },
    };
    const message: OpenAI.Chat.Completions.ChatCompletionToolMessageParam = await tools.openai.handle(call);
    expect(definitions).toHaveLength(13);
    expect(message.tool_call_id).toBe("call_1");
  });

  it("fit the OpenAI Responses API", async () => {
    const definitions: OpenAI.Responses.Tool[] = tools.openai.responses.tools;
    const call: OpenAI.Responses.ResponseFunctionToolCall = {
      type: "function_call",
      call_id: "call_2",
      name: "get_run",
      arguments: "{}",
    };
    const output: OpenAI.Responses.ResponseInputItem.FunctionCallOutput = await tools.openai.responses.handle(call);
    expect(definitions).toHaveLength(13);
    expect(output).toEqual({
      type: "function_call_output",
      call_id: "call_2",
      output: "get_run: `run_id` must be a non-empty string.",
    });
  });
});
