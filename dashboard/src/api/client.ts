// The dashboard's client of the API /v1: typed by the generated `paths`, authenticated with the
// operator's key, and read-only. Every call is a GET, so a `reader` key is enough.
import createClient from "openapi-fetch";
import type {
  Entry,
  FailureGroupPage,
  FailuresQuery,
  ListRunsQuery,
  ListScriptsQuery,
  Problem,
  paths,
  Run,
  RunPage,
  RunStats,
  RunStatsQuery,
  Script,
  ScriptPage,
  ServerInfo,
  ToolStatsPage,
  ToolStatsQuery,
  Trace,
} from "./types";

const PROBLEM_PREFIX = "urn:ptc:problem:";

/** An answer of the API that is not a success, read from its `application/problem+json`. */
export class ApiError extends Error {
  readonly status: number;
  readonly problem: Problem;

  constructor(status: number, problem: Problem) {
    super(problem.detail || problem.title);
    this.name = "ApiError";
    this.status = status;
    this.problem = problem;
  }

  /** `not-found`, `unauthorized`…; `about:blank` when the body was not a problem. */
  get slug(): string {
    const type = this.problem.type;
    return type.startsWith(PROBLEM_PREFIX) ? type.slice(PROBLEM_PREFIX.length) : type;
  }
}

/** The term at fault of a query the server could not read (`invalid-query`), if that is the error. */
export function queryFault(error: unknown): { message: string; start: number; end: number } | null {
  if (!(error instanceof ApiError) || error.slug !== "invalid-query") return null;
  const span = error.problem.query;
  return {
    message: error.problem.detail || error.problem.title,
    start: span?.start ?? 0,
    end: span?.end ?? 0,
  };
}

/** Reads a problem from an error body, whatever the server sent. */
export function toProblem(status: number, body: unknown, statusText = ""): Problem {
  if (body && typeof body === "object" && "type" in body && "title" in body) {
    const p = body as Partial<Problem>;
    return {
      type: String(p.type),
      title: String(p.title),
      status: typeof p.status === "number" ? p.status : status,
      detail: typeof p.detail === "string" ? p.detail : "",
      instance: p.instance ?? null,
      ...(p.query && typeof p.query.start === "number" && typeof p.query.end === "number"
        ? { query: { start: p.query.start, end: p.query.end } }
        : {}),
    };
  }
  const text = typeof body === "string" ? body.trim() : "";
  return {
    type: "about:blank",
    title: statusText || `HTTP ${status}`,
    status,
    detail: text.length > 0 && text.length < 500 ? text : `The server answered ${status}.`,
  };
}

export interface ApiOptions {
  /** The origin serving the API, such as `https://ptc.example.com`. */
  baseUrl: string;
  /** The operator's key, `ptc_sk_…`, or `null` to send none. */
  key: () => string | null;
  fetch?: (request: Request) => Promise<Response>;
}

type Client = ReturnType<typeof createClient<paths>>;

interface Answer<T> {
  data?: T;
  error?: unknown;
  response: Response;
}

export class Api {
  readonly baseUrl: string;
  readonly client: Client;
  private readonly key: () => string | null;
  private readonly doFetch: (request: Request) => Promise<Response>;

  constructor(options: ApiOptions) {
    this.baseUrl = options.baseUrl.replace(/\/$/, "");
    this.key = options.key;
    this.doFetch = options.fetch ?? ((request) => globalThis.fetch(request));
    this.client = createClient<paths>({
      baseUrl: this.baseUrl,
      fetch: (request) => this.doFetch(request),
    });
    this.client.use({
      onRequest: ({ request }) => {
        this.authorize(request.headers);
        return request;
      },
    });
  }

  /** Adds the key to a request's headers. */
  authorize(headers: Headers): void {
    const key = this.key();
    if (key) headers.set("Authorization", `Bearer ${key}`);
  }

  /** A request outside the typed client (the event stream, downloads), authenticated. */
  async raw(path: string, init: RequestInit = {}): Promise<Response> {
    const headers = new Headers(init.headers);
    this.authorize(headers);
    const response = await this.doFetch(
      new Request(`${this.baseUrl}${path}`, { ...init, headers }),
    );
    if (!response.ok) {
      const text = await response.text().catch(() => "");
      let body: unknown = text;
      try {
        body = JSON.parse(text);
      } catch {
        // Not JSON: keep the text.
      }
      throw new ApiError(response.status, toProblem(response.status, body, response.statusText));
    }
    return response;
  }

  private unwrap<T>(answer: Answer<T>): T {
    const { data, error, response } = answer;
    if (!response.ok || error !== undefined) {
      throw new ApiError(response.status, toProblem(response.status, error, response.statusText));
    }
    return data as T;
  }

  async server(): Promise<ServerInfo> {
    return this.unwrap(await this.client.GET("/v1/server"));
  }

  async runs(query: ListRunsQuery, signal?: AbortSignal): Promise<RunPage> {
    return this.unwrap(await this.client.GET("/v1/runs", { params: { query }, signal }));
  }

  async runStats(query: RunStatsQuery, signal?: AbortSignal): Promise<RunStats> {
    return this.unwrap(await this.client.GET("/v1/runs/stats", { params: { query }, signal }));
  }

  async failures(query: FailuresQuery, signal?: AbortSignal): Promise<FailureGroupPage> {
    return this.unwrap(await this.client.GET("/v1/runs/failures", { params: { query }, signal }));
  }

  async run(id: string, signal?: AbortSignal): Promise<Run> {
    return this.unwrap(
      await this.client.GET("/v1/runs/{id}", { params: { path: { id } }, signal }),
    );
  }

  async entry(id: string, idx: number, signal?: AbortSignal): Promise<Entry> {
    return this.unwrap(
      await this.client.GET("/v1/runs/{id}/entries/{idx}", {
        params: { path: { id, idx } },
        signal,
      }),
    );
  }

  async trace(id: string, signal?: AbortSignal): Promise<Trace> {
    return this.unwrap(
      await this.client.GET("/v1/runs/{id}/trace", {
        params: { path: { id }, query: { format: "llm" } },
        signal,
      }),
    );
  }

  async scripts(query: ListScriptsQuery, signal?: AbortSignal): Promise<ScriptPage> {
    return this.unwrap(await this.client.GET("/v1/scripts", { params: { query }, signal }));
  }

  async script(digest: string, signal?: AbortSignal): Promise<Script> {
    return this.unwrap(
      await this.client.GET("/v1/scripts/{digest}", { params: { path: { digest } }, signal }),
    );
  }

  async toolStats(query: ToolStatsQuery, signal?: AbortSignal): Promise<ToolStatsPage> {
    return this.unwrap(await this.client.GET("/v1/tools/stats", { params: { query }, signal }));
  }

  /** A payload's value, read through the run whose journal holds it: a key limited to labels must. */
  async payload(digest: string, run: string, signal?: AbortSignal): Promise<unknown> {
    return this.unwrap(
      await this.client.GET("/v1/payloads/{digest}", {
        params: { path: { digest }, query: { run } },
        signal,
      }),
    );
  }

  /** The journal document, or the cassette, as a file to save. */
  async journal(id: string, as: "journal" | "cassette"): Promise<Blob> {
    const response = await this.raw(`/v1/runs/${encodeURIComponent(id)}/journal?as=${as}`, {
      headers: { Accept: "application/json" },
    });
    return response.blob();
  }
}
