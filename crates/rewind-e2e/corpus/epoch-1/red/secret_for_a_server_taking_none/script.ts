import { crm } from "rewind:tools";

export default async function main(input: { customer: string }) {
  const customer = await crm.lookup({ id: input.customer });
  return { name: customer.name };
}
