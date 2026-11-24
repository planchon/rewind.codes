import { Webhook as StandardWebhook, WebhookVerificationError } from "standardwebhooks";

import { RewindError } from "./errors.js";
import type { Webhook, WebhookType } from "./types.js";

/** The headers of a delivery: a fetch `Headers`, or a plain map such as Node's `req.headers`. */
export type WebhookHeaders = Headers | Readonly<Record<string, string | readonly string[] | undefined>>;

/** A delivery whose signature, timestamp or body does not hold. Answer it with 400 and ignore it. */
export class WebhookVerificationFailed extends RewindError {
  override readonly name = "WebhookVerificationFailed";
}

/** Every type of webhook: a type the API adds and this list misses fails to compile. */
const WEBHOOK_TYPES: ReadonlySet<string> = new Set(
  Object.keys({
    "run.completed": true,
    "run.failed": true,
    "run.cancelled": true,
    "run.aborted": true,
    "run.waiting": true,
    "run.quarantined": true,
    "action.schema_changed": true,
    "secret.expired": true,
    "quota.alert": true,
  } satisfies Record<WebhookType, true>),
);

/**
 * Verifies a webhook Rewind sent and returns it: `{ type, timestamp, data }`. Deliveries are signed following Standard
 * Webhooks: `webhook-signature` must hold an HMAC-SHA256 of `{webhook-id}.{webhook-timestamp}.{body}`
 * made with the subscription's secret (`whsec_…`), and `webhook-timestamp` must be within 5 minutes
 * of now, so an old delivery cannot be replayed.
 *
 * Pass the body exactly as received, before any JSON parsing. A retried delivery keeps its
 * `webhook-id`: deduplicate on it.
 *
 * @throws {WebhookVerificationFailed} when the delivery is not authentic or not a webhook of a known type.
 */
export function verifyWebhook(payload: string | Uint8Array, headers: WebhookHeaders, secret: string): Webhook {
  let verifier: StandardWebhook;
  try {
    verifier = new StandardWebhook(secret);
  } catch (error) {
    throw new RewindError(`invalid webhook secret: ${error instanceof Error ? error.message : String(error)}`);
  }
  const body = typeof payload === "string" ? payload : new TextDecoder().decode(payload);
  let webhook: unknown;
  try {
    webhook = verifier.verify(body, signatureHeaders(headers));
  } catch (error) {
    if (error instanceof WebhookVerificationError) {
      throw new WebhookVerificationFailed(`webhook refused: ${error.message}`);
    }
    throw new WebhookVerificationFailed(
      `webhook refused: the body is not JSON (${error instanceof Error ? error.message : String(error)})`,
    );
  }
  if (!isWebhook(webhook)) {
    throw new WebhookVerificationFailed("webhook refused: the body is not a webhook of a known type");
  }
  return webhook;
}

function signatureHeaders(headers: WebhookHeaders): Record<string, string> {
  const read = (name: string): string | undefined => {
    if (headers instanceof Headers) {
      return headers.get(name) ?? undefined;
    }
    for (const [key, value] of Object.entries(headers)) {
      if (key.toLowerCase() === name) {
        return typeof value === "string" ? value : value?.join(" ");
      }
    }
    return undefined;
  };
  const result: Record<string, string> = {};
  for (const name of ["webhook-id", "webhook-timestamp", "webhook-signature"]) {
    const value = read(name);
    if (value !== undefined) {
      result[name] = value;
    }
  }
  return result;
}

function isWebhook(value: unknown): value is Webhook {
  if (typeof value !== "object" || value === null) {
    return false;
  }
  const webhook = value as Record<string, unknown>;
  return (
    typeof webhook["type"] === "string" &&
    WEBHOOK_TYPES.has(webhook["type"]) &&
    typeof webhook["timestamp"] === "string" &&
    typeof webhook["data"] === "object" &&
    webhook["data"] !== null
  );
}
