// A small world of workflows and runs for the mocked API: every status (queued, running, waiting
// on a signal, a timer, a harness action, an approval, a parked call or a quarantine, and each
// ending), a fork, a ten-day sleep, redeliveries, a payload above the inline limit and, for
// `pnpm dev:mock`, a run that goes on live. Times are relative to `now`. Who may read them is the
// access world's (`access.ts`): the tenant `local` without WorkOS, the organization Acme with it.
import type { Labels, Quarantine, Run, RunWorkflow, Waiting } from "../model/types";
import { type AccessOptions, type AccessWorld, createAccessWorld } from "./access";
import { ACTION_SETS, type ConnectionWorld, createConnectionWorld } from "./connections";
import { ensureHandles, originSlug } from "./handles";
import { fakeDigest, HistoryBuilder } from "./history";
import { createNamespaceWorld, type NamespaceWorld } from "./namespaces";
import { createSecretWorld, type SecretWorld } from "./secrets";
import {
  codeComment,
  createWorkflowWorld,
  type RecordedRun,
  resolveByCode,
  runWorkflowOf,
  shortIdOf,
  type WorkflowWorld,
  workflowId,
} from "./workflows";

const SECOND = 1_000;
const MINUTE = 60 * SECOND;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;

/** A code of the world: the file it was written as, its digest and its TypeScript source. */
export interface CodeFixture {
  name: string;
  digest: string;
  source: string;
}

/** What a call's state is while it waits, when not simply `claimed` by the harness's worker. */
type CallState = Pick<Extract<Waiting, { kind: "call" }>, "state" | "parked">;

export interface RunFixture {
  id: string;
  /** The end user the run acts for. */
  namespace: string;
  code: CodeFixture;
  actionSet: string;
  labels: Labels;
  history: HistoryBuilder;
  /** For a fork, the run it copies and the event it was forked at. */
  source?: { run: string; at?: number | null };
  /** The workflow it is a run of, and the version it executes: every run has one. */
  workflow?: RunWorkflow;
  /** For a run code started, that run and the `seq` of its call. */
  startedBy?: Omit<NonNullable<Run["startedBy"]>, "handle">;
  quarantine?: Quarantine;
  /** Where `fail_run` happened, for `Run.error.location`. */
  errorAt?: { line: number; column: number };
  /** The `seq` of the call whose failure failed the run. */
  failedSeq?: number;
  /** States of calls waiting, by `seq`, when they are not simply `claimed`. */
  callStates?: Record<number, CallState>;
  /** Deliveries beyond the first, by `seq`. */
  redeliveries?: Record<number, number>;
  /** The secrets it uses, by server (`ns/github`, `org/slack`). */
  secrets?: Record<string, string>;
  /** What ran, the middle of its handle, and the characters of its suffix: given once. */
  origin?: string;
  suffixLen?: number;
}

/**
 * The secrets a run of this code uses: each end user's GitHub for triage, the organization's for
 * the digest, the organization's Slack and CRM; `dax` is signed, `llm` and `weather` take none.
 */
function secretsOf(c: { name?: string; source: string }): Record<string, string> {
  const imported = /import \{([^}]*)\} from "rewind:actions"/.exec(c.source)?.[1] ?? "";
  const servers = new Set(imported.split(",").map((name) => name.trim()));
  const out: Record<string, string> = {};
  if (servers.has("github")) out.github = c.name?.startsWith("digest") ? "org/github" : "ns/github";
  if (servers.has("slack")) out.slack = "org/slack";
  if (servers.has("crm")) out.crm = "org/crm";
  return out;
}

export interface World {
  now: number;
  codes: CodeFixture[];
  runs: RunFixture[];
  payloads: Map<string, unknown>;
  /** The operator's key: when set, every request must carry a key or an access token. */
  requireKey?: string;
  embedded: boolean;
  /** Identity and access: the sign-in, organizations, roles, grants, API keys. */
  access: AccessWorld;
  /** The end users of each tenant, their quotas, use and alerts. */
  namespaces: Map<string, NamespaceWorld>;
  /** The workflows, their versions and suites, and the stored tests: the tenant's. */
  workflows: WorkflowWorld;
  /** People's connections, the requests waiting for their consent, the action sets, `/mcp`. */
  connections: ConnectionWorld;
  /** The secrets of each tenant, the organization's and its namespaces', and its signing keys. */
  secrets: Map<string, SecretWorld>;
}

function code(name: string, source: string): CodeFixture {
  return { name, source, digest: fakeDigest(source) };
}

const TRIAGE = code(
  "triage.ts",
  `import { github, slack } from "rewind:actions";
import { sleep, signal } from "rewind";

export default async function main(input: { repo: string }) {
  const issues = await github.list_issues({ repo: input.repo, state: "open" });
  const stale = issues.filter((i) => Date.now() - Date.parse(i.updated_at) > 30 * 86_400_000);
  console.log(\`\${stale.length} of \${issues.length} open issues are stale\`);

  await slack.post_message({ channel: "#triage", text: \`\${stale.length} stale issues in \${input.repo}\` });

  const decision = await signal<{ close: boolean }>("approval", { timeout: "3d" });
  if (!decision?.close) return { closed: 0 };

  await Promise.all(stale.map((i) => github.close_issue({ repo: input.repo, number: i.number })));

  await sleep("10d");
  const reopened = await github.list_issues({ repo: input.repo, state: "open", labels: "reopened" });
  return { closed: stale.length, reopened: reopened.length };
}
`,
);

const TRIAGE_FIXED = code(
  "triage.ts",
  `import { github, slack } from "rewind:actions";
import { sleep, signal } from "rewind";

export default async function main(input: { repo: string }) {
  const issues = await github.list_issues({ repo: input.repo, state: "open" });
  const stale = issues.filter((i) => Date.now() - Date.parse(i.updated_at) > 30 * 86_400_000);
  console.log(\`\${stale.length} of \${issues.length} open issues are stale\`);

  await slack.post_message({ channel: "#triage", text: \`\${stale.length} stale issues in \${input.repo}\` });

  const decision = await signal<{ close: boolean }>("approval", { timeout: "3d" });
  if (!decision?.close) return { closed: 0 };

  const results = await Promise.allSettled(stale.map((i) => github.close_issue({ repo: input.repo, number: i.number })));
  const failed = results.filter((r) => r.status === "rejected").length;
  if (failed > 0) console.warn(\`\${failed} issues could not be closed\`);

  return { closed: results.length - failed, failed };
}
`,
);

const DIGEST = code(
  "digest.ts",
  `import { github, llm, slack } from "rewind:actions";

export default async function main(input: { repos: string[]; channel: string }) {
  const prs = [];
  for (const repo of input.repos) {
    const page = await github.list_pull_requests({ repo, state: "closed", since: "7d" });
    prs.push(...page.filter((pr) => pr.merged_at));
  }
  console.info(\`summarising \${prs.length} merged pull requests\`);

  const summary = await llm.complete({
    model: "claude-haiku-5-5",
    prompt: \`Summarise these changes for the team:\\n\${prs.map((p) => \`- \${p.title}\`).join("\\n")}\`,
  });

  const posted = await slack.post_message({ channel: input.channel, text: summary.text });
  return { prs: prs.length, ts: posted.ts };
}
`,
);

const ONBOARDING = code(
  "onboarding.ts",
  `import { ask_user, crm } from "rewind:actions";
import { sleep } from "rewind";

export default async function main(input: { email: string }) {
  const account = await crm.find_account({ email: input.email });
  if (!account) {
    const answer = await ask_user({ question: \`No account for \${input.email}. Create one?\` });
    if (answer !== "yes") return { created: false };
  }
  let attempt = 0;
  while (true) {
    try {
      return await crm.provision({ email: input.email, plan: account?.plan ?? "trial" });
    } catch (e) {
      if (++attempt === 3) throw e;
      console.warn(\`provision failed (\${attempt}/3), retrying in 1m\`);
      await sleep("1m");
    }
  }
}
`,
);

const FORECAST = code(
  "forecast.ts",
  `// One-shot lookup started by the support agent.
import { weather } from "rewind:actions";

export default async function main(input: { city: string }) {
  const forecast = await weather.forecast({ city: input.city, days: 3 });
  return forecast.days[0];
}
`,
);

/**
 * A notifications workflow that never returns: it loops on \`signal("notify")\` (line 25) until
 * its deadline cancels it, as the run 01a1e9ea-…-2f2ec0a9da28 of dev_notifications did.
 */
const DEV_NOTIFICATIONS = code(
  "dev_notifications.ts",
  `import { dax } from "rewind:actions";
import { signal } from "rewind";

// The user's notifications for one day: the one door to the lock screen.
// Every goal asks it with \`dax.notify\`; it alone calls \`dax.push\`, and holds
// the policy: nothing in the quiet hours, at most \`perDay\` a day, the most
// urgent first when several wait.

type Input = {
  user: string;
  settings: { perDay: number; quietFrom: number; quietTo: number };
};

type Notify = { goal: string; title: string; body: string; urgency: "low" | "normal" | "urgent" };

const URGENCY = { low: 0, normal: 1, urgent: 2 } as const;

export default async function main({ user, settings }: Input) {
  let sent = 0;
  const held: Notify[] = [];

  // Each goal's request arrives as a signal, and is pushed now or held for
  // later.
  while (true) {
    const note = await signal<Notify>("notify");
    const hour = new Date().getHours();
    const wraps = settings.quietFrom > settings.quietTo;
    const quiet = wraps ? hour >= settings.quietFrom || hour < settings.quietTo : hour >= settings.quietFrom && hour < settings.quietTo;
    const capped = sent >= settings.perDay || (sent === settings.perDay - 1 && (URGENCY[note.urgency] < URGENCY.urgent));
    if (quiet || capped) {
      held.push(note);
      held.sort((a, b) => URGENCY[b.urgency] - URGENCY[a.urgency]);
      continue;
    }
    await dax.push({ user, goal: note.goal, title: note.title, body: note.body });
    sent += 1;
  }
}
`,
);

/** The code of crates/rewind-e2e/corpus/epoch-1/blue/triage_workflow. */
const TRIAGE_WORKFLOW = code(
  "triage_workflow.ts",
  `import { github, slack } from "rewind:actions";
import { sleep, signal } from "rewind";

export default async function main(input: { repo: string }) {
  const issues = await github.list_issues({ repo: input.repo, state: "open" });
  const stale = issues.filter((i) => Date.now() - Date.parse(i.updated_at) > 30 * 86_400_000);

  await slack.post_message({ channel: "#triage", text: \`\${stale.length} stale issues in \${input.repo}\` });

  const decision = await signal<{ close: boolean }>("approval", { timeout: "3d" });
  if (!decision?.close) return { closed: 0 };

  await Promise.all(stale.map((i) => github.close_issue({ repo: input.repo, number: i.number })));

  await sleep("10d");
  const reopened = await github.list_issues({ repo: input.repo, state: "open", labels: "reopened" });
  return { closed: stale.length, reopened: reopened.length };
}
`,
);

/** The first version of the workflow `triage`: it closed the closed issues too. */
const TRIAGE_REPO_V1 = code(
  "triage_repo.ts",
  `import { github } from "rewind:actions";
import { signal } from "rewind";

export default async function main(input: { repo: string }) {
  const issues = await github.list_issues({ repo: input.repo, state: "all" });
  const stale = issues.filter((i) => Date.now() - Date.parse(i.updated_at) > 30 * 86_400_000);
  const { close } = await signal("approval");
  if (!close) return { closed: 0 };
  await Promise.all(stale.map((i) => github.close_issue({ repo: input.repo, number: i.number })));
  return { closed: stale.length };
}
`,
);

/** The third version of the workflow `triage`: it says how many issues it found stale. */
const TRIAGE_REPO_V3 = code(
  "triage_repo.ts",
  `import { github } from "rewind:actions";
import { signal } from "rewind";

export default async function main(input: { repo: string }) {
  const issues = await github.list_issues({ repo: input.repo, state: "open" });
  const stale = issues.filter((i) => Date.now() - Date.parse(i.updated_at) > 30 * 86_400_000);
  console.log(\`\${stale.length} of \${issues.length} open issues are stale\`);
  const { close } = await signal("approval");
  if (!close) return { closed: 0 };
  await Promise.all(stale.map((i) => github.close_issue({ repo: input.repo, number: i.number })));
  return { closed: stale.length };
}
`,
);

/** The code of the workflow `triage` at its second version: a run per repository, which the run below starts. */
const TRIAGE_REPO = code(
  "triage_repo.ts",
  `import { github } from "rewind:actions";
import { signal } from "rewind";

export default async function main(input: { repo: string }) {
  const issues = await github.list_issues({ repo: input.repo, state: "open" });
  const stale = issues.filter((i) => Date.now() - Date.parse(i.updated_at) > 30 * 86_400_000);
  const { close } = await signal("approval");
  if (!close) return { closed: 0 };
  await Promise.all(stale.map((i) => github.close_issue({ repo: input.repo, number: i.number })));
  return { closed: stale.length };
}
`,
);

/** Code that starts runs of the workflows `triage` and `digest`, signals them and waits. */
const TRIAGE_ALL = code(
  "triage_all.ts",
  `import { digest, triage } from "rewind:workflows";

export default async function main(input: { repos: string[] }) {
  const runs = await Promise.all(input.repos.map((repo) => triage.start({ repo })));
  await Promise.all(runs.map((run) => run.signal("approval", { close: true })));
  await digest.start({ repos: input.repos, channel: "#eng" }, { detached: true });
  const results = await Promise.all(runs.map((run) => run.result()));
  return { closed: results.reduce((sum, result) => sum + result.closed, 0) };
}
`,
);

/** The ids of the world's workflows. */
export const WORKFLOWS = {
  /** The organization's `triage`, versions 1 to 3. */
  triage: workflowId("triage"),
  /** The organization's `digest`, version 1. */
  digest: workflowId("digest"),
  /** dana's fork of `triage`, at its version 2. */
  triageFork: workflowId("triage@dana"),
  /** The organization's forecast, unnamed: it shows by its code's first comment line. */
  forecast: workflowId("forecast"),
  /** The organization's onboarding, unnamed and without a comment: its short id. */
  onboarding: workflowId("onboarding"),
  /** chen's daily digest, unnamed and without a comment: its short id. */
  chenDigest: workflowId("digest@chen"),
  /** alice's `triage_stale_issues`, versions 1 and 2. */
  staleIssues: workflowId("triage_stale_issues"),
  /** alice's run that starts runs of triage and digest, unnamed. */
  triageAll: workflowId("triage_all@alice"),
  /** local's run of the e2e corpus's triage_workflow, unnamed. */
  triageWorkflow: workflowId("triage_workflow@local"),
  /** Dax's `dev_notifications`, in its end user's namespace. */
  devNotifications: workflowId("dev_notifications"),
};

export const CODES = {
  TRIAGE,
  TRIAGE_FIXED,
  DIGEST,
  ONBOARDING,
  FORECAST,
  TRIAGE_WORKFLOW,
  TRIAGE_REPO_V1,
  TRIAGE_REPO,
  TRIAGE_REPO_V3,
  TRIAGE_ALL,
  DEV_NOTIFICATIONS,
};

/**
 * The history of crates/rewind-e2e/corpus/epoch-1/blue/triage_workflow, started at `start`: 18
 * events, 8 steps. Its callsites are the ones the runtime recorded.
 */
export function triageWorkflowHistory(start: number, actionSet = ACTION_SETS.github) {
  const repo = "acme/api";
  return new HistoryBuilder(start)
    .started({ code: TRIAGE_WORKFLOW.digest, actionSet, input: { repo }, mode: "test" })
    .call(1, "github.list_issues", { repo, state: "open" }, 5, 31)
    .ok(1, [
      { number: 1, updated_at: "2025-10-01T00:00:00Z" },
      { number: 2, updated_at: "2025-12-30T00:00:00Z" },
      { number: 3, updated_at: "2025-09-15T00:00:00Z" },
    ])
    .call(
      2,
      "slack.post_message",
      { channel: "#triage", text: "2 stale issues in acme/api" },
      8,
      15,
    )
    .ok(2, { ok: true })
    .timer(3, 3 * DAY, 10, 26)
    .awaitSignal("approval", 10, 3, 26)
    .after(HOUR)
    .signal("approval", { close: true })
    .cancelTimer(3)
    .call(4, "github.close_issue", { number: 1, repo }, 13, 45)
    .call(5, "github.close_issue", { number: 3, repo }, 13, 45)
    .ok(4, { number: 1, repo })
    .ok(5, { number: 3, repo })
    .timer(6, 10 * DAY, 15, 9)
    .after(10 * DAY)
    .fired(6)
    .call(7, "github.list_issues", { labels: "reopened", repo, state: "open" }, 16, 33)
    .ok(7, [{ number: 3, updated_at: "2026-01-12T00:00:00Z" }])
    .complete({ closed: 2, reopened: 1 });
}

/** The name a callsite points at: the callee's last name (`list_issues` of `github.list_issues`). */
function calleeNames(event: { type: string; action?: string; level?: string }): string[] {
  switch (event.type) {
    case "schedule_call":
      return [event.action?.slice(event.action.lastIndexOf(".") + 1) ?? ""];
    case "start_timer":
      return ["sleep", "signal"];
    case "signal_awaited":
      return ["signal"];
    case "log":
      return event.level === "info" ? ["log", "info"] : [event.level ?? "log"];
    default:
      return [];
  }
}

/**
 * Points each callsite at its callee's name, as the runtime records it, where a fixture gave
 * only the line: the column of the first name on the line the event could have come from.
 */
function alignCallsites(fixture: RunFixture) {
  const lines = fixture.code.source.split("\n");
  for (const entry of fixture.history.events) {
    const event = entry.event as {
      type: string;
      callsite?: { line: number; column: number } | null;
    };
    const site = event.callsite;
    if (!site) continue;
    const text = lines[site.line - 1] ?? "";
    for (const name of calleeNames(event as never)) {
      const at = new RegExp(`\\b${name}\\b`).exec(text)?.index;
      if (at === undefined) continue;
      const inside = site.column - 1 >= at && site.column - 1 < at + name.length;
      if (!inside) site.column = at + 1;
      break;
    }
  }
}

const SET_GITHUB = ACTION_SETS.github;
const SET_DIGEST = ACTION_SETS.digest;
const SET_CRM = ACTION_SETS.crm;
const SET_WEATHER = ACTION_SETS.weather;
const SET_WORKFLOWS = ACTION_SETS.workflows;
const SET_DAX = ACTION_SETS.dax;

/** A deterministic UUID v7 for a run started at `at`. */
export function runId(at: number, n: number): string {
  const hex = Math.max(0, Math.floor(at)).toString(16).padStart(12, "0").slice(-12);
  const rand = fakeDigest(`run-${n}-${at}`);
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-7${rand.slice(0, 3)}-a${rand.slice(3, 6)}-${rand.slice(6, 18)}`;
}

function mulberry32(seed: number): () => number {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4_294_967_296;
  };
}

function issues(count: number, now: number, pad = 0) {
  return Array.from({ length: count }, (_, i) => ({
    number: 4100 + i,
    title: `Issue ${4100 + i}: ${["flaky test in ci", "docs typo", "panic on empty body", "slow query"][i % 4]}`,
    updated_at: new Date(now - (i % 3 === 0 ? 45 : 3) * DAY).toISOString(),
    body: pad > 0 ? "x".repeat(pad) : undefined,
  }));
}

export interface WorldOptions {
  now: number;
  /** Adds a run whose events keep arriving after `now`, for `pnpm dev:mock`. */
  live?: boolean;
  requireKey?: string | undefined;
  embedded?: boolean;
  /** WorkOS on this server: members sign in, and the runs are the organization Acme's. */
  workos?: AccessOptions["workos"];
  tokenTtlMs?: number;
  githubEmail?: string;
  /** Where the codes and invitation links a deployment would email go. */
  log?: (line: string) => void;
  /** What `GET /v1/server` says of `/mcp`; by default served with OAuth when WorkOS is. */
  mcp?: { mcpUrl: string | null; oauth: boolean };
}

/**
 * The end user a run acts for, unless it names one: tests run in `local`, triage for alice,
 * digests for chen, support for several.
 */
function namespaceOf(labels: Labels, n: number): string {
  if (labels.env === "test") return "local";
  if (labels.agent === "triage") return "alice";
  if (labels.agent === "digest") return "chen";
  const spread = ["bob", "bob", "bob", "alice", "dana", "erin", "user_2fQ81", "user_7Kd02"];
  return spread[n % spread.length] ?? "bob";
}

export function createWorld(options: WorldOptions): World {
  const { now } = options;
  const runs: RunFixture[] = [];
  const recordedRuns: RecordedRun[] = [];
  let n = 0;
  const run = (
    start: number,
    c: CodeFixture,
    actionSet: string,
    labels: Labels,
    input: unknown,
    extra: Partial<RunFixture> = {},
    mode: "live" | "test" = "live",
  ): RunFixture => {
    n += 1;
    const history = new HistoryBuilder(start).started({
      code: c.digest,
      actionSet,
      input,
      mode,
    });
    const fixture: RunFixture = {
      id: runId(start, n),
      namespace: namespaceOf(labels, n),
      code: c,
      actionSet,
      labels,
      history,
      ...extra,
    };
    runs.push(fixture);
    return fixture;
  };

  // triage: completed after a ten-day sleep.
  {
    const r = run(
      now - 14 * DAY - 3 * HOUR,
      TRIAGE,
      SET_GITHUB,
      { agent: "triage", conversation: "c-41", env: "prod" },
      { repo: "acme/api" },
    );
    const j = r.history;
    j.after(40)
      .call(1, "github.list_issues", { repo: "acme/api", state: "open" }, 5)
      .after(310)
      .ok(1, issues(37, now - 14 * DAY))
      .log("info", "3 of 37 open issues are stale", 7)
      .call(2, "slack.post_message", { channel: "#triage", text: "3 stale issues in acme/api" }, 9)
      .after(120)
      .ok(2, { ok: true, ts: "1731146531.000200" })
      .timer(3, 3 * DAY, 11)
      .awaitSignal("approval", 11, 3)
      .after(2 * HOUR + 14 * MINUTE)
      .signal("approval", { close: true, by: "maya@acme.dev" })
      .cancelTimer(3)
      .call(4, "github.close_issue", { repo: "acme/api", number: 4100 }, 14, 52)
      .call(5, "github.close_issue", { repo: "acme/api", number: 4103 }, 14, 52)
      .call(6, "github.close_issue", { repo: "acme/api", number: 4106 }, 14, 52)
      .after(180)
      .ok(5, { state: "closed" })
      .after(40)
      .ok(4, { state: "closed" })
      .after(95)
      .ok(6, { state: "closed" })
      .timer(7, 10 * DAY, 16)
      .after(10 * DAY)
      .fired(7)
      .call(8, "github.list_issues", { repo: "acme/api", state: "open", labels: "reopened" }, 17)
      .after(260)
      .ok(8, issues(1, now - 4 * DAY))
      .complete({ closed: 3, reopened: 1 });
    r.redeliveries = { 1: 1 };
  }

  // triage: active, waiting for the approval, with a result above the inline limit.
  {
    const r = run(
      now - 2 * HOUR - 11 * MINUTE,
      TRIAGE,
      SET_GITHUB,
      { agent: "triage", conversation: "c-42", env: "prod" },
      { repo: "acme/web" },
    );
    r.history
      .after(35)
      .call(1, "github.list_issues", { repo: "acme/web", state: "open" }, 5)
      .after(1_840)
      .ok(1, issues(120, now - 2 * HOUR, 2_400))
      .log("info", "41 of 120 open issues are stale", 7)
      .call(2, "slack.post_message", { channel: "#triage", text: "41 stale issues in acme/web" }, 9)
      .after(140)
      .ok(2, { ok: true, ts: "1731396123.000400" })
      .timer(3, 3 * DAY, 11)
      .awaitSignal("approval", 11, 3);
  }

  // triage: failed, one close_issue refused inside Promise.all.
  const failedTriage = run(
    now - 26 * HOUR,
    TRIAGE,
    SET_GITHUB,
    { agent: "triage", conversation: "c-39", env: "prod" },
    { repo: "acme/infra" },
    {
      errorAt: { line: 14, column: 3 },
      failedSeq: 6,
    },
  );
  {
    const j = failedTriage.history;
    j.after(30)
      .call(1, "github.list_issues", { repo: "acme/infra", state: "open" }, 5)
      .after(410)
      .ok(1, issues(12, now - 26 * HOUR))
      .log("info", "5 of 12 open issues are stale", 7)
      .call(
        2,
        "slack.post_message",
        { channel: "#triage", text: "5 stale issues in acme/infra" },
        9,
      )
      .after(130)
      .ok(2, { ok: true, ts: "1731310000.000100" })
      .signal("approval", { close: true, by: "sam@acme.dev" })
      .timer(3, 3 * DAY, 11)
      .awaitSignal("approval", 11, 3)
      .cancelTimer(3);
    for (let seq = 4; seq <= 8; seq += 1)
      j.call(
        seq,
        "github.close_issue",
        { repo: "acme/infra", number: 4100 + 3 * (seq - 4) },
        14,
        52,
      );
    j.after(150)
      .ok(4, { state: "closed" })
      .ok(5, { state: "closed" })
      .after(30)
      .fail(6, "error", "Resource not accessible by integration (403)", {
        status: 403,
        documentation_url: "https://docs.github.com/rest/issues/issues#update-an-issue",
      })
      .after(60)
      .ok(7, { state: "closed" })
      .ok(8, { state: "closed" })
      .failRun(
        "ActionError",
        "github.close_issue: Resource not accessible by integration (403)",
        "ActionError: github.close_issue: Resource not accessible by integration (403)\n    at async Promise.all (index 2)\n    at async main (workflow.ts:14:3)",
      );
  }

  // triage: the fork of the failed run, with Promise.allSettled.
  {
    const r = run(
      now - 25 * HOUR,
      TRIAGE_FIXED,
      SET_GITHUB,
      { agent: "triage", conversation: "c-39", env: "prod" },
      { repo: "acme/infra" },
      {
        source: { run: failedTriage.id, at: null },
      },
    );
    const j = r.history;
    j.after(5)
      .call(1, "github.list_issues", { repo: "acme/infra", state: "open" }, 5)
      .ok(1, issues(12, now - 26 * HOUR))
      .log("info", "5 of 12 open issues are stale", 7)
      .call(
        2,
        "slack.post_message",
        { channel: "#triage", text: "5 stale issues in acme/infra" },
        9,
      )
      .ok(2, { ok: true, ts: "1731310000.000100" })
      .signal("approval", { close: true, by: "sam@acme.dev" })
      .timer(3, 3 * DAY, 11)
      .awaitSignal("approval", 11, 3)
      .cancelTimer(3);
    for (let seq = 4; seq <= 8; seq += 1)
      j.call(
        seq,
        "github.close_issue",
        { repo: "acme/infra", number: 4100 + 3 * (seq - 4) },
        14,
        70,
      );
    j.ok(4, { state: "closed" })
      .ok(5, { state: "closed" })
      .fail(6, "error", "Resource not accessible by integration (403)")
      .ok(7, { state: "closed" })
      .ok(8, { state: "closed" })
      .after(10)
      .log("warn", "1 issues could not be closed", 16)
      .complete({ closed: 4, failed: 1 });
  }

  // triage: active, asleep for ten days.
  {
    const r = run(
      now - 6 * DAY - 2 * HOUR,
      TRIAGE,
      SET_GITHUB,
      { agent: "triage", conversation: "c-40", env: "prod" },
      { repo: "acme/docs" },
    );
    r.history
      .after(20)
      .call(1, "github.list_issues", { repo: "acme/docs", state: "open" }, 5)
      .after(220)
      .ok(1, issues(4, now - 6 * DAY))
      .log("info", "2 of 4 open issues are stale", 7)
      .call(2, "slack.post_message", { channel: "#triage", text: "2 stale issues in acme/docs" }, 9)
      .after(90)
      .ok(2, { ok: true, ts: "1730900000.000100" })
      .timer(3, 3 * DAY, 11)
      .awaitSignal("approval", 11, 3)
      .after(31 * MINUTE)
      .signal("approval", { close: true, by: "maya@acme.dev" })
      .cancelTimer(3)
      .call(4, "github.close_issue", { repo: "acme/docs", number: 4100 }, 14, 52)
      .call(5, "github.close_issue", { repo: "acme/docs", number: 4103 }, 14, 52)
      .after(200)
      .ok(4, { state: "closed" })
      .after(20)
      .ok(5, { state: "closed" })
      .timer(6, 10 * DAY, 16);
  }

  // digest: completed.
  {
    const r = run(
      now - 3 * DAY - 5 * HOUR,
      DIGEST,
      SET_DIGEST,
      { agent: "digest", schedule: "weekly", env: "prod" },
      { repos: ["acme/api", "acme/web", "acme/infra"], channel: "#eng" },
    );
    const j = r.history;
    ["acme/api", "acme/web", "acme/infra"].forEach((repo, i) => {
      j.after(25)
        .call(i + 1, "github.list_pull_requests", { repo, state: "closed", since: "7d" }, 6, 18)
        .after(380 + 90 * i)
        .ok(
          i + 1,
          Array.from({ length: 4 + i }, (_, k) => ({
            number: 900 + k,
            title: `Improve ${repo} ${k}`,
            merged_at: "2026-11-08T09:00:00Z",
          })),
        );
    });
    j.log("info", "summarising 12 merged pull requests", 9)
      .call(
        4,
        "llm.complete",
        {
          model: "claude-haiku-5-5",
          prompt: "Summarise these changes for the team:\n- Improve acme/api 0\n…",
        },
        11,
        25,
      )
      .after(4_210)
      .ok(4, { text: "This week: faster CI, a fix for empty bodies, docs." })
      .call(
        5,
        "slack.post_message",
        { channel: "#eng", text: "This week: faster CI, a fix for empty bodies, docs." },
        16,
        24,
      )
      .after(160)
      .ok(5, { ok: true, ts: "1731150000.000900" })
      .complete({ prs: 12, ts: "1731150000.000900" });
    r.redeliveries = { 4: 2 };
  }

  // digest: aborted by the CPU budget.
  {
    const r = run(
      now - 10 * DAY,
      DIGEST,
      SET_DIGEST,
      { agent: "digest", schedule: "weekly", env: "prod" },
      { repos: ["acme/api", "acme/monorepo"], channel: "#eng" },
    );
    r.history
      .after(20)
      .call(
        1,
        "github.list_pull_requests",
        { repo: "acme/api", state: "closed", since: "7d" },
        6,
        18,
      )
      .after(390)
      .ok(1, [{ number: 1, title: "Bump deps", merged_at: "2026-11-01T09:00:00Z" }])
      .call(
        2,
        "github.list_pull_requests",
        { repo: "acme/monorepo", state: "closed", since: "7d" },
        6,
        18,
      )
      .after(2_900)
      .ok(
        2,
        Array.from({ length: 500 }, (_, k) => ({
          number: k,
          title: `Change ${k}`,
          merged_at: k % 2 ? "2026-11-02T09:00:00Z" : null,
        })),
      )
      .abort("limit_exceeded", "The code used more than its CPU budget of 2s in one call (2.31s).");
  }

  // digest: failed with a TypeError.
  {
    const r = run(
      now - 17 * HOUR,
      DIGEST,
      SET_DIGEST,
      { agent: "digest", schedule: "adhoc", env: "staging" },
      { repos: ["acme/api"], channel: "#eng-staging" },
      {
        errorAt: { line: 16, column: 75 },
      },
    );
    r.history
      .after(20)
      .call(
        1,
        "github.list_pull_requests",
        { repo: "acme/api", state: "closed", since: "7d" },
        6,
        18,
      )
      .after(350)
      .ok(1, [{ number: 7, title: "Fix login", merged_at: "2026-11-11T09:00:00Z" }])
      .log("info", "summarising 1 merged pull requests", 9)
      .call(
        2,
        "llm.complete",
        { model: "claude-haiku-5-5", prompt: "Summarise these changes for the team:\n- Fix login" },
        11,
        25,
      )
      .after(2_650)
      .ok(2, { content: [{ type: "text", text: "Login is fixed." }] })
      .failRun(
        "TypeError",
        "Cannot read properties of undefined (reading 'text')",
        "TypeError: Cannot read properties of undefined (reading 'text')\n    at main (workflow.ts:16:75)",
      );
  }

  // digest: quarantined, its replay diverged.
  {
    const r = run(
      now - 9 * HOUR,
      DIGEST,
      SET_DIGEST,
      { agent: "digest", schedule: "weekly", env: "prod" },
      { repos: ["acme/web", "acme/api"], channel: "#eng" },
      {
        quarantine: {
          cause: "non_determinism",
          reason:
            'Replay diverged at event 3: the code scheduled github.list_pull_requests({"repo":"acme/api",…}) at workflow.ts:6, where the history has github.list_pull_requests({"repo":"acme/web",…}).',
        },
      },
    );
    r.history
      .after(20)
      .call(
        1,
        "github.list_pull_requests",
        { repo: "acme/web", state: "closed", since: "7d" },
        6,
        18,
      )
      .after(420)
      .ok(1, [{ number: 3, title: "Dark mode", merged_at: "2026-11-10T09:00:00Z" }])
      .call(
        2,
        "github.list_pull_requests",
        { repo: "acme/api", state: "closed", since: "7d" },
        6,
        18,
      );
  }

  // onboarding: completed after two failed provisions and two sleeps.
  {
    const r = run(
      now - 4 * DAY - 7 * HOUR,
      ONBOARDING,
      SET_CRM,
      { agent: "support", conversation: "c-77", env: "prod" },
      { email: "lee@example.org" },
      { namespace: "bob" },
    );
    const j = r.history;
    j.after(15)
      .call(1, "crm.find_account", { email: "lee@example.org" }, 5)
      .after(95)
      .ok(1, null)
      .call(2, "ask_user", { question: "No account for lee@example.org. Create one?" }, 7, 26)
      .after(6 * MINUTE + 12 * SECOND)
      .ok(2, "yes")
      .call(3, "crm.provision", { email: "lee@example.org", plan: "trial" }, 13, 20)
      .after(30 * SECOND)
      .fail(3, "unavailable", "crm is unavailable: 503 Service Unavailable")
      .log("warn", "provision failed (1/3), retrying in 1m", 16)
      .timer(4, MINUTE, 17)
      .after(MINUTE)
      .fired(4)
      .call(5, "crm.provision", { email: "lee@example.org", plan: "trial" }, 13, 20)
      .after(30 * SECOND)
      .fail(5, "timeout", "no answer within 30s")
      .log("warn", "provision failed (2/3), retrying in 1m", 16)
      .timer(6, MINUTE, 17)
      .after(MINUTE)
      .fired(6)
      .call(7, "crm.provision", { email: "lee@example.org", plan: "trial" }, 13, 20)
      .after(1_420)
      .ok(7, { id: "acct_9f2", plan: "trial", created: true })
      .complete({ id: "acct_9f2", plan: "trial", created: true });
    r.redeliveries = { 3: 2, 5: 1 };
  }

  // onboarding: waiting, its harness action's call waits for the harness's approval.
  {
    const r = run(
      now - 47 * MINUTE,
      ONBOARDING,
      SET_CRM,
      { agent: "support", conversation: "c-81", env: "prod" },
      { email: "kim@example.org" },
      {
        namespace: "bob",
        callStates: { 2: { state: "approval" } },
      },
    );
    r.history
      .after(15)
      .call(1, "crm.find_account", { email: "kim@example.org" }, 5)
      .after(88)
      .ok(1, null)
      .call(2, "ask_user", { question: "No account for kim@example.org. Create one?" }, 7, 26);
  }

  // onboarding: waiting, its harness action's call claimed by the harness's worker.
  {
    const r = run(
      now - 12 * MINUTE,
      ONBOARDING,
      SET_CRM,
      { agent: "support", conversation: "c-85", env: "prod" },
      { email: "jo@example.org" },
      { namespace: "dana" },
    );
    r.history
      .after(15)
      .call(1, "crm.find_account", { email: "jo@example.org" }, 5)
      .after(91)
      .ok(1, null)
      .call(2, "ask_user", { question: "No account for jo@example.org. Create one?" }, 7, 26);
  }

  // The run of the e2e corpus's triage_workflow, a test run.
  {
    const r = run(
      now - 12 * DAY,
      TRIAGE_WORKFLOW,
      SET_GITHUB,
      { agent: "triage", conversation: "c-12", env: "test" },
      { repo: "acme/api" },
      {},
      "test",
    );
    r.history = triageWorkflowHistory(now - 12 * DAY, SET_GITHUB);
  }

  // onboarding: cancelled while waiting for the user.
  {
    const r = run(
      now - 2 * DAY - 3 * HOUR,
      ONBOARDING,
      SET_CRM,
      { agent: "support", conversation: "c-63", env: "prod" },
      { email: "ana@example.org" },
      {
        namespace: "erin",
        errorAt: { line: 7, column: 20 },
      },
    );
    r.history
      .after(15)
      .call(1, "crm.find_account", { email: "ana@example.org" }, 5)
      .after(102)
      .ok(1, null)
      .call(2, "ask_user", { question: "No account for ana@example.org. Create one?" }, 7, 26)
      .after(3 * HOUR)
      .cancelRequested("requested", "The conversation was closed.")
      .fail(2, "error", "cancelled")
      .failRun(
        "CancelledError",
        "The conversation was closed.",
        "CancelledError: The conversation was closed.\n    at main (workflow.ts:7:20)",
      );
  }

  // dev_notifications: its code loops on signal("notify") and never returns; its deadline
  // passed after 11h29m, which cancelled it in the await (no frame of workflow.ts in the stack).
  {
    const start = now - DAY - 2 * HOUR;
    const ttl = 11 * HOUR + 29 * MINUTE;
    const r = run(
      start,
      DEV_NOTIFICATIONS,
      SET_DAX,
      {
        dax: "iteration",
        env: "development",
        goal: "6afc64fdf4d56d8262048836",
        iteration: "6afc64fe1c0b5e2a9d7f3a41",
        kind: "routine",
        routine: "notifications_daily",
      },
      { user: "6afc64fd2de4a3f3f4c9e127", settings: { perDay: 4, quietFrom: 22, quietTo: 8 } },
      {
        id: "01a1e9ea-8258-7525-9241-2f2ec0a9da28",
        namespace: "6afc64fd2de4a3f3f4c9e127",
        origin: "dev_notifications",
      },
    );
    r.history = new HistoryBuilder(start)
      .started({
        code: DEV_NOTIFICATIONS.digest,
        actionSet: SET_DAX,
        input: {
          user: "6afc64fd2de4a3f3f4c9e127",
          settings: { perDay: 4, quietFrom: 22, quietTo: 8 },
        },
        ttlMs: ttl,
      })
      .awaitSignal("notify", 25, undefined, 24)
      .after(ttl)
      .cancelRequested("ttl")
      .failRun(
        "CancelledError",
        "run cancelled: its deadline has passed",
        "CancelledError: run cancelled: its deadline has passed",
      );
  }

  // Goals of a Dax user, each run waiting on the next notification with six labels, the ids of
  // Dax's records among them: what a row of the waiting runs holds for such an integration.
  for (const [i, ago] of [2 * HOUR, 40 * MINUTE].entries()) {
    const start = now - ago;
    const settings = { perDay: 4, quietFrom: 22, quietTo: 8 };
    const r = run(
      start,
      DEV_NOTIFICATIONS,
      SET_DAX,
      {
        dax: "iteration",
        env: "prod",
        goal: "6afbb6ae56fc7339557eb771",
        iteration: `6b0373fe1ab77da6944addc${"bc"[i]}`,
        kind: "goal",
        routine: `6afbb6ad56fc7339557eb76${7 + i}`,
      },
      { user: "6afc64fd2de4a3f3f4c9e127", settings },
      { namespace: "6afc64fd2de4a3f3f4c9e127", origin: "dev_notifications" },
    );
    r.history = new HistoryBuilder(start)
      .started({
        code: DEV_NOTIFICATIONS.digest,
        actionSet: SET_DAX,
        input: { user: "6afc64fd2de4a3f3f4c9e127", settings },
      })
      .awaitSignal("notify", 25, undefined, 24);
  }

  // onboarding: a test run, in virtual time.
  {
    const r = run(
      now - 5 * HOUR,
      ONBOARDING,
      SET_CRM,
      { agent: "support", env: "test" },
      { email: "test@example.org" },
      {},
      "test",
    );
    r.history
      .call(1, "crm.find_account", { email: "test@example.org" }, 5)
      .ok(1, { id: "acct_1", plan: "pro" })
      .call(2, "crm.provision", { email: "test@example.org", plan: "pro" }, 13, 20)
      .ok(2, { id: "acct_1", plan: "pro", created: false })
      .complete({ id: "acct_1", plan: "pro", created: false });
  }

  // forecast: many short runs, for the lists and the action statistics.
  const random = mulberry32(7);
  const cities = ["Lyon", "Oslo", "Lima", "Kyoto", "Accra", "Perth", "Quito", "Riga"];
  for (let i = 0; i < 64; i += 1) {
    const start = now - Math.floor(random() * 7 * DAY) - MINUTE;
    const city = cities[i % cities.length] ?? "Lyon";
    const r = run(
      start,
      FORECAST,
      SET_WEATHER,
      { agent: "support", conversation: `c-${200 + i}`, env: "prod" },
      { city },
    );
    const latency = Math.round(120 + random() * 380 + (random() < 0.1 ? 4_000 : 0));
    r.history.after(10).call(1, "weather.forecast", { city, days: 3 }, 5, 26).after(latency);
    const roll = random();
    if (roll < 0.06) {
      r.history
        .fail(1, "timeout", "no answer within 5s")
        .after(2)
        .failRun(
          "ActionError",
          "weather.forecast: no answer within 5s",
          "ActionError: weather.forecast: no answer within 5s\n    at main (workflow.ts:5:26)",
        );
      r.errorAt = { line: 5, column: 26 };
      r.failedSeq = 1;
    } else if (roll < 0.09) {
      r.history
        .fail(1, "unavailable", "weather is unavailable: connection refused")
        .after(2)
        .failRun("ActionError", "weather.forecast: weather is unavailable: connection refused");
      r.errorAt = { line: 5, column: 26 };
      r.failedSeq = 1;
    } else {
      const day = {
        date: "2026-11-12",
        high: Math.round(4 + random() * 20),
        low: Math.round(random() * 6),
        sky: random() < 0.5 ? "rain" : "clear",
      };
      r.history
        .ok(1, { city, days: [day, day, day] })
        .after(1)
        .complete(day);
    }
    if (random() < 0.15) r.redeliveries = { 1: 1 + Math.floor(random() * 2) };
  }

  // Workflows: a run that starts two runs of triage and waits for them, and a detached digest.
  {
    const start = now - 5 * HOUR - 20 * MINUTE;
    const labels = { agent: "triage", conversation: "c-50", env: "prod" };
    const repos = ["acme/api", "acme/web"];
    const parent = run(start, TRIAGE_ALL, SET_WORKFLOWS, labels, { repos });
    const children = repos.map((repo, i) =>
      run(
        start + 60 + 5 * i,
        TRIAGE_REPO,
        SET_GITHUB,
        labels,
        { repo },
        {
          origin: "triage",
          startedBy: { run: parent.id, seq: i + 1, detached: false },
        },
      ),
    );
    const detached = run(
      start + 400,
      DIGEST,
      SET_DIGEST,
      labels,
      { repos, channel: "#eng" },
      {
        origin: "digest",
        startedBy: { run: parent.id, seq: 5, detached: true },
      },
    );
    const [api, web] = children as [RunFixture, RunFixture];
    const closed = [2, 1];
    // The third version of triage records a test from the first run, and from an older run of
    // an end user whose runs the caller cannot read.
    recordedRuns.push(
      { id: api.id, namespace: api.namespace, start: start + 60, repo: "acme/api", closed: 2 },
      {
        id: runId(start - 2 * DAY, 9_001),
        namespace: "user_9Qx31",
        start: start - 2 * DAY,
        repo: "acme/infra",
        closed: 1,
        restricted: true,
      },
    );
    children.forEach((child, i) => {
      const repo = repos[i] as string;
      child.history
        .after(30)
        .call(1, "github.list_issues", { repo, state: "open" }, 5)
        .after(240)
        .ok(1, issues(6 - 2 * i, start))
        .awaitSignal("approval", 7)
        .after(50 + 10 * i)
        .signal("approval", { close: true });
      for (let k = 0; k < (closed[i] ?? 0); k += 1)
        child.history.call(2 + k, "github.close_issue", { repo, number: 4100 + 3 * k }, 9, 52);
      child.history.after(180);
      for (let k = 0; k < (closed[i] ?? 0); k += 1) child.history.ok(2 + k, { state: "closed" });
      child.history.complete({ closed: closed[i] });
    });
    const result = TRIAGE_ALL.source.split("\n")[6]?.indexOf("run.result") ?? 0;
    parent.history
      .after(20)
      .call(1, "workflows.triage.start", { input: { repo: "acme/api" } }, 4, 59)
      .call(2, "workflows.triage.start", { input: { repo: "acme/web" } }, 4, 59)
      .after(45)
      .ok(1, { runId: api.id })
      .ok(2, { runId: web.id })
      .after(300)
      .call(
        3,
        "workflows.triage.signal",
        { run: api.id, name: "approval", payload: { close: true } },
        5,
        50,
      )
      .call(
        4,
        "workflows.triage.signal",
        { run: web.id, name: "approval", payload: { close: true } },
        5,
        50,
      )
      .after(15)
      .ok(3, {})
      .ok(4, {})
      .call(
        5,
        "workflows.digest.start",
        { input: { repos, channel: "#eng" }, detached: true },
        6,
        16,
      )
      .after(40)
      .ok(5, { runId: detached.id })
      .awaitSignal(`rewind:ended:${api.id}`, 7, undefined, result + 5)
      .awaitSignal(`rewind:ended:${web.id}`, 7, undefined, result + 5)
      .after(200)
      .signal(`rewind:ended:${api.id}`, { status: "completed", output: { closed: 2 } })
      .after(10)
      .signal(`rewind:ended:${web.id}`, { status: "completed", output: { closed: 1 } })
      .complete({ closed: 3 });
    // The detached run ends after its parent: its end goes nowhere.
    detached.history
      .after(25)
      .call(
        1,
        "github.list_pull_requests",
        { repo: "acme/api", state: "closed", since: "7d" },
        6,
        18,
      )
      .after(400)
      .ok(1, [{ number: 31, title: "Faster CI", merged_at: "2026-11-12T09:00:00Z" }])
      .call(
        2,
        "github.list_pull_requests",
        { repo: "acme/web", state: "closed", since: "7d" },
        6,
        18,
      )
      .after(380)
      .ok(2, [])
      .log("info", "summarising 1 merged pull requests", 9)
      .call(
        3,
        "llm.complete",
        { model: "claude-haiku-5-5", prompt: "Summarise these changes for the team:\n- Faster CI" },
        11,
        25,
      )
      .after(3_100)
      .ok(3, { text: "Faster CI this week." })
      .call(4, "slack.post_message", { channel: "#eng", text: "Faster CI this week." }, 16, 24)
      .after(150)
      .ok(4, { ok: true, ts: "1731390000.000300" })
      .complete({ prs: 1, ts: "1731390000.000300" });
  }

  // Runs started since the workflows have ids: their handles take their workflow's name, else
  // its short id (archi/08, W3).
  const fresh = new Set<RunFixture>();

  // onboarding: queued, no step committed yet.
  fresh.add(
    run(
      now - 40 * SECOND,
      ONBOARDING,
      SET_CRM,
      { agent: "support", conversation: "c-90", env: "prod" },
      { email: "max@example.org" },
      { namespace: "user_2fQ81" },
    ),
  );

  // chen's digest: running, its llm.complete delivered a third time after two transient failures.
  {
    const r = run(
      now - 26 * MINUTE,
      DIGEST,
      SET_DIGEST,
      { agent: "digest", schedule: "adhoc", env: "staging" },
      { repos: ["acme/web"], channel: "#eng-staging" },
    );
    r.history
      .after(20)
      .call(
        1,
        "github.list_pull_requests",
        { repo: "acme/web", state: "closed", since: "7d" },
        6,
        18,
      )
      .after(410)
      .ok(1, [{ number: 12, title: "Dark mode", merged_at: "2026-11-10T09:00:00Z" }])
      .log("info", "summarising 1 merged pull requests", 9)
      .call(
        2,
        "llm.complete",
        { model: "claude-haiku-5-5", prompt: "Summarise these changes for the team:\n- Dark mode" },
        11,
        25,
      );
    r.redeliveries = { 2: 2 };
    fresh.add(r);
  }

  // onboarding: waiting, its harness action's call parked: the action's schema changed.
  {
    const r = run(
      now - 3 * HOUR - 12 * MINUTE,
      ONBOARDING,
      SET_CRM,
      { agent: "support", conversation: "c-88", env: "prod" },
      { email: "ivy@example.org" },
      {
        namespace: "user_7Kd02",
        callStates: { 2: { state: "parked", parked: "schema_changed" } },
      },
    );
    r.history
      .after(15)
      .call(1, "crm.find_account", { email: "ivy@example.org" }, 5)
      .after(90)
      .ok(1, null)
      .call(2, "ask_user", { question: "No account for ivy@example.org. Create one?" }, 7, 26);
    fresh.add(r);
  }

  // chen's digest: live, for pnpm dev:mock. Its events arrive over the next minute.
  if (options.live) {
    const repos = ["acme/api", "acme/web", "acme/infra", "acme/docs", "acme/cli", "acme/sdk"];
    const r = run(
      now - 4 * SECOND,
      DIGEST,
      SET_DIGEST,
      { agent: "digest", schedule: "adhoc", env: "staging" },
      { repos, channel: "#eng-staging" },
    );
    fresh.add(r);
    const j = r.history;
    repos.forEach((repo, i) => {
      j.after(1_500)
        .call(i + 1, "github.list_pull_requests", { repo, state: "closed", since: "7d" }, 6, 18)
        .after(1_800 + 700 * (i % 3))
        .ok(i + 1, [
          { number: 10 + i, title: `Change in ${repo}`, merged_at: "2026-11-11T09:00:00Z" },
        ]);
    });
    j.after(400)
      .log("info", `summarising ${repos.length} merged pull requests`, 9)
      .call(
        repos.length + 1,
        "llm.complete",
        { model: "claude-haiku-5-5", prompt: "Summarise these changes for the team:\n…" },
        11,
        25,
      )
      .after(9_000)
      .ok(repos.length + 1, { text: "Six small changes across the repositories." })
      .call(
        repos.length + 2,
        "slack.post_message",
        { channel: "#eng-staging", text: "Six small changes across the repositories." },
        16,
        24,
      )
      .after(700)
      .ok(repos.length + 2, { ok: true, ts: "1731400000.000100" })
      .complete({ prs: repos.length, ts: "1731400000.000100" });
  }

  for (const r of runs) alignCallsites(r);
  const payloads = new Map<string, unknown>();
  for (const r of runs)
    for (const [digest, value] of r.history.payloads) payloads.set(digest, value);
  const access = createAccessWorld({
    requireKey: options.requireKey,
    ...(options.workos !== undefined ? { workos: options.workos } : {}),
    ...(options.tokenTtlMs !== undefined ? { tokenTtlMs: options.tokenTtlMs } : {}),
    ...(options.githubEmail !== undefined ? { githubEmail: options.githubEmail } : {}),
    ...(options.log ? { log: options.log } : {}),
  });
  for (const r of runs) {
    const used = secretsOf(r.code);
    if (Object.keys(used).length > 0) r.secrets = used;
  }

  const workflows = createWorkflowWorld({
    now,
    codes: {
      triageV1: TRIAGE_REPO_V1,
      triageV2: TRIAGE_REPO,
      triageV3: TRIAGE_REPO_V3,
      digest: DIGEST,
      forecast: FORECAST,
      onboarding: ONBOARDING,
      staleIssues: TRIAGE,
      staleIssuesFixed: TRIAGE_FIXED,
      devNotifications: DEV_NOTIFICATIONS,
      triageAll: TRIAGE_ALL,
      triageWorkflow: TRIAGE_WORKFLOW,
    },
    actionSets: ACTION_SETS,
    secretsOf,
    recorded: recordedRuns,
  });
  // Every run is a run of a workflow: its namespace's with its code, else the organization's.
  for (const r of runs) {
    const found = resolveByCode(workflows, r.namespace, r.code.digest);
    if (!found) throw new Error(`no workflow runs ${r.code.name} in ${r.namespace}`);
    r.workflow = runWorkflowOf(found.workflow, found.version);
    if (fresh.has(r)) r.origin ??= found.workflow.name ?? shortIdOf(found.workflow.id);
    else r.origin ??= legacyOrigin(r.code);
  }

  const world: World = {
    now,
    codes: Object.values(CODES),
    runs,
    payloads,
    embedded: options.embedded ?? false,
    access,
    namespaces: new Map([
      [access.tenant, createNamespaceWorld({ now, names: runs.map((r) => r.namespace) })],
    ]),
    workflows,
    connections: createConnectionWorld(access, {
      ...(options.mcp ? { mcp: options.mcp } : {}),
      ...(options.live ? { live: true } : {}),
    }),
    secrets: new Map([
      [
        access.tenant,
        createSecretWorld({
          now,
          runs: runs.map((r) => ({
            id: r.id,
            namespace: r.namespace,
            start: Date.parse(r.history.events[0]?.recordedAt ?? new Date(now).toISOString()),
            secrets: r.secrets ?? {},
          })),
        }),
      ],
    ]),
  };
  if (options.requireKey) world.requireKey = options.requireKey;
  ensureHandles(world);
  return world;
}

/**
 * The middle of the handle of a run stored before workflows had ids, as it was computed then and
 * kept since: the title a person gave its code, else its first comment line, as a slug; else
 * `script`.
 */
function legacyOrigin(c: CodeFixture): string {
  const titles = new Map([
    [TRIAGE.digest, "Triage stale issues"],
    [DIGEST.digest, "Daily digest"],
  ]);
  const title = titles.get(c.digest) ?? codeComment(c.source)?.title;
  return (title ? originSlug(title) : null) ?? "script";
}
