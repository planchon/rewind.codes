import { crm } from "ptc:tools";

export default async function main() {
  const customer = await crm.lookup({ id: "c-1" });
  const profile = await crm.profile({ id: "c-1" });
  const score = await crm.score({ id: "c-1" });
  return {
    customer: customer.json(),
    profile: [profile.text(), profile.json()],
    keys: Object.keys(profile),
    serialized: JSON.parse(JSON.stringify(customer)),
    score: [score.score, "text" in score],
  };
}
