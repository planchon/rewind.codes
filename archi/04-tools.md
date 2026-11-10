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

Each tool has a **policy**:

| Policy | Default for | On transient error | Dispatcher crashed mid-call (outcome unknown) |
| --- | --- | --- | --- |
| `retry` | MCP tools with `readOnlyHint` or `idempotentHint` | Retry with backoff | Retry with the same idempotency key |
| `at_most_once` | Everything else | Fail | Fail with `outcomeUnknown: true` |

The script sees failures as a thrown `ToolError` and decides what to do. Policies can be
overridden per tool in the toolset.

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

In 0.1 the toolset model in `ptc-core` is only a sorted, deduplicated list of tool names
with a digest. Schemas and policies arrive with M2.

## Type-checking

`ptc check` (and `POST /check`) type-checks the script against `ptc.d.ts` and the
toolset's declarations, in strict mode, before anything runs. Diagnostics are formatted
for a model: location, message, and a short code frame.

V1 runs `tsc` inside a dedicated V8 isolate started from a snapshot, as Deno
historically did. This keeps ai-ptc a single self-contained binary. The server keeps a
pool of warm checkers. Swapping in the native compiler (tsgo) later is an implementation
detail behind the `TypeChecker` port.

`runs.start` checks by default; the harness can skip it.

Transpilation is `deno_ast` (swc: type stripping and source maps).
