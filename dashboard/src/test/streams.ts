// Streams a test writes to, in place of the mocked API's: the change stream (`GET /v1/changes`)
// and a run's event stream (`GET /v1/runs/{id}/events`). Installed after `renderApp`, they take
// precedence over its handlers.
import { http } from "msw";
import { changeEvent } from "../mocks/changes";
import { eventStream } from "../mocks/run-handlers";
import type { ControlChange, Entry, RunSummary } from "../model/types";
import { server } from "./server";

interface Open {
  controller: ReadableStreamDefaultController<Uint8Array>;
  closed: boolean;
}

function writer() {
  const open: Open[] = [];
  const encoder = new TextEncoder();
  const respond = (request: Request, first: string) => {
    const entry: Open = { controller: undefined as never, closed: false };
    const stream = new ReadableStream<Uint8Array>({
      start(controller) {
        entry.controller = controller;
        open.push(entry);
        controller.enqueue(encoder.encode(first));
      },
      cancel() {
        entry.closed = true;
      },
    });
    request.signal.addEventListener("abort", () => {
      entry.closed = true;
    });
    return eventStream(stream);
  };
  const send = (text: string) => {
    for (const s of open) {
      if (s.closed) continue;
      try {
        s.controller.enqueue(encoder.encode(text));
      } catch {
        s.closed = true;
      }
    }
  };
  const close = () => {
    for (const s of open) {
      if (s.closed) continue;
      s.closed = true;
      try {
        s.controller.close();
      } catch {
        // Already closed.
      }
    }
  };
  return { open, respond, send, close };
}

/** The organization's change stream, written by the test. */
export function changeStream() {
  const w = writer();
  let opened = 0;
  server.use(
    http.get("*/v1/changes", ({ request }) => {
      opened += 1;
      return w.respond(request, ": open\n\n");
    }),
  );
  return {
    /** How many times the tab opened it. */
    get opened() {
      return opened;
    },
    /** Whether a stream is open now. */
    get live() {
      return w.open.some((s) => !s.closed);
    },
    run(summary: RunSummary) {
      w.send(`event: run\ndata: ${JSON.stringify(summary)}\n\n`);
    },
    resync() {
      w.send("event: resync\ndata: {}\n\n");
    },
    /** A change of the control plane: a script, a workflow, a secret, a namespace, a connection. */
    control(change: ControlChange) {
      w.send(changeEvent(change));
    },
    /** The server closes every open stream. */
    drop: w.close,
  };
}

/** A run's event stream: the entries given at once, then those the test sends. */
export function runEvents(runId: string, entries: () => Entry[]) {
  const w = writer();
  server.use(
    http.get(`*/v1/runs/${runId}/events`, ({ request }) => {
      const from = Number(new URL(request.url).searchParams.get("from") ?? 0);
      const first = entries()
        .slice(from)
        .map(
          (e) => `event: entry\nid: ${e.idx}\ndata: ${JSON.stringify({ type: "entry", ...e })}\n\n`,
        )
        .join("");
      return w.respond(request, first || ": open\n\n");
    }),
  );
  return {
    send(entry: Entry) {
      w.send(
        `event: entry\nid: ${entry.idx}\ndata: ${JSON.stringify({ type: "entry", ...entry })}\n\n`,
      );
    },
  };
}
