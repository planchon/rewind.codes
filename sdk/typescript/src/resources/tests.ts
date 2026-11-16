import type { Transport } from "../http.js";
import type { Test } from "../types.js";
import type { RequestOptions } from "./toolsets.js";

/** Stored tests: those of the workflows' suites, and those recorded from runs. */
export class Tests {
  constructor(private readonly transport: Transport) {}

  /**
   * A stored test, by its digest (a suite's `tests[].digest`, or a digest `workflows.record` returned). A test recorded
   * from a run holds that run's data: it is read like the run, so a caller who may not read the runs of its namespace
   * is refused (`forbidden`, or `not-found` for a client bound to another namespace).
   */
  get(digest: string, options: RequestOptions = {}): Promise<Test> {
    return this.transport.send(
      () =>
        this.transport.api.GET("/v1/tests/{digest}", {
          params: { path: { digest } },
          signal: options.signal,
        }),
      { retry: true, signal: options.signal },
    );
  }
}
