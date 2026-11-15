import { triage } from "rewind:workflows";

export default async function main() {
  // @ts-expect-error: the input schema wants a string
  await triage.start({ repo: 42 });
  return "unreachable";
}
