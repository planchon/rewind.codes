import { audit } from "rewind:actions";

export default async function main() {
  const cut = "😀 hi".slice(0, 1); // half of a surrogate pair
  await audit.record({ text: cut });
  return { text: cut, length: cut.length };
}
