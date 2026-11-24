// What a run did, derived from the events of its history: calls, timers, signal waits and logs,
// each with the line of the code that caused it, its start and its end. A pure function of the
// events, like the run's state on the server (archi/02-history.md).
import type {
  AbortReason,
  ActionFailure,
  Callsite,
  CancelReason,
  ErrorInfo,
  EventOf,
  HistoryEvent,
  LogLevel,
  Payload,
} from "./types";

interface Point {
  /** The event's position in the history. */
  idx: number;
  /** Its `recordedAt`, in milliseconds since the epoch. */
  at: number;
}

export type CallEnd =
  | (Point & { outcome: "ok"; result: Payload })
  | (Point & { outcome: "failed"; error: ActionFailure });

/** A call of an action, from its `schedule_call` to its result. */
export interface Call extends Point {
  kind: "call";
  seq: number;
  action: string;
  args: Payload;
  callsite: Callsite | null;
  end: CallEnd | null;
}

export interface Timer extends Point {
  kind: "timer";
  seq: number;
  durationMs: number;
  fireAt: number;
  callsite: Callsite | null;
  end: (Point & { outcome: "fired" | "cancelled" }) | null;
  /** The event of the signal wait this timer is the timeout of. */
  timeoutOf: number | null;
}

export type SignalEnd =
  | (Point & { outcome: "received"; payload: Payload; buffered: boolean })
  | (Point & { outcome: "timed_out" });

export interface SignalWait extends Point {
  kind: "signal";
  name: string;
  callsite: Callsite | null;
  /** The `seq` of the timer of its timeout. */
  timeoutSeq: number | null;
  /** When the wait gives up, from its timer. */
  timeoutAt: number | null;
  end: SignalEnd | null;
}

export interface LogLine extends Point {
  kind: "log";
  level: LogLevel;
  message: string;
  callsite: Callsite | null;
}

export interface ReceivedSignal extends Point {
  name: string;
  payload: Payload;
  /** The event of the wait it resolved; `null` while it sits in the mailbox. */
  waitIdx: number | null;
}

export type Activity = Call | Timer | SignalWait | LogLine;

export type RunEnd =
  | (Point & { outcome: "completed"; output: Payload })
  | (Point & { outcome: "failed"; error: ErrorInfo })
  | (Point & { outcome: "aborted"; reason: AbortReason });

export interface RunModel {
  entries: readonly HistoryEvent[];
  start: (Point & { event: EventOf<"run_started"> }) | null;
  end: RunEnd | null;
  cancel: (Point & { reason: CancelReason; message: string | null }) | null;
  calls: Call[];
  timers: Timer[];
  signals: SignalWait[];
  received: ReceivedSignal[];
  logs: LogLine[];
  /** Every activity, in the order it started. */
  activities: Activity[];
  /** The activities caused by each line of `workflow.ts`. */
  byLine: Map<number, Activity[]>;
  /** Activities without a callsite. */
  unattributed: Activity[];
  /** The activity each event starts or ends. */
  byIdx: Map<number, Activity>;
  /** The first and last `recordedAt`. */
  firstAt: number | null;
  lastAt: number | null;
}

export function time(rfc3339: string): number {
  return Date.parse(rfc3339);
}

/** Builds the model of a run from its events, in the order of their position. */
export function buildRunModel(entries: readonly HistoryEvent[]): RunModel {
  const model: RunModel = {
    entries,
    start: null,
    end: null,
    cancel: null,
    calls: [],
    timers: [],
    signals: [],
    received: [],
    logs: [],
    activities: [],
    byLine: new Map(),
    unattributed: [],
    byIdx: new Map(),
    firstAt: null,
    lastAt: null,
  };
  const calls = new Map<number, Call>();
  const timers = new Map<number, Timer>();
  // Signal waits not yet resolved, and signals no wait took yet, per name (FIFO mailboxes).
  const waiting = new Map<string, SignalWait[]>();
  const mailbox = new Map<string, ReceivedSignal[]>();
  const waitsByTimeout = new Map<number, SignalWait>();

  const add = (activity: Activity) => {
    model.activities.push(activity);
    model.byIdx.set(activity.idx, activity);
    const line = activity.callsite?.line;
    if (line === undefined) {
      model.unattributed.push(activity);
    } else {
      const list = model.byLine.get(line);
      if (list) list.push(activity);
      else model.byLine.set(line, [activity]);
    }
  };
  const resolve = (wait: SignalWait, signal: ReceivedSignal, at: Point, buffered: boolean) => {
    wait.end = { ...at, outcome: "received", payload: signal.payload, buffered };
    signal.waitIdx = wait.idx;
    model.byIdx.set(signal.idx, wait);
  };

  for (const entry of entries) {
    const point: Point = { idx: entry.idx, at: time(entry.recordedAt) };
    model.firstAt ??= point.at;
    model.lastAt = point.at;
    const event = entry.event;
    switch (event.type) {
      case "run_started":
        model.start = { ...point, event };
        break;
      case "schedule_call": {
        const call: Call = {
          ...point,
          kind: "call",
          seq: event.seq,
          action: event.action,
          args: event.args,
          callsite: event.callsite ?? null,
          end: null,
        };
        calls.set(call.seq, call);
        model.calls.push(call);
        add(call);
        break;
      }
      case "call_completed": {
        const call = calls.get(event.seq);
        if (call) {
          call.end = { ...point, outcome: "ok", result: event.result };
          model.byIdx.set(point.idx, call);
        }
        break;
      }
      case "call_failed": {
        const call = calls.get(event.seq);
        if (call) {
          call.end = { ...point, outcome: "failed", error: event.error };
          model.byIdx.set(point.idx, call);
        }
        break;
      }
      case "start_timer": {
        const timer: Timer = {
          ...point,
          kind: "timer",
          seq: event.seq,
          durationMs: event.durationMs,
          fireAt: time(event.fireAt),
          callsite: event.callsite ?? null,
          end: null,
          timeoutOf: null,
        };
        const wait = waitsByTimeout.get(timer.seq);
        if (wait) {
          timer.timeoutOf = wait.idx;
          wait.timeoutAt = timer.fireAt;
        }
        timers.set(timer.seq, timer);
        model.timers.push(timer);
        add(timer);
        break;
      }
      case "timer_fired":
      case "cancel_timer": {
        const timer = timers.get(event.seq);
        if (timer) {
          timer.end = { ...point, outcome: event.type === "timer_fired" ? "fired" : "cancelled" };
          model.byIdx.set(point.idx, timer);
        }
        const wait = waitsByTimeout.get(event.seq);
        if (event.type === "timer_fired" && wait && wait.end === null) {
          wait.end = { ...point, outcome: "timed_out" };
          const queue = waiting.get(wait.name);
          if (queue)
            waiting.set(
              wait.name,
              queue.filter((w) => w !== wait),
            );
        }
        break;
      }
      case "signal_awaited": {
        const wait: SignalWait = {
          ...point,
          kind: "signal",
          name: event.name,
          callsite: event.callsite ?? null,
          timeoutSeq: event.timeout ?? null,
          timeoutAt: null,
          end: null,
        };
        if (wait.timeoutSeq !== null) {
          waitsByTimeout.set(wait.timeoutSeq, wait);
          const timer = timers.get(wait.timeoutSeq);
          if (timer) {
            timer.timeoutOf = wait.idx;
            wait.timeoutAt = timer.fireAt;
          }
        }
        model.signals.push(wait);
        add(wait);
        const early = mailbox.get(wait.name)?.shift();
        if (early) {
          resolve(wait, early, point, true);
        } else {
          const queue = waiting.get(wait.name);
          if (queue) queue.push(wait);
          else waiting.set(wait.name, [wait]);
        }
        break;
      }
      case "signal_received": {
        const signal: ReceivedSignal = {
          ...point,
          name: event.name,
          payload: event.payload,
          waitIdx: null,
        };
        model.received.push(signal);
        const wait = waiting.get(signal.name)?.shift();
        if (wait) {
          resolve(wait, signal, point, false);
        } else {
          const box = mailbox.get(signal.name);
          if (box) box.push(signal);
          else mailbox.set(signal.name, [signal]);
        }
        break;
      }
      case "log": {
        const log: LogLine = {
          ...point,
          kind: "log",
          level: event.level,
          message: event.message,
          callsite: event.callsite ?? null,
        };
        model.logs.push(log);
        add(log);
        break;
      }
      case "complete_run":
        model.end = { ...point, outcome: "completed", output: event.output };
        break;
      case "fail_run":
        model.end = { ...point, outcome: "failed", error: event.error };
        break;
      case "run_aborted":
        model.end = { ...point, outcome: "aborted", reason: event.reason };
        break;
      case "cancel_requested":
        model.cancel = { ...point, reason: event.reason, message: event.message ?? null };
        break;
    }
  }
  return model;
}

/** Whether an activity is still open: a call in flight, a pending timer, a signal awaited. */
export function isOpen(activity: Activity): boolean {
  return activity.kind !== "log" && activity.end === null;
}

/** When an activity ended, if it did. */
export function endOf(activity: Activity): number | null {
  if (activity.kind === "log") return activity.at;
  return activity.end?.at ?? null;
}

/** The outcome of an activity, for display: `ok`, `failed`, `open`, `fired`… */
export function outcomeOf(activity: Activity): string {
  if (activity.kind === "log") return activity.level;
  if (activity.end === null) return "open";
  return activity.end.outcome;
}

/** A short label: `#3 github.list_issues`, `timer #4 3d`, `signal "approval"`, `log`. */
export function labelOf(activity: Activity): string {
  switch (activity.kind) {
    case "call":
      return activity.action;
    case "timer":
      return activity.timeoutOf !== null ? "timeout" : "sleep";
    case "signal":
      return `signal "${activity.name}"`;
    case "log":
      return `console.${activity.level}`;
  }
}

/** A stable key for an activity, used for selection: its first event. */
export function keyOf(activity: Activity): string {
  return String(activity.idx);
}
