// Workflows: create a named workflow with an interface, then start its runs from a workflow and from the harness.
//
//   pnpm workflows
import { readFile } from "node:fs/promises";
import { Rewind } from "@rewind.codes/sdk";

const rewind = new Rewind();
const read = (name: string) => readFile(new URL(`./workflows/${name}`, import.meta.url), "utf8");

const base = await rewind.actionSets.create({ http: { allow: ["api.github.com"] } });

// 1. Create the workflow, the organization's: an id, a name, and its version 1 with an interface. Creating it again
//    with the same code answers the same workflow.
const repoCheck = await rewind.workflows.create({
  name: "repo_check",
  description: "The stars and open issues of a GitHub repository, once someone acknowledges it.",
  actionSet: base.id,
  code: await read("repo-check.ts"),
  input: { type: "object", properties: { repo: { type: "string" } }, required: ["repo"] },
  output: {
    type: "object",
    properties: {
      repo: { type: "string" },
      stars: { type: "integer" },
      openIssues: { type: "integer" },
      acknowledgedBy: { type: "string" },
    },
    required: ["repo", "stars", "openIssues"],
  },
  signals: {
    acknowledge: { type: "object", properties: { by: { type: "string" } }, required: ["by"] },
  },
});
console.log(`workflow ${repoCheck.name} ${repoCheck.id} v${repoCheck.version.version} (${repoCheck.version.digest.slice(0, 12)})`);

// 2. An action set whose code may start it: derived from the base, which pins its latest version.
const actionSet = await rewind.actionSets.create({ base: base.id, workflows: ["repo_check"] });
console.log(`declarations:\n${actionSet.declarations.prompt}\n`);

// 3. Code that fans out over repositories, one run of the workflow each.
const team = rewind.namespace("team_platform");
const parent = await team.runs.start({
  actionSet: actionSet.id,
  code: await read("check-all.ts"),
  input: { repos: ["nodejs/node", "denoland/deno", "oven-sh/bun"] },
  wait: "60s",
  labels: { example: "workflows" },
});
console.log(`parent ${parent.handle}: ${parent.status}`, parent.output?.value);

const children = await team.runs.list({ startedBy: parent.id });
for (const child of children.runs) {
  console.log(`  started ${child.handle}: ${child.workflow?.name} v${child.workflow?.version} ${child.status}`);
}

// 4. The harness starts a run of the workflow too, by name (its latest version), and signals it.
const direct = await team.runs.start({ workflow: "repo_check", input: { repo: "microsoft/typescript" } });
await team.runs.signal(direct.id, "acknowledge", { by: "the harness" });
const done = await team.runs.get(direct.id, { wait: "30s" });
console.log(`direct ${done.handle}: ${done.status}`, done.output?.value);
