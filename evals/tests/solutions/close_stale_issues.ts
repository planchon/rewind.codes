import { tracker } from "ptc:tools";

export default async function main() {
  const cutoff = Date.now() - 30 * 86_400_000;
  const stale: number[] = [];
  let cursor: string | undefined;
  do {
    const page = await tracker.list_issues({ repo: "acme/web", limit: 25, ...(cursor ? { cursor } : {}) });
    for (const issue of page.issues) {
      if (Date.parse(issue.updated_at) < cutoff) stale.push(issue.number);
    }
    cursor = page.next_cursor;
  } while (cursor);
  for (const number of stale) {
    await tracker.add_comment({ repo: "acme/web", number, body: "Closing as stale." });
    await tracker.close_issue({ repo: "acme/web", number });
  }
  return stale;
}
