import { github } from "ptc:tools";
import { signal } from "ptc";

export default async function main() {
  const issue = await github.get_issue({ number: 4 });
  const decision = await signal<{ close: boolean }>("approval");
  if (decision.close) await github.close_issue({ number: issue.number });
  return decision;
}
