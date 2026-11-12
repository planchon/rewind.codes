import { payments } from "ptc:tools";

export default async function main() {
  const receipt = await payments.charge({ amount: 100 });
  return { receipt };
}
