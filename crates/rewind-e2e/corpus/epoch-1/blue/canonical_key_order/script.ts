import { audit } from "rewind:actions";

export default async function main(input: Record<string, unknown>) {
  await audit.record({ first: Object.keys(input)[0] }); // the next activation replays this
  return Object.keys(input);
}
