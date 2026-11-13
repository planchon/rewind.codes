import { crm } from "rewind:tools";

export default async function main() {
  const ids = Array.from({ length: 12 }, (_, i) => `C-${1001 + i}`);
  const customers = await Promise.all(
    ids.map(async (id) => (await crm.get_customer({ id })).json<{ lifetime_value_cents: number }>()),
  );
  return customers.reduce((sum, customer) => sum + customer.lifetime_value_cents, 0);
}
