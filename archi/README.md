# Rewind architecture

Rewind is a programmatic tool calling engine for **agent harness developers**. The
model inside a harness writes a TypeScript script that calls tools (MCP servers and
harness tools). The script runs in a deterministic V8 sandbox. Every interaction with
the outside world goes through a per-run **journal**, and durable pauses, replay,
mocks and observability are all derived from it.

**One rule above all: the journal is the only source of truth.** No feature gets its
own side channel: traces, metrics about runs, and the dashboard are projections of it.

> Status, November 2026: the target is version 0.1, which this whole specification describes.
> The runtime spike is done, and so is the crate layering of
> [07-code-architecture.md](07-code-architecture.md), with the decisions of the November design
> review (run TTL, determinism epoch, how a script ends, format v1): see
> [08-decisions.md](08-decisions.md). The contracts and designs of 0.1 were approved in
> November 2026. [09-roadmap.md](09-roadmap.md) holds the plan for 0.1 and the exact state of
> the code.

## Reading order

| File | Covers |
| --- | --- |
| [01-product.md](01-product.md) | What Rewind is, who it is for, how a harness integrates it, what the model writes, the sandbox |
| [02-journal.md](02-journal.md) | The journal: event families, `seq`, callsites, the commit invariant, run state, the stored format v1 |
| [03-execution.md](03-execution.md) | Activations, logical time, replay, sticky execution, forks, determinism epoch and quarantine, how a script ends, signals, cancellation, run TTL, limits |
| [04-tools.md](04-tools.md) | Tool routes, idempotency, the tool task lifecycle, policies, toolsets, code generation, observed result types, credentials, egress of the `http` tool, type-checking |
| [05-testing-and-observability.md](05-testing-and-observability.md) | Mocks, cassettes, scenarios, replay and forks, evals; OpenTelemetry, read model and dashboard as projections of the journal |
| [06-storage-and-scale.md](06-storage-and-scale.md) | Tables, leases and fencing, idempotency of the API, payload tiers, scaling path, tenancy and authentication, retention, security and sandboxed workers |
| [07-code-architecture.md](07-code-architecture.md) | Crate layering, ports, boundary representations, dependency rules, the HTTP API `/v1`, the embedded server and the CLI |
| [08-decisions.md](08-decisions.md) | Every decision taken so far, who took it, and why |
| [09-roadmap.md](09-roadmap.md) | Plan for 0.1: current state of the code, contracts, workstreams, designs settled, what is out of 0.1, acceptance criteria |

## Positioning

*Code mode / programmatic tool calling* for how the model works, *Temporal / Restate*
for how the code executes, on a runtime we fully control. Because we own the runtime,
determinism is guaranteed by construction instead of by developer discipline, and
because scripts are immutable and content-addressed, there is no workflow versioning
problem. What still changes is the runtime itself, and a determinism epoch versions it.

## Process

Before code depends on a contract or a design, its architecture principles are presented
for review, with diagrams. Implementation starts after approval.
