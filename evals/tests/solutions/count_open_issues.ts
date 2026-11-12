import { tracker } from "ptc:tools";

export default async function main() {
  let open = 0;
  let customer = 0;
  let cursor: string | undefined;
  do {
    const page = await tracker.list_issues({ repo: "acme/api", limit: 25, ...(cursor ? { cursor } : {}) });
    open += page.issues.length;
    customer += page.issues.filter((issue) => issue.labels.includes("customer")).length;
    cursor = page.next_cursor;
  } while (cursor);
  return { open, customer };
}
