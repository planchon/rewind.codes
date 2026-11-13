import createClient, { type Client } from "openapi-fetch";

import { RewindApiError, RewindConnectionError, RewindError, problemOf } from "./errors.js";
import type { paths } from "./generated/api.js";

/** How the client reaches the server. */
export interface TransportOptions {
  /** The server's base URL, such as `https://rewind.example.com`. */
  url: string;
  /** The API key, `rewind_sk_…`, sent as `Authorization: Bearer …`. */
  apiKey?: string | undefined;
  /** A `fetch` to send requests with, for a proxy, a unix socket or tests. */
  fetch?: typeof globalThis.fetch | undefined;
  /** Headers added to every request. */
  headers?: Record<string, string> | undefined;
  /**
   * How many times a request that is safe to repeat is sent again after a network error or a
   * 503. Safe: reads, `PUT`, `DELETE`, and requests carrying an `Idempotency-Key`. 2 by default.
   */
  maxRetries?: number | undefined;
}

interface ApiResult<D> {
  data?: D;
  error?: unknown;
  response: Response;
}

/** Options of one request. */
export interface SendOptions {
  /** Send the request again after a network error or a 503. */
  retry: boolean;
  signal?: AbortSignal | undefined;
}

/** The typed HTTP client of the API, with authentication, errors and retries. */
export class Transport {
  readonly url: string;
  readonly api: Client<paths>;
  readonly maxRetries: number;

  constructor(options: TransportOptions) {
    this.url = options.url.replace(/\/+$/, "");
    this.maxRetries = options.maxRetries ?? 2;
    const headers: Record<string, string> = { ...options.headers };
    if (options.apiKey !== undefined && options.apiKey !== "") {
      headers["Authorization"] = `Bearer ${options.apiKey}`;
    }
    const fetchImpl = options.fetch ?? globalThis.fetch;
    this.api = createClient<paths>({
      baseUrl: this.url,
      headers,
      fetch: (request: Request) => fetchImpl(request),
    });
  }

  /** Sends a request built by `request`; returns its data, or throws a `RewindError`. */
  async send<D>(request: () => Promise<ApiResult<D>>, options: SendOptions): Promise<D> {
    return (await this.sendForResponse(request, options)).data;
  }

  /** As `send`, with the response, for its status and headers. */
  async sendForResponse<D>(
    request: () => Promise<ApiResult<D>>,
    options: SendOptions,
  ): Promise<{ data: D; response: Response }> {
    const { signal } = options;
    for (let attempt = 0; ; attempt++) {
      throwIfAborted(signal);
      let result: ApiResult<D>;
      try {
        result = await request();
      } catch (error) {
        if (signal?.aborted || isAbortError(error)) {
          throw signal?.reason ?? error;
        }
        if (options.retry && attempt < this.maxRetries) {
          await sleep(backoff(attempt), signal);
          continue;
        }
        throw new RewindConnectionError(`cannot reach the Rewind server at ${this.url}: ${messageOf(error)}`, {
          cause: error,
        });
      }
      const { response } = result;
      if (response.ok) {
        return { data: result.data as D, response };
      }
      if (options.retry && response.status === 503 && attempt < this.maxRetries) {
        await sleep(retryAfter(response) ?? backoff(attempt), signal);
        continue;
      }
      throw new RewindApiError(problemOf(response.status, response.statusText, result.error));
    }
  }
}

/** 250 ms, 500 ms, 1 s… up to 8 s, with jitter. */
export function backoff(attempt: number, base = 250, max = 8_000): number {
  const ceiling = Math.min(max, base * 2 ** attempt);
  return ceiling / 2 + Math.random() * (ceiling / 2);
}

function retryAfter(response: Response): number | undefined {
  const header = response.headers.get("Retry-After");
  if (header === null) {
    return undefined;
  }
  const seconds = Number(header);
  return Number.isFinite(seconds) && seconds >= 0 ? Math.min(seconds * 1000, 30_000) : undefined;
}

/** Waits `ms`, or rejects with the signal's reason when it aborts first. */
export function sleep(ms: number, signal?: AbortSignal): Promise<void> {
  return new Promise((resolve, reject) => {
    if (signal?.aborted) {
      reject(signal.reason);
      return;
    }
    const onAbort = () => {
      clearTimeout(timer);
      reject(signal?.reason);
    };
    const timer = setTimeout(() => {
      signal?.removeEventListener("abort", onAbort);
      resolve();
    }, ms);
    signal?.addEventListener("abort", onAbort, { once: true });
  });
}

export function throwIfAborted(signal: AbortSignal | undefined): void {
  if (signal?.aborted) {
    throw signal.reason;
  }
}

export function isAbortError(error: unknown): boolean {
  return error instanceof Error && (error.name === "AbortError" || error.name === "TimeoutError");
}

export function messageOf(error: unknown): string {
  if (error instanceof Error) {
    const cause = error.cause instanceof Error ? ` (${error.cause.message})` : "";
    return `${error.message}${cause}`;
  }
  return String(error);
}

/** Refuses a key the server would refuse: 1 to 255 visible ASCII characters. */
export function checkIdempotencyKey(key: string | undefined): void {
  if (key === undefined) {
    return;
  }
  if (!isValidIdempotencyKey(key)) {
    throw new RewindError(`invalid idempotency key ${JSON.stringify(key)}: expected 1 to 255 visible ASCII characters`);
  }
}

export function isValidIdempotencyKey(key: string): boolean {
  return key.length >= 1 && key.length <= 255 && /^[\x20-\x7e]+$/.test(key);
}
