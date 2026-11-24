// Quickstart: register an action set, serve its harness action, run a workflow and read its result.
//
//   pnpm quickstart
import { readFile } from "node:fs/promises";
import { Rewind } from "@rewind.codes/sdk";

// REWIND_URL and REWIND_API_KEY from the environment.
const rewind = new Rewind();

// 1. An action set: what workflows may call. Here the built-in `http` action, limited to api.github.com, and one
//    harness action. Registering the same action set again returns the same id.
const actionSet = await rewind.actionSets.create({
  http: { allow: ["api.github.com"] },
  harness: [
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
console.log(`action set ${actionSet.id.slice(0, 12)}, declarations for the model:\n${actionSet.declarations.prompt}\n`);

// 2. A worker executes the harness actions when workflows call them.
const worker = rewind.worker({
  actionSet: actionSet.id,
  actions: {
    notify: async ({ text }: { text: string }, ctx) => {
      console.log(`[notify for ${ctx.namespace}] ${text}`);
      return { sent: true };
    },
  },
});

// 3. A run, in the namespace of the end user it acts for. In a harness the model writes the code; here it is a file.
//    Code the namespace has not run before becomes one of its workflows, with this code as its version 1.
const code = await readFile(new URL("./workflows/repo-report.ts", import.meta.url), "utf8");
const run = await rewind.namespace("quickstart-user").runs.start({
  actionSet: actionSet.id,
  code,
  input: { repo: "nodejs/node" },
  wait: "30s",
  labels: { example: "quickstart" },
});

console.log(`run ${run.handle}: ${run.status}, workflow ${run.workflow?.id} v${run.workflow?.version}`);
if (run.status === "completed") console.log(run.output?.value);
if (run.status === "failed") console.log(run.error);

await worker.stop();
