import { shop } from "ptc:tools";

export default async function main() {
  const low: { sku: string; reorder_quantity: number; supplier: string }[] = [];
  let cursor: string | undefined;
  do {
    const page = await shop.list_products(cursor ? { cursor } : {});
    low.push(...page.products.filter((product) => product.stock < product.reorder_point));
    cursor = page.next_cursor;
  } while (cursor);
  return await Promise.all(
    low.map((product) =>
      shop.create_purchase_order({
        sku: product.sku,
        quantity: product.reorder_quantity,
        supplier: product.supplier,
      }),
    ),
  );
}
