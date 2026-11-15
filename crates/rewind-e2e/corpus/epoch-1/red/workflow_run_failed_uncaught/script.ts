import { triage } from "rewind:workflows";

export default async function main() {
  const run = await triage.start({ repo: "acme/api" });
  return run.result();
}
