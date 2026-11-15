// Testing scripts: type-check them, run scenarios in mock mode and virtual time, fix a failed run
// with a fork, and test against the calls a real run made (a cassette).
//
//   pnpm testing
import { readFile } from "node:fs/promises";
import { Rewind } from "@rewind.codes/sdk";

const rewind = new Rewind();
const read = (name: string) => readFile(new URL(`./scripts/${name}`, import.meta.url), "utf8");
const object = (properties: Record<string, unknown>, required = Object.keys(properties)) => ({
  type: "object",
  properties,
  required,
});

// Three host tools stand for a GitHub and a Slack server.
const issueSchema = object({
  number: { type: "integer" },
  title: { type: "string" },
  updated_at: { type: "string" },
  labels: { type: "array", items: object({ name: { type: "string" } }) },
});
const toolset = await rewind.toolsets.create({
  host: [
    {
      name: "github.list_issues",
      description: "The issues of a repository.",
      inputSchema: object({ repo: { type: "string" }, state: { enum: ["open", "closed"] } }),
      outputSchema: { type: "array", items: issueSchema },
    },
    {
      name: "github.close_issue",
      description: "Close an issue.",
      inputSchema: object({ repo: { type: "string" }, number: { type: "integer" } }),
    },
    {
      name: "slack.post_message",
      description: "Post a message to a channel.",
      inputSchema: object({ channel: { type: "string" }, text: { type: "string" } }),
    },
  ],
});

const ci = rewind.namespace("ci");
const triage = await read("triage.ts");
const input = { repo: "acme/api" };
const issues = [
  { number: 12, title: "Flaky test on Windows", updated_at: "2025-01-10T09:00:00Z", labels: [{ name: "ci" }] },
  { number: 31, title: "Typo in the README", updated_at: "2025-03-02T17:30:00Z", labels: [] },
  { number: 57, title: "Support Node 24", updated_at: "2099-01-01T00:00:00Z", labels: [{ name: "runtime" }] },
];

// 1. Type-check: a script that passes a number as `repo` is refused before anything runs.
const check = await ci.scripts.check({ toolset: toolset.id, script: triage.replace("repo: input.repo, state", "repo: 42, state") });
for (const d of check.diagnostics) console.log(`check: ${d.severity} ${d.code} ${d.message}\n${d.frame}\n`);

// 2. Scenarios, in mock mode: static answers per tool, signals, expectations. Time is virtual:
//    the 3-day timeout of the second scenario passes at once. The approval is sent 1 hour (of
//    virtual time) after the start: a run that waits with a timeout is never "blocked", so a
//    signal sent when blocked (the default) would come after the timeout.
const tools = {
  "github.list_issues": issues,
  "slack.post_message": { ok: true },
  "github.close_issue": { closed: true },
};
const approved = await ci.scripts.test({
  toolset: toolset.id,
  script: triage,
  input,
  tools,
  signals: [{ name: "approval", payload: { close: true }, when: "1h" }],
  expect: {
    status: "completed",
    output: { closed: 2 },
    calls: { "github.list_issues": 1, "slack.post_message": 1, "github.close_issue": 2 },
  },
});
const ignored = await ci.scripts.test({
  toolset: toolset.id,
  script: triage,
  input,
  tools,
  expect: {
    status: "completed",
    output: { closed: 0 },
    calls: { "github.list_issues": 1, "slack.post_message": 1, "github.close_issue": 0 },
  },
});
console.log(`scenario "approved": ${approved.passed ? "passed" : approved.failures.join("; ")}`);
console.log(`scenario "no answer in 3 days": ${ignored.passed ? "passed" : ignored.failures.join("; ")}`);

// 3. A live run of the first version fails on the issue without labels, after it posted to Slack.
const sent: string[] = [];
const worker = rewind.worker({
  toolset: toolset.id,
  tools: {
    "github.list_issues": async () => (sent.push("github.list_issues"), issues),
    "slack.post_message": async () => (sent.push("slack.post_message"), { ok: true }),
    "github.close_issue": async ({ number }: { number: number }) => (sent.push(`github.close_issue #${number}`), { closed: true }),
  },
});
const failed = await ci.runs.start({ toolset: toolset.id, script: await read("triage-v1.ts"), input, wait: "10s" });
console.log(`\nv1 ${failed.id}: ${failed.status}, ${failed.error?.name}: ${failed.error?.message}`);

// The fork reuses what the failed run recorded while the fixed script makes the same calls, then
// goes on live: Slack does not get the message twice.
let fixed = await ci.runs.fork(failed.id, { script: triage, wait: "10s" });
await ci.runs.signal(fixed.id, "approval", { close: true });
fixed = await ci.runs.get(fixed.id, { wait: "10s" });
console.log(`fork ${fixed.id}: ${fixed.status}`, fixed.output?.value);
console.log("calls the worker executed, both runs together:", sent);
await worker.stop();

// 4. The calls and signals of a real run, as a cassette: a test against what the tools really
//    answered. The cassette holds the approval too.
const cassette = await ci.runs.journal(fixed.id, { as: "cassette" });
const replayed = await ci.scripts.test({
  toolset: toolset.id,
  script: triage,
  input,
  cassette,
  expect: { status: "completed", output: { closed: 2 } },
});
console.log(`\ncassette of ${fixed.id}: ${replayed.passed ? "passed" : replayed.failures.join("; ")}`);
