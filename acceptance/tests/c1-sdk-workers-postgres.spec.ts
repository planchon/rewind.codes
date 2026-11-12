/**
 * Criterion 1 (`archi/09-roadmap.md`, "0.1 is done when"): a harness drives runs through the SDK,
 * with several workers on one Postgres. Each run appears as one trace under the harness's span,
 * and runs can be searched by label and tool.
 *
 * The deployment: Postgres, blobs on MinIO, an OpenTelemetry Collector writing to a file, one
 * `ptc server --role api` and two `ptc server --role worker`. The harness is the TypeScript SDK:
 * its host-tool worker serves the harness's tools; it sends a signal and approves a call.
 */
import { randomBytes } from "node:crypto";
import { type HostToolContext, Ptc, type Run } from "@ai-ptc/sdk";
import { expect, test } from "@playwright/test";
import { type Deployment, startDeployment } from "../support/deployment.js";
import { dockerAvailable } from "../support/docker.js";
import { flattenSpans, HarnessSpan, parseTraceparent, type SpanRecord } from "../support/otlp.js";
import { resultDir } from "../support/paths.js";
import { eventually } from "../support/processes.js";

const SCRIPT = `import { signal, sleep } from "ptc";
import { crm, payments } from "ptc:tools";

export default async function main(input: { email: string }) {
  const customer = await crm.lookup({ email: input.email });
  await sleep("1s");
  const decision = await signal<{ refund: number }>("decision");
  if (decision.refund === 0) {
    return { customer: customer.id, refunded: 0 };
  }
  const refund = await payments.refund({ customer: customer.id, amount: decision.refund });
  return { customer: customer.id, refunded: refund.amount, notes: customer.notes.length };
}
`;

/** A customer record above 64 KiB: its payload goes to the blob store, MinIO. */
const NOTES = "a long customer history. ".repeat(4 * 1024);

const RUNS = 6;
/** This run gets no refund: it never calls `payments.refund`. */
const NO_REFUND = 5;

test.describe("criterion 1: SDK, two workers, one Postgres", () => {
  test.skip(!dockerAvailable(), "needs Docker");

  let deployment: Deployment;

  test.beforeAll(async () => {
    deployment = await startDeployment(resultDir("c1-sdk-workers-postgres"));
  });

  test.afterAll(async () => {
    await deployment?.stop();
  });

  test("runs driven by the SDK on an api server and two workers, traced under the harness", async () => {
    const { admin } = deployment;
    const harnessKey = await admin.apiKeys.create({ tenant: "acme", role: "harness", name: "acceptance harness" });
    const ptc = new Ptc({ url: deployment.url, apiKey: harnessKey.key ?? undefined });
    const suite = `acceptance-${randomBytes(4).toString("hex")}`;

    const toolset = await ptc.toolsets.create({
      host: [
        {
          name: "crm.lookup",
          description: "Find a customer by email.",
          inputSchema: {
            type: "object",
            properties: { email: { type: "string" } },
            required: ["email"],
          },
          outputSchema: {
            type: "object",
            properties: { id: { type: "string" }, notes: { type: "string" } },
            required: ["id", "notes"],
          },
        },
        {
          name: "payments.refund",
          description: "Refund a customer. A person approves every refund.",
          inputSchema: {
            type: "object",
            properties: { customer: { type: "string" }, amount: { type: "number" } },
            required: ["customer", "amount"],
          },
          outputSchema: {
            type: "object",
            properties: { id: { type: "string" }, amount: { type: "number" } },
            required: ["id", "amount"],
          },
          policy: { approval: true },
        },
      ],
    });
    expect(toolset.declarations.dts).toContain("lookup");

    // The harness's host tools, served by the SDK's worker. Each call is logged with its context.
    const calls: { tool: string; runId: string; seq: number; traceparent: string | undefined; key: string }[] = [];
    const log = (tool: string, ctx: HostToolContext) =>
      calls.push({ tool, runId: ctx.runId, seq: ctx.seq, traceparent: ctx.traceparent, key: ctx.idempotencyKey });
    const worker = ptc.worker({
      toolset: toolset.id,
      pollWait: "2s",
      tools: {
        "crm.lookup": async ({ email }: { email: string }, ctx) => {
          log("crm.lookup", ctx);
          return { id: `cus_${email.split("@")[0]}`, notes: NOTES };
        },
        "payments.refund": async ({ amount }: { customer: string; amount: number }, ctx) => {
          log("payments.refund", ctx);
          return { id: `re_${ctx.runId.slice(-6)}`, amount };
        },
      },
      onError: (error, info) => console.error("worker", info.phase, error),
    });

    try {
      // Each run is started by its own LLM call: the harness's span.
      const harness = Array.from({ length: RUNS }, (_, n) => new HarnessSpan(`chat ${suite} #${n}`));
      const runs: Run[] = [];
      for (let n = 0; n < RUNS; n++) {
        const request = {
          toolset: toolset.id,
          script: SCRIPT,
          input: { email: `user${n}@example.com` },
          idempotencyKey: `${suite}-tool-call-${n}`,
          labels: { suite, conversation: `c${n}` },
          traceparent: harness[n]!.traceparent,
        };
        const run = await ptc.runs.start(request);
        // The model's tool call handled twice starts one run.
        expect((await ptc.runs.start(request)).id).toBe(run.id);
        expect(run.traceId).toBe(harness[n]!.traceId);
        runs.push(run);
      }

      // Each run looks its customer up, sleeps, then waits for the decision.
      for (const [n, run] of runs.entries()) {
        await eventually(`run ${n} waits for its decision`, async () => {
          const state = await ptc.runs.get(run.id);
          return state.waitingFor?.some((w) => w.kind === "signal" && w.name === "decision") ? state : undefined;
        });
        const refund = n === NO_REFUND ? 0 : 10 * (n + 1);
        await ptc.runs.signal(run.id, "decision", { refund }, { idempotencyKey: `${suite}-decision-${n}` });
      }

      // A person approves each refund, after reading its arguments.
      for (const [n, run] of runs.entries()) {
        if (n === NO_REFUND) continue;
        const pending = await eventually(`run ${n}'s refund waits for approval`, async () => {
          const found = ptc.approvals.pending(await ptc.runs.get(run.id));
          return found.length > 0 ? found : undefined;
        });
        expect(pending).toHaveLength(1);
        expect(pending[0]!.tool).toBe("payments.refund");
        expect(await ptc.approvals.args(pending[0]!)).toEqual({ customer: `cus_user${n}`, amount: 10 * (n + 1) });
        await ptc.approvals.approve(pending[0]!);
      }

      const ended = await Promise.all(runs.map((run) => ptc.runs.get(run.id, { wait: "120s" })));
      for (const [n, run] of ended.entries()) {
        expect(run.status, JSON.stringify(run)).toBe("completed");
        expect(run.output?.value).toEqual(
          n === NO_REFUND
            ? { customer: `cus_user${n}`, refunded: 0 }
            : { customer: `cus_user${n}`, refunded: 10 * (n + 1), notes: NOTES.length },
        );
        expect(run.labels).toEqual({ suite, conversation: `c${n}` });
      }
      // Every tool call was made once.
      expect(calls.filter((c) => c.tool === "crm.lookup")).toHaveLength(RUNS);
      expect(calls.filter((c) => c.tool === "payments.refund")).toHaveLength(RUNS - 1);

      // Found through the API by label and by tool (the read model, filled by the projector).
      const ids = (page: { runs: { id: string }[] }) => page.runs.map((r) => r.id).sort();
      const all = runs.map((r) => r.id).sort();
      await expect.poll(async () => ids(await ptc.runs.list({ labels: { suite } }))).toEqual(all);
      await expect
        .poll(async () => ids(await ptc.runs.list({ labels: { suite }, tool: "payments.refund" })))
        .toEqual(all.filter((id) => id !== runs[NO_REFUND]!.id));
      await expect.poll(async () => ids(await ptc.runs.list({ labels: { suite }, tool: "crm.lookup" }))).toEqual(all);
      expect(ids(await ptc.runs.list({ labels: { suite, conversation: "c2" } }))).toEqual([runs[2]!.id]);
      expect(ids(await ptc.runs.list({ labels: { suite }, status: "failed" }))).toEqual([]);

      // The large customer record went to MinIO.
      const objects = deployment.minio.objects();
      expect(objects.some((key) => key.includes("sha256")), objects.join("\n")).toBe(true);

      // The traces: every run is one trace under its harness span.
      for (const span of harness) await span.export(deployment.collector.http);
      const spans = await eventually(
        "the collector has every run span, its activations, and every harness span",
        () => {
          // Delivery is at least once: a span exported again is the same span, with the same id.
          const spans = dedupe(flattenSpans(deployment.collector.traces()));
          const done =
            runs.every(
              (run) =>
                spans.some((s) => s.name === "run" && s.attributes["ptc.run_id"] === run.id) &&
                spans.some((s) => s.name === "activation" && s.attributes.run === `acme/${run.id}`),
            ) && harness.every((h) => spans.some((s) => s.spanId === h.spanId));
          return done ? spans : undefined;
        },
        90_000,
        500,
      );
      const activatedBy = new Set<unknown>();
      for (const [n, run] of runs.entries()) {
        const span = harness[n]!;
        expect(spans.find((s) => s.spanId === span.spanId)?.traceId).toBe(span.traceId);
        // Run telemetry names the run `ptc.run_id`; service spans (activations) `run`, as `{tenant}/{run}`.
        const mine = spans.filter((s) => s.attributes["ptc.run_id"] === run.id || s.attributes.run === `acme/${run.id}`);
        // One trace: the harness's.
        expect(new Set(mine.map((s) => s.traceId))).toEqual(new Set([span.traceId]));
        const runSpan = only(mine.filter((s) => s.name === "run" && s.scope === "ai-ptc.run"));
        expect(runSpan.parentSpanId).toBe(span.spanId);
        const children = (name: string) => mine.filter((s) => s.name === name);
        const lookup = only(children("crm.lookup"));
        expect(lookup.parentSpanId).toBe(runSpan.spanId);
        expect(only(children("timer")).parentSpanId).toBe(runSpan.spanId);
        expect(only(children("signal decision")).parentSpanId).toBe(runSpan.spanId);
        if (n !== NO_REFUND) expect(only(children("payments.refund")).parentSpanId).toBe(runSpan.spanId);
        // The activations (service telemetry, from the workers) sit under the run span.
        const activations = children("activation");
        expect(activations.length).toBeGreaterThan(0);
        for (const activation of activations) {
          expect(activation.parentSpanId).toBe(runSpan.spanId);
          activatedBy.add(activation.resource["service.instance.id"]);
        }
        // The host tool got the trace context, with the tool call span as parent.
        const call = calls.find((c) => c.runId === run.id && c.tool === "crm.lookup");
        expect(call?.traceparent).toBeDefined();
        expect(parseTraceparent(call!.traceparent!)).toEqual({ traceId: span.traceId, parentId: lookup.spanId });
      }
      // Both workers ran activations; the api server ran none.
      expect([...activatedBy].sort()).toEqual(deployment.workers);
    } finally {
      await worker.stop();
    }
  });
});

function dedupe(spans: SpanRecord[]): SpanRecord[] {
  const seen = new Map<string, SpanRecord>();
  for (const span of spans) seen.set(`${span.traceId}/${span.spanId}`, span);
  return [...seen.values()];
}

function only(spans: SpanRecord[]): SpanRecord {
  expect(spans, spans.map((s) => `${s.name} ${s.spanId}`).join(", ")).toHaveLength(1);
  return spans[0]!;
}
