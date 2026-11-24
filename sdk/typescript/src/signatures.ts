/**
 * Checking a call Rewind signed (`archi/04-tools.md`, "Signed calls"): a server whose `auth` is `{ "type": "rewind" }`
 * gets, in `Authorization: Bearer`, a JWT signed with the organization's Ed25519 key. `verifyRewindCall` does every
 * check that server must do, and answers who the call acts for.
 */
import { base64url, createRemoteJWKSet, errors, jwtVerify, type JWTPayload, type JWTVerifyGetKey } from "jose";

import { RewindConnectionError, RewindError } from "./errors.js";
import { messageOf } from "./http.js";
import { rewindJwksUrl } from "./resources/secrets.js";
import type { Labels } from "./types.js";

/** Leeway on `exp` and `iat`, in seconds: the clocks of Rewind and of the server may differ by that much. */
const LEEWAY_S = 5;
/** A key with an unknown `kid` is fetched again at most this often. */
const REFETCH_MS = 10_000;
/** The public keys are cached as long as their endpoint says (`Cache-Control: max-age=300`). */
const CACHE_MS = 300_000;
const FETCH_TIMEOUT_MS = 5_000;

/**
 * The check a signed call failed:
 *
 * - `token`: no `Authorization: Bearer` JWT, or one that cannot be read;
 * - `alg`: not `EdDSA`;
 * - `kid`: no key of the issuer's `jwks.json` has its `kid`, even fetched again;
 * - `signature`: the signature does not match;
 * - `iss`, `aud`: another issuer (organization), another audience (server);
 * - `exp`, `iat`: expired, or issued in the future, beyond 5 s of leeway;
 * - `htm`, `htu`, `body`: signed for another method, URL or body: a JWT replayed on another request;
 * - `labels`: the run does not carry the labels this server expects (`env=prod`);
 * - `call`: the JWT names no call (`sub`, `run`, `action`, `idempotency_key`): a request of an MCP session outside a call,
 *   which only `session: true` accepts.
 */
export type SignatureCheck =
  "token" | "alg" | "kid" | "signature" | "iss" | "aud" | "exp" | "iat" | "htm" | "htu" | "body" | "labels" | "call";

/** A signed call was refused: `check` names what failed. Answer it with a 401: Rewind parks the call. */
export class RewindSignatureError extends RewindError {
  override readonly name = "RewindSignatureError";
  readonly check: SignatureCheck;

  constructor(check: SignatureCheck, message: string, options?: ErrorOptions) {
    super(`${check}: ${message}`, options);
    this.check = check;
  }
}

/** Who a signed call acts for. */
export interface RewindCall {
  /** The run's namespace (`sub`): the end user the call acts for. */
  namespace: string;
  /** The run's labels. */
  labels: Labels;
  /** The run's id. */
  run: string;
  /** The action called, as the action set names it: `dax.createMeasure`. */
  action: string;
  /**
   * `{run_id}/{seq}`: the same for every delivery of the call. A server that acts once per key never repeats an effect,
   * whatever the redeliveries.
   */
  idempotencyKey: string;
}

/** A request, as a server that has already read it holds it (Express, NestJS, Fastify). */
export interface ReceivedRequest {
  /** `GET`, `POST`… */
  method: string;
  /**
   * The absolute URL the client called, `https://api.example.com/v1/measures?x=1`: behind a proxy, the public one
   * (in Express, with `trust proxy`, `` `${req.protocol}://${req.get("host")}${req.originalUrl}` ``).
   */
  url: string;
  /** The headers: a Fetch `Headers`, or Node's (`req.headers`). */
  headers: Headers | Record<string, string | string[] | undefined>;
  /**
   * The raw body, as received: its bytes are hashed. A body already parsed as JSON cannot be: keep the raw one
   * (`express.raw()`, `express.json({ verify })`, NestJS's `rawBody: true`). `undefined` or `null` without a body.
   */
  body?: Uint8Array | ArrayBuffer | string | null | undefined;
}

export interface VerifyRewindCallOptions {
  /**
   * The issuer the server expects, `{public-url}/organizations/{organizationId}` (the dashboard's Organization › Signing
   * keys, `signingKeys.list()`, or `rewindIssuer`): its keys are read from `{issuer}/jwks.json`, never from the JWT.
   */
  issuer: string;
  /** The server's own audience: the API's `baseUrl` in the action set, or the MCP server's URL. */
  audience: string;
  /**
   * The labels the run must carry, such as `{ env: "prod" }`. One key signs every run of the organization: an
   * organization whose environments share it tells them apart by labels, and a production server refuses the calls
   * of a development run.
   */
  labels?: Labels | undefined;
  /**
   * An MCP server: the requests of the session outside a call (`initialize`, the listing of the tools) are signed too,
   * without `sub`, `labels`, `run`, `action` nor `idempotency_key`. They are answered `null` rather than refused.
   */
  session?: boolean | undefined;
}

/**
 * Checks a call Rewind signed with the organization's key (`auth: { "type": "rewind" }`), and answers who it acts
 * for. The checks: the signature, with a key of `{issuer}/jwks.json` (cached; an unknown `kid` fetched again at most
 * every 10 s); `iss` and `aud`; `exp` and `iat`, with 5 s of leeway; `htm`, `htu` and `body` (the SHA-256 of the raw
 * bytes) against the request; and `labels` when given. A failed check throws a `RewindSignatureError` naming it:
 * answer 401. The keys cannot be fetched: a `RewindConnectionError`, answer 503.
 *
 * ```ts
 * const call = await verifyRewindCall(request, {
 *   issuer: "https://api.rewind.codes/organizations/org_01ACME",
 *   audience: "https://api.example.com",
 *   labels: { env: "prod" },
 * });
 * // call.namespace: the end user; call.idempotencyKey: act once per key.
 * ```
 *
 * @param request A Fetch `Request` (its body is read from a clone), or `{ method, url, headers, body }`.
 */
export function verifyRewindCall(
  request: Request | ReceivedRequest,
  options: VerifyRewindCallOptions & { session?: false | undefined },
): Promise<RewindCall>;
export function verifyRewindCall(
  request: Request | ReceivedRequest,
  options: VerifyRewindCallOptions,
): Promise<RewindCall | null>;
export async function verifyRewindCall(
  request: Request | ReceivedRequest,
  options: VerifyRewindCallOptions,
): Promise<RewindCall | null> {
  const issuer = options.issuer.replace(/\/+$/, "");
  const received = await read(request);
  const token = bearerOf(received.authorization);

  let payload: JWTPayload;
  try {
    ({ payload } = await jwtVerify(token, keysOf(issuer), {
      algorithms: ["EdDSA"],
      issuer,
      audience: options.audience,
      clockTolerance: LEEWAY_S,
      requiredClaims: ["exp", "iat", "htm", "htu"],
    }));
  } catch (error) {
    throw refusal(error);
  }

  const now = Math.floor(Date.now() / 1000);
  if (typeof payload.iat === "number" && payload.iat > now + LEEWAY_S) {
    throw new RewindSignatureError("iat", "the JWT is issued in the future: is this server's clock right?");
  }
  if (payload["htm"] !== received.method.toUpperCase()) {
    throw new RewindSignatureError(
      "htm",
      `signed for ${JSON.stringify(payload["htm"])}, received ${JSON.stringify(received.method)}`,
    );
  }
  if (!sameUrl(payload["htu"], received.url)) {
    throw new RewindSignatureError(
      "htu",
      `signed for ${JSON.stringify(payload["htu"])}, received ${JSON.stringify(received.url)}: behind a proxy, ` +
        "give the URL the client called",
    );
  }
  const signedBody = payload["body"];
  if (signedBody === undefined) {
    if (received.body.byteLength > 0) {
      throw new RewindSignatureError("body", "signed without a body, received one");
    }
  } else if (typeof signedBody !== "string" || signedBody !== (await sha256(received.body))) {
    throw new RewindSignatureError(
      "body",
      "the body's SHA-256 does not match: pass the raw bytes received, before any parsing",
    );
  }

  const call = callOf(payload);
  if (call === null) {
    if (options.session === true) {
      return null;
    }
    throw new RewindSignatureError(
      "call",
      "the JWT names no call (sub, run, action, idempotency_key): a request of an MCP session, which only `session: true` accepts",
    );
  }
  for (const [key, value] of Object.entries(options.labels ?? {})) {
    if (call.labels[key] !== value) {
      throw new RewindSignatureError(
        "labels",
        `the run does not carry the label ${key}=${value}${key in call.labels ? ` (it carries ${key}=${call.labels[key]})` : ""}`,
      );
    }
  }
  return call;
}

/** The request's parts the checks read. */
interface Received {
  method: string;
  url: string;
  authorization: string | null;
  body: Uint8Array;
}

async function read(request: Request | ReceivedRequest): Promise<Received> {
  if (isFetchRequest(request)) {
    let body: ArrayBuffer;
    try {
      body = await request.clone().arrayBuffer();
    } catch (error) {
      throw new TypeError(
        `verifyRewindCall: the request's body cannot be read (${messageOf(error)}): verify the request before reading ` +
          "its body, or pass { method, url, headers, body }",
        { cause: error },
      );
    }
    return {
      method: request.method,
      url: request.url,
      authorization: request.headers.get("authorization"),
      body: new Uint8Array(body),
    };
  }
  if (typeof request.method !== "string" || typeof request.url !== "string") {
    throw new TypeError("verifyRewindCall: pass a Fetch Request, or { method, url, headers, body }");
  }
  return {
    method: request.method,
    url: request.url,
    authorization: headerOf(request.headers, "authorization"),
    body: bytesOf(request.body),
  };
}

function isFetchRequest(request: Request | ReceivedRequest): request is Request {
  const candidate = request as Partial<Request>;
  return typeof candidate.clone === "function" && typeof candidate.arrayBuffer === "function";
}

function headerOf(headers: ReceivedRequest["headers"] | undefined, name: string): string | null {
  if (headers === undefined || headers === null) {
    return null;
  }
  if (typeof (headers as Headers).get === "function") {
    return (headers as Headers).get(name);
  }
  for (const [key, value] of Object.entries(headers)) {
    if (key.toLowerCase() === name) {
      return (Array.isArray(value) ? value[0] : value) ?? null;
    }
  }
  return null;
}

function bytesOf(body: ReceivedRequest["body"]): Uint8Array {
  if (body === undefined || body === null) {
    return new Uint8Array();
  }
  if (typeof body === "string") {
    return new TextEncoder().encode(body);
  }
  if (body instanceof Uint8Array) {
    return body;
  }
  if (body instanceof ArrayBuffer) {
    return new Uint8Array(body);
  }
  throw new TypeError(
    "verifyRewindCall: the body must be the raw bytes received (a Buffer, a Uint8Array or a string), not a parsed " +
      "object: keep the raw body (express.raw(), express.json({ verify }), NestJS's rawBody: true)",
  );
}

function bearerOf(authorization: string | null): string {
  const match = authorization === null ? null : /^Bearer\s+(\S+)\s*$/i.exec(authorization);
  if (match === null || match[1] === undefined) {
    throw new RewindSignatureError("token", "no Authorization: Bearer header holding Rewind's JWT");
  }
  return match[1];
}

/** Whether the signed URL is the one received, both read as URLs: the case of the scheme and the host, a default port. */
function sameUrl(signed: unknown, received: string): boolean {
  let actual: URL;
  try {
    actual = new URL(received);
  } catch {
    throw new TypeError(
      `verifyRewindCall: url must be the absolute URL the client called (https://…), not ${JSON.stringify(received)}`,
    );
  }
  if (typeof signed !== "string" || !URL.canParse(signed)) {
    return false;
  }
  const expected = new URL(signed);
  expected.hash = "";
  actual.hash = "";
  return expected.href === actual.href;
}

async function sha256(bytes: Uint8Array): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", bytes as Uint8Array<ArrayBuffer>);
  return base64url.encode(new Uint8Array(digest));
}

function callOf(payload: JWTPayload): RewindCall | null {
  const { sub, run, action } = payload as { sub?: unknown; run?: unknown; action?: unknown };
  const idempotencyKey = payload["idempotency_key"];
  if (sub === undefined && run === undefined && action === undefined && idempotencyKey === undefined) {
    return null;
  }
  if (
    typeof sub !== "string" ||
    typeof run !== "string" ||
    typeof action !== "string" ||
    typeof idempotencyKey !== "string"
  ) {
    throw new RewindSignatureError("call", "the JWT names a call without all of sub, run, action and idempotency_key");
  }
  return { namespace: sub, labels: labelsOf(payload["labels"]), run, action, idempotencyKey };
}

function labelsOf(value: unknown): Labels {
  if (value === undefined || value === null) {
    return {};
  }
  if (typeof value !== "object" || Array.isArray(value)) {
    throw new RewindSignatureError("labels", "the claim labels is not an object");
  }
  const labels: Labels = {};
  for (const [key, label] of Object.entries(value)) {
    if (typeof label === "string") {
      labels[key] = label;
    }
  }
  return labels;
}

/** The keys of each issuer, fetched once and shared by every call. */
const keySets = new Map<string, JWTVerifyGetKey>();

function keysOf(issuer: string): JWTVerifyGetKey {
  const url = rewindJwksUrl(issuer);
  let keys = keySets.get(url);
  if (keys === undefined) {
    const remote = createRemoteJWKSet(new URL(url), {
      cooldownDuration: REFETCH_MS,
      cacheMaxAge: CACHE_MS,
      timeoutDuration: FETCH_TIMEOUT_MS,
    });
    keys = async (header, token) => {
      try {
        return await remote(header, token);
      } catch (error) {
        if (error instanceof errors.JWKSNoMatchingKey || error instanceof errors.JWKSMultipleMatchingKeys) {
          throw error;
        }
        throw new RewindConnectionError(`cannot read the public keys at ${url}: ${messageOf(error)}`, {
          cause: error,
        });
      }
    };
    keySets.set(url, keys);
  }
  return keys;
}

/** What jose's refusal means, as the check that failed. */
function refusal(error: unknown): Error {
  if (error instanceof RewindConnectionError || error instanceof RewindSignatureError) {
    return error;
  }
  if (error instanceof errors.JWTExpired) {
    return new RewindSignatureError(error.claim === "iat" ? "iat" : "exp", "the JWT has expired", { cause: error });
  }
  if (error instanceof errors.JWTClaimValidationFailed) {
    const check = CLAIM_CHECKS[error.claim] ?? "token";
    const why = error.reason === "missing" ? `the JWT has no claim ${error.claim}` : error.message;
    return new RewindSignatureError(check, why, { cause: error });
  }
  if (error instanceof errors.JWKSNoMatchingKey || error instanceof errors.JWKSMultipleMatchingKeys) {
    return new RewindSignatureError("kid", "no public key of the issuer has the JWT's kid", { cause: error });
  }
  if (error instanceof errors.JWSSignatureVerificationFailed) {
    return new RewindSignatureError("signature", "the signature does not match", { cause: error });
  }
  if (error instanceof errors.JOSEAlgNotAllowed || error instanceof errors.JOSENotSupported) {
    return new RewindSignatureError("alg", "the JWT is not signed with EdDSA", { cause: error });
  }
  if (error instanceof errors.JOSEError) {
    return new RewindSignatureError("token", `the JWT cannot be read: ${error.message}`, { cause: error });
  }
  return error instanceof Error ? error : new Error(String(error));
}

const CLAIM_CHECKS: Record<string, SignatureCheck> = {
  iss: "iss",
  aud: "aud",
  exp: "exp",
  iat: "iat",
  nbf: "iat",
  htm: "htm",
  htu: "htu",
};
