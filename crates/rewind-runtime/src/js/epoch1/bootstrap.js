// Bootstrap of the Rewind sandbox, determinism epoch 1.
//
// FROZEN once 0.1 ships: a change a code can observe (a seq, a digest, a value, a global)
// needs a new epoch (archi/03-execution.md, "Determinism epoch").
//
// It runs once per process, when the epoch's V8 startup snapshot is built: it makes every
// source of non-determinism deterministic, installs the web-standard globals of deno_web,
// and leaves the hooks Rust drives the code with in `globalThis.__rewindHooks`. Every instance
// starts from that snapshot. Rust takes the hooks, then calls init(config) with what belongs to
// the run (its id, its start, its seed, whether the action set has the http action); init removes the
// ambient capabilities, `Deno`, `__bootstrap` and the hooks from the global object before any
// module is loaded. The `rewind` modules get their hooks through a global that
// `rewind:internal` deletes before any user code runs.
//
// Nothing here is asynchronous from V8's point of view: ops record a command and return its
// seq, and promises are settled when Rust delivers the matching completion.

((globalThis) => {
  "use strict";

  const { core, primordials } = globalThis.__bootstrap;
  const {
    ArrayBufferIsView,
    ArrayBufferPrototypeGetByteLength,
    ArrayBufferPrototypeSlice,
    ArrayIsArray,
    ArrayPrototypeFilter,
    ArrayPrototypeIndexOf,
    ArrayPrototypeJoin,
    ArrayPrototypeMap,
    ArrayPrototypePush,
    ArrayPrototypeShift,
    ArrayPrototypeSort,
    ArrayPrototypeSplice,
    ArrayPrototypeValues,
    Error,
    JSONParse,
    JSONStringify,
    NumberIsInteger,
    NumberParseInt,
    ObjectCreate,
    ObjectDefineProperty,
    ObjectEntries,
    ObjectFreeze,
    ObjectGetOwnPropertyDescriptor,
    ObjectPrototype,
    ObjectPrototypeIsPrototypeOf,
    ObjectSetPrototypeOf,
    Promise,
    PromisePrototypeThen,
    PromiseReject,
    PromiseResolve,
    RangeError,
    ReflectApply,
    ReflectConstruct,
    RegExpPrototypeExec,
    RegExpPrototypeSymbolReplace,
    SafeMap,
    SafeSet,
    String,
    StringPrototypeStartsWith,
    StringPrototypePadStart,
    StringPrototypeSlice,
    StringPrototypeSplit,
    StringPrototypeToLowerCase,
    StringPrototypeToUpperCase,
    StringPrototypeTrim,
    SymbolIterator,
    TypeError,
    TypedArrayPrototypeGetBuffer,
    TypedArrayPrototypeGetByteLength,
    TypedArrayPrototypeGetByteOffset,
    Uint8Array,
  } = primordials;

  const {
    op_drain_pending_rejections,
    op_rewind_call_action,
    op_rewind_check_workflow_output,
    op_rewind_heap_exceeded,
    op_rewind_cancel_timer,
    op_rewind_complete,
    op_rewind_fail,
    op_rewind_log,
    op_rewind_now,
    op_rewind_random_fill,
    op_rewind_signal_awaited,
    op_rewind_start_timer,
  } = core.ops;

  // ---------------------------------------------------------------------------------------
  // Web-standard globals, from deno_web: pure and deterministic. Their non-deterministic
  // parts (timers, performance, blobs, message ports) are never installed.

  const { URL, URLSearchParams } = core.loadExtScript("ext:deno_web/00_url.js");
  const { TextDecoder, TextEncoder } = core.loadExtScript("ext:deno_web/08_text_encoding.js");
  const { atob, btoa } = core.loadExtScript("ext:deno_web/05_base64.js");
  const { structuredClone: cloneStructured } = core.loadExtScript("ext:deno_web/13_message_port.js");
  const { forgivingBase64Decode, forgivingBase64Encode } = core.loadExtScript(
    "ext:deno_web/00_infra.js",
  );
  // Blob URLs need the blob store, which the sandbox does not have. Loading the module that
  // adds them first means nothing adds them back later.
  core.loadExtScript("ext:deno_web/09_file.js");
  delete URL.createObjectURL;
  delete URL.revokeObjectURL;

  function defineGlobal(name, value, writable = true) {
    ObjectDefineProperty(globalThis, name, { value, writable, configurable: true });
  }

  for (
    const [name, value] of [
      ["URL", URL],
      ["URLSearchParams", URLSearchParams],
      ["TextEncoder", TextEncoder],
      ["TextDecoder", TextDecoder],
      ["atob", atob],
      ["btoa", btoa],
    ]
  ) {
    defineGlobal(name, value);
  }

  // ---------------------------------------------------------------------------------------
  // Array buffers. Their memory lives outside V8's heap, under a budget per instance that the
  // isolate's allocator enforces: past it, an allocation fails with V8's RangeError. A resizable
  // buffer escapes the allocator (V8 reserves its pages itself), so it takes its maximum length
  // from the budget when it is created, through a buffer of that length that lives exactly as
  // long as it does. It does so before V8 reserves anything: V8 aborts the process when one
  // buffer changes its accounting by 32 GiB. V8 creates a resizable buffer only through the
  // constructor, `transfer` and `structuredClone`, so those three are covered.

  const NativeArrayBuffer = primordials.ArrayBuffer;
  const ArrayBufferPrototype = NativeArrayBuffer.prototype;
  const MAX_SAFE_INTEGER = 2 ** 53 - 1;
  // A resizable buffer -> the buffer charged with its maximum length.
  const reservations = new primordials.SafeWeakMap();
  // Resizable buffers created so far: until there is one, no clone can hold one.
  let resizableBuffers = 0;

  const toIntegerOrInfinity = (number) => (number !== number ? 0 : primordials.MathTrunc(number));
  const isIndex = (integer) => integer >= 0 && integer <= MAX_SAFE_INTEGER;

  function reserve(buffer, maxByteLength) {
    primordials.WeakMapPrototypeSet(reservations, buffer, new NativeArrayBuffer(maxByteLength));
  }

  // Converts the arguments in the specification's order, and lets V8 itself throw the
  // RangeError of a length that is not an index.
  const SandboxArrayBuffer = function ArrayBuffer(length, options) {
    if (new.target === undefined) return NativeArrayBuffer(length, options);
    const byteLength = +length;
    const integer = toIntegerOrInfinity(byteLength);
    const object = options !== null && (typeof options === "object" || typeof options === "function");
    if (!isIndex(integer) || !object) {
      return ReflectConstruct(NativeArrayBuffer, [byteLength], new.target);
    }
    const max = options.maxByteLength;
    if (max === undefined) return ReflectConstruct(NativeArrayBuffer, [byteLength], new.target);
    const maxByteLength = +max;
    const maxInteger = toIntegerOrInfinity(maxByteLength);
    const resizable = { __proto__: null, maxByteLength };
    if (!isIndex(maxInteger) || integer > maxInteger) {
      return ReflectConstruct(NativeArrayBuffer, [byteLength, resizable], new.target);
    }
    const reservation = new NativeArrayBuffer(maxInteger);
    const buffer = ReflectConstruct(NativeArrayBuffer, [byteLength, resizable], new.target);
    primordials.WeakMapPrototypeSet(reservations, buffer, reservation);
    resizableBuffers++;
    return buffer;
  };
  for (const key of ["isView", primordials.SymbolSpecies]) {
    ObjectDefineProperty(SandboxArrayBuffer, key, ObjectGetOwnPropertyDescriptor(NativeArrayBuffer, key));
  }
  ObjectDefineProperty(SandboxArrayBuffer, "prototype", { value: ArrayBufferPrototype, writable: false });
  ObjectDefineProperty(SandboxArrayBuffer, "length", { value: 1 });
  ObjectDefineProperty(ArrayBufferPrototype, "constructor", {
    value: SandboxArrayBuffer,
    writable: true,
    configurable: true,
  });
  defineGlobal("ArrayBuffer", SandboxArrayBuffer);

  // A transfer moves the reservation of a resizable buffer to the new buffer, which keeps its
  // maximum length; `transferToFixedLength` releases it (the new buffer is the allocator's).
  for (const name of ["transfer", "transferToFixedLength"]) {
    const native = ArrayBufferPrototype[name];
    const method = {
      [name](newLength) {
        const reservation = primordials.WeakMapPrototypeGet(reservations, this);
        const buffer = ReflectApply(native, this, [newLength]);
        primordials.WeakMapPrototypeDelete(reservations, this);
        if (primordials.ArrayBufferPrototypeGetResizable(buffer)) {
          if (reservation !== undefined) {
            primordials.WeakMapPrototypeSet(reservations, buffer, reservation);
          } else {
            reserve(buffer, primordials.ArrayBufferPrototypeGetMaxByteLength(buffer));
          }
        }
        return buffer;
      },
    }[name];
    ObjectDefineProperty(method, "length", { value: 0 });
    ObjectDefineProperty(ArrayBufferPrototype, name, {
      value: method,
      writable: true,
      configurable: true,
    });
  }

  // A clone copies the resizable buffers it holds: each copy takes its own reservation. A
  // clone is plain data made by V8 (no accessor, no proxy), so walking it runs no code code.
  function reserveClonedBuffers(clone) {
    const seen = new primordials.SafeSet();
    const stack = [clone];
    while (stack.length > 0) {
      const value = primordials.ArrayPrototypePop(stack);
      if (value === null || typeof value !== "object" || seen.has(value)) continue;
      seen.add(value);
      const prototype = primordials.ObjectGetPrototypeOf(value);
      if (prototype === ArrayBufferPrototype) {
        if (
          primordials.ArrayBufferPrototypeGetResizable(value) &&
          !primordials.WeakMapPrototypeHas(reservations, value)
        ) {
          reserve(value, primordials.ArrayBufferPrototypeGetMaxByteLength(value));
        }
      } else if (ArrayBufferIsView(value)) {
        ArrayPrototypePush(
          stack,
          prototype === primordials.DataView.prototype
            ? primordials.DataViewPrototypeGetBuffer(value)
            : TypedArrayPrototypeGetBuffer(value),
        );
      } else if (prototype === primordials.Map.prototype) {
        primordials.MapPrototypeForEach(value, (entry, key) => {
          ArrayPrototypePush(stack, key);
          ArrayPrototypePush(stack, entry);
        });
      } else if (prototype === primordials.Set.prototype) {
        primordials.SetPrototypeForEach(value, (entry) => ArrayPrototypePush(stack, entry));
      } else if (prototype !== primordials.String.prototype) {
        const keys = primordials.ReflectOwnKeys(value);
        for (let i = 0; i < keys.length; i++) ArrayPrototypePush(stack, value[keys[i]]);
      }
    }
  }

  const sandboxStructuredClone = {
    structuredClone(value, options) {
      const clone = ReflectApply(cloneStructured, undefined, arguments);
      if (resizableBuffers > 0) reserveClonedBuffers(clone);
      return clone;
    },
  }.structuredClone;
  ObjectDefineProperty(sandboxStructuredClone, "length", { value: cloneStructured.length });
  defineGlobal("structuredClone", sandboxStructuredClone);

  // ---------------------------------------------------------------------------------------
  // Built-ins V8 runs to their end. The CPU budget and the heap limit terminate the execution
  // at V8's next interrupt check, and some built-ins create elements one by one over a whole
  // length without one: `new Array(2 ** 27).fill(1.5)` ran 18 s and took 1.9 GiB under a
  // 64 MiB heap, and past 2^27 elements V8 aborts the process ("invalid size"). An element takes
  // at least 8 bytes of heap, so a call that must create more than the heap holds can only
  // exceed the heap limit: it ends the call at once, as the heap limit does. A call is measured
  // only from what reading cannot run code code: the length of an array that is not a proxy,
  // or an own data property `length`, and arguments that are primitives. Anything else goes to
  // V8 unchecked.

  let maxElements = Infinity; // set by init: min(heap limit / 8, 2^27)
  const MAX_ARRAY_LENGTH = 2 ** 32 - 1;
  const isProxy = core.isProxy;
  const isPrimitive = (value) =>
    value === null || (typeof value !== "object" && typeof value !== "function");

  function exceedHeap() {
    op_rewind_heap_exceeded();
    // The termination takes effect at the loop's interrupt check.
    for (;;) {
      // Nothing.
    }
  }

  const toNumberOrUndefined = (value) =>
    isPrimitive(value) && typeof value !== "symbol" && typeof value !== "bigint" ? +value : undefined;

  // ToLength of a primitive; undefined when converting could run code code or throw.
  function toLength(value) {
    const number = toNumberOrUndefined(value);
    if (number === undefined) return undefined;
    const integer = toIntegerOrInfinity(number);
    return integer <= 0 ? 0 : integer > MAX_SAFE_INTEGER ? MAX_SAFE_INTEGER : integer;
  }

  // The length a built-in reads from `object`, when reading it cannot run code code.
  function plainLength(object) {
    if (isPrimitive(object) || isProxy(object)) return undefined;
    if (ArrayIsArray(object)) return object.length;
    const descriptor = ObjectGetOwnPropertyDescriptor(object, "length");
    if (descriptor === undefined || !primordials.ObjectHasOwn(descriptor, "value")) return undefined;
    return toLength(descriptor.value);
  }

  // A relative index (`start`, `end`) resolved against `length`, as the specification does.
  function relativeIndex(value, length, absent) {
    if (value === undefined) return absent;
    const number = toNumberOrUndefined(value);
    if (number === undefined) return undefined;
    const relative = toIntegerOrInfinity(number);
    return relative < 0 ? primordials.MathMax(length + relative, 0) : primordials.MathMin(relative, length);
  }

  // Whether nothing on `object`'s prototype chain, `object` included, has `key`, and no proxy
  // is on it: looking `key` up then runs no code code.
  function plainlyLacks(object, key) {
    for (let current = object; current !== null; current = primordials.ObjectGetPrototypeOf(current)) {
      if (isProxy(current) || ObjectGetOwnPropertyDescriptor(current, key) !== undefined) return false;
    }
    return true;
  }

  // Runs `check` with the call's receiver and arguments, then the built-in.
  function guard(target, name, check) {
    const native = target[name];
    const method = {
      [name]() {
        ReflectApply(check, this, arguments);
        return ReflectApply(native, this, arguments);
      },
    }[name];
    ObjectDefineProperty(method, "length", { value: native.length });
    ObjectDefineProperty(target, name, { value: method, writable: true, configurable: true });
  }

  // A new array of `length` elements: V8 throws its RangeError past 2^32 - 1.
  function creates(length) {
    if (length !== undefined && length > maxElements && length <= MAX_ARRAY_LENGTH) exceedHeap();
  }

  const ArrayPrototype = primordials.Array.prototype;

  guard(ArrayPrototype, "fill", function (value, start, end) {
    const length = plainLength(this);
    if (length === undefined || length <= maxElements) return;
    const first = relativeIndex(start, length, 0);
    const final = relativeIndex(end, length, length);
    if (first !== undefined && final !== undefined && final - first > maxElements) exceedHeap();
  });

  guard(ArrayPrototype, "toReversed", function () {
    creates(plainLength(this));
  });

  guard(ArrayPrototype, "toSorted", function (compareFn) {
    if (compareFn === undefined || typeof compareFn === "function") creates(plainLength(this));
  });

  guard(ArrayPrototype, "with", function (index, value) {
    const length = plainLength(this);
    if (length === undefined || length <= maxElements) return;
    const number = toNumberOrUndefined(index);
    if (number === undefined) return;
    const relative = toIntegerOrInfinity(number);
    const actual = relative >= 0 ? relative : length + relative;
    // V8 throws its RangeError for an index out of range.
    if (actual >= 0 && actual < length) creates(length);
  });

  guard(ArrayPrototype, "toSpliced", function (start, skipCount) {
    const length = plainLength(this);
    if (length === undefined || length <= maxElements) return;
    const actualStart = relativeIndex(start, length, 0);
    if (actualStart === undefined) return;
    const inserted = primordials.MathMax(arguments.length - 2, 0);
    let skipped;
    if (arguments.length === 0) {
      skipped = 0;
    } else if (arguments.length === 1) {
      skipped = length - actualStart;
    } else {
      const number = toNumberOrUndefined(skipCount);
      if (number === undefined) return;
      skipped = primordials.MathMin(primordials.MathMax(toIntegerOrInfinity(number), 0), length - actualStart);
    }
    creates(length + inserted - skipped);
  });

  const NativeArray = primordials.Array;
  guard(NativeArray, "from", function (items, mapFn) {
    // With a function to map elements, each element runs code code, which checks for
    // interrupts. An iterable goes through the iteration protocol, which does too.
    if (this !== NativeArray || mapFn !== undefined || isPrimitive(items)) return;
    if (!plainlyLacks(items, primordials.SymbolIterator)) return;
    creates(plainLength(items));
  });

  const StringPrototype = primordials.String.prototype;
  guard(StringPrototype, "split", function (separator, limit) {
    if (typeof this !== "string" || typeof separator !== "string") return;
    if (this.length < maxElements || !plainlyLacks(StringPrototype, primordials.SymbolSplit)) return;
    const lim = limit === undefined ? MAX_ARRAY_LENGTH : toNumberOrUndefined(limit);
    if (lim === undefined) return;
    const most = lim >>> 0;
    let parts;
    if (separator === "") {
      parts = primordials.MathMin(this.length, most);
    } else {
      parts = 1;
      let at = primordials.StringPrototypeIndexOf(this, separator, 0);
      while (at !== -1 && parts <= maxElements) {
        parts++;
        at = primordials.StringPrototypeIndexOf(this, separator, at + separator.length);
      }
      parts = primordials.MathMin(parts, most);
    }
    if (parts > maxElements) exceedHeap();
  });

  // ---------------------------------------------------------------------------------------
  // Stacks: the code's own frames only. Frames of the runtime's modules (`rewind:…`) and of
  // the web globals (`ext:…`) would leak their line numbers into histories.

  const CODE_FRAME = /file:\/\/\/workflow\.ts:(\d+):(\d+)/;
  // Such as "    at start (rewind:bootstrap:467:16)" or "    at new URL (ext:deno_web/00_url.js:1:2)".
  const INTERNAL_FRAME = /^\s+at (?:.* \()?(?:rewind:\w+|ext:[\w/.-]+):\d+:\d+\)?$/;

  function callsite() {
    const stack = new Error().stack ?? "";
    for (const line of StringPrototypeSplit(stack, "\n")) {
      const match = RegExpPrototypeExec(CODE_FRAME, line);
      if (match) return [NumberParseInt(match[1], 10), NumberParseInt(match[2], 10)];
    }
    return [0, 0];
  }

  function codeStack(stack) {
    return ArrayPrototypeJoin(
      ArrayPrototypeFilter(
        StringPrototypeSplit(String(stack ?? ""), "\n"),
        (line) => RegExpPrototypeExec(INTERNAL_FRAME, line) === null,
      ),
      "\n",
    );
  }

  // An error raised by the glue on behalf of a call the code made: a fresh error of the same
  // kind, whose stack starts at the code's call.
  function atCallSite(error) {
    const Kind = ObjectPrototypeIsPrototypeOf(RangeError.prototype, error) ? RangeError : TypeError;
    const raised = new Kind(String(error?.message ?? error));
    ObjectDefineProperty(raised, "stack", {
      value: codeStack(raised.stack),
      writable: true,
      configurable: true,
    });
    return raised;
  }

  // ---------------------------------------------------------------------------------------
  // Logical time: the recorded_at of the last delivered trigger.

  const NativeDate = globalThis.Date;
  const DateTimeFormat = globalThis.Intl.DateTimeFormat;
  if (new DateTimeFormat().resolvedOptions().timeZone !== "UTC") {
    throw new Error("Rewind requires the process to run with TZ=UTC");
  }

  const now = () => op_rewind_now();
  let startedAt = 0; // set by init

  const LogicalDate = function Date(...args) {
    if (new.target === undefined) return String(new NativeDate(now()));
    if (args.length === 0) return ReflectConstruct(NativeDate, [now()], new.target);
    return ReflectConstruct(NativeDate, args, new.target);
  };
  // NativeDate stays private: through it, a code would read the real clock. Only its pure
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

  // V8 installs Temporal when an isolate starts from the snapshot, not in it: init calls this.
  function logicalTemporalNow() {
    const Temporal = globalThis.Temporal;
    if (Temporal === undefined) return;
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

  // ---------------------------------------------------------------------------------------
  // Randomness: sfc32 for Math.random, ChaCha20 (in Rust) for crypto. Both are keyed by the
  // run's seed, each with its own derived key. init sets the state of sfc32.

  let a = 0, b = 0, c = 0, d = 0;
  function sfc32() {
    const t = (((a + b) | 0) + d) | 0;
    d = (d + 1) | 0;
    a = b ^ (b >>> 9);
    b = (c + (c << 3)) | 0;
    c = (c << 21) | (c >>> 11);
    c = (c + t) | 0;
    return t >>> 0;
  }
  globalThis.Math.random = () => sfc32() / 4294967296;

  const hex = (byte) => StringPrototypePadStart(byte.toString(16), 2, "0");
  defineGlobal(
    "crypto",
    ObjectFreeze({
      getRandomValues(array) {
        if (!ArrayBufferIsView(array) || array instanceof globalThis.DataView) {
          throw new TypeError("crypto.getRandomValues expects an integer typed array");
        }
        op_rewind_random_fill(new Uint8Array(array.buffer, array.byteOffset, array.byteLength));
        return array;
      },
      randomUUID() {
        const bytes = new Uint8Array(16);
        op_rewind_random_fill(bytes);
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        const h = ArrayPrototypeJoin(ArrayPrototypeMap(bytes, hex), "");
        return `${StringPrototypeSlice(h, 0, 8)}-${StringPrototypeSlice(h, 8, 12)}-${
          StringPrototypeSlice(h, 12, 16)
        }-${StringPrototypeSlice(h, 16, 20)}-${StringPrototypeSlice(h, 20)}`;
      },
    }),
    false,
  );

  // ---------------------------------------------------------------------------------------
  // Errors the code can see. They are created by the glue, outside the code's code, so
  // their stack is only their name and message: no frame of the glue leaks into a history.

  function ownStack(error) {
    ObjectDefineProperty(error, "stack", {
      value: `${error.name}: ${error.message}`,
      writable: true,
      configurable: true,
    });
  }

  class ActionError extends Error {
    constructor(action, failure) {
      super(`${action} failed: ${failure.message}`);
      this.name = "ActionError";
      this.action = action;
      this.outcomeUnknown = failure.outcomeUnknown;
      this.data = failure.data ?? undefined;
      ownStack(this);
    }
  }

  // Code written before actions were named catches a `ToolError`, the same class, and reads its
  // `tool`, the same value as `action` (archi/08-decisions.md, R1). Neither is declared.
  ObjectDefineProperty(ActionError.prototype, "tool", {
    get() {
      return this.action;
    },
    configurable: true,
  });

  class CancelledError extends Error {
    constructor(message) {
      super(message);
      this.name = "CancelledError";
      ownStack(this);
    }
  }

  // A run this run started ended badly, or its output does not match its workflow's schema.
  class RunError extends Error {
    constructor(workflow, runId, status, detail) {
      const what = status === "invalid_output"
        ? "returned an output its schema refuses"
        : status === "failed"
        ? "failed"
        : `was ${status}`;
      super(`workflow ${workflow} (run ${runId}) ${what}: ${detail}`);
      this.name = "RunError";
      this.workflow = workflow;
      this.runId = runId;
      this.status = status;
      ownStack(this);
    }
  }

  class DeadlockError extends Error {
    constructor() {
      super(
        "main has not settled, and nothing can wake the code: no call, timer or signal wait is pending",
      );
      this.name = "DeadlockError";
      ownStack(this);
    }
  }

  // ---------------------------------------------------------------------------------------
  // How a code ends (archi/03-execution.md): the outcome is evaluated each time the code
  // goes idle, and the terminal command waits until no call is in flight.

  let main;
  let mainState = "running"; // "returned" | "threw"
  // The JSON of main's return value, serialized as soon as main returns: serializing runs the
  // code's code (toJSON, getters), which must not happen once the outcome is decided.
  let output;
  // { name, message, stack }, captured as soon as the run is known to fail. From then on, no
  // code code runs any more.
  let failure;
  let cancellation; // the message of the cancellation, once one was requested
  let ended = false; // the terminal command was emitted
  let callsInFlight = 0;

  // seq -> { kind: "call" | "sleep" | "timeout" | "callback", ... }
  const pending = new SafeMap();

  function take(seq) {
    const entry = pending.get(seq);
    if (entry !== undefined) pending.delete(seq);
    return entry;
  }

  function fail(error) {
    if (failure === undefined) failure = describe(error);
  }

  function describe(error) {
    try {
      if (!(error instanceof Error)) return { name: "Error", message: show(error), stack: "" };
      return {
        name: String(error.name),
        message: String(error.message),
        stack: codeStack(error.stack),
      };
    } catch {
      return { name: "Error", message: "the code failed with a value that cannot be described", stack: "" };
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

  // What an MCP action pinned without an output schema returns, `{ content, structuredContent? }`,
  // reaches the code with two helpers (archi/04-tools.md, "Action sets and code generation"),
  // as `rewind-actions.d.ts` declares them in `ActionContent`: `text()` joins the text blocks with
  // newlines; `json()` is `structuredContent` when the action sent it, else `text()` parsed as
  // JSON. They live on a frozen prototype, not enumerable: the value's own keys stay the
  // result's, what JSON.stringify and the history see.
  const ActionContentPrototype = ObjectFreeze(
    ObjectCreate(ObjectPrototype, {
      text: {
        value: function text() {
          const content = this.content;
          if (!ArrayIsArray(content)) return "";
          const texts = [];
          for (let i = 0; i < content.length; i++) {
            const block = content[i];
            if (block !== null && typeof block === "object" && block.type === "text") {
              ArrayPrototypePush(texts, String(block.text ?? ""));
            }
          }
          return ArrayPrototypeJoin(texts, "\n");
        },
      },
      json: {
        value: function json() {
          if (this.structuredContent !== undefined) return this.structuredContent;
          return JSONParse(ReflectApply(ActionContentPrototype.text, this, []));
        },
      },
    }),
  );

  function actionContent(result) {
    if (result !== null && typeof result === "object" && !ArrayIsArray(result)) {
      ObjectSetPrototypeOf(result, ActionContentPrototype);
    }
    return result;
  }

  function callActionContent(action, args) {
    return callAction(action, args, true);
  }

  // A call: the op checks the arguments against the action's pinned input schema and
  // records the command. A call without arguments sends `{}`. A call refused before it is
  // recorded rejects with an error whose stack starts at the code's call. `content`: the
  // result is MCP content, which gets the helpers of `ActionContent`.
  function callAction(action, args, content = false) {
    let json;
    try {
      json = JSONStringify(args === undefined ? {} : args);
    } catch (error) {
      return PromiseReject(
        atCallSite(`the arguments of ${action} are not serializable to JSON: ${error.message}`),
      );
    }
    if (json === undefined) {
      return PromiseReject(atCallSite(`the arguments of ${action} are not serializable to JSON`));
    }
    const [line, column] = callsite();
    let seq;
    try {
      seq = op_rewind_call_action(action, wellFormed(json), line, column);
    } catch (error) {
      return PromiseReject(atCallSite(error));
    }
    callsInFlight++;
    return new Promise((resolve, reject) => {
      pending.set(seq, { kind: "call", action, content, resolve, reject });
    });
  }

  function sleep(duration) {
    const [line, column] = callsite();
    const seq = op_rewind_start_timer(durationArg(duration), line, column);
    return new Promise((resolve, reject) => {
      pending.set(seq, { kind: "sleep", resolve, reject });
    });
  }

  // `signalUrl(name)`: a call of the built-in `signalUrl` action, which returns `{ url }`.
  function signalUrl(name) {
    return callAction("signalUrl", { name: String(name) });
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
    const seq = op_rewind_start_timer(String(ms), line, column);
    pending.set(seq, { kind: "callback", callback, delay: ms, args, intervalId });
    return seq;
  }

  function clearTimer(id) {
    const seq = intervals.get(id) ?? id;
    intervals.delete(id);
    const entry = pending.get(seq);
    if (entry !== undefined && entry.kind === "callback") {
      pending.delete(seq);
      op_rewind_cancel_timer(seq);
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

  // Names starting with this prefix are Rewind's: no mailbox the code reads holds them.
  const RESERVED = "rewind:";

  function signal(name, options) {
    name = String(name);
    if (StringPrototypeStartsWith(name, RESERVED)) {
      return PromiseReject(
        atCallSite(new TypeError(`signal names starting with "${RESERVED}" are reserved: ${name}`)),
      );
    }
    const mailbox = mailboxes.get(name);
    if (mailbox !== undefined && mailbox.length > 0) {
      return PromiseResolve(ArrayPrototypeShift(mailbox));
    }
    const [line, column] = callsite();
    return new Promise((resolve, reject) => {
      const waiter = { resolve, reject, timeoutSeq: undefined };
      if (options?.timeout !== undefined) {
        waiter.timeoutSeq = op_rewind_start_timer(durationArg(options.timeout), line, column);
        pending.set(waiter.timeoutSeq, { kind: "timeout", name, waiter });
      }
      op_rewind_signal_awaited(name, waiter.timeoutSeq ?? 0, line, column);
      let queue = waiters.get(name);
      if (queue === undefined) {
        queue = [];
        waiters.set(name, queue);
      }
      ArrayPrototypePush(queue, waiter);
    });
  }

  function receiveSignal(name, payload) {
    if (StringPrototypeStartsWith(name, ENDED)) {
      receiveEnd(StringPrototypeSlice(name, ENDED.length), payload);
      return;
    }
    const queue = waiters.get(name);
    if (queue !== undefined && queue.length > 0) {
      const waiter = ArrayPrototypeShift(queue);
      if (waiter.timeoutSeq !== undefined && take(waiter.timeoutSeq) !== undefined) {
        op_rewind_cancel_timer(waiter.timeoutSeq);
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

  // ---------------------------------------------------------------------------------------
  // Workflows across runs (archi/03-execution.md): `workflows.<name>.start` and
  // `workflows.<name>.signal` are calls. The end of a run this run started, or watches,
  // comes back as the signal `rewind:ended:<run id>`, which the glue takes itself: it never
  // reaches a mailbox the code reads. An end may arrive before the start's result: it is kept.

  const ENDED = "rewind:ended:";
  const ends = new SafeMap(); // run id -> its end, as the signal carried it
  const attached = new SafeSet(); // runs started attached whose end has not arrived

  function workflow(name) {
    return ObjectFreeze({
      name,
      start(input, options = undefined) {
        const args = { input: input === undefined ? null : input };
        if (options !== undefined && options !== null) {
          // The options go to the op as given, which refuses those it does not know.
          for (const [key, value] of ObjectEntries(options)) {
            if (key === "input") {
              return PromiseReject(
                atCallSite(
                  `${name}.start() takes no option "input": its options are detached, labels and ttl`,
                ),
              );
            }
            if (value !== undefined) args[key] = value;
          }
        }
        const detached = args.detached === true;
        return PromisePrototypeThen(callAction(`workflows.${name}.start`, args), (result) => {
          const runId = String(result.runId);
          if (!detached && !ends.has(runId)) attached.add(runId);
          return workflowRun(name, runId, true);
        });
      },
      get(runId) {
        return workflowRun(name, String(runId), false);
      },
    });
  }

  // A handle on a run of the workflow `name`: one this run started has `result()`.
  function workflowRun(name, runId, started) {
    const handle = {
      id: runId,
      signal(signalName, payload = undefined) {
        const args = {
          run: runId,
          name: String(signalName),
          payload: payload === undefined ? null : payload,
        };
        return PromisePrototypeThen(
          callAction(`workflows.${name}.signal`, args),
          () => undefined,
        );
      },
    };
    if (started) {
      handle.result = (options = undefined) => awaitEnd(name, runId, options);
    }
    return ObjectFreeze(handle);
  }

  // The run's output, or a RunError; with a timeout, `undefined` if the run has not ended
  // in time. A wait on the end is a signal wait: `signal_awaited` names `rewind:ended:<run id>`.
  function awaitEnd(name, runId, options) {
    if (ends.has(runId)) {
      const settled = settleEnd(name, runId, ends.get(runId));
      return settled.ok ? PromiseResolve(settled.value) : PromiseReject(settled.value);
    }
    const signalName = ENDED + runId;
    const [line, column] = callsite();
    return new Promise((resolve, reject) => {
      const waiter = { resolve, reject, timeoutSeq: undefined, workflow: name };
      if (options?.timeout !== undefined) {
        waiter.timeoutSeq = op_rewind_start_timer(durationArg(options.timeout), line, column);
        pending.set(waiter.timeoutSeq, { kind: "timeout", name: signalName, waiter });
      }
      op_rewind_signal_awaited(signalName, waiter.timeoutSeq ?? 0, line, column);
      let queue = waiters.get(signalName);
      if (queue === undefined) {
        queue = [];
        waiters.set(signalName, queue);
      }
      ArrayPrototypePush(queue, waiter);
    });
  }

  function receiveEnd(runId, end) {
    if (ends.has(runId)) return;
    ends.set(runId, end);
    attached.delete(runId);
    const queue = waiters.get(ENDED + runId);
    while (queue !== undefined && queue.length > 0) {
      const waiter = ArrayPrototypeShift(queue);
      if (waiter.timeoutSeq !== undefined && take(waiter.timeoutSeq) !== undefined) {
        op_rewind_cancel_timer(waiter.timeoutSeq);
      }
      const settled = settleEnd(waiter.workflow, runId, end);
      if (settled.ok) waiter.resolve(settled.value);
      else waiter.reject(settled.value);
    }
  }

  // What `result()` gives for an end: its output once checked against the pinned output schema,
  // or the error it rejects with.
  function settleEnd(name, runId, end) {
    switch (end?.status) {
      case "completed": {
        const output = end.output === undefined ? null : end.output;
        const refused = op_rewind_check_workflow_output(name, wellFormed(JSONStringify(output)));
        if (refused !== "") {
          return { ok: false, value: new RunError(name, runId, "invalid_output", refused) };
        }
        return { ok: true, value: output };
      }
      case "failed":
      case "cancelled":
        return {
          ok: false,
          value: new RunError(
            name,
            runId,
            end.status,
            `${end.error?.name ?? "Error"}: ${end.error?.message ?? ""}`,
          ),
        };
      default:
        return {
          ok: false,
          value: new RunError(
            name,
            runId,
            "aborted",
            `${end?.reason?.kind ?? "internal"}: ${end?.reason?.message ?? ""}`,
          ),
        };
    }
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
        case "call":
          // Cancellation does not wait for the calls in flight: their results are dropped.
          callsInFlight--;
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
      if (entry.kind !== "call") return true;
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
  // fetch: a call of the built-in `http` action, present only when the action set enables it
  // (archi/04-tools.md, "The `http` action and egress"). The request and the response cross the
  // boundary as JSON, in the format of the action `rewind-mcp` implements (archi/08-decisions.md):
  //
  //   args   { url, method, headers: [[name, value], …], body?, bodyEncoding?: "base64" }
  //   result { status, statusText, url, redirected, headers: [[name, value], …], body,
  //            bodyEncoding?: "base64" }
  //
  // A body is text, or bytes in base64 with `"bodyEncoding": "base64"`. Reading a result, only
  // `status` is required: the other fields default to empty.

  const HTTP_TOKEN = /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/;
  const HEADER_VALUE = /^[^\0\r\n]*$/;
  const NORMALIZED_METHODS = ["DELETE", "GET", "HEAD", "OPTIONS", "POST", "PUT"];
  const FORBIDDEN_METHODS = ["CONNECT", "TRACE", "TRACK"];

  function headerName(name) {
    name = String(name);
    if (RegExpPrototypeExec(HTTP_TOKEN, name) === null) {
      throw new TypeError(`invalid header name: ${JSONStringify(name)}`);
    }
    return StringPrototypeToLowerCase(name);
  }

  function headerValue(value) {
    value = StringPrototypeTrim(String(value));
    if (RegExpPrototypeExec(HEADER_VALUE, value) === null) {
      throw new TypeError(`invalid header value: ${JSONStringify(value)}`);
    }
    return value;
  }

  function headerList(init) {
    const list = [];
    if (init === undefined || init === null) return list;
    if (ObjectPrototypeIsPrototypeOf(HeadersPrototype, init)) {
      for (const [name, value] of init) ArrayPrototypePush(list, [name, value]);
    } else if (typeof init[SymbolIterator] === "function") {
      for (const pair of init) {
        if (!ArrayIsArray(pair) || pair.length !== 2) {
          throw new TypeError("headers must be [name, value] pairs");
        }
        ArrayPrototypePush(list, [headerName(pair[0]), headerValue(pair[1])]);
      }
    } else if (typeof init === "object") {
      for (const [name, value] of ObjectEntries(init)) {
        ArrayPrototypePush(list, [headerName(name), headerValue(value)]);
      }
    } else {
      throw new TypeError("headers must be [name, value] pairs, an object or Headers");
    }
    return list;
  }

  function hasHeader(list, name) {
    for (const [key] of list) if (key === name) return true;
    return false;
  }

  function bytesOf(view) {
    if (ArrayBufferIsView(view)) {
      return new Uint8Array(
        TypedArrayPrototypeGetBuffer(view),
        TypedArrayPrototypeGetByteOffset(view),
        TypedArrayPrototypeGetByteLength(view),
      );
    }
    return new Uint8Array(view, 0, ArrayBufferPrototypeGetByteLength(view));
  }

  function httpRequest(input, init) {
    const url = new URL(ObjectPrototypeIsPrototypeOf(URL.prototype, input) ? input.href : String(input));
    if (url.protocol !== "http:" && url.protocol !== "https:") {
      throw new TypeError(`fetch only sends http and https requests, not ${url.protocol}`);
    }
    let method = init?.method === undefined ? "GET" : String(init.method);
    if (RegExpPrototypeExec(HTTP_TOKEN, method) === null) {
      throw new TypeError(`invalid method: ${JSONStringify(method)}`);
    }
    const upper = StringPrototypeToUpperCase(method);
    if (ArrayPrototypeIndexOf(FORBIDDEN_METHODS, upper) !== -1) {
      throw new TypeError(`the method ${method} is not allowed`);
    }
    if (ArrayPrototypeIndexOf(NORMALIZED_METHODS, upper) !== -1) method = upper;

    const headers = headerList(init?.headers);
    const request = { url: url.href, method, headers };
    const body = init?.body;
    if (body !== undefined && body !== null) {
      if (method === "GET" || method === "HEAD") {
        throw new TypeError(`a ${method} request cannot have a body`);
      }
      let contentType;
      if (typeof body === "string") {
        request.body = body;
        contentType = "text/plain;charset=UTF-8";
      } else if (ObjectPrototypeIsPrototypeOf(URLSearchParams.prototype, body)) {
        request.body = body.toString();
        contentType = "application/x-www-form-urlencoded;charset=UTF-8";
      } else if (ArrayBufferIsView(body) || body instanceof globalThis.ArrayBuffer) {
        request.body = forgivingBase64Encode(bytesOf(body));
        request.bodyEncoding = "base64";
      } else {
        throw new TypeError("a body is a string, URLSearchParams, an ArrayBuffer or a typed array");
      }
      if (contentType !== undefined && !hasHeader(headers, "content-type")) {
        ArrayPrototypePush(headers, ["content-type", contentType]);
      }
    }
    return request;
  }

  // The headers of a response, read-only. Iteration is sorted by name, and the values of a
  // name repeated are joined with ", ", as the Fetch standard does.
  class Headers {
    #list;
    constructor(list) {
      this.#list = list;
    }
    get(name) {
      name = StringPrototypeToLowerCase(String(name));
      const values = [];
      for (const [key, value] of this.#list) if (key === name) ArrayPrototypePush(values, value);
      return values.length === 0 ? null : ArrayPrototypeJoin(values, ", ");
    }
    has(name) {
      return hasHeader(this.#list, StringPrototypeToLowerCase(String(name)));
    }
    #combined() {
      const names = [];
      for (const [key] of this.#list) {
        if (ArrayPrototypeIndexOf(names, key) === -1) ArrayPrototypePush(names, key);
      }
      ArrayPrototypeSort(names);
      return ArrayPrototypeMap(names, (name) => [name, this.get(name)]);
    }
    forEach(callback, thisArg) {
      for (const [name, value] of this.#combined()) {
        ReflectApply(callback, thisArg, [value, name, this]);
      }
    }
    entries() {
      return ArrayPrototypeValues(this.#combined());
    }
    keys() {
      return ArrayPrototypeValues(ArrayPrototypeMap(this.#combined(), (entry) => entry[0]));
    }
    values() {
      return ArrayPrototypeValues(ArrayPrototypeMap(this.#combined(), (entry) => entry[1]));
    }
    [SymbolIterator]() {
      return this.entries();
    }
  }
  const HeadersPrototype = Headers.prototype;

  // The response of a request. Its body can be read once.
  class Response {
    #status;
    #statusText;
    #url;
    #redirected;
    #headers;
    #body;
    #base64;
    #used = false;
    constructor(result) {
      const notAResponse = () =>
        new TypeError("the http action returned something that is not a response");
      if (result === null || typeof result !== "object" || !NumberIsInteger(result.status)) {
        throw notAResponse();
      }
      const body = result.body ?? "";
      const encoding = result.bodyEncoding ?? "utf8";
      if (typeof body !== "string" || (encoding !== "utf8" && encoding !== "base64")) {
        throw notAResponse();
      }
      this.#status = result.status;
      this.#statusText = String(result.statusText ?? "");
      this.#url = String(result.url ?? "");
      this.#redirected = result.redirected === true;
      this.#headers = new Headers(
        ArrayPrototypeMap(
          ArrayIsArray(result.headers) ? result.headers : [],
          ([name, value]) => [StringPrototypeToLowerCase(String(name)), String(value)],
        ),
      );
      this.#body = body;
      this.#base64 = encoding === "base64";
    }
    get ok() {
      return this.#status >= 200 && this.#status <= 299;
    }
    get status() {
      return this.#status;
    }
    get statusText() {
      return this.#statusText;
    }
    get url() {
      return this.#url;
    }
    get redirected() {
      return this.#redirected;
    }
    get headers() {
      return this.#headers;
    }
    get bodyUsed() {
      return this.#used;
    }
    #read() {
      if (this.#used) throw new TypeError("the body of this response was already read");
      this.#used = true;
      return this.#body;
    }
    #bytes() {
      const body = this.#read();
      return this.#base64 ? forgivingBase64Decode(body) : new TextEncoder().encode(body);
    }
    #text() {
      const body = this.#read();
      return this.#base64 ? new TextDecoder().decode(forgivingBase64Decode(body)) : body;
    }
    text() {
      return settle(() => this.#text());
    }
    json() {
      return settle(() => JSONParse(this.#text()));
    }
    bytes() {
      return settle(() => this.#bytes());
    }
    arrayBuffer() {
      return settle(() => {
        const bytes = this.#bytes();
        const offset = TypedArrayPrototypeGetByteOffset(bytes);
        return ArrayBufferPrototypeSlice(
          TypedArrayPrototypeGetBuffer(bytes),
          offset,
          offset + TypedArrayPrototypeGetByteLength(bytes),
        );
      });
    }
  }

  function settle(read) {
    try {
      return PromiseResolve(read());
    } catch (error) {
      return PromiseReject(error);
    }
  }

  function fetch(input, init = undefined) {
    let request;
    try {
      request = httpRequest(input, init);
    } catch (error) {
      return PromiseReject(atCallSite(error));
    }
    return PromisePrototypeThen(callAction("http", request), (result) => new Response(result));
  }

  // ---------------------------------------------------------------------------------------
  // Console: recorded as annotations.

  function show(value) {
    if (typeof value === "string") return value;
    if (value instanceof Error) {
      return value.stack === undefined ? `${value.name}: ${value.message}` : codeStack(value.stack);
    }
    try {
      return JSONStringify(value) ?? String(value);
    } catch {
      return String(value);
    }
  }

  function log(level, args) {
    const [line, column] = callsite();
    op_rewind_log(level, ArrayPrototypeJoin(ArrayPrototypeMap(args, show), " "), line, column);
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

  // What belongs to the run, once per instance, before any module is loaded:
  // { runId, startedAt, mathRandom: [a, b, c, d], http, maxElements }.
  function init(configJson) {
    const config = JSONParse(configJson);
    startedAt = config.startedAt;
    maxElements = config.maxElements;
    [a, b, c, d] = ArrayPrototypeMap(config.mathRandom, (word) => word >>> 0);
    for (let i = 0; i < 12; i++) sfc32();

    defineGlobal(
      "performance",
      ObjectFreeze({ now: () => now() - startedAt, timeOrigin: startedAt }),
      false,
    );
    if (config.http) defineGlobal("fetch", fetch);

    const run = ObjectFreeze({
      id: config.runId,
      get startedAt() {
        return new NativeDate(startedAt);
      },
    });

    globalThis.__rewindHandoff = ObjectFreeze({
      callAction,
      callActionContent,
      sleep,
      signal,
      signalUrl,
      // Code written before signal URLs were named calls `webhook(name)`, the same function
      // (archi/08-decisions.md, R1).
      webhook: signalUrl,
      run,
      ActionError,
      ToolError: ActionError,
      CancelledError,
      RunError,
      // Code written before runs' errors were named catches a `WorkflowError`, the same class
      // (archi/08-decisions.md, R1).
      WorkflowError: RunError,
      workflow,
      setMain(fn) {
        if (typeof fn !== "function") {
          throw new TypeError("the code must `export default` its main function");
        }
        main = fn;
      },
    });

    logicalTemporalNow();

    // Capabilities go now, before any module is loaded: what exposes GC timing or threads,
    // WebAssembly (code the sandbox cannot see the source of), and the runtime's own handles.
    // Not in the snapshot: V8 completes these globals when an isolate starts from it, and
    // deno_core reads its callbacks from `Deno.core` then.
    for (
      const name of [
        "WeakRef",
        "FinalizationRegistry",
        "SharedArrayBuffer",
        "Atomics",
        "WebAssembly",
        "Deno",
        "__bootstrap",
        "__rewindHooks",
      ]
    ) {
      delete globalThis[name];
    }
  }

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
      case "call_completed": {
        const entry = take(completion.seq);
        if (entry === undefined) break;
        callsInFlight--;
        if (failure === undefined) {
          entry.resolve(entry.content ? actionContent(completion.result) : completion.result);
        }
        break;
      }
      case "call_failed": {
        const entry = take(completion.seq);
        if (entry === undefined) break;
        callsInFlight--;
        if (failure === undefined) entry.reject(new ActionError(entry.action, completion));
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
          callsInFlight = 0;
        }
        break;
      default:
        throw new TypeError(`unknown completion: ${completion.type}`);
    }
  }

  // Called once microtasks are drained: decides whether the code has ended. Once it has,
  // the terminal command is emitted from what was captured: no code code runs any more.
  function idle() {
    if (ended) return;
    // Rejections still without a handler after the microtask checkpoint are unhandled.
    const rejections = op_drain_pending_rejections();
    if (rejections !== undefined && rejections.length > 0) fail(rejections[1]);

    if (failure !== undefined) {
      if (callsInFlight === 0) end();
    } else if (mainState === "returned") {
      // The runs it started attached end first, unless the run is being cancelled: its end
      // cancels them.
      if (callsInFlight === 0 && (attached.size === 0 || cancellation !== undefined)) end();
    } else if (callsInFlight === 0 && !hasPendingTimers() && !hasSignalWaiters()) {
      // Nothing can wake the code any more. After a cancellation, that is how it ends.
      fail(cancellation === undefined ? new DeadlockError() : new CancelledError(cancellation));
      end();
    }
  }

  function end() {
    ended = true;
    pending.clear();
    if (failure !== undefined) {
      op_rewind_fail(failure.name, failure.message, failure.stack);
    } else {
      op_rewind_complete(output);
    }
  }

  globalThis.__rewindHooks = { init, start, deliver, idle };
})(globalThis);
