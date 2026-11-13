import { signal } from "rewind";

export default async function main() {
  return await signal("approval"); // never comes
}
