import type { Diagnostic, Problem, QuerySpan, QuotaReached } from "./types.js";

/** The kinds of problem the API answers with: the slug of `urn:rewind:problem:<slug>`. */
export type ProblemSlug =
  | "invalid-request"
  | "invalid-query"
  | "unauthorized"
  | "forbidden"
  | "not-found"
  | "run-ended"
  | "not-quarantined"
  | "task-not-waiting"
  | "role-in-use"
  | "namespace-deleting"
  | "last-owner"
  | "task-gone"
  | "payload-too-large"
  | "unsupported-media-type"
  | "invalid-script"
  | "invalid-toolset"
  | "invalid-workflow"
  | "invalid-input"
  | "invalid-signal"
  | "quota-exceeded"
  | "internal"
  | "toolset-unavailable"
  | "identity-unavailable"
  | "unavailable";

const PROBLEM_PREFIX = "urn:rewind:problem:";

/** Every error the SDK throws. */
export class RewindError extends Error {
  override readonly name: string = "RewindError";
}

/** The API answered with an error (`application/problem+json`). */
export class RewindApiError extends RewindError {
  override readonly name: string = "RewindApiError";
  /** The HTTP status. */
  readonly status: number;
  /** The problem as the server sent it; `about:blank` when the body was not a problem. */
  readonly problem: Problem;
  /** The kind of problem, when the server named one of the known kinds. */
  readonly slug: ProblemSlug | undefined;

  constructor(problem: Problem) {
    super(problem.detail || problem.title);
    this.status = problem.status;
    this.problem = problem;
    this.slug = problem.type.startsWith(PROBLEM_PREFIX)
      ? (problem.type.slice(PROBLEM_PREFIX.length) as ProblemSlug)
      : undefined;
  }

  /** What went wrong, written for a model. */
  get detail(): string {
    return this.problem.detail;
  }

  /** With `invalid-script`: why the script was refused. Empty otherwise. */
  get diagnostics(): Diagnostic[] {
    return this.problem.diagnostics ?? [];
  }

  /** With `invalid-query`: where the term at fault is in the query, in characters. */
  get query(): QuerySpan | undefined {
    return this.problem.query ?? undefined;
  }

  /**
   * With `quota-exceeded`: the quota reached, at which scope (`namespace` or `organization`), its limit, the use, and
   * when a monthly quota frees (`resetsAt`).
   */
  get quota(): QuotaReached | undefined {
    const { quota, scope, limit, used, resetsAt } = this.problem;
    if (quota === undefined || scope === undefined || limit === undefined || used === undefined) {
      return undefined;
    }
    return { quota, scope, limit, used, ...(resetsAt === undefined ? {} : { resetsAt }) };
  }
}

/**
 * A key's request that creates in a namespace (a run, a check, a test) named none: the server's 400
 * (`invalid-request`). Every run belongs to an end user: send it from a client bound to the end user's namespace,
 * `rewind.namespace(name)`, or from a client holding the end user's token, `new Rewind({ token })`.
 */
export class NamespaceRequiredError extends RewindApiError {
  override readonly name: string = "NamespaceRequiredError";

  constructor(problem: Problem) {
    super(problem);
    this.message =
      `${problem.detail.replace(/[.\s]+$/, "")}. This client names no namespace: ` +
      "start runs, checks and tests with rewind.namespace(endUserId), or with the end user's token (new Rewind({ token }))";
  }
}

/** The server could not be reached, or its answer could not be read. */
export class RewindConnectionError extends RewindError {
  override readonly name = "RewindConnectionError";
}

/** Whether an error is an API problem of this kind. */
export function isProblem(error: unknown, slug: ProblemSlug): error is RewindApiError {
  return error instanceof RewindApiError && error.slug === slug;
}

/**
 * The problem of an error answer. A body that is not a problem reads as `about:blank`, with
 * the body as its detail.
 */
export function problemOf(status: number, statusText: string, body: unknown): Problem {
  if (isProblemBody(body)) {
    return body;
  }
  const text = typeof body === "string" ? body : body === undefined ? "" : JSON.stringify(body);
  return {
    type: "about:blank",
    title: statusText || `HTTP ${status}`,
    status,
    detail: text.trim() || statusText || `HTTP ${status}`,
  };
}

function isProblemBody(body: unknown): body is Problem {
  if (typeof body !== "object" || body === null) {
    return false;
  }
  const candidate = body as Record<string, unknown>;
  return (
    typeof candidate["type"] === "string" &&
    typeof candidate["title"] === "string" &&
    typeof candidate["status"] === "number" &&
    typeof candidate["detail"] === "string"
  );
}
