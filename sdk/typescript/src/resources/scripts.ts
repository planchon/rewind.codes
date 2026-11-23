import type { Transport } from "../http.js";
import { timeParam } from "../params.js";
import type { CheckResult, Script, ScriptPage, ScriptSummary, TestRequest, TestRun } from "../types.js";
import type { RequestOptions } from "./toolsets.js";

export interface CheckParams extends RequestOptions {
  /** The TypeScript source: a single ES module whose default export is the entry point. */
  script: string;
  /** The toolset whose `rewind:tools` the script imports. */
  toolset?: string | undefined;
}

export interface ScriptTitleParams extends RequestOptions {
  /**
   * The title, trimmed: 1 to 120 characters. `null` removes the title and the description a person gave: those of the
   * comment at the top of the source show again.
   */
  title: string | null;
  /** The description, trimmed: at most 2,000 characters. Absent, `null` or `""`: none. */
  description?: string | null | undefined;
}

export interface ListScriptsParams extends RequestOptions {
  /**
   * The scope, as `runs.stats` takes it: one term `namespace:<name>`, or nothing for every namespace the client
   * reaches. Any other term is refused with `invalid-query`: the list comes from hourly counters.
   */
  q?: string | undefined;
  /** Scripts of the runs started at or after this time, rounded down to the hour. No lower bound by default. */
  since?: Date | string | undefined;
  /** Scripts of the runs started before this time, rounded up to the hour. Now by default. */
  until?: Date | string | undefined;
  /** Only this script: its digest. */
  digest?: string | undefined;
  /** Only scripts that called this tool. */
  tool?: string | undefined;
  /** At most this many scripts (50 by default, 500 at most). */
  limit?: number | undefined;
  cursor?: string | undefined;
}

/** Scripts: type-checking, tests in mock mode, and the distinct scripts executed. */
export class Scripts {
  constructor(private readonly transport: Transport) {}

  /**
   * Type-checks a script against the sandbox lib and the toolset's declarations, under the limits of the client's
   * namespace (its longest script). Nothing runs. A key's client without a namespace is refused
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
   * Runs tests against a script in mock mode, in virtual time, under the limits of the client's namespace. Each test
   * gives the run's world (its input, the tools' answers by rules, mock handlers or a cassette, its signals) and what
   * must hold at the end; a call the world does not answer fails the test. No tool is really called.
   *
   * A test is given inline, by the digest of a stored test (one `workflows.record` recorded), or as
   * `{ from: digest, expect }`. With `workflow` (`"triage"`, `"triage@<version>"` or `"triage@v12"`), that version's
   * suite runs too, checked against its interface, and `script` and `toolset` default to the version's: give a
   * corrected `script`, or a new `toolset`, to check a candidate before it is saved. The tests are not stored.
   *
   * Returns each test's outcome, in the order given, then the suite's. A test refused before running (an invalid test,
   * a digest that does not exist) is `invalid-test`. A key's client without a namespace is refused
   * (`NamespaceRequiredError`).
   */
  test(request: TestRequest, options: RequestOptions = {}): Promise<TestRun> {
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
   * A page of the distinct scripts the runs of a scope ran in a window, most recently run first, each with its runs by
   * status and the tools it called.
   */
  list(params: ListScriptsParams = {}): Promise<ScriptPage> {
    const { signal } = params;
    return this.transport.send(
      () =>
        this.transport.api.GET("/v1/scripts", {
          params: {
            query: {
              q: params.q,
              since: timeParam(params.since),
              until: timeParam(params.until),
              digest: params.digest,
              tool: params.tool,
              limit: params.limit,
              cursor: params.cursor,
            },
          },
          signal,
        }),
      { retry: true, signal },
    );
  }

  /** Every script matching the filter, page after page. */
  async *listAll(params: Omit<ListScriptsParams, "cursor"> = {}): AsyncGenerator<ScriptSummary> {
    let cursor: string | undefined;
    do {
      const page = await this.list({ ...params, cursor });
      yield* page.scripts;
      cursor = page.nextCursor ?? undefined;
    } while (cursor !== undefined);
  }

  /**
   * One script's source, by its digest, with its title: the one a person gave it (`titleSource: "person"`), else the
   * first line of the comment at its top (`"comment"`).
   */
  get(digest: string, options: RequestOptions = {}): Promise<Script> {
    return this.transport.send(
      () =>
        this.transport.api.GET("/v1/scripts/{digest}", {
          params: { path: { digest } },
          signal: options.signal,
        }),
      { retry: true, signal: options.signal },
    );
  }

  /**
   * Gives the script a title and a description for the whole organization, over those of the comment at its top; the
   * source does not change. `title: null` removes them. A new run of the script takes its handle's origin from the
   * title. Needs `scripts:write`. A title out of bounds is refused with `invalid-title`; a script the caller cannot
   * read is `not-found`.
   */
  setTitle(digest: string, params: ScriptTitleParams): Promise<Script> {
    const { signal, title, description } = params;
    return this.transport.send(
      () =>
        this.transport.api.PATCH("/v1/scripts/{digest}", {
          params: { path: { digest } },
          body: { title, ...(description === undefined ? {} : { description }) },
          signal,
        }),
      { retry: true, signal },
    );
  }
}
