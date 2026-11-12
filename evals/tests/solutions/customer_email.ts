import { crm } from "ptc:tools";

export default async function main() {
  const customer = (await crm.get_customer({ id: "C-1007" })).json<{ email: string }>();
  return customer.email;
}
