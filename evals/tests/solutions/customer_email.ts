import { crm } from "rewind:actions";

export default async function main() {
  const customer = (await crm.get_customer({ id: "C-1007" })).json<{ email: string }>();
  return customer.email;
}
