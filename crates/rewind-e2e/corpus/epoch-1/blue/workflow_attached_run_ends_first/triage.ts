import { sleep } from "rewind";

export default async function main(input: { repo: string }) {
  await sleep("1d");
  return { closed: input.repo.length };
}
