import { shop } from "ptc:tools";

export default async function main() {
  const totals: Record<string, number> = {};
  let cursor: string | undefined;
  do {
    const page = await shop.list_sales({ month: "2026-02", limit: 200, ...(cursor ? { cursor } : {}) });
    for (const sale of page.sales) {
      totals[sale.region] = (totals[sale.region] ?? 0) + sale.amount_cents;
    }
    cursor = page.next_cursor;
  } while (cursor);
  return totals;
}
