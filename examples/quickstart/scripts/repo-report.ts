// What a model would write: one TypeScript module whose default export is the entry point.
// `fetch` exists because the toolset enables the `http` tool for api.github.com; `notify` is a
// host tool, executed by the harness's worker.
import { notify } from "rewind:tools";

export default async function main(input: { repo: string }) {
  const res = await fetch(`https://api.github.com/repos/${input.repo}`, {
    headers: { accept: "application/vnd.github+json", "user-agent": "rewind-quickstart" },
  });
  if (!res.ok) throw new Error(`GitHub answered ${res.status} for ${input.repo}`);
  const repo = await res.json();

  const report = {
    repo: repo.full_name as string,
    stars: repo.stargazers_count as number,
    openIssues: repo.open_issues_count as number,
    pushedAt: repo.pushed_at as string,
  };
  console.log(`${report.repo}: ${report.stars} stars`);

  await notify({ text: `${report.repo} has ${report.openIssues} open issues and pull requests.` });
  return report;
}
