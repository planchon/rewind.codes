// The workflows of the mocked world (archi/01, "Workflows"): each one entity, with an id the API
// assigns, an optional name unique in its scope (a namespace's, or the organization's), versions
// numbered from 1 and a suite (archi/05, "Suites and the gate"). The organization's `triage` has
// three versions, whose suites inherit, add, replace and retire tests with reasons, the last one
// adding two tests recorded from its runs (one from a namespace no caller may read); `digest` has
// one, with TypeScript handlers; dana forked `triage` at its second version. The other workflows
// were made by the first run of their code in a namespace: a few were named, two promoted to the
// organization, the others show by their code's first comment line or their short id.
import type {
  ActionRules,
  CodeComment,
  Labels,
  ReplacedTest,
  RetiredTest,
  RunWorkflow,
  SuiteEntry,
  Test,
  Workflow,
  WorkflowSource,
  WorkflowVersion,
} from "../model/types";
import { fakeDigest } from "./history";

const HOUR = 3_600_000;
const DAY = 24 * HOUR;

export interface TestFixture {
  test: Test;
  /** A recorded test whose namespace's runs the caller cannot read: 403. */
  restricted?: boolean;
}

/** A workflow as the mocked catalog keeps it. */
export interface MockWorkflow {
  id: string;
  name: string | null;
  description: string | null;
  /** The namespace it belongs to; `null` for the organization's. */
  namespace: string | null;
  createdAt: number;
  promotedAt: number | null;
  /** A fork's source: the organization's workflow and the version it last took. */
  source: WorkflowSource | null;
  labels: Labels;
  /** Its versions, oldest first: `versions[n - 1]` is version `n`. */
  versions: WorkflowVersion[];
  /** Every test its suite holds or held. */
  suite: SuiteEntry[];
}

export interface WorkflowWorld {
  workflows: MockWorkflow[];
  /** The stored tests, by digest. */
  tests: Map<string, TestFixture>;
  /** The source of each code, by its digest. */
  codes: Map<string, string>;
}

/** A code: its digest and its TypeScript source. */
export interface Code {
  digest: string;
  source: string;
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

/** The codes of the world's workflows, their action sets and the secrets their runs use. */
export interface WorkflowOptions {
  now: number;
  codes: {
    triageV1: Code;
    triageV2: Code;
    triageV3: Code;
    digest: Code;
    forecast: Code;
    onboarding: Code;
    staleIssues: Code;
    staleIssuesFixed: Code;
    devNotifications: Code;
    triageAll: Code;
    triageWorkflow: Code;
  };
  actionSets: {
    github: string;
    digest: string;
    crm: string;
    weather: string;
    workflows: string;
    dax: string;
  };
  /** The secrets the runs of a code use, by server. */
  secretsOf: (code: Code) => Record<string, string>;
  /** The runs of triage the third version records tests from. */
  recorded: RecordedRun[];
}

function iso(ms: number): string {
  return new Date(ms).toISOString();
}

const CROCKFORD = "0123456789abcdefghjkmnpqrstvwxyz";

/** `wf_` and 12 characters of Crockford's base32 in lower case, drawn from `key`. */
export function workflowId(key: string): string {
  const hex = fakeDigest(`workflow:${key}`);
  let out = "";
  for (let i = 0; i < 12; i += 1)
    out += CROCKFORD[Number.parseInt(hex.slice(2 * i, 2 * i + 2), 16) % 32];
  return `wf_${out}`;
}

/** `wf-7k2m9q`: `wf-` and the first 6 characters after `wf_`. */
export function shortIdOf(id: string): string {
  return `wf-${id.slice(3, 9)}`;
}

/** The comment at the top of a code: its first line the title, the rest the description. */
export function codeComment(source: string): CodeComment | null {
  const rest = source.trimStart();
  let lines: string[];
  if (rest.startsWith("/*")) {
    const end = rest.indexOf("*/");
    if (end < 0) return null;
    lines = rest
      .slice(2, end)
      .replace(/^\*+/, "")
      .split("\n")
      .map((l) =>
        l
          .trim()
          .replace(/^\*\s?/, "")
          .trim(),
      );
  } else if (rest.startsWith("//")) {
    lines = [];
    for (const line of rest.split("\n")) {
      const t = line.trim();
      if (!t.startsWith("//")) break;
      lines.push(t.replace(/^\/+/, "").trim());
    }
  } else {
    return null;
  }
  lines = lines.filter((l) => !l.startsWith("@"));
  const first = lines.findIndex((l) => l !== "");
  if (first < 0) return null;
  const title = lines[first] ?? "";
  const description = lines
    .slice(first + 1)
    .join("\n")
    .split(/\n\s*\n/)
    .map((p) => p.split("\n").join(" ").trim())
    .filter(Boolean)
    .join("\n\n");
  return description ? { title, description } : { title };
}

/** Its latest version. */
export function latestVersion(w: MockWorkflow): WorkflowVersion {
  return w.versions[w.versions.length - 1] as WorkflowVersion;
}

/** How a workflow shows: its name, its latest code's first comment line, or its short id. */
export function workflowLabelOf(w: MockWorkflow | undefined): string {
  if (!w) return "";
  return w.name ?? latestVersion(w).comment?.title ?? shortIdOf(w.id);
}

/** The workflow a run of `w` at `version` names. */
export function runWorkflowOf(w: MockWorkflow, version: WorkflowVersion): RunWorkflow {
  return {
    id: w.id,
    ...(w.name ? { name: w.name } : {}),
    version: version.version,
    digest: version.digest,
  };
}

/** A version's digest: of its document on the server; of what makes it here. */
function versionDigest(workflow: string, n: number, code: string): string {
  return fakeDigest(`version:${workflow}:${n}:${code}`);
}

/**
 * The workflow a run given `code` in `namespace` is a run of (archi/01): its namespace's that has
 * a version with that code, else the organization's; at its latest version with that code.
 */
export function resolveByCode(
  world: WorkflowWorld,
  namespace: string,
  code: string,
): { workflow: MockWorkflow; version: WorkflowVersion } | null {
  for (const scope of [namespace, null]) {
    for (const w of world.workflows) {
      if (w.namespace !== scope) continue;
      const version = [...w.versions].reverse().find((v) => v.codeDigest === code);
      if (version) return { workflow: w, version };
    }
  }
  return null;
}

/** The versions the caller sees, newest first: those carrying its grant's labels. */
export function visibleVersions(w: MockWorkflow, labels: Labels): WorkflowVersion[] {
  const pairs = Object.entries(labels);
  return [...w.versions].reverse().filter((v) => pairs.every(([k, val]) => v.labels[k] === val));
}

/** A workflow as `GET /v1/workflows/{id}` answers it, at `version`. */
export function workflowDto(
  world: WorkflowWorld,
  w: MockWorkflow,
  version: WorkflowVersion,
  labels: Labels = {},
): Workflow {
  return {
    id: w.id,
    shortId: shortIdOf(w.id),
    ...(w.name ? { name: w.name } : {}),
    ...(w.description ? { description: w.description } : {}),
    ...(w.namespace ? { namespace: w.namespace } : {}),
    ...(w.promotedAt !== null ? { promotedAt: iso(w.promotedAt) } : {}),
    ...(w.source ? { source: w.source } : {}),
    createdAt: iso(w.createdAt),
    labels: w.labels,
    code: world.codes.get(version.codeDigest) ?? "",
    version,
    versions: visibleVersions(w, labels),
    suite: w.suite,
  };
}

// ---------------------------------------------------------------------------------------------
// Tests

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
    actions: {
      "github.list_issues": { args: { state: "open" }, returns: [STALE] },
      "github.close_issue": { returns: {} },
    },
  },
  signals: [{ name: "approval", payload: { close: false } }],
  expect: {
    status: "completed",
    output: { closed: 0 },
    calls: [{ action: "github.close_issue", times: 0 }],
  },
};

const CLOSES_ALL: Test = {
  name: "closes the open issues once approved",
  input: { repo: "acme/api" },
  world: {
    actions: {
      "github.list_issues": { returns: [STALE, FRESH] },
      "github.close_issue": { returns: {} },
    },
  },
  signals: [{ name: "approval", payload: { close: true } }],
  expect: { status: "completed", output: { closed: 2 } },
};

const STALE_RULES: ActionRules = [
  { args: { state: "open" }, returns: [STALE, FRESH], times: 1 },
  { error: { message: "rate limited", kind: "unavailable" } },
];

const CLOSES_STALE: Test = {
  name: "closes the open issues once approved",
  input: { repo: "acme/api" },
  start: "2026-01-01T00:00:00Z",
  world: {
    actions: {
      "github.list_issues": STALE_RULES,
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
      { action: "github.list_issues", args: { state: "open" }, times: 1 },
      { action: "github.close_issue", args: { number: 4 }, times: 1 },
      { action: "github.close_issue", args: { number: 7 }, times: 0 },
    ],
    order: "listed",
  },
};

const LISTS_CLOSED: Test = {
  name: "lists the closed issues too",
  input: { repo: "acme/api" },
  world: { actions: { "github.list_issues": { returns: [] } } },
  signals: [{ name: "approval", payload: { close: false } }],
  expect: {
    status: "completed",
    calls: [{ action: "github.list_issues", args: { state: "closed" }, times: 1 }],
  },
};

const DIGEST_TEST: Test = {
  name: "posts the summary of the merged pull requests",
  input: { repos: ["acme/api"], channel: "#eng" },
  world: {
    actions: {
      "slack.post_message": { returns: { ok: true, ts: "1731390000.000300" } },
      "llm.complete": { returns: { text: "Faster CI this week." } },
    },
    handlers: `// Answers that compute: each repository has one merged pull request.
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
      { action: "llm.complete", times: 1 },
      { action: "slack.post_message", args: { channel: "#eng" }, times: 1 },
    ],
    order: "listed",
  },
};

/** The test a recording takes from a run: its cassette, its outcome, the run it comes from. */
function recordedTest(run: RecordedRun, version: string): Test {
  const calls = [
    {
      action: "github.list_issues",
      args: { repo: run.repo, state: "open" },
      outcome: {
        result: Array.from({ length: 4 }, (_, i) => ({
          number: 4100 + i,
          updated_at: "2025-10-01T00:00:00Z",
        })),
      },
    },
    ...Array.from({ length: run.closed }, (_, k) => ({
      action: "github.close_issue",
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
        { action: "github.list_issues", times: 1 },
        { action: "github.close_issue", times: run.closed },
      ],
    },
    from: { run: run.id, version, namespace: run.namespace },
  };
}

/** A test's digest: of its canonical JSON on the server; of its JSON here. */
export function testDigest(test: Test): string {
  return fakeDigest(`test:${JSON.stringify(test)}`);
}

// ---------------------------------------------------------------------------------------------
// The world

export function createWorkflowWorld(options: WorkflowOptions): WorkflowWorld {
  const { now, codes: c, actionSets } = options;
  const world: WorkflowWorld = { workflows: [], tests: new Map(), codes: new Map() };
  for (const code of Object.values(c)) world.codes.set(code.digest, code.source);

  const store = (test: Test, restricted = false) => {
    const digest = testDigest(test);
    world.tests.set(digest, restricted ? { test, restricted } : { test });
    return { name: test.name, digest };
  };

  /** A workflow, its versions numbered and their digests drawn from its id. */
  const workflow = (
    key: string,
    w: Omit<MockWorkflow, "id" | "versions" | "suite">,
    versions: (Omit<WorkflowVersion, "version" | "digest" | "base" | "comment"> & {
      code: Code;
    })[],
  ): MockWorkflow => {
    const id = workflowId(key);
    const made: WorkflowVersion[] = [];
    versions.forEach(({ code, ...v }, i) => {
      const comment = codeComment(code.source);
      const base = made[i - 1]?.digest;
      made.push({
        ...v,
        version: i + 1,
        digest: versionDigest(id, i + 1, code.digest),
        ...(base ? { base } : {}),
        ...(comment ? { comment } : {}),
      });
    });
    const mock: MockWorkflow = { ...w, id, versions: made, suite: [] };
    world.workflows.push(mock);
    return mock;
  };

  /** A workflow a run made: its code and the run's action set and secrets, no interface. */
  const madeByRun = (
    key: string,
    w: Partial<Omit<MockWorkflow, "id" | "versions" | "suite">> & { namespace: string | null },
    actionSet: string,
    labels: Labels,
    at: number,
    ...versionCodes: Code[]
  ): MockWorkflow =>
    workflow(
      key,
      {
        name: null,
        description: null,
        promotedAt: null,
        source: null,
        createdAt: at,
        labels,
        ...w,
      },
      versionCodes.map((code, i) => ({
        code,
        codeDigest: code.digest,
        actionSet,
        labels,
        createdAt: iso(at + i * DAY),
        secrets: options.secretsOf(code),
      })),
    );

  // triage: the organization's, made in alice's namespace by a run, then saved with an interface
  // and a suite, and promoted.
  const noApproval = store(NO_APPROVAL);
  const closesAll = store(CLOSES_ALL);
  const closesStale = store(CLOSES_STALE);
  const listsClosed = store(LISTS_CLOSED);
  const triageLabels: Labels = { agent: "triage" };
  const description = "Triage the stale issues of a repository, after approval.";
  const triageVersion = {
    actionSet: actionSets.github,
    input: TRIAGE_INPUT,
    output: TRIAGE_OUTPUT,
    signals: { approval: APPROVAL },
    labels: triageLabels,
    // Each end user's GitHub, the organization's Slack.
    secrets: { github: "ns/github", slack: "org/slack" },
    declarations: triageDeclarations(description),
  };
  const v1At = now - 12 * DAY;
  const v2At = now - 6 * DAY;
  const v3At = now - 3 * HOUR;
  const triage = workflow(
    "triage",
    {
      name: "triage",
      description,
      namespace: null,
      createdAt: v1At,
      promotedAt: now - 10 * DAY,
      source: null,
      labels: triageLabels,
    },
    [
      {
        ...triageVersion,
        code: c.triageV1,
        codeDigest: c.triageV1.digest,
        createdAt: iso(v1At),
        tests: [noApproval, closesAll, listsClosed],
      },
      {
        ...triageVersion,
        code: c.triageV2,
        codeDigest: c.triageV2.digest,
        createdAt: iso(v2At),
        tests: [noApproval, closesStale],
      },
      {
        ...triageVersion,
        code: c.triageV3,
        codeDigest: c.triageV3.digest,
        createdAt: iso(v3At),
        tests: [noApproval, closesStale],
      },
    ],
  );
  const [t1, t2, t3] = triage.versions as [WorkflowVersion, WorkflowVersion, WorkflowVersion];
  const replaced: ReplacedTest = {
    name: CLOSES_ALL.name,
    old: closesAll.digest,
    new: closesStale.digest,
    reason:
      "only the issues untouched for 30 days are stale: the issue updated yesterday stays open",
  };
  const retired: RetiredTest = {
    name: LISTS_CLOSED.name,
    digest: listsClosed.digest,
    reason: "the workflow never reads closed issues any more: deprecated with the state filter",
  };
  t2.replaced = [replaced];
  t2.retired = [retired];
  // The third version records tests from two runs of the second.
  const recorded = options.recorded.map((run) =>
    store(recordedTest(run, t2.digest), run.restricted === true),
  );
  t3.tests = [noApproval, closesStale, ...recorded];
  triage.suite = [
    { ...noApproval, addedAt: iso(v1At) },
    {
      ...closesAll,
      addedAt: iso(v1At),
      retiredAt: iso(v2At),
      reason: `replaced: ${replaced.reason}`,
    },
    { ...listsClosed, addedAt: iso(v1At), retiredAt: iso(v2At), reason: retired.reason },
    { ...closesStale, addedAt: iso(v2At) },
    ...recorded.map((t) => ({ ...t, addedAt: iso(v3At) })),
  ];
  void t1;

  // digest: the organization's, saved with handlers in its suite.
  const digestTest = store(DIGEST_TEST);
  const digestDescription = "Post a weekly digest of the merged pull requests to a channel.";
  const digestAt = now - 9 * DAY;
  const digest = workflow(
    "digest",
    {
      name: "digest",
      description: digestDescription,
      namespace: null,
      createdAt: digestAt,
      promotedAt: null,
      source: null,
      labels: { agent: "digest" },
    },
    [
      {
        code: c.digest,
        codeDigest: c.digest.digest,
        actionSet: actionSets.digest,
        input: {
          type: "object",
          properties: {
            repos: { type: "array", items: { type: "string" } },
            channel: { type: "string" },
          },
          required: ["repos", "channel"],
        },
        output: {
          type: "object",
          properties: { prs: { type: "integer" }, ts: { type: "string" } },
        },
        signals: {},
        labels: { agent: "digest" },
        secrets: { github: "org/github", slack: "org/slack" },
        createdAt: iso(digestAt),
        declarations: {
          dts: 'declare module "rewind:workflows" {\n  export const digest: Workflow<{ repos: string[]; channel: string }, { prs?: number; ts?: string }, {}>;\n}',
          prompt: `digest.start({ repos: string[], channel: string }) → run · run.result() → { prs?: number, ts?: string } · ${digestDescription}`,
        },
        tests: [digestTest],
      },
    ],
  );
  digest.suite = [{ ...digestTest, addedAt: iso(digestAt) }];

  // dana's fork of triage, at its second version: its suite copied, without the tests recorded
  // from other namespaces' runs. The source has a third version to pull.
  const forkAt = now - 4 * DAY;
  forkWorkflow(world, triage, t2, "dana", forkAt, workflowId("triage@dana"));

  // The forecast: made by a run of bob, promoted to the organization without a name: it shows
  // by its code's first comment line.
  madeByRun(
    "forecast",
    { namespace: null, promotedAt: now - 8 * DAY },
    actionSets.weather,
    {},
    now - 9 * DAY,
    c.forecast,
  );

  // The onboarding: made by a run of bob, promoted without a name or a comment: its short id.
  madeByRun(
    "onboarding",
    { namespace: null, promotedAt: now - 5 * DAY },
    actionSets.crm,
    {},
    now - 7 * DAY,
    c.onboarding,
  );

  // chen's daily digest: the same code as `digest`, run in chen's namespace before `digest` was
  // saved: chen's runs keep running chen's workflow, which has no name and no comment.
  madeByRun("digest@chen", { namespace: "chen" }, actionSets.digest, {}, now - 13 * DAY, c.digest);

  // alice's stale issues, named and described, its fixed code a second version.
  madeByRun(
    "triage_stale_issues",
    {
      namespace: "alice",
      name: "triage_stale_issues",
      description: "Posts the stale issues of a repository, then closes them once approved.",
    },
    actionSets.github,
    triageLabels,
    now - 15 * DAY,
    c.staleIssues,
    c.staleIssuesFixed,
  );

  // The run that starts runs of triage and digest: alice's, unnamed.
  madeByRun(
    "triage_all@alice",
    { namespace: "alice" },
    actionSets.workflows,
    triageLabels,
    now - 6 * HOUR,
    c.triageAll,
  );

  // The e2e corpus's triage_workflow, run in a test: local's, unnamed.
  madeByRun(
    "triage_workflow@local",
    { namespace: "local" },
    actionSets.github,
    {},
    now - 13 * DAY,
    c.triageWorkflow,
  );

  // Dax's notifications, named in its end user's namespace.
  madeByRun(
    "dev_notifications",
    { namespace: "6afc64fd2de4a3f3f4c9e127", name: "dev_notifications" },
    actionSets.dax,
    {},
    now - 20 * DAY,
    c.devNotifications,
  );

  return world;
}

/**
 * Forks an organization's workflow at `version` into `namespace`: the same name and description,
 * `source` the workflow and the version, its version 1 that version, and a copy of its suite
 * without the tests recorded from another namespace's runs.
 */
export function forkWorkflow(
  world: WorkflowWorld,
  source: MockWorkflow,
  version: WorkflowVersion,
  namespace: string,
  at: number,
  id: string,
): MockWorkflow {
  const kept = (version.tests ?? []).filter((t) => {
    const from = world.tests.get(t.digest)?.test.from;
    return !from || from.namespace === namespace;
  });
  const { replaced: _r, retired: _t, base: _b, ...copy } = version;
  const fork: MockWorkflow = {
    id,
    name: source.name,
    description: source.description,
    namespace,
    createdAt: at,
    promotedAt: null,
    source: { workflow: source.id, version: version.version },
    labels: source.labels,
    versions: [
      {
        ...copy,
        version: 1,
        digest: versionDigest(id, 1, version.codeDigest),
        createdAt: iso(at),
        tests: kept,
      },
    ],
    suite: kept.map((t) => ({ ...t, addedAt: iso(at) })),
  };
  world.workflows.push(fork);
  return fork;
}

/**
 * Takes the source's latest version into a fork, as its next version: its code, interface,
 * action set and secrets, with the fork's own suite. `false` when there is nothing to pull.
 */
export function pullSource(world: WorkflowWorld, fork: MockWorkflow, at: number): boolean {
  const source = world.workflows.find((w) => w.id === fork.source?.workflow);
  if (!source || !fork.source) return false;
  const latest = latestVersion(source);
  if (latest.version <= fork.source.version) return false;
  const mine = latestVersion(fork);
  const n = mine.version + 1;
  const { replaced: _r, retired: _t, base: _b, tests: _s, ...copy } = latest;
  fork.versions.push({
    ...copy,
    version: n,
    digest: versionDigest(fork.id, n, latest.codeDigest),
    base: mine.digest,
    createdAt: iso(at),
    ...(mine.tests ? { tests: mine.tests } : {}),
  });
  fork.source = { workflow: source.id, version: latest.version };
  return true;
}

const RESERVED = new Set(
  "break case catch class const continue debugger default delete do else enum export extends false finally for function if import in instanceof new null return super switch this throw true try typeof var void while with yield let static implements interface package private protected public await".split(
    " ",
  ),
);

/** Why a workflow's name is refused, as the API says it (`invalid-workflow`), or `null`. */
export function workflowNameProblem(name: string): string | null {
  const ok =
    name.length > 0 &&
    name.length <= 64 &&
    /^[A-Za-z_$][A-Za-z0-9_$]*$/.test(name) &&
    !RESERVED.has(name) &&
    !name.startsWith("wf_");
  return ok
    ? null
    : `invalid workflow name ${JSON.stringify(name)}: expected an identifier (letters, digits, _ and $) of at most 64 characters that is not a reserved word of JavaScript, since workflows import it from rewind:workflows, and that does not start with wf_, which starts the workflows' ids`;
}
