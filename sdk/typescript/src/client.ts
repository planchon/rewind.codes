import { AgentTools, type AgentToolsOptions } from "./agent-tools/agent-tools.js";
import { Approvals } from "./approvals.js";
import { RewindError } from "./errors.js";
import { Transport, type TransportOptions } from "./http.js";
import { ApiKeys, Payloads, Server, Subscriptions, Tools } from "./resources/admin.js";
import { Runs } from "./resources/runs.js";
import { Scripts } from "./resources/scripts.js";
import { Tasks } from "./resources/tasks.js";
import { Toolsets } from "./resources/toolsets.js";
import { Workflows } from "./resources/workflows.js";
import type { WebhookEvent } from "./types.js";
import { verifyWebhook, type WebhookHeaders } from "./webhooks.js";
import { HostToolWorker, type WorkerOptions } from "./worker.js";

export interface RewindOptions extends Partial<TransportOptions> {
  /** The server's base URL. `REWIND_URL` by default. */
  url?: string | undefined;
  /** The API key, `rewind_sk_…`. `REWIND_API_KEY` by default. */
  apiKey?: string | undefined;
}

/** Verifying the lifecycle webhooks the server sends. */
export interface Webhooks {
  /**
   * Verifies a delivery (Standard Webhooks, HMAC-SHA256, 5 minutes of tolerance) and returns
   * its event. Pass the body as received, and the subscription's secret `whsec_…`.
   */
  verify(payload: string | Uint8Array, headers: WebhookHeaders, secret: string): WebhookEvent;
}

/**
 * The client of an Rewind server: its API `/v1`, the host-tool worker and the agent tools.
 *
 * ```ts
 * const rewind = new Rewind({ url: process.env.REWIND_URL, apiKey: process.env.REWIND_API_KEY });
 * const toolset = await rewind.toolsets.create({ mcp: [{ name: "github", url: "https://mcp.example.com/github" }] });
 * const run = await rewind.runs.start({ toolset: toolset.id, script, wait: "30s" });
 * ```
 */
export class Rewind {
  readonly toolsets: Toolsets;
  readonly workflows: Workflows;
  readonly scripts: Scripts;
  readonly runs: Runs;
  readonly tasks: Tasks;
  readonly approvals: Approvals;
  readonly payloads: Payloads;
  readonly subscriptions: Subscriptions;
  readonly webhooks: Webhooks;
  readonly tools: Tools;
  readonly apiKeys: ApiKeys;
  readonly server: Server;

  private readonly transport: Transport;

  constructor(options: RewindOptions = {}) {
    const url = options.url ?? env("REWIND_URL");
    if (url === undefined || url === "") {
      throw new RewindError("no Rewind server URL: pass `url`, or set REWIND_URL");
    }
    this.transport = new Transport({ ...options, url, apiKey: options.apiKey ?? env("REWIND_API_KEY") });
    this.toolsets = new Toolsets(this.transport);
    this.workflows = new Workflows(this.transport);
    this.scripts = new Scripts(this.transport);
    this.runs = new Runs(this.transport);
    this.tasks = new Tasks(this.transport);
    this.payloads = new Payloads(this.transport);
    this.approvals = new Approvals(this.tasks, this.payloads);
    this.subscriptions = new Subscriptions(this.transport);
    this.webhooks = { verify: verifyWebhook };
    this.tools = new Tools(this.transport);
    this.apiKeys = new ApiKeys(this.transport);
    this.server = new Server(this.transport);
  }

  /** The server's base URL. */
  get url(): string {
    return this.transport.url;
  }

  /**
   * Starts a worker executing these host tools: it long-polls their tasks, runs the handlers,
   * keeps the claims alive and answers. Stop it with `worker.stop()`.
   */
  worker(options: WorkerOptions): HostToolWorker {
    return new HostToolWorker(this.tasks, options);
  }

  /** The agent tools for a model, scoped to a toolset and to labels (a conversation, an agent). */
  agentTools(options: AgentToolsOptions = {}): AgentTools {
    return new AgentTools(this, options);
  }
}

function env(name: string): string | undefined {
  const process = (globalThis as { process?: { env?: Record<string, string | undefined> } }).process;
  return process?.env?.[name];
}
