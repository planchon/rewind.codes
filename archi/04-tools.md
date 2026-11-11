# 4. Tools

## Side effects and dispatch

Each `schedule_tool_call` creates a **tool task**, executed outside the isolate by the
dispatcher:

```
scheduled → dispatched (attempt n, lease until T) → succeeded | failed → completion pushed to the run's inbox
```

Attempts, backoff and leases live in the tool task, not in the journal. The journal only
gets the final outcome.

Every call carries an **idempotency key** `"{run_id}/{seq}"`, passed in MCP `_meta`
(`io.ai-ptc/idempotency-key`) and to host tool handlers. Calls also carry the W3C trace
context (`traceparent`, `tracestate`) in `_meta`, with the tool call span as parent, so
the MCP server's own spans join the run's trace.

This key protects the way out. The way in (starting a run, sending a signal) has its
own keys: see [storage](06-storage-and-scale.md#idempotency-of-the-api).

Each tool has a **policy**:

| Policy | Default for | On transient error | Dispatcher crashed mid-call (outcome unknown) |
| --- | --- | --- | --- |
| `retry` | Every tool | Retry with backoff | Retry with the same idempotency key |
| `at_most_once` | Tools the toolset opts out | Fail | Fail with `outcomeUnknown: true` |

An interrupted call is called again with the same idempotency key, so the tool can recognize
it and not repeat its effect. A tool that ignores the key may then run twice: such a tool is
declared `at_most_once` in the toolset. A call whose dispatcher died 5 times in a row fails
with `outcomeUnknown: true` instead of being tried again.

The script sees failures as a thrown `ToolError` and decides what to do. Policies can be
overridden per tool in the toolset.

Until M2, toolsets carry no policies: every tool is `retry`, and only the crash case is
implemented (a task claimed again after its claim expired). Transient errors and their
backoff arrive with the MCP dispatcher.

Routes:

- **MCP tools**: the dispatcher is an MCP client (`rmcp`, stdio or streamable HTTP).
  Credentials are provided by the harness per toolset or per run and never enter the
  isolate.
- **Host tools**: tasks are long-polled by the harness's SDK worker.
- **Built-ins**: `http` (opt-in; the only way a script reaches the network), `webhook`.

Results arriving for a run that already ended are dropped.

## Toolsets and code generation

A toolset is a list of MCP servers and host tools. Registering it connects to the
servers, lists their tools, and **pins** the schemas. The toolset is content-addressed,
and every run records the digest it was started with.

From a toolset, `ptc-codegen` generates:

- the `ptc:tools` runtime module: thin wrappers over the tool op;
- `ptc-tools.d.ts`: types for args (from `inputSchema`) and results (from `outputSchema`
  when present, otherwise the MCP content type with `.text()` and `.json()` helpers),
  with tool descriptions as JSDoc;
- a **compact prompt rendering** of the same declarations, for the model's context
  window.

Tool names are kept as the server defines them (`github.list_issues`) so they match the
descriptions the model reads. Args are validated against the pinned schema inside the
op, before any command is recorded. Invalid args throw a `TypeError` at the call site.
This is deterministic, since the schema is pinned.

The JSON Schema → TypeScript converter is ours (objects, required, enums,
`anyOf`/`oneOf`/`allOf`, `$ref`/`$defs`, arrays, `additionalProperties`).

Until M2 the toolset model in `ptc-core` is only a sorted, deduplicated list of tool names
with a digest. Schemas and policies arrive with M2.

### When a toolset changes during a run

A run keeps the toolset it started with, but the server behind it can change during a long
pause: a tool renamed, removed, or with a new schema. A call that no longer matches is not
failed silently: the harness is warned, and the agent or the developer decides what happens
to the run (continue against the new schema, fork it with a corrected script, or cancel it).
The mechanism is designed with M2.

### Observed result types

Most MCP servers declare no `outputSchema`, so the model would write code against a
result it cannot see. But every result ever returned is in a journal. A projection
infers, per tool, the **shape observed** in recorded results, and codegen uses it when
the server declares nothing (M3).

- Shapes only: keys and types, never values. Observations stay within a tenant.
- A field missing from some results is optional. A tool never observed keeps the MCP
  content type and its helpers.
- The declaration says it is observed, not declared (`@observed` in its JSDoc), so the
  model knows it may be incomplete.
- Observed types serve type-checking and the prompt only. Results are never validated
  against them, and they are not part of the toolset digest, so replay does not depend
  on them.

## Type-checking

`ptc check` (and `POST /check`) type-checks the script against `ptc.d.ts` and the
toolset's declarations, in strict mode, before anything runs. Diagnostics are formatted
for a model: location, message, and a short code frame.

The checker's `lib` declares exactly the globals the sandbox has
([product](01-product.md#what-exists-in-the-sandbox)): what type-checks exists at run
time, and the reverse.

Beyond `tsc`, the checker runs rules of its own, with the same type information:

| Rule | Flags | Fix |
| --- | --- | --- |
| Floating promise | A statement whose expression is a `Promise`, or an array of them, that is neither awaited, returned nor assigned | `await`; `void expr` says it is intended |
| Ignored async callback | An `async` function passed where its result is ignored, such as `forEach` | `for … of` with `await`, or `Promise.all` over `map` |
| Absent global | `fetch`, `require`, `process`, `Buffer` | A hint saying what to use instead |

These rules are the first line. At run time, a forgotten `await` is still harmless
([how a script ends](03-execution.md#how-a-script-ends)).

V1 runs `tsc` inside a dedicated V8 isolate started from a snapshot, as Deno
historically did. This keeps ai-ptc a single self-contained binary. The server keeps a
pool of warm checkers. Swapping in the native compiler (tsgo) later is an implementation
detail behind the `TypeChecker` port.

`runs.start` checks by default; the harness can skip it.

Transpilation is `deno_ast` (swc: type stripping and source maps).
