import { triage } from "rewind:workflows";

export default async function main() {
  const run = await triage.start({ customer: "c-7" });
  return run.result();
}
