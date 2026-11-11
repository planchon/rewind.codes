// The `ptc` module: the standard library of scripts (archi/01-product.md, "Modules available to
// scripts"). The type-checker gives this file to tsgo next to the script; it declares what the
// runtime's `ptc` module exports, and nothing else.

declare module "ptc" {
  /**
   * A duration: a number of milliseconds, or a number and a unit (`ms`, `s`, `m`, `h`, `d`,
   * `w`), possibly several: `"500ms"`, `"30s"`, `"1h30m"`, `"2d"`.
   */
  export type Duration = number | `${number}${"ms" | "s" | "m" | "h" | "d" | "w"}` | string;

  /**
   * Waits for `duration`. The wait is durable: the run survives restarts and deploys
   * meanwhile, and costs nothing while it waits. Afterwards `Date.now()` is at least the time
   * before plus `duration`.
   */
  export function sleep(duration: Duration): Promise<void>;

  /**
   * Waits for the next signal named `name` sent to this run, and returns its payload. A
   * signal sent before the script waits is kept in the name's mailbox, first in first out, so
   * none is lost. Without a timeout, the wait lasts as long as the run.
   */
  export function signal<T = unknown>(name: string): Promise<T>;
  /**
   * Waits for the next signal named `name`, and returns its payload, or `undefined` if none
   * arrived within `opts.timeout`.
   */
  export function signal<T = unknown>(
    name: string,
    opts: { timeout: Duration },
  ): Promise<T | undefined>;

  /**
   * Returns a unique URL bound to this run and the signal `name`: every request posted to it
   * becomes a signal `name` on this run, with the request's body as payload. Give the URL to an
   * external system, then wait with `signal(name)`.
   */
  export function webhook(name: string): Promise<{ url: string }>;

  /** This run. */
  export const run: {
    /** The run's id. A fork of this run has its own. */
    readonly id: string;
    /** When the run started. */
    readonly startedAt: Date;
  };

  /**
   * Thrown by a tool call that failed. Catch it to handle the failure; uncaught, it fails the
   * run. Only the runtime creates one.
   */
  export class ToolError extends Error {
    private constructor();
    /** The tool that failed, such as `github.close_issue`. */
    tool: string;
    /**
     * The call was interrupted and may or may not have taken effect: check before calling the
     * tool again.
     */
    outcomeUnknown: boolean;
    /** What the tool returned with its error, if anything. */
    data?: unknown;
  }

  /**
   * Rejects every pending `await` of the script when the run is cancelled. Catch it to clean
   * up: the script then has one chance to end by itself.
   */
  export class CancelledError extends Error {}
}
