// Bootstrap of the ai-ptc sandbox, determinism epoch 1.
//
// FROZEN: a change a script can observe (a seq, a digest, a value, a global) needs a new epoch
// (archi/03-execution.md, "Determinism epoch").
//
// Runs once per isolate, before any module. It makes every source of non-determinism
// deterministic, removes ambient capabilities, and returns the hooks Rust drives the script
// with: start(input), deliver(completion) and idle(). The `ptc` modules get their hooks
// through a global that `ptc:internal` deletes before any user code runs.
//
// Nothing here is asynchronous from V8's point of view: ops record a command and return its
// seq, and promises are settled when Rust delivers the matching completion.

((globalThis) => {
  "use strict";

  const {
    ArrayBufferIsView,
    ArrayPrototypeFilter,
    ArrayPrototypeIndexOf,
    ArrayPrototypeJoin,
    ArrayPrototypeMap,
    ArrayPrototypePush,
    ArrayPrototypeShift,
    ArrayPrototypeSplice,
    Error,
    JSONParse,
    JSONStringify,
    NumberParseInt,
    ObjectDefineProperty,
    ObjectFreeze,
    ObjectGetOwnPropertyDescriptor,
    Promise,
    PromisePrototypeThen,
    PromiseReject,
    PromiseResolve,
    ReflectApply,
    ReflectConstruct,
    RegExpPrototypeExec,
    RegExpPrototypeSymbolReplace,
    SafeMap,
    String,
    StringPrototypePadStart,
    StringPrototypeSlice,
    StringPrototypeSplit,
    TypeError,
    Uint8Array,
  } = globalThis.__bootstrap.primordials;

  const {
    op_drain_pending_rejections,
    op_ptc_call_tool,
    op_ptc_cancel_timer,
    op_ptc_complete,
    op_ptc_fail,
    op_ptc_log,
    op_ptc_math_random_state,
    op_ptc_now,
    op_ptc_random_fill,
    op_ptc_run_id,
    op_ptc_signal_awaited,
    op_ptc_start_timer,
    op_ptc_started_at,
  } = globalThis.Deno.core.ops;

  // ---------------------------------------------------------------------------------------
  // Callsites: the position in the model's script, source-mapped to TypeScript.

  const SCRIPT_FRAME = /file:\/\/\/script\.ts:(\d+):(\d+)/;

  function callsite() {
    const stack = new Error().stack ?? "";
    for (const line of StringPrototypeSplit(stack, "\n")) {
      const match = RegExpPrototypeExec(SCRIPT_FRAME, line);
      if (match) return [NumberParseInt(match[1], 10), NumberParseInt(match[2], 10)];
    }
    return [0, 0];
  }

  // ---------------------------------------------------------------------------------------
  // Logical time: the recorded_at of the last delivered trigger.

  const NativeDate = globalThis.Date;
  const DateTimeFormat = globalThis.Intl.DateTimeFormat;
  if (new DateTimeFormat().resolvedOptions().timeZone !== "UTC") {
    throw new Error("ai-ptc requires the process to run with TZ=UTC");
  }

  const now = () => op_ptc_now();
  const startedAt = op_ptc_started_at();

  const LogicalDate = function Date(...args) {
    if (new.target === undefined) return String(new NativeDate(now()));
    if (args.length === 0) return ReflectConstruct(NativeDate, [now()], new.target);
    return ReflectConstruct(NativeDate, args, new.target);
  };
  // NativeDate stays private: through it, a script would read the real clock. Only its pure
  // static methods are copied.
  for (const name of ["parse", "UTC"]) {
    ObjectDefineProperty(LogicalDate, name, {
      value: NativeDate[name],
      writable: true,
      configurable: true,
    });
  }
  ObjectDefineProperty(LogicalDate, "prototype", { value: NativeDate.prototype });
  ObjectDefineProperty(LogicalDate, "length", { value: 7 });
  ObjectDefineProperty(LogicalDate, "now", { value: now, writable: true, configurable: true });
  ObjectDefineProperty(NativeDate.prototype, "constructor", {
    value: LogicalDate,
    writable: true,
    configurable: true,
  });
  globalThis.Date = LogicalDate;

  // Intl formats "now" when called without a date.
  const getFormat = ObjectGetOwnPropertyDescriptor(DateTimeFormat.prototype, "format").get;
  ObjectDefineProperty(DateTimeFormat.prototype, "format", {
    get() {
      const format = ReflectApply(getFormat, this, []);
      return (date) => format(date === undefined ? now() : date);
    },
    configurable: true,
  });
  const formatToParts = DateTimeFormat.prototype.formatToParts;
  DateTimeFormat.prototype.formatToParts = function (date) {
    return ReflectApply(formatToParts, this, [date === undefined ? now() : date]);
  };

  if (globalThis.Temporal !== undefined) {
    const Temporal = globalThis.Temporal;
    const instant = () => Temporal.Instant.fromEpochMilliseconds(now());
    const zoned = (timeZone = "UTC") => instant().toZonedDateTimeISO(timeZone);
    ObjectDefineProperty(Temporal, "Now", {
      value: ObjectFreeze({
        instant,
        timeZoneId: () => "UTC",
        zonedDateTimeISO: zoned,
        plainDateTimeISO: (timeZone) => zoned(timeZone).toPlainDateTime(),
        plainDateISO: (timeZone) => zoned(timeZone).toPlainDate(),
        plainTimeISO: (timeZone) => zoned(timeZone).toPlainTime(),
      }),
      configurable: true,
    });
  }

  ObjectDefineProperty(globalThis, "performance", {
    value: ObjectFreeze({ now: () => now() - startedAt, timeOrigin: startedAt }),
    configurable: true,
  });

  // ---------------------------------------------------------------------------------------
  // Randomness: sfc32 for Math.random, ChaCha20 (in Rust) for crypto. Both are keyed by the
  // run's seed, each with its own derived key.

  let [a, b, c, d] = ArrayPrototypeMap(
    StringPrototypeSplit(op_ptc_math_random_state(), ","),
    (word) => NumberParseInt(word, 10) >>> 0,
  );
  function sfc32() {
    const t = (((a + b) | 0) + d) | 0;
    d = (d + 1) | 0;
    a = b ^ (b >>> 9);
    b = (c + (c << 3)) | 0;
    c = (c << 21) | (c >>> 11);
    c = (c + t) | 0;
    return t >>> 0;
  }
  for (let i = 0; i < 12; i++) sfc32();
  globalThis.Math.random = () => sfc32() / 4294967296;

  const hex = (byte) => StringPrototypePadStart(byte.toString(16), 2, "0");
  ObjectDefineProperty(globalThis, "crypto", {
    value: ObjectFreeze({
      getRandomValues(array) {
        if (!ArrayBufferIsView(array) || array instanceof globalThis.DataView) {
          throw new TypeError("crypto.getRandomValues expects an integer typed array");
        }
        op_ptc_random_fill(new Uint8Array(array.buffer, array.byteOffset, array.byteLength));
        return array;
      },
      randomUUID() {
        const bytes = new Uint8Array(16);
        op_ptc_random_fill(bytes);
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        const h = ArrayPrototypeJoin(ArrayPrototypeMap(bytes, hex), "");
        return `${StringPrototypeSlice(h, 0, 8)}-${StringPrototypeSlice(h, 8, 12)}-${
          StringPrototypeSlice(h, 12, 16)
        }-${StringPrototypeSlice(h, 16, 20)}-${StringPrototypeSlice(h, 20)}`;
      },
    }),
    configurable: true,
  });

  // ---------------------------------------------------------------------------------------
  // Errors the script can see. They are created by the glue, outside the script's code, so
  // their stack is only their name and message: no frame of the glue leaks into a journal.

  function ownStack(error) {
    ObjectDefineProperty(error, "stack", {
      value: `${error.name}: ${error.message}`,
      writable: true,
      configurable: true,
    });
  }

  class ToolError extends Error {
    constructor(tool, failure) {
      super(`${tool} failed: ${failure.message}`);
      this.name = "ToolError";
      this.tool = tool;
      this.outcomeUnknown = failure.outcomeUnknown;
      this.data = failure.data ?? undefined;
      ownStack(this);
    }
  }

  class CancelledError extends Error {
    constructor(message) {
      super(message);
      this.name = "CancelledError";
      ownStack(this);
    }
  }

  class DeadlockError extends Error {
    constructor() {
      super(
        "main has not settled, and nothing can wake the script: no tool call, timer or signal wait is pending",
      );
      this.name = "DeadlockError";
      ownStack(this);
    }
  }

  // ---------------------------------------------------------------------------------------
  // How a script ends (archi/03-execution.md): the outcome is evaluated each time the script
  // goes idle, and the terminal command waits until no tool call is in flight.

  let main;
  let mainState = "running"; // "returned" | "threw"
  // The JSON of main's return value, serialized as soon as main returns: serializing runs the
  // script's code (toJSON, getters), which must not happen once the outcome is decided.
  let output;
  // { name, message, stack }, captured as soon as the run is known to fail. From then on, no
  // script code runs any more.
  let failure;
  let cancellation; // the message of the cancellation, once one was requested
  let ended = false; // the terminal command was emitted
  let toolsInFlight = 0;

  // seq -> { kind: "tool" | "sleep" | "timeout" | "callback", ... }
  const pending = new SafeMap();

  function take(seq) {
    const entry = pending.get(seq);
    if (entry !== undefined) pending.delete(seq);
    return entry;
  }

  function fail(error) {
    if (failure === undefined) failure = describe(error);
  }

  // A frame of the runtime's own modules, such as "    at start (ptc:bootstrap:467:16)".
  const GLUE_FRAME = /^\s+at (?:.* \()?ptc:\w+:\d+:\d+\)?$/;

  function describe(error) {
    try {
      if (!(error instanceof Error)) return { name: "Error", message: show(error), stack: "" };
      const stack = ArrayPrototypeFilter(
        StringPrototypeSplit(String(error.stack ?? ""), "\n"),
        (line) => RegExpPrototypeExec(GLUE_FRAME, line) === null,
      );
      return {
        name: String(error.name),
        message: String(error.message),
        stack: ArrayPrototypeJoin(stack, "\n"),
      };
    } catch {
      return { name: "Error", message: "the script failed with a value that cannot be described", stack: "" };
    }
  }

  // JSON.stringify escapes lone surrogates ("\ud83d"), which no string of the host can hold:
  // they become U+FFFD, as TextEncoder and String.prototype.toWellFormed do.
  const LONE_SURROGATE = /\\\\|\\ud[89a-f][0-9a-f]{2}/gi;

  function wellFormed(json) {
    return RegExpPrototypeSymbolReplace(
      LONE_SURROGATE,
      json,
      (match) => match === "\\\\" ? match : "\\ufffd",
    );
  }

  function durationArg(duration) {
    if (typeof duration === "number") return String(duration);
    if (typeof duration === "string") return duration;
    throw new TypeError(`invalid duration: ${String(duration)}`);
  }

  function callTool(tool, args) {
    let json;
    try {
      json = JSONStringify(args === undefined ? {} : args);
    } catch (error) {
      return PromiseReject(
        new TypeError(`the arguments of ${tool} are not serializable to JSON: ${error.message}`),
      );
    }
    if (json === undefined) {
      return PromiseReject(new TypeError(`the arguments of ${tool} are not serializable to JSON`));
    }
    const [line, column] = callsite();
    const seq = op_ptc_call_tool(tool, wellFormed(json), line, column);
    toolsInFlight++;
    return new Promise((resolve, reject) => {
      pending.set(seq, { kind: "tool", tool, resolve, reject });
    });
  }

  function sleep(duration) {
    const [line, column] = callsite();
    const seq = op_ptc_start_timer(durationArg(duration), line, column);
    return new Promise((resolve, reject) => {
      pending.set(seq, { kind: "sleep", resolve, reject });
    });
  }

  // setTimeout and setInterval on top of durable timers. An interval keeps its first seq as
  // its id.
  const intervals = new SafeMap(); // id -> seq of the timer currently armed

  function armTimer(callback, delay, args, intervalId) {
    if (typeof callback !== "function") {
      throw new TypeError("timer callback must be a function");
    }
    const ms = delay > 0 ? delay : 0;
    const [line, column] = callsite();
    const seq = op_ptc_start_timer(String(ms), line, column);
    pending.set(seq, { kind: "callback", callback, delay: ms, args, intervalId });
    return seq;
  }

  function clearTimer(id) {
    const seq = intervals.get(id) ?? id;
    intervals.delete(id);
    const entry = pending.get(seq);
    if (entry !== undefined && entry.kind === "callback") {
      pending.delete(seq);
      op_ptc_cancel_timer(seq);
    }
  }

  globalThis.setTimeout = (callback, delay = 0, ...args) =>
    armTimer(callback, Number(delay) || 0, args, undefined);
  globalThis.setInterval = (callback, delay = 0, ...args) => {
    const seq = armTimer(callback, Number(delay) || 0, args, undefined);
    pending.get(seq).intervalId = seq;
    intervals.set(seq, seq);
    return seq;
  };
  globalThis.clearTimeout = clearTimer;
  globalThis.clearInterval = clearTimer;

  // Signals: one FIFO mailbox per name; waiters are served in order.
  const mailboxes = new SafeMap(); // name -> [payload]
  const waiters = new SafeMap(); // name -> [{ resolve, reject, timeoutSeq }]

  function signal(name, options) {
    name = String(name);
    const mailbox = mailboxes.get(name);
    if (mailbox !== undefined && mailbox.length > 0) {
      return PromiseResolve(ArrayPrototypeShift(mailbox));
    }
    const [line, column] = callsite();
    return new Promise((resolve, reject) => {
      const waiter = { resolve, reject, timeoutSeq: undefined };
      if (options?.timeout !== undefined) {
        waiter.timeoutSeq = op_ptc_start_timer(durationArg(options.timeout), line, column);
        pending.set(waiter.timeoutSeq, { kind: "timeout", name, waiter });
      }
      op_ptc_signal_awaited(name, waiter.timeoutSeq ?? 0, line, column);
      let queue = waiters.get(name);
      if (queue === undefined) {
        queue = [];
        waiters.set(name, queue);
      }
      ArrayPrototypePush(queue, waiter);
    });
  }

  function receiveSignal(name, payload) {
    const queue = waiters.get(name);
    if (queue !== undefined && queue.length > 0) {
      const waiter = ArrayPrototypeShift(queue);
      if (waiter.timeoutSeq !== undefined && take(waiter.timeoutSeq) !== undefined) {
        op_ptc_cancel_timer(waiter.timeoutSeq);
      }
      waiter.resolve(payload);
      return;
    }
    let mailbox = mailboxes.get(name);
    if (mailbox === undefined) {
      mailbox = [];
      mailboxes.set(name, mailbox);
    }
    ArrayPrototypePush(mailbox, payload);
  }

  function fireTimer(seq) {
    const entry = take(seq);
    if (entry === undefined) return; // cancelled, or settled by a cancellation
    switch (entry.kind) {
      case "sleep":
        entry.resolve();
        break;
      case "timeout": {
        const queue = waiters.get(entry.name);
        const index = queue === undefined ? -1 : ArrayPrototypeIndexOf(queue, entry.waiter);
        if (index !== -1) ArrayPrototypeSplice(queue, index, 1);
        entry.waiter.resolve(undefined);
        break;
      }
      case "callback": {
        if (entry.intervalId !== undefined) {
          if (!intervals.has(entry.intervalId)) break;
          const next = armTimer(entry.callback, entry.delay, entry.args, entry.intervalId);
          intervals.set(entry.intervalId, next);
        }
        try {
          ReflectApply(entry.callback, undefined, entry.args);
        } catch (error) {
          // An uncaught exception, as in Node: the run fails.
          fail(error);
        }
        break;
      }
    }
  }

  function cancelAll(message) {
    const error = () => new CancelledError(message);
    for (const [seq, entry] of pending) {
      pending.delete(seq);
      switch (entry.kind) {
        case "tool":
          // Cancellation does not wait for the calls in flight: their results are dropped.
          toolsInFlight--;
          entry.reject(error());
          break;
        case "sleep":
          entry.reject(error());
          break;
        case "callback":
          intervals.delete(entry.intervalId);
          break;
      }
    }
    for (const [, queue] of waiters) {
      while (queue.length > 0) ArrayPrototypeShift(queue).reject(error());
    }
  }

  function hasPendingTimers() {
    for (const [, entry] of pending) {
      if (entry.kind !== "tool") return true;
    }
    return false;
  }

  function hasSignalWaiters() {
    for (const [, queue] of waiters) {
      if (queue.length > 0) return true;
    }
    return false;
  }

  // ---------------------------------------------------------------------------------------
  // Console: journaled as annotations.

  function show(value) {
    if (typeof value === "string") return value;
    if (value instanceof Error) return value.stack ?? `${value.name}: ${value.message}`;
    try {
      return JSONStringify(value) ?? String(value);
    } catch {
      return String(value);
    }
  }

  function log(level, args) {
    const [line, column] = callsite();
    op_ptc_log(level, ArrayPrototypeJoin(ArrayPrototypeMap(args, show), " "), line, column);
  }

  globalThis.console = ObjectFreeze({
    log: (...args) => log("info", args),
    info: (...args) => log("info", args),
    debug: (...args) => log("debug", args),
    warn: (...args) => log("warn", args),
    error: (...args) => log("error", args),
  });

  // ---------------------------------------------------------------------------------------
  // Hooks driven by Rust.

  function start(inputJson) {
    let result;
    try {
      result = main(JSONParse(inputJson));
    } catch (error) {
      mainState = "threw";
      fail(error);
      return;
    }
    PromisePrototypeThen(
      PromiseResolve(result),
      (value) => {
        mainState = "returned";
        output = serializeOutput(value);
      },
      (error) => {
        mainState = "threw";
        fail(error);
      },
    );
  }

  function serializeOutput(value) {
    try {
      return wellFormed(JSONStringify(value === undefined ? null : value) ?? "null");
    } catch (error) {
      fail(new TypeError(`the output of main is not serializable to JSON: ${error.message}`));
      return undefined;
    }
  }

  function deliver(completionJson) {
    if (ended) return;
    const completion = JSONParse(completionJson);
    switch (completion.type) {
      case "tool_call_completed": {
        const entry = take(completion.seq);
        if (entry === undefined) break;
        toolsInFlight--;
        if (failure === undefined) entry.resolve(completion.result);
        break;
      }
      case "tool_call_failed": {
        const entry = take(completion.seq);
        if (entry === undefined) break;
        toolsInFlight--;
        if (failure === undefined) entry.reject(new ToolError(entry.tool, completion));
        break;
      }
      case "timer_fired":
        if (failure === undefined) fireTimer(completion.seq);
        break;
      case "signal_received":
        if (failure === undefined) receiveSignal(completion.name, completion.payload);
        break;
      case "cancel_requested":
        if (failure === undefined) {
          cancellation = completion.message;
          cancelAll(completion.message);
        } else {
          // Already failing: stop waiting for the calls in flight.
          pending.clear();
          toolsInFlight = 0;
        }
        break;
      default:
        throw new TypeError(`unknown completion: ${completion.type}`);
    }
  }

  // Called once microtasks are drained: decides whether the script has ended. Once it has,
  // the terminal command is emitted from what was captured: no script code runs any more.
  function idle() {
    if (ended) return;
    // Rejections still without a handler after the microtask checkpoint are unhandled.
    const rejections = op_drain_pending_rejections();
    if (rejections !== undefined && rejections.length > 0) fail(rejections[1]);

    if (failure !== undefined) {
      if (toolsInFlight === 0) end();
    } else if (mainState === "returned") {
      if (toolsInFlight === 0) end();
    } else if (toolsInFlight === 0 && !hasPendingTimers() && !hasSignalWaiters()) {
      // Nothing can wake the script any more. After a cancellation, that is how it ends.
      fail(cancellation === undefined ? new DeadlockError() : new CancelledError(cancellation));
      end();
    }
  }

  function end() {
    ended = true;
    pending.clear();
    if (failure !== undefined) {
      op_ptc_fail(failure.name, failure.message, failure.stack);
    } else {
      op_ptc_complete(output);
    }
  }

  // ---------------------------------------------------------------------------------------
  // Remove ambient and non-deterministic capabilities.

  for (
    const name of [
      "WeakRef",
      "FinalizationRegistry",
      "SharedArrayBuffer",
      "Atomics",
      "Deno",
      "__bootstrap",
    ]
  ) {
    delete globalThis[name];
  }

  const run = ObjectFreeze({
    id: op_ptc_run_id(),
    get startedAt() {
      return new NativeDate(startedAt);
    },
  });

  globalThis.__ptcHandoff = ObjectFreeze({
    callTool,
    sleep,
    signal,
    run,
    ToolError,
    CancelledError,
    setMain(fn) {
      if (typeof fn !== "function") {
        throw new TypeError("the script must `export default` its main function");
      }
      main = fn;
    },
  });

  return { start, deliver, idle };
})(globalThis);
