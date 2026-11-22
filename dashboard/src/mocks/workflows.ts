// The workflows of the mocked world, their versions and their suites (archi/05, "Suites and the
// gate"): triage has three versions, whose suites inherit, add, update and retire tests with
// reasons, the last one adding two tests recorded from its runs; digest has one, with TypeScript
// mock handlers. One recorded test comes from a namespace the caller cannot read.
import type { Labels, Test, Workflow } from "../model/types";
import { fakeDigest } from "./journal";

const HOUR = 3_600_000;
const DAY = 24 * HOUR;

export interface TestFixture {
  test: Test;
  /** A recorded test whose namespace's runs the caller cannot read: 403. */
  restricted?: boolean;
}

export interface WorkflowWorld {
  /** Every version of every workflow, newest first within a name. */
  versions: Workflow[];
  /** The stored tests, by digest. */
  tests: Map<string, TestFixture>;
}

/** A run of the workflow `triage` a test is recorded from. */
export interface RecordedRun {
  id: string;
  namespace: string;
  start: number;
  repo: string;
  /** The issues it closed. */
  closed: number;
  /** Its namespace is beyond every caller's grant: its test answers 403. */
  restricted?: boolean;
}

export interface WorkflowOptions {
  now: number;
  /** The versions the world's runs execute: triage's second, digest's only one. */
  triageVersion: string;
  digestVersion: string;
  /** The scripts of the versions. */
  triageScript: string;
  digestScript: string;
  triageToolset: string;
  digestToolset: string;
  /** The runs of triage the third version records tests from. */
  recorded: RecordedRun[];
}

function iso(ms: number): string {
  return new Date(ms).toISOString();
}

const TRIAGE_INPUT = {
  type: "object",
  properties: { repo: { type: "string" } },
  required: ["repo"],
};
const TRIAGE_OUTPUT = {
  type: "object",
  properties: { closed: { type: "integer" } },
  required: ["closed"],
};
const APPROVAL = {
  type: "object",
  properties: { close: { type: "boolean" } },
  required: ["close"],
};

function triageDeclarations(description: string) {
  return {
    dts: 'declare module "rewind:workflows" {\n  export const triage: Workflow<{ repo: string }, { closed: number }, { approval: { close: boolean } }>;\n}',
    prompt: `triage.start({ repo: string }) → run · run.signal("approval", { close: boolean }) · run.result() → { closed: number } · ${description}`,
  };
}

const STALE = { number: 4, updated_at: "2025-10-01T00:00:00Z" };
const FRESH = { number: 7, updated_at: "2025-12-31T00:00:00Z" };

const NO_APPROVAL: Test = {
  name: "closes nothing without approval",
  input: { repo: "acme/api" },
  world: {
    tools: {
      "github.list_issues": { args: { state: "open" }, returns: [STALE] },
      "github.close_issue": { returns: {} },
    },
  },
  signals: [{ name: "approval", payload: { close: false } }],
  expect: {
    status: "completed",
    output: { closed: 0 },
    calls: [{ tool: "github.close_issue", times: 0 }],
  },
};

const CLOSES_ALL: Test = {
  name: "closes the open issues once approved",
  input: { repo: "acme/api" },
  world: {
    tools: {
      "github.list_issues": { returns: [STALE, FRESH] },
      "github.close_issue": { returns: {} },
    },
  },
  signals: [{ name: "approval", payload: { close: true } }],
  expect: { status: "completed", output: { closed: 2 } },
};

const CLOSES_STALE: Test = {
  name: "closes the open issues once approved",
  input: { repo: "acme/api" },
  start: "2026-01-01T00:00:00Z",
  world: {
    tools: {
      "github.list_issues": [
        { args: { state: "open" }, returns: [STALE, FRESH], times: 1 },
        { error: { message: "rate limited", kind: "unavailable" } },
      ],
      "github.close_issue": { args: { number: 4 }, returns: {} },
    },
  },
  signals: [
    { name: "approval", payload: { close: true } },
    { name: "approval", payload: { close: true }, when: "2h" },
  ],
  expect: {
    status: "completed",
    output: { closed: 1 },
    calls: [
      { tool: "github.list_issues", args: { state: "open" }, times: 1 },
      { tool: "github.close_issue", args: { number: 4 }, times: 1 },
      { tool: "github.close_issue", args: { number: 7 }, times: 0 },
    ],
    order: "listed",
  },
};

const LISTS_CLOSED: Test = {
  name: "lists the closed issues too",
  input: { repo: "acme/api" },
  world: { tools: { "github.list_issues": { returns: [] } } },
  signals: [{ name: "approval", payload: { close: false } }],
  expect: {
    status: "completed",
    calls: [{ tool: "github.list_issues", args: { state: "closed" }, times: 1 }],
  },
};

const DIGEST_TEST: Test = {
  name: "posts the summary of the merged pull requests",
  input: { repos: ["acme/api"], channel: "#eng" },
  world: {
    tools: {
      "slack.post_message": { returns: { ok: true, ts: "1731390000.000300" } },
      "llm.complete": { returns: { text: "Faster CI this week." } },
    },
    mocks: `// Answers that compute: each repository has one merged pull request.
export default {
  github: {
    list_pull_requests: ({ repo }) => [
      { number: 31, title: \`Faster CI in \${repo}\`, merged_at: "2026-01-01T09:00:00Z" },
    ],
  },
};
`,
  },
  expect: {
    status: "completed",
    output: { prs: 1 },
    calls: [
      { tool: "llm.complete", times: 1 },
      { tool: "slack.post_message", args: { channel: "#eng" }, times: 1 },
    ],
    order: "listed",
  },
};

/** The test a recording takes from a run: its cassette, its outcome, its origin. */
function recordedTest(run: RecordedRun, version: string): Test {
  const calls = [
    {
      tool: "github.list_issues",
      args: { repo: run.repo, state: "open" },
      outcome: {
        result: Array.from({ length: 4 }, (_, i) => ({
          number: 4100 + i,
          updated_at: "2025-10-01T00:00:00Z",
        })),
      },
    },
    ...Array.from({ length: run.closed }, (_, k) => ({
      tool: "github.close_issue",
      args: { repo: run.repo, number: 4100 + 3 * k },
      outcome: { result: { state: "closed" } },
    })),
  ];
  return {
    name: `recorded ${run.id}`,
    input: { repo: run.repo },
    start: iso(run.start),
    seed: fakeDigest(`seed:${run.id}`),
    world: {
      cassette: {
        v: 1,
        matching: "by_args",
        calls,
        signals: [{ name: "approval", payload: { close: true }, elapsed_ms: 330_000 }],
      },
    },
    expect: {
      status: "completed",
      output: { closed: run.closed },
      calls: [
        { tool: "github.list_issues", times: 1 },
        { tool: "github.close_issue", times: run.closed },
      ],
    },
    origin: { run: run.id, version, namespace: run.namespace },
  };
}

/** A test's digest: of its canonical JSON on the server; of its JSON here. */
export function testDigest(test: Test): string {
  return fakeDigest(`test:${JSON.stringify(test)}`);
}

export function createWorkflowWorld(options: WorkflowOptions): WorkflowWorld {
  const { now } = options;
  const tests = new Map<string, TestFixture>();
  const store = (test: Test, restricted = false) => {
    const digest = testDigest(test);
    tests.set(digest, restricted ? { test, restricted } : { test });
    return { name: test.name, digest };
  };
  const labels: Labels = { agent: "triage" };
  const description = "Triage the stale issues of a repository, after approval.";
  const base = {
    name: "triage",
    description,
    toolset: options.triageToolset,
    script: options.triageScript,
    input: TRIAGE_INPUT,
    output: TRIAGE_OUTPUT,
    signals: { approval: APPROVAL },
    labels,
    declarations: triageDeclarations(description),
  };

  const noApproval = store(NO_APPROVAL);
  const closesAll = store(CLOSES_ALL);
  const closesStale = store(CLOSES_STALE);
  const listsClosed = store(LISTS_CLOSED);

  const v1: Workflow = {
    ...base,
    version: fakeDigest("workflow:triage:v1"),
    createdAt: iso(now - 12 * DAY),
    tests: [noApproval, closesAll, listsClosed],
  };
  const v2: Workflow = {
    ...base,
    version: options.triageVersion,
    createdAt: iso(now - 6 * DAY),
    parent: v1.version,
    tests: [noApproval, closesStale],
    updated: [
      {
        name: CLOSES_ALL.name,
        from: closesAll.digest,
        to: closesStale.digest,
        reason:
          "only the issues untouched for 30 days are stale: the issue updated yesterday stays open",
      },
    ],
    retired: [
      {
        name: LISTS_CLOSED.name,
        digest: listsClosed.digest,
        reason: "the workflow never reads closed issues any more: deprecated with the state filter",
      },
    ],
  };
  const recorded = options.recorded.map((run) =>
    store(recordedTest(run, options.triageVersion), run.restricted === true),
  );
  const v3: Workflow = {
    ...base,
    version: fakeDigest("workflow:triage:v3"),
    createdAt: iso(now - 3 * HOUR),
    parent: v2.version,
    tests: [noApproval, closesStale, ...recorded],
  };

  const digestTest = store(DIGEST_TEST);
  const digest: Workflow = {
    name: "digest",
    version: options.digestVersion,
    description: "Post a weekly digest of the merged pull requests to a channel.",
    toolset: options.digestToolset,
    script: options.digestScript,
    input: {
      type: "object",
      properties: {
        repos: { type: "array", items: { type: "string" } },
        channel: { type: "string" },
      },
      required: ["repos", "channel"],
    },
    output: { type: "object", properties: { prs: { type: "integer" }, ts: { type: "string" } } },
    signals: {},
    labels: { agent: "digest" },
    createdAt: iso(now - 9 * DAY),
    declarations: {
      dts: 'declare module "rewind:workflows" {\n  export const digest: Workflow<{ repos: string[]; channel: string }, { prs?: number; ts?: string }, {}>;\n}',
      prompt:
        "digest.start({ repos: string[], channel: string }) → run · run.result() → { prs?: number, ts?: string } · Post a weekly digest of the merged pull requests to a channel.",
    },
    tests: [digestTest],
  };

  return { versions: [digest, v3, v2, v1], tests };
}
