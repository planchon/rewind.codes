import { crm } from "rewind:actions";

export default async function main(input: { customer: string }) {
  const customer = await crm.lookup({ id: input.customer });
  throw new Error(`no plan for ${customer.name}`);
}
