import { github } from "rewind:actions";

export default async function main(input: { repo: string }) {
  const issues = await github.list_issues({ repo: input.repo });
  const details = await Promise.all(
    issues.map((issue: { number: number }) =>
      github.get_issue({ repo: input.repo, number: issue.number }),
    ),
  );
  const oldest = details[0];
  await github.close_issue({ repo: input.repo, number: oldest.number });
  return { closed: oldest.number };
}
