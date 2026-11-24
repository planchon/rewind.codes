import { triage } from "rewind:workflows";

export default async function main(input: { repos: string[] }) {
  const runs = await Promise.all(input.repos.map((repo) => triage.start({ repo })));
  await Promise.all(runs.map((run) => run.signal("approval", { close: true })));
  const results = await Promise.all(runs.map((run) => run.result()));
  return { closed: results.reduce((sum, result) => sum + result.closed, 0) };
}
