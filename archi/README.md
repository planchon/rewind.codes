# ai-ptc architecture

ai-ptc is a programmatic tool calling engine for **agent harness developers**. The
model inside a harness writes a TypeScript script that calls tools (MCP servers and
harness tools). The script runs in a deterministic V8 sandbox. Every interaction with
the outside world goes through a per-run **journal**, and durable pauses, replay,
mocks and observability are all derived from it.

**One rule above all: the journal is the only source of truth.** No feature gets its
own side channel: traces, metrics about runs, and the dashboard are projections of it.

> Status, November 2026: version 0.1 in progress. M0 (runtime spike) is done. The crate
> layering in [07-code-architecture.md](07-code-architecture.md) is approved and being
> implemented. See [09-roadmap.md](09-roadmap.md) for the exact state of the code.

## Reading order

| File | Covers |
| --- | --- |
| [01-product.md](01-product.md) | What ai-ptc is, who it is for, how a harness integrates it, what the model writes, the sandbox |
| [02-journal.md](02-journal.md) | The journal: event families, `seq`, callsites, the commit invariant, run state |
| [03-execution.md](03-execution.md) | Activations, logical time, replay, signals, cancellation, limits |
| [04-tools.md](04-tools.md) | Tool dispatch, policies, idempotency, toolsets, code generation, type-checking |
| [05-testing-and-observability.md](05-testing-and-observability.md) | Mocks, cassettes, scenarios, replay and forks; OpenTelemetry, read model and dashboard as projections of the journal |
| [06-storage-and-scale.md](06-storage-and-scale.md) | Tables, leases and fencing, payloads in S3, scaling path, security |
| [07-code-architecture.md](07-code-architecture.md) | Crate layering, ports, boundary representations, dependency rules, implementation plan |
| [08-decisions.md](08-decisions.md) | Every decision taken so far, who took it, and why |
| [09-roadmap.md](09-roadmap.md) | Milestones, current state of the code, next steps, open questions |

## Positioning

*Code mode / programmatic tool calling* for how the model works, *Temporal / Restate*
for how the code executes, on a runtime we fully control. Because we own the runtime,
determinism is guaranteed by construction instead of by developer discipline, and
because scripts are immutable and content-addressed, there is no workflow versioning
problem.

## Process

Before a milestone is implemented, its architecture principles are presented for
review. Implementation starts after approval.
