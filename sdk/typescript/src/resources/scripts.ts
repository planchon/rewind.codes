import type { Transport } from "../http.js";
import { labelParams } from "../params.js";
import type { CheckResult, Labels, Script, ScriptPage, ScriptSummary, TestRequest, TestResult } from "../types.js";
import type { RequestOptions } from "./toolsets.js";

export interface CheckParams extends RequestOptions {
  /** The TypeScript source: a single ES module whose default export is the entry point. */
  script: string;
  /** The toolset whose `rewind:tools` the script imports. */
  toolset?: string | undefined;
}

export interface ListScriptsParams extends RequestOptions {
  /** Only scripts that ran with all these labels. */
  labels?: Labels | undefined;
  /** Only scripts that called this tool. */
  tool?: string | undefined;
  /** At most this many scripts (50 by default, 500 at most). */
  limit?: number | undefined;
  cursor?: string | undefined;
}

/** Scripts: type-checking, scenarios in mock mode, and the distinct scripts executed. */
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
   * Runs a scenario in mock mode, in virtual time, under the limits of the client's namespace: against a cassette,
   * mock handlers and static responses, and checks its expectations. No tool is really called. A key's client without
   * a namespace is refused (`NamespaceRequiredError`).
   */
  test(request: TestRequest, options: RequestOptions = {}): Promise<TestResult> {
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

  /** A page of the distinct scripts executed, most recently run first. */
  list(params: ListScriptsParams = {}): Promise<ScriptPage> {
    const { signal } = params;
    return this.transport.send(
      () =>
        this.transport.api.GET("/v1/scripts", {
          params: {
            query: {
              label: labelParams(params.labels),
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

  /** One script's source, by its digest. */
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
}
