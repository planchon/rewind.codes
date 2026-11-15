// A script that starts a run of `repo_check` per repository, signals each, and gathers their
// outputs. Every call is checked against the workflow's schemas before the script runs.
import { repo_check } from "rewind:workflows";

export default async function main(input: { repos: string[] }) {
  const runs = await Promise.all(input.repos.map((repo) => repo_check.start({ repo })));
  await Promise.all(runs.map((run) => run.signal("acknowledge", { by: "check-all" })));
  const results = await Promise.all(runs.map((run) => run.result()));
  return results.sort((a, b) => b.stars - a.stars);
}
