import { github } from "rewind:actions";

export default async function main(input: { numbers: unknown[] }) {
  const titles: string[] = [];
  const refused: string[] = [];
  for (const number of input.numbers) {
    try {
      const issue = await github.get_issue({ number } as { number: number });
      titles.push(issue.title);
    } catch (error) {
      refused.push(`${(error as Error).name}: ${(error as Error).message}`);
    }
  }
  return { titles, refused };
}
