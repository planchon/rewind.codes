import { signal, sleep } from "rewind";

export default async function main() {
  await sleep("1h"); // "approval" arrives meanwhile
  const approval = await signal("approval"); // buffered: returns at once
  const comment = await signal("comment", { timeout: "1d" }); // arrives: its timeout is cancelled
  const never = await signal("never", { timeout: "2h" }); // times out
  return { approval, comment, never: never ?? null, elapsed: performance.now() };
}
