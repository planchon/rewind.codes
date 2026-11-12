// Follows a run's journal through `GET /v1/runs/{id}/events?follow=1` (server-sent events).
// `EventSource` cannot send the Authorization header, so the stream is read with `fetch` and
// parsed with eventsource-parser. A dropped stream is resumed from the next entry.
import { createParser } from "eventsource-parser";
import { type Api, ApiError } from "./client";
import type { Entry, RunStatus, StreamEvent } from "./types";

export type StreamState = "connecting" | "live" | "retrying";

export interface FollowHandlers {
  /** New entries, in index order, once per chunk received. */
  onEntries(entries: Entry[]): void;
  /** The run has ended: the stream is over. */
  onEnd(status: RunStatus): void;
  onState?(state: StreamState, error?: unknown): void;
}

export interface FollowOptions {
  /** The first entry to read. */
  from?: number;
  /** Waits between reconnections, the last one repeated. */
  backoffMs?: readonly number[];
}

function sleep(ms: number, signal: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    const timer = setTimeout(resolve, ms);
    signal.addEventListener(
      "abort",
      () => {
        clearTimeout(timer);
        resolve();
      },
      { once: true },
    );
  });
}

/** Whether an error will not go away by retrying (an unknown run, a refused key). */
function isFatal(error: unknown): boolean {
  return error instanceof ApiError && error.status < 500 && error.status !== 429;
}

/**
 * Reads the run's entries from `from`, then each new one as it is committed, until the run ends
 * or `signal` aborts. Resolves when the stream is over; rejects only on a fatal error.
 */
export async function followRun(
  api: Api,
  runId: string,
  handlers: FollowHandlers,
  signal: AbortSignal,
  options: FollowOptions = {},
): Promise<void> {
  const backoff = options.backoffMs ?? [500, 1_000, 2_000, 5_000, 10_000];
  let next = options.from ?? 0;
  let attempt = 0;

  while (!signal.aborted) {
    handlers.onState?.(attempt === 0 ? "connecting" : "retrying");
    try {
      const response = await api.raw(
        `/v1/runs/${encodeURIComponent(runId)}/events?follow=1&from=${next}`,
        { headers: { Accept: "text/event-stream" }, signal },
      );
      if (!response.body) throw new Error("The event stream has no body.");
      handlers.onState?.("live");

      let ended: RunStatus | null = null;
      let batch: Entry[] = [];
      const parser = createParser({
        onEvent(message) {
          if (ended !== null) return;
          const data = JSON.parse(message.data) as StreamEvent;
          if (data.type === "end") {
            ended = data.status;
            return;
          }
          const { type: _type, ...entry } = data;
          // A reconnection may send again what was already read.
          if (entry.idx < next) return;
          next = entry.idx + 1;
          batch.push(entry);
        },
      });
      const reader = response.body.getReader();
      const decoder = new TextDecoder();
      try {
        while (ended === null) {
          const { value, done } = await reader.read();
          if (done) break;
          parser.feed(decoder.decode(value, { stream: true }));
          if (batch.length > 0) {
            handlers.onEntries(batch);
            batch = [];
            attempt = 0;
          }
        }
      } finally {
        if (ended !== null) await reader.cancel().catch(() => undefined);
      }
      if (ended !== null) {
        handlers.onEnd(ended);
        return;
      }
      // The stream closed before the end of the run: reconnect from the next entry.
    } catch (error) {
      if (signal.aborted) return;
      if (isFatal(error)) throw error;
      handlers.onState?.("retrying", error);
    }
    const wait = backoff[Math.min(attempt, backoff.length - 1)] ?? 1_000;
    attempt += 1;
    await sleep(wait, signal);
  }
}
