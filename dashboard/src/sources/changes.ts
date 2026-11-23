// Follows what changes in the organization through `GET /v1/changes` (server-sent events), read
// with `fetch` since `EventSource` cannot send the Authorization header. A `run` event carries a
// run's whole summary, as `GET /v1/runs` lists it; `script`, `workflow`, `secret`, `namespace` and
// `connection` name what changed in the control plane; `resync` says that too much changed at once.
//
// There is no resume cursor: after a drop the stream is opened again (after 1 s, doubling up to
// 30 s) and whoever follows it reads again what it shows. The server closes the stream at the
// expiry of the access token it was opened with: it is then opened again at once, with the token
// the session holds then. One stream per tab and organization.
import { createParser } from "eventsource-parser";
import type { ControlChange, RunSummary } from "../model/types";
import type { Api } from "./client";

/** A control-plane change from an event's name and data; `null` for one this tab does not know. */
export function controlChange(event: string | undefined, data: string): ControlChange | null {
  let value: unknown;
  try {
    value = JSON.parse(data);
  } catch {
    return null;
  }
  if (!value || typeof value !== "object") return null;
  const v = value as Record<string, unknown>;
  const text = (key: string) => (typeof v[key] === "string" ? (v[key] as string) : null);
  switch (event) {
    case "script": {
      const digest = text("digest");
      return digest === null ? null : { kind: "script", digest };
    }
    case "workflow": {
      const name = text("name");
      const version = text("version");
      return name === null || version === null ? null : { kind: "workflow", name, version };
    }
    case "secret": {
      const name = text("name");
      const namespace = text("namespace");
      if (name === null) return null;
      return namespace === null ? { kind: "secret", name } : { kind: "secret", name, namespace };
    }
    case "namespace": {
      const name = text("name");
      return name === null ? { kind: "namespace" } : { kind: "namespace", name };
    }
    case "connection": {
      const id = text("id");
      return id === null ? null : { kind: "connection", id };
    }
    default:
      return null;
  }
}

/** `connecting` before the first answer, `reconnecting` after a drop until the next one. */
export type ChangeStreamState = "connecting" | "live" | "reconnecting";

export interface ChangeHandlers {
  /**
   * The stream is open. `reopened`: it was open before, and what changed in between is not
   * known.
   */
  onOpen(reopened: boolean): void;
  /** A run changed: its whole summary. */
  onRun(run: RunSummary): void;
  /** A script, a workflow, a secret, a namespace or a connection changed. */
  onControl(change: ControlChange): void;
  /** Too much changed at once: read again what is shown. */
  onResync(): void;
  onState(state: ChangeStreamState, error?: unknown): void;
}

export interface ChangeStreamOptions {
  /** The wait after a first drop, doubled after each one. */
  backoffMs?: number;
  /** The longest wait. */
  maxBackoffMs?: number;
  /** A stream the server closes sooner than this after it opened counts as a drop. */
  minLifeMs?: number;
}

export interface ChangeStream {
  /** Closes the stream and opens it again at once: a fresh token. */
  reopen(): void;
  /** Closes the stream for good. */
  close(): void;
}

/** The wait before the `attempt`-th reconnection (from 0): 1 s, 2 s, 4 s… up to 30 s. */
export function backoffDelay(attempt: number, baseMs = 1_000, maxMs = 30_000): number {
  return Math.min(maxMs, baseMs * 2 ** Math.min(attempt, 30));
}

/** A wait spread by ±25 %: the tabs a restart of the server drops do not come back in step. */
export function spread(ms: number): number {
  return Math.round(ms * (0.75 + Math.random() * 0.5));
}

export function followChanges(
  api: Api,
  handlers: ChangeHandlers,
  options: ChangeStreamOptions = {},
): ChangeStream {
  const baseMs = options.backoffMs ?? 1_000;
  const maxMs = options.maxBackoffMs ?? 30_000;
  const minLifeMs = options.minLifeMs ?? 5_000;
  const closed = new AbortController();
  let connection: AbortController | null = null;
  /** Ends the current wait at once. */
  let wake: (() => void) | null = null;
  let reopenAsked = false;
  let opened = false;
  let attempt = 0;

  const wait = (ms: number) =>
    new Promise<void>((resolve) => {
      const done = () => {
        clearTimeout(timer);
        closed.signal.removeEventListener("abort", done);
        wake = null;
        resolve();
      };
      const timer = setTimeout(done, ms);
      closed.signal.addEventListener("abort", done, { once: true });
      wake = done;
    });

  const read = async (response: Response) => {
    if (!response.body) throw new Error("The change stream has no body.");
    const parser = createParser({
      onEvent(message) {
        if (message.event === "resync") {
          handlers.onResync();
          return;
        }
        if (message.event !== "run") {
          const change = controlChange(message.event, message.data);
          if (change) handlers.onControl(change);
          return;
        }
        let run: RunSummary;
        try {
          run = JSON.parse(message.data) as RunSummary;
        } catch {
          return;
        }
        if (run && typeof run.id === "string") handlers.onRun(run);
      },
    });
    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    try {
      for (;;) {
        const { value, done } = await reader.read();
        if (done) return;
        parser.feed(decoder.decode(value, { stream: true }));
      }
    } finally {
      await reader.cancel().catch(() => undefined);
    }
  };

  /** When the stream being read opened; `null` while none is. */
  let openedAt: number | null = null;

  const run = async () => {
    while (!closed.signal.aborted) {
      reopenAsked = false;
      if (!opened && attempt === 0) handlers.onState("connecting");
      const current = new AbortController();
      connection = current;
      const abort = () => current.abort();
      closed.signal.addEventListener("abort", abort, { once: true });
      try {
        const response = await api.raw("/v1/changes", {
          headers: { Accept: "text/event-stream" },
          signal: current.signal,
        });
        openedAt = Date.now();
        handlers.onState("live");
        handlers.onOpen(opened);
        opened = true;
        await read(response);
        if (closed.signal.aborted) return;
        // Closed by the server: at the expiry of the token it was opened with, or by a proxy.
        // Opened again at once with the token held now, unless it closed as soon as it opened.
        if (Date.now() - openedAt >= minLifeMs) {
          attempt = 0;
          continue;
        }
        throw new Error("The change stream closed as soon as it opened.");
      } catch (error) {
        if (closed.signal.aborted) return;
        if (reopenAsked) continue;
        handlers.onState("reconnecting", error);
      } finally {
        closed.signal.removeEventListener("abort", abort);
        if (connection === current) connection = null;
        // A stream that lived starts the waits over; one that closed at once does not.
        if (openedAt !== null && Date.now() - openedAt >= minLifeMs) attempt = 0;
        openedAt = null;
      }
      const delay = spread(backoffDelay(attempt, baseMs, maxMs));
      attempt += 1;
      await wait(delay);
    }
  };
  void run();

  return {
    reopen() {
      if (closed.signal.aborted) return;
      reopenAsked = true;
      attempt = 0;
      connection?.abort();
      wake?.();
    },
    close() {
      closed.abort();
    },
  };
}
