import { audit } from "ptc:tools";

export default async function main() {
  await audit.record({ step: 1 });
  await audit.record({ step: 2 });
  return "done";
}
