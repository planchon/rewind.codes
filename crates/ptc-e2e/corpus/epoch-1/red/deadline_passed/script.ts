import { signal } from "ptc";

export default async function main() {
  return await signal("approval"); // never comes
}
