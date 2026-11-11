import { github } from "ptc:tools";

export default async function main(input: { repo: string }) {
  const issues = await Promise.all(
    [1, 2, 3].map((number) => github.get_issue({ repo: input.repo, number })),
  );
  return { numbers: issues.map((issue: { number: number }) => issue.number) };
}
