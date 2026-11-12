import { AgentTools, type AgentToolsOptions } from "./agent-tools/agent-tools.js";
import { Approvals } from "./approvals.js";
import { PtcError } from "./errors.js";
import { Transport, type TransportOptions } from "./http.js";
import { ApiKeys, Payloads, Server, Subscriptions, Tools } from "./resources/admin.js";
import { Runs } from "./resources/runs.js";
import { Scripts } from "./resources/scripts.js";
import { Tasks } from "./resources/tasks.js";
import { Toolsets } from "./resources/toolsets.js";
import type { WebhookEvent } from "./types.js";
import { verifyWebhook, type WebhookHeaders } from "./webhooks.js";
import { HostToolWorker, type WorkerOptions } from "./worker.js";

export interface PtcOptions extends Partial<TransportOptions> {
  /** The server's base URL. `PTC_URL` by default. */
  url?: string | undefined;
  /** The API key, `ptc_sk_…`. `PTC_API_KEY` by default. */
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
 * The client of an ai-ptc server: its API `/v1`, the host-tool worker and the agent tools.
 *
 * ```ts
 * const ptc = new Ptc({ url: process.env.PTC_URL, apiKey: process.env.PTC_API_KEY });
 * const toolset = await ptc.toolsets.create({ mcp: [{ name: "github", url: "https://mcp.example.com/github" }] });
 * const run = await ptc.runs.start({ toolset: toolset.id, script, wait: "30s" });
 * ```
 */
export class Ptc {
  readonly toolsets: Toolsets;
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

  constructor(options: PtcOptions = {}) {
    const url = options.url ?? env("PTC_URL");
    if (url === undefined || url === "") {
      throw new PtcError("no ai-ptc server URL: pass `url`, or set PTC_URL");
    }
    this.transport = new Transport({ ...options, url, apiKey: options.apiKey ?? env("PTC_API_KEY") });
    this.toolsets = new Toolsets(this.transport);
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
