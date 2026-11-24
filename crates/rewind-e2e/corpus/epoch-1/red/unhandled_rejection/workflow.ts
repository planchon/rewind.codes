import { payments, audit } from "rewind:actions";

export default async function main() {
  const charge = payments.charge({ amount: 100 }); // rejected before it is awaited
  await audit.record({ event: "charging" });
  try {
    await charge;
  } catch {
    return "handled too late";
  }
}
