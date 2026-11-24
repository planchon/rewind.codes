import { ops } from "rewind:actions";

type Page = { events: { id: string }[]; next_cursor?: string };

export default async function main() {
  const events: unknown[] = [];
  let cursor: string | undefined;
  do {
    const page = (
      await ops.list_audit_events({ month: "2026-02", limit: 500, ...(cursor ? { cursor } : {}) })
    ).json<Page>();
    events.push(...page.events);
    cursor = page.next_cursor;
  } while (cursor);
  return events;
}
