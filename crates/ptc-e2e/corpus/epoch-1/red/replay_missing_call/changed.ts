import { audit } from "ptc:tools";
import { signal } from "ptc";

export default async function main() {
  await audit.record({ step: 1 });
  await signal("go"); // waits where the journal has the second call
  return "done";
}
