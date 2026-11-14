import { RewindConnectionError } from "../errors.js";
import { checkNamespaceName, type Transport } from "../http.js";
import type {
  CreateToken,
  CreateTokenKey,
  Credentials,
  Namespace,
  NamespaceDefaults,
  NamespaceDetail,
  NamespaceList,
  NamespaceOrder,
  PutNamespace,
  PutNamespaceDefaults,
  QuotaAlertList,
  RevokedTokens,
  Token,
  TokenKey,
} from "../types.js";
import type { RequestOptions } from "./toolsets.js";

export interface ListNamespacesParams extends RequestOptions {
  /** Only the names starting with this. */
  prefix?: string | undefined;
  /**
   * Only the namespaces that used at least this share of one of their limits this month: `0.8` for the end users at
   * 80 % or more of a quota.
   */
  above?: number | undefined;
  /** `cpu` (the default), `runs` or `scripts`: the largest use of the month first; or `name`. */
  order?: NamespaceOrder | undefined;
  /** The calendar month whose use is listed, `2026-11`. The current one by default. */
  month?: string | undefined;
  /** At most this many namespaces (50 by default, 500 at most). */
  limit?: number | undefined;
  /** The `next` cursor of the previous page. */
  after?: string | undefined;
}

export interface QuotaAlertsParams extends RequestOptions {
  /** The calendar month, `2026-11`. The current one by default. */
  month?: string | undefined;
  /** Only this namespace's alerts. Without it, every alert of the tenant, its ceiling's included. */
  namespace?: string | undefined;
}

/**
 * A public key to register (`CreateTokenKey`), its JWK as any object: `jose`'s `JWK`, WebCrypto's `JsonWebKey`, or
 * Node's `KeyObject.export({ format: "jwk" })`.
 */
export type NewTokenKey = Omit<CreateTokenKey, "jwk"> & { jwk: object };

/** An end user's opaque token, as minted: the answer holds its secret. */
export type CreatedToken = Token & {
  /** `rewind_nt_…`: give it to the agent acting for the end user (`new Rewind({ token })`). Only in this answer. */
  token: string;
};

/**
 * Namespaces: the end users of the customer. Each has its runs, the scripts they ran, its credentials, its use and its
 * limits; nothing of one end user is visible to another. A namespace is created at its first use (a run, a token), with
 * the tenant's default limits.
 */
export class Namespaces {
  /** End users' opaque tokens: minted for one namespace, revoked one by one or all at once. */
  readonly tokens: Tokens;
  /** The default limits of every namespace, and the tenant's ceiling. */
  readonly defaults: DefaultLimits;

  constructor(private readonly transport: Transport) {
    this.tokens = new Tokens(transport);
    this.defaults = new DefaultLimits(transport);
  }

  /**
   * A page of namespaces, each with its limits, its exceptions and its use of the month: the largest CPU time first
   * unless `order` says otherwise.
   */
  list(params: ListNamespacesParams = {}): Promise<NamespaceList> {
    const { signal } = params;
    return this.transport.send(
      () =>
        this.transport.api.GET("/v1/namespaces", {
          params: {
            query: {
              prefix: params.prefix,
              above: params.above,
              order: params.order,
              month: params.month,
              limit: params.limit,
              after: params.after,
            },
          },
          signal,
        }),
      { retry: true, signal },
    );
  }

  /** Every namespace matching the filter, page after page. */
  async *listAll(params: Omit<ListNamespacesParams, "after"> = {}): AsyncGenerator<Namespace> {
    let after: string | undefined;
    do {
      const page = await this.list({ ...params, after });
      yield* page.namespaces;
      after = page.next ?? undefined;
    } while (after !== undefined);
  }

  /** One namespace: its limits and exceptions, its use day by day over a month, its alerts, its tokens in use. */
  async get(name: string, options: RequestOptions & { month?: string | undefined } = {}): Promise<NamespaceDetail> {
    checkNamespaceName(name);
    return this.transport.send(
      () =>
        this.transport.api.GET("/v1/namespaces/{name}", {
          params: { path: { name }, query: { month: options.month } },
          signal: options.signal,
        }),
      { retry: true, signal: options.signal },
    );
  }

  /**
   * Creates the namespace ahead of its first use, or replaces its exceptions to the default limits: a paying end user,
   * or a blocked one (`{ exceptions: { activeRuns: 0 } }`). A quota absent from `exceptions` follows the default.
   */
  async put(name: string, namespace: PutNamespace = {}, options: RequestOptions = {}): Promise<NamespaceDetail> {
    checkNamespaceName(name);
    return this.transport.send(
      () =>
        this.transport.api.PUT("/v1/namespaces/{name}", {
          params: { path: { name } },
          body: namespace,
          signal: options.signal,
        }),
      { retry: true, signal: options.signal },
    );
  }

  /**
   * Erases the end user, in the background: the namespace takes no run and no token from now on
   * (`namespace-deleting`), its tokens are revoked and its active runs cancelled, then its runs, credentials, use and
   * alerts are erased. The tenant's own use keeps what the namespace consumed.
   */
  async delete(name: string, options: RequestOptions = {}): Promise<void> {
    checkNamespaceName(name);
    await this.transport.send(
      () =>
        this.transport.api.DELETE("/v1/namespaces/{name}", {
          params: { path: { name } },
          signal: options.signal,
        }),
      { retry: true, signal: options.signal },
    );
  }

  /**
   * Sets or renews the end user's own credentials for a toolset (Alice's GitHub token). A run uses its own
   * credentials if it has some, else its namespace's for its toolset, else the toolset's. Calls parked by
   * `credentials.expired` go on.
   */
  async setCredentials(
    name: string,
    toolset: string,
    credentials: Credentials,
    options: RequestOptions = {},
  ): Promise<void> {
    checkNamespaceName(name);
    await this.transport.send(
      () =>
        this.transport.api.PUT("/v1/namespaces/{name}/toolsets/{id}/credentials", {
          params: { path: { name, id: toolset } },
          body: credentials,
          signal: options.signal,
        }),
      { retry: true, signal: options.signal },
    );
  }

  /**
   * The quota alerts of a month, newest first: a limit crossed at 80 % or 100 % (for `active_runs`, the first refusal
   * of the month). An alert of the tenant's ceiling has no `namespace`. Each is also sent as the webhook `quota.alert`.
   */
  async alerts(params: QuotaAlertsParams = {}): Promise<QuotaAlertList> {
    const { signal } = params;
    if (params.namespace !== undefined) {
      checkNamespaceName(params.namespace);
    }
    return this.transport.send(
      () =>
        this.transport.api.GET("/v1/organization/quota-alerts", {
          params: { query: { month: params.month, namespace: params.namespace } },
          signal,
        }),
      { retry: true, signal },
    );
  }
}

/** The limits of every namespace without an exception, set by the customer, under the tenant's ceiling. */
export class DefaultLimits {
  constructor(private readonly transport: Transport) {}

  /** The default limits, the tenant's ceiling (set by the operator: the customer's offer) and the tenant's use. */
  get(options: RequestOptions & { month?: string | undefined } = {}): Promise<NamespaceDefaults> {
    return this.transport.send(
      () =>
        this.transport.api.GET("/v1/organization/namespace-defaults", {
          params: { query: { month: options.month } },
          signal: options.signal,
        }),
      { retry: true, signal: options.signal },
    );
  }

  /** Replaces the default limits: a quota absent from `defaults` does not limit, but the ceiling still does. */
  put(defaults: PutNamespaceDefaults, options: RequestOptions = {}): Promise<NamespaceDefaults> {
    return this.transport.send(
      () =>
        this.transport.api.PUT("/v1/organization/namespace-defaults", {
          body: defaults,
          signal: options.signal,
        }),
      { retry: true, signal: options.signal },
    );
  }
}

/**
 * End users' opaque tokens, `rewind_nt_…`. The agent acting for an end user holds one: with it, the agent and every
 * script its model writes reach that end user's namespace only.
 */
export class Tokens {
  constructor(private readonly transport: Transport) {}

  /**
   * Mints a token of the namespace, creating the namespace if needed. It lives `ttl` (1 hour by default, 30 days at
   * most), holds `permissions` (by default `runs:read`, `runs:start`, `runs:control` and `toolsets:read`; each must be
   * one the caller holds) and reaches only the runs carrying all its `labels`. The secret is only in this answer.
   */
  async create(namespace: string, token: CreateToken = {}, options: RequestOptions = {}): Promise<CreatedToken> {
    checkNamespaceName(namespace);
    const created = await this.transport.send(
      () =>
        this.transport.api.POST("/v1/namespaces/{name}/tokens", {
          params: { path: { name: namespace } },
          body: token,
          signal: options.signal,
        }),
      // Each request mints a token: never sent again.
      { retry: false, signal: options.signal },
    );
    if (typeof created.token !== "string" || created.token === "") {
      throw new RewindConnectionError("the server minted a token but its answer holds no secret");
    }
    return { ...created, token: created.token };
  }

  /** Revokes one opaque token, by its id (the SHA-256 of its secret). It is refused from the next request. */
  async revoke(id: string, options: RequestOptions = {}): Promise<void> {
    await this.transport.send(
      () => this.transport.api.DELETE("/v1/tokens/{id}", { params: { path: { id } }, signal: options.signal }),
      { retry: true, signal: options.signal },
    );
  }

  /**
   * Revokes every opaque token of the namespace, and refuses the signed tokens issued before now. An event stream
   * opened with one of them is closed.
   */
  async revokeAll(namespace: string, options: RequestOptions = {}): Promise<RevokedTokens> {
    checkNamespaceName(namespace);
    return this.transport.send(
      () =>
        this.transport.api.DELETE("/v1/namespaces/{name}/tokens", {
          params: { path: { name: namespace } },
          signal: options.signal,
        }),
      { retry: true, signal: options.signal },
    );
  }
}

/**
 * The public keys the customer signs end users' tokens with, without calling Rewind: a JWT whose header names the
 * key (`kid` `rk_…`), with the claims `sub` (the namespace), `aud` `rewind`, `iat` and `exp` (at most 30 days apart),
 * and optionally `permissions`, `labels` and `jti` (`SignedTokenClaims`).
 */
export class TokenKeys {
  constructor(private readonly transport: Transport) {}

  /**
   * Registers a public key, as a JWK: Ed25519 (`OKP`, signing with EdDSA), P-256 (`EC`, ES256) or RSA of 2048 bits or
   * more (RS256). A private part is refused. `permissions` are those its tokens may carry, among those the caller
   * holds. The answer names the key: its `kid`.
   */
  create(key: NewTokenKey, options: RequestOptions = {}): Promise<TokenKey> {
    const body: CreateTokenKey = { ...key, jwk: key.jwk as Record<string, unknown> };
    return this.transport.send(
      () => this.transport.api.POST("/v1/organization/token-keys", { body, signal: options.signal }),
      { retry: false, signal: options.signal },
    );
  }

  /** Every key the tenant signs its tokens with. */
  async list(options: RequestOptions = {}): Promise<TokenKey[]> {
    const list = await this.transport.send(
      () => this.transport.api.GET("/v1/organization/token-keys", { signal: options.signal }),
      { retry: true, signal: options.signal },
    );
    return list.keys;
  }

  /** Deletes a key: every token it signed is refused from the next request. */
  async delete(kid: string, options: RequestOptions = {}): Promise<void> {
    await this.transport.send(
      () =>
        this.transport.api.DELETE("/v1/organization/token-keys/{kid}", {
          params: { path: { kid } },
          signal: options.signal,
        }),
      { retry: true, signal: options.signal },
    );
  }
}
