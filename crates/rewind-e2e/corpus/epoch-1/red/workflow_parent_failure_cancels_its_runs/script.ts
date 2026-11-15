import { triage } from "rewind:workflows";

export default async function main() {
  await triage.start({ repo: "acme/api" });
  throw new Error("the parent gave up");
}
