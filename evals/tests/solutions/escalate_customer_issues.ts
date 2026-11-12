import { chat, crm, tracker } from "ptc:tools";

type Customer = { name: string; tier: string };

export default async function main() {
  const issues: { number: number; title: string; reporter: string }[] = [];
  let cursor: string | undefined;
  do {
    const page = await tracker.list_issues({
      repo: "acme/api",
      label: "customer",
      limit: 25,
      ...(cursor ? { cursor } : {}),
    });
    issues.push(...page.issues);
    cursor = page.next_cursor;
  } while (cursor);
  const customers = await Promise.all(
    issues.map(async (issue) => (await crm.get_customer({ id: issue.reporter })).json<Customer>()),
  );
  const lines = issues.flatMap((issue, k) => {
    const customer = customers[k];
    return customer && customer.tier === "enterprise"
      ? [`#${issue.number} ${issue.title} (${customer.name})`]
      : [];
  });
  await chat.post_message({ channel: "#escalations", text: lines.join("\n") });
  return lines;
}
