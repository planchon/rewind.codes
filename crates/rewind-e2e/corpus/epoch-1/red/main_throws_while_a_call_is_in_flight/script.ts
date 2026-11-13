import { audit } from "rewind:tools";

export default async function main(input: { amount: number }) {
  audit.record({ amount: input.amount }).then(() => console.log("must not run"));
  if (input.amount < 0) throw new RangeError(`invalid amount ${input.amount}`);
  return "ok";
}
