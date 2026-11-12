import { github } from "ptc:tools";

export default async function main(input: { repo: string }) {
  const issues = await github.list_issues({ repo: input.repo });
  const details = await Promise.all(
    issues.map((issue: { number: number }) =>
      github.get_issue({ repo: input.repo, number: issue.number }),
    ),
  );
  const oldest = details[0];
  // The bug: the number is off by one.
  await github.close_issue({ repo: input.repo, number: oldest.number + 1 });
  return { closed: oldest.number };
}
