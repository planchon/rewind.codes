import { chat, tracker } from "rewind:actions";

export default async function main(input: { repo: string }) {
  const issues: { number: number; title: string }[] = [];
  let cursor: string | undefined;
  do {
    const page = await tracker.list_issues({
      repo: input.repo,
      label: "bug",
      limit: 25,
      ...(cursor === undefined ? {} : { cursor }),
    });
    issues.push(...page.issues);
    cursor = page.next_cursor;
  } while (cursor !== undefined);
  if (issues.length === 0) {
    return { posted: 0 };
  }
  const lines = issues.map((issue) => `#${issue.number} ${issue.title}`);
  await chat.post_message({
    channel: "#bugs",
    text: `Open bugs in ${input.repo}:\n${lines.join("\n")}`,
  });
  return { posted: issues.length };
}
