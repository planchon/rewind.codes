import { triage } from "rewind:workflows";

export default async function main() {
  const run = await triage.start({ repo: "acme/api" });
  await run.signal("approval", { close: true });
  return run.result();
}
