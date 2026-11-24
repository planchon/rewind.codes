import type { Transport } from "../http.js";
import { timeParam, type RequestOptions } from "../params.js";
import type {
  ChangeTests,
  CheckResult,
  CreateVersion,
  CreateWorkflow,
  PatchWorkflow,
  RecordedTests,
  RecordTests,
  TestReport,
  TestRequest,
  Workflow,
  WorkflowPage,
  WorkflowSummary,
} from "../types.js";

export interface CheckParams extends RequestOptions {
  /** The TypeScript source: a single ES module whose default export is the entry point. */
  code: string;
  /** The action set whose `rewind:actions` the code imports. Without one, `rewind:actions` is empty. */
  actionSet?: string | undefined;
}

export interface ListWorkflowsParams extends RequestOptions {
  /** Only the namespace's workflows, or only the organization's. Both by default. */
  scope?: "namespace" | "organization" | undefined;
  /** Only the workflows one of whose versions has this code: its digest (`Run.codeDigest`). */
  code?: string | undefined;
  /**
   * The scope of the counts, as `runs.stats` takes it: one term `namespace:<name>`, or nothing. Any other term is
   * refused with `invalid-query`: the counts come from hourly counters.
   */
  q?: string | undefined;
  /** Count the runs started at or after this time, rounded down to the hour. The last 30 days by default. */
  since?: Date | string | undefined;
  /** Count the runs started before this time, rounded up to the hour. Now by default. */
  until?: Date | string | undefined;
  /** The workflows whose id comes after this one: the `next` of the previous page. */
  after?: string | undefined;
  /** At most this many workflows (50 by default, 500 at most). */
  limit?: number | undefined;
}

export interface RecordTestsParams extends RequestOptions {
  /**
   * Which runs, in the query language of the run lists (`status:completed`, `@env:prod`…): among the completed and
   * failed runs of the workflow, of any version, that the caller sees, the 1,000 newest at most.
   */
  query?: string | undefined;
  /** These runs, by id, rather than a query. */
  runs?: string[] | undefined;
  /** At most this many tests, one per path: 20 by default, 100 at most. */
  limit?: number | undefined;
}

/**
 * Workflows: what the model writes and Rewind runs. A workflow has an id (`wf_…`), a name unique in its scope if it
 * has one, a description, versions numbered from 1 (each a code, its interface, its action set and its secrets) and a
 * suite of tests every new version passes. It belongs to the namespace whose run created it, or to the organization.
 *
 * Every method that names a workflow takes its id or its name (the namespace's first, then the organization's), with
 * `@v12` or `@<digest>` after it for one version where a version is read.
 */
export class Workflows {
  constructor(private readonly transport: Transport) {}

  /**
   * Type-checks code against the sandbox lib and the action set's declarations, under the limits of the client's
   * namespace (its longest code). Nothing runs and nothing is created; when the namespace, or the organization, already
   * has a workflow with that code, `workflow` names it and its version. A key's client without a namespace is refused
   * (`NamespaceRequiredError`).
   */
  check(params: CheckParams): Promise<CheckResult> {
    const { signal, ...body } = params;
    return this.transport.send(
      () => this.transport.api.POST("/v1/check", { params: { header: this.transport.namespaceHeader }, body, signal }),
      { retry: true, createsInNamespace: true, signal },
    );
  }

  /**
   * Runs tests as test runs, in virtual time, under the limits of the client's namespace. Each test gives the run's
   * world (its input, the actions' answers by rules, handlers or a cassette, its signals) and what must hold at the
   * end; a call the world does not answer fails the test. No action is really called.
   *
   * A test is given inline, by the digest of a stored test (one `record` recorded), or as `{ test: digest, expect }`.
   * With `workflow` (`"triage"`, `"triage@v12"` or `"triage@<digest>"`), that version's suite runs too, checked
   * against its interface, and `code` and `actionSet` default to the version's: give corrected `code`, or a new
   * `actionSet`, to check a candidate before it becomes a version. Nothing is created and no workflow changes.
   *
   * Returns each test's outcome, in the order given, then the suite's. A test refused before running (an invalid test,
   * a digest that does not exist) is `invalid-test`. A key's client without a namespace is refused
   * (`NamespaceRequiredError`).
   */
  test(request: TestRequest, options: RequestOptions = {}): Promise<TestReport> {
    return this.transport.send(
      () =>
        this.transport.api.POST("/v1/test", {
          params: { header: this.transport.namespaceHeader },
          body: request,
          signal: options.signal,
        }),
      { retry: true, createsInNamespace: true, signal: options.signal },
    );
  }

  /**
   * Creates a workflow and its version 1: the client's namespace's, or the organization's from a client without one.
   * Its code is checked against its action set and, when it has one, its interface (`input`, `output`, `signals`): its
   * default export must take the input and return the output, and `signal()` only takes the declared names. Its
   * `secrets` (`{ github: "ns/github", zendesk: "org/zendesk" }`) are part of the version: checked against the caller's
   * rights now, then read by its runs without checking their callers again. Its `tests` are its first suite, and each
   * passes against version 1 first. Needs `workflows:write`.
   *
   * Without a name, a code its scope already has answers that code's workflow, unchanged; a creation sent again answers
   * the workflow it created. Refusals, each a `RewindApiError`: a name its scope already has, `name-taken`; a name, a
   * signal's name or a schema that cannot be used, `invalid-workflow`; code that does not check, `invalid-code` (with
   * `diagnostics`); a secret the caller may not use, `secret-required` (with `secrets`); a test that cannot be read,
   * `invalid-test`; tests that did not pass, `tests-failed`, whose `tests` holds each test's outcome.
   */
  create(workflow: CreateWorkflow, options: RequestOptions = {}): Promise<Workflow> {
    return this.transport.send(
      () => this.transport.api.POST("/v1/workflows", { body: workflow, signal: options.signal }),
      { retry: true, signal: options.signal },
    );
  }

  /**
   * A page of the workflows the client sees, by id: its namespace's and the organization's, each with its latest
   * version and what its runs did in the window (their counts by status, the actions they called).
   */
  list(params: ListWorkflowsParams = {}): Promise<WorkflowPage> {
    const { signal } = params;
    return this.transport.send(
      () =>
        this.transport.api.GET("/v1/workflows", {
          params: {
            query: {
              scope: params.scope,
              code: params.code,
              q: params.q,
              since: timeParam(params.since),
              until: timeParam(params.until),
              after: params.after,
              limit: params.limit,
            },
          },
          signal,
        }),
      { retry: true, signal },
    );
  }

  /** Every workflow matching the filter, page after page. */
  async *listAll(params: Omit<ListWorkflowsParams, "after"> = {}): AsyncGenerator<WorkflowSummary> {
    let after: string | undefined;
    do {
      const page = await this.list({ ...params, after });
      yield* page.workflows;
      after = page.next ?? undefined;
    } while (after !== undefined);
  }

  /**
   * One workflow, by its id or its name: its scope, its `source` for a fork, its versions newest first (each with its
   * `version`, `digest`, interface, action set, `base`, the suite it passed and the tests it retired and replaced), its
   * suite with the reasons its tests left it, and the code of its latest version, or of the one `@v12` or `@<digest>`
   * names (`version`).
   */
  get(workflow: string, options: RequestOptions = {}): Promise<Workflow> {
    return this.transport.send(
      () =>
        this.transport.api.GET("/v1/workflows/{id}", {
          params: { path: { id: workflow } },
          signal: options.signal,
        }),
      { retry: true, signal: options.signal },
    );
  }

  /**
   * Names or describes a workflow, as a JSON merge patch: `name`, `description`, `null` to remove one. The workflow
   * keeps its id and versions; no version is made. A name its scope already has is `name-taken`, an invalid one
   * `invalid-workflow`. Needs `workflows:write`.
   */
  update(workflow: string, patch: PatchWorkflow, options: RequestOptions = {}): Promise<Workflow> {
    return this.transport.send(
      () =>
        this.transport.api.PATCH("/v1/workflows/{id}", {
          params: { path: { id: workflow } },
          body: patch,
          signal: options.signal,
        }),
      { retry: true, signal: options.signal },
    );
  }

  /**
   * Makes a workflow's next version through the gate: new `code`, an interface (`input`, `output`, `signals`), another
   * `actionSet` or other `secrets`; what is left out is its latest version's. Its suite (the workflow's, minus those
   * `retire` names, with those `replace` names replaced, plus `tests`) runs against the version, which is stored only
   * if every test passes; `retire` and `replace` give a reason, kept in the lineage. There is no way around the gate.
   * The same content without a change of tests makes no version: the latest is answered.
   *
   * Refusals, each a `RewindApiError`: tests that did not pass, `tests-failed`, whose `tests` holds each test's
   * outcome; a suite that cannot be read (an unknown digest, a retired or replaced test that is not in the suite, two
   * tests of the same name), `invalid-test`; code that does not check, `invalid-code`; a secret the caller may not
   * use, `secret-required`. Needs `workflows:write`.
   */
  createVersion(workflow: string, version: CreateVersion, options: RequestOptions = {}): Promise<Workflow> {
    return this.transport.send(
      () =>
        this.transport.api.POST("/v1/workflows/{id}/versions", {
          params: { path: { id: workflow } },
          body: version,
          signal: options.signal,
        }),
      { retry: true, signal: options.signal },
    );
  }

  /**
   * Adds tests to a workflow's suite, or replaces or retires some, with their reasons, without a version: each test
   * given and each replacement passes against its latest version first (`tests-failed` otherwise). Its next version
   * lists them. Needs `workflows:write`.
   */
  addTests(workflow: string, tests: ChangeTests, options: RequestOptions = {}): Promise<Workflow> {
    return this.transport.send(
      () =>
        this.transport.api.POST("/v1/workflows/{id}/tests", {
          params: { path: { id: workflow } },
          body: tests,
          signal: options.signal,
        }),
      { retry: false, signal: options.signal },
    );
  }

  /**
   * Moves a namespace's workflow to the organization, with the same id, versions and suite: every namespace's runs may
   * execute it from then on. An organization's workflow is answered as it is. A name the organization already has is
   * `name-taken`. Needs `workflows:write`.
   */
  promote(workflow: string, options: RequestOptions = {}): Promise<Workflow> {
    return this.transport.send(
      () =>
        this.transport.api.POST("/v1/workflows/{id}/promote", {
          params: { path: { id: workflow } },
          signal: options.signal,
        }),
      { retry: true, signal: options.signal },
    );
  }

  /**
   * Forks an organization's workflow into the client's namespace: a workflow of that namespace with the same name and
   * description, its `source` the organization's workflow and the version forked (its latest, or the one `@v12`
   * names), its version 1 that version, and a copy of its suite without the tests recorded from another namespace's
   * runs. The namespace's runs that name the workflow get the fork from then on. A namespace's workflow is
   * `not-forkable`; a namespace that has a workflow of that name, `name-taken`. Needs `workflows:write`, and a client
   * bound to a namespace (`NamespaceRequiredError` otherwise).
   */
  fork(workflow: string, options: RequestOptions = {}): Promise<Workflow> {
    return this.transport.send(
      () =>
        this.transport.api.POST("/v1/workflows/{id}/fork", {
          params: { path: { id: workflow } },
          signal: options.signal,
        }),
      { retry: false, createsInNamespace: true, signal: options.signal },
    );
  }

  /**
   * A fork takes its source's latest version (its code, interface, action set and secrets) as its next version, once
   * its own suite passes against it (`tests-failed` otherwise), and its `source` moves to it. Nothing to pull answers
   * the fork unchanged. A workflow that is not a fork is `not-a-fork`. Needs `workflows:write`.
   */
  pull(workflow: string, options: RequestOptions = {}): Promise<Workflow> {
    return this.transport.send(
      () =>
        this.transport.api.POST("/v1/workflows/{id}/pull", {
          params: { path: { id: workflow } },
          signal: options.signal,
        }),
      { retry: true, signal: options.signal },
    );
  }

  /**
   * Records tests from a workflow's past runs: its completed and failed runs that `runs` names or `query` selects,
   * grouped by the path they took (the actions called in order, the signals waited for, how the run ended), the newest
   * run of each path recorded as a test, up to `limit`. Each test holds the run's input, start, seed, calls and
   * signals, and expects its status, its output or error, and its calls per action; its `from` names the run.
   *
   * The tests are stored and join no suite: give their digests in `createVersion`'s or `addTests`'s `tests` (or
   * `{ test: digest, expect }` for a failed run, with the expectation it should have met). A run whose test would be
   * larger than 4 MiB is skipped, and named in `skipped`. A query that cannot be read is `invalid-query`.
   */
  record(workflow: string, params: RecordTestsParams = {}): Promise<RecordedTests> {
    const { signal, query, runs, limit } = params;
    const body: RecordTests = {
      ...(query === undefined ? {} : { query }),
      ...(runs === undefined || runs.length === 0 ? {} : { runs }),
      ...(limit === undefined ? {} : { limit }),
    };
    return this.transport.send(
      () =>
        this.transport.api.POST("/v1/workflows/{id}/record", {
          params: { path: { id: workflow } },
          body,
          signal,
        }),
      { retry: false, signal },
    );
  }
}
