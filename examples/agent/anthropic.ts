// An agent on Claude that solves a task by writing scripts and running them with Rewind.
//
//   pnpm agent:anthropic "Which of nodejs/node, denoland/deno and oven-sh/bun has the most stars?"
//
// The model gets the agent tools (run_script, get_run, signal_run, retry_run…) and the toolset's
// declarations. Every run it starts belongs to the end user the conversation serves.
import Anthropic from "@anthropic-ai/sdk";
import { Rewind } from "@rewind.codes/sdk";

const anthropic = new Anthropic(); // ANTHROPIC_API_KEY
const rewind = new Rewind(); // REWIND_URL, REWIND_API_KEY

const toolset = await rewind.toolsets.create({ http: { allow: ["api.github.com"] } });

// Who the agent acts for, and which conversation this is: runs carry both.
const user = { id: "user_2fQ81" };
const conversation = { id: crypto.randomUUID() };

const agentTools = rewind.namespace(user.id).agentTools({
  toolset: toolset.id,
  labels: { agent: "repo-analyst", conversation: conversation.id },
});

const system = [
  "You answer questions about GitHub repositories.",
  "Solve each task by writing one TypeScript script and running it with run_script: the script",
  "makes every call the task needs, in loops and in parallel, and returns only the answer.",
  "If a run fails, read the error, fix the script and call retry_run: calls already made are reused.",
  "",
  "What scripts can use:",
  toolset.declarations.prompt,
].join("\n");

const question =
  process.argv[2] ?? "Which of nodejs/node, denoland/deno and oven-sh/bun has the most stars per open issue?";
const messages: Anthropic.Beta.BetaMessageParam[] = [{ role: "user", content: question }];

for (;;) {
  const response = await anthropic.beta.messages.create({
    model: "claude-opus-5-5",
    max_tokens: 16000,
    output_config: { effort: "high" },
    // A request a safety classifier declines is run again on a fallback model.
    betas: ["server-side-fallback-2026-07-01"],
    fallbacks: "default",
    system,
    tools: agentTools.anthropic.tools,
    messages,
  });
  messages.push({ role: "assistant", content: response.content });

  if (response.stop_reason === "refusal") {
    console.log("The model declined the request.");
    break;
  }
  if (response.stop_reason !== "tool_use") {
    for (const block of response.content) if (block.type === "text") console.log(block.text);
    break;
  }

  // Each tool call is handled by the agent tools: run_script starts a run in the user's
  // namespace, with the tool call's id as its idempotency key.
  const calls = response.content.filter((block) => block.type === "tool_use");
  const results = await Promise.all(
    calls.map(async (call) => {
      console.log(`→ ${call.name}`);
      return agentTools.anthropic.handle(call);
    }),
  );
  messages.push({ role: "user", content: results });
}
