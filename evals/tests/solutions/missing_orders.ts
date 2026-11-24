import { crm } from "rewind:actions";
import { ActionError } from "rewind";

export default async function main() {
  const ids = Array.from({ length: 12 }, (_, i) => `O-${501 + i}`);
  let total_cents = 0;
  const missing: string[] = [];
  await Promise.all(
    ids.map(async (id) => {
      try {
        const order = (await crm.get_order({ id })).json<{ total_cents: number }>();
        total_cents += order.total_cents;
      } catch (error) {
        if (!(error instanceof ActionError)) throw error;
        missing.push(id);
      }
    }),
  );
  missing.sort();
  return { total_cents, missing };
}
