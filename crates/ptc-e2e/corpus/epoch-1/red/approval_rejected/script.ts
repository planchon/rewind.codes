import { payments } from "ptc:tools";

export default async function main(input: { order: string; amount: number }) {
  const refund = await payments.refund({ order: input.order, amount: input.amount });
  return { refunded: refund.id };
}
