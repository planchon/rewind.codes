import { AgentTools, type AgentToolsOptions } from "./agent-tools/agent-tools.js";
import { Approvals } from "./approvals.js";
import { RewindError } from "./errors.js";
import { Transport, type TransportOptions } from "./http.js";
import { ActionSets } from "./resources/action-sets.js";
import { Actions, ApiKeys, Payloads, Server, Subscriptions } from "./resources/admin.js";
import { Calls } from "./resources/calls.js";
import { Namespaces, TokenKeys } from "./resources/namespaces.js";
import { Runs } from "./resources/runs.js";
import { Secrets, SigningKeys } from "./resources/secrets.js";
import { Tests } from "./resources/tests.js";
import { Workflows } from "./resources/workflows.js";
import type { Webhook } from "./types.js";
import { verifyWebhook, type WebhookHeaders } from "./webhooks.js";
import { Worker, type WorkerOptions } from "./worker.js";

export interface RewindOptions extends Partial<TransportOptions> {
  /** The server's base URL. `REWIND_URL` by default. */
  url?: string | undefined;
  /** The API key, `rewind_sk_…`, which reaches every namespace. `REWIND_API_KEY` by default, unless `token` is given. */
  apiKey?: string | undefined;
  /**
   * An end user's token instead of a key: an opaque `rewind_nt_…` (`namespaces.tokens.create`), or a JWT the customer
   * signed with a key it registered. The client then reaches that end user's namespace only.
   */
  token?: string | undefined;
}

/** Verifying the webhooks the server sends. */
export interface Webhooks {
  /**
   * Verifies a delivery (Standard Webhooks, HMAC-SHA256, 5 minutes of tolerance) and returns
   * the webhook, `{ type, timestamp, data }`. Pass the body as received, and the subscription's secret `whsec_…`.
   */
  verify(payload: string | Uint8Array, headers: WebhookHeaders, secret: string): Webhook;
}

/**
 * The client of a Rewind server: its API `/v1`, the worker of harness actions and the agent tools.
 *
 * ```ts
 * const rewind = new Rewind({ url: process.env.REWIND_URL, apiKey: process.env.REWIND_API_KEY });
 * const actionSet = await rewind.actionSets.create({ mcp: [{ name: "github", url: "https://mcp.example.com/github" }] });
 * const run = await rewind.namespace(user.id).runs.start({ actionSet: actionSet.id, code, wait: "30s" });
 * ```
 */
export class Rewind {
  readonly actionSets: ActionSets;
  /** The workflows: checking and testing code, creating, naming, versioning, promoting, forking and pulling them. */
  readonly workflows: Workflows;
  /** The stored tests: those of the workflows' suites, and those recorded from runs. */
  readonly tests: Tests;
  readonly runs: Runs;
  /** The calls of harness actions at the level of the API, and the calls waiting for the harness's approval. */
  readonly calls: Calls;
  readonly approvals: Approvals;
  readonly payloads: Payloads;
  readonly subscriptions: Subscriptions;
  readonly webhooks: Webhooks;
  /** Statistics of the calls of actions. */
  readonly actions: Actions;
  readonly apiKeys: ApiKeys;
  /** The end users: their limits, use, alerts, secrets and tokens. */
  readonly namespaces: Namespaces;
  /**
   * The organization's secrets, used by runs as `org/<name>`. A namespace's own secrets, used as `ns/<name>`, are
   * `namespaces.secrets`.
   */
  readonly secrets: Secrets;
  /** The keys Rewind signs its calls with (`auth: { type: "rewind" }`), which `verifyRewindCall` checks. */
  readonly signingKeys: SigningKeys;
  /** The public keys the customer signs end users' tokens with. */
  readonly tokenKeys: TokenKeys;
  readonly server: Server;

  private readonly options: RewindOptions & { url: string };
  private readonly transport: Transport;

  constructor(options: RewindOptions = {}) {
    const url = options.url ?? env("REWIND_URL");
    if (url === undefined || url === "") {
      throw new RewindError("no Rewind server URL: pass `url`, or set REWIND_URL");
    }
    const apiKey = options.apiKey ?? (options.token === undefined ? env("REWIND_API_KEY") : undefined);
    this.options = { ...options, url, apiKey };
    this.transport = new Transport(this.options);
    this.actionSets = new ActionSets(this.transport);
    this.workflows = new Workflows(this.transport);
    this.tests = new Tests(this.transport);
    this.runs = new Runs(this.transport);
    this.calls = new Calls(this.transport);
    this.payloads = new Payloads(this.transport);
    this.approvals = new Approvals(this.calls, this.payloads);
    this.subscriptions = new Subscriptions(this.transport);
    this.webhooks = { verify: verifyWebhook };
    this.actions = new Actions(this.transport);
    this.apiKeys = new ApiKeys(this.transport);
    this.namespaces = new Namespaces(this.transport);
    this.secrets = new Secrets(this.transport, this.options.namespace);
    this.signingKeys = new SigningKeys(this.transport);
    this.tokenKeys = new TokenKeys(this.transport);
    this.server = new Server(this.transport);
  }

  /** The server's base URL. */
  get url(): string {
    return this.transport.url;
  }

  /**
   * The same client, acting for one end user: every request names its namespace (`Rewind-Namespace`). A key needs one
   * to start a run, check or test code, or fork a workflow; on a read, it narrows the answer to that namespace, and a
   * run of another is missing (404); a workflow's name resolves in it first. The namespace is created at its first
   * use, with the organization's default limits.
   *
   * ```ts
   * const run = await rewind.namespace(user.id).runs.start({ actionSet: actionSet.id, code, wait: "30s" });
   * const agentTools = rewind.namespace(user.id).agentTools({ actionSet: actionSet.id });
   * ```
   *
   * @param name The customer's own id for the end user: 1 to 128 ASCII letters, digits, `-`, `_`, `.`, `:` and `@`.
   *   An opaque id is better than an email: it appears in webhooks, the dashboard and the service's logs.
   */
  namespace(name: string): Rewind {
    return new Rewind({ ...this.options, namespace: name });
  }

  /**
   * Starts a worker executing these harness actions: it holds their call stream open, is handed each call as it
   * becomes ready, runs the handler and answers. Stop it with `worker.stop()`. Each call names the namespace of its
   * run, the end user the handler acts for (`ctx.namespace`). It is handed the calls of the runs its key sees: with a
   * key limited to labels (`{ env: "dev" }`), only those of the runs carrying them, so each environment of an
   * organization runs its own worker.
   */
  worker(options: WorkerOptions): Worker {
    return new Worker(this.calls, options);
  }

  /**
   * The agent tools for a model, scoped to an action set and to labels (a conversation, an agent). The model's runs
   * belong to the client's namespace: take them from `rewind.namespace(user.id)`, or from a client holding the end
   * user's token.
   */
  agentTools(options: AgentToolsOptions = {}): AgentTools {
    return new AgentTools(this, options);
  }
}

function env(name: string): string | undefined {
  const process = (globalThis as { process?: { env?: Record<string, string | undefined> } }).process;
  return process?.env?.[name];
}
