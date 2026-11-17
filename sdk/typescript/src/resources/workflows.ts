import type { Transport } from "../http.js";
import type { CreateWorkflow, RecordedTests, RecordTests, Workflow, WorkflowPage, WorkflowVersions } from "../types.js";
import type { RequestOptions } from "./toolsets.js";

export interface ListWorkflowsParams extends RequestOptions {
  /** The workflows whose name comes after this one: the `next` of the previous page. */
  after?: string | undefined;
  /** At most this many workflows (50 by default, 500 at most). */
  limit?: number | undefined;
  /**
   * The digest of a script: every version of any workflow whose script it is, newest first, on one page, rather than
   * the latest version of each workflow. `after` and `limit` do not apply.
   */
  script?: string | undefined;
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

/** Workflows: named scripts with typed inputs, outputs and signals, that scripts start and signal. */
export class Workflows {
  constructor(private readonly transport: Transport) {}

  /**
   * Checks a workflow against its toolset and its own schemas, runs its suite, and saves it as a new version, the
   * workflow's latest. Saving the same workflow again returns the version saved first.
   *
   * The suite is the parent's tests (`parent`, or the latest version of the same name the caller sees), minus those
   * `retire` names, with those `update` names replaced, plus `tests`; `retire` and `update` give a reason, kept in the
   * lineage. Every test must pass: there is no way around it.
   *
   * Refusals, each a `RewindApiError`: a script that does not check is `invalid-script` (with `diagnostics`); a name,
   * a signal's name or a schema that cannot be used, `invalid-workflow`; a test or a suite that cannot be read (an
   * unknown digest, a retired or updated test that is not inherited, two tests of the same name), `invalid-test`;
   * tests that did not pass, `tests-failed`, whose `tests` holds each test's outcome. Nothing is saved then.
   */
  create(workflow: CreateWorkflow, options: RequestOptions = {}): Promise<Workflow> {
    return this.transport.send(
      () => this.transport.api.POST("/v1/workflows", { body: workflow, signal: options.signal }),
      { retry: true, signal: options.signal },
    );
  }

  /** A page of workflows, by name: the latest version of each; with `script`, the versions whose script it is. */
  list(params: ListWorkflowsParams = {}): Promise<WorkflowPage> {
    const { signal } = params;
    return this.transport.send(
      () =>
        this.transport.api.GET("/v1/workflows", {
          params: { query: { after: params.after, limit: params.limit, script: params.script } },
          signal,
        }),
      { retry: true, signal },
    );
  }

  /** Every workflow, by name, page after page: the latest version of each. */
  async *listAll(params: Omit<ListWorkflowsParams, "after"> = {}): AsyncGenerator<Workflow> {
    let after: string | undefined;
    do {
      const page = await this.list({ ...params, after });
      yield* page.workflows;
      after = page.next ?? undefined;
    } while (after !== undefined);
  }

  /**
   * Records tests from the workflow's past runs: its completed and failed runs that `runs` names or `query` selects,
   * grouped by the path they took (the tools called in order, the signals waited for, how the run ended), the newest
   * run of each path recorded as a test, up to `limit`. Each test holds the run's input, start, seed, calls and
   * signals (a cassette matched by args), and expects its status, its output or error, and its calls per tool.
   *
   * The tests are stored and join no suite: give their digests in `create`'s `tests` (or `{ from: digest, expect }`
   * for a failed run, with the expectation it should have met). A run whose test would be larger than 4 MiB is
   * skipped, and named in `skipped`. A query that cannot be read is `invalid-query`; a name no version has,
   * `not-found`.
   */
  record(name: string, params: RecordTestsParams = {}): Promise<RecordedTests> {
    const { signal, query, runs, limit } = params;
    const body: RecordTests = {
      ...(query === undefined ? {} : { query }),
      ...(runs === undefined || runs.length === 0 ? {} : { runs }),
      ...(limit === undefined ? {} : { limit }),
    };
    return this.transport.send(
      () =>
        this.transport.api.POST("/v1/workflows/{name}/record", {
          params: { path: { name } },
          body,
          signal,
        }),
      { retry: false, signal },
    );
  }

  /**
   * The versions of a workflow, newest first. Each holds its `parent`, its suite (`tests`, by name and digest) and the
   * parent's tests it `retired` and `updated`, with their reasons. A name no version has is `not-found`.
   */
  async versions(name: string, options: RequestOptions = {}): Promise<Workflow[]> {
    const found: WorkflowVersions = await this.transport.send(
      () =>
        this.transport.api.GET("/v1/workflows/{name}", {
          params: { path: { name } },
          signal: options.signal,
        }),
      { retry: true, signal: options.signal },
    );
    return found.versions;
  }
}
