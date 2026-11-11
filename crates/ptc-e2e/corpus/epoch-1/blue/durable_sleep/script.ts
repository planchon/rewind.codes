import { sleep } from "ptc";

export default async function main() {
  const before = Date.now();
  await sleep("10d");
  await new Promise((resolve) => setTimeout(resolve, 1500));
  return { elapsed: Date.now() - before, now: new Date().toISOString() };
}
