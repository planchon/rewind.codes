// The `rewind` module: the standard library of scripts (archi/01-product.md, "Modules available to
// scripts"). The type-checker gives this file to tsgo next to the script; it declares what the
// runtime's `rewind` module exports, and nothing else.

declare module "rewind" {
  /**
   * A duration: a number of milliseconds, or a number and a unit (`ms`, `s`, `m`, `h`, `d`,
   * `w`), possibly several: `"500ms"`, `"30s"`, `"1h30m"`, `"2d"`.
   */
  export type Duration = number | `${number}${"ms" | "s" | "m" | "h" | "d" | "w"}` | string;

  /**
   * The signals this run accepts, by name, with their payload's type. For the script of a
   * workflow, its declared signals fill it, and `signal()` and `webhook()` only take their
   * names. Empty for any other script: they take any name.
   */
  export interface Signals {}

  /** A name `signal()` and `webhook()` take: one of {@link Signals}, or any string if none. */
  export type SignalName = [keyof Signals] extends [never] ? string : keyof Signals & string;

  /** What `signal(name)` gives: the declared type of the signal `K`, or `T` if none is declared. */
  export type SignalPayload<K, T> = [keyof Signals] extends [never]
    ? T
    : K extends keyof Signals
      ? Signals[K]
      : never;

  /**
   * Waits for `duration`. The wait is durable: the run survives restarts and deploys
   * meanwhile, and costs nothing while it waits. Afterwards `Date.now()` is at least the time
   * before plus `duration`.
   */
  export function sleep(duration: Duration): Promise<void>;

  /**
   * Waits for the next signal named `name` sent to this run, and returns its payload. A
   * signal sent before the script waits is kept in the name's mailbox, first in first out, so
   * none is lost. Without a timeout, the wait lasts as long as the run. Names starting with
   * `rewind:` are reserved.
   */
  export function signal<T = unknown, K extends SignalName = SignalName>(
    name: K,
  ): Promise<SignalPayload<K, T>>;
  /**
   * Waits for the next signal named `name`, and returns its payload, or `undefined` if none
   * arrived within `opts.timeout`.
   */
  export function signal<T = unknown, K extends SignalName = SignalName>(
    name: K,
    opts: { timeout: Duration },
  ): Promise<SignalPayload<K, T> | undefined>;

  /**
   * Returns a unique URL bound to this run and the signal `name`: every request posted to it
   * becomes a signal `name` on this run, with the request's body as payload. Give the URL to an
   * external system, then wait with `signal(name)`.
   */
  export function webhook(name: SignalName): Promise<{ url: string }>;

  /** This run. */
  export const run: {
    /** The run's id. A fork of this run has its own. */
    readonly id: string;
    /** When the run started. */
    readonly startedAt: Date;
  };

  /**
   * Thrown by a call that failed. Catch it to handle the failure; uncaught, it fails the run.
   * Only the runtime creates one.
   */
  export class ActionError extends Error {
    private constructor();
    /** The action that failed, such as `github.close_issue`. */
    action: string;
    /**
     * The call was interrupted and may or may not have taken effect: check before calling the
     * action again.
     */
    outcomeUnknown: boolean;
    /** What the action returned with its error, if anything. */
    data?: unknown;
  }

  /**
   * Rejects every pending `await` of the script when the run is cancelled. Catch it to clean
   * up: the script then has one chance to end by itself.
   */
  export class CancelledError extends Error {}

  /**
   * Thrown by `result()` when the run of a workflow this run started failed, was cancelled or
   * aborted, or returned an output its workflow's schema refuses. Only the runtime creates one.
   */
  export class WorkflowError extends Error {
    private constructor();
    /** The workflow, such as `triage`. */
    workflow: string;
    /** The run that ended so. */
    runId: string;
    status: "failed" | "cancelled" | "aborted" | "invalid_output";
  }

  /** The options of `start()`. */
  export interface StartOptions {
    /**
     * A detached run lives on its own. Otherwise this run does not end before it, and cancels
     * it if this run fails or is cancelled.
     */
    detached?: boolean;
    /** Added to this run's labels, which the new run carries too. */
    labels?: Record<string, string>;
    /** The new run's TTL: 30 days by default. */
    ttl?: Duration;
  }

  /** A run of a workflow, by its id: it can be signalled. */
  export interface WorkflowRunRef<S> {
    readonly id: string;
    /**
     * Sends one of the workflow's signals to the run. Resolves once the signal is in the run's
     * mailbox; throws an `ActionError` if the run has ended.
     */
    signal<K extends keyof S & string>(name: K, payload: S[K]): Promise<void>;
  }

  /** A run of a workflow this run started. */
  export interface WorkflowRun<O, S> extends WorkflowRunRef<S> {
    /**
     * The run's output, once it has ended. Throws a `WorkflowError` if it failed, was
     * cancelled or aborted.
     */
    result(): Promise<O>;
    /** The run's output, or `undefined` if it has not ended within `opts.timeout`. */
    result(opts: { timeout: Duration }): Promise<O | undefined>;
  }

  /**
   * A workflow the run's action set pins, imported from `rewind:workflows`: its input `I`, its
   * output `O` and its signals `S`.
   */
  export interface Workflow<I, O, S> {
    readonly name: string;
    /** Starts a run of the workflow, and returns it once it exists. */
    start(input: I, opts?: StartOptions): Promise<WorkflowRun<O, S>>;
    /** An existing run of the workflow, to signal it. */
    get(runId: string): WorkflowRunRef<S>;
  }
}
