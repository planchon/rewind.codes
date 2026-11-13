import { sleep } from "rewind";
import { audit } from "rewind:tools";

export default async function main() {
  try {
    await sleep("30d");
  } catch {
    await audit.record({ event: "cleanup" }); // needs another activation: too late
    return "cleaned up";
  }
}
