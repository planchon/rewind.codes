// `fetch`, for a script whose action set enables the built-in `http` tool (archi/04-tools.md, "The
// `http` tool and egress"). The type-checker gives this file to tsgo only then: without the
// `http` tool there is no `fetch`.
//
// Each call is a journaled call of the `http` tool, sent by Rewind, never from the sandbox, and
// only to the hosts the action set allows. Redirects are followed (5 at most); the response is
// bounded to 5 MB and 30 s. A refused or failed request rejects with a `ToolError` from "rewind".
// This is a subset of the web's `fetch`: no `Request`, `Response` or `Headers` constructors, no
// streams, no `AbortSignal`.

type HeadersInit = [string, string][] | Record<string, string> | Headers;

interface RequestInit {
  /** `GET` by default. */
  method?: string;
  headers?: HeadersInit;
  body?: string | URLSearchParams | ArrayBuffer | ArrayBufferView | null;
}

/** The headers of a response, read-only. Names are case-insensitive. */
interface Headers {
  get(name: string): string | null;
  has(name: string): boolean;
  forEach(callbackfn: (value: string, key: string, parent: Headers) => void, thisArg?: unknown): void;
  [Symbol.iterator](): IteratorObject<[string, string], BuiltinIteratorReturn, unknown>;
  entries(): IteratorObject<[string, string], BuiltinIteratorReturn, unknown>;
  keys(): IteratorObject<string, BuiltinIteratorReturn, unknown>;
  values(): IteratorObject<string, BuiltinIteratorReturn, unknown>;
}

/** The response of a request; its body can be read once. */
interface Response {
  /** The status is 200 to 299. */
  readonly ok: boolean;
  readonly status: number;
  readonly statusText: string;
  /** The URL of the response, after redirects. */
  readonly url: string;
  readonly redirected: boolean;
  readonly headers: Headers;
  readonly bodyUsed: boolean;
  text(): Promise<string>;
  json(): Promise<any>;
  arrayBuffer(): Promise<ArrayBuffer>;
  bytes(): Promise<Uint8Array<ArrayBuffer>>;
}

/** Sends an HTTP request through the action set's `http` tool, to a host it allows. */
declare function fetch(input: string | URL, init?: RequestInit): Promise<Response>;
