// What `state/` keeps per key (a search, a count, a run's details or history): shown while a view
// holds it, kept a while after the last one lets it go, so that a return to the page shows it at
// once, then dropped.
import { ApiError } from "../sources/client";

/** How long something no view shows is kept. */
export const KEPT_MS = 5 * 60_000;

export abstract class Kept {
  private refs = 0;
  private gcTimer: ReturnType<typeof setTimeout> | undefined;
  private releaseTimer: ReturnType<typeof setTimeout> | undefined;
  private released = true;
  protected disposed = false;
  private readonly listeners = new Set<() => void>();

  constructor(private readonly gone: () => void) {
    // Made for a render that may never mount: dropped if no view holds it.
    this.scheduleGc();
  }

  /** Held by a view until the answer is called. */
  retain(): () => void {
    this.refs += 1;
    clearTimeout(this.gcTimer);
    clearTimeout(this.releaseTimer);
    if (this.released) {
      this.released = false;
      this.onRetained();
    }
    let done = false;
    return () => {
      if (done) return;
      done = true;
      this.refs -= 1;
      if (this.refs > 0) return;
      // A view that mounts again at once (React's strict mode, a key that comes back) keeps it
      // without a pause.
      this.releaseTimer = setTimeout(() => {
        if (this.refs > 0 || this.disposed) return;
        this.released = true;
        this.onReleased();
        this.scheduleGc();
      }, 0);
    };
  }

  get held(): boolean {
    return this.refs > 0;
  }

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  };

  protected notify(): void {
    for (const listener of [...this.listeners]) listener();
  }

  protected onRetained(): void {}

  protected onReleased(): void {}

  private scheduleGc(): void {
    clearTimeout(this.gcTimer);
    this.gcTimer = setTimeout(() => {
      if (this.refs > 0 || this.disposed) return;
      this.dispose();
      this.gone();
    }, KEPT_MS);
  }

  dispose(): void {
    this.disposed = true;
    clearTimeout(this.gcTimer);
    clearTimeout(this.releaseTimer);
    this.listeners.clear();
  }
}

function sleep(ms: number, signal: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    const abort = () => {
      clearTimeout(timer);
      resolve();
    };
    const timer = setTimeout(() => {
      signal.removeEventListener("abort", abort);
      resolve();
    }, ms);
    signal.addEventListener("abort", abort, { once: true });
  });
}

/** Whether asking again may help: not for a refusal or a bad request. */
function retriable(error: unknown): boolean {
  if (!(error instanceof ApiError)) return true;
  return error.status >= 500 || error.status === 429;
}

/** `read`, asked again twice (after 1 s, then 2 s) when the failure may pass. */
export async function withRetry<T>(
  read: () => Promise<T>,
  signal: AbortSignal,
  waits: readonly number[] = [1_000, 2_000],
): Promise<T> {
  for (let attempt = 0; ; attempt += 1) {
    try {
      return await read();
    } catch (error) {
      const wait = waits[attempt];
      if (signal.aborted || wait === undefined || !retriable(error)) throw error;
      await sleep(wait, signal);
      if (signal.aborted) throw error;
    }
  }
}

let serial = 0;

/** A collection id no other collection of the tab has. */
export function collectionId(name: string): string {
  serial += 1;
  return `${name}#${serial}`;
}
