// Testing workflows: type-check code, run tests in virtual time against a world of answers, fix a failed run with a
// fork, and test against the calls a real run made (a cassette).
//
//   pnpm testing
import { readFile } from "node:fs/promises";
import { Rewind, type Test, type TestReport } from "@rewind.codes/sdk";

const rewind = new Rewind();
const read = (name: string) => readFile(new URL(`./workflows/${name}`, import.meta.url), "utf8");
const object = (properties: Record<string, unknown>, required = Object.keys(properties)) => ({
  type: "object",
  properties,
  required,
});

// Three harness actions stand for a GitHub and a Slack server.
const issueSchema = object({
  number: { type: "integer" },
  title: { type: "string" },
  updated_at: { type: "string" },
  labels: { type: "array", items: object({ name: { type: "string" } }) },
});
const actionSet = await rewind.actionSets.create({
  harness: [
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
const show = (report: TestReport) => {
  for (const test of report.tests) {
    console.log(`${test.passed ? "PASS" : "FAIL"} ${test.name}${test.problem ? ` (${test.problem})` : ""}`);
    for (const failure of test.failures) console.log(`  ${failure}`);
  }
};

// 1. Type-check: code that passes a number as `repo` is refused before anything runs. A check creates nothing.
const check = await ci.workflows.check({
  actionSet: actionSet.id,
  code: triage.replace("repo: input.repo, state", "repo: 42, state"),
});
for (const d of check.diagnostics) console.log(`check: ${d.severity} ${d.code} ${d.message}\n${d.frame}\n`);

// 2. Tests, as test runs: one answer per action, signals, expectations. Time is virtual: the 3-day timeout of the
//    second test passes at once. The approval of the first is sent 1 hour (of virtual time) after the start, well
//    before that timeout.
const world = {
  actions: {
    "github.list_issues": { returns: issues },
    "slack.post_message": { returns: { ok: true } },
    "github.close_issue": { returns: { closed: true } },
  },
};
const approved: Test = {
  name: "closes the stale issues once approved",
  input,
  world,
  signals: [{ name: "approval", payload: { close: true }, when: "1h" }],
  expect: {
    status: "completed",
    output: { closed: 2 },
    calls: [
      { action: "github.list_issues", times: 1 },
      { action: "slack.post_message", args: { channel: "#triage" }, times: 1 },
      { action: "github.close_issue", times: 2 },
    ],
  },
};
const ignored: Test = {
  name: "closes nothing without an answer in 3 days",
  input,
  world,
  expect: {
    status: "completed",
    output: { closed: 0 },
    calls: [{ action: "github.close_issue", times: 0 }],
  },
};
show(await ci.workflows.test({ actionSet: actionSet.id, code: triage, tests: [approved, ignored] }));

// 3. A live run of the first version fails on the issue without labels, after it posted to Slack.
const sent: string[] = [];
const worker = rewind.worker({
  actionSet: actionSet.id,
  actions: {
    "github.list_issues": async () => (sent.push("github.list_issues"), issues),
    "slack.post_message": async () => (sent.push("slack.post_message"), { ok: true }),
    "github.close_issue": async ({ number }: { number: number }) => (sent.push(`github.close_issue #${number}`), { closed: true }),
  },
});
const failed = await ci.runs.start({ actionSet: actionSet.id, code: await read("triage-v1.ts"), input, wait: "10s" });
console.log(`\nv1 ${failed.id}: ${failed.status}, ${failed.error?.name}: ${failed.error?.message}`);

// The fork reuses what the failed run recorded while the fixed code makes the same calls, then goes on live: Slack
// does not get the message twice.
let fixed = await ci.runs.fork(failed.id, { code: triage, wait: "10s" });
await ci.runs.signal(fixed.id, "approval", { close: true });
fixed = await ci.runs.get(fixed.id, { wait: "10s" });
console.log(`fork ${fixed.id}: ${fixed.status}`, fixed.output?.value);
console.log("calls the worker executed, both runs together:", sent);
await worker.stop();

// 4. The calls and signals of a real run, as a cassette: a test against what the actions really answered. The
//    cassette holds the approval too, sent when it arrived in the run.
const cassette = await ci.runs.export(fixed.id, { as: "cassette" });
console.log();
show(
  await ci.workflows.test({
    actionSet: actionSet.id,
    code: triage,
    tests: [
      {
        name: `does what ${fixed.handle} did`,
        input,
        world: { cassette },
        expect: { status: "completed", output: { closed: 2 } },
      },
    ],
  }),
);
