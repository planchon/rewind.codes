import { crm, tracker } from "rewind:actions";

export default async function main() {
  const ids = ["C-1001", "C-1002", "C-1003", "C-1004", "C-1005", "C-1006", "C-1007", "C-1008"];
  for (const id of ids) {
    const customer = (await crm.get_customer({ id })).json() as any;
    await tracker.create_issue({
      repo: "acme/support",
      title: `Onboard ${customer.company?.name ?? customer.name} (${id})`,
    });
  }
  return ids.length;
}
