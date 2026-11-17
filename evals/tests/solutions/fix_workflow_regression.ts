import { shipping } from "rewind:tools";

export default async function main(input: { orders: string[] }) {
  const labels = await Promise.all(
    input.orders.map((order_id) => shipping.create_label({ order_id })),
  );
  // An order collected in store has a label without tracking.
  const shipped = labels.map((label) => ({
    order: label.order_id,
    tracking: label.tracking?.number ?? null,
  }));
  return { shipped };
}
