import { crm } from "rewind:tools";
import { signal } from "rewind";

export default async function main() {
  const decision = await signal<{ approved: string[] }>("refund_approval");
  const disputed = ["O-601", "O-602", "O-603"];
  const approved = decision.approved.filter((id) => disputed.includes(id));
  for (const order_id of approved) {
    await crm.refund_order({ order_id, reason: "dispute" });
  }
  return approved;
}
