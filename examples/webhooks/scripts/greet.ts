// Waits for the signal `hello`: the run is suspended until it arrives, then completes.
import { signal } from "rewind";

export default async function main() {
  const { name } = await signal<{ name: string }>("hello");
  return { greeting: `Hello, ${name}` };
}
