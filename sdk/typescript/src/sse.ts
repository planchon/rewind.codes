import type { EventSourceMessage } from "eventsource-parser";
import { EventSourceParserStream } from "eventsource-parser/stream";

import { throwIfAborted } from "./http.js";

/**
 * The messages of a server-sent event stream, until it ends, or until `signal` aborts: the stream is then cancelled,
 * which closes its connection, and the signal's reason is thrown. A consumer that stops reading early cancels it too.
 *
 * The abort is followed here, not only through `fetch`: Node's `fetch` follows the caller's signal through a signal of
 * its own `Request`, held by a weak reference. Once that request is collected, as it may be as soon as the headers
 * have come, aborting the caller's signal no longer ends the body being read, and a stream held open by the server is
 * read forever.
 */
export async function* sseMessages(
  body: ReadableStream<Uint8Array>,
  signal: AbortSignal | undefined,
): AsyncGenerator<EventSourceMessage> {
  const reader = body.pipeThrough(new TextDecoderStream()).pipeThrough(new EventSourceParserStream()).getReader();
  const cancel = () => void reader.cancel(signal?.reason).catch(() => undefined);
  signal?.addEventListener("abort", cancel, { once: true });
  try {
    throwIfAborted(signal);
    for (;;) {
      const { done, value } = await reader.read();
      if (done) {
        break;
      }
      yield value;
    }
    throwIfAborted(signal);
  } finally {
    signal?.removeEventListener("abort", cancel);
    await reader.cancel().catch(() => undefined);
  }
}
