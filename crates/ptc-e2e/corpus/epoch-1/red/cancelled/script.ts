import { sleep } from "ptc";

export default async function main() {
  await sleep("1d");
  return "woke up";
}
