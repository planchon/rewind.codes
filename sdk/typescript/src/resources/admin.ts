import type { Transport } from "../http.js";
import { timeParam } from "../params.js";
import type { ApiKey, CreateApiKey, CreateSubscription, ServerInfo, Subscription, ToolStatsPage } from "../types.js";
import type { RequestOptions } from "./toolsets.js";

/** Subscriptions to the signed lifecycle webhooks. */
export class Subscriptions {
  constructor(private readonly transport: Transport) {}

  /** Subscribes `url` to lifecycle events. The answer holds the signing secret `whsec_…`, once: keep it. */
  create(subscription: CreateSubscription, options: RequestOptions = {}): Promise<Subscription> {
    return this.transport.send(
      () => this.transport.api.POST("/v1/subscriptions", { body: subscription, signal: options.signal }),
      { retry: false, signal: options.signal },
    );
  }

  /** Every subscription of the tenant, without secrets. */
  async list(options: RequestOptions = {}): Promise<Subscription[]> {
    const list = await this.transport.send(
      () => this.transport.api.GET("/v1/subscriptions", { signal: options.signal }),
      { retry: true, signal: options.signal },
    );
    return list.subscriptions;
  }

  async delete(id: string, options: RequestOptions = {}): Promise<void> {
    await this.transport.send(
      () =>
        this.transport.api.DELETE("/v1/subscriptions/{id}", {
          params: { path: { id } },
          signal: options.signal,
        }),
      { retry: true, signal: options.signal },
    );
  }
}

export interface PayloadOptions extends RequestOptions {
  /**
   * A run whose journal holds the payload. Required with a key limited to labels, which reads only
   * the payloads of the runs it sees.
   */
  run?: string | undefined;
}

/** Payloads by digest: the values left out of run states, streams and webhooks above 256 KiB. */
export class Payloads {
  constructor(private readonly transport: Transport) {}

  /** The JSON value of a payload, read through `run` when given. */
  get(digest: string, options: PayloadOptions = {}): Promise<unknown> {
    const query = options.run === undefined ? undefined : { run: options.run };
    return this.transport.send(
      () =>
        this.transport.api.GET("/v1/payloads/{digest}", {
          params: { path: { digest }, query },
          signal: options.signal,
        }),
      { retry: true, signal: options.signal },
    );
  }
}

export interface ToolStatsParams extends RequestOptions {
  toolset?: string | undefined;
  tool?: string | undefined;
  /** 24 hours ago by default. */
  since?: Date | string | undefined;
  /** Now by default. */
  until?: Date | string | undefined;
}

/** Statistics of tool calls. */
export class Tools {
  constructor(private readonly transport: Transport) {}

  /** Latency, errors and retries per tool, over a window. */
  stats(params: ToolStatsParams = {}): Promise<ToolStatsPage> {
    const { signal } = params;
    return this.transport.send(
      () =>
        this.transport.api.GET("/v1/tools/stats", {
          params: {
            query: {
              toolset: params.toolset,
              tool: params.tool,
              since: timeParam(params.since),
              until: timeParam(params.until),
            },
          },
          signal,
        }),
      { retry: true, signal },
    );
  }
}

/** API keys, for an `admin` key. */
export class ApiKeys {
  constructor(private readonly transport: Transport) {}

  /** Creates a key `rewind_sk_…` for a tenant. The answer holds the key, once. */
  create(key: CreateApiKey, options: RequestOptions = {}): Promise<ApiKey> {
    return this.transport.send(() => this.transport.api.POST("/v1/api-keys", { body: key, signal: options.signal }), {
      retry: false,
      signal: options.signal,
    });
  }

  async revoke(id: string, options: RequestOptions = {}): Promise<void> {
    await this.transport.send(
      () => this.transport.api.DELETE("/v1/api-keys/{id}", { params: { path: { id } }, signal: options.signal }),
      { retry: true, signal: options.signal },
    );
  }
}

/** The server itself. */
export class Server {
  constructor(private readonly transport: Transport) {}

  /** The server's version, determinism epochs and roles. */
  info(options: RequestOptions = {}): Promise<ServerInfo> {
    return this.transport.send(() => this.transport.api.GET("/v1/server", { signal: options.signal }), {
      retry: true,
      signal: options.signal,
    });
  }

  /** Stops the embedded server once its calls in flight are committed. Other servers refuse (403). */
  async stop(options: RequestOptions = {}): Promise<void> {
    await this.transport.send(() => this.transport.api.POST("/v1/server/stop", { signal: options.signal }), {
      retry: false,
      signal: options.signal,
    });
  }
}
