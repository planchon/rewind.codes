import type { Transport } from "../http.js";
import type { CreateWorkflow, Workflow, WorkflowPage, WorkflowVersions } from "../types.js";
import type { RequestOptions } from "./toolsets.js";

export interface ListWorkflowsParams extends RequestOptions {
  /** The workflows whose name comes after this one: the `next` of the previous page. */
  after?: string | undefined;
  /** At most this many workflows (50 by default, 500 at most). */
  limit?: number | undefined;
}

/** Workflows: named scripts with typed inputs, outputs and signals, that scripts start and signal. */
export class Workflows {
  constructor(private readonly transport: Transport) {}

  /**
   * Checks a workflow against its toolset and its own schemas, and saves it as a new version,
   * the workflow's latest. Saving the same workflow again returns the version saved first. A
   * script that does not check is refused with `invalid-script` (a `RewindApiError` with
   * `diagnostics`); a name, a signal's name or a schema that cannot be used, with
   * `invalid-workflow`.
   */
  create(workflow: CreateWorkflow, options: RequestOptions = {}): Promise<Workflow> {
    return this.transport.send(
      () => this.transport.api.POST("/v1/workflows", { body: workflow, signal: options.signal }),
      { retry: true, signal: options.signal },
    );
  }

  /** A page of workflows, by name: the latest version of each. */
  list(params: ListWorkflowsParams = {}): Promise<WorkflowPage> {
    const { signal } = params;
    return this.transport.send(
      () =>
        this.transport.api.GET("/v1/workflows", {
          params: { query: { after: params.after, limit: params.limit } },
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

  /** The versions of a workflow, newest first. A name no version has is `not-found`. */
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
