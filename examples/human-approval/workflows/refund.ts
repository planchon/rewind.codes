/** Refund an order once a manager has reviewed it and the harness approves the payment. */
// Two human decisions: a manager's review, a signal the code waits for (durably: minutes or days); and the harness's
// consent to the exact `payments.refund` call, whose action has the `approval` policy.
import { orders, payments, notify } from "rewind:actions";
import { signal } from "rewind";

export default async function main(input: { orderId: string; reason: string }) {
  const order = await orders.get({ id: input.orderId });
  await notify({ text: `Refund of ${order.total} ${order.currency} asked for order ${order.id}: ${input.reason}` });

  const review = await signal<{ approved: boolean; note?: string }>("manager_review", { timeout: "3d" });
  if (review === undefined) return { refunded: false, why: "no review within 3 days" };
  if (!review.approved) return { refunded: false, why: review.note ?? "rejected" };

  const refund = await payments.refund({ orderId: order.id, amount: order.total });
  return { refunded: true, refundId: refund.id };
}
