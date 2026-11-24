import { crm } from "rewind:actions";

export default async function main() {
  const exported = await crm.export();
  const rows = exported.json();
  return rows.length;
}
