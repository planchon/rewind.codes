import { triage } from "rewind:workflows";

export default async function main() {
  // Started attached, and never awaited: the run still ends after it.
  await triage.start({ repo: "acme/api" });
  return "started";
}
