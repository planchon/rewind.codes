// A refund that needs two human decisions:
// - a manager's review, which the script waits for as a signal (durably: minutes or days);
// - the harness's consent to the exact `payments.refund` call, which has the `approval` policy.
import { orders, payments, notify } from "rewind:tools";
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
