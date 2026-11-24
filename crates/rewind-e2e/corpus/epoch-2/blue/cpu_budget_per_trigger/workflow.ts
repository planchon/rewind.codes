import { sleep } from "rewind";
import { audit } from "rewind:actions";

function work(rounds: number): number {
  let sum = 0;
  for (let i = 0; i < rounds; i++) sum = (sum + i * 7) % 1000003;
  return sum;
}

export default async function main() {
  const sums: number[] = [];
  for (let step = 0; step < 6; step++) {
    sums.push(work(200_000));
    await audit.record({ step });
    await sleep("1d");
  }
  return { sums: sums.length };
}
