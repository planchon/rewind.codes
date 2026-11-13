import { Webhook, WebhookVerificationError } from "standardwebhooks";

import { RewindError } from "./errors.js";
import type { WebhookEvent, WebhookEventType } from "./types.js";

/** The headers of a delivery: a fetch `Headers`, or a plain map such as Node's `req.headers`. */
export type WebhookHeaders = Headers | Readonly<Record<string, string | readonly string[] | undefined>>;

/** A delivery whose signature, timestamp or body does not hold. Answer it with 400 and ignore it. */
export class WebhookVerificationFailed extends RewindError {
  override readonly name = "WebhookVerificationFailed";
}

const EVENT_TYPES: ReadonlySet<string> = new Set<WebhookEventType>([
  "run.completed",
  "run.failed",
  "run.cancelled",
  "run.aborted",
  "run.suspended",
  "run.quarantined",
  "tool.schema_changed",
  "credentials.expired",
]);

/**
 * Verifies a lifecycle webhook and returns its event. Deliveries are signed following Standard
 * Webhooks: `webhook-signature` must hold an HMAC-SHA256 of `{webhook-id}.{webhook-timestamp}.{body}`
 * made with the subscription's secret (`whsec_…`), and `webhook-timestamp` must be within 5 minutes
 * of now, so an old delivery cannot be replayed.
 *
 * Pass the body exactly as received, before any JSON parsing. A retried delivery keeps its
 * `webhook-id`: deduplicate on it.
 *
 * @throws {WebhookVerificationFailed} when the delivery is not authentic or not an event.
 */
export function verifyWebhook(payload: string | Uint8Array, headers: WebhookHeaders, secret: string): WebhookEvent {
  let webhook: Webhook;
  try {
    webhook = new Webhook(secret);
  } catch (error) {
    throw new RewindError(`invalid webhook secret: ${error instanceof Error ? error.message : String(error)}`);
  }
  const body = typeof payload === "string" ? payload : new TextDecoder().decode(payload);
  let event: unknown;
  try {
    event = webhook.verify(body, signatureHeaders(headers));
  } catch (error) {
    if (error instanceof WebhookVerificationError) {
      throw new WebhookVerificationFailed(`webhook refused: ${error.message}`);
    }
    throw new WebhookVerificationFailed(
      `webhook refused: the body is not JSON (${error instanceof Error ? error.message : String(error)})`,
    );
  }
  if (!isEvent(event)) {
    throw new WebhookVerificationFailed("webhook refused: the body is not a lifecycle event");
  }
  return event;
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

function isEvent(value: unknown): value is WebhookEvent {
  if (typeof value !== "object" || value === null) {
    return false;
  }
  const event = value as Record<string, unknown>;
  return (
    typeof event["type"] === "string" &&
    EVENT_TYPES.has(event["type"]) &&
    typeof event["timestamp"] === "string" &&
    typeof event["data"] === "object" &&
    event["data"] !== null
  );
}
