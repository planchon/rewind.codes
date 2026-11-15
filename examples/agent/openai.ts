// The same agent on the OpenAI Responses API.
//
//   OPENAI_MODEL=gpt-5 pnpm agent:openai "Which of nodejs/node, denoland/deno and oven-sh/bun has the most stars?"
import OpenAI from "openai";
import { Rewind } from "@rewind.codes/sdk";

const openai = new OpenAI(); // OPENAI_API_KEY
const rewind = new Rewind();

const toolset = await rewind.toolsets.create({ http: { allow: ["api.github.com"] } });
const user = { id: "user_2fQ81" };
const conversation = { id: crypto.randomUUID() };

const agentTools = rewind.namespace(user.id).agentTools({
  toolset: toolset.id,
  labels: { agent: "repo-analyst", conversation: conversation.id },
});

const instructions = [
  "You answer questions about GitHub repositories.",
  "Solve each task by writing one TypeScript script and running it with run_script.",
  "",
  "What scripts can use:",
  toolset.declarations.prompt,
].join("\n");

const question =
  process.argv[2] ?? "Which of nodejs/node, denoland/deno and oven-sh/bun has the most stars per open issue?";
let input: OpenAI.Responses.ResponseInput = [{ role: "user", content: question }];
let previous: string | undefined;

for (;;) {
  const response = await openai.responses.create({
    model: process.env.OPENAI_MODEL ?? "gpt-5",
    instructions,
    tools: agentTools.openai.responses.tools,
    input,
    previous_response_id: previous,
  });

  const calls = response.output.filter((item) => item.type === "function_call");
  if (calls.length === 0) {
    console.log(response.output_text);
    break;
  }
  for (const call of calls) console.log(`→ ${call.name}`);
  // The next request carries the tool results only: the API keeps the conversation.
  input = await Promise.all(calls.map((call) => agentTools.openai.responses.handle(call)));
  previous = response.id;
}
