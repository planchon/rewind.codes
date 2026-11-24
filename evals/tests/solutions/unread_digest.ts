import { office } from "rewind:actions";

type Inbox = { emails: { from: string; unread: boolean }[]; next_cursor?: string };

export default async function main() {
  const counts = new Map<string, number>();
  let cursor: string | undefined;
  do {
    const page = (await office.list_inbox(cursor ? { cursor } : {})).json<Inbox>();
    for (const email of page.emails) {
      if (!email.unread) continue;
      const domain = email.from.split("@")[1] ?? "";
      counts.set(domain, (counts.get(domain) ?? 0) + 1);
    }
    cursor = page.next_cursor;
  } while (cursor);
  const lines = [...counts].sort((a, b) => b[1] - a[1]).map(([domain, n]) => `${domain}: ${n}`);
  await office.send_email({ to: "cfo@acme.test", subject: "Inbox digest", body: lines.join("\n") });
  return lines;
}
