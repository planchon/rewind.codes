// The globals of the Rewind sandbox beyond ECMAScript (archi/01-product.md, "What exists in the
// sandbox"). The type-checker gives this file to tsgo next to the code, with the ECMAScript
// lib of tsconfig.json and no DOM: what type-checks is what exists. `Temporal` comes from that
// lib (esnext.temporal); `fetch` from fetch.d.ts, only when the action set enables the `http` action.
//
// Adding or changing a global here changes what scripts can observe: it goes with a new
// determinism epoch, and with globals.json.

// -------------------------------------------------------------------------------------------
// Console: recorded as log lines of the run.

interface Console {
  log(...data: unknown[]): void;
  info(...data: unknown[]): void;
  debug(...data: unknown[]): void;
  warn(...data: unknown[]): void;
  error(...data: unknown[]): void;
}

/** Writes to the run's log: each call is recorded with its line in the code. */
declare var console: Console;

// -------------------------------------------------------------------------------------------
// Timers: durable, like `sleep` from "rewind". Time is the run's logical time.

// The callback takes `any` arguments, as on the web, so that `setTimeout(resolve, 100)` in a
// `new Promise` checks. It must be a function: a string is refused.

/** Calls `callback` after `delay` milliseconds. Durable: it survives restarts. */
declare function setTimeout(
  callback: (...args: any[]) => void,
  delay?: number,
  ...args: any[]
): number;

/** Calls `callback` every `delay` milliseconds, until `clearInterval`. */
declare function setInterval(
  callback: (...args: any[]) => void,
  delay?: number,
  ...args: any[]
): number;

declare function clearTimeout(id?: number): void;
declare function clearInterval(id?: number): void;
declare function queueMicrotask(callback: () => void): void;

// -------------------------------------------------------------------------------------------
// Time and randomness: deterministic, replayed identically.

interface Performance {
  /** Milliseconds of logical time since the run started. */
  now(): number;
  /** When the run started, in milliseconds since the Unix epoch. */
  readonly timeOrigin: number;
}

declare var performance: Performance;

interface Crypto {
  /** Fills the array with cryptographically strong random values, and returns it. */
  getRandomValues<
    T extends
      | Int8Array
      | Uint8Array
      | Uint8ClampedArray
      | Int16Array
      | Uint16Array
      | Int32Array
      | Uint32Array
      | BigInt64Array
      | BigUint64Array,
  >(array: T): T;
  /** A random UUID (version 4). */
  randomUUID(): `${string}-${string}-${string}-${string}-${string}`;
}

/** Cryptographically strong randomness, for tokens and passwords. No `crypto.subtle`. */
declare var crypto: Crypto;

// -------------------------------------------------------------------------------------------
// URLs

interface URL {
  hash: string;
  host: string;
  hostname: string;
  href: string;
  toString(): string;
  readonly origin: string;
  password: string;
  pathname: string;
  port: string;
  protocol: string;
  search: string;
  readonly searchParams: URLSearchParams;
  username: string;
  toJSON(): string;
}

declare var URL: {
  prototype: URL;
  new (url: string | URL, base?: string | URL): URL;
  /** Whether `new URL(url, base)` would succeed. */
  canParse(url: string | URL, base?: string | URL): boolean;
  /** `new URL(url, base)`, or `null` where it would throw. */
  parse(url: string | URL, base?: string | URL): URL | null;
};

interface URLSearchParamsIterator<T> extends IteratorObject<T, BuiltinIteratorReturn, unknown> {
  [Symbol.iterator](): URLSearchParamsIterator<T>;
}

interface URLSearchParams {
  readonly size: number;
  append(name: string, value: string): void;
  delete(name: string, value?: string): void;
  get(name: string): string | null;
  getAll(name: string): string[];
  has(name: string, value?: string): boolean;
  set(name: string, value: string): void;
  sort(): void;
  toString(): string;
  forEach(
    callbackfn: (value: string, key: string, parent: URLSearchParams) => void,
    thisArg?: unknown,
  ): void;
  [Symbol.iterator](): URLSearchParamsIterator<[string, string]>;
  entries(): URLSearchParamsIterator<[string, string]>;
  keys(): URLSearchParamsIterator<string>;
  values(): URLSearchParamsIterator<string>;
}

declare var URLSearchParams: {
  prototype: URLSearchParams;
  new (init?: string[][] | Record<string, string> | string | URLSearchParams): URLSearchParams;
};

// -------------------------------------------------------------------------------------------
// Text and bytes

interface TextEncoderEncodeIntoResult {
  read: number;
  written: number;
}

interface TextEncoder {
  /** Always `"utf-8"`. */
  readonly encoding: string;
  encode(input?: string): Uint8Array<ArrayBuffer>;
  encodeInto(source: string, destination: Uint8Array): TextEncoderEncodeIntoResult;
}

declare var TextEncoder: {
  prototype: TextEncoder;
  new (): TextEncoder;
};

interface TextDecoderOptions {
  fatal?: boolean;
  ignoreBOM?: boolean;
}

interface TextDecodeOptions {
  stream?: boolean;
}

interface TextDecoder {
  readonly encoding: string;
  readonly fatal: boolean;
  readonly ignoreBOM: boolean;
  decode(input?: ArrayBuffer | ArrayBufferView, options?: TextDecodeOptions): string;
}

declare var TextDecoder: {
  prototype: TextDecoder;
  new (label?: string, options?: TextDecoderOptions): TextDecoder;
};

/** Decodes a base64 string to a binary string. */
declare function atob(data: string): string;
/** Encodes a binary string (each character below U+0100) to base64. */
declare function btoa(data: string): string;

// -------------------------------------------------------------------------------------------
// Values

interface StructuredSerializeOptions {
  transfer?: ArrayBuffer[];
}

/** A deep copy of `value`, as the structured clone algorithm makes it. */
declare function structuredClone<T = any>(value: T, options?: StructuredSerializeOptions): T;
