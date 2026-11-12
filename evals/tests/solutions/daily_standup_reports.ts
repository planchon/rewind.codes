import { chat, tracker } from "ptc:tools";
import { sleep } from "ptc";

const DAY = 86_400_000;
const NINE = 9 * 3_600_000;

export default async function main() {
  for (let report = 0; report < 3; report++) {
    const now = Date.now();
    const today = Math.floor(now / DAY) * DAY;
    const next = now % DAY >= NINE ? today + DAY + NINE : today + NINE;
    await sleep(next - now);
    let open = 0;
    let cursor: string | undefined;
    do {
      const page = await tracker.list_issues({ repo: "acme/api", limit: 25, ...(cursor ? { cursor } : {}) });
      open += page.issues.length;
      cursor = page.next_cursor;
    } while (cursor);
    await chat.post_message({ channel: "#standup", text: `Open issues in acme/api: ${open}` });
  }
  return "done";
}
