// Triage the stale issues of a repository, after a human approves.
import { github, slack } from "rewind:tools";
import { signal } from "rewind";

const DAY = 86_400_000;

export default async function main(input: { repo: string }) {
  const issues = await github.list_issues({ repo: input.repo, state: "open" });
  const stale = issues.filter((i) => Date.now() - Date.parse(i.updated_at) > 30 * DAY);
  await slack.post_message({ channel: "#triage", text: `${stale.length} stale issues in ${input.repo}` });

  const areas = [...new Set(stale.map((i) => i.labels[0]?.name ?? "unlabelled"))];
  const decision = await signal<{ close: boolean }>("approval", { timeout: "3d" });
  if (!decision?.close) return { closed: 0, areas };

  for (const issue of stale) await github.close_issue({ repo: input.repo, number: issue.number });
  return { closed: stale.length, areas };
}
