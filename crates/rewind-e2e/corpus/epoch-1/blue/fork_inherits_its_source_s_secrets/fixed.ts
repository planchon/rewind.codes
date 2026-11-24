import { crm } from "rewind:actions";

export default async function main(input: { customer: string }) {
  const customer = await crm.lookup({ id: input.customer });
  const plan = await crm.plan({ id: input.customer });
  return { name: customer.name, plan: plan.tier };
}
