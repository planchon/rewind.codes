import { crm } from "ptc:tools";

export default async function main() {
  const exported = await crm.export();
  const rows = exported.json();
  return rows.length;
}
