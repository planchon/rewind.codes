import { github } from "ptc:tools";
import { signal, webhook } from "ptc";

export default async function main(input: { repo: string }) {
  const hook = await webhook("pr_merged");
  await github.create_webhook({ repo: input.repo, url: hook.url });
  const event = await signal<{ pr: number }>("pr_merged");
  return { url: hook.url, merged: event.pr };
}
