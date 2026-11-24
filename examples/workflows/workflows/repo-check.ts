/** The stars and open issues of a GitHub repository, once someone acknowledges it. */
// The code of the workflow `repo_check`, with a typed input, output and signal. Creating it checks that `main` takes
// the input schema's type, returns the output schema's, and that `signal()` only names declared signals.
import { signal } from "rewind";

export default async function main(input: { repo: string }) {
  const res = await fetch(`https://api.github.com/repos/${input.repo}`, {
    headers: { accept: "application/vnd.github+json", "user-agent": "rewind-examples" },
  });
  if (!res.ok) throw new Error(`GitHub answered ${res.status} for ${input.repo}`);
  const repo = await res.json();

  // Typed by the workflow's interface: the payload is { by: string }.
  const ack = await signal("acknowledge", { timeout: "1h" });
  return {
    repo: input.repo,
    stars: repo.stargazers_count as number,
    openIssues: repo.open_issues_count as number,
    acknowledgedBy: ack?.by,
  };
}
