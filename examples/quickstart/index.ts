// Quickstart: register a toolset, serve its host tool, run a script and read its result.
//
//   pnpm quickstart
import { readFile } from "node:fs/promises";
import { Rewind } from "@rewind.codes/sdk";

// REWIND_URL and REWIND_API_KEY from the environment.
const rewind = new Rewind();

// 1. A toolset: what scripts may call. Here the built-in `http` tool, limited to api.github.com,
//    and one host tool of the harness. Registering the same toolset again returns the same id.
const toolset = await rewind.toolsets.create({
  http: { allow: ["api.github.com"] },
  host: [
    {
      name: "notify",
      description: "Send a short message to the user.",
      inputSchema: {
        type: "object",
        properties: { text: { type: "string" } },
        required: ["text"],
      },
    },
  ],
});
console.log(`toolset ${toolset.id.slice(0, 12)}, declarations for the model:\n${toolset.declarations.prompt}\n`);

// 2. A worker executes the host tools when scripts call them.
const worker = rewind.worker({
  toolset: toolset.id,
  tools: {
    notify: async ({ text }: { text: string }, ctx) => {
      console.log(`[notify for ${ctx.namespace}] ${text}`);
      return { sent: true };
    },
  },
});

// 3. A run, in the namespace of the end user it acts for. In a harness the model writes the
//    script; here it is a file.
const script = await readFile(new URL("./scripts/repo-report.ts", import.meta.url), "utf8");
const run = await rewind.namespace("quickstart-user").runs.start({
  toolset: toolset.id,
  script,
  input: { repo: "nodejs/node" },
  wait: "30s",
  labels: { example: "quickstart" },
});

console.log(`run ${run.id}: ${run.status}`);
if (run.status === "completed") console.log(run.output?.value);
if (run.status === "failed") console.log(run.error);

await worker.stop();
