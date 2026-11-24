// A human in the loop, twice: a signal the workflow waits for, and a call the harness approves.
//
//   pnpm human-approval
import { readFile } from "node:fs/promises";
import { Rewind } from "@rewind.codes/sdk";

const rewind = new Rewind();
const object = (properties: Record<string, unknown>, required = Object.keys(properties)) => ({
  type: "object",
  properties,
  required,
});

const actionSet = await rewind.actionSets.create({
  harness: [
    {
      name: "orders.get",
      description: "An order by its id.",
      inputSchema: object({ id: { type: "string" } }),
      outputSchema: object({ id: { type: "string" }, total: { type: "number" }, currency: { type: "string" } }),
    },
    {
      name: "payments.refund",
      description: "Refund an order. Moves money: the harness approves each call.",
      inputSchema: object({ orderId: { type: "string" }, amount: { type: "number" } }),
      outputSchema: object({ id: { type: "string" } }),
      // Sent only once the harness approves it; never delivered twice after an interruption.
      policy: { approval: true, idempotent: false },
    },
    {
      name: "notify",
      description: "Post a message to the support channel.",
      inputSchema: object({ text: { type: "string" } }),
    },
  ],
});

const worker = rewind.worker({
  actionSet: actionSet.id,
  actions: {
    "orders.get": async ({ id }: { id: string }) => ({ id, total: 129.9, currency: "EUR" }),
    "payments.refund": async ({ orderId }: { orderId: string }, ctx) => {
      // ctx.idempotencyKey is the same on every delivery: pass it to the payment provider.
      console.log(`[payments] refund ${orderId}, idempotency key ${ctx.idempotencyKey}`);
      return { id: `re_${orderId}` };
    },
    notify: async ({ text }: { text: string }) => console.log(`[notify] ${text}`),
  },
});

const customer = rewind.namespace("customer_8812");
const code = await readFile(new URL("./workflows/refund.ts", import.meta.url), "utf8");

// The run goes until it needs the outside world, then waits there, for days if need be.
let run = await customer.runs.start({
  actionSet: actionSet.id,
  code,
  input: { orderId: "ord_1042", reason: "arrived broken" },
  wait: "10s",
  labels: { example: "human-approval" },
});
console.log(`run ${run.id}: ${run.status}, for`, run.waitingFor?.map((w) => w.kind));

// 1. A manager approves in your product; the harness sends the signal. The run could have been
//    waiting for days: nothing runs, nothing is held in memory meanwhile.
await customer.runs.signal(run.id, "manager_review", { approved: true }, { idempotencyKey: `review-${run.id}` });
run = await customer.runs.get(run.id, { wait: "10s" });

// 2. The run now waits for consent to the refund call: show its arguments, then decide.
for (const approval of rewind.approvals.pending(run)) {
  console.log(`${approval.action} waits for approval, at line ${approval.callsite?.line}:`, await rewind.approvals.args(approval));
  await rewind.approvals.approve(approval);
}

run = await customer.runs.get(run.id, { wait: "10s" });
console.log(`run ${run.id}: ${run.status}`, run.output?.value);
await worker.stop();
