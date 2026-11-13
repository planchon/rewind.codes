import { shop } from "rewind:tools";
import { sleep, ToolError } from "rewind";

export default async function main() {
  const shipped: string[] = [];
  for (let n = 601; n <= 608; n++) {
    const order_id = `O-${n}`;
    const request_id = `ship-${order_id}`;
    for (;;) {
      try {
        const shipment = await shop.create_shipment({ order_id, request_id });
        shipped.push(shipment.shipment_id);
        break;
      } catch (error) {
        if (!(error instanceof ToolError)) throw error;
        const data = error.data as { retry_after_s?: number } | undefined;
        await sleep(`${data?.retry_after_s ?? 1}s`);
      }
    }
  }
  return shipped;
}
