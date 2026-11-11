import { signal } from "ptc";

export default async function main() {
  const first = await signal("approval");
  const second = await signal("approval", { timeout: "1h" });
  return { first, second: second ?? null };
}
