import { audit } from "rewind:actions";
import { signal } from "rewind";

export default async function main() {
  await audit.record({ step: 1 });
  await signal("go"); // waits where the history has the second call
  return "done";
}
