import { sleep } from "rewind";
import { crm } from "rewind:tools";

export default async function main(input: { customer: string }) {
  const before = await crm.lookup({ id: input.customer });
  await sleep("1h");
  const after = await crm.lookup({ id: input.customer });
  return { before: before.name, after: after.name };
}
