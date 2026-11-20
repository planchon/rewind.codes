// The dashboard's client of the API: `/v1`, typed by the generated `paths` and authenticated
// with a Bearer token (the operator's key, or a member's access token), and the sign-in routes
// `/auth`, which read and write the session cookie and never see a token. It reads every namespace
// of the organization, and names one (`Rewind-Namespace`) only for the scripts of one end user.
import createClient from "openapi-fetch";
import type {
  ApiKey,
  AuthConfig,
  AuthState,
  CreateApiKey,
  CreateInvitation,
  Entry,
  FailureGroupPage,
  FailuresQuery,
  Grant,
  Identity,
  Invitation,
  ListNamespacesQuery,
  ListRunsQuery,
  ListScriptsQuery,
  ListWorkflowsQuery,
  Member,
  NamespaceDefaults,
  NamespaceDetail,
  NamespaceList,
  NamespaceNames,
  NamespacePatch,
  Organization,
  OrganizationSettings,
  PermissionInfo,
  Problem,
  PutNamespace,
  PutNamespaceDefaults,
  PutRole,
  paths,
  QuotaAlertList,
  QuotaAlertsQuery,
  RevokedTokens,
  Role,
  Run,
  RunPage,
  RunStats,
  RunStatsQuery,
  Script,
  ScriptPage,
  ServerInfo,
  SetGrant,
  Test,
  ToolStatsPage,
  ToolStatsQuery,
  ToolsetSummary,
  Trace,
  UpdateOrganization,
  Workflow,
  WorkflowPage,
} from "./types";

const PROBLEM_PREFIX = "urn:rewind:problem:";

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

async function errorOf(response: Response): Promise<ApiError> {
  const text = await response.text().catch(() => "");
  let body: unknown = text;
  try {
    body = JSON.parse(text);
  } catch {
    // Not JSON: keep the text.
  }
  return new ApiError(response.status, toProblem(response.status, body, response.statusText));
}

export interface ApiOptions {
  /** The origin serving the API, such as `https://rewind.example.com`. */
  baseUrl: string;
  /** The Bearer token for `/v1`: an API key `rewind_sk_…`, an access token, or `null` for none. */
  token: () => string | null;
  /**
   * Called when `/v1` refuses the token (401): renews it, and answers whether the request is
   * worth sending again, once. A key cannot be renewed: leave it out.
   */
  renew?: () => Promise<boolean>;
  fetch?: (request: Request) => Promise<Response>;
}

type Client = ReturnType<typeof createClient<paths>>;

interface Answer<T> {
  data?: T;
  error?: unknown;
  response: Response;
}

/** An `AuthState`, with how far the server's clock is ahead of the browser's. */
export interface AuthAnswer {
  state: AuthState;
  /** The server's clock minus the browser's, from the answer's `Date` header; 0 without one. */
  skewMs: number;
}

function skewOf(response: Response): number {
  const date = response.headers.get("Date");
  const at = date ? Date.parse(date) : Number.NaN;
  return Number.isFinite(at) ? at - Date.now() : 0;
}

/** Whether a request goes to `/v1`, the only routes that take the Bearer token. */
function isV1(url: string): boolean {
  return new URL(url).pathname.startsWith("/v1/");
}

export class Api {
  readonly baseUrl: string;
  readonly client: Client;
  private readonly token: () => string | null;
  private readonly renew: (() => Promise<boolean>) | undefined;
  private readonly doFetch: (request: Request) => Promise<Response>;

  constructor(options: ApiOptions) {
    this.baseUrl = options.baseUrl.replace(/\/$/, "");
    this.token = options.token;
    this.renew = options.renew;
    this.doFetch = options.fetch ?? ((request) => globalThis.fetch(request));
    this.client = createClient<paths>({
      baseUrl: this.baseUrl,
      fetch: (request) => this.send(request),
    });
  }

  /** Adds the token to a request's headers. */
  authorize(headers: Headers): void {
    const token = this.token();
    if (token) headers.set("Authorization", `Bearer ${token}`);
  }

  private withToken(request: Request): Request {
    const headers = new Headers(request.headers);
    this.authorize(headers);
    return new Request(request, { headers });
  }

  /**
   * Sends a request. To `/v1`, with the token: a refused token is renewed once, and the request
   * sent again with the new one. To `/auth`, as it is: the session is the cookie's.
   */
  private async send(request: Request): Promise<Response> {
    if (!isV1(request.url)) return this.doFetch(request);
    const again = this.renew && this.token() !== null ? request.clone() : null;
    const response = await this.doFetch(this.withToken(request));
    if (response.status !== 401 || !again || !this.renew) return response;
    if (!(await this.renew())) return response;
    await response.body?.cancel().catch(() => undefined);
    return this.doFetch(this.withToken(again));
  }

  /** A request outside the typed client (the event stream, downloads), authenticated. */
  async raw(path: string, init: RequestInit = {}): Promise<Response> {
    const response = await this.send(new Request(`${this.baseUrl}${path}`, init));
    if (!response.ok) throw await errorOf(response);
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

  /** The scripts the runs ran: of one end user's runs when `namespace` names one. */
  async scripts(
    query: ListScriptsQuery,
    signal?: AbortSignal,
    namespace: string | null = null,
  ): Promise<ScriptPage> {
    return this.unwrap(
      await this.client.GET("/v1/scripts", {
        params: { query },
        ...(namespace !== null ? { headers: { "Rewind-Namespace": namespace } } : {}),
        signal,
      }),
    );
  }

  async script(digest: string, signal?: AbortSignal): Promise<Script> {
    return this.unwrap(
      await this.client.GET("/v1/scripts/{digest}", { params: { path: { digest } }, signal }),
    );
  }

  /** The latest version of each workflow, by name; `next` reads the page after. */
  async workflows(query: ListWorkflowsQuery, signal?: AbortSignal): Promise<WorkflowPage> {
    return this.unwrap(await this.client.GET("/v1/workflows", { params: { query }, signal }));
  }

  /** A workflow's versions, newest first. */
  async workflowVersions(name: string, signal?: AbortSignal): Promise<Workflow[]> {
    return this.unwrap(
      await this.client.GET("/v1/workflows/{name}", { params: { path: { name } }, signal }),
    ).versions;
  }

  /**
   * A stored test. One recorded from a run holds that run's data and is read like it: `runs:read`
   * in its namespace, or 403.
   */
  async test(digest: string, signal?: AbortSignal): Promise<Test> {
    return this.unwrap(
      await this.client.GET("/v1/tests/{digest}", { params: { path: { digest } }, signal }),
    );
  }

  /** The toolsets the organization registered, without their schemas. */
  async toolsets(signal?: AbortSignal): Promise<ToolsetSummary[]> {
    return this.unwrap(await this.client.GET("/v1/toolsets", { signal })).toolsets;
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

  // Sign-in (`/auth`). Every POST sends JSON: the server refuses anything else (415).

  /** How to sign in; `null` when the server has no sign-in (404) and takes an API key. */
  async authConfig(): Promise<AuthConfig | null> {
    const answer = await this.client.GET("/auth/config");
    if (answer.response.status === 404) return null;
    return this.unwrap(answer);
  }

  private authState(answer: Answer<AuthState>): AuthAnswer {
    return { state: this.unwrap(answer), skewMs: skewOf(answer.response) };
  }

  /**
   * The session's state, its access token refreshed when less than a minute is left; with
   * `organizationId`, a switch to that organization.
   */
  async session(organizationId?: string): Promise<AuthAnswer> {
    return this.authState(
      await this.client.POST("/auth/session", {
        body: organizationId ? { organizationId } : {},
      }),
    );
  }

  /**
   * Sends a 6-digit code to the email: `null` once sent. While sign-ups are closed, an email
   * without an account gets none, and the answer is the step `sign_up_closed`.
   */
  async sendMagicCode(email: string, invitationToken: string | null): Promise<AuthAnswer | null> {
    const answer = await this.client.POST("/auth/magic-auth", {
      body: { email, ...(invitationToken ? { invitationToken } : {}) },
    });
    if (answer.response.status === 202) {
      this.unwrap(answer);
      return null;
    }
    return this.authState(answer);
  }

  async verifyMagicCode(
    email: string,
    code: string,
    invitationToken: string | null,
  ): Promise<AuthAnswer> {
    return this.authState(
      await this.client.POST("/auth/magic-auth/verify", {
        body: { email, code, ...(invitationToken ? { invitationToken } : {}) },
      }),
    );
  }

  async verifyEmail(code: string): Promise<AuthAnswer> {
    return this.authState(await this.client.POST("/auth/email-verification", { body: { code } }));
  }

  async selectOrganization(organizationId: string): Promise<AuthAnswer> {
    return this.authState(
      await this.client.POST("/auth/organization", { body: { organizationId } }),
    );
  }

  async joinWaitlist(email: string): Promise<void> {
    this.unwrap(await this.client.POST("/auth/waitlist", { body: { email } }));
  }

  /** Revokes the session and clears its cookie. A session already gone counts as signed out. */
  async signOut(): Promise<void> {
    const response = await this.doFetch(
      new Request(`${this.baseUrl}/auth/logout`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: "{}",
      }),
    );
    if (response.ok || response.status === 401) return;
    throw await errorOf(response);
  }

  /** Where the browser goes to sign in with GitHub. */
  githubUrl(invitationToken: string | null): string {
    const query = invitationToken ? `?invitationToken=${encodeURIComponent(invitationToken)}` : "";
    return `${this.baseUrl}/auth/github${query}`;
  }

  // The organization and access (`/v1`).

  async whoami(signal?: AbortSignal): Promise<Identity> {
    return this.unwrap(await this.client.GET("/v1/whoami", { signal }));
  }

  async createOrganization(name: string): Promise<Organization> {
    return this.unwrap(await this.client.POST("/v1/organizations", { body: { name } }));
  }

  async updateOrganization(body: UpdateOrganization): Promise<OrganizationSettings> {
    return this.unwrap(await this.client.PATCH("/v1/organization", { body }));
  }

  /** The pending invitations sent to the caller's email. */
  async myInvitations(signal?: AbortSignal): Promise<Invitation[]> {
    return this.unwrap(await this.client.GET("/v1/invitations", { signal })).invitations;
  }

  async acceptInvitation(id: string): Promise<Invitation> {
    return this.unwrap(
      await this.client.POST("/v1/invitations/{id}/accept", { params: { path: { id } } }),
    );
  }

  async members(signal?: AbortSignal): Promise<Member[]> {
    return this.unwrap(await this.client.GET("/v1/organization/members", { signal })).members;
  }

  async setGrant(user: string, body: SetGrant): Promise<Grant> {
    return this.unwrap(
      await this.client.PUT("/v1/organization/members/{user}/grant", {
        params: { path: { user } },
        body,
      }),
    );
  }

  async removeMember(user: string): Promise<void> {
    this.unwrap(
      await this.client.DELETE("/v1/organization/members/{user}", { params: { path: { user } } }),
    );
  }

  async invitations(signal?: AbortSignal): Promise<Invitation[]> {
    return this.unwrap(await this.client.GET("/v1/organization/invitations", { signal }))
      .invitations;
  }

  async invite(body: CreateInvitation): Promise<Invitation> {
    return this.unwrap(await this.client.POST("/v1/organization/invitations", { body }));
  }

  async resendInvitation(id: string): Promise<Invitation> {
    return this.unwrap(
      await this.client.POST("/v1/organization/invitations/{id}/resend", {
        params: { path: { id } },
      }),
    );
  }

  async revokeInvitation(id: string): Promise<void> {
    this.unwrap(
      await this.client.DELETE("/v1/organization/invitations/{id}", { params: { path: { id } } }),
    );
  }

  async permissions(signal?: AbortSignal): Promise<PermissionInfo[]> {
    return this.unwrap(await this.client.GET("/v1/permissions", { signal })).permissions;
  }

  async roles(signal?: AbortSignal): Promise<Role[]> {
    return this.unwrap(await this.client.GET("/v1/roles", { signal })).roles;
  }

  async putRole(name: string, body: PutRole): Promise<Role> {
    return this.unwrap(
      await this.client.PUT("/v1/roles/{name}", { params: { path: { name } }, body }),
    );
  }

  async deleteRole(name: string): Promise<void> {
    this.unwrap(await this.client.DELETE("/v1/roles/{name}", { params: { path: { name } } }));
  }

  async apiKeys(signal?: AbortSignal): Promise<ApiKey[]> {
    return this.unwrap(await this.client.GET("/v1/api-keys", { signal })).keys;
  }

  /** Creates a key: the answer holds its secret, once. */
  async createApiKey(body: CreateApiKey): Promise<ApiKey> {
    return this.unwrap(await this.client.POST("/v1/api-keys", { body }));
  }

  async revokeApiKey(id: string): Promise<void> {
    this.unwrap(await this.client.DELETE("/v1/api-keys/{id}", { params: { path: { id } } }));
  }

  // Namespaces: the end users, their limits, use, alerts and tokens.

  async namespaces(query: ListNamespacesQuery, signal?: AbortSignal): Promise<NamespaceList> {
    return this.unwrap(await this.client.GET("/v1/namespaces", { params: { query }, signal }));
  }

  /** One namespace: its limits, its use day by day over the month, its alerts, its tokens. */
  async namespaceDetail(
    name: string,
    month: string | null,
    signal?: AbortSignal,
  ): Promise<NamespaceDetail> {
    return this.unwrap(
      await this.client.GET("/v1/namespaces/{name}", {
        params: { path: { name }, query: month ? { month } : {} },
        signal,
      }),
    );
  }

  /** Creates the namespace, or replaces its exceptions to the defaults. */
  async putNamespace(name: string, body: PutNamespace): Promise<NamespaceDetail> {
    return this.unwrap(
      await this.client.PUT("/v1/namespaces/{name}", { params: { path: { name } }, body }),
    );
  }

  /** Sets or removes the namespace's display name (`null` or `""`); never creates one. */
  async patchNamespace(name: string, body: NamespacePatch): Promise<NamespaceDetail> {
    return this.unwrap(
      await this.client.PATCH("/v1/namespaces/{name}", { params: { path: { name } }, body }),
    );
  }

  /** The namespaces that have a display name, by name: what every view shows for an id. */
  async namespaceNames(signal?: AbortSignal): Promise<NamespaceNames> {
    return this.unwrap(await this.client.GET("/v1/organization/namespace-names", { signal }));
  }

  /** Erases the end user in the background: its tokens, runs, credentials and use. */
  async deleteNamespace(name: string): Promise<void> {
    this.unwrap(await this.client.DELETE("/v1/namespaces/{name}", { params: { path: { name } } }));
  }

  /** Revokes the namespace's opaque tokens, and refuses its signed ones issued before now. */
  async revokeNamespaceTokens(name: string): Promise<RevokedTokens> {
    return this.unwrap(
      await this.client.DELETE("/v1/namespaces/{name}/tokens", { params: { path: { name } } }),
    );
  }

  /** The default limits, the organization's ceiling and its use of the month. */
  async namespaceDefaults(signal?: AbortSignal): Promise<NamespaceDefaults> {
    return this.unwrap(await this.client.GET("/v1/organization/namespace-defaults", { signal }));
  }

  async putNamespaceDefaults(body: PutNamespaceDefaults): Promise<NamespaceDefaults> {
    return this.unwrap(await this.client.PUT("/v1/organization/namespace-defaults", { body }));
  }

  /** The alerts of a month, of one namespace or of the whole organization, newest first. */
  async quotaAlerts(query: QuotaAlertsQuery, signal?: AbortSignal): Promise<QuotaAlertList> {
    return this.unwrap(
      await this.client.GET("/v1/organization/quota-alerts", { params: { query }, signal }),
    );
  }
}
