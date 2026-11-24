import { github } from "rewind:actions";
import { signal } from "rewind";

export default async function main(input: { repo: string }) {
  const issues: { number: number }[] = await github.list_issues({ repo: input.repo });
  const approval = await signal<{ close: boolean }>("approval");
  if (!approval.close) return { closed: 0 };
  await Promise.all(issues.map((issue) => github.close_issue({ repo: input.repo, number: issue.number })));
  return { closed: issues.length };
}
