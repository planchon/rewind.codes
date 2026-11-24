import { chat, office } from "rewind:actions";
import { signal } from "rewind";

export default async function main() {
  await chat.post_message({ channel: "#finance", text: "Please approve the Q2 budget" });
  const decision = await signal<{ approved: boolean }>("budget_decision", { timeout: "24h" });
  if (decision === undefined) {
    await chat.post_message({ channel: "#managers", text: "No decision on the Q2 budget, escalating" });
    return "escalated";
  }
  if (decision.approved) {
    await office.send_email({ to: "cfo@acme.test", subject: "Budget approved", body: "The Q2 budget is approved." });
  }
  return decision.approved ? "approved" : "rejected";
}
