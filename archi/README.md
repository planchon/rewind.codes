# Rewind architecture

Rewind is a programmatic tool calling engine for **agent harness developers**. The
model inside a harness writes a TypeScript **workflow** that calls actions (the tools of MCP
servers, the operations of APIs and harness actions). Its code runs in a deterministic V8
sandbox. Every interaction with the outside world goes through a per-run **history**, and
durable pauses, replay, tests and observability are all derived from it.

**One rule above all: the history is the only source of truth.** No feature gets its
own side channel: traces, metrics about runs, and the dashboard are projections of it.

> Status, November 2026: the target is version 0.1, which this whole specification describes.
> The runtime spike is done, and so is the crate layering of
> [07-code-architecture.md](07-code-architecture.md), with the decisions of the November design
> review (run TTL, determinism epoch, how a run ends, format v1): see
> [08-decisions.md](08-decisions.md). The contracts and designs of 0.1 were approved in
> November 2026. This specification uses the vocabulary decided in November 2026
> ([08](08-decisions.md#taken-for-the-vocabulary-november-2026)).
> [09-roadmap.md](09-roadmap.md) holds the plan for 0.1 and the exact state of the code.

## Reading order

| File | Covers |
| --- | --- |
| [01-product.md](01-product.md) | What Rewind is, who it is for, how a harness integrates it, what the model writes, workflows (versions, names, namespaces and the organization, forks), the sandbox |
| [02-history.md](02-history.md) | The history: event families, `seq`, callsites, the commit invariant, run state and statuses, the stored format v1 |
| [03-execution.md](03-execution.md) | Activations, logical time, replay, sticky execution, forks, determinism epoch and quarantine, how a run ends, signals, workflows across runs, cancellation, run TTL, limits |
| [04-tools.md](04-tools.md) | Action routes, idempotency, the call lifecycle, policies, action sets, code generation, observed result types, API actions from OpenAPI documents, authenticating calls (the secrets a run uses, calls signed with the organization's key), egress of the `http` action, MCP servers and APIs, workflow names, type-checking |
| [05-testing-and-observability.md](05-testing-and-observability.md) | Tests, test runs and handlers, cassettes, scenarios, suites and the gate, replay and forks, evals; OpenTelemetry, read model and dashboard as projections of the history |
| [06-storage-and-scale.md](06-storage-and-scale.md) | The control plane and the run plane, partitions, owners and nodes, idempotency of the API, payload tiers, tenancy and authentication, quotas, retention, security and sandboxed nodes |
| [07-code-architecture.md](07-code-architecture.md) | Crate layering, ports, boundary representations, dependency rules, the actors inside a node, the HTTP API `/v1`, the embedded server and the CLI |
| [08-decisions.md](08-decisions.md) | Every decision taken so far, who took it, and why |
| [09-roadmap.md](09-roadmap.md) | Plan for 0.1: current state of the code, contracts, workstreams, designs settled, what is out of 0.1, acceptance criteria |

## Positioning

*Code mode / programmatic tool calling* for how the model works, *Temporal / Restate*
for how the code executes, on a runtime we fully control. Because we own the runtime,
determinism is guaranteed by construction instead of by developer discipline, and
because a run executes one immutable version of its workflow, whose code is content-addressed,
there is no workflow versioning problem: a new version never changes a run in flight. What
still changes is the runtime itself, and a determinism epoch versions it.

## Process

Before code depends on a contract or a design, its architecture principles are presented
for review, with diagrams. Implementation starts after approval.
