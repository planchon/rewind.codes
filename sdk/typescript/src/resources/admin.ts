import type { Transport } from "../http.js";
import { timeParam, type RequestOptions } from "../params.js";
import type { ActionStatsPage, ApiKey, CreateApiKey, CreateSubscription, ServerInfo, Subscription } from "../types.js";

/** Subscriptions to the signed webhooks Rewind sends. */
export class Subscriptions {
  constructor(private readonly transport: Transport) {}

  /**
   * Subscribes `url` to webhooks of the `types` it lists (every type when empty), about the runs carrying its `labels`.
   * The answer holds the signing secret `whsec_…`, once: keep it.
   */
  create(subscription: CreateSubscription, options: RequestOptions = {}): Promise<Subscription> {
    return this.transport.send(
      () => this.transport.api.POST("/v1/subscriptions", { body: subscription, signal: options.signal }),
      { retry: false, signal: options.signal },
    );
  }

  /** Every subscription of the organization, without secrets. */
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
  /** The run whose history holds the payload: a payload belongs to its run, and is read through it. */
  run: string;
}

/** Payloads by digest: the values left out of run states, streams and webhooks above 256 KiB. */
export class Payloads {
  constructor(private readonly transport: Transport) {}

  /** The JSON value of a payload, read through the run that holds it. */
  get(digest: string, options: PayloadOptions): Promise<unknown> {
    return this.transport.send(
      () =>
        this.transport.api.GET("/v1/payloads/{digest}", {
          params: { path: { digest }, query: { run: options.run } },
          signal: options.signal,
        }),
      { retry: true, signal: options.signal },
    );
  }
}

export interface ActionStatsParams extends RequestOptions {
  /**
   * The scope, as `runs.stats` takes it: one term `namespace:<name>`, or nothing for every namespace the client
   * reaches. Any other term is refused with `invalid-query`.
   */
  q?: string | undefined;
  /** Only the actions of this group: an MCP server, an API or a set of harness actions (`github`). */
  group?: string | undefined;
  /** Only the calls of runs of this action set. */
  actionSet?: string | undefined;
  /** Only this action (`github.list_issues`). */
  action?: string | undefined;
  /** Rounded down to the hour; 24 hours ago by default. */
  since?: Date | string | undefined;
  /** Rounded up to the hour; now by default. */
  until?: Date | string | undefined;
}

/** Statistics of the calls of actions. */
export class Actions {
  constructor(private readonly transport: Transport) {}

  /** Calls, latency, errors and redeliveries per action, over a window of whole hours, from hourly counters. */
  stats(params: ActionStatsParams = {}): Promise<ActionStatsPage> {
    const { signal } = params;
    return this.transport.send(
      () =>
        this.transport.api.GET("/v1/actions/stats", {
          params: {
            query: {
              q: params.q,
              group: params.group,
              actionSet: params.actionSet,
              action: params.action,
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

/** API keys, for a caller holding `api_keys:read` and `api_keys:write`. */
export class ApiKeys {
  constructor(private readonly transport: Transport) {}

  /**
   * Creates a key `rewind_sk_…` with a name, a role and allowed labels; the operator may name another organization
   * (`organizationId`). The answer holds the key, once.
   */
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

  /** The server's version and determinism epochs, the node that answered, and the deployment's partitions and nodes. */
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
