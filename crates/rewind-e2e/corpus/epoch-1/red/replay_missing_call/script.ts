import { audit } from "rewind:tools";

export default async function main() {
  await audit.record({ step: 1 });
  await audit.record({ step: 2 });
  return "done";
}
