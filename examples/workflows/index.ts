// Workflows: register a named, typed script, then start its runs from a script and from the harness.
//
//   pnpm workflows
import { readFile } from "node:fs/promises";
import { Rewind } from "@rewind.codes/sdk";

const rewind = new Rewind();
const read = (name: string) => readFile(new URL(`./scripts/${name}`, import.meta.url), "utf8");

const base = await rewind.toolsets.create({ http: { allow: ["api.github.com"] } });

// 1. Register the workflow: each save is a new immutable version, identified by its digest.
const repoCheck = await rewind.workflows.create({
  name: "repo_check",
  description: "The stars and open issues of a GitHub repository, once someone acknowledges it.",
  toolset: base.id,
  script: await read("repo-check.ts"),
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
console.log(`workflow repo_check@${repoCheck.version.slice(0, 12)}`);

// 2. A toolset whose scripts may start it: derived from the base, which pins this version.
const toolset = await rewind.toolsets.create({ base: base.id, workflows: ["repo_check"] });
console.log(`declarations:\n${toolset.declarations.prompt}\n`);

// 3. A script fans out over repositories, one run of the workflow each.
const team = rewind.namespace("team_platform");
const parent = await team.runs.start({
  toolset: toolset.id,
  script: await read("check-all.ts"),
  input: { repos: ["nodejs/node", "denoland/deno", "oven-sh/bun"] },
  wait: "60s",
  labels: { example: "workflows" },
});
console.log(`parent ${parent.id}: ${parent.status}`, parent.output?.value);

const children = await team.runs.list({ startedBy: parent.id });
for (const child of children.runs) console.log(`  started ${child.id}: ${child.workflow} ${child.status}`);

// 4. The harness starts a run of the workflow too, by name (its latest version), and signals it.
const direct = await team.runs.start({ workflow: "repo_check", input: { repo: "microsoft/typescript" } });
await team.runs.signal(direct.id, "acknowledge", { by: "the harness" });
const done = await team.runs.get(direct.id, { wait: "30s" });
console.log(`direct ${done.id}: ${done.status}`, done.output?.value);
