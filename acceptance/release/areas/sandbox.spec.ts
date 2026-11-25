/**
 * Area "Sandbox" of the release gate, on setup A: what code written by a model can reach.
 *
 * - Determinism: logical time, the seeded `Math.random` and `crypto`; a replay of the run
 *   export recomputes every value; two runs differ (`archi/01-product.md`, "What exists in the
 *   sandbox"; `archi/03-execution.md`, "Logical time", "Replay").
 * - Escape: the globals are exactly the sandbox lib's; no module but `rewind`, `rewind:actions` and
 *   `rewind:workflows`, statically, dynamically or through `eval`; no action outside the action set;
 *   no `fetch` without the `http` action; nothing a run leaves in its isolate reaches another run;
 *   stack traces name no path of the server; a secret the run uses never enters the isolate.
 * - Exhaustion (`archi/03-execution.md`, "Resource limits"): CPU per call, heap, array buffers
 *   (a catchable `RangeError`), commands per activation, commands and logs per trigger, payloads;
 *   each attack is followed by a canary run, and at the end every node still activates runs.
 */
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { root } from "../../support/paths.js";
import { eventually } from "../../support/processes.js";
import { type McpFixture, type Sink, startMcpFixture, startSink } from "../support/fixtures.js";
import {
  canary,
  evidence,
  expect,
  expectHealthy,
  expectOk,
  expectRefused,
  newOrg,
  stacks,
  type Org,
  test,
  unique,
} from "../support/gate.js";
import {
  collectorSpans,
  ended,
  eventTypes,
  expectLimit,
  runExport,
  replayDocument,
  runCode,
} from "../support/sandbox-helpers.js";
import { handedWithin } from "../support/actions-helpers.js";

let org: Org;
/** An action set of harness actions nobody serves: `t.noop`. */
let harnessActionSet: string;

test.beforeAll(async () => {
  org = await newOrg("sandbox");
  const created = await org.api.post("/v1/action-sets", {
    harness: [{ name: "t.noop", description: "Does nothing.", inputSchema: { type: "object" } }],
  });
  expectOk(created);
  harnessActionSet = created.json.id;
});

/** The runs of a namespace, as the read model lists them. */
async function runsIn(namespace: string): Promise<string[]> {
  const reply = await org.api.ns(namespace).get("/v1/runs", { query: { limit: 100 } });
  expectOk(reply);
  return (reply.json.runs as { id: string }[]).map((run) => run.id).sort();
}

// ---------------------------------------------------------------------------------------------
// Determinism

const DETERMINISM = `import { sleep } from "rewind";
export default async function main() {
  const a = {
    now: Date.now(),
    iso: new Date().toISOString(),
    perf: performance.now(),
    random: Math.random(),
    uuid: crypto.randomUUID(),
    bytes: Array.from(crypto.getRandomValues(new Uint8Array(16))),
  };
  await sleep("1s");
  const b = { now: Date.now(), random: Math.random(), uuid: crypto.randomUUID() };
  const options = Intl.DateTimeFormat().resolvedOptions();
  return { a, b, timeZone: options.timeZone, locale: options.locale };
}
`;

test("time is logical and randomness is seeded: a replay recomputes every value, two runs differ", { tag: "@success" }, async () => {
  const first = await runCode(org.api, DETERMINISM);
  const second = await runCode(org.api, DETERMINISM);
  for (const run of [first, second]) expect(run.status, JSON.stringify(run)).toBe("completed");
  const one = first.output.value;
  const two = second.output.value;

  // Logical time: the time of the trigger the code reacts to, in UTC and en-US.
  const { doc, text } = await runExport(org.api, first.id);
  expect(eventTypes(doc)).toEqual(["run_started", "start_timer", "timer_fired", "complete_run"]);
  expect(one.a.now).toBe(doc.events[0].recorded_at);
  expect(one.a.iso).toBe(new Date(one.a.now).toISOString());
  expect(one.b.now).toBe(doc.events[2].recorded_at);
  expect(one.a.perf).toBe(0);
  expect(one.timeZone).toBe("UTC");
  expect(one.locale).toBe("en-US");

  // Each run has its own seed: its generators, crypto included, differ from another run's.
  expect(doc.events[0].event.seed).toMatch(/^[0-9a-f]{64}$/);
  expect(two.a.uuid).not.toBe(one.a.uuid);
  expect(two.a.bytes).not.toEqual(one.a.bytes);
  expect(two.a.random).not.toBe(one.a.random);
  expect(one.b.uuid).not.toBe(one.a.uuid);

  // The history replays in another process, without the server, to the same output.
  const replayed = replayDocument(text);
  expect(replayed.code, replayed.stdout + replayed.stderr).toBe(0);
  expect(replayed.stdout).toContain("the code replays the 4 events");
  evidence("a CLI replay of the run export recomputed the same time, random values and UUIDs");
});

// ---------------------------------------------------------------------------------------------
// Escape

test("code finds no global of Node, Deno or a browser, and eval sees the same sandbox", { tag: "@attack" }, async () => {
  const lib = JSON.parse(readFileSync(join(root, "crates", "rewind-runtime", "lib", "globals.json"), "utf8"));
  const declared = [...lib.ecmascript, ...lib.sandbox].sort();
  const run = await runCode(
    org.api,
    `export default async function main() {
  const names = ["process", "Deno", "Bun", "require", "module", "exports", "__dirname", "Buffer", "global", "window", "self",
    "navigator", "fetch", "XMLHttpRequest", "WebSocket", "Worker", "importScripts", "setImmediate", "WebAssembly",
    "SharedArrayBuffer", "Atomics", "WeakRef", "FinalizationRegistry"];
  const present = names.filter((name) => typeof (globalThis as any)[name] !== "undefined");
  return {
    present,
    globals: Object.getOwnPropertyNames(globalThis).sort(),
    symbols: Object.getOwnPropertySymbols(globalThis).length,
    viaEval: (0, eval)("typeof process + typeof Deno + typeof require + typeof fetch"),
    viaFunction: new Function("return [typeof process, typeof Deno, Object.getOwnPropertyNames(globalThis).length]")(),
  };
}
`,
  );
  expect(run.status, JSON.stringify(run.error)).toBe("completed");
  const out = run.output.value;
  expect(out.present).toEqual([]);
  expect(out.globals).toEqual(declared);
  expect(out.symbols).toBe(0);
  // eval and new Function exist (they are ECMAScript, in globals.json) and reach nothing more.
  expect(out.viaEval).toBe("undefinedundefinedundefinedundefined");
  expect(out.viaFunction).toEqual(["undefined", "undefined", declared.length]);
  evidence(`the isolate's globals are exactly the ${declared.length} of the sandbox lib; eval and new Function see the same`);
});

const SPECIFIERS = [
  "node:fs",
  "fs",
  "file:///etc/passwd",
  "https://esm.sh/lodash",
  "data:text/javascript,export default 1",
  "./other.ts",
  "rewind:secrets",
];

test("a module outside rewind, rewind:actions and rewind:workflows is refused before any run exists", { tag: "@attack" }, async () => {
  const namespace = unique("imports-");
  for (const specifier of SPECIFIERS) {
    const reply = await expectRefused({
      attempt: () =>
        org.api.ns(namespace).post("/v1/runs", {
          code: `import x from ${JSON.stringify(specifier)};\nexport default async function main() { return String(x); }\n`,
          check: false,
        }),
      status: 422,
      slug: "invalid-code",
      probe: () => runsIn(namespace),
    });
    expect(reply.json.diagnostics?.[0]?.code).toBe("rewind/invalid-code");
    expect(reply.json.detail).toContain(`cannot import ${JSON.stringify(specifier)}`);
  }
  evidence(`${SPECIFIERS.length} imports refused with 422 invalid-code; no run was created`);
});

test("a dynamic import() is refused before any run exists", { tag: "@attack" }, async () => {
  const namespace = unique("dynamic-");
  for (const specifier of ["node:fs", "data:text/javascript,export default 1", "rewind"]) {
    const reply = await expectRefused({
      attempt: () =>
        org.api.ns(namespace).post("/v1/runs", {
          code: `export default async function main() { const m = await import(${JSON.stringify(specifier)}); return Object.keys(m); }\n`,
          check: false,
        }),
      status: 422,
      slug: "invalid-code",
      probe: () => runsIn(namespace),
    });
    expect(reply.json.detail).toContain("dynamic import() is not available");
  }
  evidence("import() refused at start with 422 invalid-code");
});

test("an import() hidden in eval or new Function loads nothing", { tag: "@attack" }, async () => {
  const code = `export default async function main(input: { specifier: string; how: string }) {
  const p: Promise<any> = input.how === "eval"
    ? (0, eval)("import(" + JSON.stringify(input.specifier) + ")")
    : new Function("s", "return import(s)")(input.specifier);
  return await Promise.race([
    p.then((m) => "LOADED " + Object.keys(m).join(","), (e) => "REJECTED " + String(e)),
    new Promise((resolve) => setTimeout(() => resolve("PENDING"), 100)),
  ]);
}
`;
  const outcomes: string[] = [];
  for (const specifier of ["node:fs", "file:///etc/passwd", "data:text/javascript,export default 42", "rewind", "rewind:actions"]) {
    for (const how of ["eval", "Function"]) {
      const run = await runCode(org.api, code, { input: { specifier, how } });
      expect(run.status, JSON.stringify(run.error)).toBe("completed");
      // Spec silent on a hidden import: it never settles, or it rejects; it never loads.
      expect(String(run.output.value)).toMatch(/^(PENDING|REJECTED )/);
      outcomes.push(`${how} ${specifier}: ${String(run.output.value).slice(0, 40)}`);
    }
  }
  evidence(`hidden import() never loads a module (${[...new Set(outcomes.map((o) => o.split(": ")[1]))].join(", ")})`);
});

test("an action outside the action set: an absent group is refused at start, an absent action calls nothing", { tag: "@attack" }, async () => {
  const namespace = unique("actions-");
  const refused = await expectRefused({
    attempt: () =>
      org.api.ns(namespace).post("/v1/runs", {
        actionSet: harnessActionSet,
        code: `import { payments } from "rewind:actions";\nexport default async function main() { return await payments.refund({ amount: 100 }); }\n`,
        check: false,
      }),
    status: 422,
    slug: "invalid-code",
    probe: () => runsIn(namespace),
  });
  expect(refused.json.detail).toContain('has no export named "payments"');

  const run = await runCode(
    org.api,
    `import { t } from "rewind:actions";\nexport default async function main() { return await (t as any).refund({ amount: 100 }); }\n`,
    { actionSet: harnessActionSet, namespace },
  );
  expect(run.status).toBe("failed");
  expect(run.error).toMatchObject({ name: "TypeError" });
  // No call was scheduled: the history holds the start and the failure only.
  expect(eventTypes((await runExport(org.api, run.id)).doc)).toEqual(["run_started", "fail_run"]);
  evidence("absent group: 422 invalid-code; absent action: TypeError, no schedule_call");
});

test("fetch does not exist without the http action, and nothing reaches the network", { tag: "@attack" }, async () => {
  const sink: Sink = await startSink();
  try {
    const run = await runCode(
      org.api,
      `declare const fetch: (url: string) => Promise<unknown>;\nexport default async function main(input: { url: string }) { return await fetch(input.url); }\n`,
      { input: { url: `${sink.origin}/steal` } },
    );
    expect(run.status).toBe("failed");
    expect(run.error).toMatchObject({ name: "ReferenceError", message: "fetch is not defined" });
    expect(sink.hits).toEqual([]);
    evidence("ReferenceError: fetch is not defined; the internal server received nothing");
  } finally {
    await sink.close();
  }
});

test("what a run leaves in its isolate never reaches another run", { tag: "@attack" }, async () => {
  const namespace = unique("pollution-");
  const polluter = await runCode(
    org.api,
    `import { signal } from "rewind";
export default async function main() {
  (Object.prototype as any).polluted = "yes";
  (Array.prototype as any).includes = () => true;
  (globalThis as any).leak = "from the polluter";
  Math.random = () => 0.42;
  (Promise.prototype as any).leaked = 1;
  (String.prototype as any).toUpperCase = () => "HIJACKED";
  const go = await signal<string>("go");
  return { go, mine: ({} as any).polluted, includes: [1].includes(2) };
}
`,
    // It waits for its signal: the start does not wait for its end.
    { namespace, extra: { wait: "2s" } },
  );
  expect(polluter.waitingFor?.[0]).toMatchObject({ kind: "signal", name: "go" });

  const VICTIM = `export default async function main() {
  return {
    polluted: ({} as any).polluted ?? null,
    leak: (globalThis as any).leak ?? null,
    includes: [1].includes(2),
    random: Math.random() === 0.42,
    leaked: (Promise.prototype as any).leaked ?? null,
    upper: "abc".toUpperCase(),
  };
}
`;
  // Several runs, so that both workers, which hold the polluter's instance in their cache, serve some.
  const victims = await Promise.all(Array.from({ length: 6 }, () => runCode(org.api, VICTIM, { namespace })));
  for (const victim of victims) {
    expect(victim.status, JSON.stringify(victim.error)).toBe("completed");
    expect(victim.output.value).toEqual({ polluted: null, leak: null, includes: false, random: false, leaked: null, upper: "ABC" });
  }
  const signalled = await org.api.ns(namespace).post(`/v1/runs/${polluter.id}/signals/go`, "now");
  expectOk(signalled);
  const done = await ended(org.api, polluter.id);
  // The pollution was real, in the polluter's own isolate.
  expect(done.output?.value ?? done.error, JSON.stringify(done)).toMatchObject({ go: "now", mine: "yes", includes: true });
  evidence("prototypes, globals and Math.random changed by one run: 6 runs started meanwhile saw none of it");
});

test("stack traces name the code and the runtime, never a path of the server", { tag: "@attack" }, async () => {
  const run = await runCode(
    org.api,
    `import { t } from "rewind:actions";
export default async function main() {
  (Error as any).prepareStackTrace = (_e: unknown, frames: unknown[]) => frames;
  const stacks: unknown[] = [];
  try { await (t.noop as any)(null); } catch (e: any) { stacks.push(typeof e.stack, String(e.stack)); }
  try { (null as any).x; } catch (e: any) { stacks.push(typeof e.stack, String(e.stack)); }
  try { (0, eval)("null.y"); } catch (e: any) { stacks.push(typeof e.stack, String(e.stack)); }
  function deep(n: number): never { if (n === 0) throw new Error("deep"); return deep(n - 1); }
  try { deep(20); } catch (e: any) { stacks.push(typeof e.stack, String(e.stack)); }
  throw new Error("the end");
}
`,
    { actionSet: harnessActionSet },
  );
  expect(run.status).toBe("failed");
  const text = JSON.stringify(run);
  for (const path of ["/Users/", "/home/", "/root/", "/private/", "/var/", "target/", ".rs:", "cargo", "deno_core", "node_modules"]) {
    expect(text, `the run names ${path}`).not.toContain(path);
  }
  expect(run.error.stack).toContain("workflow.ts");
  // Glue frames reach the code as text only: no CallSite object to reach the runtime through.
  const { doc } = await runExport(org.api, run.id);
  const failed = doc.events.at(-1).event;
  expect(JSON.stringify(failed)).not.toContain("/Users/");
  evidence("stacks name workflow.ts and rewind:bootstrap frames; prepareStackTrace never hands the code a CallSite");
});

test("a secret the run uses never enters the isolate, a run export, a transcript or an answer", { tag: "@attack" }, async () => {
  const mcp: McpFixture = await startMcpFixture();
  const secret = canary();
  try {
    expectOk(await org.api.put("/v1/secrets/sandbox-fx", { value: secret }));
    const created = await org.api.post("/v1/action-sets", {
      mcp: [{ name: "fx", url: mcp.url, auth: { type: "secret", header: "authorization", format: "Bearer {secret}" } }],
    });
    expectOk(created);
    const run = await runCode(
      org.api,
      `import * as actions from "rewind:actions";
import * as rewind from "rewind";
import { fx } from "rewind:actions";
export default async function main() {
  const result: any = await fx.echo({ text: "hello" });
  const dump: string[] = [];
  const seen = new Set<unknown>();
  const walk = (value: any, depth: number) => {
    if (depth > 5 || value === null || (typeof value !== "object" && typeof value !== "function") || seen.has(value)) return;
    seen.add(value);
    for (const key of Reflect.ownKeys(value)) {
      let item: any;
      try { item = value[key]; } catch (e) { dump.push(String(e)); continue; }
      dump.push(String(key));
      if (typeof item === "string") dump.push(item);
      else if (typeof item === "function") dump.push(Function.prototype.toString.call(item).slice(0, 400));
      walk(item, depth + 1);
    }
  };
  for (const root of [globalThis, actions, rewind, result, Object.getPrototypeOf(result)]) walk(root, 0);
  try { await (fx as any).echo(); } catch (e: any) { dump.push(String(e), String(e.stack), JSON.stringify(e)); }
  try { await (fx as any).echo({ text: 1 }); } catch (e: any) { dump.push(String(e), String(e.stack)); }
  const text = dump.join("\\n");
  console.log(text.slice(0, 200_000));
  return { reached: dump.length, text: text.slice(0, 1_000_000), echoed: JSON.stringify(result) };
}
`,
      { actionSet: created.json.id, extra: { secrets: { fx: "org/sandbox-fx" } } },
    );
    expect(run.status, JSON.stringify(run.error)).toBe("completed");
    expect(run.output.value.reached).toBeGreaterThan(100);
    expect(run.secrets).toEqual({ fx: "org/sandbox-fx" });
    // The call carried the secret the run uses to the MCP server, in its transport.
    expect(mcp.calls.length).toBeGreaterThan(0);
    expect(mcp.calls[0]!.headers.authorization).toBe(`Bearer ${secret}`);
    // Nothing the code reached, logged or returned holds it; nor its history or transcript.
    expect(JSON.stringify(run)).not.toContain(secret);
    const exported = await runExport(org.api, run.id);
    expect(exported.text).not.toContain(secret);
    const transcript = await org.api.get(`/v1/runs/${run.id}/transcript`);
    expectOk(transcript);
    expect(transcript.text).not.toContain(secret);
    const cassette = await org.api.get(`/v1/runs/${run.id}/export`, { query: { as: "cassette" } });
    expectOk(cassette);
    expect(cassette.text).not.toContain(secret);
    const actionSets = await org.api.get("/v1/action-sets");
    expect(actionSets.text).not.toContain(secret);
    const events = await org.api.get(`/v1/runs/${run.id}/events`);
    expectOk(events);
    expect(events.text).not.toContain(secret);
    evidence(`the code walked ${run.output.value.reached} names it can reach; the canary secret, used as org/sandbox-fx, reached the MCP server only`);
  } finally {
    await mcp.close();
  }
});

// ---------------------------------------------------------------------------------------------
// Exhaustion: each attack ends as archi/03 says, then the canary run still completes.

test("an infinite loop is stopped after 5 s of CPU time", { tag: "@attack" }, async () => {
  const run = await runCode(org.api, "export default async function main() { for (;;) {} }\n");
  expectLimit(run, "more than 5000ms of CPU time reacting to one trigger");
  expect(eventTypes((await runExport(org.api, run.id)).doc)).toEqual(["run_started", "run_aborted"]);
  await expectHealthy(org);
  evidence("run_aborted limit_exceeded: more than 5000ms of CPU time");
});

test("catastrophic regex backtracking is stopped by the CPU limit", { tag: "@attack" }, async () => {
  const run = await runCode(org.api, 'export default async function main() { return /^(a+)+$/.test("a".repeat(48) + "!"); }\n');
  expectLimit(run, "more than 5000ms of CPU time");
  await expectHealthy(org);
  evidence("run_aborted limit_exceeded: the regex was interrupted at 5 s of CPU");
});

test("a heap bomb is stopped at 128 MiB", { tag: "@attack" }, async () => {
  const run = await runCode(
    org.api,
    "export default async function main() { const hoard: number[][] = []; for (;;) hoard.push(new Array(100_000).fill(1)); }\n",
  );
  expectLimit(run, "more than 128 MiB of heap");
  await expectHealthy(org);
  evidence("run_aborted limit_exceeded: more than 128 MiB of heap");
});

test("a built-in asked to fill a billion elements is stopped before V8 runs it", { tag: "@attack" }, async () => {
  const run = await runCode(org.api, "export default async function main() { return new Array(1e9).fill(0).length; }\n");
  expectLimit(run, "more than 128 MiB of heap");
  await expectHealthy(org);
  evidence("new Array(1e9).fill(0): run_aborted limit_exceeded (heap), the worker kept serving");
});

test("strings past V8's length or the heap end the run, catchably for the first", { tag: "@attack" }, async () => {
  const repeat = await runCode(
    org.api,
    'export default async function main() { try { return "x".repeat(2 ** 29).length; } catch (e: any) { return e.name + ": " + e.message; } }\n',
  );
  expect(repeat.status).toBe("completed");
  expect(repeat.output.value).toBe("RangeError: Invalid string length");
  const doubling = await runCode(
    org.api,
    'export default async function main() { let s = "xxxxxxxx"; const keep: string[] = []; for (;;) { s = s + s; keep.push(s.slice(1)); } }\n',
  );
  expectLimit(doubling, "more than 128 MiB of heap");
  await expectHealthy(org);
  evidence('"x".repeat(2**29): RangeError caught by the code; doubling kept strings: heap limit');
});

test("array buffers past their 128 MiB budget fail with a RangeError the code catches", { tag: "@attack" }, async () => {
  const run = await runCode(
    org.api,
    `export default async function main() {
  const keep: ArrayBuffer[] = [];
  let caught = "";
  try { for (;;) keep.push(new ArrayBuffer(16 * 1024 * 1024)); } catch (e: any) { caught = e.name + ": " + e.message; }
  const held = keep.length;
  keep.length = 0;
  let resizable = "";
  try { new ArrayBuffer(1, { maxByteLength: 512 * 1024 * 1024 }); resizable = "created"; } catch (e: any) { resizable = e.name + ": " + e.message; }
  return { caught, held, resizable, after: new Uint8Array(1024 * 1024).length };
}
`,
  );
  expect(run.status, JSON.stringify(run.error ?? run.abort)).toBe("completed");
  expect(run.output.value).toEqual({
    caught: "RangeError: Array buffer allocation failed",
    held: 7,
    resizable: "RangeError: Array buffer allocation failed",
    after: 1024 * 1024,
  });
  await expectHealthy(org);
  evidence("7 × 16 MiB held, the 8th refused with a catchable RangeError; a resizable buffer's maximum counts");
});

test("a million timers in one trigger end the run", { tag: "@attack" }, async () => {
  const run = await runCode(
    org.api,
    'import { sleep } from "rewind";\nexport default async function main() { await Promise.all(Array.from({ length: 1_000_000 }, () => sleep(1))); }\n',
  );
  expectLimit(run, "reacting to one trigger");
  expect(run.historyLen).toBe(2);
  await expectHealthy(org);
  evidence(String(run.abort.message));
});

test("a million calls in one trigger end the run, and no call is handed to the harness", { tag: "@attack" }, async () => {
  const run = await runCode(
    org.api,
    'import { t } from "rewind:actions";\nexport default async function main() { await Promise.all(Array.from({ length: 1_000_000 }, () => t.noop({}))); }\n',
    { actionSet: harnessActionSet },
  );
  expectLimit(run, "reacting to one trigger");
  expect(eventTypes((await runExport(org.api, run.id)).doc)).toEqual(["run_started", "run_aborted"]);
  const worker = await org.key("worker");
  expect(await handedWithin(org.api.as(worker.key), { actionSet: harnessActionSet, slots: 100 })).toEqual([]);
  await expectHealthy(org);
  evidence(`${String(run.abort.message)}; no call handed on a stream`);
});

test("more than 10,000 commands in one activation end the run", { tag: "@attack" }, async () => {
  const run = await runCode(
    org.api,
    "export default async function main() { for (let i = 0; i < 20_000; i++) setInterval(() => {}, 1); return 1; }\n",
  );
  expectLimit(run, "more than 10000 commands in one activation");
  expect(run.waitingFor ?? []).toEqual([]);
  await expectHealthy(org);
  evidence("20,000 intervals: run_aborted limit_exceeded, no timer kept");
});

test("infinite recursion is a RangeError the code can catch", { tag: "@attack" }, async () => {
  const caught = await runCode(
    org.api,
    "export default async function main() { function f(n: number): number { return f(n + 1) + 1; } try { return f(0); } catch (e: any) { return e.name + ': ' + e.message; } }\n",
  );
  expect(caught.status).toBe("completed");
  expect(caught.output.value).toBe("RangeError: Maximum call stack size exceeded");
  const uncaught = await runCode(
    org.api,
    "export default async function main() { function f(n: number): number { return f(n + 1) + 1; } return f(0); }\n",
  );
  expect(uncaught.status).toBe("failed");
  expect(uncaught.error).toMatchObject({ name: "RangeError", message: "Maximum call stack size exceeded" });
  await expectHealthy(org);
  evidence("stack overflow: a catchable RangeError; uncaught, the run fails with it");
});

test("an output above 8 MiB ends the run", { tag: "@attack" }, async () => {
  const run = await runCode(org.api, 'export default async function main() { return "x".repeat(9 * 1024 * 1024); }\n');
  expectLimit(run, "the payload of complete_run is");
  expectLimit(run, "above the limit of 8388608 bytes");
  await expectHealthy(org);
  evidence(String(run.abort.message));
});

test("logs are bounded: 64 MiB of logs in one trigger end the run", { tag: "@attack" }, async () => {
  const within = await runCode(
    org.api,
    'export default async function main() { const s = "y".repeat(1024 * 1024); for (let i = 0; i < 8; i++) console.log(s); return "logged"; }\n',
  );
  expect(within.status).toBe("completed");
  expect(within.historyLen).toBe(10);
  const flood = await runCode(
    org.api,
    'export default async function main() { const s = "z".repeat(1024 * 1024); for (let i = 0; i < 120; i++) console.log(s); return "logged"; }\n',
  );
  expectLimit(flood, "more than 64 MiB of commands and logs reacting to one trigger");
  expect(flood.historyLen).toBe(2);
  const many = await runCode(
    org.api,
    'export default async function main() { for (let i = 0; i < 200_000; i++) console.log("line " + i); return "logged"; }\n',
  );
  expectLimit(many, "commands and logs reacting to one trigger");
  await expectHealthy(org);
  evidence("8 MiB of logs kept; 120 MiB and 200,000 lines in one trigger: run_aborted limit_exceeded");
});

test("after every attack, every node still activates runs", { tag: "@success" }, async () => {
  const { a } = stacks();
  const traces = a.collector?.traces;
  expect(traces, "setup A has a Collector").toBeDefined();
  // Runs of three activations, each with a little CPU, each in a namespace of its own: the
  // namespaces spread over the partitions, so over the nodes that own them. Batches until each
  // node's activation spans reach the Collector.
  const CODE = `import { sleep } from "rewind";
function spin(n: number): number { let x = 0; for (let i = 0; i < n; i++) x = (x + i) % 1000003; return x; }
export default async function main() { spin(20_000_000); await sleep(1); spin(20_000_000); await sleep(1); return "done"; }
`;
  const ids = new Set<string>();
  const seen = () =>
    new Set(
      collectorSpans(traces!)
        .filter((span) => span.name === "activation" && ids.has(String(span.attributes["rewind.run_id"])))
        .map((span) => String(span.resource["service.instance.id"])),
    );
  for (let batch = 0; batch < 6 && seen().size < a.nodes.length; batch++) {
    const runs = await Promise.all(
      Array.from({ length: 8 }, async () => {
        const started = await org.api.ns(unique("nodes-")).post("/v1/runs", { code: CODE, check: false });
        expectOk(started);
        ids.add(started.json.id);
        return started.json.id as string;
      }),
    );
    for (const id of runs) expect((await ended(org.api, id)).status).toBe("completed");
    await eventually("the batch's activation spans reach the Collector", () => (seen().size > 0 ? true : undefined), 30_000, 500).catch(() => undefined);
  }
  const nodes = await eventually(
    "every node's activation spans reach the Collector",
    () => (seen().size === a.nodes.length ? [...seen()].sort() : undefined),
    30_000,
    500,
  );
  expect(nodes).toEqual([...a.nodes].sort());
  evidence(`activations of ${ids.size} runs started after the attacks, by ${nodes.join(" and ")}`);
});
