import { sleep } from "ptc";
import { audit } from "ptc:tools";

export default async function main() {
  try {
    await sleep("30d");
  } catch {
    await audit.record({ event: "cleanup" }); // needs another activation: too late
    return "cleaned up";
  }
}
