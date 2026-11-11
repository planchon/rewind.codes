import { payments } from "ptc:tools";

export default async function main() {
  payments.charge({ amount: 100 }); // no await
  return "charged";
}
