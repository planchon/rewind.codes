import { RewindError } from "../errors.js";
import { checkNamespaceName, type Transport } from "../http.js";
import type { Jwks, PutSecret, Secret, SecretUsage, SigningKey, SigningKeyList } from "../types.js";
import type { RequestOptions } from "./toolsets.js";

/** A secret's name, as the server takes it: 1 to 64 lowercase letters, digits, `-` and `_`, starting with a letter. */
export function isSecretName(name: string): boolean {
  return /^[a-z][a-z0-9_-]{0,63}$/.test(name);
}

/** Refuses a name the server would refuse. */
function checkSecretName(name: string): void {
  if (!isSecretName(name)) {
    throw new RewindError(
      `invalid secret name ${JSON.stringify(name)}: expected 1 to 64 lowercase letters, digits, - and _, starting with a letter`,
    );
  }
}

/**
 * The organization's secrets: named values, encrypted at rest, that only the dispatcher reads, for the runs that
 * attach them (`secrets: { zendesk: "org/zendesk" }` at start). Write-only: no answer ever holds a value. A key limited
 * to labels sees, writes and attaches only the secrets carrying them, and the secrets it writes take them.
 */
export class Secrets {
  constructor(
    private readonly transport: Transport,
    /** The namespace the client acts for (`rewind.namespace(name)`), if any. */
    private readonly namespace?: string | undefined,
  ) {}

  /**
   * The organization's secrets the caller sees, by name: each one's labels, who wrote it and when, its reads per day
   * over 30 days and its last read; never a value.
   */
  async list(options: RequestOptions = {}): Promise<Secret[]> {
    const list = await this.transport.send(() => this.transport.api.GET("/v1/secrets", { signal: options.signal }), {
      retry: true,
      signal: options.signal,
    });
    return list.secrets;
  }

  /**
   * The secrets of the namespace the client acts for (`rewind.namespace(name)`), attached as `ns/<name>`; `undefined`
   * when the client names no namespace. A client holding an end user's token lists them with
   * `namespaces.secrets.list(name)`.
   */
  async ofNamespace(options: RequestOptions = {}): Promise<Secret[] | undefined> {
    if (this.namespace === undefined) {
      return undefined;
    }
    return new NamespaceSecrets(this.transport).list(this.namespace, options);
  }

  /**
   * Creates or replaces an organization's secret: its `value` (a string of at most 16 KiB) and its `labels`, which
   * replace those it had. The calls parked because the secret was missing or refused, in the runs that attach it, are
   * sent again with the same idempotency key. Refused to an end user's token.
   */
  async put(name: string, secret: PutSecret, options: RequestOptions = {}): Promise<Secret> {
    checkSecretName(name);
    return this.transport.send(
      () =>
        this.transport.api.PUT("/v1/secrets/{name}", {
          params: { path: { name } },
          body: secret,
          signal: options.signal,
        }),
      { retry: true, signal: options.signal },
    );
  }

  /**
   * Deletes it: the next call of a run that attaches it is parked (`credentials.expired`, reason `missing`) until a
   * secret of the same name is put again. `usage` says first what that would stop.
   */
  async delete(name: string, options: RequestOptions = {}): Promise<void> {
    checkSecretName(name);
    await this.transport.send(
      () =>
        this.transport.api.DELETE("/v1/secrets/{name}", {
          params: { path: { name } },
          signal: options.signal,
        }),
      { retry: true, signal: options.signal },
    );
  }

  /**
   * What reads it, and what deleting it would stop: the active runs that attach it, those of them waiting on a call it
   * serves, the workflows whose latest version attaches it; with its reads per day and its last read.
   */
  async usage(name: string, options: RequestOptions = {}): Promise<SecretUsage> {
    checkSecretName(name);
    return this.transport.send(
      () =>
        this.transport.api.GET("/v1/secrets/{name}/usage", {
          params: { path: { name } },
          signal: options.signal,
        }),
      { retry: true, signal: options.signal },
    );
  }
}

/**
 * A namespace's secrets: one end user's own values, such as Alice's GitHub token, attached as `ns/<name>` by the runs
 * of that namespace only. A key writes those of any namespace; an end user's token those of its own (with
 * `secrets:write`). A key or a token limited to labels never writes one.
 */
export class NamespaceSecrets {
  constructor(private readonly transport: Transport) {}

  /** The namespace's secrets, by name, with their metadata; never a value. */
  async list(namespace: string, options: RequestOptions = {}): Promise<Secret[]> {
    checkNamespaceName(namespace);
    const list = await this.transport.send(
      () =>
        this.transport.api.GET("/v1/namespaces/{name}/secrets", {
          params: { path: { name: namespace } },
          signal: options.signal,
        }),
      { retry: true, signal: options.signal },
    );
    return list.secrets;
  }

  /**
   * Creates or replaces the end user's secret `name` with `value` (a string of at most 16 KiB): the runs of that
   * namespace attaching `ns/<name>` read it. The calls parked because it was missing or refused are sent again.
   */
  async put(namespace: string, name: string, secret: { value: string }, options: RequestOptions = {}): Promise<Secret> {
    checkNamespaceName(namespace);
    checkSecretName(name);
    return this.transport.send(
      () =>
        this.transport.api.PUT("/v1/namespaces/{name}/secrets/{secret}", {
          params: { path: { name: namespace, secret: name } },
          body: { value: secret.value },
          signal: options.signal,
        }),
      { retry: true, signal: options.signal },
    );
  }

  /** Deletes it: the next call of a run of the namespace that attaches it is parked. */
  async delete(namespace: string, name: string, options: RequestOptions = {}): Promise<void> {
    checkNamespaceName(namespace);
    checkSecretName(name);
    await this.transport.send(
      () =>
        this.transport.api.DELETE("/v1/namespaces/{name}/secrets/{secret}", {
          params: { path: { name: namespace, secret: name } },
          signal: options.signal,
        }),
      { retry: true, signal: options.signal },
    );
  }

  /**
   * What reads it: the namespace's active runs that attach it, those waiting on a call it serves, and the workflows
   * whose latest version attaches `ns/<name>`.
   */
  async usage(namespace: string, name: string, options: RequestOptions = {}): Promise<SecretUsage> {
    checkNamespaceName(namespace);
    checkSecretName(name);
    return this.transport.send(
      () =>
        this.transport.api.GET("/v1/namespaces/{name}/secrets/{secret}/usage", {
          params: { path: { name: namespace, secret: name } },
          signal: options.signal,
        }),
      { retry: true, signal: options.signal },
    );
  }
}

/**
 * The organization's signing keys: Ed25519, one current, with which Rewind signs the calls to servers whose `auth` is
 * `{ "type": "rewind" }`. The servers check them with `verifyRewindCall`, against the public keys at `jwksUrl`.
 */
export class SigningKeys {
  constructor(private readonly transport: Transport) {}

  /**
   * The current key and those retired less than 24 hours ago, newest first, with the issuer of the JWTs (what
   * `verifyRewindCall` takes as `issuer`) and the address of the public keys (`{issuer}/jwks.json`).
   */
  list(options: RequestOptions = {}): Promise<SigningKeyList> {
    return this.transport.send(
      () => this.transport.api.GET("/v1/organization/signing-keys", { signal: options.signal }),
      { retry: true, signal: options.signal },
    );
  }

  /**
   * Replaces the signing key: the current one stops signing at once and stays published 24 hours, so the servers
   * still accept the calls it signed. Answers the new key. Needs `organization:write`.
   */
  rotate(options: RequestOptions = {}): Promise<SigningKey> {
    return this.transport.send(
      () => this.transport.api.POST("/v1/organization/signing-keys", { signal: options.signal }),
      // Each request replaces the key: never sent again.
      { retry: false, signal: options.signal },
    );
  }

  /** The public keys of an organization (`GET /organizations/{tenant}/jwks.json`), which needs no authentication. */
  jwks(tenant: string, options: RequestOptions = {}): Promise<Jwks> {
    return this.transport.send(
      () =>
        this.transport.api.GET("/organizations/{tenant}/jwks.json", {
          params: { path: { tenant } },
          signal: options.signal,
        }),
      { retry: true, signal: options.signal },
    );
  }
}

/**
 * The issuer of an organization's signed calls, `{public-url}/organizations/{tenant}`: what `verifyRewindCall` takes
 * as `issuer`. `signingKeys.list()` answers it too.
 */
export function rewindIssuer(publicUrl: string, tenant: string): string {
  return `${publicUrl.replace(/\/+$/, "")}/organizations/${encodeURIComponent(tenant)}`;
}

/** Where an issuer's public keys are: `{issuer}/jwks.json`, served without authentication. */
export function rewindJwksUrl(issuer: string): string {
  return `${issuer.replace(/\/+$/, "")}/jwks.json`;
}
